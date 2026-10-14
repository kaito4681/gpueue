//! Multifactor priority and fair share in the daemon: usage accounting,
//! the inputs of the scheduler's ordering, and the operations behind
//! `puzuz prio`, `share`, `nice`, `top`, `site` and `group priority`.
//!
//! The calculations are in [`puzuz_core::priority`]; this module reads
//! their inputs from the database and stores their results.
//!
//! Only time spent `Running` is billed. While a job runs, `jobs.run_since`
//! records when its run time was last charged: it is set when the job
//! starts running (spawned, restored, resumed), and when the job stops
//! running (paused, checkpointed, exited, killed, lost) the time since is
//! charged right away and it is cleared ([`Core::account_run`]). Every
//! `priority_calc_period_secs` (and when the daemon starts) every user's
//! usage is decayed by the time since the last calculation, and running
//! jobs are charged up to now. A charge covers at most two periods, so
//! time the daemon was not running is not billed. Usage is reset at the
//! boundaries of `priority_usage_reset_period`.
//!
//! The scheduler orders jobs by fair-share factors cached in [`Core`],
//! recomputed with every usage calculation and whenever shares change, so
//! scheduling passes do not read the usage tables. A user who becomes
//! active in between has no usage and gets the factor 1 (the factor of a
//! user without usage), and the other users' normalised shares catch up at
//! the next calculation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use puzuz_core::authz::{self, AdminOperation};
use puzuz_core::model::{Caller, Group, Job, JobId, JobState, Timestamp, Uid};
use puzuz_core::priority::{self, ResetPeriod, Resources, RunChange, UserShare};
use puzuz_core::scheduler::{self, PriorityInput};
use puzuz_core::storage::{JobFilter, StorageError, StoreOps};
use puzuz_protocol::api::{
    BulkJobResponse, JobActionResult, NiceRequest, PriorityResponse, PriorityWeights,
    SharesResponse, SiteRequest, TopRequest,
};

use crate::error::{ApiResult, Failure};
use crate::state::{Core, now_ms};

/// The fair-share table from the store: shares, usage, and the owners of
/// unfinished jobs as the active users.
pub(crate) fn fair_share_table(s: &dyn StoreOps) -> Result<Vec<UserShare>, StorageError> {
    Ok(priority::fair_share(
        &s.list_user_shares()?,
        &s.list_user_usage()?,
        &s.list_active_owners()?,
    ))
}

/// The local time's offset from UTC, for the reset periods.
fn utc_offset_secs() -> i32 {
    chrono::Local::now().offset().local_minus_utc()
}

/// The number of CPUs of the machine (for the job size and TRES factors).
pub(crate) fn machine_cpus() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

/// Why a job's priority settings cannot be changed in its state.
fn require_unfinished(job: &Job, action: &'static str) -> Result<(), puzuz_core::Error> {
    if matches!(job.state, JobState::Done { .. }) {
        return Err(puzuz_core::Error::InvalidTransition {
            id: job.id,
            action,
            state: job.state.name(),
        });
    }
    Ok(())
}

impl Core {
    /// What the scheduler orders candidates by, for `gpus` usable GPUs.
    pub(crate) fn priority_resources(&self, gpus: usize) -> Resources {
        Resources {
            gpus: gpus as u32,
            cpus: self.cpus,
        }
    }

    /// The cached fair-share factor per user (see the
    /// [module documentation](self)).
    pub(crate) fn fairshare_factors(&self) -> Arc<HashMap<Uid, f64>> {
        self.fairshare.lock().unwrap().clone()
    }

    /// Recomputes the cached fair-share factors from `s`.
    fn refresh_fairshare(&self, s: &dyn StoreOps) -> Result<(), StorageError> {
        let factors = priority::fair_share_factors(&fair_share_table(s)?);
        *self.fairshare.lock().unwrap() = Arc::new(factors);
        Ok(())
    }

    /// The longest stretch one charge covers.
    fn max_charge_ms(&self) -> i64 {
        2 * self.config.priority.priority_calc_period_secs as i64 * 1000
    }

    /// Usage accounting for `job`, which just changed from `old` to its
    /// current state at `now` (see the [module documentation](self)).
    pub(crate) fn account_run(
        &self,
        tx: &dyn StoreOps,
        old: &JobState,
        job: &Job,
        now: Timestamp,
    ) -> Result<(), StorageError> {
        match priority::run_change(old, &job.state) {
            RunChange::Start => tx.set_job_run_since(job.id, Some(now)),
            RunChange::Stop => {
                if let Some(since) = tx.job_run_since(job.id)? {
                    let charge = priority::run_charge(
                        job,
                        since,
                        now,
                        self.max_charge_ms(),
                        &self.config.priority.billing_weights(),
                    );
                    tx.add_user_usage(job.owner, charge)?;
                }
                tx.set_job_run_since(job.id, None)
            }
            RunChange::None => Ok(()),
        }
    }

    // -----------------------------------------------------------------
    // Usage
    // -----------------------------------------------------------------

