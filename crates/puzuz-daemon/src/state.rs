//! The daemon's runtime state and the machinery shared by the API, the
//! scheduler and the helper event loop.
//!
//! Every state change follows the same pattern:
//! 1. [`Core::write`] runs a closure in one SQLite transaction. The closure
//!    re-reads the job, asks `puzuz_core::lifecycle` for a [`Transition`],
//!    and persists it with [`Core::apply_in_tx`] (which also performs the
//!    effects that only touch the database).
//! 2. Still under the database lock, the changed jobs are published to
//!    `/v1/events` subscribers, so events are delivered in commit order.
//! 3. The remaining effects (helper calls, log files) run afterwards with
//!    [`Core::run_effects`]; their failures may lead to further transitions.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::FutureExt;
use futures::future::BoxFuture;
use puzuz_core::lifecycle::{self, Effect, ShutdownAction, Transition};
use puzuz_core::model::{Caller, GpuIndex, Group, Job, JobId, JobResult, JobState, Timestamp, Uid};
use puzuz_core::scheduler;
use puzuz_core::storage::StorageError;
use puzuz_core::storage::{CheckpointRecord, CheckpointStatus, JobFilter, Store, StoreOps, Tx};
use puzuz_protocol::api::{self, ErrorCode, JobView};
use puzuz_protocol::helper::{
    self as hp, CheckpointOptions, CheckpointOutcome, HelperErrorCode, RestoreOutcome,
    RestoreRequest, RunningJob, SpawnRequest, signal,
};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::{ApiResult, Failure};
use crate::helper::{Helper, HelperError, HelperNotice};
use crate::users::UserDb;

/// Current time in Unix epoch milliseconds.
pub fn now_ms() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as Timestamp)
        .unwrap_or(0)
}

/// The single SQLite connection, used from blocking threads.
#[derive(Clone)]
pub struct Db(Arc<Mutex<Store>>);

impl Db {
    pub fn new(store: Store) -> Self {
        Self(Arc::new(Mutex::new(store)))
    }

    /// Runs `f` with exclusive access to the store on a blocking thread.
    pub async fn call<R, F>(&self, f: F) -> R
    where
        F: FnOnce(&mut Store) -> R + Send + 'static,
        R: Send + 'static,
    {
        let db = self.0.clone();
        let task = tokio::task::spawn_blocking(move || {
            let mut store = db.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut store)
        });
        match task.await {
            Ok(value) => value,
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
}

/// A job plus the data needed to render its views.
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub job: Job,
    pub checkpoint: Option<CheckpointRecord>,
}

impl JobInfo {
    pub fn load(store: &impl StoreOps, job: Job) -> Result<Self, StorageError> {
        let checkpoint = store.get_checkpoint(job.id)?;
        Ok(Self { job, checkpoint })
    }

    /// The view `caller` may see, with checkpoint information filled in.
    pub fn view(&self, caller: &Caller) -> JobView {
        let mut view = JobView::for_caller(&self.job, caller);
        if let Some(record) = &self.checkpoint {
            if record.status == CheckpointStatus::Complete {
                view.checkpoint_size_bytes = record.size_bytes;
            }
            if let Some(details) = view.details.as_mut() {
                details.last_error = record.failure_reason.clone();
            }
        }
        view
    }
}

/// Internal change notifications; turned into per-caller
/// [`api::Event`]s by the SSE endpoint.
#[derive(Debug, Clone)]
pub enum DaemonEvent {
    JobAdded(Arc<JobInfo>),
    JobUpdated(Arc<JobInfo>),
    JobRemoved(JobId),
    GroupUpdated(Group),
    GroupRemoved(String),
    Shutdown,
}

impl DaemonEvent {
    pub fn to_api(&self, caller: &Caller) -> api::Event {
        match self {
            DaemonEvent::JobAdded(info) => api::Event::JobAdded {
                job: info.view(caller),
            },
            DaemonEvent::JobUpdated(info) => api::Event::JobUpdated {
                job: info.view(caller),
            },
            DaemonEvent::JobRemoved(id) => api::Event::JobRemoved { id: *id },
            DaemonEvent::GroupUpdated(group) => api::Event::GroupUpdated {
                group: group.clone(),
            },
            DaemonEvent::GroupRemoved(name) => api::Event::GroupRemoved { name: name.clone() },
            DaemonEvent::Shutdown => api::Event::DaemonShutdown,
        }
    }
}

