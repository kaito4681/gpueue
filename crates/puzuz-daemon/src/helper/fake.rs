//! [`FakeHelper`]: an in-memory helper for tests.
//!
//! It keeps a table of "processes", answers requests like the real helper
//! and emits events with sequence numbers that stay queued until
//! acknowledged, so tests can simulate exits, checkpoint failures, helper
//! disconnects and daemon restarts. Clones share the same state, so one
//! instance can outlive a daemon and be handed to the next one.
//!
//! [`FakeHelper::hold_launches`] makes spawn and restore requests wait
//! (unanswered and without effect) until [`FakeHelper::release_launches`],
//! to reproduce the window in which the daemon has recorded a launch the
//! helper does not know about yet. Combined with
//! [`FakeHelper::refuse_launches`], it holds the daemon's retry of a launch
//! a stopping helper refused. [`FakeHelper::hold_checkpoints`] does the
//! same for checkpoint requests.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::BoxFuture;
use puzuz_core::model::{GpuIndex, JobId, Uid};
use puzuz_protocol::helper::{
    CheckpointOptions, CheckpointOutcome, Event, EventEnvelope, EventSeq, ExitStatus,
    HelperErrorCode, RestoreOutcome, RestoreRequest, RunningJob, RunningPhase, SpawnRequest,
    signal,
};
use tokio::sync::{mpsc, watch};

use super::{Helper, HelperError, HelperNotice, HelperResult, HelperState};

/// What a simulated checkpoint or restore does.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum FakeOutcome {
    #[default]
    Succeed,
    Fail(String),
    /// Checkpoints only: accept the request, then report the dump refused
    /// before the job was touched (`code: checkpoint_refused`), as the
    /// helper does when its pre-checks fail at dump time. The job keeps
    /// running.
    Refuse(String),
    /// Checkpoints only: accept the request and report nothing, as if the
    /// dump were still running (finish it with [`FakeHelper::dump`]).
    Hold,
    /// The request is lost on its way: it fails with
    /// [`HelperError::Timeout`] and nothing happens.
    LoseRequest,
}

/// A simulated live job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeJob {
    pub pid: u32,
    pub phase: RunningPhase,
}

#[derive(Debug, Default)]
struct State {
    connected: bool,
    notices: Option<mpsc::UnboundedSender<HelperNotice>>,
    jobs: BTreeMap<JobId, FakeJob>,
    images: BTreeSet<JobId>,
    next_pid: u32,
    next_seq: u64,
    unacked: Vec<EventEnvelope>,
    spawn_failure: Option<String>,
    /// Refuse spawns and restores as a stopping helper does.
    stopping: bool,
    /// Accept restores, then report them abandoned (`shutting_down`).
    abandon_restores: bool,
    /// Ignore `leave_stopped` and resume restored jobs, as helpers that
    /// predate it do.
    ignore_leave_stopped: bool,
    /// The `leave_stopped` of every restore request.
    restores_stopped: Vec<(JobId, bool)>,
    checkpoint_outcome: FakeOutcome,
    restore_outcome: FakeOutcome,
    restore_rejection: Option<String>,
    checkpoint_rejection: Option<String>,
    ignore_sigterm: bool,
    /// Acknowledgements are lost (as when the daemon stops between
    /// applying an event and acknowledging it).
    ignore_acks: bool,
    spawned: Vec<SpawnRequest>,
    signals: Vec<(JobId, i32)>,
    /// Spawn/restore requests still to refuse with `shutting_down` right
    /// away (also while launches are held).
    refuse_launches: usize,
    hold_launches: bool,
    /// Held requests fail with `ConnectionLost` when released.
    lose_held: bool,
    /// Owner and GPUs of the jobs started by spawn or restore requests
    /// (or [`FakeHelper::insert_running_as`]), reported by `list_running`.
    launched: BTreeMap<JobId, (Uid, Vec<GpuIndex>)>,
    /// Event notices sent (see [`Helper::events_delivered`]).
    delivered: u64,
    /// Spawn/restore requests currently held.
    held: usize,
    hold_checkpoints: bool,
    /// Checkpoint requests currently held.
    held_checkpoints: usize,
}

