//! The operations behind the API endpoints.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use puzuz_core::authz::{self, AdminOperation};
use puzuz_core::lifecycle::{self, Action, Effect, Transition};
use puzuz_core::model::{Caller, Gpu, GpuIndex, Group, Job, JobId, JobState, Timestamp};
use puzuz_core::scheduler::{self, LaunchWait};
use puzuz_core::storage::{JobFilter, StoreOps, Tx};
use puzuz_protocol::api::{
    ActionOutcome, AddJobRequest, AddJobResponse, BulkJobRequest, BulkJobResponse, CleanRequest,
    CleanResponse, CreateGroupRequest, ErrorCode, GpuJob, GpuView, JobAction, JobActionResult,
    JobSelector, JobView, KillOptions, ResetRequest, SetGpusParallelRequest, SwitchRequest,
};

use crate::error::{ApiResult, Failure};
use crate::state::{Core, Force, JobInfo, Retried, needs_helper, now_ms, targets_live_process};

fn lifecycle_action(action: &JobAction) -> Action {
    match action {
        JobAction::Pause(_) => Action::Pause,
        JobAction::Start => Action::Start,
        JobAction::Kill(_) => Action::Kill,
        JobAction::Checkpoint => Action::Checkpoint,
        JobAction::Restore(_) => Action::Restore,
        JobAction::Restart(_) => Action::Restart,
        JobAction::Stash(_) => Action::Stash,
        JobAction::Enqueue(_) => Action::Enqueue,
        JobAction::Remove => Action::Remove,
    }
}

fn transition_for(
    action: &JobAction,
    job: &Job,
    now: Timestamp,
) -> Result<Transition, puzuz_core::Error> {
    match action {
        JobAction::Pause(_) => lifecycle::pause(job),
        JobAction::Start => lifecycle::start(job),
        JobAction::Kill(options) => lifecycle::kill(job, options.signal),
        JobAction::Checkpoint => lifecycle::checkpoint(job),
        JobAction::Restore(options) => lifecycle::restore(job, options.immediate, now),
        JobAction::Restart(options) => {
            lifecycle::restart(job, options.in_place, options.stashed, now)
        }
        JobAction::Stash(options) => lifecycle::stash(job, options.enqueue_at),
        JobAction::Enqueue(options) => lifecycle::enqueue(job, options.at, now),
        JobAction::Remove => lifecycle::remove(job),
    }
}

/// What a successful [`Core::job_action`] reports.
#[derive(Debug, Clone, Default)]
pub struct ActionDone {
    /// The job created by `restart` (unless in place).
    pub new_id: Option<JobId>,
    /// The launch the action asked for waits for the helper to connect.
    pub launch_pending: bool,
    /// The launch the action asked for (a non-administrator's) waits in
    /// line for a GPU slot or its paused group, and why.
    pub waiting: Option<LaunchWait>,
}

impl ActionDone {
    pub fn outcome(self) -> ActionOutcome {
        ActionOutcome::Ok {
            new_id: self.new_id,
            launch_pending: self.launch_pending,
            waiting: self.waiting,
        }
    }
}

/// Whether the launch an action asked for waits in line: for the helper
/// to connect (`pending`), or, for a non-administrator, for a GPU slot or
/// a paused group (`waiting`).
#[derive(Debug, Default)]
struct LaunchState {
    pending: bool,
    waiting: Option<LaunchWait>,
}

/// How a launching action launches its job.
enum LaunchPlan {
    /// Right away, with the action's own transition.
    Now,
    /// It waits in line, ahead of the queue (the helper is not connected,
    /// or, with `Some`, a non-administrator's launch waits for a GPU slot
    /// or a paused group).
    Queue(Option<LaunchWait>),
}

/// A launch (`Spawn` or `Restore`) among the effects.
fn launches(effects: &[Effect]) -> bool {
    effects
        .iter()
        .any(|e| matches!(e, Effect::Spawn | Effect::Restore))
}

fn job_not_found(id: JobId) -> Failure {
    puzuz_core::Error::JobNotFound(id).into()
}