/// What a transaction changed, for publishing and waking the scheduler.
#[derive(Debug, Default)]
pub struct Changes {
    added: Vec<JobId>,
    updated: Vec<JobId>,
    removed: Vec<JobId>,
    groups: Vec<String>,
    groups_removed: Vec<String>,
    other: bool,
}

impl Changes {
    pub fn added(&mut self, id: JobId) {
        self.added.push(id);
    }
    pub fn updated(&mut self, id: JobId) {
        self.updated.push(id);
    }
    pub fn removed(&mut self, id: JobId) {
        self.removed.push(id);
    }
    pub fn group(&mut self, name: &str) {
        self.groups.push(name.to_owned());
    }
    pub fn group_removed(&mut self, name: &str) {
        self.groups_removed.push(name.to_owned());
    }
    /// Something else that may affect scheduling changed (e.g. GPU limits).
    pub fn touch(&mut self) {
        self.other = true;
    }
    fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.updated.is_empty()
            && self.removed.is_empty()
            && self.groups.is_empty()
            && self.groups_removed.is_empty()
            && !self.other
    }
}

/// Whether an effect needs the helper.
pub fn needs_helper(effect: &Effect) -> bool {
    matches!(
        effect,
        Effect::Spawn
            | Effect::SignalStop
            | Effect::SignalContinue
            | Effect::Kill
            | Effect::SendSignal(_)
            | Effect::Terminate
            | Effect::Checkpoint
            | Effect::Restore
            | Effect::DeleteCheckpointImage
    )
}

/// A pre-transition hook for [`Core::handle_event`] that does nothing.
pub fn no_pre(_: &dyn StoreOps, _: &Job) -> Result<(), StorageError> {
    Ok(())
}

pub struct Core {
    pub config: Config,
    pub db: Db,
    pub helper: Arc<dyn Helper>,
    pub users: Arc<dyn UserDb>,
    /// Memory sizes reported by GPU detection.
    pub gpu_memory: HashMap<GpuIndex, u64>,
    /// Testing only: let the `x-puzuz-test-uid` header override the peer
    /// uid. Never settable from the configuration file.
    pub trust_uid_header: bool,
    events: broadcast::Sender<DaemonEvent>,
    changes: watch::Sender<u64>,
    schedule: Notify,
    shutting_down: AtomicBool,
    /// Set once the current helper connection has been reconciled.
    reconciled: AtomicBool,
    /// Stops every task (daemon exit).
    pub cancel: CancellationToken,
    /// Fired when `puzuz shutdown` has finished.
    pub shutdown_complete: CancellationToken,
    /// Jobs `reset` will remove once they have stopped.
    pending_removal: Mutex<BTreeSet<JobId>>,
    /// Live helper jobs the database does not account for.
    orphans: Mutex<BTreeSet<JobId>>,
}

