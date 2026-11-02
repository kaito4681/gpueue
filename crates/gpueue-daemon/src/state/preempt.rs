//! Preemption: checkpointing running jobs to make room for higher-priority
//! ones, and restoring them by themselves once there is room again.
//!
//! The scheduler picks the jobs
//! ([`gpueue_core::scheduler::plan_preemptive`]); `Core::schedule_once`
//! preempts them with [`Core::preempt_in_tx`] in the transaction it
//! planned in, and sends their dumps once it has committed
//! ([`Core::send_preemptions`]). A preempted job is checkpointed like one
//! of `gpueue checkpoint` (see [`lifecycle::preempt`]), marked with
//! [`Job::preempted_at`] in the same transaction. Its dump's outcome is applied by the usual event handling:
//! a successful dump puts it back in line to be restored (`RestoreQueued`),
//! where the scheduler restores it like any other job waiting for a slot,
//! and every other outcome is that of a checkpoint (see the lifecycle
//! documentation). The mark is persisted, so a dump that is still running
//! when the daemon restarts ends the same way.
//!
//! Two things the scheduler needs are kept in memory only:
//! - [`Core::preempt_refused`]: jobs whose preemption checkpoint did not
//!   happen (the helper refused it, rolled it back, or the request was
//!   lost). They are not offered for preemption again for
//!   [`PREEMPT_REFUSAL_MEMORY_MS`] or until they stop running, so that a
//!   job the helper cannot dump is not tried at every scheduling pass (and
//!   its owner warned each time). After a daemon restart such a job is
//!   tried once more.
//! - [`Core::running_since`]: when each running job last started running,
//!   by spawn or restore, for `preempt_exempt_secs`. `jobs.run_since` does
//!   not tell: it is moved forward whenever the job's run time is charged
//!   and cleared while the job is paused. After a daemon restart the jobs
//!   that were running have no entry, and are not exempt.
//!
//! A preempted job's log gets one line before its dump is requested
//! ([`PREEMPTION_NOTE`]). Not after the dump: CRIU checks on restore that
//! the files the job had open still have the size they had when it was
//! dumped, so writing to the log in between would make the restore fail.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use gpueue_core::lifecycle::{self, Effect};
use gpueue_core::model::{Job, JobId, JobState, Timestamp};
use gpueue_core::storage::{StoreOps, Tx};

use super::{Changes, Core};
use crate::config::PriorityConfig;
use crate::error::ApiResult;
use crate::logfile;

/// How long a job whose preemption checkpoint did not happen is not
/// offered for preemption again (see [`Core::preempt_refused`]).
pub(crate) const PREEMPT_REFUSAL_MEMORY_MS: Timestamp = 10 * 60 * 1000;

/// The line appended to a preempted job's log before its dump.
pub(crate) const PREEMPTION_NOTE: &str = "[gpueue] checkpointing the job to make room for a \
     higher-priority job (preemption); it resumes automatically once there is room again";

/// The preemption settings of the `[priority]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preemption {
    /// `preemption`: whether the scheduler may preempt jobs at all.
    pub enabled: bool,
    /// `preempt_exempt_secs`, in milliseconds: a job is not preempted
    /// until it has run this long since it last started or was restored
    /// (see [`Core::running_since`]).
    pub exempt_ms: i64,
}

impl Preemption {
    pub fn new(config: &PriorityConfig) -> Self {
        Self {
            enabled: config.preemption,
            // At most a year (validated when the file was read).
            exempt_ms: i64::try_from(config.preempt_exempt_secs.saturating_mul(1000))
                .unwrap_or(i64::MAX),
        }
    }
}

/// What [`Core::running_since`] and [`Core::preempt_refused`] learn from a
/// committed transaction (see [`note_run_change`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunNote {
    /// The job started running (spawned or restored) at this time.
    Started(Timestamp),
    /// The job is no longer running, paused or being checkpointed (or was
    /// removed).
    Stopped,
    /// The job's preemption checkpoint did not happen, at this time; it
    /// runs (or is paused) as before.
    PreemptRefused(Timestamp),
}

/// Running, paused, or being checkpointed: still the same run, as far as
/// [`Core::running_since`] is concerned.
fn in_run(state: &JobState) -> bool {
    matches!(
        state,
        JobState::Running | JobState::Paused | JobState::Checkpointing
    )
}