fn list_ids(ids: &[JobId]) -> String {
    ids.iter()
        .map(JobId::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The jobs a `group` or `all` selection acts on: those in `scope`; for
/// `start` only paused ones (queued jobs wait for their slot); none for
/// `pause --wait`.
fn bulk_targets(
    jobs: &[Job],
    action: &JobAction,
    scope: &authz::Scope,
    waiting_pause: bool,
) -> Vec<JobId> {
    if waiting_pause {
        return Vec::new();
    }
    jobs.iter()
        .filter(|j| scope.includes(j))
        .filter(|j| *action != JobAction::Start || j.state == JobState::Paused)
        .map(|j| j.id)
        .collect()
}

impl Core {
    // -----------------------------------------------------------------
    // Jobs: reading
    // -----------------------------------------------------------------

    pub async fn list_jobs(
        &self,
        caller: Caller,
        group: Option<String>,
    ) -> ApiResult<Vec<JobView>> {
        let infos = self
            .read(move |s| {
                let mut filter = JobFilter::all();
                if let Some(group) = group.as_deref() {
                    filter = filter.group(group);
                }
                // A fixed number of queries, not a few per job.
                let jobs = s.list_jobs(&filter)?;
                let mut checkpoints: HashMap<JobId, _> = s
                    .list_checkpoints()?
                    .into_iter()
                    .map(|record| (record.job_id, record))
                    .collect();
                let mut errors: HashMap<JobId, _> = s
                    .list_job_errors()?
                    .into_iter()
                    .map(|error| (error.job_id, error))
                    .collect();
                Ok(jobs
                    .into_iter()
                    .map(|job| JobInfo {
                        checkpoint: checkpoints.remove(&job.id),
                        error: errors.remove(&job.id),
                        waiting: None,
                        job,
                    })
                    .collect::<Vec<_>>())
            })
            .await?;
        Ok(infos
            .into_iter()
            .map(|mut info| {
                info.waiting = self.launch_wait(info.job.id);
                info.view(&caller)
            })
            .collect())
    }

    pub async fn job_info(&self, id: JobId) -> ApiResult<JobInfo> {
        self.read(move |s| match s.get_job(id)? {
            Some(job) => JobInfo::load(s, job).map(Some),
            None => Ok(None),
        })
        .await?
        .map(|mut info| {
            info.waiting = self.launch_wait(id);
            info
        })
        .ok_or_else(|| job_not_found(id))
    }

    // -----------------------------------------------------------------
    // Jobs: add
    // -----------------------------------------------------------------

    /// `POST /v1/jobs`. With `immediate` the job starts right away,
    /// ignoring its group's slots (an administrator's also GPU slots and a
    /// paused group); while the helper is not connected, or a
    /// non-administrator's job has to wait for a GPU slot or its paused
    /// group, it is added as queued instead and started ahead of the queue
    /// as soon as it may, which the response points out.
    pub async fn add_job(
        self: &Arc<Self>,
        caller: Caller,
        request: AddJobRequest,
    ) -> ApiResult<AddJobResponse> {
        self.reject_if_shutting_down()?;
        if caller.uid.0 == 0 {
            return Err(Failure::bad_request(
                "jobs cannot run as root; submit them as a regular user",
            ));
        }
        let spec = request.spec;
        if spec.command.trim().is_empty() {
            return Err(Failure::bad_request("the command is empty"));
        }
        if !spec.cwd.is_absolute() {
            return Err(Failure::bad_request(format!(
                "the working directory {} is not absolute",
                spec.cwd.display()
            )));
        }
        let mut seen = HashSet::new();
        if let Some(gpu) = spec.gpus.iter().find(|g| !seen.insert(**g)) {
            return Err(Failure::bad_request(format!("GPU {gpu} is listed twice")));
        }
        authz::require_nice_value(&caller, 0, spec.nice)?;
        let helper_ready = self.helper_ready();
        let now = now_ms();
        let core = self.clone();
        let (job, effects, waiting) = self
            .write(move |tx, ch| {
                if tx.get_group(&spec.group)?.is_none() {
                    return Err(puzuz_core::Error::GroupNotFound(spec.group.clone()).into());
                }
                for gpu in &spec.gpus {
                    if tx.get_gpu(*gpu)?.is_none() {
                        return Err(Failure::bad_request(format!(
                            "GPU {gpu} does not exist (see `puzuz gpu`)"
                        )));
                    }
                }
                core.require_gpus_available(&spec.gpus)?;
                for dep in &spec.dependencies {
                    let Some(dep_job) = tx.get_job(*dep)? else {
                        return Err(Failure::bad_request(format!(
                            "dependency {dep} does not exist"
                        )));
                    };
                    // A dependant keeps its dependency from being removed
                    // or cleaned, so only jobs the caller could remove
                    // themselves can be depended on.
                    if !authz::can_mutate(&caller, &dep_job) {
                        return Err(Failure::new(
                            ErrorCode::PermissionDenied,
                            format!(
                                "job {dep} belongs to another user; \
                                 a job can only depend on your own jobs"
                            ),
                        ));
                    }
                }
                let state = if request.stashed {
                    JobState::Stashed {
                        enqueue_at: request.enqueue_at,
                    }
                } else {
                    JobState::Queued { enqueued_at: now }
                };
                let mut job = tx.insert_job(caller.uid, &spec, &state, now)?;
                ch.added(job.id);
                let mut effects = Vec::new();
                let mut waiting = None;
                if request.immediate {
                    let force = Force::of(&caller);
                    let t = match core.launch_plan(tx, &job, force, helper_ready, now)? {
                        LaunchPlan::Now => lifecycle::start(&job)?,
                        LaunchPlan::Queue(wait) => {
                            waiting = wait;
                            lifecycle::queue_for_launch(&job, now)?
                        }
                    };
                    core.apply_in_tx(tx, &mut job, &t, now, ch)?;
                    // Also when starting now: should the helper not start
                    // it after all (it is stopping), it is retried the same
                    // way.
                    core.force_launch(job.id, force);
                    if let Some(wait) = &waiting {
                        core.set_launch_wait(job.id, wait.clone());
                    }
                    effects = t.effects;
                }
                Ok((job, effects, waiting))
            })
            .await?;
        let mut launch_pending = request.immediate && !helper_ready;
        match self.run_launch_effects(&job, &effects).await {
            Ok(deferred) => launch_pending |= deferred,
            Err(e) => {
                tracing::warn!(job = %job.id, "cannot start job immediately: {}", e.0.message)
            }
        }
        if launch_pending {
            tracing::info!(job = %job.id, "the helper is not connected; the job starts once it is");
        }
        Ok(AddJobResponse {
            id: job.id,
            launch_pending,
            waiting,
        })
    }

    // -----------------------------------------------------------------
    // Jobs: actions
    // -----------------------------------------------------------------

    /// Applies `action` to one job. `removed_together` lists jobs removed in
    /// the same request (they do not block each other's removal). Returns
    /// the id of a job created by `restart`.
    ///
    /// An action on the live process (pause, kill, checkpoint, ...) of a job
    /// whose spawn or restore request has not been answered yet waits for
    /// that answer, then re-evaluates the action against the job's state at
    /// that point (so it fails cleanly if the spawn failed).
    ///
    /// Launching actions (`start` of a queued or stashed job, `restore
    /// --immediate`, `restart --immediate`) skip the queue and ignore the
    /// group's slots; an administrator's also GPU slots and a paused group,
    /// anybody else's wait for those ([`ActionDone::waiting`], see
    /// [`Core::launch_plan`]). While the helper is not connected they do
    /// not fail: the job is queued (or queued for restore) and launched,
    /// still ahead of the queue, as soon as the helper is back
    /// ([`ActionDone::launch_pending`]). Actions on a live process
    /// (`start` of a paused job, `pause`, `kill`, `checkpoint`) need the
    /// helper and fail with `helper_unavailable` instead.
    pub async fn job_action(
        self: &Arc<Self>,
        caller: Caller,
        id: JobId,
        action: JobAction,
        removed_together: Vec<JobId>,
    ) -> ApiResult<ActionDone> {
        self.reject_if_shutting_down()?;
        let (job, effects, copy, launch) = loop {
            let step = self
                .job_action_tx(caller, id, action.clone(), removed_together.clone())
                .await?;
            match step {
                Some(step) => break step,
                // Invalid actions were rejected above without waiting.
                None => self.wait_for_launch(id).await?,
            }
        };
        let mut launch_pending = launch.pending;
        let result = self.run_launch_effects(&job, &effects).await;
        if let Some((copy, effects)) = &copy {
            // Run even if the first effects failed: the copy may be launching.
            launch_pending |= self.run_launch_effects(copy, effects).await?;
        }
        launch_pending |= result?;
        Ok(ActionDone {
            new_id: copy.map(|(copy, _)| copy.id),
            launch_pending,
            waiting: launch.waiting,
        })
    }

    /// [`Core::job_action`] on a job named by its id. An administrator's
    /// `kill` of an orphan (a process the helper runs that the database
    /// does not account for, see [`crate::state::Orphan`]) stops that
    /// process, whatever the database says about the id.
    pub async fn job_action_by_id(
        self: &Arc<Self>,
        caller: Caller,
        id: JobId,
        action: JobAction,
        removed_together: Vec<JobId>,
    ) -> ApiResult<ActionDone> {
        if let JobAction::Kill(options) = &action
            && caller.is_admin
            && self.kill_orphan(id, options.signal).await?
        {
            return Ok(ActionDone::default());
        }
        self.job_action(caller, id, action, removed_together).await
    }

    /// Runs the effects of an action. A launch the helper did not carry
    /// out but that is retried by itself (the helper is stopping or went
    /// away, see [`Core::launch_retried`]) is no failure: returns whether
    /// that retry is still pending. A retry that has already succeeded is
    /// reported as a launch, one that has already failed with its error.
    async fn run_launch_effects(
        self: &Arc<Self>,
        job: &Job,
        effects: &[Effect],
    ) -> ApiResult<bool> {
        // The launch these effects run, registered by the action's
        // transaction (the job cannot be launched again before its request
        // has been answered).
        let launch = if launches(effects) {
            self.latest_launch(job.id)
        } else {
            None
        };
        let error = match self.run_effects(job, effects).await {
            Ok(()) => return Ok(false),
            Err(e) => e,
        };
        let Some(seq) = launch else {
            return Err(error);
        };
        match self.launch_retried(job.id, seq).await {
            Retried::No => Err(error),
            Retried::Pending => {
                tracing::info!(job = %job.id, "launch deferred until the helper is back: {}", error.0.message);
                Ok(true)
            }
            Retried::Launched => {
                tracing::info!(job = %job.id, "launch retried successfully after: {}", error.0.message);
                Ok(false)
            }
            Retried::Failed(retry) => {
                tracing::info!(job = %job.id, "retried launch failed: {}", retry.0.message);
                Err(retry)
            }
        }
    }

    /// The transaction of [`Core::job_action`]. Returns `None`, changing
    /// nothing, when the action targets the live process and the job's
    /// launch is still in flight. Otherwise returns the job and the effects
    /// to run, the job created by `restart` and its effects, and whether a
    /// launch waits in line ([`LaunchState`]).
    #[allow(clippy::type_complexity)]
    async fn job_action_tx(
        self: &Arc<Self>,
        caller: Caller,
        id: JobId,
        action: JobAction,
        removed_together: Vec<JobId>,
    ) -> ApiResult<Option<(Job, Vec<Effect>, Option<(Job, Vec<Effect>)>, LaunchState)>> {
        let helper_ready = self.helper_ready();
        let now = now_ms();
        let core = self.clone();
        self.write(move |tx, ch| {
                let mut job = tx.get_job(id)?.ok_or_else(|| job_not_found(id))?;
                authz::require_mutate(&caller, &job)?;
                let mut transition = transition_for(&action, &job, now).map_err(|e| {
                    Failure::invalid_transition(e, lifecycle_action(&action), &job.state)
                })?;
                if transition.effects.iter().any(targets_live_process)
                    && core.launch_in_flight(id)
                {
                    return Ok(None);
                }
                let immediate_copy = matches!(&action, JobAction::Restart(o) if o.immediate && !o.in_place && !o.stashed);
                let launch = launches(&transition.effects);
                // A launch waits for the helper instead of failing.
                let launch_pending = !helper_ready && (launch || immediate_copy);
                if launch || immediate_copy {
                    core.require_gpus_available(&job.spec.gpus)?;
                }
                let force = Force::of(&caller);
                let mut waiting = None;
                if launch
                    && let LaunchPlan::Queue(wait) =
                        core.launch_plan(tx, &job, force, helper_ready, now)?
                {
                    transition = lifecycle::queue_for_launch(&job, now)?;
                    waiting = wait;
                }
                if !helper_ready && transition.effects.iter().any(needs_helper) {
                    return Err(Core::helper_unavailable());
                }
                if action == JobAction::Remove {
                    let all = tx.list_jobs(&JobFilter::all())?;
                    let mut together = removed_together;
                    together.push(id);
                    let blocking = lifecycle::blocking_dependants(&all, id, &together);
                    if !blocking.is_empty() {
                        return Err(Failure::new(
                            ErrorCode::InvalidState,
                            format!(
                                "job {id} cannot be removed: jobs {} depend on it",
                                list_ids(&blocking)
                            ),
                        ));
                    }
                }
                let snapshot = job.clone();
                let copy = core.apply_in_tx(tx, &mut job, &transition, now, ch)?;
                if launch {
                    // Also when launching now: should the helper not start
                    // it after all (it is stopping), it is retried the same
                    // way.
                    core.force_launch(id, force);
                    if let Some(wait) = &waiting {
                        core.set_launch_wait(id, wait.clone());
                    }
                }
                let copy = match copy {
                    Some(mut copy) if immediate_copy => {
                        core.force_launch(copy.id, force);
                        match core.launch_plan(tx, &copy, force, helper_ready, now)? {
                            LaunchPlan::Now => {
                                let start = lifecycle::start(&copy)?;
                                core.apply_in_tx(tx, &mut copy, &start, now, ch)?;
                                Some((copy, start.effects))
                            }
                            LaunchPlan::Queue(wait) => {
                                if let Some(wait) = &wait {
                                    core.set_launch_wait(copy.id, wait.clone());
                                }
                                waiting = wait;
                                Some((copy, Vec::new()))
                            }
                        }
                    }
                    Some(copy) => Some((copy, Vec::new())),
                    None => None,
                };
                let launch = LaunchState {
                    pending: launch_pending,
                    waiting,
                };
                if action == JobAction::Remove {
                    tx.remove_job(id)?;
                    ch.removed(id);
                    return Ok(Some((snapshot, transition.effects, copy, launch)));
                }
                Ok(Some((job, transition.effects, copy, launch)))
            })
            .await
    }

    /// `POST /v1/jobs/bulk`.
    pub async fn bulk(
        self: &Arc<Self>,
        caller: Caller,
        request: BulkJobRequest,
    ) -> ApiResult<BulkJobResponse> {
        self.reject_if_shutting_down()?;
        let action = request.action;
        let scope = authz::selection_scope(&caller);
        // Pausing/resuming whole groups (`-g`, or `-a` for all groups) is
        // admin-only; others only act on their own jobs in them. As in
        // pueue, `kill` pauses the groups too.
        let paused = match &action {
            JobAction::Pause(_) | JobAction::Kill(_) => Some(true),
            JobAction::Start => Some(false),
            _ => None,
        };
        let set_paused = paused.filter(|_| caller.is_admin);
        // `pause --wait`: only the groups are paused; running jobs may finish.
        let waiting_pause = matches!(&action, JobAction::Pause(o) if o.wait);
        let (ids, explicit, groups) = match request.target {
            JobSelector::Ids { ids } => (ids, true, Vec::new()),
            JobSelector::Group { group } => {
                let (jobs, groups) = self
                    .write(move |tx, ch| {
                        if tx.get_group(&group)?.is_none() {
                            return Err(puzuz_core::Error::GroupNotFound(group).into());
                        }
                        let mut groups = Vec::new();
                        if let Some(paused) = set_paused {
                            tx.set_group_paused(&group, paused)?;
                            ch.group(&group);
                            groups.push(group.clone());
                        }
                        Ok((tx.list_jobs(&JobFilter::all().group(&group))?, groups))
                    })
                    .await?;
                (
                    bulk_targets(&jobs, &action, &scope, waiting_pause),
                    false,
                    groups,
                )
            }
            JobSelector::All => {
                let (jobs, groups) = self
                    .write(move |tx, ch| {
                        let mut groups = Vec::new();
                        if let Some(paused) = set_paused {
                            for group in tx.list_groups()? {
                                tx.set_group_paused(&group.name, paused)?;
                                ch.group(&group.name);
                                groups.push(group.name);
                            }
                        }
                        Ok((tx.list_jobs(&JobFilter::all())?, groups))
                    })
                    .await?;
                (
                    bulk_targets(&jobs, &action, &scope, waiting_pause),
                    false,
                    groups,
                )
            }
        };
        let together = if action == JobAction::Remove {
            ids.clone()
        } else {
            Vec::new()
        };
        let mut results = Vec::new();
        for id in ids {
            // Orphans are only killed when named.
            let result = if explicit {
                self.job_action_by_id(caller, id, action.clone(), together.clone())
                    .await
            } else {
                self.job_action(caller, id, action.clone(), together.clone())
                    .await
            };
            match result {
                Ok(done) => results.push(JobActionResult {
                    id,
                    outcome: done.outcome(),
                }),
                // Group/all selections only touch jobs the action applies to.
                Err(e) if !explicit && e.code() == ErrorCode::InvalidState => {}
                Err(e) => results.push(JobActionResult::error(id, e.0)),
            }
        }
        Ok(BulkJobResponse { results, groups })
    }

    /// `POST /v1/jobs/clean`.
    pub async fn clean(
        self: &Arc<Self>,
        caller: Caller,
        request: CleanRequest,
    ) -> ApiResult<CleanResponse> {
        self.reject_if_shutting_down()?;
        let scope = authz::selection_scope(&caller);
        let core = self.clone();
        let now = now_ms();
        let removed = self
            .write(move |tx, ch| {
                if let Some(group) = &request.group
                    && tx.get_group(group)?.is_none()
                {
                    return Err(puzuz_core::Error::GroupNotFound(group.clone()).into());
                }
                let all = tx.list_jobs(&JobFilter::all())?;
                let candidates: Vec<&Job> = all
                    .iter()
                    .filter(|j| scope.includes(j))
                    .filter(|j| request.group.as_ref().is_none_or(|g| *g == j.spec.group))
                    .filter(|j| lifecycle::is_cleanable(j, request.successful_only))
                    .collect();
                let together: Vec<JobId> = candidates.iter().map(|j| j.id).collect();
                let mut removed = Vec::new();
                for job in candidates {
                    if !lifecycle::blocking_dependants(&all, job.id, &together).is_empty() {
                        continue;
                    }
                    let t = lifecycle::clean(job, request.successful_only)?;
                    let mut current = job.clone();
                    core.apply_in_tx(tx, &mut current, &t, now, ch)?;
                    tx.remove_job(job.id)?;
                    ch.removed(job.id);
                    removed.push((job.clone(), t.effects));
                }
                Ok(removed)
            })
            .await?;
        for (job, effects) in &removed {
            let _ = self.run_effects(job, effects).await;
        }
        Ok(CleanResponse {
            removed: removed.into_iter().map(|(job, _)| job.id).collect(),
        })
    }

    /// `POST /v1/jobs/reset`: kill and remove every job in scope. Running
    /// jobs are removed once they have stopped.
    pub async fn reset(
        self: &Arc<Self>,
        caller: Caller,
        request: ResetRequest,
    ) -> ApiResult<BulkJobResponse> {
        self.reject_if_shutting_down()?;
        let scope = authz::reset_scope(&caller);
        let core = self.clone();
        let now = now_ms();
        let (removed, live) = self
            .write(move |tx, ch| {
                if let Some(group) = &request.group
                    && tx.get_group(group)?.is_none()
                {
                    return Err(puzuz_core::Error::GroupNotFound(group.clone()).into());
                }
                let jobs: Vec<Job> = tx
                    .list_jobs(&JobFilter::all())?
                    .into_iter()
                    .filter(|j| scope.includes(j))
                    .filter(|j| request.group.as_ref().is_none_or(|g| *g == j.spec.group))
                    .collect();
                let mut removed = Vec::new();
                let mut live = Vec::new();
                for job in jobs {
                    match lifecycle::remove(&job) {
                        Ok(t) => {
                            let mut current = job.clone();
                            core.apply_in_tx(tx, &mut current, &t, now, ch)?;
                            tx.remove_job(job.id)?;
                            ch.removed(job.id);
                            removed.push((job, t.effects));
                        }
                        Err(_) => live.push(job),
                    }
                }
                Ok((removed, live))
            })
            .await?;
        let mut results = Vec::new();
        for (job, effects) in &removed {
            let _ = self.run_effects(job, effects).await;
            results.push(JobActionResult::ok(job.id));
        }
        for job in live {
            self.mark_for_removal(job.id);
            let killable = matches!(job.state, JobState::Running | JobState::Paused);
            let result = if killable {
                self.job_action(
                    caller,
                    job.id,
                    JobAction::Kill(KillOptions::default()),
                    Vec::new(),
                )
                .await
            } else {
                Ok(ActionDone::default())
            };
            match result {
                Ok(_) => results.push(JobActionResult::ok(job.id)),
                Err(e) => results.push(JobActionResult::error(job.id, e.0)),
            }
            // It may already have stopped.
            self.reap_pending(job.id).await;
        }
        results.sort_by_key(|r| r.id);
        Ok(BulkJobResponse::new(results))
    }

    /// `POST /v1/jobs/switch`.
    pub async fn switch(self: &Arc<Self>, caller: Caller, request: SwitchRequest) -> ApiResult<()> {
        self.reject_if_shutting_down()?;
        let SwitchRequest { first, second } = request;
        let core = self.clone();
        let swapped_flag = Arc::new(AtomicBool::new(false));
        let swapped = swapped_flag.clone();
        let result = self
            .write(move |tx, ch| {
                let a = tx.get_job(first)?.ok_or_else(|| job_not_found(first))?;
                let b = tx.get_job(second)?.ok_or_else(|| job_not_found(second))?;
                authz::require_mutate(&caller, &a)?;
                authz::require_mutate(&caller, &b)?;
                lifecycle::check_switch(&a, &b)?;
                // A queued job whose earlier launch (put back in line) has not
                // returned yet: its request settles under the old id.
                if let Some(id) = [first, second]
                    .into_iter()
                    .find(|id| core.launch_in_flight(*id))
                {
                    return Err(Failure::new(
                        ErrorCode::InvalidState,
                        format!("job {id} is being launched; try again in a moment"),
                    ));
                }
                tx.swap_job_ids(first, second)?;
                // Under the database lock, so the scheduler sees both swapped.
                // Should the commit fail, the swap is undone below.
                core.swap_in_memory(first, second);
                swapped.store(true, Ordering::SeqCst);
                ch.updated(first);
                ch.updated(second);
                Ok(())
            })
            .await;
        if result.is_err() && swapped_flag.load(Ordering::SeqCst) {
            // The commit failed: the ids were not swapped after all.
            self.swap_in_memory(first, second);
        }
        result
    }

    // -----------------------------------------------------------------
    // Groups
    // -----------------------------------------------------------------

    pub async fn list_groups(&self) -> ApiResult<Vec<Group>> {
        self.read(|s| s.list_groups()).await
    }

    pub async fn create_group(
        self: &Arc<Self>,
        caller: Caller,
        request: CreateGroupRequest,
    ) -> ApiResult<Group> {
        authz::require_admin_for(&caller, AdminOperation::CreateGroup)?;
        let name = request.name;
        if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(Failure::bad_request(format!("invalid group name {name:?}")));
        }
        let parallel = request.parallel.unwrap_or(1);
        self.write(move |tx, ch| {
            let group = tx.create_group(&name, parallel)?;
            ch.group(&name);
            Ok(group)
        })
        .await
    }

    pub async fn delete_group(self: &Arc<Self>, caller: Caller, name: String) -> ApiResult<()> {
        authz::require_admin_for(&caller, AdminOperation::RemoveGroup)?;
        self.write(move |tx, ch| {
            tx.remove_group(&name)?;
            ch.group_removed(&name);
            Ok(())
        })
        .await
    }

    pub async fn set_group_parallel(
        self: &Arc<Self>,
        caller: Caller,
        name: String,
        parallel: u32,
    ) -> ApiResult<Group> {
        authz::require_admin_for(&caller, AdminOperation::SetGroupParallel)?;
        self.write(move |tx, ch| {
            tx.set_group_parallel(&name, parallel)?;
            ch.group(&name);
            tx.get_group(&name)?
                .ok_or_else(|| puzuz_core::Error::GroupNotFound(name).into())
        })
        .await
    }

    // -----------------------------------------------------------------
    // GPUs
    // -----------------------------------------------------------------

    /// How a launching action (`start` of a queued or stashed job, `add
    /// --immediate`, `restore --immediate`, `restart --immediate`) with
    /// `force` launches `job`: right away, or waiting in line while the
    /// helper is not connected or, for a non-administrator, while one of
    /// its GPUs has no free slot or its group is paused (decided like the
    /// scheduler would, with the job forced: see
    /// [`scheduler::Reserved::forced`]).
    fn launch_plan(
        &self,
        tx: &Tx<'_>,
        job: &Job,
        force: Force,
        helper_ready: bool,
        now: Timestamp,
    ) -> ApiResult<LaunchPlan> {
        if !helper_ready {
            return Ok(LaunchPlan::Queue(None));
        }
        if force == Force::IgnoreLimits {
            return Ok(LaunchPlan::Now);
        }
        let mut queued = job.clone();
        lifecycle::queue_for_launch(job, now)?.apply(&mut queued, now);
        let mut jobs = tx.list_unfinished_jobs()?;
        jobs.retain(|j| j.id != job.id);
        jobs.push(queued);
        let groups = tx.list_groups()?;
        let gpus: Vec<Gpu> = tx
            .list_gpus()?
            .into_iter()
            .filter(|gpu| self.gpu_available(gpu.index))
            .collect();
        let mut reserved = self.orphan_reservations();
        reserved.forced = self.queue_jumping_launches();
        reserved.forced.insert(job.id);
        let plan = scheduler::plan_with(now, &jobs, &groups, &gpus, &reserved);
        if plan.start.contains(&job.id) || plan.restore.contains(&job.id) {
            return Ok(LaunchPlan::Now);
        }
        let wait = plan
            .waiting
            .into_iter()
            .find(|(id, _)| *id == job.id)
            .map_or(LaunchWait::Unavailable, |(_, wait)| wait);
        Ok(LaunchPlan::Queue(Some(wait)))
    }

    /// Rejects launching on (or submitting for) a GPU that was not detected
    /// at start-up. Jobs already queued for one simply wait.
    fn require_gpus_available(&self, gpus: &[GpuIndex]) -> ApiResult<()> {
        match gpus.iter().find(|gpu| !self.gpu_available(**gpu)) {
            Some(gpu) => Err(Failure::new(
                ErrorCode::InvalidState,
                format!(
                    "GPU {gpu} is not available: it was not detected when the daemon started \
                     (see `puzuz gpu`)"
                ),
            )),
            None => Ok(()),
        }
    }

    fn gpu_views(&self, gpus: Vec<Gpu>, jobs: &[Job]) -> Vec<GpuView> {
        // Orphans take slots too (listed by `status` for administrators).
        let orphan_slots = self.orphan_reservations().gpus;
        gpus.into_iter()
            .map(|gpu| {
                let holders: Vec<GpuJob> = jobs
                    .iter()
                    .filter(|j| j.state.occupies_slot() && j.spec.gpus.contains(&gpu.index))
                    .map(|j| GpuJob {
                        id: j.id,
                        owner: j.owner,
                    })
                    .collect();
                let detected = self.detected_gpus.get(&gpu.index);
                GpuView {
                    index: gpu.index,
                    memory_total_bytes: detected.and_then(|d| d.memory_total_bytes),
                    driver_version: detected.and_then(|d| d.driver_version.clone()),
                    available: detected.is_some(),
                    uuid: gpu.uuid,
                    name: gpu.name,
                    parallel: gpu.parallel,
                    used: holders.len() as u32 + orphan_slots.get(&gpu.index).copied().unwrap_or(0),
                    jobs: holders,
                }
            })
            .collect()
    }

    pub async fn list_gpus(&self) -> ApiResult<Vec<GpuView>> {
        let (gpus, jobs) = self
            .read(|s| Ok((s.list_gpus()?, s.list_unfinished_jobs()?)))
            .await?;
        Ok(self.gpu_views(gpus, &jobs))
    }

    pub async fn set_gpus_parallel(
        self: &Arc<Self>,
        caller: Caller,
        request: SetGpusParallelRequest,
    ) -> ApiResult<Vec<GpuView>> {
        authz::require_admin_for(&caller, AdminOperation::SetGpuParallel)?;
        let SetGpusParallelRequest { gpus, parallel } = request;
        let (all, jobs) = self
            .write(move |tx, ch| {
                let targets: Vec<GpuIndex> = match gpus {
                    Some(list) => list,
                    None => tx.list_gpus()?.into_iter().map(|g| g.index).collect(),
                };
                for index in &targets {
                    tx.set_gpu_parallel(*index, parallel)?;
                }
                ch.touch();
                Ok((tx.list_gpus()?, tx.list_unfinished_jobs()?))
            })
            .await?;
        Ok(self.gpu_views(all, &jobs))
    }
}