    /// Prepares usage accounting when the daemon starts: records the first
    /// reset time, resets usage for `priority_usage_reset_period = "now"`,
    /// and runs a first calculation.
    pub async fn start_usage(self: &Arc<Self>) {
        let now = now_ms();
        let reset_now = self.config.priority.priority_usage_reset_period == ResetPeriod::Now;
        let result = self
            .write(move |tx, _| {
                if reset_now {
                    tx.replace_user_usage(&Default::default())?;
                    tx.set_usage_reset_at(now)?;
                    // Nothing before the reset is charged.
                    tx.set_usage_calc_at(now)?;
                    tracing::info!("priority_usage_reset_period = \"now\": usage reset");
                }
                if tx.usage_reset_at()?.is_none() {
                    tx.set_usage_reset_at(now)?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = result {
            tracing::error!("cannot prepare usage accounting: {}", e.0.message);
        }
        if let Err(e) = self.update_usage(now).await {
            tracing::error!("cannot calculate usage: {}", e.0.message);
        }
    }

    /// Recalculates usage every `priority_calc_period_secs`.
    pub async fn usage_loop(self: Arc<Self>) {
        let period = Duration::from_secs(self.config.priority.priority_calc_period_secs.max(1));
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = tokio::time::sleep(period) => {}
            }
            if let Err(e) = self.update_usage(now_ms()).await {
                tracing::error!("cannot calculate usage: {}", e.0.message);
            }
        }
    }

    /// One usage calculation at `now` (see the [module documentation](self)).
    pub async fn update_usage(self: &Arc<Self>, now: Timestamp) -> ApiResult<()> {
        let settings = self.config.priority.clone();
        let offset = utc_offset_secs();
        let max_ms = self.max_charge_ms();
        let core = self.clone();
        self.write(move |tx, ch| {
            let last = tx.usage_calc_at()?.unwrap_or(now);
            let mut usage = tx.list_user_usage()?;
            // Run time before this is not charged (a reset).
            let mut not_before = None;
            let reset_at = tx.usage_reset_at()?.unwrap_or(now);
            let period = settings.priority_usage_reset_period;
            if priority::reset_due(period, reset_at, now, offset) {
                usage.clear();
                tx.set_usage_reset_at(now)?;
                not_before = priority::period_start(now, offset, period);
                tracing::info!(period = %period, "usage reset");
            }
            let mut run_since = tx.list_job_run_since()?;
            let running = tx.list_jobs(&JobFilter::all().state("running"))?;
            let mut charged = Vec::with_capacity(running.len());
            let running_ids: HashSet<JobId> = running.iter().map(|j| j.id).collect();
            for job in &running {
                // A job running without a record (e.g. from before the
                // upgrade) is charged from now on.
                if let Some(since) = run_since.remove(&job.id) {
                    charged.push((job, not_before.map_or(since, |b| since.max(b))));
                }
                tx.set_job_run_since(job.id, Some(now))?;
            }
            // Records of jobs that no longer run (should not happen).
            for id in run_since.into_keys().filter(|id| !running_ids.contains(id)) {
                tx.set_job_run_since(id, None)?;
            }
            let charges =
                priority::running_charges(now, max_ms, &charged, &settings.billing_weights());
            let decay = priority::decay_factor(
                now - last,
                settings.priority_decay_half_life_secs as i64 * 1000,
            );
            let next = priority::apply_usage(&usage, decay, &charges);
            tx.replace_user_usage(&next)?;
            tx.set_usage_calc_at(now)?;
            core.refresh_fairshare(tx)?;
            // The fair-share factors changed.
            ch.touch();
            Ok(())
        })
        .await
    }

    // -----------------------------------------------------------------
    // Reading
    // -----------------------------------------------------------------

    /// `GET /v1/priority`.
    pub async fn priorities(self: &Arc<Self>) -> ApiResult<PriorityResponse> {
        let core = self.clone();
        let now = now_ms();
        // What the scheduler uses: the cached factors and the orphans.
        let fairshare = self.fairshare_factors();
        let reserved = self.orphan_reservations();
        let jobs = self
            .read(move |s| {
                let jobs = s.list_unfinished_jobs()?;
                let groups = s.list_groups()?;
                let gpus = s
                    .list_gpus()?
                    .into_iter()
                    .filter(|gpu| core.gpu_available(gpu.index))
                    .count();
                let input = PriorityInput {
                    params: &core.priority_params,
                    fairshare: &fairshare,
                    resources: core.priority_resources(gpus),
                };
                Ok(scheduler::pending_priorities(
                    now, &jobs, &groups, &reserved, &input,
                ))
            })
            .await?;
        let p = &self.priority_params;
        Ok(PriorityResponse {
            priority_type: p.kind,
            weights: PriorityWeights {
                age: p.weight_age,
                fairshare: p.weight_fairshare,
                job_size: p.weight_job_size,
                partition: p.weight_partition,
                tres: p.weight_tres.to_text(),
            },
            jobs,
        })
    }

    /// `GET /v1/shares`.
    pub async fn shares(&self) -> ApiResult<SharesResponse> {
        let (users, usage_calc_at) = self
            .read(|s| Ok((fair_share_table(s)?, s.usage_calc_at()?)))
            .await?;
        let settings = &self.config.priority;
        Ok(SharesResponse {
            users,
            decay_half_life_secs: settings.priority_decay_half_life_secs,
            calc_period_secs: settings.priority_calc_period_secs,
            usage_reset_period: settings.priority_usage_reset_period,
            billing_weights: settings.billing_weights().to_text(),
            usage_calc_at,
        })
    }

    // -----------------------------------------------------------------
    // Changing
    // -----------------------------------------------------------------

    /// `PUT /v1/shares/{uid}`.
    pub async fn set_shares(
        self: &Arc<Self>,
        caller: Caller,
        uid: Uid,
        shares: u32,
    ) -> ApiResult<()> {
        authz::require_admin_for(&caller, AdminOperation::SetShares)?;
        let core = self.clone();
        self.write(move |tx, ch| {
            tx.set_user_shares(uid, shares)?;
            core.refresh_fairshare(tx)?;
            ch.touch();
            Ok(())
        })
        .await
    }

    /// `PUT /v1/groups/{name}/priority`.
    pub async fn set_group_priority(
        self: &Arc<Self>,
        caller: Caller,
        name: String,
        priority: u32,
    ) -> ApiResult<Group> {
        authz::require_admin_for(&caller, AdminOperation::SetGroupPriority)?;
        self.write(move |tx, ch| {
            tx.set_group_priority(&name, priority)?;
            ch.group(&name);
            tx.get_group(&name)?
                .ok_or_else(|| puzuz_core::Error::GroupNotFound(name).into())
        })
        .await
    }

    /// Applies `check` to each job of `ids` in one transaction, then
    /// `apply` to the jobs that passed; reports per job.
    async fn per_job(
        self: &Arc<Self>,
        ids: Vec<JobId>,
        check: impl Fn(&Job) -> ApiResult<()> + Send + 'static,
        apply: impl FnOnce(&dyn StoreOps, &[JobId]) -> Result<(), StorageError> + Send + 'static,
    ) -> ApiResult<BulkJobResponse> {
        if ids.is_empty() {
            return Err(Failure::bad_request("no job ids given"));
        }
        self.write(move |tx, ch| {
            let mut results = Vec::with_capacity(ids.len());
            let mut passed = Vec::new();
            for id in ids {
                let outcome = match tx.get_job(id)? {
                    None => Err(Failure::from(puzuz_core::Error::JobNotFound(id))),
                    Some(job) => check(&job),
                };
                match outcome {
                    Ok(()) => {
                        if !passed.contains(&id) {
                            passed.push(id);
                        }
                        results.push(JobActionResult::ok(id));
                    }
                    Err(e) => results.push(JobActionResult::error(id, e.0)),
                }
            }
            apply(tx, &passed)?;
            for id in passed {
                ch.updated(id);
            }
            Ok(BulkJobResponse::new(results))
        })
        .await
    }

    /// `POST /v1/jobs/nice`.
    pub async fn nice(
        self: &Arc<Self>,
        caller: Caller,
        request: NiceRequest,
    ) -> ApiResult<BulkJobResponse> {
        let NiceRequest { ids, nice } = request;
        self.per_job(
            ids,
            move |job| {
                authz::require_nice(&caller, job, nice)?;
                Ok(require_unfinished(job, "reniced")?)
            },
            move |tx, ids| {
                for id in ids {
                    tx.set_job_nice(*id, nice)?;
                }
                Ok(())
            },
        )
        .await
    }

    /// `POST /v1/jobs/top`.
    pub async fn top(
        self: &Arc<Self>,
        caller: Caller,
        request: TopRequest,
    ) -> ApiResult<BulkJobResponse> {
        let enable_user_top = self.config.priority.enable_user_top;
        self.per_job(
            request.ids,
            move |job| {
                authz::require_top(&caller, job, enable_user_top)?;
                Ok(require_unfinished(job, "moved to the top")?)
            },
            |tx, ids| {
                if ids.is_empty() {
                    Ok(())
                } else {
                    tx.move_jobs_to_top(ids)
                }
            },
        )
        .await
    }

    /// `POST /v1/jobs/site`.
    pub async fn site(
        self: &Arc<Self>,
        caller: Caller,
        request: SiteRequest,
    ) -> ApiResult<BulkJobResponse> {
        authz::require_admin_for(&caller, AdminOperation::SetSiteFactor)?;
        let SiteRequest { ids, site } = request;
        self.per_job(
            ids,
            |job| Ok(require_unfinished(job, "given a site factor")?),
            move |tx, ids| {
                for id in ids {
                    tx.set_job_site_factor(*id, site)?;
                }
                Ok(())
            },
        )
        .await
    }
}