/// Records in `changes` what `job`'s change from `old` at `now` means for
/// [`Core::running_since`] and [`Core::preempt_refused`];
/// `preempted_before`: its [`Job::preempted_at`] before the change.
pub(super) fn note_run_change(
    changes: &mut Changes,
    old: &JobState,
    job: &Job,
    preempted_before: Option<Timestamp>,
    now: Timestamp,
) {
    let runs = matches!(job.state, JobState::Running | JobState::Paused);
    if runs && !in_run(old) {
        changes.note_run(job.id, RunNote::Started(now));
    } else if in_run(old) && !in_run(&job.state) {
        changes.note_run(job.id, RunNote::Stopped);
    }
    // Back from a dump without being dumped: refused or rolled back.
    if preempted_before.is_some()
        && job.preempted_at.is_none()
        && *old == JobState::Checkpointing
        && runs
    {
        changes.note_run(job.id, RunNote::PreemptRefused(now));
    }
}

impl Core {
    /// Keeps a committed [`RunNote`] in memory.
    pub(super) fn apply_run_note(&self, id: JobId, note: RunNote) {
        match note {
            RunNote::Started(at) => {
                self.running_since.lock().unwrap().insert(id, at);
                self.preempt_refused.lock().unwrap().remove(&id);
            }
            RunNote::Stopped => {
                self.running_since.lock().unwrap().remove(&id);
                self.preempt_refused.lock().unwrap().remove(&id);
            }
            RunNote::PreemptRefused(at) => {
                tracing::info!(job = %id, "the preemption checkpoint did not happen; the job keeps its slots");
                self.preempt_refused.lock().unwrap().insert(id, at);
            }
        }
    }

    /// The jobs not to preempt now because their preemption checkpoint did
    /// not happen (refused, rolled back, or its request lost) within the
    /// last [`PREEMPT_REFUSAL_MEMORY_MS`] before `now`. A job is dropped
    /// earlier once it stops running (finishes, is checkpointed some other
    /// way, ...); pausing or resuming it keeps it. Kept in memory only.
    pub(crate) fn preempt_refused(&self, now: Timestamp) -> HashSet<JobId> {
        let mut refused = self.preempt_refused.lock().unwrap();
        refused.retain(|_, at| now.saturating_sub(*at) < PREEMPT_REFUSAL_MEMORY_MS);
        refused.keys().copied().collect()
    }

    /// When each running or paused job (and each one being checkpointed)
    /// last started running: when it was spawned, or when its restore
    /// finished. Resuming a paused job, or a dump that did not happen, does
    /// not count as a start. Kept in memory only: jobs that were running
    /// when the daemon started are missing, and so not exempt from
    /// preemption by `preempt_exempt_secs`.
    pub(crate) fn running_since(&self) -> HashMap<JobId, Timestamp> {
        self.running_since.lock().unwrap().clone()
    }

    /// Whether jobs can be preempted now: preemption is enabled, and jobs
    /// could be launched (the helper is ready, the daemon is not stopping
    /// or shutting down, and launches are not paused). A preempted job must
    /// be restored later, and its dump needs the helper. Consistent with
    /// the database when called inside [`Core::write`] (see
    /// `Core::stop_launches`).
    pub(super) fn can_preempt(&self) -> bool {
        self.preemption.enabled
            && !self.is_shutting_down()
            && self.helper_ready()
            && !self.launches_stopped.is_cancelled()
            && !self.launches_paused()
    }

    /// The jobs of `jobs` the scheduler must not choose to preempt at
    /// `now`: those of [`Core::preempt_refused`], and the running or paused
    /// ones [`Core::preempt_in_tx`] would leave alone (their spawn or
    /// restore has not been answered yet, or `reset` is removing them), so
    /// that the scheduler does not count on slots that are not freed.
    /// Consistent with the database when called inside [`Core::write`].
    pub(super) fn not_preemptible(&self, jobs: &[Job], now: Timestamp) -> HashSet<JobId> {
        let mut out = self.preempt_refused(now);
        let removing = self.pending_removal.lock().unwrap();
        out.extend(
            jobs.iter()
                .filter(|j| matches!(j.state, JobState::Running | JobState::Paused))
                .filter(|j| self.launch_in_flight(j.id) || removing.contains_key(&j.id))
                .map(|j| j.id),
        );
        out
    }

