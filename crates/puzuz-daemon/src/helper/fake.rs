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
//! helper does not know about yet.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use futures::FutureExt;
use futures::future::BoxFuture;
use puzuz_core::model::JobId;
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
    checkpoint_outcome: FakeOutcome,
    restore_outcome: FakeOutcome,
    ignore_sigterm: bool,
    spawned: Vec<SpawnRequest>,
    signals: Vec<(JobId, i32)>,
    hold_launches: bool,
    /// Spawn/restore requests currently held.
    held: usize,
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
        {
            let _ = tx.send(HelperNotice::Event(envelope));
        }
    }

    fn running(&self) -> Vec<RunningJob> {
        self.jobs
            .iter()
            .map(|(id, job)| RunningJob {
                job: *id,
                pid: Some(job.pid),
                phase: job.phase,
            })
            .collect()
    }

    /// Replays unacknowledged events, then reports the live jobs, as the
    /// real helper connection does.
    fn announce(&self) {
        let Some(tx) = &self.notices else { return };
        for event in &self.unacked {
            let _ = tx.send(HelperNotice::Event(event.clone()));
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

/// Decrements the held count when a held request stops waiting (also when
/// its future is dropped).
struct Held<'a>(&'a FakeHelper);

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.0.with(|s| s.held -= 1);
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
    async fn launch_gate(&self) {
        let mut rx = self.wake.subscribe();
        if !self.with(|s| s.hold_launches) {
            return;
        }
        self.with(|s| s.held += 1);
        self.bump();
        let _held = Held(self);
        while self.with(|s| s.hold_launches) {
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

    /// A live job the daemon does not know about.
    pub fn insert_running(&self, job: JobId) {
        self.with(|s| {
            let pid = s.next_pid();
            s.jobs.insert(
                job,
                FakeJob {
                    pid,
                    phase: RunningPhase::Running,
                },
            );
        });
    }

    /// Make the next spawns fail with `reason` (`None` to succeed again).
    pub fn set_spawn_failure(&self, reason: Option<&str>) {
        self.with(|s| s.spawn_failure = reason.map(str::to_owned));
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

    /// Resolves once `count` spawn/restore requests are being held.
    pub async fn wait_held(&self, count: usize) {
        let mut rx = self.wake.subscribe();
        while self.with(|s| s.held) < count {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    pub fn set_checkpoint_outcome(&self, outcome: FakeOutcome) {
        self.with(|s| s.checkpoint_outcome = outcome);
    }

    pub fn set_restore_outcome(&self, outcome: FakeOutcome) {
        self.with(|s| s.restore_outcome = outcome);
    }

    /// Jobs survive SIGTERM (so only the SIGKILL after the grace period
    /// stops them).
    pub fn set_ignore_sigterm(&self, ignore: bool) {
        self.with(|s| s.ignore_sigterm = ignore);
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
            let outcome = match s.checkpoint_outcome.clone() {
                FakeOutcome::Succeed => {
                    s.jobs.remove(&job);
                    s.images.insert(job);
                    CheckpointOutcome::Success {
                        image_size_bytes: 4096,
                    }
                }
                FakeOutcome::Fail(reason) => CheckpointOutcome::Failed { reason },
            };
            s.emit(Event::CheckpointFinished { job, outcome });
            Ok(())
        })
    }

    fn do_restore(&self, request: RestoreRequest) -> HelperResult<()> {
        self.with(|s| {
            s.check_connected()?;
            let job = request.job;
            if s.jobs.contains_key(&job) {
                return Err(HelperError::Rejected {
                    code: HelperErrorCode::Busy,
                    message: format!("job {job} is already running"),
                });
            }
            let outcome = if !s.images.contains(&job) {
                RestoreOutcome::Failed {
                    reason: "no checkpoint image".to_owned(),
                }
            } else {
                match s.restore_outcome.clone() {
                    FakeOutcome::Succeed => {
                        s.images.remove(&job);
                        let pid = s.next_pid();
                        s.jobs.insert(
                            job,
                            FakeJob {
                                pid,
                                phase: RunningPhase::Running,
                            },
                        );
                        RestoreOutcome::Success { pid }
                    }
                    FakeOutcome::Fail(reason) => RestoreOutcome::Failed { reason },
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

    fn spawn(&self, request: SpawnRequest) -> BoxFuture<'_, HelperResult<u32>> {
        async move {
            self.launch_gate().await;
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
        let result = self.do_checkpoint(job);
        async move { result }.boxed()
    }

    fn restore(&self, request: RestoreRequest) -> BoxFuture<'_, HelperResult<()>> {
        async move {
            self.launch_gate().await;
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
            s.unacked.retain(|e| e.seq > up_to);
            Ok(())
        });
        async move { result }.boxed()
    }
}
