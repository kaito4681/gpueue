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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
    CheckpointRecord, CheckpointStatus, JobError, JobErrorKind, Store, StoreOps, Tx,
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
use crate::limits::{MAX_STREAMS_PER_UID, UidLimiter};
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
    /// Why the job, a forced launch, waits in line (see
    /// [`Core::launch_wait`]).
    pub waiting: Option<scheduler::LaunchWait>,
}

impl JobInfo {
    pub fn load(store: &impl StoreOps, job: Job) -> Result<Self, StorageError> {
        let checkpoint = store.get_checkpoint(job.id)?;
        let error = store.get_job_error(job.id)?;
        Ok(Self {
            job,
            checkpoint,
            error,
            waiting: None,
        })
    }

    /// The view `caller` may see, with checkpoint information and the last
    /// error (owner and administrators only) filled in.
    pub fn view(&self, caller: &Caller) -> JobView {
        let mut view = JobView::for_caller(&self.job, caller);
        view.waiting = self.waiting.clone();
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

/// The warnings a caller has not been shown yet (see
/// [`Core::pending_warnings`]), with what identifies the failures among
/// them, so that acknowledging them cannot swallow a newer one.
#[derive(Debug, Default)]
pub struct PendingWarnings {
    pub warnings: Vec<Warning>,
    /// Failed dumps: job and the `created_at` of its checkpoint record.
    dumps: Vec<(JobId, Timestamp)>,
    /// Failed restores: job and the `recorded_at` of its error.
    errors: Vec<(JobId, Timestamp)>,
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
    /// Jobs whose checkpoint request is sent after the commit.
    checkpointing: Vec<JobId>,
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
    fn checkpointing(&mut self, id: JobId) {
        self.checkpointing.push(id);
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

/// How far a forced launch (see [`Core::force_launch`]) may ignore limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Force {
    /// An administrator's: launched right away, ignoring the group's and
    /// the GPUs' slots and a paused group.
    IgnoreLimits,
    /// Anybody else's: ahead of the queue and ignoring the group's
    /// `parallel` limit (and dependencies), but waiting for a free slot on
    /// its GPUs and for its group to be resumed (see
    /// [`scheduler::Reserved::forced`]).
    JumpQueue,
}

impl Force {
    /// The force `caller`'s launching actions have.
    pub fn of(caller: &Caller) -> Self {
        if caller.is_admin {
            Force::IgnoreLimits
        } else {
            Force::JumpQueue
        }
    }
}

/// Swaps the entries of `a` and `b` in `map`.
fn swap_entries<V>(map: &mut HashMap<JobId, V>, a: JobId, b: JobId) {
    let (va, vb) = (map.remove(&a), map.remove(&b));
    if let Some(v) = va {
        map.insert(b, v);
    }
    if let Some(v) = vb {
        map.insert(a, v);
    }
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

/// How long the scheduler launches nothing after a helper refused (or
/// abandoned) a launch because it is stopping (see [`Core::pause_launches`]).
const STOPPING_HELPER_PAUSE: Duration = Duration::from_secs(1);

/// Spawn/restore requests the scheduler has outstanding at most.
const MAX_CONCURRENT_LAUNCHES: usize = 8;

/// `log --tail` scans running at once at most (see `api::log`).
const MAX_TAIL_SCANS: usize = 2;

/// How long a check that found a job gone from the helper waits for the
/// helper events received before that answer to be applied (see
/// [`Core::spawn_lost_check`]).
const EVENT_CATCH_UP_WAIT: Duration = Duration::from_secs(30);

/// A job the helper runs that the database does not account for: it has
/// it finished, queued, checkpointed, or not at all (e.g. after restoring
/// the database from a backup). It keeps its GPU slots taken, a queued job
/// with its id is not launched, administrators see it in `status` and
/// `doctor`, and may stop it with `kill`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub job: JobId,
    pub owner: Option<Uid>,
    /// As reported by the helper; from the database for older helpers.
    pub gpus: Vec<GpuIndex>,
    pub pid: Option<u32>,
}

impl Orphan {
    pub fn view(&self) -> api::OrphanJob {
        api::OrphanJob {
            id: self.job,
            owner: self.owner,
            gpus: self.gpus.clone(),
            pid: self.pid,
        }
    }
}

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
    /// `config.priority` as the priority calculation takes it.
    pub priority_params: puzuz_core::priority::Params,
    /// CPUs of the machine (for the job size and TRES factors).
    pub cpus: u32,
    /// Fair-share factor per user (see [`crate::priority`]).
    pub(crate) fairshare: Mutex<Arc<HashMap<Uid, f64>>>,
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
    /// Live helper jobs the database does not account for (see
    /// [`Orphan`]).
    orphans: Mutex<BTreeMap<JobId, Orphan>>,
    /// How many helper event notices the notice loop has handled (see
    /// [`Helper::events_delivered`]).
    events_applied: watch::Sender<u64>,
    /// Bounds the concurrent backward scans of `log --tail` (see
    /// `api::log`), which run on the blocking pool shared with the
    /// database.
    pub(crate) tail_scans: Semaphore,
    /// Open `log --follow` and `/v1/events` streams per caller (see
    /// [`crate::limits`]).
    pub(crate) streams: Arc<UidLimiter>,
    /// Spawn and restore requests the helper has not answered yet.
    launches: Launches,
    /// Checkpoint requests the helper has not answered yet (only
    /// [`Launches::in_flight`], [`Launches::take_settled`] and
    /// [`Launches::wait`] are used): see [`Core::reconcile`].
    checkpoint_requests: Launches,
    /// Bounds the scheduler's concurrent spawn/restore requests.
    launch_slots: Semaphore,
    /// Jobs whose spawn or restore request went unanswered (connection
    /// lost or timed out), with the state that launch put them in
    /// (`Running` or `Restoring`); see [`Core::spawn_lost_check`].
    unconfirmed_launches: Mutex<HashMap<JobId, JobState>>,
    /// Jobs to launch ahead of the queue (`start`, `add --immediate`,
    /// `restore --immediate`, `restart --immediate`) and how far they may
    /// ignore limits ([`Force`]), kept until they leave the queued, running
    /// and restoring states. While one is queued (the helper was not
    /// connected or did not start it, or a non-administrator's launch waits
    /// for a GPU slot or a paused group), the scheduler launches it as soon
    /// as it may. Kept in memory only: after a daemon restart such a job
    /// waits in line like any other.
    forced: Mutex<HashMap<JobId, Force>>,
    /// Why forced launches that wait in line cannot launch yet, as of the
    /// last scheduling round (shown by `status`).
    launch_waits: Mutex<HashMap<JobId, scheduler::LaunchWait>>,
    /// See [`LaunchCheckHook`].
    launch_check_hook: Mutex<Option<LaunchCheckHook>>,
    /// The scheduler launches nothing before this instant (see
    /// [`Core::pause_launches`]).
    launches_paused_until: Mutex<Option<tokio::time::Instant>>,
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
            priority_params: config.priority.params(),
            cpus: crate::priority::machine_cpus(),
            fairshare: Mutex::new(Arc::new(HashMap::new())),
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
            orphans: Mutex::new(BTreeMap::new()),
            events_applied: watch::channel(0).0,
            tail_scans: Semaphore::new(MAX_TAIL_SCANS),
            streams: UidLimiter::new(MAX_STREAMS_PER_UID, usize::MAX),
            launches: Launches::default(),
            checkpoint_requests: Launches::default(),
            launch_slots: Semaphore::new(MAX_CONCURRENT_LAUNCHES),
            unconfirmed_launches: Mutex::new(HashMap::new()),
            forced: Mutex::new(HashMap::new()),
            launch_waits: Mutex::new(HashMap::new()),
            launch_check_hook: Mutex::new(None),
            launches_paused_until: Mutex::new(None),
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

    /// Marks `id` to be launched ahead of the queue, ignoring the limits
    /// `force` allows (see [`Core::forced`]).
    pub fn force_launch(&self, id: JobId, force: Force) {
        self.forced.lock().unwrap().insert(id, force);
    }

    /// The forced launches that wait for GPU slots and paused groups
    /// ([`Force::JumpQueue`]).
    pub(crate) fn queue_jumping_launches(&self) -> HashSet<JobId> {
        self.forced
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, force)| **force == Force::JumpQueue)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Records why forced launch `id` waits in line (see
    /// [`Core::launch_waits`]).
    pub(crate) fn set_launch_wait(&self, id: JobId, wait: scheduler::LaunchWait) {
        self.launch_waits.lock().unwrap().insert(id, wait);
    }

    /// Why forced launch `id` waits in line, if it does.
    pub fn launch_wait(&self, id: JobId) -> Option<scheduler::LaunchWait> {
        self.launch_waits.lock().unwrap().get(&id).cloned()
    }

    /// A helper refused or abandoned a launch because it is stopping: the
    /// job goes back in line right away (so that it is not shown as running
    /// while nothing runs), and the scheduler launches nothing for
    /// [`STOPPING_HELPER_PAUSE`], until that helper is likely gone, rather
    /// than offering the job to it again in a tight loop. Every job is
    /// paused, not just this one: the stopping helper would refuse the
    /// others too, and a lower-priority job must not take the slot of the
    /// one put back in line.
    fn pause_launches(self: &Arc<Self>) {
        let until = tokio::time::Instant::now() + STOPPING_HELPER_PAUSE;
        {
            let mut paused = self.launches_paused_until.lock().unwrap();
            *paused = Some(paused.map_or(until, |p| p.max(until)));
        }
        let core = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = core.cancel.cancelled() => {}
                _ = tokio::time::sleep_until(until) => core.wake_scheduler(),
            }
        });
    }

    /// Whether launches are paused (see [`Core::pause_launches`]).
    fn launches_paused(&self) -> bool {
        self.launches_paused_until
            .lock()
            .unwrap()
            .is_some_and(|until| tokio::time::Instant::now() < until)
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
                        if waiting && core.forced.lock().unwrap().contains_key(&id) {
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

    /// The live helper jobs the database does not account for, by id.
    pub fn orphans(&self) -> Vec<Orphan> {
        self.orphans.lock().unwrap().values().cloned().collect()
    }

    fn is_orphan(&self, id: JobId) -> bool {
        self.orphans.lock().unwrap().contains_key(&id)
    }

    /// Forgets orphan `id` (its process ended); returns whether it was one.
    fn forget_orphan(&self, id: JobId) -> bool {
        let removed = self.orphans.lock().unwrap().remove(&id).is_some();
        if removed {
            tracing::info!(job = %id, "the orphaned job has ended");
            self.wake_scheduler();
        }
        removed
    }

    /// GPU slots taken and job ids held by orphans, for scheduling.
    pub(crate) fn orphan_reservations(&self) -> scheduler::Reserved {
        let mut reserved = scheduler::Reserved::default();
        for orphan in self.orphans.lock().unwrap().values() {
            reserved.held.insert(orphan.job);
            let mut seen = HashSet::new();
            for gpu in orphan.gpus.iter().filter(|g| seen.insert(**g)) {
                *reserved.gpus.entry(*gpu).or_default() += 1;
            }
        }
        reserved
    }

    /// `kill` of orphan `id` by an administrator: `signal`, or SIGTERM and
    /// SIGKILL after the grace period (as for jobs). Returns `false` if `id`
    /// is no orphan.
    pub async fn kill_orphan(self: &Arc<Self>, id: JobId, sig: Option<i32>) -> ApiResult<bool> {
        if !self.is_orphan(id) {
            return Ok(false);
        }
        if !self.helper_ready() {
            return Err(Self::helper_unavailable());
        }
        tracing::warn!(job = %id, signal = ?sig, "killing an orphaned job");
        match self.helper.signal(id, sig.unwrap_or(signal::SIGTERM)).await {
            Ok(()) => {}
            Err(HelperError::Rejected {
                code: HelperErrorCode::JobNotFound,
                ..
            }) => {
                self.forget_orphan(id);
                return Ok(true);
            }
            Err(e) => return Err(e.into()),
        }
        if sig.is_none() {
            let core = self.clone();
            let grace = Duration::from_secs(self.config.grace_secs);
            tokio::spawn(async move {
                tokio::select! {
                    _ = core.cancel.cancelled() => return,
                    _ = tokio::time::sleep(grace) => {}
                }
                if core.is_orphan(id)
                    && let Err(HelperError::Rejected {
                        code: HelperErrorCode::JobNotFound,
                        ..
                    }) = core.helper.signal(id, signal::SIGKILL).await
                {
                    core.forget_orphan(id);
                }
            });
        }
        Ok(true)
    }

    /// Swaps what is kept in memory under the ids of two jobs `switch`
    /// swaps (inside the transaction that swaps them in the database): the
    /// "launch ahead of the queue" mark, why it waits, and the launch
    /// records.
    pub(crate) fn swap_in_memory(&self, a: JobId, b: JobId) {
        swap_entries(&mut self.forced.lock().unwrap(), a, b);
        swap_entries(&mut self.launch_waits.lock().unwrap(), a, b);
        self.launches.swap(a, b);
    }

    /// Waits until the notice loop has handled `count` helper event notices
    /// (see [`Helper::events_delivered`]); `false` if it has not by
    /// [`EVENT_CATCH_UP_WAIT`] or the daemon stops.
    async fn events_applied_up_to(&self, count: u64) -> bool {
        let mut applied = self.events_applied.subscribe();
        tokio::select! {
            result = applied.wait_for(|n| *n >= count) => result.is_ok(),
            _ = self.cancel.cancelled() => false,
            _ = tokio::time::sleep(EVENT_CATCH_UP_WAIT) => false,
        }
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
    pub async fn pending_warnings(&self, owner: Uid) -> ApiResult<PendingWarnings> {
        let (dumps, errors) = self
            .read(move |s| {
                Ok((
                    s.unacknowledged_checkpoint_failures(owner)?,
                    s.unacknowledged_job_errors(owner)?,
                ))
            })
            .await?;
        let mut warnings: Vec<Warning> = dumps
            .iter()
            .map(|&(job, _)| Warning::CheckpointFailed { job })
            .chain(errors.iter().map(|error| match error.kind {
                JobErrorKind::Restore => Warning::RestoreFailed { job: error.job_id },
            }))
            .collect();
        warnings.sort_by_key(Warning::job);
        warnings.extend(self.pending_size_warnings(owner).await);
        Ok(PendingWarnings {
            warnings,
            dumps,
            errors: errors
                .into_iter()
                .map(|error| (error.job_id, error.recorded_at))
                .collect(),
        })
    }

    /// Records that `pending` has been shown to `owner`: exactly the
    /// failures that were read, so one recorded since (a newer failed dump
    /// or restore of the same job) is still reported.
    pub async fn acknowledge_warnings(
        &self,
        owner: Uid,
        pending: &PendingWarnings,
    ) -> ApiResult<()> {
        self.acknowledge_size_warnings(owner, &pending.warnings);
        let dumps = pending.dumps.clone();
        let errors = pending.errors.clone();
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
                    for &id in &changes.checkpointing {
                        core.checkpoint_requests.begin(id);
                    }
                    for &id in &changes.removed {
                        core.launches.forget(id);
                        core.checkpoint_requests.forget(id);
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
            Ok(Some(job)) => JobInfo::load(store, job).ok().map(|mut info| {
                info.waiting = self.launch_wait(id);
                Arc::new(info)
            }),
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
            let old = job.state.clone();
            transition.apply(job, now);
            tx.update_job_state(job.id, &job.state, job.started_at, job.finished_at)?;
            self.account_run(tx, &old, job, now)?;
            if matches!(old, JobState::Done { .. })
                && matches!(
                    job.state,
                    JobState::Queued { .. } | JobState::Stashed { .. }
                )
            {
                // `restart --in-place`: like a restarted copy, the job
                // starts without an administrator's grants (a negative
                // nice, the site factor) and its `top` position.
                tx.clear_job_priority_grants(job.id)?;
                job.spec.nice = job.spec.nice.max(0);
                job.site_factor = 0;
                job.top_seq = None;
            }
            if !is_launch_state(&job.state) {
                self.forced.lock().unwrap().remove(&job.id);
            }
            if !matches!(
                job.state,
                JobState::Queued { .. } | JobState::RestoreQueued { .. }
            ) {
                self.launch_waits.lock().unwrap().remove(&job.id);
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
                    changes.checkpointing(job.id);
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
                    // A negative nice is an administrator's grant to
                    // this job; the copy does not inherit it.
                    let mut spec = job.spec.clone();
                    spec.nice = spec.nice.max(0);
                    let new = tx.insert_job(job.owner, &spec, &state, now)?;
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
            Effect::Checkpoint => {
                let _settle = self.checkpoint_requests.settle_on_drop(job.id);
                self.start_checkpoint(job).await
            }
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
                // start). Not the job's fault; put it back in line now,
                // and offer it again once that helper is likely gone.
                tracing::info!(job = %job.id, "the helper is stopping; requeueing the job");
                self.pause_launches();
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
    /// that was to be launched ahead of the queue stays so.
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
            // Every event the helper sent before this answer, e.g. the
            // job's exit, has been received by now.
            let received = core.helper.events_delivered();
            if running.iter().any(|r| r.job == id) {
                core.unconfirmed_launches.lock().unwrap().remove(&id);
                return;
            }
            // Its exit (or restore outcome) may have been received but not
            // applied yet: the notice loop applies events one at a time.
            // Deciding before it has would record an exited job as lost
            // (and fail its dependants) or launch an exited job again.
            if !core.events_applied_up_to(received).await {
                tracing::warn!(job = %id, "helper events are not applied in time; checking the job anyway");
            }
            // An event the helper reported right around the list arrives
            // shortly; give it time too.
            tokio::select! {
                _ = core.cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            let unconfirmed = core.unconfirmed_launches.lock().unwrap().remove(&id);
            if let Some(launched) = unconfirmed {
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
            if !core.still_live(id, started_at).await {
                return;
            }
            tracing::warn!(job = %id, "the helper no longer knows the job; marking it lost");
            core.handle_event_in_run(id, lifecycle::Event::Lost, Some(started_at), no_pre)
                .await;
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
            let old = std::mem::replace(&mut job.state, next.clone());
            core.account_run(tx, &old, &job, now)?;
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
                let received = core.helper.events_delivered();
                match running.iter().find(|r| r.job == id).map(|r| r.phase) {
                    Some(phase @ (hp::RunningPhase::Running | hp::RunningPhase::Stopped)) => {
                        core.abandon_lost_checkpoint(id, phase).await;
                        return;
                    }
                    Some(_) => missing = false,
                    None if missing => {
                        // Its outcome may be received but not applied yet
                        // (see `Core::spawn_lost_check`).
                        core.events_applied_up_to(received).await;
                        let still_checkpointing = matches!(
                            core.read(move |s| s.get_job(id)).await,
                            Ok(Some(job)) if job.state == JobState::Checkpointing
                                && job.started_at == started_at
                        );
                        if still_checkpointing {
                            tracing::warn!(job = %id, "the helper no longer knows the job; marking it lost");
                            core.handle_event_in_run(
                                id,
                                lifecycle::Event::Lost,
                                Some(started_at),
                                no_pre,
                            )
                            .await;
                        }
                        return;
                    }
                    None => missing = true,
                }
            }
        });
    }

    async fn start_restore(self: &Arc<Self>, job: &Job) -> Result<(), LaunchFailure> {
        self.warn_if_gpus_changed(job).await;
        // A job paused when it was dumped is restored stopped: it does not
        // run before its owner resumes it (see `lifecycle::keep_paused`,
        // which still stops it if the helper resumed it after all).
        let id = job.id;
        let paused = match self.read(move |s| s.get_checkpoint(id)).await {
            Ok(record) => record.is_some_and(|r| r.paused),
            Err(e) => {
                tracing::warn!(job = %id, "cannot read the checkpoint record: {}", e.0.message);
                false
            }
        };
        let request = RestoreRequest {
            job: job.id,
            uid: job.owner,
            image_dir: self.image_dir(job.id),
            gpus: job.spec.gpus.clone(),
            leave_stopped: paused,
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
                // restored and it is not the job's fault; put the job back
                // in line, to be offered again once that helper is gone.
                tracing::info!(job = %job.id, "the helper is stopping; requeueing the restore");
                self.pause_launches();
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
        self.handle_event_in_run(id, event, None, pre).await
    }

    /// [`Core::handle_event`], only if the job is still in the run that
    /// began at `started_at` (when given).
    async fn handle_event_in_run<P>(
        self: &Arc<Self>,
        id: JobId,
        event: lifecycle::Event,
        started_at: Option<Option<Timestamp>>,
        pre: P,
    ) where
        P: FnOnce(&dyn StoreOps, &Job) -> Result<(), StorageError> + Send + 'static,
    {
        let core = self.clone();
        let now = now_ms();
        let result = self
            .write(move |tx, ch| {
                let Some(mut job) = tx.get_job(id)? else {
                    return Ok(None);
                };
                if started_at.is_some_and(|at| at != job.started_at) {
                    return Ok(None);
                }
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
        // An orphan's process is gone once it exited or was dumped (also
        // when the dump is not finalized yet).
        match &event {
            hp::Event::JobExited { job, .. }
            | hp::Event::CheckpointFinished {
                job,
                outcome:
                    CheckpointOutcome::Success { .. }
                    | CheckpointOutcome::Failed {
                        code: Some(HelperErrorCode::CheckpointUnfinalized),
                        ..
                    },
            } => {
                self.forget_orphan(*job);
            }
            _ => {}
        }
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
                    CheckpointOutcome::Failed {
                        reason,
                        code: Some(HelperErrorCode::CheckpointUnfinalized),
                    } => {
                        // The job is gone and its image kept; the helper
                        // finalizes it before the restore (which fails,
                        // keeping the image, if it still cannot).
                        tracing::warn!(job = %job, "checkpoint not finalized yet: {reason}");
                        (
                            E::CheckpointSucceeded,
                            CheckpointStatus::Complete,
                            None,
                            None,
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
                    RestoreOutcome::Success { stopped, .. } => E::RestoreSucceeded { stopped },
                    RestoreOutcome::Failed {
                        code: Some(HelperErrorCode::ShuttingDown),
                        reason,
                    } => {
                        // A stopping helper abandoned the restore before
                        // anything ran: queue it again rather than report a
                        // failure. (A restore already given up as
                        // interrupted stays as it is.)
                        tracing::info!(job = %job, "restore abandoned by the helper ({reason}); requeueing it");
                        self.pause_launches();
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
    /// - known to the helper but not live in the database → [`Orphan`]: its
    ///   GPU slots stay taken until it ends; it is not stopped by itself;
    /// - `Terminating` jobs get their SIGTERM/SIGKILL sequence re-armed;
    /// - `Checkpointing` jobs the helper runs but does not dump lost their
    ///   checkpoint request (see [`Core::abandon_lost_checkpoint`]).
    ///
    /// A job whose spawn or restore request is in flight, or was answered
    /// since the previous reconciliation, may be missing from `running`
    /// only because that list was taken before the request reached the
    /// helper; it is checked again once the request has settled. Likewise,
    /// a `Checkpointing` job whose checkpoint request is in flight, or was
    /// answered since, may be listed as running (or, its dump done, not at
    /// all) only because the list predates the request (or its outcome):
    /// it is left to [`Core::checkpoint_lost_check`] once the request has
    /// settled.
    pub async fn reconcile(self: &Arc<Self>, running: Vec<RunningJob>) {
        let live: HashMap<JobId, hp::RunningPhase> =
            running.iter().map(|r| (r.job, r.phase)).collect();
        let jobs = match self.read(|s| s.list_unfinished_jobs()).await {
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
        let checkpoint_in_flight: HashSet<JobId> = jobs
            .iter()
            .map(|j| j.id)
            .filter(|id| self.checkpoint_requests.in_flight(*id))
            .collect();
        let recently_checkpointed = self.checkpoint_requests.take_settled();
        let mut accounted = HashSet::new();
        for job in &jobs {
            accounted.insert(job.id);
            match live.get(&job.id) {
                None if in_flight.contains(&job.id) || recently_launched.contains(&job.id) => {
                    tracing::debug!(job = %job.id, "job was launched around the reconnect; checking again");
                    self.recheck_after_launch(job.id, job.started_at);
                }
                None | Some(hp::RunningPhase::Running | hp::RunningPhase::Stopped)
                    if job.state == JobState::Checkpointing
                        && (checkpoint_in_flight.contains(&job.id)
                            || recently_checkpointed.contains(&job.id)) =>
                {
                    tracing::debug!(job = %job.id, "job was asked to checkpoint around the reconnect; checking again");
                    self.recheck_after_checkpoint_request(job.id, job.started_at);
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
        let unaccounted: Vec<RunningJob> = running
            .into_iter()
            .filter(|r| !accounted.contains(&r.job))
            .collect();
        // A helper that predates reporting owner and GPUs: take them from
        // the database, if it still has the job.
        let ids: Vec<JobId> = unaccounted
            .iter()
            .filter(|r| r.uid.is_none() || r.gpus.is_none())
            .map(|r| r.job)
            .collect();
        let known: HashMap<JobId, Job> = self
            .read(move |s| {
                let mut known = HashMap::new();
                for id in ids {
                    if let Some(job) = s.get_job(id)? {
                        known.insert(id, job);
                    }
                }
                Ok(known)
            })
            .await
            .unwrap_or_default();
        let mut orphans = BTreeMap::new();
        for r in unaccounted {
            let job = known.get(&r.job);
            let orphan = Orphan {
                job: r.job,
                owner: r.uid.or(job.map(|j| j.owner)),
                gpus: r
                    .gpus
                    .or_else(|| job.map(|j| j.spec.gpus.clone()))
                    .unwrap_or_default(),
                pid: r.pid,
            };
            tracing::warn!(
                job = %orphan.job,
                owner = ?orphan.owner,
                gpus = ?orphan.gpus,
                "the helper runs a job the daemon does not account for; its GPU slots stay \
                 taken until it ends (an administrator can stop it with `puzuz kill`)"
            );
            orphans.insert(orphan.job, orphan);
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

    /// Once the checkpoint request for `id` has settled, checks what the
    /// helper does with the job (see [`Core::reconcile`]).
    fn recheck_after_checkpoint_request(
        self: &Arc<Self>,
        id: JobId,
        started_at: Option<Timestamp>,
    ) {
        let core = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = core.cancel.cancelled() => return,
                _ = core.checkpoint_requests.wait(id) => {}
            }
            core.checkpoint_lost_check(id, started_at);
        });
    }

    /// The sequence number of the last helper event applied (0 if none,
    /// or if it cannot be read).
    async fn last_helper_event(&self) -> u64 {
        match self.read(|s| s.helper_event_seq()).await {
            Ok(last) => last,
            Err(e) => {
                tracing::error!("cannot read the last helper event applied: {}", e.0.message);
                0
            }
        }
    }

    /// Applies helper event `seq` (`None`: undecodable, only recorded) and
    /// records it as applied.
    ///
    /// The helper keeps its sequence numbers across restarts. Should an
    /// event that is not a replayed duplicate (see
    /// [`Core::receive_helper_event`]) still come with a number not above
    /// the last one applied (the helper's state was reset), the numbering
    /// starts over from it.
    async fn apply_helper_event(self: &Arc<Self>, seq: u64, event: Option<hp::Event>) {
        let last = self.last_helper_event().await;
        if seq <= last {
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

    /// Counts `count` event notices as handled (see
    /// [`Helper::events_delivered`]).
    fn events_handled(&self, count: usize) {
        self.events_applied.send_modify(|n| *n += count as u64);
    }

    /// Acknowledges the helper's events up to `seq`.
    async fn ack_helper_events(&self, seq: hp::EventSeq) {
        if let Err(e) = self.helper.ack_events(seq).await {
            tracing::debug!("cannot acknowledge helper events: {e}");
        }
    }

    /// Handles helper event `seq` (`None`: undecodable): applies,
    /// counts and acknowledges it, unless it is held back in `held`.
    ///
    /// The helper replays the events the daemon has not acknowledged after
    /// every reconnect, and the daemon acknowledges an event only after
    /// applying it, so a replay may repeat events applied already. Applying
    /// one again would be wrong for some, e.g. a refused dump of a job that
    /// has been running since would mark it as terminating. During a replay
    /// (`replaying`: between a (re)connect and its
    /// [`HelperNotice::Connected`]) events up to the last one applied are
    /// therefore held back: they were applied already if the replay goes
    /// on past the last one applied, or ends there. A replay that ends
    /// below it comes from a helper whose numbering was reset, whose
    /// events are new (see [`Core::end_replay`]).
    async fn receive_helper_event(
        self: &Arc<Self>,
        seq: hp::EventSeq,
        event: Option<hp::Event>,
        replaying: bool,
        held: &mut Vec<(hp::EventSeq, Option<hp::Event>)>,
    ) {
        if replaying {
            if seq.0 <= self.last_helper_event().await {
                held.push((seq, event));
                return;
            }
            if !held.is_empty() {
                // Acknowledged with this event.
                tracing::debug!(
                    count = held.len(),
                    "skipping replayed helper events applied already"
                );
                self.events_handled(held.len());
                held.clear();
            }
        }
        self.apply_helper_event(seq.0, event).await;
        self.events_handled(1);
        self.ack_helper_events(seq).await;
    }

    /// Settles the events a replay held back (see
    /// [`Core::receive_helper_event`]) at its end: duplicates if the
    /// replay reached the last event applied, otherwise the helper's
    /// numbering was reset and they are applied.
    async fn end_replay(self: &Arc<Self>, held: &mut Vec<(hp::EventSeq, Option<hp::Event>)>) {
        let Some(&(up_to, _)) = held.last() else {
            return;
        };
        let last = self.last_helper_event().await;
        if up_to.0 >= last {
            tracing::debug!(
                count = held.len(),
                "skipping replayed helper events applied already"
            );
            self.events_handled(held.len());
            held.clear();
            self.ack_helper_events(up_to).await;
            return;
        }
        tracing::warn!(
            last,
            replayed_up_to = up_to.0,
            "the helper's event numbering was reset (was its state wiped?); applying its replayed events"
        );
        for (seq, event) in std::mem::take(held) {
            self.apply_helper_event(seq.0, event).await;
            self.events_handled(1);
            self.ack_helper_events(seq).await;
        }
    }

    /// Consumes helper notices until the daemon stops.
    pub async fn notice_loop(self: Arc<Self>, mut notices: mpsc::UnboundedReceiver<HelperNotice>) {
        // Events before the first `Connected` are a replay, too.
        let mut replaying = true;
        // See `Core::receive_helper_event`.
        let mut held = Vec::new();
        loop {
            let notice = tokio::select! {
                _ = self.cancel.cancelled() => return,
                notice = notices.recv() => notice,
            };
            let Some(notice) = notice else { return };
            match notice {
                HelperNotice::Event(envelope) => {
                    self.receive_helper_event(
                        envelope.seq,
                        Some(envelope.event),
                        replaying,
                        &mut held,
                    )
                    .await;
                }
                HelperNotice::UndecodableEvent { seq, error } => {
                    tracing::warn!(
                        seq = seq.0,
                        "skipping a helper event this daemon cannot decode \
                         (is the helper newer than the daemon?): {error}"
                    );
                    self.receive_helper_event(seq, None, replaying, &mut held)
                        .await;
                }
                HelperNotice::Connected { version, running } => {
                    replaying = false;
                    self.end_replay(&mut held).await;
                    tracing::info!(%version, live = running.len(), "reconciling with the helper");
                    self.reconcile(running).await;
                }
                HelperNotice::Disconnected => {
                    replaying = true;
                    // Not acknowledged: replayed again after the reconnect.
                    self.events_handled(held.len());
                    held.clear();
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
        let launch = !self.is_shutting_down() && self.helper_ready() && !self.launches_paused();
        let now = now_ms();
        let core = self.clone();
        let launched = self
            .write(move |tx, ch| {
                // Finished jobs only matter as dependencies.
                let mut jobs = tx.list_unfinished_jobs()?;
                let groups = tx.list_groups()?;
                let mut reserved = core.orphan_reservations();
                let mut launched = Vec::new();
                let forced: HashMap<JobId, Force> = {
                    let mut forced = core.forced.lock().unwrap();
                    forced.retain(|id, _| {
                        jobs.iter()
                            .any(|j| j.id == *id && is_launch_state(&j.state))
                    });
                    forced.clone()
                };
                // An administrator's forced launches first, ignoring every
                // limit; they still take their slots, which the plan below
                // accounts for.
                if launch {
                    for job in jobs
                        .iter_mut()
                        .filter(|j| forced.get(&j.id) == Some(&Force::IgnoreLimits))
                    {
                        let queued = matches!(
                            job.state,
                            JobState::Queued { .. } | JobState::RestoreQueued { .. }
                        );
                        if !queued
                            || reserved.held.contains(&job.id)
                            || job.spec.gpus.iter().any(|g| !core.gpu_available(*g))
                        {
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
                // Everybody else's go first in the plan, within GPU slots
                // and unpaused groups.
                reserved.forced = forced
                    .iter()
                    .filter(|(_, force)| **force == Force::JumpQueue)
                    .map(|(id, _)| *id)
                    .collect::<HashSet<_>>();
                let fairshare = core.fairshare_factors();
                let input = scheduler::PriorityInput {
                    params: &core.priority_params,
                    fairshare: &fairshare,
                    resources: core.priority_resources(gpus.len()),
                };
                let plan =
                    scheduler::plan_prioritized(now, &jobs, &groups, &gpus, &reserved, &input);
                {
                    let mut waits = core.launch_waits.lock().unwrap();
                    waits.clear();
                    if launch {
                        waits.extend(plan.waiting.iter().cloned());
                    }
                }
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
                    if forced.contains_key(id) {
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
            if let Ok(jobs) = self.read(|s| s.list_unfinished_jobs()).await
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
            let jobs = match self.read(|s| s.list_unfinished_jobs()).await {
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
            nice: 0,
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

    /// The helper no longer knows a job whose exit it has sent but the
    /// notice loop has not applied yet: the job is not marked lost (which
    /// would fail its dependants), its exit is applied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_exit_received_but_not_applied_yet_is_not_overridden_as_lost() {
        let helper = FakeHelper::new();
        let core = core_with(helper.clone(), None, Vec::new());
        let job = core
            .db
            .call(|s| s.insert_job(Uid(1), &spec(), &JobState::Running, 0))
            .await
            .unwrap();
        let id = job.id;
        helper.insert_running(id);
        let notices = core.helper.connect();
        // The job exits; the event is received, but the notice loop is busy
        // (here: not running yet).
        helper.exit(id, 0);
        assert_eq!(core.helper.events_delivered(), 1);
        // A signal found the job gone.
        core.spawn_lost_check(id, job.started_at);
        let state = |core: Arc<Core>| async move {
            core.db
                .call(move |s| s.get_job(id).unwrap().unwrap().state)
                .await
        };
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert_eq!(state(core.clone()).await, JobState::Running);

        // The notice loop catches up.
        tokio::spawn(core.clone().notice_loop(notices));
        let success = JobState::Done {
            result: JobResult::Success,
        };
        for _ in 0..100 {
            if state(core.clone()).await == success {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(state(core.clone()).await, success);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(state(core.clone()).await, success);
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
