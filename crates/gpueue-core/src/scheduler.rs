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
//!    for the same slots and are considered in one list. The list is ordered
//!    by [`order_key`] first (higher first): the tier of the job's group
//!    ([`Group::tier`]) and its [`YieldMode`], so jobs of a higher tier come
//!    first, `--yield` jobs come after every job that does not yield, and
//!    `--yield=group` jobs come after the jobs of the same tier that do not
//!    yield. Within the same key the order is, with [`plan`] and
//!    [`plan_with`], higher `-o` priority first, then lower [`JobId`]
//!    (pueue's order); with [`plan_prioritized`] and [`plan_preemptive`] the
//!    multifactor priority of [`crate::priority`]. Without tiers and yields
//!    every job has the same key and the order is unchanged. A job is
//!    launched when
//!    - its group exists and is not paused,
//!    - its group has a free slot,
//!    - every GPU in `spec.gpus` exists and has a free slot, and
//!    - (queued jobs only) every dependency is `Done { Success }`.
//!
//!    Restore-queued jobs skip the dependency check: they already ran, so
//!    their dependencies were satisfied when they first started.
//! 4. **Preemption** (only [`plan_preemptive`] with
//!    [`PreemptInput::enabled`]; Slurm's `preempt/partition_prio` with
//!    `PreemptMode=CHECKPOINT`). After the launch pass, every candidate `J`
//!    that was not launched is considered in the order above. If `J` could
//!    launch now except for free slots (rule 3 without the slot conditions,
//!    and not `reserved.held`), the scheduler may choose running jobs to
//!    checkpoint ([`Plan::preempt`]) so that `J` fits. A job `V` is a
//!    candidate victim when
//!    - it is `Running` or `Paused`,
//!    - `J` may preempt it ([`may_preempt`]),
//!    - it is in `J`'s group or uses one of `J`'s GPUs (so stopping it frees
//!      a slot `J` needs),
//!    - it is not `no_checkpoint`,
//!    - its checkpoint for a preemption was not refused recently
//!      ([`PreemptInput::refused`]),
//!    - it has been running for at least [`PreemptInput::exempt_ms`] since
//!      it last started running (launch or restore), and
//!    - it was not chosen for an earlier `J` of the same plan.
//!
//!    Candidates are added lowest first (lowest [`order_key`], then lowest
//!    multifactor priority without the age factor, or with basic priority
//!    the last in pueue's order, then the most recently started) until `J`
//!    fits; then, in reverse order, every victim without which `J` still
//!    fits is dropped. If `J` does not fit even with every candidate
//!    stopped, nothing is stopped for it. The slots freed by the chosen
//!    victims are set aside for `J`: later candidates of the same plan
//!    cannot count them. Jobs already being checkpointed for a preemption
//!    (`Checkpointing` with [`Job::preempted_at`]) count as freed for this
//!    computation, and the first `J` in order that fits thanks to them
//!    takes them, so a preemption is not repeated on the next tick while
//!    the checkpoint runs. For launching they still occupy their slots
//!    until the checkpoint has finished. Since a job that may preempt
//!    another is always ordered before it, the slots freed by a
//!    preemption go to `J` before the victim is restored.
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
//! counter that by pausing groups or adjusting priorities. Preemption does
//! not change this: it only stops jobs for a job that then fits.
//!
//! **Unknown dependencies** (ids that no longer exist, e.g. removed or
//! cleaned jobs) are ignored, i.e. treated as satisfied. This is what pueue
//! does; validating ids at submission time is the caller's job.
//!
//! Jobs whose group or GPUs are unknown are never launched and are left
//! untouched; the daemon should report them rather than guess. They never
//! cause a preemption either.
//!
//! [`occupies_slot`]: JobState::occupies_slot

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use crate::model::{
    Gpu, GpuIndex, Group, Job, JobId, JobResult, JobState, Timestamp, Uid, YieldMode,
};
use crate::priority::{self, JobPriority, Params, PriorityType, Resources};

/// What the daemon should do, as decided by [`plan`].
///
/// The daemon should apply the lists in field order: `enqueue`, then
/// `dependency_failed`, then `start` and `restore`, then `preempt`. Every
/// list is in decision order and contains each job at most once; `start`
/// may contain jobs that also appear in `enqueue`. `start` and `restore`
/// never count on the slots of the jobs in `preempt`.
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
    /// Running or paused jobs to checkpoint to make room for a waiting job
    /// that may preempt them (see the [module documentation](self)). Only
    /// [`plan_preemptive`] fills this.
    pub preempt: Vec<JobId>,
}

