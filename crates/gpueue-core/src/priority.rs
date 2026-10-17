//! Job priority: Slurm's priority/multifactor plugin, adapted to gpueue.
//!
//! Pure functions only; the daemon reads the inputs (usage, shares, groups,
//! configuration) and stores the results.
//!
//! # The formula
//!
//! With `priority_type = "multifactor"` (the default) every pending job gets
//!
//! ```text
//! priority = site_factor
//!          + priority_weight_age       * age_factor
//!          + priority_weight_fairshare * fairshare_factor
//!          + priority_weight_job_size  * job_size_factor
//!          + priority_weight_partition * partition_factor
//!          + Σ priority_weight_tres[t] * tres_factor[t]
//!          - nice
//! ```
//!
//! where each factor lies in `[0, 1]`:
//!
//! - **age**: the time since the job became eligible (queued and every
//!   dependency done) divided by `priority_max_age`, at most 1. See
//!   [the owner's order](#the-owners-order) for how ages are assigned.
//! - **fairshare**: the owner's classic Slurm fair-share factor
//!   `F = 2^(-U/S)` (see [`fair_share`]).
//! - **job_size**: the job's GPUs over the machine's GPUs, or for CPU-only
//!   jobs [`CPUS_PER_JOB`] over the machine's CPUs. With
//!   `priority_favor_small`, `(total - requested + 1) / total` instead, so
//!   the smallest jobs get 1.
//! - **partition**: the priority of the job's group over the highest group
//!   priority (0 when every group has priority 0).
//! - **tres**: per resource (`cpu`, `gres/gpu`), the job's request over the
//!   machine's total.
//!
//! `site_factor` is set per job by administrators, `nice` by the owner
//! (`>= 0`) or an administrator (any value).
//!
//! # The owner's order
//!
//! pueue's `-o/--priority`, `gpueue top` and `switch` only decide the order
//! of one user's own jobs (like HTCondor's `condor_prio` and Slurm's
//! `scontrol top`); they never move a job ahead of other users' jobs. The
//! owner's order is: jobs moved to the top first (most recent `top` first),
//! then higher `-o` priority, then lower id ([`owner_order`]).
//!
//! It is applied through the age factor: the ages of a user's pending jobs
//! that could launch now (see [`Eligibility::launchable`]) are pooled and
//! handed out in the owner's order, oldest age to the first of them. A job
//! that cannot launch now (its group is missing or paused, one of its GPUs
//! was not detected, or a dependency has not finished) neither lends nor
//! borrows: it keeps its own age, so an old job parked in a paused group
//! cannot lift the owner's newest job above other users' jobs, and it joins
//! the pool with its full age once it can launch. A job waiting for a
//! dependency has an age of 0 (its age counts from when the dependency
//! finished). Waiting for a free group or GPU slot is ordinary queueing:
//! such a job can launch and pools its age. The sum of the priorities of a
//! user's launchable jobs is unchanged, so other users are not affected,
//! and among jobs of one user that differ in nothing else (the common case:
//! same group, no nice or site factor, job size not weighted) the owner's
//! order decides. Remaining ties are broken by the owner's order, then by
//! id.
//!
//! With `priority_type = "basic"` jobs are ordered as in pueue: higher `-o`
//! priority first, then lower id; nice, site factors and `top` are ignored.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::model::{Group, Job, JobId, JobState, Timestamp, Uid};

/// CPUs a job is counted as using: gpueue does not allocate CPUs, so every
/// job counts as one.
pub const CPUS_PER_JOB: u32 = 1;

/// Shares of a user nobody set shares for.
pub const DEFAULT_SHARES: u32 = 1;

/// Decayed usage below this (billing-seconds) is dropped.
pub const MIN_USAGE: f64 = 1e-3;

const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// `priority_type` (Slurm's `PriorityType`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorityType {
    /// priority/multifactor.
    #[default]
    Multifactor,
    /// priority/basic: pueue's order (`-o` priority, then id), where a user's
    /// `-o` only orders their own jobs (see [`basic_order`]).
    Basic,
}

