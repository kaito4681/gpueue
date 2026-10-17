use std::collections::BTreeMap;
use std::path::PathBuf;

use super::*;
use crate::model::{DEFAULT_GROUP, GpuIndex, JobResult, JobSpec};

const HOUR: i64 = 60 * 60 * 1000;
const DAY: i64 = 24 * HOUR;
const NOW: Timestamp = 100 * DAY;

fn job(id: u64, owner: u32) -> Job {
    Job {
        id: JobId(id),
        owner: Uid(owner),
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
        state: JobState::Queued { enqueued_at: NOW },
        created_at: 0,
        started_at: None,
        finished_at: None,
        site_factor: 0,
        top_seq: None,
    }
}

fn queued_at(mut job: Job, at: Timestamp) -> Job {
    job.state = JobState::Queued { enqueued_at: at };
    job
}

fn with_gpus(mut job: Job, gpus: &[u32]) -> Job {
    job.spec.gpus = gpus.iter().map(|&g| GpuIndex(g)).collect();
    job
}

fn group(name: &str, priority: u32) -> Group {
    Group {
        name: name.into(),
        parallel: 1,
        paused: false,
        priority,
    }
}

/// Only the factor named weighs 1 (or `weight`).
fn only(factor: &str, weight: u32) -> Params {
    let mut p = Params {
        weight_age: 0,
        weight_fairshare: 0,
        ..Params::default()
    };
    match factor {
        "age" => p.weight_age = weight,
        "fairshare" => p.weight_fairshare = weight,
        "job_size" => p.weight_job_size = weight,
        "partition" => p.weight_partition = weight,
        "none" => {}
        other => panic!("{other}"),
    }
    p
}

struct Env {
    params: Params,
    fairshare: HashMap<Uid, f64>,
    groups: Vec<Group>,
    resources: Resources,
}

impl Env {
    fn new(params: Params) -> Self {
        Self {
            params,
            fairshare: HashMap::new(),
            groups: vec![group(DEFAULT_GROUP, 0)],
            resources: Resources { gpus: 4, cpus: 8 },
        }
    }

    /// Queued jobs are eligible since they were queued, and launchable.
    fn run(&self, jobs: &[Job]) -> Vec<JobPriority> {
        self.run_with(jobs, |job| match job.state {
            JobState::Queued { enqueued_at } => Eligibility {
                since: Some(enqueued_at),
                launchable: true,
            },
            _ => Eligibility {
                since: None,
                launchable: false,
            },
        })
    }

    fn run_with(
        &self,
        jobs: &[Job],
        eligibility: impl Fn(&Job) -> Eligibility,
    ) -> Vec<JobPriority> {
        let refs: Vec<&Job> = jobs.iter().collect();
        let ctx = Context {
            params: &self.params,
            now: NOW,
            fairshare: &self.fairshare,
            groups: &self.groups,
            resources: self.resources,
        };
        prioritize(&refs, &ctx, eligibility)
    }

    fn order(&self, jobs: &[Job]) -> Vec<u64> {
        self.run(jobs).iter().map(|p| p.id.0).collect()
    }

