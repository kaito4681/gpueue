//! The operations behind the API endpoints.

use std::collections::HashSet;
use std::sync::Arc;

use puzuz_core::authz::{self, AdminOperation};
use puzuz_core::lifecycle::{self, Action, Effect, Transition};
use puzuz_core::model::{Caller, Gpu, GpuIndex, Group, Job, JobId, JobState, Timestamp};
use puzuz_core::storage::{JobFilter, StoreOps};
use puzuz_protocol::api::{
    AddJobRequest, BulkJobRequest, BulkJobResponse, CleanRequest, CleanResponse,
    CreateGroupRequest, ErrorCode, GpuJob, GpuView, JobAction, JobActionResult, JobSelector,
    JobView, KillOptions, ResetRequest, SetGpusParallelRequest, SwitchRequest,
};

use crate::error::{ApiResult, Failure};
use crate::state::{Core, JobInfo, needs_helper, now_ms};

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

fn job_not_found(id: JobId) -> Failure {
    puzuz_core::Error::JobNotFound(id).into()
}

fn list_ids(ids: &[JobId]) -> String {
    ids.iter()
        .map(JobId::to_string)
        .collect::<Vec<_>>()
        .join(", ")
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
                s.list_jobs(&filter)?
                    .into_iter()
                    .map(|job| JobInfo::load(s, job))
                    .collect::<Result<Vec<_>, _>>()
            })
            .await?;
        Ok(infos.iter().map(|info| info.view(&caller)).collect())
    }

    pub async fn job_info(&self, id: JobId) -> ApiResult<JobInfo> {
        self.read(move |s| match s.get_job(id)? {
            Some(job) => JobInfo::load(s, job).map(Some),
            None => Ok(None),
        })
        .await?
        .ok_or_else(|| job_not_found(id))
    }

    // -----------------------------------------------------------------
    // Jobs: add
    // -----------------------------------------------------------------

    pub async fn add_job(
        self: &Arc<Self>,
        caller: Caller,
        request: AddJobRequest,
    ) -> ApiResult<JobId> {
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
        if request.immediate && !self.helper_ready() {
            return Err(Core::helper_unavailable());
        }
        let now = now_ms();
        let core = self.clone();
        let (job, effects) = self
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
                for dep in &spec.dependencies {
                    if tx.get_job(*dep)?.is_none() {
                        return Err(Failure::bad_request(format!(
                            "dependency {dep} does not exist"
                        )));
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
                if request.immediate {
                    let t = lifecycle::start(&job)?;
                    core.apply_in_tx(tx, &mut job, &t, now, ch)?;
                    effects = t.effects;
                }
                Ok((job, effects))
            })
            .await?;
        if let Err(e) = self.run_effects(&job, &effects).await {
            tracing::warn!(job = %job.id, "cannot start job immediately: {}", e.0.message);
        }
        Ok(job.id)
    }

    // -----------------------------------------------------------------
    // Jobs: actions
    // -----------------------------------------------------------------

    /// Applies `action` to one job. `removed_together` lists jobs removed in
    /// the same request (they do not block each other's removal). Returns
    /// the id of a job created by `restart`.
    pub async fn job_action(
        self: &Arc<Self>,
        caller: Caller,
        id: JobId,
        action: JobAction,
        removed_together: Vec<JobId>,
    ) -> ApiResult<Option<JobId>> {
        self.reject_if_shutting_down()?;
        let helper_ready = self.helper_ready();
        let now = now_ms();
        let core = self.clone();
        type Launch = (Job, Vec<Effect>);
        let (job, effects, copy): (Job, Vec<Effect>, Option<Launch>) = self
            .write(move |tx, ch| {
                let mut job = tx.get_job(id)?.ok_or_else(|| job_not_found(id))?;
                authz::require_mutate(&caller, &job)?;
                let transition = transition_for(&action, &job, now).map_err(|e| {
                    Failure::invalid_transition(e, lifecycle_action(&action), &job.state)
                })?;
                let immediate_copy = matches!(&action, JobAction::Restart(o) if o.immediate && !o.in_place && !o.stashed);
                if !helper_ready
                    && (transition.effects.iter().any(needs_helper) || immediate_copy)
                {
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
                let copy = match copy {
                    Some(mut copy) if immediate_copy => {
                        let start = lifecycle::start(&copy)?;
                        core.apply_in_tx(tx, &mut copy, &start, now, ch)?;
                        Some((copy, start.effects))
                    }
                    Some(copy) => Some((copy, Vec::new())),
                    None => None,
                };
                if action == JobAction::Remove {
                    tx.remove_job(id)?;
                    ch.removed(id);
                    return Ok((snapshot, transition.effects, copy));
                }
                Ok((job, transition.effects, copy))
            })
            .await?;
        self.run_effects(&job, &effects).await?;
        if let Some((copy, effects)) = &copy {
            self.run_effects(copy, effects).await?;
        }
        Ok(copy.map(|(copy, _)| copy.id))
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
        let (ids, explicit) = match request.target {
            JobSelector::Ids { ids } => (ids, true),
            JobSelector::Group { group } => {
                // Pausing/resuming a whole group is admin-only; others only
                // act on their own jobs in it.
                let paused = match &action {
                    JobAction::Pause(_) | JobAction::Kill(_) => Some(true),
                    JobAction::Start => Some(false),
                    _ => None,
                };
                let set_status = caller.is_admin && paused.is_some();
                let name = group.clone();
                let jobs = self
                    .write(move |tx, ch| {
                        if tx.get_group(&name)?.is_none() {
                            return Err(puzuz_core::Error::GroupNotFound(name).into());
                        }
                        if set_status && let Some(paused) = paused {
                            tx.set_group_paused(&name, paused)?;
                            ch.group(&name);
                        }
                        Ok(tx.list_jobs(&JobFilter::all().group(&name))?)
                    })
                    .await?;
                let waiting_pause = matches!(&action, JobAction::Pause(o) if o.wait);
                let ids = if waiting_pause {
                    // `pause --wait`: running jobs may finish.
                    Vec::new()
                } else {
                    jobs.iter()
                        .filter(|j| scope.includes(j))
                        .filter(|j| action != JobAction::Start || j.state == JobState::Paused)
                        .map(|j| j.id)
                        .collect()
                };
                (ids, false)
            }
            JobSelector::All => {
                let jobs = self.read(|s| s.list_jobs(&JobFilter::all())).await?;
                let ids = jobs
                    .iter()
                    .filter(|j| scope.includes(j))
                    .filter(|j| action != JobAction::Start || j.state == JobState::Paused)
                    .map(|j| j.id)
                    .collect();
                (ids, false)
            }
        };
        let together = if action == JobAction::Remove {
            ids.clone()
        } else {
            Vec::new()
        };
        let mut results = Vec::new();
        for id in ids {
            match self
                .job_action(caller, id, action.clone(), together.clone())
                .await
            {
                Ok(new_id) => results.push(JobActionResult {
                    id,
                    outcome: puzuz_protocol::api::ActionOutcome::Ok { new_id },
                }),
                // Group/all selections only touch jobs the action applies to.
                Err(e) if !explicit && e.code() == ErrorCode::InvalidState => {}
                Err(e) => results.push(JobActionResult::error(id, e.0)),
            }
        }
        Ok(BulkJobResponse { results })
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
                Ok(None)
            };
            match result {
                Ok(_) => results.push(JobActionResult::ok(job.id)),
                Err(e) => results.push(JobActionResult::error(job.id, e.0)),
            }
            // It may already have stopped.
            self.reap_pending(job.id).await;
        }
        results.sort_by_key(|r| r.id);
        Ok(BulkJobResponse { results })
    }

    /// `POST /v1/jobs/switch`.
    pub async fn switch(self: &Arc<Self>, caller: Caller, request: SwitchRequest) -> ApiResult<()> {
        self.reject_if_shutting_down()?;
        let SwitchRequest { first, second } = request;
        self.write(move |tx, ch| {
            let a = tx.get_job(first)?.ok_or_else(|| job_not_found(first))?;
            let b = tx.get_job(second)?.ok_or_else(|| job_not_found(second))?;
            authz::require_mutate(&caller, &a)?;
            authz::require_mutate(&caller, &b)?;
            lifecycle::check_switch(&a, &b)?;
            tx.swap_job_ids(first, second)?;
            ch.updated(first);
            ch.updated(second);
            Ok(())
        })
        .await
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

    fn gpu_views(&self, gpus: Vec<Gpu>, jobs: &[Job]) -> Vec<GpuView> {
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
                GpuView {
                    index: gpu.index,
                    memory_total_bytes: self.gpu_memory.get(&gpu.index).copied(),
                    uuid: gpu.uuid,
                    name: gpu.name,
                    parallel: gpu.parallel,
                    used: holders.len() as u32,
                    jobs: holders,
                }
            })
            .collect()
    }

    pub async fn list_gpus(&self) -> ApiResult<Vec<GpuView>> {
        let (gpus, jobs) = self
            .read(|s| Ok((s.list_gpus()?, s.list_jobs(&JobFilter::all())?)))
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
                Ok((tx.list_gpus()?, tx.list_jobs(&JobFilter::all())?))
            })
            .await?;
        Ok(self.gpu_views(all, &jobs))
    }
}