impl Plan {
    /// Whether the plan asks for nothing.
    pub fn is_empty(&self) -> bool {
        self.enqueue.is_empty()
            && self.dependency_failed.is_empty()
            && self.start.is_empty()
            && self.restore.is_empty()
            && self.preempt.is_empty()
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

/// The key candidates are ordered by before anything else (higher first):
/// `(1, tier, 1)` for a job that does not yield, `(1, tier, 0)` for
/// `--yield=group` and `(0, tier, 0)` for `--yield`, where `tier` is the
/// tier of the job's group (0 if the group is unknown).
///
/// A job that [may preempt](may_preempt) another always has a higher key.
pub fn order_key(job: &Job, groups: &[Group]) -> (u8, i32, u8) {
    let tier = priority::group_tier(job, groups);
    match job.spec.yield_mode {
        YieldMode::None => (1, tier, 1),
        YieldMode::Group => (1, tier, 0),
        YieldMode::All => (0, tier, 0),
    }
}

/// Whether the waiting job `waiting` may preempt (have checkpointed) the
/// running job `running`: either `waiting` ranks strictly higher, where
/// the rank is `(0 if --yield else 1, tier of the group)`, or both are in
/// the same group, `running` yields to its group (`--yield=group`) and
/// `waiting` does not yield.
///
/// This is not a total order: jobs of different groups with the same tier
/// that both do not yield to everybody never preempt each other.
pub fn may_preempt(waiting: &Job, running: &Job, groups: &[Group]) -> bool {
    let rank = |job: &Job| {
        let (yields, tier, _) = order_key(job, groups);
        (yields, tier)
    };
    rank(waiting) > rank(running)
        || (waiting.spec.group == running.spec.group
            && running.spec.yield_mode == YieldMode::Group
            && waiting.spec.yield_mode == YieldMode::None)
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
    plan_inner(now, jobs, groups, gpus, reserved, None, None)
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
/// `priority_type = "basic"` the order is pueue's. Never preempts:
/// [`plan_preemptive`] with preemption disabled.
pub fn plan_prioritized(
    now: Timestamp,
    jobs: &[Job],
    groups: &[Group],
    gpus: &[Gpu],
    reserved: &Reserved,
    input: &PriorityInput<'_>,
) -> Plan {
    plan_inner(now, jobs, groups, gpus, reserved, Some(input), None)
}

/// Inputs of preemption (see the [module documentation](self)).
#[derive(Debug, Clone, Copy)]
pub struct PreemptInput<'a> {
    /// `preemption` setting; false: never preempt.
    pub enabled: bool,
    /// `preempt_exempt_secs` in ms: a job that started running (launched
    /// or restored) less than this ago is not preempted.
    pub exempt_ms: i64,
    /// Jobs whose preemption checkpoint was refused recently: never chosen.
    pub refused: &'a HashSet<JobId>,
    /// When each running job last started running (launch or restore). A
    /// job without an entry is not exempt.
    pub running_since: &'a HashMap<JobId, Timestamp>,
}

/// [`plan_prioritized`], and with `preempt.enabled` also the jobs to
/// checkpoint so that waiting jobs of a higher tier (or jobs others yield
/// to) can start ([`Plan::preempt`]).
pub fn plan_preemptive(
    now: Timestamp,
    jobs: &[Job],
    groups: &[Group],
    gpus: &[Gpu],
    reserved: &Reserved,
    input: &PriorityInput<'_>,
    preempt: &PreemptInput<'_>,
) -> Plan {
    let preempt = preempt.enabled.then_some(preempt);
    plan_inner(now, jobs, groups, gpus, reserved, Some(input), preempt)
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
    let mut ordered = priority::prioritize(candidates, &ctx, |job| {
        eligibility(job, now, status, &by_id, &groups, &gpus)
    });
    // Stable: the priority order is kept within each key.
    ordered.sort_by_cached_key(|p| Reverse(order_key(by_id[&p.id], group_list)));
    ordered
}

/// Slots in use, per group and per GPU.
#[derive(Debug, Clone)]
struct Usage<'a> {
    group: HashMap<&'a str, u32>,
    gpu: HashMap<GpuIndex, u32>,
}

impl<'a> Usage<'a> {
    fn add(&mut self, job: &'a Job) {
        *self.group.entry(job.spec.group.as_str()).or_default() += 1;
        for gpu in dedup(&job.spec.gpus) {
            *self.gpu.entry(gpu).or_default() += 1;
        }
    }

    fn remove(&mut self, job: &Job) {
        if let Some(used) = self.group.get_mut(job.spec.group.as_str()) {
            *used = used.saturating_sub(1);
        }
        for gpu in dedup(&job.spec.gpus) {
            if let Some(used) = self.gpu.get_mut(&gpu) {
                *used = used.saturating_sub(1);
            }
        }
    }