impl State {
    fn emit(&mut self, event: Event) {
        self.next_seq += 1;
        let envelope = EventEnvelope {
            seq: EventSeq(self.next_seq),
            event,
        };
        self.unacked.push(envelope.clone());
        if self.connected
            && let Some(tx) = &self.notices
            && tx.send(HelperNotice::Event(envelope)).is_ok()
        {
            self.delivered += 1;
        }
    }

    fn running(&self) -> Vec<RunningJob> {
        self.jobs
            .iter()
            .map(|(id, job)| {
                let launched = self.launched.get(id);
                RunningJob {
                    job: *id,
                    pid: Some(job.pid),
                    phase: job.phase,
                    uid: launched.map(|(uid, _)| *uid),
                    gpus: launched.map(|(_, gpus)| gpus.clone()),
                }
            })
            .collect()
    }

    /// Replays unacknowledged events, then reports the live jobs, as the
    /// real helper connection does.
    fn announce(&mut self) {
        let Some(tx) = &self.notices else { return };
        for event in &self.unacked {
            if tx.send(HelperNotice::Event(event.clone())).is_ok() {
                self.delivered += 1;
            }
        }
        let _ = tx.send(HelperNotice::Connected {
            version: "fake".to_owned(),
            running: self.running(),
        });
    }

    fn check_connected(&self) -> HelperResult<()> {
        if self.connected {
            Ok(())
        } else {
            Err(HelperError::NotConnected)
        }
    }

    fn next_pid(&mut self) -> u32 {
        self.next_pid += 1;
        1000 + self.next_pid
    }
}

fn not_found(job: JobId) -> HelperError {
    HelperError::Rejected {
        code: HelperErrorCode::JobNotFound,
        message: format!("job {job} is not running"),
    }
}

#[derive(Debug, Clone)]
pub struct FakeHelper {
    state: Arc<Mutex<State>>,
    /// Bumped whenever held requests may proceed or the held count changes.
    wake: Arc<watch::Sender<u64>>,
}

impl Default for FakeHelper {
    fn default() -> Self {
        Self {
            state: Arc::default(),
            wake: Arc::new(watch::channel(0).0),
        }
    }
}

/// Decrements a held count (`held` or `held_checkpoints`) when a held
/// request stops waiting (also when its future is dropped).
struct Held<'a>(&'a FakeHelper, fn(&mut State) -> &mut usize);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        let count = self.1;
        self.0.with(|s| *count(s) -= 1);
        self.0.bump();
    }
}

impl FakeHelper {
    pub fn new() -> Self {
        Self::default()
    }

    fn bump(&self) {
        self.wake.send_modify(|n| *n = n.wrapping_add(1));
    }

    /// Waits while launches are held. A request dropped while waiting has
    /// no effect.
    async fn launch_gate(&self) -> HelperResult<()> {
        let mut rx = self.wake.subscribe();
        if !self.with(|s| s.hold_launches) {
            return Ok(());
        }
        self.with(|s| s.held += 1);
        self.bump();
        let _held = Held(self, |s| &mut s.held);
        while self.with(|s| s.hold_launches) {
            if rx.changed().await.is_err() {
                return Ok(());
            }
        }
        if self.with(|s| s.lose_held) {
            return Err(HelperError::ConnectionLost);
        }
        Ok(())
    }