    fn of(&self, jobs: &[Job], id: u64) -> JobPriority {
        self.run(jobs).into_iter().find(|p| p.id.0 == id).unwrap()
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[test]
fn tres_weights_parse() {
    assert_eq!(
        TresWeights::parse("CPU=1.0,GRES/gpu=10.0").unwrap(),
        TresWeights {
            cpu: 1.0,
            gpu: 10.0
        }
    );
    assert_eq!(
        TresWeights::parse(" gres/GPU = 1000 ").unwrap(),
        TresWeights {
            cpu: 0.0,
            gpu: 1000.0
        }
    );
    assert_eq!(TresWeights::parse("").unwrap(), TresWeights::default());
    for bad in [
        "mem=1",
        "cpu",
        "cpu=x",
        "cpu=-1",
        "cpu=1,CPU=2",
        "cpu=inf",
        "gres/gpu=NaN",
    ] {
        assert!(TresWeights::parse(bad).is_err(), "{bad}");
    }
    let w = TresWeights::parse("CPU=1.5,GRES/gpu=10").unwrap();
    assert_eq!(TresWeights::parse(&w.to_text()).unwrap(), w);
}

#[test]
fn reset_period_parses_case_insensitively() {
    assert_eq!(
        "WEEKLY".parse::<ResetPeriod>().unwrap(),
        ResetPeriod::Weekly
    );
    assert_eq!("none".parse::<ResetPeriod>().unwrap(), ResetPeriod::None);
    assert!("hourly".parse::<ResetPeriod>().is_err());
    let json = serde_json::to_string(&ResetPeriod::Quarterly).unwrap();
    assert_eq!(json, r#""quarterly""#);
    assert_eq!(
        serde_json::from_str::<ResetPeriod>(r#""MONTHLY""#).unwrap(),
        ResetPeriod::Monthly
    );
}

// ---------------------------------------------------------------------------
// Factors
// ---------------------------------------------------------------------------

#[test]
fn age_factor_grows_until_max_age() {
    let env = Env::new(only("age", 1000));
    let jobs = [
        queued_at(job(1, 1), NOW),
        queued_at(job(2, 2), NOW - 7 * DAY / 2),
        queued_at(job(3, 3), NOW - 30 * DAY),
    ];
    assert!(close(env.of(&jobs, 1).factors.age, 0.0));
    assert!(close(env.of(&jobs, 2).factors.age, 0.5));
    assert!(close(env.of(&jobs, 3).factors.age, 1.0));
    assert!(close(env.of(&jobs, 2).priority, 500.0));
    assert_eq!(env.order(&jobs), [3, 2, 1]);
}

#[test]
fn ineligible_jobs_do_not_age_and_zero_max_age_means_full_age() {
    let env = Env::new(only("age", 1000));
    let mut stashed = job(1, 1);
    stashed.state = JobState::Stashed { enqueue_at: None };
    assert!(close(env.of(&[stashed], 1).factors.age, 0.0));

    let mut params = only("age", 1000);
    params.max_age_ms = 0;
    let env = Env::new(params);
    assert!(close(env.of(&[job(1, 1)], 1).factors.age, 1.0));
}

#[test]
fn fairshare_factor_comes_from_the_table() {
    let mut env = Env::new(only("fairshare", 10000));
    env.fairshare.insert(Uid(1), 0.25);
    let jobs = [job(1, 1), job(2, 2)];
    assert!(close(env.of(&jobs, 1).factors.fairshare, 0.25));
    assert!(close(env.of(&jobs, 1).priority, 2500.0));
    // Users missing from the table have not used anything.
    assert!(close(env.of(&jobs, 2).factors.fairshare, 1.0));
    assert_eq!(env.order(&jobs), [2, 1]);
}

#[test]
fn job_size_factor_and_favor_small() {
    let env = Env::new(only("job_size", 100));
    let jobs = [
        with_gpus(job(1, 1), &[0]),
        with_gpus(job(2, 1), &[0, 1, 2, 3]),
        job(3, 1),
        // A repeated GPU counts once.
        with_gpus(job(4, 1), &[1, 1]),
    ];
    assert!(close(env.of(&jobs, 1).factors.job_size, 0.25));
    assert!(close(env.of(&jobs, 2).factors.job_size, 1.0));
    // CPU-only: one CPU of eight.
    assert!(close(env.of(&jobs, 3).factors.job_size, 0.125));
    assert!(close(env.of(&jobs, 4).factors.job_size, 0.25));

    let mut params = only("job_size", 100);
    params.favor_small = true;
    let env = Env::new(params);
    assert!(close(env.of(&jobs, 1).factors.job_size, 1.0));
    assert!(close(env.of(&jobs, 2).factors.job_size, 0.25));
    assert!(close(env.of(&jobs, 3).factors.job_size, 1.0));
    assert_eq!(env.order(&jobs[..2]), [1, 2]);

    // No GPUs on the machine: GPU requests fall back to the CPU count.
    let mut env = Env::new(only("job_size", 100));
    env.resources = Resources { gpus: 0, cpus: 0 };
    assert!(close(env.of(&jobs, 1).factors.job_size, 0.0));
}

#[test]
fn partition_factor_is_relative_to_the_highest_group() {
    let mut env = Env::new(only("partition", 1000));
    env.groups = vec![group(DEFAULT_GROUP, 0), group("high", 10), group("mid", 5)];
    let mut a = job(1, 1);
    a.spec.group = "mid".into();
    let mut b = job(2, 1);
    b.spec.group = "high".into();
    let c = job(3, 1);
    let jobs = [a, b, c];
    assert!(close(env.of(&jobs, 1).factors.partition, 0.5));
    assert!(close(env.of(&jobs, 2).factors.partition, 1.0));
    assert!(close(env.of(&jobs, 3).factors.partition, 0.0));
    assert_eq!(env.order(&jobs), [2, 1, 3]);

    // Every group at 0: no partition factor.
    env.groups = vec![group(DEFAULT_GROUP, 0)];
    assert!(close(env.of(&jobs, 3).factors.partition, 0.0));
}

#[test]
fn tres_factor_weights_each_resource() {
    let mut params = only("none", 0);
    params.weight_tres = TresWeights::parse("CPU=800,GRES/gpu=1000").unwrap();
    let env = Env::new(params);
    let p = env.of(&[with_gpus(job(1, 1), &[0, 1])], 1);
    assert!(close(p.factors.tres_gpu, 0.5));
    assert!(close(p.factors.tres_cpu, 0.125));
    assert!(close(p.weighted.tres, 500.0 + 100.0));
    assert!(close(p.priority, 600.0));
}

#[test]
fn formula_sums_weighted_factors_site_and_nice() {
    let mut env = Env::new(Params {
        weight_age: 1000,
        weight_fairshare: 10000,
        weight_job_size: 100,
        weight_partition: 10,
        weight_tres: TresWeights::parse("GRES/gpu=1000").unwrap(),
        ..Params::default()
    });
    env.fairshare.insert(Uid(1), 0.5);
    env.groups = vec![group(DEFAULT_GROUP, 2), group("other", 4)];
    let mut j = with_gpus(queued_at(job(1, 1), NOW - 7 * DAY / 4), &[0]);
    j.site_factor = 7;
    j.spec.nice = 3;
    let p = env.of(&[j], 1);
    let expected =
        7.0 + 1000.0 * 0.25 + 10000.0 * 0.5 + 100.0 * 0.25 + 10.0 * 0.5 + 1000.0 * 0.25 - 3.0;
    assert!(close(p.priority, expected), "{} != {expected}", p.priority);
    assert_eq!(p.site, 7);
    assert_eq!(p.nice, 3);
    assert!(close(p.weighted.fairshare, 5000.0));
}

// ---------------------------------------------------------------------------
// Ordering
// ---------------------------------------------------------------------------

#[test]
fn lower_usage_user_goes_first() {
    let mut env = Env::new(Params::default());
    let table = fair_share(
        &BTreeMap::new(),
        &BTreeMap::from([(Uid(1), 3600.0), (Uid(2), 60.0)]),
        &BTreeSet::from([Uid(1), Uid(2)]),
    );
    env.fairshare = fair_share_factors(&table);
    // User 1 submitted first and waited longer, but used much more.
    let jobs = [queued_at(job(1, 1), NOW - HOUR), queued_at(job(2, 2), NOW)];
    assert_eq!(env.order(&jobs), [2, 1]);
}

#[test]
fn nice_lowers_and_negative_nice_raises() {
    let env = Env::new(Params::default());
    let mut a = job(1, 1);
    a.spec.nice = 100;
    let b = job(2, 2);
    assert_eq!(env.order(&[a.clone(), b.clone()]), [2, 1]);
    a.spec.nice = -100;
    assert_eq!(env.order(&[a, b]), [1, 2]);
}

#[test]
fn site_factor_raises() {
    let env = Env::new(Params::default());
    let a = job(1, 1);
    let mut b = job(2, 2);
    b.site_factor = 1;
    assert_eq!(env.order(&[a, b]), [2, 1]);
}

#[test]
fn o_priority_only_orders_the_owners_jobs() {
    let env = Env::new(Params::default());
    // User 1's newer job has -o 10: it takes the place of user 1's older
    // job, which waited longer than user 2's job; the older job takes the
    // newer one's place, after user 2's job.
    let old = queued_at(job(1, 1), NOW - DAY);
    let mut new = queued_at(job(2, 1), NOW);
    new.spec.priority = 10;
    let other = queued_at(job(3, 2), NOW - HOUR);
    let order = env.order(&[old, new, other]);
    assert_eq!(order, [2, 3, 1]);
    // User 1 keeps their total: the ages were swapped, not raised.
    let jobs = [queued_at(job(1, 1), NOW - DAY), {
        let mut j = queued_at(job(2, 1), NOW);
        j.spec.priority = 10;
        j
    }];
    let p = env.run(&jobs);
    assert_eq!(p[0].id, JobId(2));
    assert!(close(p[0].factors.age, 1.0 / 7.0));
    assert!(close(p[1].factors.age, 0.0));
}

#[test]
fn o_priority_does_not_beat_other_users() {
    // In pueue a higher -o wins globally; here another user's job with a
    // higher priority still goes first.
    let env = Env::new(Params::default());
    let mut mine = job(2, 1);
    mine.spec.priority = 100;
    let theirs = queued_at(job(1, 2), NOW - HOUR);
    assert_eq!(env.order(&[mine, theirs]), [1, 2]);
}

#[test]
fn top_moves_jobs_to_the_front_of_the_owners_queue() {
    let env = Env::new(Params::default());
    let a = queued_at(job(1, 1), NOW - 3 * HOUR);
    let b = queued_at(job(2, 1), NOW - 2 * HOUR);
    let mut c = queued_at(job(3, 1), NOW - HOUR);
    // `top` beats -o priority.
    let mut a_prio = a.clone();
    a_prio.spec.priority = 50;
    c.top_seq = Some(0);
    assert_eq!(env.order(&[a_prio, b.clone(), c.clone()]), [3, 1, 2]);
    // The latest top comes first.
    let mut b_top = b.clone();
    b_top.top_seq = Some(1);
    assert_eq!(env.order(&[a.clone(), b_top, c.clone()]), [2, 3, 1]);
    // Another user's older job is not overtaken.
    let other = queued_at(job(4, 2), NOW - 3 * HOUR + 1);
    assert_eq!(env.order(&[a, b, c, other]), [3, 4, 1, 2]);
}

#[test]
fn ineligible_jobs_take_no_age_from_runnable_ones() {
    let env = Env::new(only("age", 1000));
    // Job 1 waits for a dependency (not eligible) but comes first in its
    // owner's order (-o 10); job 2 can start. Job 2 keeps its own age.
    let mut blocked = queued_at(job(1, 1), NOW - 7 * DAY / 2);
    blocked.spec.priority = 10;
    let runnable = queued_at(job(2, 1), NOW - 7 * DAY / 4);
    let p = env.run_with(&[blocked, runnable], |job| match (job.id.0, &job.state) {
        (1, _) => Eligibility {
            since: None,
            launchable: false,
        },
        (_, JobState::Queued { enqueued_at }) => Eligibility {
            since: Some(*enqueued_at),
            launchable: true,
        },
        _ => unreachable!(),
    });
    let of = |id| p.iter().find(|p| p.id.0 == id).unwrap();
    assert!(close(of(1).factors.age, 0.0));
    assert!(close(of(2).factors.age, 0.25));
    assert_eq!(p[0].id, JobId(2));
}

#[test]
fn unlaunchable_jobs_keep_their_own_age_and_lend_none() {
    let env = Env::new(only("age", 1000));
    // User 1: job 1 is 3.5 days old but cannot launch (say its group is
    // paused); job 2 is new and first in the owner's order (-o 10). User
    // 2: job 3 is 3 days old. Job 1 must not lend its age to job 2.
    let parked = queued_at(job(1, 1), NOW - 7 * DAY / 2);
    let mut new = queued_at(job(2, 1), NOW);
    new.spec.priority = 10;
    let other = queued_at(job(3, 2), NOW - 3 * DAY);
    let p = env.run_with(&[parked, new, other], |job| {
        let JobState::Queued { enqueued_at } = job.state else {
            unreachable!()
        };
        Eligibility {
            since: Some(enqueued_at),
            launchable: job.id.0 != 1,
        }
    });
    let of = |id| p.iter().find(|p| p.id.0 == id).unwrap();
    // Job 1 keeps its own age, for when it can launch again.
    assert!(close(of(1).factors.age, 0.5));
    assert!(close(of(2).factors.age, 0.0));
    assert!(close(of(3).factors.age, 3.0 / 7.0));
    let order: Vec<u64> = p.iter().map(|p| p.id.0).collect();
    assert!(
        order.iter().position(|&id| id == 3) < order.iter().position(|&id| id == 2),
        "{order:?}"
    );
}

#[test]
fn equal_priorities_follow_the_owners_order_then_id() {
    // Everything aged out: totals tie.
    let env = Env::new(Params::default());
    let mut jobs: Vec<Job> = (1..=4)
        .map(|id| queued_at(job(id, 1), NOW - 30 * DAY))
        .collect();
    jobs[3].spec.priority = 1;
    assert_eq!(env.order(&jobs), [4, 1, 2, 3]);
    // Two users, equal totals: lower id first within equal rank.
    let jobs = [
        queued_at(job(2, 1), NOW - 30 * DAY),
        queued_at(job(1, 2), NOW - 30 * DAY),
    ];
    assert_eq!(env.order(&jobs), [1, 2]);
}

#[test]
fn basic_mode_is_pueues_order() {
    let mut params = Params {
        kind: PriorityType::Basic,
        ..Params::default()
    };
    params.weight_fairshare = 10000;
    let mut env = Env::new(params);
    env.fairshare.insert(Uid(1), 0.0);
    let mut a = job(3, 1);
    a.spec.priority = 5;
    let mut b = job(1, 2);
    b.spec.nice = 1000;
    let mut c = job(2, 2);
    c.top_seq = Some(9);
    // Nice, top and fair share are ignored, and another user's -o priority
    // does not jump the queue: user 1's single job keeps its place (3).
    let p = env.run(&[a, b, c]);
    assert_eq!(p.iter().map(|p| p.id.0).collect::<Vec<_>>(), [1, 2, 3]);
    assert!(close(p[2].priority, 5.0));
    assert_eq!(p[0].factors, Factors::default());

    // A user's -o orders that user's own jobs: the higher-priority job takes
    // the place of the user's earliest job.
    let mut late = job(3, 1);
    late.spec.priority = 5;
    let p = env.run(&[job(1, 1), job(2, 2), late]);
    assert_eq!(p.iter().map(|p| p.id.0).collect::<Vec<_>>(), [3, 2, 1]);
}

// ---------------------------------------------------------------------------
// Fair share
// ---------------------------------------------------------------------------

fn share_of(table: &[UserShare], uid: u32) -> &UserShare {
    table.iter().find(|u| u.uid == Uid(uid)).unwrap()
}

#[test]
fn fair_share_classic_formula() {
    let shares = BTreeMap::from([(Uid(2), 3)]);
    let usage = BTreeMap::from([(Uid(1), 300.0), (Uid(2), 100.0)]);
    let table = fair_share(&shares, &usage, &BTreeSet::from([Uid(1)]));
    let a = share_of(&table, 1);
    let b = share_of(&table, 2);
    // S: 1/4 and 3/4; U: 3/4 and 1/4.
    assert!(close(a.norm_shares, 0.25));
    assert!(close(b.norm_shares, 0.75));
    assert!(close(a.effective_usage, 0.75));
    assert!(close(b.effective_usage, 0.25));
    assert!(close(a.fairshare, 2f64.powf(-3.0)));
    assert!(close(b.fairshare, 2f64.powf(-1.0 / 3.0)));
    assert!(a.active && b.active);
    assert_eq!(a.shares, DEFAULT_SHARES);
}

#[test]
fn fair_share_edge_cases() {
    // Nobody used anything: everybody gets 1.
    let table = fair_share(
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeSet::from([Uid(1), Uid(2)]),
    );
    assert!(table.iter().all(|u| close(u.fairshare, 1.0)));
    assert!(close(share_of(&table, 1).norm_shares, 0.5));

    // Using exactly one's share gives 0.5.
    let table = fair_share(
        &BTreeMap::new(),
        &BTreeMap::from([(Uid(1), 10.0), (Uid(2), 10.0)]),
        &BTreeSet::new(),
    );
    assert!(close(share_of(&table, 1).fairshare, 0.5));

    // Zero shares: factor 0. An inactive user with explicit shares is
    // listed with what their share would be.
    let shares = BTreeMap::from([(Uid(1), 0), (Uid(9), 2)]);
    let table = fair_share(&shares, &BTreeMap::new(), &BTreeSet::from([Uid(1), Uid(2)]));
    assert!(close(share_of(&table, 1).fairshare, 0.0));
    assert!(close(share_of(&table, 2).norm_shares, 1.0));
    let inactive = share_of(&table, 9);
    assert!(!inactive.active);
    assert!(close(inactive.norm_shares, 2.0 / 3.0));
    assert!(close(inactive.fairshare, 1.0));
    assert_eq!(table.iter().map(|u| u.uid.0).collect::<Vec<_>>(), [1, 2, 9]);
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

#[test]
fn decay_halves_per_half_life() {
    assert!(close(decay_factor(7 * DAY, 7 * DAY), 0.5));
    assert!(close(decay_factor(14 * DAY, 7 * DAY), 0.25));
    assert!(close(decay_factor(0, 7 * DAY), 1.0));
    // No decay.
    assert!(close(decay_factor(30 * DAY, 0), 1.0));
    // Many small steps equal one big step.
    let mut usage = 1000.0;
    for _ in 0..(7 * 24 * 12) {
        usage *= decay_factor(5 * 60 * 1000, 7 * DAY);
    }
    assert!((usage - 500.0).abs() < 1e-6, "{usage}");
}

fn running(id: u64, owner: u32, started: Timestamp, gpus: &[u32]) -> Job {
    let mut j = with_gpus(job(id, owner), gpus);
    j.state = JobState::Running;
    j.started_at = Some(started);
    j
}

#[test]
fn only_running_time_is_charged() {
    use JobState::*;
    let done = Done {
        result: JobResult::Success,
    };
    let queued = Queued { enqueued_at: 0 };
    // Entering and leaving Running is all that matters.
    assert_eq!(run_change(&queued, &Running), RunChange::Start);
    assert_eq!(run_change(&Restoring, &Running), RunChange::Start);
    assert_eq!(run_change(&Paused, &Running), RunChange::Start);
    assert_eq!(run_change(&Checkpointing, &Running), RunChange::Start);
    assert_eq!(run_change(&Running, &Paused), RunChange::Stop);
    assert_eq!(run_change(&Running, &Checkpointing), RunChange::Stop);
    assert_eq!(run_change(&Running, &done), RunChange::Stop);
    // A job killed while checkpointed (or restore-queued) did not run.
    assert_eq!(run_change(&Checkpointed, &done), RunChange::None);
    assert_eq!(
        run_change(&RestoreQueued { enqueued_at: 0 }, &done),
        RunChange::None
    );
    assert_eq!(run_change(&Checkpointing, &Checkpointed), RunChange::None);
    assert_eq!(run_change(&Paused, &done), RunChange::None);
    assert_eq!(run_change(&Running, &Running), RunChange::None);
}

#[test]
fn running_charges_bill_billing_times_seconds() {
    let w = TresWeights::parse("CPU=1.0,GRES/gpu=10.0").unwrap();
    let a = running(1, 1, 0, &[0, 1]);
    let b = running(2, 1, 0, &[]);
    let c = running(3, 2, 0, &[0]);
    let charges = running_charges(
        NOW,
        600_000,
        &[(&a, NOW - 300_000), (&b, NOW - 100_000), (&c, NOW - 60_000)],
        &w,
    );
    // (1 + 2 × 10) × 300 s + 1 × 100 s; 11 × 60 s.
    assert!(close(charges[&Uid(1)], 21.0 * 300.0 + 100.0));
    assert!(close(charges[&Uid(2)], 11.0 * 60.0));
    // At most `max_ms`; nothing for a time in the future.
    assert!(close(
        run_charge(&a, NOW - DAY, NOW, 600_000, &w),
        21.0 * 600.0
    ));
    assert!(close(run_charge(&a, NOW + 1, NOW, 600_000, &w), 0.0));
    assert!(running_charges(NOW, 600_000, &[(&a, NOW)], &w).is_empty());
}

#[test]
fn apply_usage_decays_adds_and_prunes() {
    let usage = BTreeMap::from([(Uid(1), 100.0), (Uid(2), 0.001)]);
    let charges = BTreeMap::from([(Uid(3), 5.0)]);
    let next = apply_usage(&usage, 0.5, &charges);
    assert_eq!(next, BTreeMap::from([(Uid(1), 50.0), (Uid(3), 5.0)]));
}

// ---------------------------------------------------------------------------
// Reset periods
// ---------------------------------------------------------------------------

/// Unix milliseconds of a UTC date and time.
fn utc(year: i64, month: u32, day: u32, hour: i64) -> Timestamp {
    days_from_civil(year, month, day) * DAY + hour * HOUR
}

#[test]
fn civil_date_conversions_round_trip() {
    assert_eq!(days_from_civil(1970, 1, 1), 0);
    assert_eq!(days_from_civil(2000, 3, 1), 11_017);
    for day in [-1000, 0, 59, 11_017, 20_000, 30_000] {
        let (y, m, d) = civil_from_days(day);
        assert_eq!(days_from_civil(y, m, d), day);
    }
    assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
}

#[test]
fn period_starts() {
    // Wednesday 2026-10-14 15:00 UTC.
    let now = utc(2026, 10, 14, 15);
    let start = |period| period_start(now, 0, period);
    assert_eq!(start(ResetPeriod::None), None);
    assert_eq!(start(ResetPeriod::Now), None);
    assert_eq!(start(ResetPeriod::Daily), Some(utc(2026, 10, 14, 0)));
    // Sunday 2026-10-11.
    assert_eq!(start(ResetPeriod::Weekly), Some(utc(2026, 10, 11, 0)));
    assert_eq!(start(ResetPeriod::Monthly), Some(utc(2026, 10, 1, 0)));
    assert_eq!(start(ResetPeriod::Quarterly), Some(utc(2026, 10, 1, 0)));
    assert_eq!(start(ResetPeriod::Yearly), Some(utc(2026, 1, 1, 0)));
    assert_eq!(
        period_start(utc(2026, 5, 20, 0), 0, ResetPeriod::Quarterly),
        Some(utc(2026, 4, 1, 0))
    );
    // Local time 9 hours ahead (JST): 2026-10-15 00:30 local is still
    // 2026-10-14 in UTC, but the local day has begun.
    let jst = 9 * 3600;
    let late = utc(2026, 10, 14, 15) + 30 * 60 * 1000;
    assert_eq!(
        period_start(late, jst, ResetPeriod::Daily),
        Some(utc(2026, 10, 14, 15))
    );
}

#[test]
fn reset_due_after_a_boundary() {
    let last = utc(2026, 10, 13, 23);
    assert!(reset_due(ResetPeriod::Daily, last, utc(2026, 10, 14, 1), 0));
    assert!(!reset_due(
        ResetPeriod::Daily,
        last,
        utc(2026, 10, 13, 23) + 1,
        0
    ));
    assert!(!reset_due(
        ResetPeriod::Weekly,
        last,
        utc(2026, 10, 14, 1),
        0
    ));
    assert!(reset_due(
        ResetPeriod::Weekly,
        last,
        utc(2026, 10, 18, 0),
        0
    ));
    assert!(reset_due(
        ResetPeriod::Monthly,
        last,
        utc(2026, 11, 1, 0),
        0
    ));
    assert!(!reset_due(
        ResetPeriod::Yearly,
        last,
        utc(2026, 12, 31, 23),
        0
    ));
    assert!(reset_due(ResetPeriod::Yearly, last, utc(2027, 1, 1, 0), 0));
    assert!(!reset_due(ResetPeriod::None, last, utc(2030, 1, 1, 0), 0));
    assert!(!reset_due(ResetPeriod::Now, last, utc(2030, 1, 1, 0), 0));
}

#[test]
fn a_reset_is_due_once_per_period_across_a_utc_offset_change() {
    // Central European time: daylight saving time (UTC+2) ends on
    // 2026-10-25 at 03:00 local, which becomes 02:00 (UTC+1).
    let (summer, winter) = (2 * 3600, 3600);
    // The daily reset at local midnight, still in summer time.
    let reset = utc(2026, 10, 24, 22) + 60 * 1000;
    let day = period_start_day(reset, summer, ResetPeriod::Daily).unwrap();
    assert_eq!(
        period_start(reset, summer, ResetPeriod::Daily),
        Some(utc(2026, 10, 24, 22))
    );
    // After the change the day's midnight is 23:00 UTC, after the reset:
    // compared as instants, the reset would be due again.
    let later = utc(2026, 10, 25, 12);
    assert!(reset_due(ResetPeriod::Daily, reset, later, winter));
    assert!(!reset_due_since_day(ResetPeriod::Daily, day, later, winter));
    // The next midnight is due.
    assert!(reset_due_since_day(
        ResetPeriod::Daily,
        day,
        utc(2026, 10, 25, 23) + 1,
        winter
    ));
    // Spring forward does not skip a reset either.
    let reset = utc(2026, 3, 28, 23) + 60 * 1000; // 2026-03-29 00:01 CET
    let day = period_start_day(reset, winter, ResetPeriod::Daily).unwrap();
    assert!(!reset_due_since_day(
        ResetPeriod::Daily,
        day,
        utc(2026, 3, 29, 12),
        summer
    ));
    assert!(reset_due_since_day(
        ResetPeriod::Daily,
        day,
        utc(2026, 3, 29, 22) + 1,
        summer
    ));
    assert!(!reset_due_since_day(ResetPeriod::None, 0, later, winter));
    assert!(!reset_due_since_day(ResetPeriod::Now, 0, later, winter));
}