    /// Whether `job`'s group and every one of its GPUs have a free slot.
    /// A `parallel` of 0 is unlimited; unknown groups and GPUs never fit.
    fn fits(
        &self,
        job: &Job,
        groups: &HashMap<&str, &Group>,
        gpus: &HashMap<GpuIndex, &Gpu>,
    ) -> bool {
        let has_room = |parallel: u32, used: Option<&u32>| {
            parallel == 0 || used.copied().unwrap_or(0) < parallel
        };
        groups
            .get(job.spec.group.as_str())
            .is_some_and(|g| has_room(g.parallel, self.group.get(g.name.as_str())))
            && dedup(&job.spec.gpus).iter().all(|idx| {
                gpus.get(idx)
                    .is_some_and(|gpu| has_room(gpu.parallel, self.gpu.get(idx)))
            })
    }
}

fn plan_inner(
    now: Timestamp,
    jobs: &[Job],
    group_list: &[Group],
    gpus: &[Gpu],
    reserved: &Reserved,
    input: Option<&PriorityInput<'_>>,
    preempt: Option<&PreemptInput<'_>>,
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
    let mut used = Usage {
        group: HashMap::new(),
        gpu: reserved.gpus.clone(),
    };
    for job in jobs.iter().filter(|j| j.state.occupies_slot()) {
        used.add(job);
    }

    match input {
        Some(input) => {
            let ordered = order(now, jobs, &status, &candidates, group_list, gpu_list, input);
            let by_id: HashMap<JobId, &Job> = candidates.iter().map(|j| (j.id, *j)).collect();
            candidates = ordered.iter().map(|p| by_id[&p.id]).collect();
        }
        None => {
            candidates = crate::priority::basic_order(&candidates);
            // Stable: pueue's order is kept within each key.
            candidates.sort_by_cached_key(|job| Reverse(order_key(job, group_list)));
        }
    }

    // Candidates that could launch but for a free slot, in order.
    let mut waiting = Vec::new();
    for job in candidates {
        let restoring = status[&job.id] == Status::RestoreQueued;
        let launchable = groups
            .get(job.spec.group.as_str())
            .is_some_and(|g| !g.paused)
            && job.spec.gpus.iter().all(|idx| gpus.contains_key(idx))
            && (restoring
                || job
                    .spec
                    .dependencies
                    .iter()
                    .all(|dep| matches!(status.get(dep), None | Some(Status::DoneSuccess))));
        if !launchable {
            continue;
        }
        if !used.fits(job, &groups, &gpus) {
            waiting.push(job);
            continue;
        }
        used.add(job);
        if restoring {
            out.restore.push(job.id);
        } else {
            out.start.push(job.id);
        }
    }

    if let Some(preempt) = preempt
        && !waiting.is_empty()
    {
        let ctx = Victims {
            now,
            jobs,
            group_list,
            groups: &groups,
            gpus: &gpus,
            input,
            preempt,
        };
        out.preempt = ctx.choose(&waiting, &used);
    }

    out
}

/// What choosing preemption victims needs.
struct Victims<'p, 'a> {
    now: Timestamp,
    jobs: &'a [Job],
    group_list: &'p [Group],
    groups: &'p HashMap<&'p str, &'p Group>,
    gpus: &'p HashMap<GpuIndex, &'p Gpu>,
    input: Option<&'p PriorityInput<'p>>,
    preempt: &'p PreemptInput<'p>,
}

impl<'a> Victims<'_, 'a> {
    /// The jobs to preempt for the `waiting` jobs (in order), given the
    /// slots `used` after the launch pass.
    fn choose(&self, waiting: &[&'a Job], used: &Usage<'a>) -> Vec<JobId> {
        // The slots as they will be once the preemptions in progress have
        // finished; each waiting job adds what it is going to take.
        let mut planned = used.clone();
        for job in self
            .jobs
            .iter()
            .filter(|j| j.state == JobState::Checkpointing && j.preempted_at.is_some())
        {
            planned.remove(job);
        }
        let pool = self.pool();
        let mut chosen: HashSet<JobId> = HashSet::new();
        let mut out = Vec::new();
        for &job in waiting {
            if planned.fits(job, self.groups, self.gpus) {
                // Waits for a preemption in progress; nobody else gets
                // those slots.
                planned.add(job);
                continue;
            }
            let mut trial = planned.clone();
            let mut victims: Vec<&Job> = Vec::new();
            for &victim in &pool {
                if chosen.contains(&victim.id)
                    || !may_preempt(job, victim, self.group_list)
                    || !frees_slot_for(victim, job)
                {
                    continue;
                }
                trial.remove(victim);
                victims.push(victim);
                if trial.fits(job, self.groups, self.gpus) {
                    break;
                }
            }
            if !trial.fits(job, self.groups, self.gpus) {
                // Stopping every candidate would not be enough.
                continue;
            }
            // Keep only the victims the job needs.
            for i in (0..victims.len()).rev() {
                trial.add(victims[i]);
                if trial.fits(job, self.groups, self.gpus) {
                    victims.remove(i);
                } else {
                    trial.remove(victims[i]);
                }
            }
            trial.add(job);
            planned = trial;
            chosen.extend(victims.iter().map(|v| v.id));
            out.extend(victims.iter().map(|v| v.id));
        }
        out
    }

    /// The jobs that may be preempted at all, lowest first: lowest
    /// [`order_key`], then lowest priority (the multifactor priority
    /// without the age factor, or the last in pueue's order), then the most
    /// recently started, then the highest id.
    fn pool(&self) -> Vec<&'a Job> {
        let preempt = self.preempt;
        let exempt = |job: &Job| {
            preempt.exempt_ms > 0
                && preempt
                    .running_since
                    .get(&job.id)
                    .is_some_and(|&since| self.now.saturating_sub(since) < preempt.exempt_ms)
        };
        let pool: Vec<&'a Job> = self
            .jobs
            .iter()
            .filter(|j| matches!(j.state, JobState::Running | JobState::Paused))
            .filter(|j| !j.spec.no_checkpoint)
            .filter(|j| !preempt.refused.contains(&j.id))
            .filter(|j| !exempt(j))
            .collect();
        let low: HashMap<JobId, f64> = match self.input {
            Some(input) if input.params.kind == PriorityType::Multifactor => {
                let ctx = priority::Context {
                    params: input.params,
                    now: self.now,
                    fairshare: input.fairshare,
                    groups: self.group_list,
                    resources: input.resources,
                };
                priority::prioritize(&pool, &ctx, |_| priority::Eligibility {
                    since: None,
                    launchable: false,
                })
                .into_iter()
                .map(|p| (p.id, p.priority))
                .collect()
            }
            _ => priority::basic_order(&pool)
                .iter()
                .enumerate()
                .map(|(i, job)| (job.id, -(i as f64)))
                .collect(),
        };
        let started = |job: &Job| {
            preempt
                .running_since
                .get(&job.id)
                .copied()
                .or(job.started_at)
        };
        let mut pool = pool;
        pool.sort_by(|a, b| {
            order_key(a, self.group_list)
                .cmp(&order_key(b, self.group_list))
                .then_with(|| low[&a.id].total_cmp(&low[&b.id]))
                .then_with(|| started(b).cmp(&started(a)))
                .then_with(|| b.id.cmp(&a.id))
        });
        pool
    }
}