    /// Waits while checkpoint requests are held. A request dropped while
    /// waiting has no effect.
    async fn checkpoint_gate(&self) {
        let mut rx = self.wake.subscribe();
        if !self.with(|s| s.hold_checkpoints) {
            return;
        }
        self.with(|s| s.held_checkpoints += 1);
        self.bump();
        let _held = Held(self, |s| &mut s.held_checkpoints);
        while self.with(|s| s.hold_checkpoints) {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    fn with<T>(&self, f: impl FnOnce(&mut State) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }

    // ----- simulation controls -----

    /// The job's process exits with `code`.
    pub fn exit(&self, job: JobId, code: i32) {
        self.with(|s| {
            if s.jobs.remove(&job).is_some() {
                s.emit(Event::JobExited {
                    job,
                    status: ExitStatus::Exited { code },
                });
            }
        });
    }

    /// The job vanishes without any event (e.g. lost across a reboot).
    pub fn vanish(&self, job: JobId) {
        self.with(|s| s.jobs.remove(&job));
    }

    /// The helper dumps the job on its own, as on an OS shutdown: the
    /// process is gone, its image is there, and the outcome is an event.
    pub fn dump(&self, job: JobId) {
        self.with(|s| {
            if s.jobs.remove(&job).is_some() {
                s.images.insert(job);
                s.emit(Event::CheckpointFinished {
                    job,
                    outcome: CheckpointOutcome::Success {
                        image_size_bytes: 4096,
                    },
                });
            }
        });
    }

    /// A live job the daemon does not know about.
    pub fn insert_running(&self, job: JobId) {
        self.with(|s| {
            let pid = s.next_pid();
            s.launched.remove(&job);
            s.jobs.insert(
                job,
                FakeJob {
                    pid,
                    phase: RunningPhase::Running,
                },
            );
        });
    }

    /// Like [`FakeHelper::insert_running`], for a job running as `uid` on
    /// `gpus` (reported by `list_running`).
    pub fn insert_running_as(&self, job: JobId, uid: Uid, gpus: &[GpuIndex]) {
        self.insert_running(job);
        self.with(|s| {
            s.launched.insert(job, (uid, gpus.to_vec()));
        });
    }

    /// Make the next spawns fail with `reason` (`None` to succeed again).
    pub fn set_spawn_failure(&self, reason: Option<&str>) {
        self.with(|s| s.spawn_failure = reason.map(str::to_owned));
    }

    /// Refuse spawns and restores with `shutting_down`, as a helper that
    /// is stopping does (`false` to accept them again).
    pub fn set_stopping(&self, stopping: bool) {
        self.with(|s| s.stopping = stopping);
    }

    /// Refuses the next `count` spawn and restore requests with
    /// `shutting_down`, as a stopping helper does, then accepts them again
    /// (as the next helper does). They are refused right away, also while
    /// launches are held, so that the daemon's retry can be held (see
    /// [`FakeHelper::hold_launches`]).
    pub fn refuse_launches(&self, count: usize) {
        self.with(|s| s.refuse_launches = count);
    }

    /// A refusal due by [`FakeHelper::refuse_launches`].
    fn take_refusal(&self) -> HelperResult<()> {
        self.with(|s| {
            if !s.connected || s.refuse_launches == 0 {
                return Ok(());
            }
            s.refuse_launches -= 1;
            Err(HelperError::Rejected {
                code: HelperErrorCode::ShuttingDown,
                message: "the helper is shutting down".to_owned(),
            })
        })
    }

    /// Accept restores but report them abandoned by a stopping helper (a
    /// failed outcome with code `shutting_down`, image untouched), as the
    /// real helper does for restores whose unit systemd had not begun to
    /// start when it was stopped. The helper then counts as stopping (see
    /// [`FakeHelper::set_stopping`]); `false` to restore them again.
    pub fn set_abandon_restores(&self, abandon: bool) {
        self.with(|s| s.abandon_restores = abandon);
    }

    /// Resume restored jobs whatever `leave_stopped` asks, and report them
    /// running, as helpers that predate stopped restores do.
    pub fn set_ignore_leave_stopped(&self, ignore: bool) {
        self.with(|s| s.ignore_leave_stopped = ignore);
    }

    /// The job and `leave_stopped` of every restore request received.
    pub fn restore_requests(&self) -> Vec<(JobId, bool)> {
        self.with(|s| s.restores_stopped.clone())
    }

    /// Holds spawn and restore requests until [`release_launches`].
    ///
    /// [`release_launches`]: FakeHelper::release_launches
    pub fn hold_launches(&self) {
        self.with(|s| s.hold_launches = true);
    }

    /// Lets held (and later) spawn and restore requests proceed.
    pub fn release_launches(&self) {
        self.with(|s| s.hold_launches = false);
        self.bump();
    }

    /// Fails the held spawn and restore requests with `ConnectionLost`, as
    /// if the connection broke before the helper read them (until
    /// [`reconnect`]).
    ///
    /// [`reconnect`]: FakeHelper::reconnect
    pub fn lose_held_launches(&self) {
        self.with(|s| {
            s.lose_held = true;
            s.hold_launches = false;
        });
        self.bump();
    }

    /// Resolves once `count` spawn/restore requests (or more) are being
    /// held; `0` waits until none is.
    pub async fn wait_held(&self, count: usize) {
        let mut rx = self.wake.subscribe();
        while self.with(|s| {
            if count == 0 {
                s.held > 0
            } else {
                s.held < count
            }
        }) {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Holds checkpoint requests (unanswered and without effect, as if not
    /// read by the helper yet) until [`release_checkpoints`].
    ///
    /// [`release_checkpoints`]: FakeHelper::release_checkpoints
    pub fn hold_checkpoints(&self) {
        self.with(|s| s.hold_checkpoints = true);
    }

    /// Lets held (and later) checkpoint requests proceed.
    pub fn release_checkpoints(&self) {
        self.with(|s| s.hold_checkpoints = false);
        self.bump();
    }

    /// Resolves once `count` checkpoint requests (or more) are being held.
    pub async fn wait_held_checkpoints(&self, count: usize) {
        let mut rx = self.wake.subscribe();
        while self.with(|s| s.held_checkpoints < count) {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn set_checkpoint_outcome(&self, outcome: FakeOutcome) {
        self.with(|s| s.checkpoint_outcome = outcome);
    }

    /// Make the next checkpoint requests be refused up front with `reason`
    /// (`checkpoint_refused`), as the helper's synchronous pre-checks do
    /// (hidden mounts, not enough free space, ...); `None` to accept them
    /// again.
    pub fn set_checkpoint_rejection(&self, reason: Option<&str>) {
        self.with(|s| s.checkpoint_rejection = reason.map(str::to_owned));
    }

    pub fn set_restore_outcome(&self, outcome: FakeOutcome) {
        self.with(|s| s.restore_outcome = outcome);
    }

    /// Make the next restore requests be refused up front with `reason`, as
    /// the real helper's synchronous checks do (e.g. an incompatible GPU);
    /// `None` to accept them again.
    pub fn set_restore_rejection(&self, reason: Option<&str>) {
        self.with(|s| s.restore_rejection = reason.map(str::to_owned));
    }

    /// Emits `event` as if the helper had produced it (e.g. a late answer).
    pub fn emit_event(&self, event: Event) {
        self.with(|s| s.emit(event));
    }

    /// Jobs survive SIGTERM (so only the SIGKILL after the grace period
    /// stops them).
    pub fn set_ignore_sigterm(&self, ignore: bool) {
        self.with(|s| s.ignore_sigterm = ignore);
    }

    /// Loses the daemon's acknowledgements, so every event is replayed on
    /// the next (re)connect, as if the daemon had stopped between applying
    /// and acknowledging it.
    pub fn set_ignore_acks(&self, ignore: bool) {
        self.with(|s| s.ignore_acks = ignore);
    }

    /// Simulates the helper going away; requests fail until [`reconnect`].
    ///
    /// [`reconnect`]: FakeHelper::reconnect
    pub fn disconnect(&self) {
        self.with(|s| {
            s.connected = false;
            if let Some(tx) = &s.notices {
                let _ = tx.send(HelperNotice::Disconnected);
            }
        });
    }

    pub fn reconnect(&self) {
        self.with(|s| {
            s.connected = true;
            s.lose_held = false;
            s.announce();
        });
    }

    // ----- inspection -----

    pub fn jobs(&self) -> BTreeMap<JobId, FakeJob> {
        self.with(|s| s.jobs.clone())
    }

    pub fn is_running(&self, job: JobId) -> bool {
        self.with(|s| s.jobs.contains_key(&job))
    }

    pub fn has_image(&self, job: JobId) -> bool {
        self.with(|s| s.images.contains(&job))
    }

    pub fn spawned(&self) -> Vec<SpawnRequest> {
        self.with(|s| s.spawned.clone())
    }

    pub fn signals(&self) -> Vec<(JobId, i32)> {
        self.with(|s| s.signals.clone())
    }

    pub fn unacked(&self) -> Vec<EventEnvelope> {
        self.with(|s| s.unacked.clone())
    }

    // ----- request handling -----

    fn do_spawn(&self, request: SpawnRequest) -> HelperResult<u32> {
        self.with(|s| {
            s.check_connected()?;
            if request.uid.0 == 0 {
                return Err(HelperError::Rejected {
                    code: HelperErrorCode::InvalidRequest,
                    message: "refusing to run a job as root".to_owned(),
                });
            }
            if s.jobs.contains_key(&request.job) {
                return Err(HelperError::Rejected {
                    code: HelperErrorCode::Busy,
                    message: format!("job {} is already running", request.job),
                });
            }
            if s.stopping {
                return Err(HelperError::Rejected {
                    code: HelperErrorCode::ShuttingDown,
                    message: "the helper is shutting down".to_owned(),
                });
            }
            if let Some(reason) = &s.spawn_failure {
                return Err(HelperError::Rejected {
                    code: HelperErrorCode::SpawnFailed,
                    message: reason.clone(),
                });
            }
            let pid = s.next_pid();
            s.jobs.insert(
                request.job,
                FakeJob {
                    pid,
                    phase: RunningPhase::Running,
                },
            );
            s.launched
                .insert(request.job, (request.uid, request.gpus.clone()));
            s.spawned.push(request);
            Ok(pid)
        })
    }

    fn do_signal(&self, job: JobId, sig: i32) -> HelperResult<()> {
        self.with(|s| {
            s.check_connected()?;
            let Some(entry) = s.jobs.get_mut(&job) else {
                return Err(not_found(job));
            };
            s.signals.push((job, sig));
            match sig {
                signal::SIGSTOP => entry.phase = RunningPhase::Stopped,
                signal::SIGCONT => entry.phase = RunningPhase::Running,
                signal::SIGTERM | signal::SIGINT if s.ignore_sigterm => {}
                signal::SIGTERM | signal::SIGINT | signal::SIGKILL => {
                    s.jobs.remove(&job);
                    s.emit(Event::JobExited {
                        job,
                        status: ExitStatus::Signaled { signal: sig },
                    });
                }
                _ => {}
            }
            Ok(())
        })
    }

    fn do_checkpoint(&self, job: JobId) -> HelperResult<()> {
        self.with(|s| {
            s.check_connected()?;
            if !s.jobs.contains_key(&job) {
                return Err(not_found(job));
            }
            if let Some(reason) = &s.checkpoint_rejection {
                return Err(HelperError::Rejected {
                    code: HelperErrorCode::CheckpointRefused,
                    message: reason.clone(),
                });
            }
            let outcome = match s.checkpoint_outcome.clone() {
                FakeOutcome::LoseRequest => return Err(HelperError::Timeout),
                FakeOutcome::Hold => {
                    if let Some(entry) = s.jobs.get_mut(&job) {
                        entry.phase = RunningPhase::Checkpointing;
                    }
                    return Ok(());
                }
                FakeOutcome::Succeed => {
                    s.jobs.remove(&job);
                    s.images.insert(job);
                    CheckpointOutcome::Success {
                        image_size_bytes: 4096,
                    }
                }
                FakeOutcome::Fail(reason) => CheckpointOutcome::failed(reason),
                FakeOutcome::Refuse(reason) => CheckpointOutcome::refused(reason),
            };
            s.emit(Event::CheckpointFinished { job, outcome });
            Ok(())
        })
    }

    fn do_restore(&self, request: RestoreRequest) -> HelperResult<()> {
        self.with(|s| {
            s.check_connected()?;
            let job = request.job;
            s.restores_stopped.push((job, request.leave_stopped));
            if s.jobs.contains_key(&job) {
                return Err(HelperError::Rejected {
                    code: HelperErrorCode::Busy,
                    message: format!("job {job} is already running"),
                });
            }
            if s.stopping {
                return Err(HelperError::Rejected {
                    code: HelperErrorCode::ShuttingDown,
                    message: "the helper is shutting down".to_owned(),
                });
            }
            if let Some(reason) = &s.restore_rejection {
                return Err(HelperError::Rejected {
                    code: HelperErrorCode::InvalidRequest,
                    message: reason.clone(),
                });
            }
            let outcome = if !s.images.contains(&job) {
                RestoreOutcome::failed("no checkpoint image")
            } else if s.abandon_restores {
                // Only a stopping helper abandons restores; it refuses the
                // next ones up front.
                s.stopping = true;
                RestoreOutcome::Failed {
                    reason: "the helper is stopping; the restore unit was not started".to_owned(),
                    code: Some(HelperErrorCode::ShuttingDown),
                }
            } else {
                match s.restore_outcome.clone() {
                    FakeOutcome::Succeed => {
                        s.images.remove(&job);
                        let pid = s.next_pid();
                        let stopped = request.leave_stopped && !s.ignore_leave_stopped;
                        let phase = if stopped {
                            RunningPhase::Stopped
                        } else {
                            RunningPhase::Running
                        };
                        s.jobs.insert(job, FakeJob { pid, phase });
                        s.launched.insert(job, (request.uid, request.gpus.clone()));
                        RestoreOutcome::Success { pid, stopped }
                    }
                    FakeOutcome::Fail(reason) | FakeOutcome::Refuse(reason) => {
                        RestoreOutcome::failed(reason)
                    }
                    FakeOutcome::Hold => return Ok(()),
                    FakeOutcome::LoseRequest => return Err(HelperError::Timeout),
                }
            };
            s.emit(Event::RestoreFinished { job, outcome });
            Ok(())
        })
    }
}

impl Helper for FakeHelper {
    fn connect(&self) -> mpsc::UnboundedReceiver<HelperNotice> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.with(|s| {
            s.notices = Some(tx);
            s.connected = true;
            s.announce();
        });
        rx
    }

    fn close(&self) {
        self.with(|s| {
            s.connected = false;
            s.notices = None;
        });
    }

    fn state(&self) -> HelperState {
        let connected = self.with(|s| s.connected);
        HelperState {
            connected,
            version: connected.then(|| "fake".to_owned()),
        }
    }

    fn events_delivered(&self) -> u64 {
        self.with(|s| s.delivered)
    }

    fn spawn(&self, request: SpawnRequest) -> BoxFuture<'_, HelperResult<u32>> {
        async move {
            self.take_refusal()?;
            self.launch_gate().await?;
            self.do_spawn(request)
        }
        .boxed()
    }

    fn signal(&self, job: JobId, signal: i32) -> BoxFuture<'_, HelperResult<()>> {
        let result = self.do_signal(job, signal);
        async move { result }.boxed()
    }

    fn checkpoint(
        &self,
        job: JobId,
        _image_dir: PathBuf,
        _options: CheckpointOptions,
    ) -> BoxFuture<'_, HelperResult<()>> {
        async move {
            self.checkpoint_gate().await;
            self.do_checkpoint(job)
        }
        .boxed()
    }

    fn restore(&self, request: RestoreRequest) -> BoxFuture<'_, HelperResult<()>> {
        async move {
            self.take_refusal()?;
            self.launch_gate().await?;
            self.do_restore(request)
        }
        .boxed()
    }

    fn delete_checkpoint(
        &self,
        job: JobId,
        _image_dir: PathBuf,
    ) -> BoxFuture<'_, HelperResult<()>> {
        let result = self.with(|s| {
            s.check_connected()?;
            s.images.remove(&job);
            Ok(())
        });
        async move { result }.boxed()
    }

    fn list_running(&self) -> BoxFuture<'_, HelperResult<Vec<RunningJob>>> {
        let result = self.with(|s| s.check_connected().map(|()| s.running()));
        async move { result }.boxed()
    }

    fn ack_events(&self, up_to: EventSeq) -> BoxFuture<'_, HelperResult<()>> {
        let result = self.with(|s| {
            s.check_connected()?;
            if !s.ignore_acks {
                s.unacked.retain(|e| e.seq > up_to);
            }
            Ok(())
        });
        async move { result }.boxed()
    }
}