impl PriorityType {
    pub fn as_str(self) -> &'static str {
        match self {
            PriorityType::Multifactor => "multifactor",
            PriorityType::Basic => "basic",
        }
    }
}

/// `priority_usage_reset_period` (Slurm's `PriorityUsageResetPeriod`).
/// Parsed case-insensitively (`"weekly"` or Slurm's `"WEEKLY"`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum ResetPeriod {
    /// Never reset (only the decay applies).
    #[default]
    None,
    /// Reset whenever the daemon starts.
    Now,
    /// Every day at 00:00 local time.
    Daily,
    /// Every Sunday at 00:00 (as in Slurm).
    Weekly,
    /// The first day of every month at 00:00.
    Monthly,
    /// January, April, July and October 1st at 00:00.
    Quarterly,
    /// January 1st at 00:00.
    Yearly,
}

impl ResetPeriod {
    pub fn as_str(self) -> &'static str {
        match self {
            ResetPeriod::None => "none",
            ResetPeriod::Now => "now",
            ResetPeriod::Daily => "daily",
            ResetPeriod::Weekly => "weekly",
            ResetPeriod::Monthly => "monthly",
            ResetPeriod::Quarterly => "quarterly",
            ResetPeriod::Yearly => "yearly",
        }
    }
}

impl fmt::Display for ResetPeriod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ResetPeriod {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "none" => ResetPeriod::None,
            "now" => ResetPeriod::Now,
            "daily" => ResetPeriod::Daily,
            "weekly" => ResetPeriod::Weekly,
            "monthly" => ResetPeriod::Monthly,
            "quarterly" => ResetPeriod::Quarterly,
            "yearly" => ResetPeriod::Yearly,
            _ => {
                return Err(format!(
                    "unknown reset period {s:?} (expected none, now, daily, weekly, monthly, \
                     quarterly or yearly)"
                ));
            }
        })
    }
}

impl TryFrom<String> for ResetPeriod {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<ResetPeriod> for String {
    fn from(value: ResetPeriod) -> Self {
        value.as_str().to_owned()
    }
}

/// Weights per resource, as in Slurm's `TRESBillingWeights` and
/// `PriorityWeightTRES`: `"CPU=1.0,GRES/gpu=10.0"`. Names are
/// case-insensitive; `cpu` and `gres/gpu` are known (gpueue tracks no
/// memory). Missing resources weigh 0.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TresWeights {
    pub cpu: f64,
    pub gpu: f64,
}

impl TresWeights {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut weights = TresWeights::default();
        let mut seen = HashSet::new();
        for item in text.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (name, value) = item
                .split_once('=')
                .ok_or_else(|| format!("{item:?} is not NAME=WEIGHT"))?;
            let name = name.trim().to_ascii_lowercase();
            let value: f64 = value
                .trim()
                .parse()
                .map_err(|_| format!("{item:?}: the weight is not a number"))?;
            if !value.is_finite() || value < 0.0 {
                return Err(format!("{item:?}: the weight must be a number >= 0"));
            }
            if !seen.insert(name.clone()) {
                return Err(format!("{name} is listed twice"));
            }
            match name.as_str() {
                "cpu" => weights.cpu = value,
                "gres/gpu" => weights.gpu = value,
                _ => {
                    return Err(format!(
                        "unknown resource {name:?} (gpueue knows CPU and GRES/gpu)"
                    ));
                }
            }
        }
        Ok(weights)
    }

    /// `CPU=..,GRES/gpu=..`, leaving out zero weights.
    pub fn to_text(&self) -> String {
        let mut parts = Vec::new();
        if self.cpu != 0.0 {
            parts.push(format!("CPU={}", self.cpu));
        }
        if self.gpu != 0.0 {
            parts.push(format!("GRES/gpu={}", self.gpu));
        }
        parts.join(",")
    }
}