    /// Preempts `job` (re-read in this transaction) at `now`: marks it
    /// ([`Job::preempted_at`]) and applies [`lifecycle::preempt`]. Returns
    /// the effects to run once the transaction has committed (see
    /// [`Core::run_preemption`]), or `None`, changing nothing, if the job
    /// cannot be preempted now: it is not running or paused, has
    /// `--no-checkpoint`, its spawn or restore has not been answered yet,
    /// or `reset` is removing it. Does not check [`Core::can_preempt`].
    pub(crate) fn preempt_in_tx(
        &self,
        tx: &Tx<'_>,
        job: &mut Job,
        now: Timestamp,
        ch: &mut Changes,
    ) -> ApiResult<Option<Vec<Effect>>> {
        let Ok(transition) = lifecycle::preempt(job) else {
            return Ok(None);
        };
        if self.launch_in_flight(job.id)
            || self.pending_removal.lock().unwrap().contains_key(&job.id)
        {
            return Ok(None);
        }
        job.preempted_at = Some(now);
        tx.set_job_preempted_at(job.id, Some(now))?;
        self.apply_in_tx(tx, job, &transition, now, ch)?;
        Ok(Some(transition.effects))
    }

    /// Notes the preemption in `job`'s log, then runs the effects
    /// [`Core::preempt_in_tx`] returned (the dump request), as for
    /// `gpueue checkpoint`. A request that fails puts the job back as it
    /// was (and in [`Core::preempt_refused`]).
    pub(crate) async fn run_preemption(self: &Arc<Self>, job: &Job, effects: &[Effect]) {
        let (logs, owner, id) = (self.config.logs_dir(), job.owner, job.id);
        let noted = tokio::task::spawn_blocking(move || {
            logfile::append_note(&logs, owner, id, PREEMPTION_NOTE)
        })
        .await;
        if let Ok(Err(e)) = noted {
            tracing::warn!(job = %id, "cannot note the preemption in the log: {e}");
        }
        if let Err(e) = self.run_effects(job, effects).await {
            tracing::info!(job = %id, "the preemption checkpoint did not start: {}", e.0.message);
        }
    }

    /// Sends the dump requests of the jobs `preempted` in a committed
    /// transaction (with the effects [`Core::preempt_in_tx`] returned),
    /// each on its own task, so a slow helper answer does not hold up the
    /// caller.
    pub(super) fn send_preemptions(self: &Arc<Self>, preempted: Vec<(Job, Vec<Effect>)>) {
        for (job, effects) in preempted {
            tracing::info!(job = %job.id, "preempting the job to make room for a higher-priority job");
            let core = self.clone();
            tokio::spawn(async move {
                // A request not sent when the daemon stops leaves the job
                // checkpointing; the next daemon's reconciliation puts it
                // back to running (see `Core::abandon_lost_checkpoint`).
                tokio::select! {
                    biased;
                    _ = core.cancel.cancelled() => {}
                    _ = core.run_preemption(&job, &effects) => {}
                }
            });
        }
    }

    /// Preempts the jobs `ids` (see [`Core::preempt_in_tx`]) in one
    /// transaction, and sends their dump requests (see
    /// [`Core::send_preemptions`]). Returns the jobs that are being
    /// preempted; none if jobs cannot be preempted now (see
    /// [`Core::can_preempt`]). Skips jobs that cannot be preempted.
    ///
    /// The scheduler does not use this: it preempts the jobs it chooses in
    /// the transaction it chose them in (see `Core::schedule_once`).
    #[cfg(test)]
    pub(crate) async fn preempt_jobs(self: &Arc<Self>, ids: Vec<JobId>) -> ApiResult<Vec<JobId>> {
        if ids.is_empty() || !self.can_preempt() {
            return Ok(Vec::new());
        }
        let now = self.now_ms();
        let core = self.clone();
        let preempted = self
            .write(move |tx, ch| {
                // Checked again under the database lock, as for launches
                // (see `Core::stop_launches`).
                if !core.can_preempt() {
                    return Ok(Vec::new());
                }
                let mut preempted = Vec::new();
                for id in ids {
                    let Some(mut job) = tx.get_job(id)? else {
                        continue;
                    };
                    if let Some(effects) = core.preempt_in_tx(tx, &mut job, now, ch)? {
                        preempted.push((job, effects));
                    }
                }
                Ok(preempted)
            })
            .await?;
        let ids = preempted.iter().map(|(job, _)| job.id).collect();
        self.send_preemptions(preempted);
        Ok(ids)
    }
}
