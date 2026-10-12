//! The daemon's runtime state and the machinery shared by the API, the
//! scheduler and the helper event loop.
//!
//! Every state change follows the same pattern:
//! 1. [`Core::write`] runs a closure in one SQLite transaction. The closure
//!    re-reads the job, asks `puzuz_core::lifecycle` for a [`Transition`],
//!    and persists it with [`Core::apply_in_tx`] (which also performs the
//!    effects that only touch the database).
//! 2. Still under the database lock, the changed jobs are published to
//!    `/v1/events` subscribers, so events are delivered in commit order, and
//!    launches (spawn/restore) are registered in [`Launches`].
//! 3. The remaining effects (helper calls, log files) run afterwards with
//!    [`Core::run_effects`]; their failures may lead to further transitions.
//!
//! Effects that act on a live process (signals, checkpoints) must not reach
//! the helper before the spawn or restore request that started the process
//! has been answered (see [`crate::launch`]); [`Core::wait_for_launch`] and
//! [`Core::launch_in_flight`] enforce that ordering per job.

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
use puzuz_core::storage::{
    CheckpointRecord, CheckpointStatus, JobError, JobErrorKind, JobFilter, Store, StoreOps, Tx,
};
use puzuz_protocol::api::{self, ErrorCode, GpuDetectionStatus, JobView, Warning};
use puzuz_protocol::helper::{
    self as hp, CheckpointOptions, CheckpointOutcome, HelperErrorCode, RestoreOutcome,
    RestoreRequest, RunningJob, SpawnRequest, signal,
};
use tokio::sync::{Notify, Semaphore, broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::{ApiResult, Failure};
use crate::gpu::DetectedGpu;
use crate::helper::{Helper, HelperError, HelperNotice};
use crate::launch::{LaunchOutcome, Launches, RetryStatus, SettleGuard};
use crate::logfile;
use crate::usage::SizeWarnings;
use crate::users::UserDb;

/// Current time in Unix epoch milliseconds.
pub fn now_ms() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as Timestamp)
        .unwrap_or(0)
}

/// The reason recorded for a dump the helper refused before touching the
/// job (`code: checkpoint_refused`).
pub fn refusal_reason(helper_reason: &str) -> String {
    format!("checkpoint refused: {helper_reason}")
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
    /// The last error recorded for the job (a failed restore).
    pub error: Option<JobError>,
}

impl JobInfo {
    pub fn load(store: &impl StoreOps, job: Job) -> Result<Self, StorageError> {
        let checkpoint = store.get_checkpoint(job.id)?;
        let error = store.get_job_error(job.id)?;
        Ok(Self {
            job,
            checkpoint,
            error,
        })
    }