/// Everything about priorities that comes from the configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct Params {
    pub kind: PriorityType,
    pub weight_age: u32,
    pub weight_fairshare: u32,
    pub weight_job_size: u32,
    pub weight_partition: u32,
    pub weight_tres: TresWeights,
    /// Age at which the age factor reaches 1 (`priority_max_age`).
    pub max_age_ms: i64,
    pub favor_small: bool,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            kind: PriorityType::Multifactor,
            weight_age: 1000,
            weight_fairshare: 10000,
            weight_job_size: 0,
            weight_partition: 0,
            weight_tres: TresWeights::default(),
            max_age_ms: 7 * MS_PER_DAY,
            favor_small: false,
        }
    }
}

/// The machine's totals, for the job size and TRES factors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Resources {
    /// GPUs that can run jobs.
    pub gpus: u32,
    /// CPUs of the machine.
    pub cpus: u32,
}

// ---------------------------------------------------------------------------
// Job priorities
// ---------------------------------------------------------------------------

/// The factors of a job, each in `[0, 1]`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Factors {
    pub age: f64,
    pub fairshare: f64,
    pub job_size: f64,
    pub partition: f64,
    /// TRES factor of the CPUs (`cpu`).
    pub tres_cpu: f64,
    /// TRES factor of the GPUs (`gres/gpu`).
    pub tres_gpu: f64,
}

/// Each factor times its weight: what it adds to the priority.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Weighted {
    pub age: f64,
    pub fairshare: f64,
    pub job_size: f64,
    pub partition: f64,
    /// All TRES factors together.
    pub tres: f64,
}

/// A pending job's priority and how it is made up.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobPriority {
    pub id: JobId,
    pub owner: Uid,
    pub group: String,
    /// The priority jobs are ordered by (higher first).
    pub priority: f64,
    pub factors: Factors,
    pub weighted: Weighted,
    pub site: i32,
    pub nice: i32,
}

/// What ordering pending jobs needs besides the jobs.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    pub params: &'a Params,
    pub now: Timestamp,
    /// Fair-share factor per user ([`fair_share_factors`]); users missing
    /// from it get 1.
    pub fairshare: &'a HashMap<Uid, f64>,
    pub groups: &'a [Group],
    pub resources: Resources,
}

/// How [`prioritize`] counts a pending job's age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Eligibility {
    /// When the job became eligible to start: when it was queued, or when
    /// its last dependency finished. `None` while a dependency has not
    /// finished (its age is then 0).
    pub since: Option<Timestamp>,
    /// Whether the job could launch now if a slot were free: its group
    /// exists and is not paused, every GPU it asks for was detected and
    /// (queued jobs) every dependency is done. Only these jobs pool their
    /// ages in their owner's order (see [the owner's
    /// order](self#the-owners-order)). Waiting for a busy group or GPU slot
    /// does not make a job unlaunchable.
    pub launchable: bool,
}

/// The order of one user's jobs: moved to the top first (most recent
/// `top` first), then higher `-o` priority, then lower id.
pub fn owner_order(a: &Job, b: &Job) -> Ordering {
    let top = |job: &Job| job.top_seq.map_or(0, |seq| seq.saturating_add(1));
    top(b)
        .cmp(&top(a))
        .then_with(|| b.spec.priority.cmp(&a.spec.priority))
        .then_with(|| a.id.cmp(&b.id))
}

/// The `priority_type = "basic"` order: pueue's (higher `-o` priority first,
/// then lower id), but a user's `-o` only reorders that user's own jobs.
/// Each owner's jobs are sorted by priority and then take that owner's queue
/// positions (their ids, ascending) in that order, so with equal priorities
/// this is plain FIFO, and a high `-o` cannot put a job ahead of other
/// users' jobs that were queued before the owner's earlier jobs.
pub fn basic_order<'a>(jobs: &[&'a Job]) -> Vec<&'a Job> {
    let mut by_owner: BTreeMap<Uid, Vec<&'a Job>> = BTreeMap::new();
    for job in jobs {
        by_owner.entry(job.owner).or_default().push(job);
    }
    let mut positioned: Vec<(JobId, &'a Job)> = Vec::with_capacity(jobs.len());
    for owned in by_owner.values_mut() {
        let mut slots: Vec<JobId> = owned.iter().map(|job| job.id).collect();
        slots.sort();
        owned.sort_by(|a, b| {
            b.spec
                .priority
                .cmp(&a.spec.priority)
                .then_with(|| a.id.cmp(&b.id))
        });
        positioned.extend(slots.into_iter().zip(owned.iter().copied()));
    }
    positioned.sort_by_key(|(slot, _)| *slot);
    positioned.into_iter().map(|(_, job)| job).collect()
}