impl Core {
    pub fn new(
        config: Config,
        store: Store,
        helper: Arc<dyn Helper>,
        users: Arc<dyn UserDb>,
        gpu_memory: HashMap<GpuIndex, u64>,
        trust_uid_header: bool,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        let (changes, _) = watch::channel(0);
        Arc::new(Self {
            config,
            db: Db::new(store),
            helper,
            users,
            gpu_memory,
            trust_uid_header,
            events,
            changes,
            schedule: Notify::new(),
            shutting_down: AtomicBool::new(false),
            reconciled: AtomicBool::new(false),
            cancel: CancellationToken::new(),
            shutdown_complete: CancellationToken::new(),
            pending_removal: Mutex::new(BTreeSet::new()),
            orphans: Mutex::new(BTreeSet::new()),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.events.subscribe()
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub fn reject_if_shutting_down(&self) -> ApiResult<()> {
        if self.is_shutting_down() {
            Err(Failure::new(
                ErrorCode::ShuttingDown,
                "the daemon is shutting down and accepts no new work",
            ))
        } else {
            Ok(())
        }
    }

    /// Whether jobs can be launched and signalled right now.
    pub fn helper_ready(&self) -> bool {
        self.reconciled.load(Ordering::SeqCst) && self.helper.state().connected
    }

    pub fn helper_unavailable() -> Failure {
        Failure::new(
            ErrorCode::HelperUnavailable,
            "the puzuz helper is not connected; check `systemctl status puzuz-helper`",
        )
    }

    pub fn orphans(&self) -> Vec<JobId> {
        self.orphans.lock().unwrap().iter().copied().collect()
    }

    pub fn log_path(&self, owner: Uid, id: JobId) -> PathBuf {
        self.config
            .logs_dir()
            .join(owner.0.to_string())
            .join(format!("{id}.log"))
    }

    pub fn image_dir(&self, id: JobId) -> PathBuf {
        self.config.checkpoints_dir().join(id.to_string())
    }

    pub fn wake_scheduler(&self) {
        self.schedule.notify_one();
    }

    // -----------------------------------------------------------------
    // Transactions and publishing
    // -----------------------------------------------------------------

    /// Runs `f` in one transaction, then publishes what it changed.
    pub async fn write<R, F>(self: &Arc<Self>, f: F) -> ApiResult<R>
    where
        F: FnOnce(&Tx<'_>, &mut Changes) -> ApiResult<R> + Send + 'static,
        R: Send + 'static,
    {
        let core = self.clone();
        let (result, changed) = self
            .db
            .call(move |store| {
                let mut changes = Changes::default();
                let result = store.transaction(|tx| f(tx, &mut changes));
                let changed = result.is_ok() && !changes.is_empty();
                if changed {
                    core.publish(store, &changes);
                }
                (result, changed)
            })
            .await;
        if changed {
            self.changes.send_modify(|n| *n += 1);
            self.schedule.notify_one();
        }
        result
    }

    /// Runs a read-only closure against the store.
    pub async fn read<R, F>(&self, f: F) -> ApiResult<R>
    where
        F: FnOnce(&Store) -> Result<R, StorageError> + Send + 'static,
        R: Send + 'static,
    {
        self.db
            .call(move |store| f(store))
            .await
            .map_err(Failure::from)
    }

    fn publish(&self, store: &Store, changes: &Changes) {
        let mut seen = HashSet::new();
        let removed: HashSet<JobId> = changes.removed.iter().copied().collect();
        let load = |id: JobId| match store.get_job(id) {
            Ok(Some(job)) => JobInfo::load(store, job).ok().map(Arc::new),
            _ => None,
        };
        for &id in &changes.added {
            if seen.insert(id)
                && !removed.contains(&id)
                && let Some(info) = load(id)
            {
                let _ = self.events.send(DaemonEvent::JobAdded(info));
            }
        }
        for &id in &changes.updated {
            if seen.insert(id)
                && !removed.contains(&id)
                && let Some(info) = load(id)
            {
                let _ = self.events.send(DaemonEvent::JobUpdated(info));
            }
        }
        for &id in &removed {
            let _ = self.events.send(DaemonEvent::JobRemoved(id));
        }
        let mut seen_groups = HashSet::new();
        for name in &changes.groups {
            if seen_groups.insert(name)
                && let Ok(Some(group)) = store.get_group(name)
            {
                let _ = self.events.send(DaemonEvent::GroupUpdated(group));
            }
        }
        for name in &changes.groups_removed {
            let _ = self.events.send(DaemonEvent::GroupRemoved(name.clone()));
        }
    }

    /// Persists `transition` for `job` (updating `job` in place) and
    /// performs its database-only effects. Returns the job created by
    /// [`Effect::CreateRestartedCopy`], if any.
    pub fn apply_in_tx(
        &self,
        tx: &Tx<'_>,
        job: &mut Job,
        transition: &Transition,
        now: Timestamp,
        changes: &mut Changes,
    ) -> ApiResult<Option<Job>> {
        if transition.next.is_some() {
            transition.apply(job, now);
            tx.update_job_state(job.id, &job.state, job.started_at, job.finished_at)?;
            changes.updated(job.id);
        }
        let mut copy = None;
        for effect in &transition.effects {
            match effect {
                Effect::Checkpoint => {
                    tx.put_checkpoint(&CheckpointRecord {
                        job_id: job.id,
                        path: self.image_dir(job.id),
                        size_bytes: None,
                        gpu_model: None,
                        driver_version: None,
                        created_at: now,
                        status: CheckpointStatus::InProgress,
                        failure_reason: None,
                    })?;
                    changes.updated(job.id);
                }
                Effect::RecordRestoreError(reason) => {
                    if let Some(record) = tx.get_checkpoint(job.id)? {
                        tx.set_checkpoint_status(
                            job.id,
                            record.status,
                            record.size_bytes,
                            Some(reason),
                        )?;
                    }
                    changes.updated(job.id);
                }
                Effect::DeleteCheckpointImage => {
                    // After a failed dump the record stays (marked failed,
                    // with the reason) so `status` can explain the failure.
                    if !matches!(job.state, JobState::Terminating { .. }) {
                        tx.delete_checkpoint(job.id)?;
                        changes.updated(job.id);
                    }
                }
                Effect::CreateRestartedCopy { stashed } => {
                    let state = if *stashed {
                        JobState::Stashed { enqueue_at: None }
                    } else {
                        JobState::Queued { enqueued_at: now }
                    };
                    let new = tx.insert_job(job.owner, &job.spec, &state, now)?;
                    changes.added(new.id);
                    copy = Some(new);
                }
                _ => {}
            }
        }
        Ok(copy)
    }

    // -----------------------------------------------------------------
    // Effects
    // -----------------------------------------------------------------

    /// Performs the effects that leave the database, in order. Returns the
    /// first failure; later effects are still attempted.
    pub fn run_effects<'a>(
        self: &'a Arc<Self>,
        job: &'a Job,
        effects: &'a [Effect],
    ) -> BoxFuture<'a, ApiResult<()>> {
        async move {
            let mut first_error = None;
            for effect in effects {
                if let Err(e) = self.run_effect(job, effect).await {
                    tracing::warn!(job = %job.id, ?effect, "effect failed: {}", e.0.message);
                    first_error.get_or_insert(e);
                }
            }
            first_error.map_or(Ok(()), Err)
        }
        .boxed()
    }

    async fn run_effect(self: &Arc<Self>, job: &Job, effect: &Effect) -> ApiResult<()> {
        match effect {
            Effect::Spawn => self.spawn_job(job).await,
            Effect::SignalStop => self.signal(job, signal::SIGSTOP).await,
            Effect::SignalContinue => self.signal(job, signal::SIGCONT).await,
            Effect::SendSignal(sig) => self.signal(job, *sig).await,
            Effect::Kill | Effect::Terminate => self.terminate(job).await,
            Effect::Checkpoint => self.start_checkpoint(job).await,
            Effect::Restore => self.start_restore(job).await,
            // After a successful restore the helper has already removed the
            // image (and refuses to touch the directory of a live job).
            Effect::DeleteCheckpointImage if job.state == JobState::Running => Ok(()),
            Effect::DeleteCheckpointImage => Ok(self
                .helper
                .delete_checkpoint(job.id, self.image_dir(job.id))
                .await?),
            Effect::DeleteLog => {
                // Logs live in a directory created by the helper; removing
                // them needs write access to it (see the crate docs).
                match tokio::fs::remove_file(self.log_path(job.owner, job.id)).await {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(Failure::internal(format!("cannot delete the log: {e}"))),
                }
            }
            Effect::ResetLog => {
                let path = self.log_path(job.owner, job.id);
                match tokio::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&path)
                    .await
                {
                    Ok(_) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(Failure::internal(format!("cannot reset the log: {e}"))),
                }
            }
            Effect::CreateRestartedCopy { .. } | Effect::RecordRestoreError(_) => Ok(()),
        }
    }

    async fn spawn_job(self: &Arc<Self>, job: &Job) -> ApiResult<()> {
        let users = self.users.clone();
        let owner = job.owner;
        let gid = tokio::task::spawn_blocking(move || users.primary_gid(owner))
            .await
            .ok()
            .flatten();
        let Some(gid) = gid else {
            self.handle_event(
                job.id,
                lifecycle::Event::SpawnFailed {
                    reason: format!("user {owner} does not exist"),
                },
                no_pre,
            )
            .await;
            return Ok(());
        };
        let request = SpawnRequest {
            job: job.id,
            uid: job.owner,
            gid,
            groups: None,
            cwd: job.spec.cwd.clone(),
            command: job.spec.command.clone(),
            shell: self.config.shell.clone(),
            envs: job.spec.envs.clone(),
            gpus: job.spec.gpus.clone(),
            log_path: self.log_path(job.owner, job.id),
            no_checkpoint: job.spec.no_checkpoint,
        };
        match self.helper.spawn(request).await {
            Ok(pid) => {
                tracing::info!(job = %job.id, pid, "job started");
                Ok(())
            }
            Err(HelperError::Rejected { message, .. }) => {
                self.handle_event(
                    job.id,
                    lifecycle::Event::SpawnFailed { reason: message },
                    no_pre,
                )
                .await;
                Ok(())
            }
            Err(HelperError::NotConnected) => {
                // Never reached the helper: put the job back in line.
                let id = job.id;
                self.write(move |tx, ch| {
                    if let Some(job) = tx.get_job(id)?
                        && job.state == JobState::Running
                    {
                        tx.update_job_state(
                            id,
                            &JobState::Queued {
                                enqueued_at: now_ms(),
                            },
                            None,
                            None,
                        )?;
                        ch.updated(id);
                    }
                    Ok(())
                })
                .await?;
                Err(HelperError::NotConnected.into())
            }
            // Unknown whether it started; reconciliation will tell.
            Err(e) => Err(e.into()),
        }
    }

    async fn signal(self: &Arc<Self>, job: &Job, sig: i32) -> ApiResult<()> {
        match self.helper.signal(job.id, sig).await {
            Ok(()) => Ok(()),
            Err(
                e @ HelperError::Rejected {
                    code: HelperErrorCode::JobNotFound,
                    ..
                },
            ) => {
                self.spawn_lost_check(job.id, job.started_at);
                Err(Failure::new(
                    ErrorCode::InvalidState,
                    format!("job {} is not running: {e}", job.id),
                ))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// SIGTERM now, SIGKILL after the grace period if the job is still
    /// there.
    pub async fn terminate(self: &Arc<Self>, job: &Job) -> ApiResult<()> {
        match self.helper.signal(job.id, signal::SIGTERM).await {
            Ok(()) => {}
            Err(HelperError::Rejected {
                code: HelperErrorCode::JobNotFound,
                ..
            }) => {
                self.spawn_lost_check(job.id, job.started_at);
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        }
        let core = self.clone();
        let id = job.id;
        let started_at = job.started_at;
        let grace = Duration::from_secs(self.config.grace_secs);
        tokio::spawn(async move {
            tokio::select! {
                _ = core.cancel.cancelled() => return,
                _ = tokio::time::sleep(grace) => {}
            }
            if core.still_live(id, started_at).await {
                tracing::info!(job = %id, "grace period over, sending SIGKILL");
                if let Err(HelperError::Rejected {
                    code: HelperErrorCode::JobNotFound,
                    ..
                }) = core.helper.signal(id, signal::SIGKILL).await
                {
                    core.spawn_lost_check(id, started_at);
                }
            }
        });
        Ok(())
    }

    /// Whether the job still holds its slots in the same run.
    async fn still_live(&self, id: JobId, started_at: Option<Timestamp>) -> bool {
        matches!(
            self.read(move |s| s.get_job(id)).await,
            Ok(Some(job)) if job.state.occupies_slot() && job.started_at == started_at
        )
    }

    /// The helper does not know the job: unless its exit event shows up
    /// shortly, record it as lost.
    fn spawn_lost_check(self: &Arc<Self>, id: JobId, started_at: Option<Timestamp>) {
        let core = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = core.cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            if !core.still_live(id, started_at).await {
                return;
            }
            if let Ok(running) = core.helper.list_running().await
                && !running.iter().any(|r| r.job == id)
            {
                tracing::warn!(job = %id, "the helper no longer knows the job; marking it lost");
                core.handle_event(id, lifecycle::Event::Lost, no_pre).await;
            }
        });
    }

    async fn start_checkpoint(self: &Arc<Self>, job: &Job) -> ApiResult<()> {
        let options = CheckpointOptions {
            tcp_close: self.config.checkpoint_tcp_close,
            ..Default::default()
        };
        match self
            .helper
            .checkpoint(job.id, self.image_dir(job.id), options)
            .await
        {
            Ok(()) => Ok(()),
            Err(
                e @ (HelperError::NotConnected
                | HelperError::Rejected { .. }
                | HelperError::Protocol(_)),
            ) => {
                // The dump never started; the job keeps running.
                let id = job.id;
                let reason = e.to_string();
                self.write(move |tx, ch| {
                    if let Some(job) = tx.get_job(id)?
                        && job.state == JobState::Checkpointing
                    {
                        tx.update_job_state(id, &JobState::Running, job.started_at, None)?;
                        if tx.get_checkpoint(id)?.is_some() {
                            tx.set_checkpoint_status(
                                id,
                                CheckpointStatus::Failed,
                                None,
                                Some(&reason),
                            )?;
                        }
                        ch.updated(id);
                    }
                    Ok(())
                })
                .await?;
                Err(e.into())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn start_restore(self: &Arc<Self>, job: &Job) -> ApiResult<()> {
        let request = RestoreRequest {
            job: job.id,
            uid: job.owner,
            image_dir: self.image_dir(job.id),
            gpus: job.spec.gpus.clone(),
        };
        match self.helper.restore(request).await {
            Ok(()) => Ok(()),
            Err(
                e @ (HelperError::NotConnected
                | HelperError::Rejected { .. }
                | HelperError::Protocol(_)),
            ) => {
                self.handle_event(
                    job.id,
                    lifecycle::Event::RestoreFailed {
                        reason: e.to_string(),
                    },
                    no_pre,
                )
                .await;
                Err(e.into())
            }
            Err(e) => Err(e.into()),
        }
    }

    // -----------------------------------------------------------------
    // Runtime events
    // -----------------------------------------------------------------

    /// Applies a runtime event to a job. `pre` runs in the same transaction
    /// before the transition, only if the event is valid for the job's
    /// state (so replayed duplicates are ignored as a whole).
    pub async fn handle_event<P>(self: &Arc<Self>, id: JobId, event: lifecycle::Event, pre: P)
    where
        P: FnOnce(&dyn StoreOps, &Job) -> Result<(), StorageError> + Send + 'static,
    {
        let core = self.clone();
        let now = now_ms();
        let result = self
            .write(move |tx, ch| {
                let Some(mut job) = tx.get_job(id)? else {
                    return Ok(None);
                };
                let transition = match lifecycle::on_event(&job, &event) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!("ignoring helper event: {e}");
                        return Ok(None);
                    }
                };
                pre(tx, &job)?;
                ch.updated(id);
                core.apply_in_tx(tx, &mut job, &transition, now, ch)?;
                Ok(Some((job, transition.effects)))
            })
            .await;
        match result {
            Ok(Some((job, effects))) => {
                let _ = self.run_effects(&job, &effects).await;
                self.reap_pending(id).await;
            }
            Ok(None) => {}
            Err(e) => tracing::error!(job = %id, "cannot record a job event: {}", e.0.message),
        }
    }

    async fn process_helper_event(self: &Arc<Self>, event: hp::Event) {
        use lifecycle::{Event as E, ExitStatus as X};
        match event {
            hp::Event::JobExited { job, status } => {
                let event = match status {
                    hp::ExitStatus::Exited { code } => E::Exited(X::Code(code)),
                    hp::ExitStatus::Signaled { signal } => E::Exited(X::Signal(signal)),
                    hp::ExitStatus::Unknown => E::Lost,
                };
                self.handle_event(job, event, no_pre).await;
            }
            hp::Event::CheckpointFinished { job, outcome } => {
                let path = self.image_dir(job);
                let now = now_ms();
                let (event, status, size, reason) = match outcome {
                    CheckpointOutcome::Success { image_size_bytes } => (
                        E::CheckpointSucceeded,
                        CheckpointStatus::Complete,
                        Some(image_size_bytes),
                        None,
                    ),
                    CheckpointOutcome::Failed { reason } => (
                        E::CheckpointFailed {
                            reason: reason.clone(),
                        },
                        CheckpointStatus::Failed,
                        None,
                        Some(reason),
                    ),
                };
                self.handle_event(job, event, move |tx, _| {
                    if tx.get_checkpoint(job)?.is_some() {
                        tx.set_checkpoint_status(job, status, size, reason.as_deref())
                    } else {
                        tx.put_checkpoint(&CheckpointRecord {
                            job_id: job,
                            path,
                            size_bytes: size,
                            gpu_model: None,
                            driver_version: None,
                            created_at: now,
                            status,
                            failure_reason: reason,
                        })
                    }
                })
                .await;
            }
            hp::Event::RestoreFinished { job, outcome } => {
                let event = match outcome {
                    RestoreOutcome::Success { .. } => E::RestoreSucceeded,
                    RestoreOutcome::Failed { reason } => E::RestoreFailed { reason },
                };
                self.handle_event(job, event, no_pre).await;
            }
        }
    }

    /// Compares the database with the helper's live jobs after a
    /// (re)connect (implementation-notes: "daemon / helper の再起動とジョブの
    /// 再発見"):
    /// - live in the database but unknown to the helper → `Lost` (an exit
    ///   status would have been replayed as an event before this point);
    /// - known to the helper but not live in the database → orphan, logged
    ///   and left alone;
    /// - `Terminating` jobs get their SIGTERM/SIGKILL sequence re-armed.
    pub async fn reconcile(self: &Arc<Self>, running: Vec<RunningJob>) {
        let live: HashSet<JobId> = running.iter().map(|r| r.job).collect();
        let jobs = match self.read(|s| s.list_jobs(&JobFilter::all())).await {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::error!("cannot reconcile with the helper: {}", e.0.message);
                return;
            }
        };
        let mut accounted = HashSet::new();
        for job in jobs.iter().filter(|j| j.state.occupies_slot()) {
            accounted.insert(job.id);
            if !live.contains(&job.id) {
                tracing::warn!(job = %job.id, state = job.state.name(), "job is gone; marking it lost");
                self.handle_event(job.id, lifecycle::Event::Lost, no_pre)
                    .await;
            } else if matches!(job.state, JobState::Terminating { .. })
                && let Err(e) = self.terminate(job).await
            {
                tracing::warn!(job = %job.id, "cannot stop job: {}", e.0.message);
            }
        }
        let orphans: BTreeSet<JobId> = live.difference(&accounted).copied().collect();
        for id in &orphans {
            tracing::warn!(
                job = %id,
                "the helper runs a job the daemon does not account for; leaving it alone"
            );
        }
        *self.orphans.lock().unwrap() = orphans;
        self.reconciled.store(true, Ordering::SeqCst);
        self.wake_scheduler();
    }

    /// Consumes helper notices until the daemon stops.
    pub async fn notice_loop(self: Arc<Self>, mut notices: mpsc::UnboundedReceiver<HelperNotice>) {
        loop {
            let notice = tokio::select! {
                _ = self.cancel.cancelled() => return,
                notice = notices.recv() => notice,
            };
            let Some(notice) = notice else { return };
            match notice {
                HelperNotice::Event(envelope) => {
                    self.process_helper_event(envelope.event).await;
                    if let Err(e) = self.helper.ack_events(envelope.seq).await {
                        tracing::debug!("cannot acknowledge helper events: {e}");
                    }
                }
                HelperNotice::Connected { version, running } => {
                    tracing::info!(%version, live = running.len(), "reconciling with the helper");
                    self.reconcile(running).await;
                }
                HelperNotice::Disconnected => {
                    self.reconciled.store(false, Ordering::SeqCst);
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Scheduling
    // -----------------------------------------------------------------

    /// Runs [`scheduler::plan`] whenever something changes, and at least
    /// once a second (for delayed enqueues).
    pub async fn scheduler_loop(self: Arc<Self>) {
        loop {
            if let Err(e) = self.schedule_once().await {
                tracing::error!("scheduling failed: {}", e.0.message);
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return,
                _ = self.schedule.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    pub async fn schedule_once(self: &Arc<Self>) -> ApiResult<()> {
        let launch = !self.is_shutting_down() && self.helper_ready();
        let now = now_ms();
        let core = self.clone();
        let launched = self
            .write(move |tx, ch| {
                let jobs = tx.list_jobs(&JobFilter::all())?;
                let groups = tx.list_groups()?;
                let gpus = tx.list_gpus()?;
                let plan = scheduler::plan(now, &jobs, &groups, &gpus);
                let mut by_id: HashMap<JobId, Job> = jobs.into_iter().map(|j| (j.id, j)).collect();
                let mut launched = Vec::new();
                for id in &plan.enqueue {
                    let Some(job) = by_id.get_mut(id) else {
                        continue;
                    };
                    if let Some(t) = lifecycle::enqueue_if_due(job, now) {
                        core.apply_in_tx(tx, job, &t, now, ch)?;
                    }
                }
                for id in &plan.dependency_failed {
                    let Some(job) = by_id.get_mut(id) else {
                        continue;
                    };
                    let t = Transition {
                        next: Some(JobState::Done {
                            result: JobResult::DependencyFailed,
                        }),
                        effects: Vec::new(),
                    };
                    core.apply_in_tx(tx, job, &t, now, ch)?;
                }
                if launch {
                    for id in plan.start.iter().chain(&plan.restore) {
                        let Some(job) = by_id.get_mut(id) else {
                            continue;
                        };
                        let t = lifecycle::dispatch(job)?;
                        core.apply_in_tx(tx, job, &t, now, ch)?;
                        launched.push((job.clone(), t.effects));
                    }
                }
                Ok(launched)
            })
            .await?;
        for (job, effects) in launched {
            let _ = self.run_effects(&job, &effects).await;
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // reset and shutdown
    // -----------------------------------------------------------------

    pub fn mark_for_removal(&self, id: JobId) {
        self.pending_removal.lock().unwrap().insert(id);
    }

    /// Removes a job `reset` asked for once it is removable.
    pub async fn reap_pending(self: &Arc<Self>, id: JobId) {
        if !self.pending_removal.lock().unwrap().contains(&id) {
            return;
        }
        let core = self.clone();
        let now = now_ms();
        let result = self
            .write(move |tx, ch| {
                let Some(mut job) = tx.get_job(id)? else {
                    return Ok(Some(None));
                };
                let Ok(t) = lifecycle::remove(&job) else {
                    return Ok(None);
                };
                let snapshot = job.clone();
                core.apply_in_tx(tx, &mut job, &t, now, ch)?;
                tx.remove_job(id)?;
                ch.removed(id);
                Ok(Some(Some((snapshot, t.effects))))
            })
            .await;
        match result {
            Ok(Some(removed)) => {
                self.pending_removal.lock().unwrap().remove(&id);
                if let Some((job, effects)) = removed {
                    let _ = self.run_effects(&job, &effects).await;
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(job = %id, "cannot remove job: {}", e.0.message),
        }
    }

    /// Waits until `pred` holds for the job list, the timeout passes or the
    /// daemon stops. Returns whether `pred` held.
    async fn wait_until(&self, timeout: Duration, pred: impl Fn(&[Job]) -> bool) -> bool {
        let mut rx = self.changes.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            rx.borrow_and_update();
            if let Ok(jobs) = self.read(|s| s.list_jobs(&JobFilter::all())).await
                && pred(&jobs)
            {
                return true;
            }
            tokio::select! {
                _ = self.cancel.cancelled() => return false,
                _ = tokio::time::sleep_until(deadline) => return false,
                _ = rx.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    /// `puzuz shutdown`: checkpoint (or with `no_checkpoint`, terminate)
    /// every running job, one checkpoint at a time, then stop the daemon.
    pub async fn shutdown(self: &Arc<Self>, no_checkpoint: bool) {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::info!(no_checkpoint, "shutting down");
        let checkpoint_wait = Duration::from_secs(30 * 60);
        let settle_wait = Duration::from_secs(self.config.grace_secs + 10);
        for _pass in 0..3 {
            let jobs = match self.read(|s| s.list_jobs(&JobFilter::all())).await {
                Ok(jobs) => jobs,
                Err(_) => break,
            };
            for job in jobs {
                match lifecycle::shutdown_action(&job, !no_checkpoint) {
                    ShutdownAction::Keep => {}
                    ShutdownAction::Terminate => {
                        if let Err(e) = self.terminate(&job).await {
                            tracing::warn!(job = %job.id, "cannot stop job: {}", e.0.message);
                        }
                    }
                    ShutdownAction::Checkpoint(_) => {
                        let id = job.id;
                        if let Err(e) = self.checkpoint_for_shutdown(id).await {
                            tracing::warn!(job = %id, "cannot checkpoint job: {}", e.0.message);
                            continue;
                        }
                        // One dump at a time.
                        self.wait_until(checkpoint_wait, |jobs| {
                            jobs.iter()
                                .find(|j| j.id == id)
                                .is_none_or(|j| j.state != JobState::Checkpointing)
                        })
                        .await;
                    }
                }
            }
            let settled = self
                .wait_until(settle_wait, |jobs| {
                    jobs.iter().all(|j| !j.state.occupies_slot())
                })
                .await;
            if settled {
                break;
            }
        }
        let _ = self.events.send(DaemonEvent::Shutdown);
        tracing::info!("shutdown complete");
        self.shutdown_complete.cancel();
    }

    async fn checkpoint_for_shutdown(self: &Arc<Self>, id: JobId) -> ApiResult<()> {
        let core = self.clone();
        let now = now_ms();
        let done = self
            .write(move |tx, ch| {
                let Some(mut job) = tx.get_job(id)? else {
                    return Ok(None);
                };
                let Ok(t) = lifecycle::checkpoint(&job) else {
                    return Ok(None);
                };
                core.apply_in_tx(tx, &mut job, &t, now, ch)?;
                Ok(Some((job, t.effects)))
            })
            .await?;
        if let Some((job, effects)) = done {
            self.run_effects(&job, &effects).await?;
        }
        Ok(())
    }
}