    /// The view `caller` may see, with checkpoint information and the last
    /// error (owner and administrators only) filled in.
    pub fn view(&self, caller: &Caller) -> JobView {
        let mut view = JobView::for_caller(&self.job, caller);
        if let Some(record) = &self.checkpoint
            && record.status == CheckpointStatus::Complete
        {
            view.checkpoint_size_bytes = record.size_bytes;
        }
        if let Some(details) = view.details.as_mut() {
            // A recorded error is the most recent one: it is cleared by the
            // next successful restore, and a dump can only follow that.
            let dump_failure = self
                .checkpoint
                .as_ref()
                .and_then(|record| record.failure_reason.clone());
            if let Some(error) = &self.error {
                details.last_error = Some(error.message.clone());
            } else if dump_failure.is_some() {
                details.last_error = dump_failure;
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
    /// The event as `caller` may see it. Job views leave out the
    /// environment, like `GET /v1/jobs`.
    pub fn to_api(&self, caller: &Caller) -> api::Event {
        match self {
            DaemonEvent::JobAdded(info) => api::Event::JobAdded {
                job: info.view(caller).without_envs(),
            },
            DaemonEvent::JobUpdated(info) => api::Event::JobUpdated {
                job: info.view(caller).without_envs(),
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
    /// Jobs whose spawn or restore request is sent after the commit.
    launching: Vec<JobId>,
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
    fn launching(&mut self, id: JobId) {
        self.launching.push(id);
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

/// The states a forced launch (see [`Core::force_launch`]) goes through:
/// waiting for the helper, then launching or launched.
fn is_launch_state(state: &JobState) -> bool {
    matches!(
        state,
        JobState::Queued { .. }
            | JobState::RestoreQueued { .. }
            | JobState::Running
            | JobState::Restoring
    )
}

/// Whether an effect acts on the job's live process, which the helper only
/// knows once the spawn or restore request that started it was answered.
pub fn targets_live_process(effect: &Effect) -> bool {
    needs_helper(effect)
        && !matches!(
            effect,
            Effect::Spawn | Effect::Restore | Effect::DeleteCheckpointImage
        )
}

/// How long an operation waits for the helper to answer a job's spawn or
/// restore request (the helper client gives up on a request after 60 s).
const LAUNCH_WAIT: Duration = Duration::from_secs(75);

/// How long an action whose launch is being retried waits for the retry's
/// request to return before it reports the launch as pending.
const RETRY_SETTLE_WAIT: Duration = Duration::from_millis(500);

/// How often the daemon asks the helper about a job whose checkpoint
/// request went unanswered (see [`Core::checkpoint_lost_check`]).
const CHECKPOINT_CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// Spawn/restore requests the scheduler has outstanding at most.
const MAX_CONCURRENT_LAUNCHES: usize = 8;

/// Testing only: awaited by an action whose launch the helper did not carry
/// out, before it finds out whether the launch is being retried (see
/// [`Core::set_launch_check_hook`]).
pub type LaunchCheckHook = Arc<dyn Fn(JobId) -> BoxFuture<'static, ()> + Send + Sync>;

/// How a launching action's launch that failed fares (see
/// [`Core::launch_retried`]).
#[derive(Debug)]
pub enum Retried {
    /// Not retried: the failure stands.
    No,
    /// Being retried (back in line, or launching again), or unknown whether
    /// the helper got the request.
    Pending,
    /// Retried, and the retry succeeded.
    Launched,
    /// Retried, and the retry failed with this error.
    Failed(Failure),
}

/// A spawn or restore request that failed: the error to report and what
/// became of the launch.
struct LaunchFailure {
    error: Failure,
    outcome: LaunchOutcome,
}

impl LaunchFailure {
    fn new(error: Failure, outcome: LaunchOutcome) -> Self {
        Self { error, outcome }
    }

    /// Put back in line (`requeued`), or failed as the job has moved on.
    fn requeued(error: Failure, requeued: bool) -> Self {
        let outcome = if requeued {
            LaunchOutcome::Requeued
        } else {
            LaunchOutcome::Failed(error.clone())
        };
        Self { error, outcome }
    }
}

impl From<Failure> for LaunchFailure {
    fn from(error: Failure) -> Self {
        Self::new(error.clone(), LaunchOutcome::Failed(error))
    }
}

/// Settles a launch with the outcome of its request.
fn settle_launch(settle: SettleGuard<'_>, result: Result<(), LaunchFailure>) -> ApiResult<()> {
    match result {
        Ok(()) => {
            settle.finish(LaunchOutcome::Done);
            Ok(())
        }
        Err(LaunchFailure { error, outcome }) => {
            settle.finish(outcome);
            Err(error)
        }
    }
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
    /// The GPUs detected at start-up. GPUs in the store but not here are
    /// unavailable (see [`crate::gpu`]).
    pub detected_gpus: HashMap<GpuIndex, DetectedGpu>,
    /// How the GPUs were detected, for `GET /v1/version`.
    pub gpu_detection: GpuDetectionStatus,
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
    /// Spawn and restore requests the helper has not answered yet.
    launches: Launches,
    /// Bounds the scheduler's concurrent spawn/restore requests.
    launch_slots: Semaphore,
    /// Jobs whose spawn or restore request went unanswered (connection
    /// lost or timed out), with the state that launch put them in
    /// (`Running` or `Restoring`); see [`Core::spawn_lost_check`].
    unconfirmed_launches: Mutex<HashMap<JobId, JobState>>,
    /// Jobs to launch regardless of free slots (`start`, `add
    /// --immediate`, `restore --immediate`, `restart --immediate`), kept
    /// until they leave the queued, running and restoring states. While
    /// one is queued (the helper was not connected, or did not start it),
    /// the scheduler launches it as soon as it can, ignoring slots. Kept in
    /// memory only: after a daemon restart such a job waits for a slot.
    forced: Mutex<HashSet<JobId>>,
    /// See [`LaunchCheckHook`].
    launch_check_hook: Mutex<Option<LaunchCheckHook>>,
    /// Checkpoint size warnings (see [`crate::usage`]).
    pub(crate) size_warnings: Mutex<SizeWarnings>,
}

impl Core {
    pub fn new(
        config: Config,
        store: Store,
        helper: Arc<dyn Helper>,
        users: Arc<dyn UserDb>,
        detected_gpus: HashMap<GpuIndex, DetectedGpu>,
        gpu_detection: GpuDetectionStatus,
        trust_uid_header: bool,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(1024);
        let (changes, _) = watch::channel(0);
        Arc::new(Self {
            config,
            db: Db::new(store),
            helper,
            users,
            detected_gpus,
            gpu_detection,
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
            launches: Launches::default(),
            launch_slots: Semaphore::new(MAX_CONCURRENT_LAUNCHES),
            unconfirmed_launches: Mutex::new(HashMap::new()),
            forced: Mutex::new(HashSet::new()),
            launch_check_hook: Mutex::new(None),
            size_warnings: Mutex::default(),
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

    /// Whether a spawn or restore request for `id` is waiting for the
    /// helper's answer. Consistent with the database when called inside
    /// [`Core::write`], since launches are registered under its lock.
    pub fn launch_in_flight(&self, id: JobId) -> bool {
        self.launches.in_flight(id)
    }

    /// The sequence number of the latest launch of `id` (see
    /// [`Launches::latest`]).
    pub fn latest_launch(&self, id: JobId) -> Option<u64> {
        self.launches.latest(id)
    }

    /// Waits until the helper has answered a pending spawn or restore
    /// request for `id`, so that requests for the live process do not
    /// overtake it. Fails (retryably) when the daemon stops or the helper
    /// takes too long.
    pub async fn wait_for_launch(&self, id: JobId) -> ApiResult<()> {
        if !self.launches.in_flight(id) {
            return Ok(());
        }
        tokio::select! {
            _ = self.launches.wait(id) => Ok(()),
            _ = self.cancel.cancelled() => Err(Failure::new(
                ErrorCode::ShuttingDown,
                "the daemon is stopping",
            )),
            _ = tokio::time::sleep(LAUNCH_WAIT) => Err(Failure::new(
                ErrorCode::HelperUnavailable,
                format!(
                    "the puzuz helper has not confirmed that job {id} started; try again later"
                ),
            )),
        }
    }

    /// Marks `id` to be launched regardless of free slots (see
    /// [`Core::forced`]).
    pub fn force_launch(&self, id: JobId) {
        self.forced.lock().unwrap().insert(id);
    }

    /// Testing only: see [`LaunchCheckHook`].
    pub fn set_launch_check_hook(&self, hook: Option<LaunchCheckHook>) {
        *self.launch_check_hook.lock().unwrap() = hook;
    }

    /// How launch `seq` of `id`, whose request failed, fares: whether it
    /// is retried by itself (the helper is stopping or went away) and, if
    /// it has been retried already, how the retry went. Derived from the
    /// outcomes the launches recorded (see [`Launches::retry_status`]), so
    /// the answer does not depend on how far the retry has got by now:
    /// - launched again: the latest launch is in flight, or was itself put
    ///   back in line (or its answer lost) → [`Retried::Pending`]; it
    ///   succeeded → [`Retried::Launched`]; it failed →
    ///   [`Retried::Failed`];
    /// - not launched again yet: requeued, still waiting in line and
    ///   forced → [`Retried::Pending`], as when its answer was lost (it is
    ///   requeued if the helper never got it, see
    ///   [`Core::spawn_lost_check`]); otherwise [`Retried::No`].
    ///
    /// A retry in flight is given [`RETRY_SETTLE_WAIT`] to return first.
    pub async fn launch_retried(self: &Arc<Self>, id: JobId, seq: u64) -> Retried {
        let hook = self.launch_check_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(id).await;
        }
        match self.retry_outcome(id, seq).await {
            // The retry may be about to settle (its effect on the job can be
            // visible before its request returns): give it a moment, so
            // the reply reflects its outcome rather than a pending launch.
            Retried::Pending if self.launches.in_flight(id) => {
                let _ = tokio::time::timeout(RETRY_SETTLE_WAIT, self.launches.wait(id)).await;
                self.retry_outcome(id, seq).await
            }
            retried => retried,
        }
    }

    /// See [`Core::launch_retried`].
    async fn retry_outcome(self: &Arc<Self>, id: JobId, seq: u64) -> Retried {
        let core = self.clone();
        // Under the database lock, so that a job waiting in line has not
        // been launched again in between.
        let result = self
            .read(move |s| {
                Ok(match core.launches.retry_status(id, seq) {
                    RetryStatus::RetryInFlight
                    | RetryStatus::RetrySettled(
                        None | Some(LaunchOutcome::Requeued | LaunchOutcome::Unconfirmed),
                    ) => Retried::Pending,
                    RetryStatus::RetrySettled(Some(LaunchOutcome::Done)) => Retried::Launched,
                    RetryStatus::RetrySettled(Some(LaunchOutcome::Failed(e))) => Retried::Failed(e),
                    RetryStatus::NotRelaunched(Some(LaunchOutcome::Unconfirmed)) => {
                        Retried::Pending
                    }
                    RetryStatus::NotRelaunched(Some(LaunchOutcome::Requeued)) => {
                        let waiting = matches!(
                            s.get_job(id)?.map(|job| job.state),
                            Some(JobState::Queued { .. } | JobState::RestoreQueued { .. })
                        );
                        if waiting && core.forced.lock().unwrap().contains(&id) {
                            Retried::Pending
                        } else {
                            Retried::No
                        }
                    }
                    RetryStatus::NotRelaunched(_) => Retried::No,
                })
            })
            .await;
        result.unwrap_or(Retried::No)
    }

    pub fn orphans(&self) -> Vec<JobId> {
        self.orphans.lock().unwrap().iter().copied().collect()
    }

    pub fn log_path(&self, owner: Uid, id: JobId) -> PathBuf {
        self.config
            .logs_dir()
            .join(owner.0.to_string())
            .join(logfile::file_name(id))
    }

    /// Opens job `id`'s log, refusing anything but a regular file of its
    /// owner (see [`logfile`]). Blocking.
    pub fn open_log(&self, owner: Uid, id: JobId) -> std::io::Result<std::fs::File> {
        logfile::open(&self.config.logs_dir(), owner, id, logfile::Access::Read)
    }

    pub fn image_dir(&self, id: JobId) -> PathBuf {
        self.config.checkpoints_dir().join(id.to_string())
    }

    pub fn wake_scheduler(&self) {
        self.schedule.notify_one();
    }

    /// Whether `index` was detected at start-up (and so may start jobs).
    pub fn gpu_available(&self, index: GpuIndex) -> bool {
        self.detected_gpus.contains_key(&index)
    }

    /// The GPU model(s) and driver version to record with a checkpoint of a
    /// job using `gpus`; `(None, None)` for CPU-only jobs.
    fn gpu_identity(&self, gpus: &[GpuIndex]) -> (Option<String>, Option<String>) {
        let detected: Vec<&DetectedGpu> = gpus
            .iter()
            .filter_map(|index| self.detected_gpus.get(index))
            .collect();
        let mut models: Vec<&str> = Vec::new();
        for gpu in &detected {
            if !models.contains(&gpu.name.as_str()) {
                models.push(&gpu.name);
            }
        }
        let model = (!models.is_empty()).then(|| models.join(", "));
        let driver = detected.iter().find_map(|gpu| gpu.driver_version.clone());
        (model, driver)
    }

    /// A new in-progress checkpoint record for `job`; `paused`: the job
    /// was paused when the dump began (see [`lifecycle::keep_paused`]).
    fn new_checkpoint_record(&self, job: &Job, paused: bool, now: Timestamp) -> CheckpointRecord {
        let (gpu_model, driver_version) = self.gpu_identity(&job.spec.gpus);
        CheckpointRecord {
            job_id: job.id,
            path: self.image_dir(job.id),
            size_bytes: None,
            gpu_model,
            driver_version,
            created_at: now,
            status: CheckpointStatus::InProgress,
            failure_reason: None,
            paused,
        }
    }

    /// The warnings `owner` has not been shown yet (failed dumps and failed
    /// restores, ordered by job), followed by checkpoint size warnings (see
    /// [`crate::usage`]).
    pub async fn pending_warnings(&self, owner: Uid) -> ApiResult<Vec<Warning>> {
        let (dumps, errors) = self
            .read(move |s| {
                Ok((
                    s.unacknowledged_checkpoint_failures(owner)?,
                    s.unacknowledged_job_errors(owner)?,
                ))
            })
            .await?;
        let mut warnings: Vec<Warning> = dumps
            .into_iter()
            .map(|job| Warning::CheckpointFailed { job })
            .chain(errors.into_iter().map(|(job, kind)| match kind {
                JobErrorKind::Restore => Warning::RestoreFailed { job },
            }))
            .collect();
        warnings.sort_by_key(Warning::job);
        warnings.extend(self.pending_size_warnings(owner).await);
        Ok(warnings)
    }

    /// Records that `warnings` have been shown to `owner`.
    pub async fn acknowledge_warnings(&self, owner: Uid, warnings: &[Warning]) -> ApiResult<()> {
        self.acknowledge_size_warnings(owner, warnings);
        let mut dumps = Vec::new();
        let mut errors = Vec::new();
        for warning in warnings {
            match warning {
                Warning::CheckpointFailed { job } => dumps.push(*job),
                Warning::RestoreFailed { job } => errors.push(*job),
                Warning::CheckpointOverLimit { .. } | Warning::CheckpointUsageOverLimit { .. } => {}
            }
        }
        self.read(move |s| {
            s.acknowledge_checkpoint_failures(&dumps)?;
            s.acknowledge_job_errors(&errors)
        })
        .await
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
                    for &id in &changes.launching {
                        core.launches.begin(id);
                    }
                    for &id in &changes.removed {
                        core.launches.forget(id);
                    }
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
    /// performs its database-only effects, and [`Effect::ResetLog`]: the
    /// log must be empty before the job is back in line, or the scheduler
    /// could start it and the truncation cut off its first output. Returns
    /// the job created by [`Effect::CreateRestartedCopy`], if any.
    pub fn apply_in_tx(
        &self,
        tx: &Tx<'_>,
        job: &mut Job,
        transition: &Transition,
        now: Timestamp,
        changes: &mut Changes,
    ) -> ApiResult<Option<Job>> {
        let was_paused = job.state == JobState::Paused;
        if transition.next.is_some() {
            // From `Checkpointed`: a late success of a restore that was
            // given up as interrupted.
            let restored = matches!(job.state, JobState::Restoring | JobState::Checkpointed)
                && transition.next.as_ref() == Some(&JobState::Running);
            transition.apply(job, now);
            tx.update_job_state(job.id, &job.state, job.started_at, job.finished_at)?;
            if !is_launch_state(&job.state) {
                self.forced.lock().unwrap().remove(&job.id);
            }
            if restored {
                // The job runs again; an earlier failed attempt is history.
                tx.clear_job_error(job.id)?;
            }
            changes.updated(job.id);
        }
        let mut copy = None;
        for effect in &transition.effects {
            match effect {
                Effect::Spawn | Effect::Restore => changes.launching(job.id),
                Effect::ResetLog => {
                    // On the database thread, before the commit.
                    logfile::truncate(&self.config.logs_dir(), job.owner, job.id).map_err(|e| {
                        let code = if e.kind() == std::io::ErrorKind::PermissionDenied {
                            ErrorCode::PermissionDenied
                        } else {
                            ErrorCode::Internal
                        };
                        Failure::new(code, format!("cannot reset the log of job {}: {e}", job.id))
                    })?;
                }
                Effect::Checkpoint => {
                    tx.put_checkpoint(&self.new_checkpoint_record(job, was_paused, now))?;
                    changes.updated(job.id);
                }
                Effect::RecordRestoreError(reason) => {
                    // Kept with the job rather than with its checkpoint
                    // record, so the reason survives without a record.
                    tracing::warn!(job = %job.id, "restore failed: {reason}");
                    tx.set_job_error(job.id, JobErrorKind::Restore, reason, now)?;
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
            Effect::Spawn => {
                let settle = self.launches.settle_on_drop(job.id);
                settle_launch(settle, self.spawn_job(job).await)
            }
            Effect::SignalStop => self.signal(job, signal::SIGSTOP).await,
            Effect::SignalContinue => self.signal(job, signal::SIGCONT).await,
            Effect::SendSignal(sig) => self.signal(job, *sig).await,
            Effect::Kill | Effect::Terminate => self.terminate(job).await,
            Effect::Checkpoint => self.start_checkpoint(job).await,
            Effect::Restore => {
                let settle = self.launches.settle_on_drop(job.id);
                settle_launch(settle, self.start_restore(job).await)
            }
            // After a successful restore the helper has already removed the
            // image (and refuses to touch the directory of a live job). A
            // job paused before its dump is `Paused` after the restore.
            Effect::DeleteCheckpointImage
                if matches!(job.state, JobState::Running | JobState::Paused) =>
            {
                Ok(())
            }
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
            // Done in the transaction (see `Core::apply_in_tx`).
            Effect::ResetLog
            | Effect::CreateRestartedCopy { .. }
            | Effect::RecordRestoreError(_) => Ok(()),
        }
    }

    async fn spawn_job(self: &Arc<Self>, job: &Job) -> Result<(), LaunchFailure> {
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
            Err(
                e @ HelperError::Rejected {
                    code: HelperErrorCode::ShuttingDown,
                    ..
                },
            ) => {
                // The helper is stopping: nothing was started (it refuses
                // new spawns, and abandons those systemd had not begun to
                // start). Not the job's fault; put it back in line once
                // that helper is likely gone, rather than offering it
                // again in a tight loop.
                tracing::info!(job = %job.id, "the helper is stopping; requeueing the job");
                tokio::select! {
                    _ = self.cancel.cancelled() => {}
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                let requeued = self
                    .requeue_unstarted(job.id, Some(job.started_at), JobState::Running)
                    .await?;
                Err(LaunchFailure::requeued(e.into(), requeued))
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
                let requeued = self
                    .requeue_unstarted(job.id, Some(job.started_at), JobState::Running)
                    .await?;
                Err(LaunchFailure::requeued(
                    HelperError::NotConnected.into(),
                    requeued,
                ))
            }
            // Unknown whether it started (the connection broke or the
            // answer is late). Reconciliation (after a reconnect) or the
            // check below tells: a job the helper does not know was never
            // started and goes back in line, see `Core::spawn_lost_check`.
            Err(e) => {
                self.unconfirmed_launches
                    .lock()
                    .unwrap()
                    .insert(job.id, JobState::Running);
                if matches!(e, HelperError::Timeout) {
                    self.spawn_lost_check(job.id, job.started_at);
                }
                Err(LaunchFailure::new(e.into(), LaunchOutcome::Unconfirmed))
            }
        }
    }

    /// Puts a job whose spawn or restore request did not start it back in
    /// line (see [`lifecycle::requeue_unlaunched`]), unless it has moved on
    /// since: it must still be in `launched` (`Running` after a spawn,
    /// `Restoring` after a restore) and, if given, have the `started_at`
    /// that identifies the launch. Returns whether it was requeued. A job
    /// that was to be launched regardless of slots stays so.
    async fn requeue_unstarted(
        self: &Arc<Self>,
        id: JobId,
        started_at: Option<Option<Timestamp>>,
        launched: JobState,
    ) -> ApiResult<bool> {
        let core = self.clone();
        let now = now_ms();
        self.write(move |tx, ch| {
            let Some(mut job) = tx.get_job(id)? else {
                return Ok(false);
            };
            if job.state != launched || started_at.is_some_and(|at| at != job.started_at) {
                return Ok(false);
            }
            let t = lifecycle::requeue_unlaunched(&job, now)?;
            core.apply_in_tx(tx, &mut job, &t, now, ch)?;
            Ok(true)
        })
        .await
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
    ///
    /// A job whose spawn request went unanswered (see
    /// [`Core::spawn_job`]) is put back in line instead: the helper
    /// records a spawn before it starts the unit and re-discovers the
    /// unit, or reports how the job ended, after a restart (its replayed
    /// events precede its list of live jobs). So a job it neither lists
    /// nor reported was never started.
    fn spawn_lost_check(self: &Arc<Self>, id: JobId, started_at: Option<Timestamp>) {
        let core = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = core.cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            if !core.still_live(id, started_at).await {
                core.unconfirmed_launches.lock().unwrap().remove(&id);
                return;
            }
            let Ok(running) = core.helper.list_running().await else {
                return;
            };
            if running.iter().any(|r| r.job == id) {
                core.unconfirmed_launches.lock().unwrap().remove(&id);
                return;
            }
            let unconfirmed = core.unconfirmed_launches.lock().unwrap().remove(&id);
            if let Some(launched) = unconfirmed {
                // An exit (or restore outcome) reported just before the
                // list was taken arrives shortly; give it time before
                // launching the job again.
                tokio::select! {
                    _ = core.cancel.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                match core.requeue_unstarted(id, Some(started_at), launched).await {
                    Ok(true) => {
                        tracing::warn!(job = %id, "the helper never started the job; requeueing it");
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(job = %id, "cannot requeue the job: {}", e.0.message);
                    }
                }
                return;
            }
            tracing::warn!(job = %id, "the helper no longer knows the job; marking it lost");
            core.handle_event(id, lifecycle::Event::Lost, no_pre).await;
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
                let refused = matches!(
                    e,
                    HelperError::Rejected {
                        code: HelperErrorCode::CheckpointRefused,
                        ..
                    }
                );
                let reason = if refused {
                    refusal_reason(&e.to_string())
                } else {
                    e.to_string()
                };
                let failure_reason = reason.clone();
                // A job that was paused for the dump was resumed for it (see
                // `lifecycle::checkpoint`); it is paused again.
                let repause = self
                    .revert_checkpoint(id, reason)
                    .await?
                    .filter(|job| job.state == JobState::Paused);
                let paused = repause.is_some();
                if let Some(job) = repause
                    && let Err(err) = self.signal(&job, signal::SIGSTOP).await
                {
                    tracing::warn!(job = %id, "cannot pause the job again: {}", err.0.message);
                }
                let failure = refused.then(|| {
                    let keeps = if paused {
                        "the job stays paused"
                    } else {
                        "the job keeps running"
                    };
                    Failure::new(
                        ErrorCode::InvalidState,
                        format!("{failure_reason}; {keeps}"),
                    )
                });
                Err(failure.unwrap_or_else(|| e.into()))
            }
            // Unknown whether the helper got the request: find out, so the
            // job does not stay checkpointing forever (holding its slots,
            // refusing kill and pause, and holding up a shutdown).
            Err(e) => {
                self.checkpoint_lost_check(job.id, job.started_at);
                Err(e.into())
            }
        }
    }

    /// Ends a checkpoint that did not run: the job goes back to `Running`,
    /// or `Paused` if it was paused when the dump was asked for (its
    /// record's `paused` flag), and the checkpoint record is marked failed
    /// with `reason` (its owner is warned). Returns the job in its new
    /// state, or `None` if it was not checkpointing.
    async fn revert_checkpoint(
        self: &Arc<Self>,
        id: JobId,
        reason: String,
    ) -> ApiResult<Option<Job>> {
        let core = self.clone();
        let now = now_ms();
        self.write(move |tx, ch| {
            let Some(mut job) = tx.get_job(id)? else {
                return Ok(None);
            };
            if job.state != JobState::Checkpointing {
                return Ok(None);
            }
            let record = tx.get_checkpoint(id)?;
            let paused = record.as_ref().is_some_and(|r| r.paused);
            let next = if paused {
                JobState::Paused
            } else {
                JobState::Running
            };
            tx.update_job_state(id, &next, job.started_at, None)?;
            if record.is_some() {
                tx.set_checkpoint_status(id, CheckpointStatus::Failed, None, Some(&reason))?;
            } else {
                tx.put_checkpoint(&CheckpointRecord {
                    status: CheckpointStatus::Failed,
                    failure_reason: Some(reason),
                    ..core.new_checkpoint_record(&job, false, now)
                })?;
            }
            ch.updated(id);
            job.state = next;
            Ok(Some(job))
        })
        .await
    }

    /// The helper runs `id` but is not dumping it (`phase`), although the
    /// database says it is checkpointing: the checkpoint request was lost
    /// (the connection broke or the helper did not answer). Back to the
    /// state before the dump (see [`Core::revert_checkpoint`]), with the
    /// process stopped or continued to match.
    async fn abandon_lost_checkpoint(self: &Arc<Self>, id: JobId, phase: hp::RunningPhase) {
        let reason = "the checkpoint request was lost before the dump began".to_owned();
        let job = match self.revert_checkpoint(id, reason).await {
            Ok(Some(job)) => job,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(job = %id, "cannot end the lost checkpoint: {}", e.0.message);
                return;
            }
        };
        tracing::warn!(
            job = %id,
            state = job.state.name(),
            "the helper is not checkpointing the job; its checkpoint request was lost"
        );
        let sig = match (&job.state, phase) {
            (JobState::Paused, hp::RunningPhase::Running) => Some(signal::SIGSTOP),
            (JobState::Running, hp::RunningPhase::Stopped) => Some(signal::SIGCONT),
            _ => None,
        };
        if let Some(sig) = sig
            && let Err(e) = self.signal(&job, sig).await
        {
            tracing::warn!(job = %id, "cannot signal the job: {}", e.0.message);
        }
    }

    /// The checkpoint request for `id` went unanswered: until the job
    /// leaves `Checkpointing` (the dump's outcome arrives), ask the helper
    /// every [`CHECKPOINT_CHECK_INTERVAL`] what it does with the job. Not
    /// dumping it → [`Core::abandon_lost_checkpoint`]; gone, twice in a row
    /// (its outcome would have arrived in between) → `Lost`.
    fn checkpoint_lost_check(self: &Arc<Self>, id: JobId, started_at: Option<Timestamp>) {
        let core = self.clone();
        tokio::spawn(async move {
            let mut missing = false;
            loop {
                tokio::select! {
                    _ = core.cancel.cancelled() => return,
                    _ = tokio::time::sleep(CHECKPOINT_CHECK_INTERVAL) => {}
                }
                let checkpointing = matches!(
                    core.read(move |s| s.get_job(id)).await,
                    Ok(Some(job)) if job.state == JobState::Checkpointing
                        && job.started_at == started_at
                );
                if !checkpointing {
                    return;
                }
                // Not connected: the reconciliation after the reconnect
                // decides instead (or this check, if it comes first).
                let Ok(running) = core.helper.list_running().await else {
                    continue;
                };
                match running.iter().find(|r| r.job == id).map(|r| r.phase) {
                    Some(phase @ (hp::RunningPhase::Running | hp::RunningPhase::Stopped)) => {
                        core.abandon_lost_checkpoint(id, phase).await;
                        return;
                    }
                    Some(_) => missing = false,
                    None if missing => {
                        tracing::warn!(job = %id, "the helper no longer knows the job; marking it lost");
                        core.handle_event(id, lifecycle::Event::Lost, no_pre).await;
                        return;
                    }
                    None => missing = true,
                }
            }
        });
    }

    async fn start_restore(self: &Arc<Self>, job: &Job) -> Result<(), LaunchFailure> {
        self.warn_if_gpus_changed(job).await;
        let request = RestoreRequest {
            job: job.id,
            uid: job.owner,
            image_dir: self.image_dir(job.id),
            gpus: job.spec.gpus.clone(),
        };
        match self.helper.restore(request).await {
            Ok(()) => Ok(()),
            Err(
                e @ HelperError::Rejected {
                    code: HelperErrorCode::ShuttingDown,
                    ..
                },
            ) => {
                // As for spawns (see `Core::spawn_job`): nothing was
                // restored and it is not the job's fault; wait for that
                // helper to go away, then put the job back in line.
                tracing::info!(job = %job.id, "the helper is stopping; requeueing the restore");
                tokio::select! {
                    _ = self.cancel.cancelled() => {}
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                let requeued = self
                    .requeue_unstarted(job.id, Some(job.started_at), JobState::Restoring)
                    .await?;
                Err(LaunchFailure::requeued(e.into(), requeued))
            }
            Err(HelperError::NotConnected) => {
                // Never reached the helper.
                let requeued = self
                    .requeue_unstarted(job.id, Some(job.started_at), JobState::Restoring)
                    .await?;
                Err(LaunchFailure::requeued(
                    HelperError::NotConnected.into(),
                    requeued,
                ))
            }
            Err(e @ (HelperError::Rejected { .. } | HelperError::Protocol(_))) => {
                self.handle_event(
                    job.id,
                    lifecycle::Event::RestoreFailed {
                        reason: e.to_string(),
                    },
                    no_pre,
                )
                .await;
                Err(Failure::from(e).into())
            }
            // Unknown whether the helper got the request (see
            // `Core::spawn_job`): its outcome event arrives if it did, and a
            // restore it neither reports nor runs is requeued.
            Err(e) => {
                self.unconfirmed_launches
                    .lock()
                    .unwrap()
                    .insert(job.id, JobState::Restoring);
                if matches!(e, HelperError::Timeout) {
                    self.spawn_lost_check(job.id, job.started_at);
                }
                Err(LaunchFailure::new(e.into(), LaunchOutcome::Unconfirmed))
            }
        }
    }

    /// Logs when a checkpoint is restored on a different GPU model or
    /// driver than it was dumped with (cuda-checkpoint may then fail; the
    /// helper reports that as a restore failure).
    async fn warn_if_gpus_changed(&self, job: &Job) {
        let id = job.id;
        let Ok(Some(record)) = self.read(move |s| s.get_checkpoint(id)).await else {
            return;
        };
        let (model, driver) = self.gpu_identity(&job.spec.gpus);
        let differs = |then: &Option<String>, now: &Option<String>| {
            then.is_some() && now.is_some() && then != now
        };
        if differs(&record.gpu_model, &model) || differs(&record.driver_version, &driver) {
            tracing::warn!(
                job = %id,
                dumped_on = ?record.gpu_model,
                dumped_driver = ?record.driver_version,
                now = ?model,
                driver = ?driver,
                "restoring a checkpoint on a different GPU model or driver"
            );
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
                let mut transition = match lifecycle::on_event(&job, &event) {
                    Ok(t) => t,
                    Err(e) => {
                        tracing::debug!("ignoring helper event: {e}");
                        return Ok(None);
                    }
                };
                if tx.get_checkpoint(id)?.is_some_and(|r| r.paused) {
                    transition = lifecycle::keep_paused(&event, transition);
                }
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

    /// Applies helper event `seq` (see [`Core::apply_helper_event`]); its
    /// sequence number is recorded in the transaction that applies it.
    async fn process_helper_event(self: &Arc<Self>, event: hp::Event, seq: u64) {
        use lifecycle::{Event as E, ExitStatus as X};
        let mark = move |tx: &dyn StoreOps, _: &Job| tx.set_helper_event_seq(seq);
        match event {
            hp::Event::JobExited { job, status } => {
                let event = match status {
                    hp::ExitStatus::Exited { code } => E::Exited(X::Code(code)),
                    hp::ExitStatus::Signaled { signal } => E::Exited(X::Signal(signal)),
                    hp::ExitStatus::Unknown => E::Lost,
                };
                self.handle_event(job, event, mark).await;
            }
            hp::Event::CheckpointFinished { job, outcome } => {
                let now = now_ms();
                let succeeded = matches!(outcome, CheckpointOutcome::Success { .. });
                let (event, status, size, reason) = match outcome {
                    CheckpointOutcome::Success { image_size_bytes } => (
                        E::CheckpointSucceeded,
                        CheckpointStatus::Complete,
                        Some(image_size_bytes),
                        None,
                    ),
                    CheckpointOutcome::Failed {
                        reason,
                        code: Some(HelperErrorCode::CheckpointRefused),
                    } => {
                        let reason = refusal_reason(&reason);
                        (
                            E::CheckpointRefused {
                                reason: reason.clone(),
                            },
                            CheckpointStatus::Failed,
                            None,
                            Some(reason),
                        )
                    }
                    CheckpointOutcome::Failed { reason, .. } => (
                        E::CheckpointFailed {
                            reason: reason.clone(),
                        },
                        CheckpointStatus::Failed,
                        None,
                        Some(reason),
                    ),
                };
                let core = self.clone();
                self.handle_event(job, event, move |tx, current| {
                    mark(tx, current)?;
                    // The record of a dump the daemon asked for exists since
                    // then; one the helper made on its own (OS shutdown)
                    // replaces any older record, so its `paused` flag is
                    // the job's state now.
                    if current.state == JobState::Checkpointing && tx.get_checkpoint(job)?.is_some()
                    {
                        tx.set_checkpoint_status(job, status, size, reason.as_deref())
                    } else {
                        tx.put_checkpoint(&CheckpointRecord {
                            size_bytes: size,
                            status,
                            failure_reason: reason,
                            ..core.new_checkpoint_record(
                                current,
                                current.state == JobState::Paused,
                                now,
                            )
                        })
                    }
                })
                .await;
                if succeeded {
                    self.note_new_checkpoint(job).await;
                }
            }
            hp::Event::RestoreFinished { job, outcome } => {
                let event = match outcome {
                    RestoreOutcome::Success { .. } => E::RestoreSucceeded,
                    RestoreOutcome::Failed {
                        code: Some(HelperErrorCode::ShuttingDown),
                        reason,
                    } => {
                        // A stopping helper abandoned the restore before
                        // anything ran: queue it again rather than report a
                        // failure. (A restore already given up as
                        // interrupted stays as it is.)
                        tracing::info!(job = %job, "restore abandoned by the helper ({reason}); requeueing it");
                        if let Err(e) = self.requeue_unstarted(job, None, JobState::Restoring).await
                        {
                            tracing::warn!(job = %job, "cannot requeue the job: {}", e.0.message);
                        }
                        return;
                    }
                    RestoreOutcome::Failed { reason, .. } => E::RestoreFailed { reason },
                };
                self.handle_event(job, event, mark).await;
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
    /// - `Terminating` jobs get their SIGTERM/SIGKILL sequence re-armed;
    /// - `Checkpointing` jobs the helper runs but does not dump lost their
    ///   checkpoint request (see [`Core::abandon_lost_checkpoint`]).
    ///
    /// A job whose spawn or restore request is in flight, or was answered
    /// since the previous reconciliation, may be missing from `running`
    /// only because that list was taken before the request reached the
    /// helper; it is checked again once the request has settled.
    pub async fn reconcile(self: &Arc<Self>, running: Vec<RunningJob>) {
        let live: HashMap<JobId, hp::RunningPhase> =
            running.iter().map(|r| (r.job, r.phase)).collect();
        let jobs = match self.read(|s| s.list_jobs(&JobFilter::all())).await {
            Ok(jobs) => jobs,
            Err(e) => {
                tracing::error!("cannot reconcile with the helper: {}", e.0.message);
                return;
            }
        };
        let jobs: Vec<Job> = jobs
            .into_iter()
            .filter(|j| j.state.occupies_slot())
            .collect();
        // Which jobs were launched around the reconnect is decided before
        // anything is awaited: a launch is either still in flight here or,
        // having settled since, among the settled ones taken right after.
        // (Taken any earlier, a launch settling in between would count as
        // neither, and its job be marked lost while it runs.)
        let in_flight: HashSet<JobId> = jobs
            .iter()
            .map(|j| j.id)
            .filter(|id| self.launches.in_flight(*id))
            .collect();
        let recently_launched = self.launches.take_settled();
        let mut accounted = HashSet::new();
        for job in &jobs {
            accounted.insert(job.id);
            match live.get(&job.id) {
                None if in_flight.contains(&job.id) || recently_launched.contains(&job.id) => {
                    tracing::debug!(job = %job.id, "job was launched around the reconnect; checking again");
                    self.recheck_after_launch(job.id, job.started_at);
                }
                None => {
                    tracing::warn!(job = %job.id, state = job.state.name(), "job is gone; marking it lost");
                    self.handle_event(job.id, lifecycle::Event::Lost, no_pre)
                        .await;
                }
                Some(_) if matches!(job.state, JobState::Terminating { .. }) => {
                    if let Err(e) = self.terminate(job).await {
                        tracing::warn!(job = %job.id, "cannot stop job: {}", e.0.message);
                    }
                }
                Some(&phase @ (hp::RunningPhase::Running | hp::RunningPhase::Stopped))
                    if job.state == JobState::Checkpointing =>
                {
                    self.abandon_lost_checkpoint(job.id, phase).await;
                }
                Some(_) => {}
            }
        }
        let live: HashSet<JobId> = live.into_keys().collect();
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

    /// Once the launch of `id` has settled, marks the job lost unless the
    /// helper knows it (see [`Core::reconcile`]).
    fn recheck_after_launch(self: &Arc<Self>, id: JobId, started_at: Option<Timestamp>) {
        let core = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = core.cancel.cancelled() => return,
                _ = core.launches.wait(id) => {}
            }
            core.spawn_lost_check(id, started_at);
        });
    }

    /// Applies helper event `seq` (`None`: undecodable, only recorded) and
    /// records it as applied.
    ///
    /// The helper replays the events the daemon has not acknowledged after
    /// every reconnect, and the daemon acknowledges an event only after
    /// applying it, so a replay may repeat an event applied already. During
    /// a replay (`replaying`: between a (re)connect and its
    /// [`HelperNotice::Connected`]) events up to the last one applied are
    /// therefore skipped. Applying one again would be wrong for some, e.g.
    /// a refused dump of a job that has been running since would mark it
    /// as terminating.
    ///
    /// The helper keeps its sequence numbers across restarts. Should a live
    /// event still come with a number not above the last one applied (the
    /// helper's state was reset), the numbering starts over from it.
    async fn apply_helper_event(
        self: &Arc<Self>,
        seq: u64,
        event: Option<hp::Event>,
        replaying: bool,
    ) {
        let last = match self.read(|s| s.helper_event_seq()).await {
            Ok(last) => last,
            Err(e) => {
                tracing::error!("cannot read the last helper event applied: {}", e.0.message);
                0
            }
        };
        if seq <= last {
            if replaying {
                tracing::debug!(seq, "skipping a replayed helper event applied already");
                return;
            }
            tracing::warn!(
                seq,
                last,
                "the helper's event numbering went back (was its state reset?); following it"
            );
        }
        if let Some(event) = event {
            self.process_helper_event(event, seq).await;
        }
        // Also when the event did not apply to the job (any more).
        if let Err(e) = self
            .write(move |tx, _| Ok(tx.set_helper_event_seq(seq)?))
            .await
        {
            tracing::error!(seq, "cannot record the helper event: {}", e.0.message);
        }
    }

    /// Consumes helper notices until the daemon stops.
    pub async fn notice_loop(self: Arc<Self>, mut notices: mpsc::UnboundedReceiver<HelperNotice>) {
        // Events before the first `Connected` are a replay, too.
        let mut replaying = true;
        loop {
            let notice = tokio::select! {
                _ = self.cancel.cancelled() => return,
                notice = notices.recv() => notice,
            };
            let Some(notice) = notice else { return };
            match notice {
                HelperNotice::Event(envelope) => {
                    self.apply_helper_event(envelope.seq.0, Some(envelope.event), replaying)
                        .await;
                    if let Err(e) = self.helper.ack_events(envelope.seq).await {
                        tracing::debug!("cannot acknowledge helper events: {e}");
                    }
                }
                HelperNotice::UndecodableEvent { seq, error } => {
                    tracing::warn!(
                        seq = seq.0,
                        "skipping a helper event this daemon cannot decode \
                         (is the helper newer than the daemon?): {error}"
                    );
                    self.apply_helper_event(seq.0, None, replaying).await;
                    if let Err(e) = self.helper.ack_events(seq).await {
                        tracing::debug!("cannot acknowledge helper events: {e}");
                    }
                }
                HelperNotice::Connected { version, running } => {
                    replaying = false;
                    tracing::info!(%version, live = running.len(), "reconciling with the helper");
                    self.reconcile(running).await;
                }
                HelperNotice::Disconnected => {
                    replaying = true;
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
                let mut jobs = tx.list_jobs(&JobFilter::all())?;
                let groups = tx.list_groups()?;
                let mut launched = Vec::new();
                // Forced launches first, ignoring slots; they still take
                // theirs, which the plan below accounts for.
                let forced: HashSet<JobId> = {
                    let mut forced = core.forced.lock().unwrap();
                    forced.retain(|id| {
                        jobs.iter()
                            .any(|j| j.id == *id && is_launch_state(&j.state))
                    });
                    forced.clone()
                };
                if launch {
                    for job in jobs.iter_mut().filter(|j| forced.contains(&j.id)) {
                        let queued = matches!(
                            job.state,
                            JobState::Queued { .. } | JobState::RestoreQueued { .. }
                        );
                        if !queued || job.spec.gpus.iter().any(|g| !core.gpu_available(*g)) {
                            continue;
                        }
                        let t = lifecycle::dispatch(job)?;
                        core.apply_in_tx(tx, job, &t, now, ch)?;
                        launched.push((job.clone(), t.effects));
                    }
                }
                // Undetected GPUs are left out, so jobs that need them wait.
                let gpus: Vec<_> = tx
                    .list_gpus()?
                    .into_iter()
                    .filter(|gpu| core.gpu_available(gpu.index))
                    .collect();
                let plan = scheduler::plan(now, &jobs, &groups, &gpus);
                let mut by_id: HashMap<JobId, Job> = jobs.into_iter().map(|j| (j.id, j)).collect();
                for id in &plan.enqueue {
                    let Some(job) = by_id.get_mut(id) else {
                        continue;
                    };
                    if let Some(t) = lifecycle::enqueue_if_due(job, now) {
                        core.apply_in_tx(tx, job, &t, now, ch)?;
                    }
                }
                for id in &plan.dependency_failed {
                    // A forced start ignores dependencies.
                    if forced.contains(id) {
                        continue;
                    }
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
        // Each launch runs on its own task, so one slow helper answer does
        // not hold up the next scheduling round; at most
        // `MAX_CONCURRENT_LAUNCHES` are sent at a time. They stop with the
        // daemon; a request dropped unanswered is settled by the next
        // reconciliation (the launch tracker settles it on drop).
        for (job, effects) in launched {
            let core = self.clone();
            tokio::spawn(async move {
                // Biased: once the daemon is stopped, a launch that has not
                // been answered yet is dropped at its next wake-up instead
                // of acting on the outcome.
                tokio::select! {
                    biased;
                    _ = core.cancel.cancelled() => {}
                    _ = async {
                        let _permit = core.launch_slots.acquire().await;
                        let _ = core.run_effects(&job, &effects).await;
                    } => {}
                }
            });
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
    ///
    /// A job whose dump fails is terminated (as for `checkpoint`). A job
    /// whose dump the helper refuses before touching it (see
    /// [`refusal_reason`]) is left running: terminating it would lose the
    /// work the refusal protects. Its owner is warned, and the next daemon
    /// finds it running again.
    pub async fn shutdown(self: &Arc<Self>, no_checkpoint: bool) {
        if self.shutting_down.swap(true, Ordering::SeqCst) {
            return;
        }
        tracing::info!(no_checkpoint, "shutting down");
        let checkpoint_wait = Duration::from_secs(30 * 60);
        let settle_wait = Duration::from_secs(self.config.grace_secs + 10);
        let mut left_running: HashSet<JobId> = HashSet::new();
        for _pass in 0..3 {
            // New launches are refused from now on; let the ones in flight
            // be answered so the jobs can be signalled or checkpointed.
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                _ = self.launches.wait_idle() => {}
                _ = tokio::time::sleep(LAUNCH_WAIT) => {
                    tracing::warn!("the helper has not answered every spawn or restore request");
                }
            }
            let jobs = match self.read(|s| s.list_jobs(&JobFilter::all())).await {
                Ok(jobs) => jobs,
                Err(_) => break,
            };
            for job in jobs {
                if left_running.contains(&job.id) {
                    continue;
                }
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
                            if e.code() == ErrorCode::InvalidState {
                                left_running.insert(id);
                                tracing::warn!(
                                    job = %id,
                                    "{} (stop it with `puzuz kill` or \
                                     `puzuz shutdown --no-checkpoint`)",
                                    e.0.message
                                );
                            } else {
                                tracing::warn!(job = %id, "cannot checkpoint job: {}", e.0.message);
                            }
                            continue;
                        }
                        // One dump at a time.
                        self.wait_until(checkpoint_wait, |jobs| {
                            jobs.iter()
                                .find(|j| j.id == id)
                                .is_none_or(|j| j.state != JobState::Checkpointing)
                        })
                        .await;
                        // Back to running: refused once the dump was due.
                        if let Ok(Some(job)) = self.read(move |s| s.get_job(id)).await
                            && matches!(job.state, JobState::Running | JobState::Paused)
                        {
                            left_running.insert(id);
                            tracing::warn!(
                                job = %id,
                                "the helper refused to checkpoint the job; leaving it running"
                            );
                        }
                    }
                }
            }
            let settled = self
                .wait_until(settle_wait, |jobs| {
                    jobs.iter()
                        .all(|j| !j.state.occupies_slot() || left_running.contains(&j.id))
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

#[cfg(test)]
mod tests {
    use puzuz_core::model::JobSpec;

    use super::*;
    use crate::helper::FakeHelper;
    use crate::users::StaticUserDb;

    fn core(limit: Option<u64>, gpus: Vec<DetectedGpu>) -> Arc<Core> {
        core_with(FakeHelper::new(), limit, gpus)
    }

    fn core_with(helper: FakeHelper, limit: Option<u64>, gpus: Vec<DetectedGpu>) -> Arc<Core> {
        let config = Config {
            checkpoint_size_limit_bytes: limit,
            ..Config::default()
        };
        Core::new(
            config,
            Store::open_in_memory().unwrap(),
            Arc::new(helper),
            Arc::new(StaticUserDb::with_admins([])),
            gpus.into_iter().map(|g| (g.index, g)).collect(),
            GpuDetectionStatus {
                mode: crate::config::GpuDetection::Auto,
                source: puzuz_protocol::api::GpuSource::Config,
                nvml_error: None,
                gpus: Vec::new(),
            },
            false,
        )
    }

    fn spec() -> JobSpec {
        JobSpec {
            command: "x".to_owned(),
            cwd: PathBuf::from("/"),
            envs: Default::default(),
            group: "default".to_owned(),
            gpus: Vec::new(),
            dependencies: Vec::new(),
            priority: 0,
            label: None,
            no_checkpoint: false,
        }
    }

    async fn add_image(core: &Core, size: u64, status: CheckpointStatus) {
        core.db
            .call(move |store| {
                let job = store
                    .insert_job(Uid(1), &spec(), &JobState::Checkpointed, 0)
                    .unwrap();
                store
                    .put_checkpoint(&CheckpointRecord {
                        job_id: job.id,
                        path: PathBuf::from("/x"),
                        size_bytes: Some(size),
                        gpu_model: None,
                        driver_version: None,
                        created_at: 0,
                        status,
                        failure_reason: None,
                        paused: false,
                    })
                    .unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn size_limit_counts_complete_images_only() {
        let over = core(Some(1000), Vec::new());
        add_image(&over, 600, CheckpointStatus::Complete).await;
        assert_eq!(over.check_checkpoint_usage().await, None);
        add_image(&over, 5000, CheckpointStatus::Failed).await;
        add_image(&over, 5000, CheckpointStatus::InProgress).await;
        assert_eq!(over.check_checkpoint_usage().await, None);
        add_image(&over, 500, CheckpointStatus::Complete).await;
        assert_eq!(over.checkpoint_usage().await.unwrap(), (2, 1100));
        assert_eq!(
            over.check_checkpoint_usage().await,
            Some(crate::usage::OverLimit {
                total_bytes: 1100,
                limit_bytes: 1000
            })
        );
        let storage = over.checkpoint_storage().await.unwrap();
        assert_eq!(storage.images, 2);
        assert_eq!(storage.total_bytes, 1100);
        assert_eq!(storage.limit_bytes, Some(1000));
        assert!(storage.over_limit());

        let unlimited = core(None, Vec::new());
        add_image(&unlimited, i64::MAX as u64, CheckpointStatus::Complete).await;
        assert_eq!(unlimited.check_checkpoint_usage().await, None);
        assert!(!unlimited.checkpoint_storage().await.unwrap().over_limit());
    }

    /// A launch that settles while `reconcile` reads the database counts as
    /// launched around the reconnect (the helper's list may predate it),
    /// not as a job gone missing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn a_launch_settling_during_reconciliation_is_not_lost() {
        let helper = FakeHelper::new();
        let core = core_with(helper.clone(), None, Vec::new());
        let _notices = core.helper.connect();
        let id = core
            .db
            .call(|s| s.insert_job(Uid(1), &spec(), &JobState::Running, 0))
            .await
            .unwrap()
            .id;
        // Its spawn request is in flight; the helper's list was taken
        // before the spawn reached it.
        core.launches.begin(id);
        helper.insert_running(id);

        let db = core.db.0.clone();
        let lock = db.lock().unwrap();
        let reconcile = tokio::spawn({
            let core = core.clone();
            async move { core.reconcile(Vec::new()).await }
        });
        // Reconciliation waits for the database; meanwhile the spawn is
        // answered.
        tokio::time::sleep(Duration::from_millis(200)).await;
        core.launches.settle_on_drop(id).finish(LaunchOutcome::Done);
        drop(lock);
        reconcile.await.unwrap();

        let state = |core: Arc<Core>| async move {
            core.db
                .call(move |s| s.get_job(id).unwrap().unwrap().state)
                .await
        };
        assert_eq!(state(core.clone()).await, JobState::Running);
        // Checked again with the helper, which runs it.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(state(core.clone()).await, JobState::Running);
        core.cancel.cancel();
    }

    #[test]
    fn gpu_identity_lists_distinct_models_of_detected_gpus() {
        let gpu = |index: u32, name: &str| DetectedGpu {
            index: GpuIndex(index),
            uuid: format!("GPU-{index}"),
            name: name.to_owned(),
            memory_total_bytes: None,
            driver_version: Some("570.86.10".to_owned()),
        };
        let core = core(None, vec![gpu(0, "A100"), gpu(1, "A100"), gpu(2, "H100")]);
        assert_eq!(core.gpu_identity(&[]), (None, None));
        assert_eq!(
            core.gpu_identity(&[GpuIndex(2), GpuIndex(0), GpuIndex(1)]),
            (Some("H100, A100".to_owned()), Some("570.86.10".to_owned()))
        );
        // Undetected GPUs contribute nothing.
        assert_eq!(core.gpu_identity(&[GpuIndex(9)]), (None, None));
        assert!(core.gpu_available(GpuIndex(1)));
        assert!(!core.gpu_available(GpuIndex(9)));
    }
}