/// Distinct GPUs a job asks for.
pub fn gpu_count(job: &Job) -> u32 {
    job.spec.gpus.iter().collect::<HashSet<_>>().len() as u32
}

fn fraction(part: u32, total: u32) -> f64 {
    if total == 0 {
        0.0
    } else {
        (part as f64 / total as f64).clamp(0.0, 1.0)
    }
}

fn job_size_factor(job: &Job, resources: Resources, favor_small: bool) -> f64 {
    let gpus = gpu_count(job);
    let (requested, total) = if gpus > 0 && resources.gpus > 0 {
        (gpus.min(resources.gpus), resources.gpus)
    } else {
        (CPUS_PER_JOB.min(resources.cpus), resources.cpus)
    };
    if total == 0 {
        return 0.0;
    }
    if favor_small {
        fraction(total - requested + 1, total)
    } else {
        fraction(requested, total)
    }
}

/// Orders the pending jobs `candidates` for launching and returns their
/// priorities in that order (highest first). `eligibility` tells how each
/// job's age is counted ([`Eligibility`]).
///
/// See the [module documentation](self) for the rules.
pub fn prioritize(
    candidates: &[&Job],
    ctx: &Context<'_>,
    eligibility: impl Fn(&Job) -> Eligibility,
) -> Vec<JobPriority> {
    let params = ctx.params;
    if params.kind == PriorityType::Basic {
        return basic_order(candidates)
            .into_iter()
            .map(|job| JobPriority {
                id: job.id,
                owner: job.owner,
                group: job.spec.group.clone(),
                priority: job.spec.priority as f64,
                factors: Factors::default(),
                weighted: Weighted::default(),
                site: job.site_factor,
                nice: job.spec.nice,
            })
            .collect();
    }

    // Raw ages, then pooled and handed out in each owner's order.
    let eligibility: HashMap<JobId, Eligibility> =
        candidates.iter().map(|j| (j.id, eligibility(j))).collect();
    let age_of = |job: &Job| -> f64 {
        let Some(since) = eligibility[&job.id].since else {
            return 0.0;
        };
        if params.max_age_ms <= 0 {
            return 1.0;
        }
        ((ctx.now - since) as f64 / params.max_age_ms as f64).clamp(0.0, 1.0)
    };
    let mut by_owner: BTreeMap<Uid, Vec<&Job>> = BTreeMap::new();
    for job in candidates {
        by_owner.entry(job.owner).or_default().push(job);
    }
    let mut age: HashMap<JobId, f64> = HashMap::with_capacity(candidates.len());
    let mut rank: HashMap<JobId, usize> = HashMap::with_capacity(candidates.len());
    for jobs in by_owner.values_mut() {
        jobs.sort_by(|a, b| owner_order(a, b));
        // Only jobs that could launch now pool their ages; the others keep
        // their own (0 while a dependency is unfinished).
        let pooled: Vec<&Job> = jobs
            .iter()
            .copied()
            .filter(|job| {
                let e = eligibility[&job.id];
                e.launchable && e.since.is_some()
            })
            .collect();
        let mut ages: Vec<f64> = pooled.iter().map(|job| age_of(job)).collect();
        ages.sort_by(|a, b| b.total_cmp(a));
        for (job, age_value) in pooled.iter().zip(ages) {
            age.insert(job.id, age_value);
        }
        for (i, job) in jobs.iter().enumerate() {
            age.entry(job.id).or_insert_with(|| age_of(job));
            rank.insert(job.id, i);
        }
    }

    let max_group = ctx.groups.iter().map(|g| g.priority).max().unwrap_or(0);
    let group_priority: HashMap<&str, u32> = ctx
        .groups
        .iter()
        .map(|g| (g.name.as_str(), g.priority))
        .collect();
    let w = params;
    let mut out: Vec<(JobPriority, usize)> = candidates
        .iter()
        .map(|job| {
            let gpus = gpu_count(job);
            let factors = Factors {
                age: age[&job.id],
                fairshare: ctx
                    .fairshare
                    .get(&job.owner)
                    .copied()
                    .unwrap_or(1.0)
                    .clamp(0.0, 1.0),
                job_size: job_size_factor(job, ctx.resources, params.favor_small),
                partition: fraction(
                    group_priority
                        .get(job.spec.group.as_str())
                        .copied()
                        .unwrap_or(0),
                    max_group,
                ),
                tres_cpu: fraction(CPUS_PER_JOB, ctx.resources.cpus),
                tres_gpu: fraction(gpus, ctx.resources.gpus),
            };
            let weighted = Weighted {
                age: w.weight_age as f64 * factors.age,
                fairshare: w.weight_fairshare as f64 * factors.fairshare,
                job_size: w.weight_job_size as f64 * factors.job_size,
                partition: w.weight_partition as f64 * factors.partition,
                tres: w.weight_tres.cpu * factors.tres_cpu + w.weight_tres.gpu * factors.tres_gpu,
            };
            let priority = job.site_factor as f64
                + weighted.age
                + weighted.fairshare
                + weighted.job_size
                + weighted.partition
                + weighted.tres
                - job.spec.nice as f64;
            (
                JobPriority {
                    id: job.id,
                    owner: job.owner,
                    group: job.spec.group.clone(),
                    priority,
                    factors,
                    weighted,
                    site: job.site_factor,
                    nice: job.spec.nice,
                },
                rank[&job.id],
            )
        })
        .collect();
    out.sort_by(|(a, rank_a), (b, rank_b)| {
        b.priority
            .total_cmp(&a.priority)
            .then_with(|| rank_a.cmp(rank_b))
            .then_with(|| a.id.cmp(&b.id))
    });
    out.into_iter().map(|(p, _)| p).collect()
}

