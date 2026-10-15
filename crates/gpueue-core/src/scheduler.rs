//! Pure scheduling decisions.
//!
//! [`plan`] looks at a snapshot of every job, group and GPU and decides what
//! the daemon should do next. It performs no I/O and mutates nothing; the
//! daemon applies the returned [`Plan`] (persisting state changes and asking
//! the helper to spawn or restore processes) and calls [`plan`] again on its
//! next tick or whenever something changes.
//!
//! The rules follow pueue 4.0.x (`task_handler.rs` and
//! `process_handler/spawn.rs`) where pueue has an equivalent, extended with
//! GPU slots and checkpoint restores:
//!
//! 1. **Delayed enqueue.** A `Stashed { enqueue_at: Some(t) }` job with
//!    `t <= now` becomes queued. Such jobs are treated as `Queued` for the
//!    rest of the same plan, so they may also be started right away.
//! 2. **Failed dependencies.** A queued job with a dependency that is `Done`
//!    with any result other than `Success` fails with `DependencyFailed`.
//!    As in pueue this is skipped while the job's group is paused, so users
//!    can fix and restart a dependency in place without breaking the chain.
//!    Failures cascade within one plan: a job depending on a job that this
//!    plan marks as `DependencyFailed` is marked as well.
//! 3. **Starting and restoring.** `Queued` and `RestoreQueued` jobs compete
//!    for the same slots and are considered in one list ordered by priority:
//!    with [`plan`] and [`plan_with`] by higher `-o` priority first, then
//!    lower [`JobId`] (pueue's order); with [`plan_prioritized`] by the
//!    multifactor priority of [`crate::priority`]. A job is launched
//!    when
//!    - its group exists and is not paused,
//!    - its group has a free slot,
//!    - every GPU in `spec.gpus` exists and has a free slot, and
//!    - (queued jobs only) every dependency is `Done { Success }`.
//!
//!    Restore-queued jobs skip the dependency check: they already ran, so
//!    their dependencies were satisfied when they first started.
//!
//! There are no forced launches in the plan: an administrator's immediate
//! launch ignores every limit and the daemon launches it itself before
//! planning; anybody else's is queued first among their own jobs (like
//! `gpueue top`) and goes through the rules above like any other job.
//!
//! Slots are counted from jobs whose state [`occupies_slot`] plus the jobs
//! launched earlier in the same plan. A `parallel` value of `0` means
//! "unlimited", matching pueue's groups; the same convention is used for
//! GPUs for consistency.
//!
//! **No head-of-line blocking.** A job that cannot be launched (for example a
//! high-priority job waiting for a busy GPU) does not prevent lower-priority
//! jobs that fit from starting. This matches pueue, which picks the best job
//! among those that *can* start, and keeps GPUs busy. The trade-off is that a
//! job needing many GPUs can be starved by a stream of smaller jobs; users can
//! counter that by pausing groups or adjusting priorities.
//!
//! **Unknown dependencies** (ids that no longer exist, e.g. removed or
//! cleaned jobs) are ignored, i.e. treated as satisfied. This is what pueue
//! does; validating ids at submission time is the caller's job.
//!
//! Jobs whose group or GPUs are unknown are never launched and are left
//! untouched; the daemon should report them rather than guess.
//!
//! [`occupies_slot`]: JobState::occupies_slot

use std::collections::{HashMap, HashSet};

use crate::model::{Gpu, GpuIndex, Group, Job, JobId, JobResult, JobState, Timestamp, Uid};
use crate::priority::{self, JobPriority, Params, Resources};

/// What the daemon should do, as decided by [`plan`].
///
/// The daemon should apply the lists in field order: `enqueue`, then
/// `dependency_failed`, then `start` and `restore`. Every list is in decision
/// order and contains each job at most once; `start` may contain jobs that
/// also appear in `enqueue`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// `Stashed` jobs whose `enqueue_at` has been reached; they become
    /// `Queued { enqueued_at: now }`.
    pub enqueue: Vec<JobId>,
    /// Queued jobs to mark `Done { DependencyFailed }`.
    pub dependency_failed: Vec<JobId>,
    /// Queued jobs to spawn.
    pub start: Vec<JobId>,
    /// Restore-queued jobs to restore from their checkpoint.
    pub restore: Vec<JobId>,
}

impl Plan {
    /// Whether the plan asks for nothing.
    pub fn is_empty(&self) -> bool {
        self.enqueue.is_empty()
            && self.dependency_failed.is_empty()
            && self.start.is_empty()
            && self.restore.is_empty()
    }
}

/// The effective state of a job while planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    /// Queued, either already or by this plan's delayed enqueue.
    Queued,
    RestoreQueued,
    DoneSuccess,
    DoneFailed,
    /// Anything else (stashed, running, checkpointed, ...).
    Other,
}

impl Status {
    fn of(job: &Job, now: Timestamp) -> Self {
        match &job.state {
            JobState::Queued { .. } => Status::Queued,
            JobState::Stashed {
                enqueue_at: Some(t),
            } if *t <= now => Status::Queued,
            JobState::RestoreQueued { .. } => Status::RestoreQueued,
            JobState::Done {
                result: JobResult::Success,
            } => Status::DoneSuccess,
            JobState::Done { .. } => Status::DoneFailed,
            _ => Status::Other,
        }
    }
}

/// Resources held outside the job list: processes the helper runs that the
/// daemon does not account for (orphans).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reserved {
    /// GPU slots taken, per GPU.
    pub gpus: HashMap<GpuIndex, u32>,
    /// Jobs not to launch (a process with their id is still running).
    pub held: HashSet<JobId>,
}

/// Decides which jobs to enqueue, fail, start and restore at time `now`.
///
/// See the [module documentation](self) for the rules.
pub fn plan(now: Timestamp, jobs: &[Job], groups: &[Group], gpus: &[Gpu]) -> Plan {
    plan_with(now, jobs, groups, gpus, &Reserved::default())
}