/// Whether preempting `victim` frees a slot `job` needs: a slot of its
/// group or of one of its GPUs.
fn frees_slot_for(victim: &Job, job: &Job) -> bool {
    victim.spec.group == job.spec.group
        || victim.spec.gpus.iter().any(|g| job.spec.gpus.contains(g))
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
    use crate::model::{DEFAULT_GROUP, JobSpec, Uid, YieldMode};

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
                yield_mode: YieldMode::None,
            },
            state,
            created_at: 0,
            started_at: None,
            finished_at: None,
            site_factor: 0,
            top_seq: None,
            preempted_at: None,
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
            tier: 0,
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

    // ----- tiers, yield and preemption -----

    impl J {
        fn yields(mut self, mode: YieldMode) -> Self {
            self.0.spec.yield_mode = mode;
            self
        }
        fn no_checkpoint(mut self) -> Self {
            self.0.spec.no_checkpoint = true;
            self
        }
        fn started(mut self, at: Timestamp) -> Self {
            self.0.started_at = Some(at);
            self
        }
    }

    fn tier(name: &str, parallel: u32, tier: i32) -> Group {
        Group {
            tier,
            ..group(name, parallel)
        }
    }

    /// A job being checkpointed for a preemption.
    fn preempting(id: u64) -> J {
        let mut j = job(id, JobState::Checkpointing);
        j.0.preempted_at = Some(NOW - 1);
        j
    }

    /// The inputs of [`plan_preemptive`] besides the jobs, groups and GPUs.
    struct Pre {
        enabled: bool,
        exempt_ms: i64,
        refused: HashSet<JobId>,
        running_since: HashMap<JobId, Timestamp>,
        reserved: Reserved,
        params: Params,
        fairshare: HashMap<Uid, f64>,
    }

    impl Default for Pre {
        fn default() -> Self {
            Self {
                enabled: true,
                exempt_ms: 0,
                refused: HashSet::new(),
                running_since: HashMap::new(),
                reserved: Reserved::default(),
                params: Params::default(),
                fairshare: HashMap::new(),
            }
        }
    }

    impl Pre {
        fn run(&self, jobs: Vec<J>, groups: &[Group], gpus: &[Gpu]) -> Plan {
            let jobs: Vec<Job> = jobs.into_iter().map(|j| j.0).collect();
            let input = PriorityInput {
                params: &self.params,
                fairshare: &self.fairshare,
                resources: Resources {
                    gpus: gpus.len() as u32,
                    cpus: 8,
                },
            };
            let preempt = PreemptInput {
                enabled: self.enabled,
                exempt_ms: self.exempt_ms,
                refused: &self.refused,
                running_since: &self.running_since,
            };
            let out = plan_preemptive(NOW, &jobs, groups, gpus, &self.reserved, &input, &preempt);
            // Without preemption the rest of the `Plan` is the same.
            let without = plan_prioritized(NOW, &jobs, groups, gpus, &self.reserved, &input);
            assert!(without.preempt.is_empty());
            assert_eq!(
                Plan {
                    preempt: Vec::new(),
                    ..out.clone()
                },
                without
            );
            out
        }

        /// The preempted jobs.
        fn victims(&self, jobs: Vec<J>, groups: &[Group], gpus: &[Gpu]) -> Vec<JobId> {
            self.run(jobs, groups, gpus).preempt
        }
    }

    fn running(id: u64) -> J {
        job(id, JobState::Running)
    }

    #[test]
    fn order_key_by_tier_and_yield() {
        let groups = [tier("hi", 0, 2), tier("lo", 0, -1)];
        let key = |j: J| order_key(&j.0, &groups);
        assert_eq!(key(job(1, queued()).group("hi")), (1, 2, 1));
        assert_eq!(
            key(job(1, queued()).group("hi").yields(YieldMode::Group)),
            (1, 2, 0)
        );
        assert_eq!(
            key(job(1, queued()).group("lo").yields(YieldMode::All)),
            (0, -1, 0)
        );
        // An unknown group counts as tier 0.
        assert_eq!(key(job(1, queued()).group("gone")), (1, 0, 1));
    }

    #[test]
    fn tiers_and_yield_come_before_priority() {
        let groups = [group(DEFAULT_GROUP, 0), tier("hi", 0, 1), tier("lo", 0, -1)];
        let gpus = [gpu(0, 0)];
        let jobs = || {
            vec![
                job(1, queued()).gpus(&[0]).yields(YieldMode::All).prio(9),
                job(2, queued()).gpus(&[0]).yields(YieldMode::Group).prio(9),
                job(3, queued()).gpus(&[0]),
                job(4, restore_queued()).gpus(&[0]).group("lo").prio(9),
                job(5, queued()).gpus(&[0]).group("hi"),
                job(6, queued())
                    .gpus(&[0])
                    .yields(YieldMode::All)
                    .group("hi"),
                job(7, queued()).gpus(&[0]).prio(1),
            ]
        };
        // hi; default without yield (pueue's order within); default
        // --yield=group; lo; --yield by tier.
        let p = run(jobs(), &groups, &gpus);
        assert_eq!(p.start, ids(&[5, 7, 3, 2, 6, 1]));
        assert_eq!(p.restore, ids(&[4]));

        let params = Params::default();
        let fairshare = HashMap::new();
        let input = PriorityInput {
            params: &params,
            fairshare: &fairshare,
            resources: Resources::default(),
        };
        let all: Vec<Job> = jobs().into_iter().map(|j| j.0).collect();
        let listed = pending_priorities(NOW, &all, &groups, &gpus, &Reserved::default(), &input);
        assert_eq!(
            listed.iter().map(|p| p.id.0).collect::<Vec<_>>(),
            [5, 7, 3, 2, 4, 6, 1]
        );
        let shown: Vec<(u64, i32, YieldMode)> = listed
            .iter()
            .map(|p| (p.id.0, p.tier, p.yield_mode))
            .collect();
        assert_eq!(shown[0], (5, 1, YieldMode::None));
        assert_eq!(shown[3], (2, 0, YieldMode::Group));
        assert_eq!(shown[4], (4, -1, YieldMode::None));
        assert_eq!(shown[5], (6, 1, YieldMode::All));
        let basic = Params {
            kind: crate::priority::PriorityType::Basic,
            ..Params::default()
        };
        let input = PriorityInput {
            params: &basic,
            ..input
        };
        let listed = pending_priorities(NOW, &all, &groups, &gpus, &Reserved::default(), &input);
        assert_eq!(
            listed.iter().map(|p| p.id.0).collect::<Vec<_>>(),
            [5, 7, 3, 2, 4, 6, 1]
        );
        assert_eq!(listed[5].tier, 1);

        // A single slot goes to the first in that order.
        let p = run(jobs(), &groups, &[gpu(0, 1)]);
        assert_eq!(p.start, ids(&[5]));
    }

    #[test]
    fn may_preempt_table() {
        // a and b: tier 0; hi: tier 1; lo: tier -1.
        let groups = [
            tier("a", 0, 0),
            tier("b", 0, 0),
            tier("hi", 0, 1),
            tier("lo", 0, -1),
        ];
        let j = |group: &str, mode: YieldMode| job(1, queued()).group(group).yields(mode).0;
        let v = |group: &str, mode: YieldMode| running(2).group(group).yields(mode).0;
        use YieldMode::{All, Group as Grp, None as Normal};
        #[rustfmt::skip]
        let table = [
            // Normal job, same group.
            (j("a", Normal), v("a", Normal), false),
            (j("a", Normal), v("a", Grp), true),
            (j("a", Normal), v("a", All), true),
            // Normal job, other group of the same tier.
            (j("a", Normal), v("b", Normal), false),
            (j("a", Normal), v("b", Grp), false),
            (j("a", Normal), v("b", All), true),
            // Normal job, higher tier.
            (j("hi", Normal), v("a", Normal), true),
            (j("hi", Normal), v("a", Grp), true),
            (j("hi", Normal), v("a", All), true),
            // Normal job, lower tier.
            (j("lo", Normal), v("a", Normal), false),
            (j("lo", Normal), v("a", Grp), false),
            (j("lo", Normal), v("a", All), true),
            // --yield=group: only by tier, or over --yield.
            (j("a", Grp), v("a", Normal), false),
            (j("a", Grp), v("a", Grp), false),
            (j("a", Grp), v("b", Normal), false),
            (j("a", Grp), v("b", Grp), false),
            (j("hi", Grp), v("a", Normal), true),
            (j("hi", Grp), v("a", Grp), true),
            (j("a", Grp), v("a", All), true),
            (j("lo", Grp), v("hi", All), true),
            // --yield: only other --yield jobs of a lower tier.
            (j("a", All), v("a", Normal), false),
            (j("a", All), v("a", Grp), false),
            (j("hi", All), v("lo", Normal), false),
            (j("hi", All), v("lo", Grp), false),
            (j("a", All), v("a", All), false),
            (j("a", All), v("b", All), false),
            (j("hi", All), v("a", All), true),
            (j("lo", All), v("a", All), false),
        ];
        for (waiting, victim, expected) in &table {
            let what = format!(
                "{} {:?} -> {} {:?}",
                waiting.spec.group,
                waiting.spec.yield_mode,
                victim.spec.group,
                victim.spec.yield_mode
            );
            assert_eq!(may_preempt(waiting, victim, &groups), *expected, "{what}");
            if *expected {
                // A job that may preempt another is ordered before it, and
                // the relation is one-way.
                assert!(
                    order_key(waiting, &groups) > order_key(victim, &groups),
                    "{what}"
                );
                assert!(!may_preempt(victim, waiting, &groups), "{what}");
            }
        }
    }

    #[test]
    fn preempts_a_yielding_job_for_one_that_fits() {
        let groups = [group(DEFAULT_GROUP, 0)];
        let gpus = [gpu(0, 1)];
        let jobs = || {
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All),
                job(2, queued()).gpus(&[0]),
            ]
        };
        let p = Pre::default().run(jobs(), &groups, &gpus);
        assert_eq!(p.preempt, ids(&[1]));
        assert!(p.start.is_empty());
        assert!(!p.is_empty());
        // Disabled: nothing.
        let off = Pre {
            enabled: false,
            ..Pre::default()
        };
        assert!(off.run(jobs(), &groups, &gpus).is_empty());
        // Paused jobs are checkpointed too.
        let p = Pre::default().victims(
            vec![
                job(1, JobState::Paused).gpus(&[0]).yields(YieldMode::All),
                job(2, queued()).gpus(&[0]),
            ],
            &groups,
            &gpus,
        );
        assert_eq!(p, ids(&[1]));
        // Other states are not victims.
        for state in [
            JobState::Checkpointing,
            JobState::Restoring,
            JobState::Terminating {
                result: JobResult::Killed,
            },
        ] {
            let p = Pre::default().victims(
                vec![
                    job(1, state.clone()).gpus(&[0]).yields(YieldMode::All),
                    job(2, queued()).gpus(&[0]),
                ],
                &groups,
                &gpus,
            );
            assert!(p.is_empty(), "{state:?}");
        }
    }

    #[test]
    fn same_rank_never_preempts() {
        let pre = Pre {
            // Job 2's owner has a far better priority.
            fairshare: HashMap::from([(Uid(1), 0.0), (Uid(2), 1.0)]),
            ..Pre::default()
        };
        let p = pre.victims(
            vec![
                running(1).owner(1).gpus(&[0]),
                job(2, queued()).owner(2).gpus(&[0]),
                // Same tier, other group, --yield=group: not its group.
                running(3).group("b").gpus(&[1]).yields(YieldMode::Group),
                job(4, queued()).owner(2).gpus(&[1]),
            ],
            &[group(DEFAULT_GROUP, 0), group("b", 0)],
            &[gpu(0, 1), gpu(1, 1)],
        );
        assert!(p.is_empty());
    }

    #[test]
    fn group_yield_gives_way_to_its_group_and_higher_tiers_only() {
        let groups = [tier("a", 0, 0), tier("b", 0, 0), tier("c", 0, 1)];
        let gpus = [gpu(0, 1)];
        let with = |waiting_group: &str| {
            Pre::default().victims(
                vec![
                    running(1).group("a").gpus(&[0]).yields(YieldMode::Group),
                    job(2, queued()).group(waiting_group).gpus(&[0]),
                ],
                &groups,
                &gpus,
            )
        };
        assert!(with("b").is_empty());
        assert_eq!(with("a"), ids(&[1]));
        assert_eq!(with("c"), ids(&[1]));
        // A lower-tier group's jobs give way to a higher tier whatever their
        // mode.
        let p = Pre::default().victims(
            vec![
                running(1).group("a").gpus(&[0]),
                job(2, queued()).group("c").gpus(&[0]),
            ],
            &groups,
            &gpus,
        );
        assert_eq!(p, ids(&[1]));
    }

    #[test]
    fn yield_jobs_compare_by_tier() {
        let groups = [tier("hi", 0, 1), tier("lo", 0, 0)];
        let gpus = [gpu(0, 1)];
        let with = |victim: &str, waiting: &str| {
            Pre::default().victims(
                vec![
                    running(1).group(victim).gpus(&[0]).yields(YieldMode::All),
                    job(2, queued())
                        .group(waiting)
                        .gpus(&[0])
                        .yields(YieldMode::All),
                ],
                &groups,
                &gpus,
            )
        };
        assert_eq!(with("lo", "hi"), ids(&[1]));
        assert!(with("hi", "lo").is_empty());
        assert!(with("lo", "lo").is_empty());
    }

    #[test]
    fn chooses_a_minimal_set_of_victims() {
        // Group g has two slots, both taken; job 1 started last, so it is
        // tried first, but only job 2 holds the GPU job 3 needs.
        let pre = Pre {
            running_since: HashMap::from([(JobId(1), NOW - 10), (JobId(2), NOW - 20)]),
            ..Pre::default()
        };
        let p = pre.victims(
            vec![
                running(1).group("g").yields(YieldMode::All),
                running(2).group("g").gpus(&[0]).yields(YieldMode::All),
                job(3, queued()).group("g").gpus(&[0]),
            ],
            &[group("g", 2)],
            &[gpu(0, 1)],
        );
        assert_eq!(p, ids(&[2]));
    }

    #[test]
    fn victims_lowest_first() {
        let groups = [group(DEFAULT_GROUP, 0)];
        let gpus = [gpu(0, 2)];
        // --yield before --yield=group.
        let p = Pre::default().victims(
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All),
                running(2).gpus(&[0]).yields(YieldMode::Group),
                job(3, queued()).gpus(&[0]),
            ],
            &groups,
            &gpus,
        );
        assert_eq!(p, ids(&[1]));
        // Same key: the lower priority first, even if it started earlier.
        let pre = Pre {
            fairshare: HashMap::from([(Uid(1), 0.1), (Uid(2), 0.9)]),
            running_since: HashMap::from([(JobId(1), NOW - 100), (JobId(2), NOW - 10)]),
            ..Pre::default()
        };
        let jobs = || {
            vec![
                running(1).owner(1).gpus(&[0]).yields(YieldMode::All),
                running(2).owner(2).gpus(&[0]).yields(YieldMode::All),
                job(3, queued()).owner(3).gpus(&[0]),
            ]
        };
        assert_eq!(pre.victims(jobs(), &groups, &gpus), ids(&[1]));
        // Equal priorities: the most recently started first (from
        // `running_since`, else `started_at`).
        let pre = Pre {
            running_since: HashMap::from([(JobId(1), NOW - 100), (JobId(2), NOW - 10)]),
            ..Pre::default()
        };
        let p = pre.victims(
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All),
                running(2).gpus(&[0]).yields(YieldMode::All),
                job(3, queued()).owner(3).gpus(&[0]),
            ],
            &groups,
            &gpus,
        );
        assert_eq!(p, ids(&[2]));
        let p = Pre::default().victims(
            vec![
                running(1)
                    .gpus(&[0])
                    .yields(YieldMode::All)
                    .started(NOW - 10),
                running(2)
                    .gpus(&[0])
                    .yields(YieldMode::All)
                    .started(NOW - 100),
                job(3, queued()).owner(3).gpus(&[0]),
            ],
            &groups,
            &gpus,
        );
        assert_eq!(p, ids(&[1]));
        // Basic priority: the last in pueue's order first.
        let basic = Pre {
            params: Params {
                kind: crate::priority::PriorityType::Basic,
                ..Params::default()
            },
            ..Pre::default()
        };
        let p = basic.victims(
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All).prio(5),
                running(2).gpus(&[0]).yields(YieldMode::All),
                job(3, queued()).owner(3).gpus(&[0]),
            ],
            &groups,
            &gpus,
        );
        assert_eq!(p, ids(&[2]));
    }

    #[test]
    fn nothing_is_preempted_when_the_job_would_still_not_fit() {
        let p = Pre::default().run(
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All),
                // Same rank as job 3: cannot be preempted.
                running(2).gpus(&[1]),
                job(3, queued()).gpus(&[0, 1]),
            ],
            &[group(DEFAULT_GROUP, 0)],
            &[gpu(0, 1), gpu(1, 1)],
        );
        assert!(p.is_empty());
        // Orphans take slots too.
        let pre = Pre {
            reserved: Reserved {
                gpus: HashMap::from([(GpuIndex(0), 1)]),
                held: HashSet::new(),
            },
            ..Pre::default()
        };
        let jobs = || {
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All),
                job(2, queued()).gpus(&[0]),
            ]
        };
        assert!(
            pre.victims(jobs(), &[group(DEFAULT_GROUP, 0)], &[gpu(0, 1)])
                .is_empty()
        );
        assert_eq!(
            pre.victims(jobs(), &[group(DEFAULT_GROUP, 0)], &[gpu(0, 2)]),
            ids(&[1])
        );
    }

    #[test]
    fn multi_gpu_job_preempts_on_every_gpu_it_needs() {
        let p = Pre::default().victims(
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All),
                running(2).gpus(&[1]).yields(YieldMode::All),
                running(3).gpus(&[2]).yields(YieldMode::All),
                job(4, queued()).gpus(&[0, 1]),
            ],
            &[group(DEFAULT_GROUP, 0)],
            &[gpu(0, 1), gpu(1, 1), gpu(2, 1)],
        );
        let mut p = p;
        p.sort();
        assert_eq!(p, ids(&[1, 2]));
        // One victim on both GPUs is enough.
        let pre = Pre {
            running_since: HashMap::from([(JobId(1), NOW - 10), (JobId(2), NOW - 20)]),
            ..Pre::default()
        };
        let p = pre.victims(
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All),
                running(2).gpus(&[0, 1]).yields(YieldMode::All),
                job(4, queued()).gpus(&[0, 1]),
            ],
            &[group(DEFAULT_GROUP, 0)],
            &[gpu(0, 2), gpu(1, 1)],
        );
        assert_eq!(p, ids(&[2]));
    }

    #[test]
    fn group_slots_are_freed_by_jobs_of_the_group() {
        let groups = [group("g", 1), group("h", 0)];
        let p = Pre::default().victims(
            vec![
                running(1).group("g").yields(YieldMode::All),
                job(2, queued()).group("g"),
            ],
            &groups,
            &[],
        );
        assert_eq!(p, ids(&[1]));
        // A job of another group sharing no GPU frees nothing for it.
        let p = Pre::default().victims(
            vec![
                running(1).group("g"),
                running(3).group("h").yields(YieldMode::All),
                job(2, queued()).group("g"),
            ],
            &groups,
            &[],
        );
        assert!(p.is_empty());
        // Unlimited group: only the GPU matters.
        let p = Pre::default().victims(
            vec![
                running(1).group("h").yields(YieldMode::All),
                running(3).group("g").gpus(&[0]).yields(YieldMode::All),
                job(2, queued()).group("h").gpus(&[0]),
            ],
            &groups,
            &[gpu(0, 1)],
        );
        assert_eq!(p, ids(&[3]));
    }

    #[test]
    fn excluded_victims() {
        let groups = [group(DEFAULT_GROUP, 0)];
        let gpus = [gpu(0, 1)];
        let jobs = || {
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All),
                job(2, queued()).gpus(&[0]),
            ]
        };
        // no_checkpoint.
        let p = Pre::default().victims(
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All).no_checkpoint(),
                job(2, queued()).gpus(&[0]),
            ],
            &groups,
            &gpus,
        );
        assert!(p.is_empty());
        // Refused recently.
        let pre = Pre {
            refused: HashSet::from([JobId(1)]),
            ..Pre::default()
        };
        assert!(pre.victims(jobs(), &groups, &gpus).is_empty());
        // Exempt for a while after it started running.
        let exempt = |since: Option<Timestamp>, exempt_ms: i64| Pre {
            exempt_ms,
            running_since: since.map(|t| (JobId(1), t)).into_iter().collect(),
            ..Pre::default()
        };
        assert!(
            exempt(Some(NOW - 999), 1000)
                .victims(jobs(), &groups, &gpus)
                .is_empty()
        );
        assert_eq!(
            exempt(Some(NOW - 1000), 1000).victims(jobs(), &groups, &gpus),
            ids(&[1])
        );
        assert_eq!(
            exempt(None, 1000).victims(jobs(), &groups, &gpus),
            ids(&[1])
        );
        assert_eq!(
            exempt(Some(NOW), 0).victims(jobs(), &groups, &gpus),
            ids(&[1])
        );
    }

    #[test]
    fn only_jobs_that_could_launch_preempt() {
        let groups = [group(DEFAULT_GROUP, 0), paused("held", 0)];
        let gpus = [gpu(0, 1)];
        let victim = || running(1).gpus(&[0]).yields(YieldMode::All);
        for (what, waiting) in [
            ("paused group", job(2, queued()).group("held").gpus(&[0])),
            ("missing group", job(2, queued()).group("gone").gpus(&[0])),
            ("undetected GPU", job(2, queued()).gpus(&[0, 7])),
            (
                "unfinished dependency",
                job(2, queued()).gpus(&[0]).deps(&[9]),
            ),
            (
                "stashed",
                job(2, JobState::Stashed { enqueue_at: None }).gpus(&[0]),
            ),
        ] {
            let p = Pre::default().run(
                vec![victim(), waiting, running(9).group("held")],
                &groups,
                &gpus,
            );
            assert!(p.preempt.is_empty(), "{what}");
        }
        // Held (an orphan with its id runs).
        let pre = Pre {
            reserved: Reserved {
                gpus: HashMap::new(),
                held: HashSet::from([JobId(2)]),
            },
            ..Pre::default()
        };
        assert!(
            pre.victims(vec![victim(), job(2, queued()).gpus(&[0])], &groups, &gpus)
                .is_empty()
        );
        // Restore-queued jobs skip the dependency check here too.
        let p = Pre::default().victims(
            vec![victim(), job(2, restore_queued()).gpus(&[0]).deps(&[9])],
            &groups,
            &gpus,
        );
        assert_eq!(p, ids(&[1]));
    }

    #[test]
    fn freed_slots_are_kept_for_the_job_that_needs_them() {
        let groups = [group(DEFAULT_GROUP, 0)];
        // One victim, two jobs that need it: chosen once, for job 2.
        let p = Pre::default().victims(
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All),
                job(2, queued()).gpus(&[0]),
                job(3, queued()).gpus(&[0]),
            ],
            &groups,
            &[gpu(0, 1)],
        );
        assert_eq!(p, ids(&[1]));
        // Two victims: one each.
        let p = Pre::default().victims(
            vec![
                running(1).gpus(&[0]).yields(YieldMode::All),
                running(4).gpus(&[0]).yields(YieldMode::All),
                job(2, queued()).gpus(&[0]),
                job(3, queued()).gpus(&[0]),
            ],
            &groups,
            &[gpu(0, 2)],
        );
        assert_eq!(p, ids(&[4, 1]));
    }

    #[test]
    fn preemptions_in_progress_are_not_repeated() {
        let groups = [group(DEFAULT_GROUP, 0)];
        // Job 1 is being checkpointed for job 3: no second victim for it,
        // and job 3 waits for the checkpoint to finish.
        let p = Pre::default().run(
            vec![
                preempting(1).gpus(&[0]).yields(YieldMode::All),
                running(2).gpus(&[0]).yields(YieldMode::All),
                job(3, queued()).gpus(&[0]),
            ],
            &groups,
            &[gpu(0, 2)],
        );
        assert!(p.is_empty());
        // A second waiting job needs a victim of its own.
        let p = Pre::default().run(
            vec![
                preempting(1).gpus(&[0]).yields(YieldMode::All),
                running(2).gpus(&[0]).yields(YieldMode::All),
                job(3, queued()).gpus(&[0]),
                job(4, queued()).gpus(&[0]),
            ],
            &groups,
            &[gpu(0, 2)],
        );
        assert_eq!(p.preempt, ids(&[2]));
        assert!(p.start.is_empty());
        // A manual checkpoint is not a preemption: it frees nothing.
        let p = Pre::default().victims(
            vec![
                job(1, JobState::Checkpointing)
                    .gpus(&[0])
                    .yields(YieldMode::All),
                running(2).gpus(&[0]).yields(YieldMode::All),
                job(3, queued()).gpus(&[0]),
            ],
            &groups,
            &[gpu(0, 2)],
        );
        assert_eq!(p, ids(&[2]));
    }

    #[test]
    fn the_preempting_job_starts_before_the_victim_is_restored() {
        let groups = [group(DEFAULT_GROUP, 0)];
        let gpus = [gpu(0, 1)];
        // The checkpoint has finished: the victim waits to be restored,
        // queued before job 2, but job 2 gets the GPU.
        let p = Pre::default().run(
            vec![
                job(1, restore_queued())
                    .gpus(&[0])
                    .yields(YieldMode::All)
                    .at(NOW - 5000),
                job(2, queued()).gpus(&[0]).at(NOW),
            ],
            &groups,
            &gpus,
        );
        assert_eq!(p.start, ids(&[2]));
        assert!(p.restore.is_empty());
        assert!(p.preempt.is_empty());
        // Once job 2 runs, job 1 is not restored (and cannot preempt it).
        let p = Pre::default().run(
            vec![
                job(1, restore_queued()).gpus(&[0]).yields(YieldMode::All),
                running(2).gpus(&[0]),
            ],
            &groups,
            &gpus,
        );
        assert!(p.is_empty());
        // When job 2 is done, job 1 is restored.
        let p = Pre::default().run(
            vec![
                job(1, restore_queued()).gpus(&[0]).yields(YieldMode::All),
                job(2, success()).gpus(&[0]),
            ],
            &groups,
            &gpus,
        );
        assert_eq!(p.restore, ids(&[1]));
    }
}