// ---------------------------------------------------------------------------
// Fair share
// ---------------------------------------------------------------------------

/// One user's line of the fair-share table (`gpueue share`, like `sshare`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserShare {
    pub uid: Uid,
    /// Raw shares ([`DEFAULT_SHARES`] unless an administrator set them).
    pub shares: u32,
    /// `S`: the shares over the shares of every active user. For an
    /// inactive user, what it would be if they became active.
    pub norm_shares: f64,
    /// Decayed usage in billing-seconds.
    pub raw_usage: f64,
    /// `U`: the raw usage over every user's raw usage.
    pub effective_usage: f64,
    /// `F = 2^(-U/S)`; 0 when `S` is 0.
    pub fairshare: f64,
    /// Whether the user has an unfinished job or recorded usage.
    pub active: bool,
}

/// The classic Slurm fair-share factor of every user that is active (has
/// an unfinished job, `with_jobs`, or recorded `usage`) or has explicitly
/// set `shares`, ordered by uid:
///
/// - `S` (normalised shares) = the user's shares / the sum of the shares of
///   all active users (users without explicit shares have
///   [`DEFAULT_SHARES`]);
/// - `U` (effective usage) = the user's decayed usage / the sum of every
///   user's decayed usage (0 while nobody has usage);
/// - `F` = `2^(-U/S)`, so a user who used exactly their share gets 0.5,
///   less than their share more, and more less. A user with 0 shares gets 0.
pub fn fair_share(
    shares: &BTreeMap<Uid, u32>,
    usage: &BTreeMap<Uid, f64>,
    with_jobs: &BTreeSet<Uid>,
) -> Vec<UserShare> {
    let shares_of = |uid: &Uid| shares.get(uid).copied().unwrap_or(DEFAULT_SHARES);
    let usage_of = |uid: &Uid| usage.get(uid).copied().unwrap_or(0.0).max(0.0);
    let active: BTreeSet<Uid> = with_jobs
        .iter()
        .copied()
        .chain(usage.iter().filter(|(_, u)| **u > 0.0).map(|(uid, _)| *uid))
        .collect();
    let total_shares: f64 = active.iter().map(|uid| shares_of(uid) as f64).sum();
    let total_usage: f64 = usage.values().map(|u| u.max(0.0)).sum();
    active
        .iter()
        .chain(shares.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|uid| {
            let is_active = active.contains(uid);
            let s = shares_of(uid) as f64;
            let denominator = if is_active {
                total_shares
            } else {
                total_shares + s
            };
            let norm_shares = if denominator > 0.0 {
                s / denominator
            } else {
                0.0
            };
            let raw_usage = usage_of(uid);
            let effective_usage = if total_usage > 0.0 {
                raw_usage / total_usage
            } else {
                0.0
            };
            let fairshare = if norm_shares > 0.0 {
                2f64.powf(-effective_usage / norm_shares)
            } else {
                0.0
            };
            UserShare {
                uid: *uid,
                shares: shares_of(uid),
                norm_shares,
                raw_usage,
                effective_usage,
                fairshare,
                active: is_active,
            }
        })
        .collect()
}