/// [`plan`], with `reserved` GPU slots taken in addition to those of the
/// jobs, and `reserved.held` jobs left waiting.
pub fn plan_with(
    now: Timestamp,
    jobs: &[Job],
    groups: &[Group],
    gpus: &[Gpu],
    reserved: &Reserved,
) -> Plan {
    plan_inner(now, jobs, groups, gpus, reserved, None)
}

/// The inputs of the multifactor priority besides jobs and groups.
#[derive(Debug, Clone, Copy)]
pub struct PriorityInput<'a> {
    pub params: &'a Params,
    /// Fair-share factor per user (see [`priority::fair_share_factors`]).
    pub fairshare: &'a HashMap<Uid, f64>,
    pub resources: Resources,
}

/// [`plan_with`], with candidates ordered by `input` (see
/// [`crate::priority`]) instead of pueue's order. With
/// `priority_type = "basic"` the order is pueue's.
pub fn plan_prioritized(
    now: Timestamp,
    jobs: &[Job],
    groups: &[Group],
    gpus: &[Gpu],
    reserved: &Reserved,
    input: &PriorityInput<'_>,
) -> Plan {
    plan_inner(now, jobs, groups, gpus, reserved, Some(input))
}

/// The priorities of the jobs [`plan_prioritized`] would consider with the
/// same arguments, in the order it considers them, as `gpueue prio` shows
/// them: queued and restore-queued jobs (including delayed enqueues that
/// are due), without jobs this plan fails for a failed dependency and
/// without `reserved.held` jobs.
pub fn pending_priorities(
    now: Timestamp,
    jobs: &[Job],
    groups: &[Group],
    gpus: &[Gpu],
    reserved: &Reserved,
    input: &PriorityInput<'_>,
) -> Vec<JobPriority> {
    let prepared = prepare(now, jobs, groups, reserved);
    order(
        now,
        jobs,
        &prepared.status,
        &prepared.candidates,
        groups,
        gpus,
        input,
    )
}

/// What [`plan_inner`] and [`pending_priorities`] share: the effective
/// status of every job, the delayed enqueues and dependency failures of
/// this plan, and the candidates (unordered).
struct Prepared<'a> {
    status: HashMap<JobId, Status>,
    enqueue: Vec<JobId>,
    dependency_failed: Vec<JobId>,
    candidates: Vec<&'a Job>,
}