/// The fair-share factor per user, for [`Context::fairshare`].
pub fn fair_share_factors(table: &[UserShare]) -> HashMap<Uid, f64> {
    table.iter().map(|u| (u.uid, u.fairshare)).collect()
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

/// The factor usage is multiplied by after `elapsed_ms` with the given
/// half-life (`priority_decay_half_life`; 0 means no decay).
pub fn decay_factor(elapsed_ms: i64, half_life_ms: i64) -> f64 {
    if half_life_ms <= 0 || elapsed_ms <= 0 {
        1.0
    } else {
        0.5f64.powf(elapsed_ms as f64 / half_life_ms as f64)
    }
}

/// A job's billing (Slurm's TRES billing): CPU weight × [`CPUS_PER_JOB`]
/// + GPU weight × its GPUs.
pub fn billing(job: &Job, weights: &TresWeights) -> f64 {
    weights.cpu * CPUS_PER_JOB as f64 + weights.gpu * gpu_count(job) as f64
}

/// How a state change affects usage accounting: only `Running` time is
/// billed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunChange {
    /// The job starts running (spawned, restored, resumed): its uncharged
    /// run time starts counting now.
    Start,
    /// The job stops running (paused, checkpointing, exited, killed, lost):
    /// the time since it was last charged is charged now.
    Stop,
    /// Neither.
    None,
}

/// What the state change from `old` to `new` means for accounting.
pub fn run_change(old: &JobState, new: &JobState) -> RunChange {
    match (*old == JobState::Running, *new == JobState::Running) {
        (false, true) => RunChange::Start,
        (true, false) => RunChange::Stop,
        _ => RunChange::None,
    }
}

/// The usage (billing × seconds) of `job` running from `since` to `until`,
/// at most `max_ms`: a job whose uncharged run time is older than that
/// (the daemon was not running) is not billed for the rest.
pub fn run_charge(
    job: &Job,
    since: Timestamp,
    until: Timestamp,
    max_ms: i64,
    weights: &TresWeights,
) -> f64 {
    let ms = (until - since).clamp(0, max_ms.max(0));
    billing(job, weights) * ms as f64 / 1000.0
}

/// The usage to charge per user at `now` for running jobs, each given with
/// when it was last charged (or started running); see [`run_charge`].
pub fn running_charges(
    now: Timestamp,
    max_ms: i64,
    running: &[(&Job, Timestamp)],
    weights: &TresWeights,
) -> BTreeMap<Uid, f64> {
    let mut out: BTreeMap<Uid, f64> = BTreeMap::new();
    for (job, since) in running {
        let charge = run_charge(job, *since, now, max_ms, weights);
        if charge > 0.0 {
            *out.entry(job.owner).or_default() += charge;
        }
    }
    out
}