fn prepare<'a>(
    now: Timestamp,
    jobs: &'a [Job],
    group_list: &[Group],
    reserved: &Reserved,
) -> Prepared<'a> {
    let groups: HashMap<&str, &Group> = group_list.iter().map(|g| (g.name.as_str(), g)).collect();
    let group_paused = |name: &str| groups.get(name).is_some_and(|g| g.paused);
    let mut enqueue = Vec::new();
    let mut dependency_failed = Vec::new();

    let mut status: HashMap<JobId, Status> = HashMap::with_capacity(jobs.len());
    for job in jobs {
        let s = Status::of(job, now);
        if s == Status::Queued && matches!(job.state, JobState::Stashed { .. }) {
            enqueue.push(job.id);
        }
        status.insert(job.id, s);
    }

    // Failed dependencies, iterated to a fixed point so failures cascade.
    // Jobs are visited in id order to keep the output deterministic.
    let mut by_id: Vec<&Job> = jobs.iter().collect();
    by_id.sort_by_key(|j| j.id);
    loop {
        let mut changed = false;
        for job in &by_id {
            if status[&job.id] != Status::Queued || group_paused(&job.spec.group) {
                continue;
            }
            let failed = job
                .spec
                .dependencies
                .iter()
                .any(|dep| status.get(dep) == Some(&Status::DoneFailed));
            if failed {
                status.insert(job.id, Status::DoneFailed);
                dependency_failed.push(job.id);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    let candidates: Vec<&Job> = jobs
        .iter()
        .filter(|j| matches!(status[&j.id], Status::Queued | Status::RestoreQueued))
        .filter(|j| !reserved.held.contains(&j.id))
        .collect();
    Prepared {
        status,
        enqueue,
        dependency_failed,
        candidates,
    }
}

/// How `job`'s age is counted (see [`priority::Eligibility`]): since when
/// it is eligible, and whether it could launch now if a slot were free.
///
/// "Could launch now" uses the same conditions as the launch loop of
/// [`plan_inner`], except free slots: the group exists and is not paused,
/// every GPU of `spec.gpus` is in `gpus` (detected), and (queued jobs)
/// every dependency is done. A job waiting for a busy group or GPU slot is
/// ordinary queueing and counts as launchable.
fn eligibility(
    job: &Job,
    now: Timestamp,
    status: &HashMap<JobId, Status>,
    by_id: &HashMap<JobId, &Job>,
    groups: &HashMap<&str, &Group>,
    gpus: &HashSet<GpuIndex>,
) -> priority::Eligibility {
    let since = eligible_since(job, now, status, by_id);
    let launchable = since.is_some()
        && groups
            .get(job.spec.group.as_str())
            .is_some_and(|g| !g.paused)
        && job.spec.gpus.iter().all(|idx| gpus.contains(idx));
    priority::Eligibility { since, launchable }
}

/// When `job` became eligible to start (for the age factor): when it was
/// queued, or when its last dependency finished; `None` while a
/// dependency has not finished successfully.
fn eligible_since(
    job: &Job,
    now: Timestamp,
    status: &HashMap<JobId, Status>,
    by_id: &HashMap<JobId, &Job>,
) -> Option<Timestamp> {
    let queued_at = match &job.state {
        JobState::Queued { enqueued_at } | JobState::RestoreQueued { enqueued_at } => *enqueued_at,
        // A delayed enqueue that is due: queued by this plan.
        _ => return Some(now),
    };
    if matches!(job.state, JobState::RestoreQueued { .. }) {
        return Some(queued_at);
    }
    let mut since = queued_at;
    for dep in &job.spec.dependencies {
        match status.get(dep) {
            None => {}
            Some(Status::DoneSuccess) => {
                if let Some(at) = by_id.get(dep).and_then(|d| d.finished_at) {
                    since = since.max(at);
                }
            }
            Some(_) => return None,
        }
    }
    Some(since.min(now))
}

fn order(
    now: Timestamp,
    jobs: &[Job],
    status: &HashMap<JobId, Status>,
    candidates: &[&Job],
    group_list: &[Group],
    gpu_list: &[Gpu],
    input: &PriorityInput<'_>,
) -> Vec<JobPriority> {
    let by_id: HashMap<JobId, &Job> = jobs.iter().map(|j| (j.id, j)).collect();
    let groups: HashMap<&str, &Group> = group_list.iter().map(|g| (g.name.as_str(), g)).collect();
    let gpus: HashSet<GpuIndex> = gpu_list.iter().map(|g| g.index).collect();
    let ctx = priority::Context {
        params: input.params,
        now,
        fairshare: input.fairshare,
        groups: group_list,
        resources: input.resources,
    };
    priority::prioritize(candidates, &ctx, |job| {
        eligibility(job, now, status, &by_id, &groups, &gpus)
    })
}

fn plan_inner(
    now: Timestamp,
    jobs: &[Job],
    group_list: &[Group],
    gpus: &[Gpu],
    reserved: &Reserved,
    input: Option<&PriorityInput<'_>>,
) -> Plan {
    let Prepared {
        status,
        enqueue,
        dependency_failed,
        mut candidates,
    } = prepare(now, jobs, group_list, reserved);
    let mut out = Plan {
        enqueue,
        dependency_failed,
        ..Plan::default()
    };

    let gpu_list = gpus;
    let groups: HashMap<&str, &Group> = group_list.iter().map(|g| (g.name.as_str(), g)).collect();
    let gpus: HashMap<GpuIndex, &Gpu> = gpus.iter().map(|g| (g.index, g)).collect();

    // Slots currently in use.
    let mut group_used: HashMap<&str, u32> = HashMap::new();
    let mut gpu_used: HashMap<GpuIndex, u32> = reserved.gpus.clone();
    for job in jobs.iter().filter(|j| j.state.occupies_slot()) {
        *group_used.entry(job.spec.group.as_str()).or_default() += 1;
        for gpu in dedup(&job.spec.gpus) {
            *gpu_used.entry(gpu).or_default() += 1;
        }
    }

    match input {
        Some(input) => {
            let ordered = order(now, jobs, &status, &candidates, group_list, gpu_list, input);
            let by_id: HashMap<JobId, &Job> = candidates.iter().map(|j| (j.id, *j)).collect();
            candidates = ordered.iter().map(|p| by_id[&p.id]).collect();
        }
        None => candidates.sort_by(|a, b| {
            b.spec
                .priority
                .cmp(&a.spec.priority)
                .then_with(|| a.id.cmp(&b.id))
        }),
    }

    let has_room = |parallel: u32, used: u32| parallel == 0 || used < parallel;

    for job in candidates {
        let restoring = status[&job.id] == Status::RestoreQueued;

        let Some(group) = groups.get(job.spec.group.as_str()) else {
            continue;
        };
        if group.paused {
            continue;
        }
        let used = group_used.get(group.name.as_str()).copied().unwrap_or(0);
        if !has_room(group.parallel, used) {
            continue;
        }

        let job_gpus = dedup(&job.spec.gpus);
        let gpus_free = job_gpus.iter().all(|idx| {
            gpus.get(idx)
                .is_some_and(|gpu| has_room(gpu.parallel, gpu_used.get(idx).copied().unwrap_or(0)))
        });
        if !gpus_free {
            continue;
        }

        if !restoring {
            let deps_done = job
                .spec
                .dependencies
                .iter()
                .all(|dep| matches!(status.get(dep), None | Some(Status::DoneSuccess)));
            if !deps_done {
                continue;
            }
        }

        *group_used.entry(group.name.as_str()).or_default() += 1;
        for idx in job_gpus {
            *gpu_used.entry(idx).or_default() += 1;
        }
        if restoring {
            out.restore.push(job.id);
        } else {
            out.start.push(job.id);
        }
    }

    out
}

/// The distinct GPUs in `gpus`, so a repeated index takes only one slot.
fn dedup(gpus: &[GpuIndex]) -> Vec<GpuIndex> {
    let mut seen = HashSet::new();
    gpus.iter().copied().filter(|g| seen.insert(*g)).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use super::*;
    use crate::model::{DEFAULT_GROUP, JobSpec, Uid};

    const NOW: Timestamp = 1_000_000;

    fn queued() -> JobState {
        JobState::Queued { enqueued_at: 0 }
    }

    fn restore_queued() -> JobState {
        JobState::RestoreQueued { enqueued_at: 0 }
    }

    fn done(result: JobResult) -> JobState {
        JobState::Done { result }
    }

    fn failed() -> JobState {
        done(JobResult::Failed { exit_code: 1 })
    }

    fn success() -> JobState {
        done(JobResult::Success)
    }

    /// A job builder with sensible defaults.
    struct J(Job);

    fn job(id: u64, state: JobState) -> J {
        J(Job {
            id: JobId(id),
            owner: Uid(1000),
            spec: JobSpec {
                command: "true".into(),
                cwd: PathBuf::from("/"),
                envs: BTreeMap::new(),
                group: DEFAULT_GROUP.into(),
                gpus: Vec::new(),
                dependencies: Vec::new(),
                priority: 0,
                label: None,
                no_checkpoint: false,
                nice: 0,
            },
            state,
            created_at: 0,
            started_at: None,
            finished_at: None,
            site_factor: 0,
            top_seq: None,
        })
    }

    impl J {
        fn gpus(mut self, gpus: &[u32]) -> Self {
            self.0.spec.gpus = gpus.iter().map(|&g| GpuIndex(g)).collect();
            self
        }
        fn group(mut self, group: &str) -> Self {
            self.0.spec.group = group.into();
            self
        }
        fn prio(mut self, priority: i32) -> Self {
            self.0.spec.priority = priority;
            self
        }
        fn deps(mut self, deps: &[u64]) -> Self {
            self.0.spec.dependencies = deps.iter().map(|&d| JobId(d)).collect();
            self
        }
    }

    fn group(name: &str, parallel: u32) -> Group {
        Group {
            name: name.into(),
            parallel,
            paused: false,
            priority: 0,
        }
    }

    fn paused(name: &str, parallel: u32) -> Group {
        Group {
            paused: true,
            ..group(name, parallel)
        }
    }

    fn gpu(index: u32, parallel: u32) -> Gpu {
        Gpu {
            index: GpuIndex(index),
            uuid: format!("GPU-{index}"),
            name: "Test GPU".into(),
            parallel,
        }
    }

    fn ids(ids: &[u64]) -> Vec<JobId> {
        ids.iter().map(|&i| JobId(i)).collect()
    }

    fn run(jobs: Vec<J>, groups: &[Group], gpus: &[Gpu]) -> Plan {
        let jobs: Vec<Job> = jobs.into_iter().map(|j| j.0).collect();
        plan(NOW, &jobs, groups, gpus)
    }

    #[test]
    fn reserved_gpu_slots_and_held_jobs_are_respected() {
        let jobs: Vec<Job> = vec![
            job(1, queued()).gpus(&[0]).0,
            job(2, queued()).gpus(&[1]).0,
            job(3, queued()).0,
        ];
        let reserved = Reserved {
            gpus: HashMap::from([(GpuIndex(0), 1)]),
            held: HashSet::from([JobId(3)]),
        };
        let out = plan_with(
            NOW,
            &jobs,
            &[group(DEFAULT_GROUP, 0)],
            &[gpu(0, 1), gpu(1, 1)],
            &reserved,
        );
        // GPU 0 is taken by an orphan; job 3 shares its id with one.
        assert_eq!(out.start, ids(&[2]));
        let out = plan(
            NOW,
            &jobs,
            &[group(DEFAULT_GROUP, 0)],
            &[gpu(0, 1), gpu(1, 1)],
        );
        assert_eq!(out.start, ids(&[1, 2, 3]));
    }

    /// Expected (start, restore) for table-driven slot tests.
    struct Case {
        name: &'static str,
        jobs: Vec<J>,
        groups: Vec<Group>,
        gpus: Vec<Gpu>,
        start: &'static [u64],
        restore: &'static [u64],
    }

    fn check(cases: Vec<Case>) {
        for case in cases {
            let p = run(case.jobs, &case.groups, &case.gpus);
            assert_eq!(p.start, ids(case.start), "start: {}", case.name);
            assert_eq!(p.restore, ids(case.restore), "restore: {}", case.name);
        }
    }

    #[test]
    fn empty_input_gives_empty_plan() {
        let p = plan(NOW, &[], &[], &[]);
        assert!(p.is_empty());
    }

    #[test]
    fn group_and_gpu_slots() {
        check(vec![
            Case {
                name: "cpu jobs limited by group parallel",
                jobs: vec![job(1, queued()), job(2, queued()), job(3, queued())],
                groups: vec![group(DEFAULT_GROUP, 2)],
                gpus: vec![],
                start: &[1, 2],
                restore: &[],
            },
            Case {
                name: "running jobs consume group slots",
                jobs: vec![
                    job(1, JobState::Running),
                    job(2, JobState::Paused),
                    job(3, queued()),
                    job(4, queued()),
                ],
                groups: vec![group(DEFAULT_GROUP, 3)],
                gpus: vec![],
                start: &[3],
                restore: &[],
            },
            Case {
                name: "checkpointing and restoring jobs hold slots, checkpointed do not",
                jobs: vec![
                    job(1, JobState::Checkpointing).gpus(&[0]),
                    job(2, JobState::Restoring).gpus(&[1]),
                    job(3, JobState::Checkpointed).gpus(&[2]),
                    job(4, queued()).gpus(&[0]),
                    job(5, queued()).gpus(&[1]),
                    job(6, queued()).gpus(&[2]),
                ],
                groups: vec![group(DEFAULT_GROUP, 0)],
                gpus: vec![gpu(0, 1), gpu(1, 1), gpu(2, 1)],
                start: &[6],
                restore: &[],
            },
            Case {
                name: "group parallel 0 is unlimited",
                jobs: (1..=5).map(|i| job(i, queued())).collect(),
                groups: vec![group(DEFAULT_GROUP, 0)],
                gpus: vec![],
                start: &[1, 2, 3, 4, 5],
                restore: &[],
            },
            Case {
                name: "gpu parallel 1 allows one job per gpu",
                jobs: vec![
                    job(1, queued()).gpus(&[0]),
                    job(2, queued()).gpus(&[0]),
                    job(3, queued()).gpus(&[1]),
                ],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![gpu(0, 1), gpu(1, 1)],
                start: &[1, 3],
                restore: &[],
            },
            Case {
                name: "gpu parallel 2 allows two jobs on the same gpu",
                jobs: vec![
                    job(1, queued()).gpus(&[0]),
                    job(2, queued()).gpus(&[0]),
                    job(3, queued()).gpus(&[0]),
                ],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![gpu(0, 2)],
                start: &[1, 2],
                restore: &[],
            },
            Case {
                name: "gpu parallel 0 is unlimited",
                jobs: (1..=4).map(|i| job(i, queued()).gpus(&[0])).collect(),
                groups: vec![group(DEFAULT_GROUP, 0)],
                gpus: vec![gpu(0, 0)],
                start: &[1, 2, 3, 4],
                restore: &[],
            },
            Case {
                name: "multi-gpu job needs every gpu free",
                jobs: vec![
                    job(1, JobState::Running).gpus(&[1]),
                    job(2, queued()).gpus(&[0, 1]),
                    job(3, queued()).gpus(&[0]),
                ],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![gpu(0, 1), gpu(1, 1)],
                start: &[3],
                restore: &[],
            },
            Case {
                name: "multi-gpu jobs share a gpu with parallel 2",
                jobs: vec![
                    job(1, queued()).gpus(&[0, 1]),
                    job(2, queued()).gpus(&[1, 2]),
                    job(3, queued()).gpus(&[1]),
                    job(4, queued()).gpus(&[0]),
                ],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![gpu(0, 1), gpu(1, 2), gpu(2, 1)],
                // 1 takes gpu0 + gpu1, 2 takes gpu1 (now full) + gpu2;
                // 3 needs gpu1 (full), 4 needs gpu0 (full).
                start: &[1, 2],
                restore: &[],
            },
            Case {
                name: "duplicate gpu index takes a single slot",
                jobs: vec![job(1, queued()).gpus(&[0, 0]), job(2, queued()).gpus(&[0])],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![gpu(0, 2)],
                start: &[1, 2],
                restore: &[],
            },
            Case {
                name: "cpu jobs ignore gpu slots",
                jobs: vec![
                    job(1, JobState::Running).gpus(&[0]),
                    job(2, queued()).gpus(&[0]),
                    job(3, queued()),
                ],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![gpu(0, 1)],
                start: &[3],
                restore: &[],
            },
            Case {
                name: "gpu slots are shared across groups",
                jobs: vec![
                    job(1, JobState::Running).group("a").gpus(&[0]),
                    job(2, queued()).group("b").gpus(&[0]),
                    job(3, queued()).group("b").gpus(&[1]),
                ],
                groups: vec![group("a", 1), group("b", 1)],
                gpus: vec![gpu(0, 1), gpu(1, 1)],
                start: &[3],
                restore: &[],
            },
            Case {
                name: "group slot full blocks even when gpu free",
                jobs: vec![
                    job(1, JobState::Running).group("a"),
                    job(2, queued()).group("a").gpus(&[0]),
                    job(3, queued()).group("b").gpus(&[0]),
                ],
                groups: vec![group("a", 1), group("b", 1)],
                gpus: vec![gpu(0, 1)],
                start: &[3],
                restore: &[],
            },
            Case {
                name: "unknown gpu never starts",
                jobs: vec![job(1, queued()).gpus(&[0, 7]), job(2, queued()).gpus(&[0])],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![gpu(0, 1)],
                start: &[2],
                restore: &[],
            },
            Case {
                name: "unknown group never starts",
                jobs: vec![job(1, queued()).group("nope"), job(2, queued())],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![],
                start: &[2],
                restore: &[],
            },
            Case {
                name: "paused group does not start, other groups do",
                jobs: vec![job(1, queued()).group("a"), job(2, queued()).group("b")],
                groups: vec![paused("a", 10), group("b", 10)],
                gpus: vec![],
                start: &[2],
                restore: &[],
            },
            Case {
                name: "running jobs in a paused group still hold gpu slots",
                jobs: vec![
                    job(1, JobState::Running).group("a").gpus(&[0]),
                    job(2, queued()).group("b").gpus(&[0]),
                ],
                groups: vec![paused("a", 10), group("b", 10)],
                gpus: vec![gpu(0, 1)],
                start: &[],
                restore: &[],
            },
            Case {
                name: "done, stashed, checkpointed jobs are not started",
                jobs: vec![
                    job(1, success()),
                    job(2, JobState::Stashed { enqueue_at: None }),
                    job(
                        3,
                        JobState::Stashed {
                            enqueue_at: Some(NOW + 1),
                        },
                    ),
                    job(4, JobState::Checkpointed),
                ],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![],
                start: &[],
                restore: &[],
            },
        ]);
    }

    #[test]
    fn ordering_and_priority() {
        check(vec![
            Case {
                name: "lower id first at equal priority",
                jobs: vec![job(3, queued()), job(1, queued()), job(2, queued())],
                groups: vec![group(DEFAULT_GROUP, 2)],
                gpus: vec![],
                start: &[1, 2],
                restore: &[],
            },
            Case {
                name: "higher priority first",
                jobs: vec![
                    job(1, queued()),
                    job(2, queued()).prio(5),
                    job(3, queued()).prio(-1),
                    job(4, queued()).prio(5),
                ],
                groups: vec![group(DEFAULT_GROUP, 3)],
                gpus: vec![],
                start: &[2, 4, 1],
                restore: &[],
            },
            Case {
                name: "priority decides who gets the gpu",
                jobs: vec![
                    job(1, queued()).gpus(&[0]),
                    job(2, queued()).gpus(&[0]).prio(1),
                ],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![gpu(0, 1)],
                start: &[2],
                restore: &[],
            },
            Case {
                // No head-of-line blocking: job 1 cannot fit, so job 2 backfills.
                name: "blocked high-priority job does not block jobs that fit",
                jobs: vec![
                    job(9, JobState::Running).gpus(&[0]),
                    job(1, queued()).gpus(&[0, 1]).prio(10),
                    job(2, queued()).gpus(&[1]),
                ],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![gpu(0, 1), gpu(1, 1)],
                start: &[2],
                restore: &[],
            },
            Case {
                name: "restore-queued and queued compete by priority then id",
                jobs: vec![
                    job(1, queued()).gpus(&[0]),
                    job(2, restore_queued()).gpus(&[0]),
                    job(3, restore_queued()).gpus(&[1]),
                    job(4, queued()).gpus(&[1]).prio(1),
                ],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![gpu(0, 1), gpu(1, 1)],
                // Decision order: 4 (prio 1) takes gpu1, then 1 takes gpu0;
                // restore-queued 2 and 3 find their gpus full.
                start: &[4, 1],
                restore: &[],
            },
            Case {
                name: "restore-queued with lower id wins at equal priority",
                jobs: vec![
                    job(1, restore_queued()).gpus(&[0]),
                    job(2, queued()).gpus(&[0]),
                ],
                groups: vec![group(DEFAULT_GROUP, 10)],
                gpus: vec![gpu(0, 1)],
                start: &[],
                restore: &[1],
            },
            Case {
                name: "restore-queued uses the group slot too",
                jobs: vec![
                    job(1, restore_queued()),
                    job(2, queued()),
                    job(3, restore_queued()),
                ],
                groups: vec![group(DEFAULT_GROUP, 2)],
                gpus: vec![],
                start: &[2],
                restore: &[1],
            },
            Case {
                name: "restore-queued in a paused group waits",
                jobs: vec![job(1, restore_queued()).group("a")],
                groups: vec![paused("a", 1)],
                gpus: vec![],
                start: &[],
                restore: &[],
            },
        ]);
    }

    #[test]
    fn dependencies() {
        let groups = [group(DEFAULT_GROUP, 10)];

        // Waits for an unfinished dependency.
        for dep_state in [
            queued(),
            JobState::Running,
            JobState::Paused,
            JobState::Stashed { enqueue_at: None },
            JobState::Checkpointed,
            restore_queued(),
        ] {
            let p = run(
                vec![job(1, dep_state.clone()), job(2, queued()).deps(&[1])],
                &groups,
                &[],
            );
            assert!(!p.start.contains(&JobId(2)), "{dep_state:?}");
            assert!(p.dependency_failed.is_empty(), "{dep_state:?}");
        }

        // Starts after success; unknown dependencies are ignored.
        let p = run(
            vec![job(1, success()), job(2, queued()).deps(&[1, 99])],
            &groups,
            &[],
        );
        assert_eq!(p.start, ids(&[2]));

        // Every non-success result fails the dependent.
        for result in [
            JobResult::Failed { exit_code: 2 },
            JobResult::FailedToSpawn { reason: "x".into() },
            JobResult::Killed,
            JobResult::DependencyFailed,
            JobResult::CheckpointFailed { reason: "x".into() },
            JobResult::Lost,
        ] {
            let p = run(
                vec![job(1, done(result.clone())), job(2, queued()).deps(&[1])],
                &groups,
                &[],
            );
            assert_eq!(p.dependency_failed, ids(&[2]), "{result:?}");
            assert!(p.start.is_empty(), "{result:?}");
        }
    }

    #[test]
    fn dependency_failure_needs_only_one_failed_dependency() {
        let p = run(
            vec![
                job(1, success()),
                job(2, failed()),
                job(3, JobState::Running),
                job(4, queued()).deps(&[1, 2, 3]),
            ],
            &[group(DEFAULT_GROUP, 10)],
            &[],
        );
        assert_eq!(p.dependency_failed, ids(&[4]));
    }

    #[test]
    fn dependency_failure_cascades_in_one_plan() {
        let p = run(
            vec![
                job(1, failed()),
                // Out of id order on purpose: 5 depends on 4, which depends on 1.
                job(5, queued()).deps(&[4]),
                job(4, queued()).deps(&[1]),
                job(6, queued()),
            ],
            &[group(DEFAULT_GROUP, 10)],
            &[],
        );
        assert_eq!(p.dependency_failed, ids(&[4, 5]));
        assert_eq!(p.start, ids(&[6]));
    }

    #[test]
    fn dependency_failure_skipped_in_paused_group() {
        let p = run(
            vec![
                job(1, failed()).group("a"),
                job(2, queued()).group("a").deps(&[1]),
                job(3, queued()).group("b").deps(&[1]),
            ],
            &[paused("a", 10), group("b", 10)],
            &[],
        );
        assert_eq!(p.dependency_failed, ids(&[3]));
        assert!(p.start.is_empty());
    }

    #[test]
    fn dependency_failure_ignores_non_queued_jobs() {
        let p = run(
            vec![
                job(1, failed()),
                job(2, JobState::Stashed { enqueue_at: None }).deps(&[1]),
                job(3, restore_queued()).deps(&[1]),
            ],
            &[group(DEFAULT_GROUP, 10)],
            &[],
        );
        assert!(p.dependency_failed.is_empty());
        // Restore-queued jobs do not re-check dependencies.
        assert_eq!(p.restore, ids(&[3]));
    }

    #[test]
    fn failed_dependency_frees_no_slot_for_itself() {
        // Job 2 fails on its dependency and must not consume the only slot.
        let p = run(
            vec![
                job(1, failed()),
                job(2, queued()).deps(&[1]).prio(5),
                job(3, queued()),
            ],
            &[group(DEFAULT_GROUP, 1)],
            &[],
        );
        assert_eq!(p.dependency_failed, ids(&[2]));
        assert_eq!(p.start, ids(&[3]));
    }

    #[test]
    fn stashed_enqueue_at() {
        let p = run(
            vec![
                job(
                    1,
                    JobState::Stashed {
                        enqueue_at: Some(NOW - 1),
                    },
                ),
                job(
                    2,
                    JobState::Stashed {
                        enqueue_at: Some(NOW),
                    },
                ),
                job(
                    3,
                    JobState::Stashed {
                        enqueue_at: Some(NOW + 1),
                    },
                ),
                job(4, JobState::Stashed { enqueue_at: None }),
            ],
            &[group(DEFAULT_GROUP, 1)],
            &[],
        );
        assert_eq!(p.enqueue, ids(&[1, 2]));
        // Newly enqueued jobs are started in the same plan if they fit.
        assert_eq!(p.start, ids(&[1]));
    }

    #[test]
    fn stashed_enqueue_in_paused_group_only_enqueues() {
        let p = run(
            vec![job(
                1,
                JobState::Stashed {
                    enqueue_at: Some(NOW),
                },
            )],
            &[paused(DEFAULT_GROUP, 1)],
            &[],
        );
        assert_eq!(p.enqueue, ids(&[1]));
        assert!(p.start.is_empty());
    }

    #[test]
    fn newly_enqueued_job_with_failed_dependency_fails() {
        let p = run(
            vec![
                job(1, failed()),
                job(
                    2,
                    JobState::Stashed {
                        enqueue_at: Some(NOW),
                    },
                )
                .deps(&[1]),
            ],
            &[group(DEFAULT_GROUP, 1)],
            &[],
        );
        assert_eq!(p.enqueue, ids(&[2]));
        assert_eq!(p.dependency_failed, ids(&[2]));
        assert!(p.start.is_empty());
    }

    #[test]
    fn dependency_started_in_same_plan_does_not_satisfy() {
        let p = run(
            vec![job(1, queued()), job(2, queued()).deps(&[1])],
            &[group(DEFAULT_GROUP, 10)],
            &[],
        );
        assert_eq!(p.start, ids(&[1]));
    }
    // ----- multifactor priority -----

    impl J {
        fn owner(mut self, uid: u32) -> Self {
            self.0.owner = Uid(uid);
            self
        }
        fn at(mut self, enqueued_at: Timestamp) -> Self {
            self.0.state = match self.0.state {
                JobState::RestoreQueued { .. } => JobState::RestoreQueued { enqueued_at },
                _ => JobState::Queued { enqueued_at },
            };
            self
        }
    }

    fn prioritized(
        jobs: Vec<J>,
        groups: &[Group],
        gpus: &[Gpu],
        params: &Params,
        fairshare: &HashMap<Uid, f64>,
    ) -> Plan {
        let jobs: Vec<Job> = jobs.into_iter().map(|j| j.0).collect();
        let input = PriorityInput {
            params,
            fairshare,
            resources: Resources {
                gpus: gpus.len() as u32,
                cpus: 8,
            },
        };
        plan_prioritized(NOW, &jobs, groups, gpus, &Reserved::default(), &input)
    }

    #[test]
    fn lower_usage_user_gets_the_gpu_first() {
        // User 1 submitted first, but used the machine a lot.
        let fairshare = HashMap::from([(Uid(1), 0.1), (Uid(2), 0.9)]);
        let jobs = || {
            vec![
                job(1, queued()).owner(1).gpus(&[0]).at(NOW - 1000),
                job(2, queued()).owner(2).gpus(&[0]).at(NOW),
            ]
        };
        let p = prioritized(
            jobs(),
            &[group(DEFAULT_GROUP, 0)],
            &[gpu(0, 1)],
            &Params::default(),
            &fairshare,
        );
        assert_eq!(p.start, ids(&[2]));
        // Basic priority: pueue's order.
        let basic = Params {
            kind: crate::priority::PriorityType::Basic,
            ..Params::default()
        };
        let p = prioritized(
            jobs(),
            &[group(DEFAULT_GROUP, 0)],
            &[gpu(0, 1)],
            &basic,
            &fairshare,
        );
        assert_eq!(p.start, ids(&[1]));
        let p = run(jobs(), &[group(DEFAULT_GROUP, 0)], &[gpu(0, 1)]);
        assert_eq!(p.start, ids(&[1]));
    }

    #[test]
    fn prioritized_plan_keeps_slots_dependencies_and_pauses() {
        let fairshare = HashMap::from([(Uid(1), 1.0), (Uid(2), 0.0)]);
        let p = prioritized(
            vec![
                // Best priority, but its dependency has not finished.
                job(1, queued()).owner(1).deps(&[9]),
                job(9, JobState::Running).owner(1),
                // Paused group.
                job(2, queued()).owner(1).group("held"),
                // The GPU is taken by job 3.
                job(3, JobState::Running).owner(2).gpus(&[0]),
                job(4, queued()).owner(1).gpus(&[0]),
                // Fits.
                job(5, queued()).owner(2),
                job(6, restore_queued()).owner(1),
            ],
            &[group(DEFAULT_GROUP, 0), paused("held", 0)],
            &[gpu(0, 1)],
            &Params::default(),
            &fairshare,
        );
        assert_eq!(p.start, ids(&[5]));
        assert_eq!(p.restore, ids(&[6]));
        // A high-priority job blocked on a GPU does not block others (no
        // head-of-line blocking), as with basic priority.
        let p = prioritized(
            vec![
                job(1, queued()).owner(1).gpus(&[0, 1]),
                job(2, JobState::Running).owner(2).gpus(&[1]),
                job(3, queued()).owner(2).gpus(&[0]),
            ],
            &[group(DEFAULT_GROUP, 0)],
            &[gpu(0, 1), gpu(1, 1)],
            &Params::default(),
            &HashMap::from([(Uid(2), 0.0)]),
        );
        assert_eq!(p.start, ids(&[3]));
    }

    #[test]
    fn group_parallel_limit_goes_to_the_best_priority() {
        let fairshare = HashMap::from([(Uid(1), 0.2), (Uid(2), 0.8)]);
        let p = prioritized(
            vec![
                job(1, queued()).owner(1),
                job(2, queued()).owner(1),
                job(3, queued()).owner(2),
                job(4, queued()).owner(2),
            ],
            &[group(DEFAULT_GROUP, 3)],
            &[],
            &Params::default(),
            &fairshare,
        );
        assert_eq!(p.start, ids(&[3, 4, 1]));
    }

    #[test]
    fn age_counts_from_the_last_dependency() {
        let finished = |id: u64, at: Timestamp| {
            let mut j = job(id, success());
            j.0.finished_at = Some(at);
            j
        };
        let jobs: Vec<Job> = vec![
            finished(1, NOW - 1000).0,
            job(2, queued()).deps(&[1]).at(NOW - 5000).0,
            job(3, queued()).owner(2).at(NOW - 3000).0,
        ];
        let params = Params {
            weight_fairshare: 0,
            max_age_ms: 10_000,
            ..Params::default()
        };
        let fairshare = HashMap::new();
        let input = PriorityInput {
            params: &params,
            fairshare: &fairshare,
            resources: Resources::default(),
        };
        let p = pending_priorities(
            NOW,
            &jobs,
            &[group(DEFAULT_GROUP, 0)],
            &[],
            &Reserved::default(),
            &input,
        );
        // Job 2 waited since its dependency finished (1 s), job 3 for 3 s.
        assert_eq!(p.iter().map(|p| p.id.0).collect::<Vec<_>>(), [3, 2]);
        assert!((p[1].factors.age - 0.1).abs() < 1e-9);
        assert!((p[0].factors.age - 0.3).abs() < 1e-9);
    }

    /// The review scenario of the "age bank": user 1 has an 8-day-old job
    /// that cannot launch and a new job first in their own order (`-o
    /// 10`); user 2 has a 3-day-old job. One CPU slot is free.
    fn parked_job_scenario(parked: J, groups: &[Group], gpus: &[Gpu]) -> (Plan, Vec<JobPriority>) {
        const DAY: Timestamp = 24 * 60 * 60 * 1000;
        let jobs: Vec<Job> = vec![
            parked.owner(1).at(NOW - 8 * DAY).0,
            job(2, queued()).owner(1).prio(10).at(NOW).0,
            job(3, queued()).owner(2).at(NOW - 3 * DAY).0,
            job(4, JobState::Running).owner(3).group("gpu").gpus(&[0]).0,
        ];
        let params = Params::default();
        let fairshare = HashMap::new();
        let input = PriorityInput {
            params: &params,
            fairshare: &fairshare,
            resources: Resources {
                gpus: gpus.len() as u32,
                cpus: 8,
            },
        };
        let reserved = Reserved::default();
        (
            plan_prioritized(NOW, &jobs, groups, gpus, &reserved, &input),
            pending_priorities(NOW, &jobs, groups, gpus, &reserved, &input),
        )
    }

    #[test]
    fn jobs_that_cannot_launch_lend_no_age() {
        let age = |p: &[JobPriority], id: u64| p.iter().find(|p| p.id.0 == id).unwrap().factors.age;
        let groups = [group(DEFAULT_GROUP, 1), paused("held", 0), group("gpu", 0)];
        let gpus = [gpu(0, 1)];
        for (what, parked) in [
            ("paused group", job(1, queued()).group("held")),
            ("missing group", job(1, queued()).group("gone")),
            ("undetected GPU", job(1, queued()).group("gpu").gpus(&[7])),
        ] {
            let (plan, p) = parked_job_scenario(parked, &groups, &gpus);
            assert_eq!(plan.start, ids(&[3]), "{what}");
            assert_eq!(age(&p, 2), 0.0, "{what}");
            assert!((age(&p, 3) - 3.0 / 7.0).abs() < 1e-9, "{what}");
            // The parked job keeps its own age for when it can launch.
            assert_eq!(age(&p, 1), 1.0, "{what}");
        }
        // Once the group is resumed the parked job pools its age again:
        // the owner's first job gets it.
        let resumed = [group(DEFAULT_GROUP, 1), group("held", 0), group("gpu", 0)];
        let (_, p) = parked_job_scenario(job(1, queued()).group("held"), &resumed, &gpus);
        assert_eq!(age(&p, 2), 1.0);
        assert_eq!(age(&p, 1), 0.0);
        // Waiting for a busy GPU is ordinary queueing: the job could launch
        // as soon as the slot frees, so it pools its age.
        let (plan, p) =
            parked_job_scenario(job(1, queued()).group("gpu").gpus(&[0]), &groups, &gpus);
        assert_eq!(age(&p, 2), 1.0);
        assert_eq!(plan.start, ids(&[2]));
    }

    #[test]
    fn pending_priorities_list_what_the_plan_considers() {
        let jobs: Vec<Job> = vec![
            job(1, failed()).0,
            // Fails for its dependency in this plan.
            job(2, queued()).deps(&[1]).0,
            // Held (an orphan with its id runs).
            job(3, queued()).0,
            job(4, queued()).owner(2).0,
            job(5, restore_queued()).0,
            job(
                6,
                JobState::Stashed {
                    enqueue_at: Some(NOW - 1),
                },
            )
            .0,
        ];
        let reserved = Reserved {
            gpus: HashMap::new(),
            held: HashSet::from([JobId(3)]),
        };
        let params = Params::default();
        let fairshare = HashMap::new();
        let input = PriorityInput {
            params: &params,
            fairshare: &fairshare,
            resources: Resources::default(),
        };
        let groups = [group(DEFAULT_GROUP, 0)];
        let listed: HashSet<u64> = pending_priorities(NOW, &jobs, &groups, &[], &reserved, &input)
            .iter()
            .map(|p| p.id.0)
            .collect();
        assert_eq!(listed, HashSet::from([4, 5, 6]));
        let plan = plan_prioritized(NOW, &jobs, &groups, &[], &reserved, &input);
        let launched: HashSet<u64> = plan
            .start
            .iter()
            .chain(&plan.restore)
            .map(|id| id.0)
            .collect();
        assert_eq!(launched, listed);
    }
}