/// Applies one calculation period: decays every user's `usage` by
/// `decay`, adds `charges`, and drops usage below [`MIN_USAGE`].
pub fn apply_usage(
    usage: &BTreeMap<Uid, f64>,
    decay: f64,
    charges: &BTreeMap<Uid, f64>,
) -> BTreeMap<Uid, f64> {
    let mut out: BTreeMap<Uid, f64> = usage
        .iter()
        .map(|(uid, u)| (*uid, u.max(0.0) * decay))
        .collect();
    for (uid, charge) in charges {
        *out.entry(*uid).or_default() += charge.max(0.0);
    }
    out.retain(|_, u| *u >= MIN_USAGE);
    out
}

// ---------------------------------------------------------------------------
// Usage reset periods
// ---------------------------------------------------------------------------

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let m = month as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day since 1970-01-01 (`civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// The local calendar day (days since 1970-01-01) on which the reset
/// period containing `now` began, with local time `utc_offset_secs` ahead
/// of UTC; `None` for `none` and `now`, which have no calendar boundaries.
pub fn period_start_day(now: Timestamp, utc_offset_secs: i32, period: ResetPeriod) -> Option<i64> {
    let offset_ms = utc_offset_secs as i64 * 1000;
    let today = (now + offset_ms).div_euclid(MS_PER_DAY);
    let (year, month, _) = civil_from_days(today);
    Some(match period {
        ResetPeriod::None | ResetPeriod::Now => return None,
        ResetPeriod::Daily => today,
        // 1970-01-01 was a Thursday; Sunday is 0.
        ResetPeriod::Weekly => today - (today + 4).rem_euclid(7),
        ResetPeriod::Monthly => days_from_civil(year, month, 1),
        ResetPeriod::Quarterly => days_from_civil(year, (month - 1) / 3 * 3 + 1, 1),
        ResetPeriod::Yearly => days_from_civil(year, 1, 1),
    })
}

/// When the current reset period began (in Unix milliseconds), with local
/// time `utc_offset_secs` ahead of UTC; `None` for `none` and `now`,
/// which have no calendar boundaries.
pub fn period_start(
    now: Timestamp,
    utc_offset_secs: i32,
    period: ResetPeriod,
) -> Option<Timestamp> {
    let offset_ms = utc_offset_secs as i64 * 1000;
    period_start_day(now, utc_offset_secs, period).map(|day| day * MS_PER_DAY - offset_ms)
}

/// Whether usage last reset (or first recorded) at `last_reset` must be
/// reset at `now`: a period boundary lies after `last_reset`.
///
/// Compares instants, so a boundary computed with a different UTC offset
/// than the one in force at `last_reset` can be off by the difference: on
/// the day daylight saving time ends, the day's midnight moves to after a
/// reset made at the earlier midnight. Prefer [`reset_due_since_day`]; this
/// is for state recorded before the day was.
pub fn reset_due(
    period: ResetPeriod,
    last_reset: Timestamp,
    now: Timestamp,
    utc_offset_secs: i32,
) -> bool {
    period_start(now, utc_offset_secs, period).is_some_and(|start| start > last_reset)
}

/// Whether usage last reset for the period that began on local day
/// `last_reset_day` (see [`period_start_day`]) must be reset at `now`: the
/// current period began on a later day. Days are compared rather than
/// instants, so a change of the UTC offset (daylight saving time) does not
/// make a boundary that has been passed due again.
pub fn reset_due_since_day(
    period: ResetPeriod,
    last_reset_day: i64,
    now: Timestamp,
    utc_offset_secs: i32,
) -> bool {
    period_start_day(now, utc_offset_secs, period).is_some_and(|day| day > last_reset_day)
}

#[cfg(test)]
mod tests;
