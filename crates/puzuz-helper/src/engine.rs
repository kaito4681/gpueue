//! The helper's request handling, job table, exit monitoring, checkpoint,
//! restore and shutdown logic, independent of the socket and of how jobs
//! are launched.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use puzuz_checkpoint::exec::{Deadline, StepError, run_plan_until, run_with_rollback};
use puzuz_checkpoint::layout::{
    self, CheckpointDir, CheckpointMeta, DirStatus, GpuMeta, check_compatibility, write_atomic,
};
use puzuz_checkpoint::{
    CheckpointConfig, CudaMode, DeviceMap, DumpTarget, Executor, RestoreTarget,
};
use puzuz_checkpoint::{plan, probe, space};
use puzuz_protocol::helper::{
    CheckpointOptions, CheckpointOutcome, Event, EventEnvelope, EventSeq, ExitStatus,
    HelperErrorCode, JobId, Request, ResponseResult, RestoreOutcome, RestoreRequest, RunningJob,
    RunningPhase, SpawnRequest, signal,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast};

use crate::config::{HelperConfig, SHUTDOWN_MANIFEST};
use crate::events::EventStore;
use crate::host::{HostProbe, ProcInfo};
use crate::jobs::{self, JobFiles, JobRecord, RecordPhase, ShimSpec, UnitKind};
use crate::launcher::{
    LaunchError, LaunchSpec, Launcher, RestoreSpec, UnitState, restore_unit_name, unit_name,
};
use crate::validate::{self, Rejection};

/// File in a checkpoint directory recording the job's owner, so a restore
/// can check it without trusting the daemon.
pub const OWNER_FILE: &str = "helper-job.json";
/// File in a checkpoint directory recording what the job's mount namespace
/// depends on ([`SavedMounts`]); a restore needs all of it to exist.
pub const MOUNTS_FILE: &str = "helper-mounts.json";

/// Contents of [`MOUNTS_FILE`]. Older helpers wrote only the list of mount
/// points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SavedMounts {
    Full {
        /// Mount points of the job's namespace at dump time.
        mount_points: Vec<String>,
        /// Paths CRIU bind-mounts from the host on restore (its
        /// auto-detected external mounts, from the dump log).
        #[serde(default)]
        external_sources: Vec<String>,
    },
    MountPoints(Vec<String>),
}

impl SavedMounts {
    /// Every path that must exist on the restoring host, without
    /// duplicates.
    pub fn required_paths(&self) -> Vec<&str> {
        let (a, b): (&[String], &[String]) = match self {
            SavedMounts::Full {
                mount_points,
                external_sources,
            } => (mount_points, external_sources),
            SavedMounts::MountPoints(m) => (m, &[]),
        };
        let mut out: Vec<&str> = Vec::new();
        for p in a.iter().chain(b) {
            if !out.contains(&p.as_str()) {
                out.push(p);
            }
        }
        out
    }
}

/// Time allowed around a checkpoint or restore for the work outside its
/// deadline: pre-checks, writing the metadata, stopping the unit.
const OP_SLACK: Duration = Duration::from_secs(60);

/// How often a stopping helper asks systemd for more time
/// (`EXTEND_TIMEOUT_USEC`) while it waits for or runs dumps.
const EXTEND_INTERVAL: Duration = Duration::from_secs(5);

/// The latest time each running checkpoint or restore may still need:
/// its deadline, plus its rollback and [`OP_SLACK`]. A stopping helper
/// extends systemd's stop timeout up to it, never beyond, so systemd stays
/// a backstop for an operation stuck past its own deadline.
#[derive(Debug, Default, Clone)]
pub struct OpEnds(Arc<Mutex<BTreeMap<JobId, Instant>>>);

impl OpEnds {
    fn set(&self, job: JobId, at: Instant) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job, at);
    }

    /// Track `job` until the guard is dropped, initially for [`OP_SLACK`].
    fn track(&self, job: JobId) -> OpEndGuard {
        self.set(job, Instant::now() + OP_SLACK);
        OpEndGuard(self.clone(), job)
    }

    /// How much more stop time to ask for: up to the latest end of a
    /// running operation, [`OP_SLACK`] when none runs, `None` when every
    /// running operation is past its end (systemd should not wait longer).
    pub fn stop_budget(&self) -> Option<Duration> {
        let ends = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match ends.values().max() {
            None => Some(OP_SLACK),
            Some(end) => {
                let left = end.saturating_duration_since(Instant::now());
                (!left.is_zero()).then_some(left)
            }
        }
    }
}

struct OpEndGuard(OpEnds, JobId);

impl Drop for OpEndGuard {
    fn drop(&mut self) {
        self.0
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.1);
    }
}

/// Creates an [`Executor`] for one checkpoint or restore.
pub type ExecutorFactory = Arc<dyn Fn() -> Box<dyn Executor + Send> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// `spawn` is in progress.
    Starting,
    Running,
    Stopped,
    Checkpointing,
    Restoring,
}

#[derive(Debug, Clone)]
struct Entry {
    record: JobRecord,
    files: JobFiles,
    phase: Phase,
    pid: Option<u32>,
}

/// Result of one job in the shutdown manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ManifestResult {
    Checkpointed {
        image_size_bytes: u64,
    },
    Failed {
        reason: String,
        /// The dump was refused before the job was touched (see
        /// [`CheckpointOutcome::is_refused`]); systemd stops it either way.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        refused: bool,
    },
    /// `no_checkpoint` job, left for systemd to stop.
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub job: JobId,
    #[serde(flatten)]
    pub result: ManifestResult,
}

/// Written atomically to `<checkpoint base>/shutdown-manifest.json` at the
/// end of a shutdown, for the daemon to reconcile on its next start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShutdownManifest {
    /// Unix epoch milliseconds.
    pub created_at: i64,
    pub jobs: Vec<ManifestEntry>,
}

/// Persistent event queue plus live fan-out to connections.
pub struct EventBus {
    store: Mutex<EventStore>,
    tx: broadcast::Sender<EventEnvelope>,
}

impl EventBus {
    pub fn open(dir: &Path) -> io::Result<Self> {
        let (tx, _) = broadcast::channel(1024);
        Ok(Self {
            store: Mutex::new(EventStore::open(dir)?),
            tx,
        })
    }

    fn lock(&self) -> MutexGuard<'_, EventStore> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Persist, then broadcast.
    pub fn emit(&self, event: Event) -> io::Result<EventEnvelope> {
        let mut store = self.lock();
        let env = store.append(event)?;
        let _ = self.tx.send(env.clone());
        Ok(env)
    }

    /// Unacknowledged events plus a receiver for everything after them,
    /// taken atomically.
    pub fn subscribe(&self) -> (Vec<EventEnvelope>, broadcast::Receiver<EventEnvelope>) {
        let store = self.lock();
        (store.pending().to_vec(), self.tx.subscribe())
    }

    /// Unacknowledged events with a sequence number above `after`.
    pub fn pending_after(&self, after: EventSeq) -> Vec<EventEnvelope> {
        self.lock()
            .pending()
            .iter()
            .filter(|e| e.seq > after)
            .cloned()
            .collect()
    }

    pub fn ack(&self, up_to: EventSeq) -> io::Result<()> {
        self.lock().ack(up_to)
    }
}

/// Counts operations that shutdown must wait for.
#[derive(Default)]
struct InFlight {
    count: Mutex<usize>,
    idle: Notify,
}

struct InFlightGuard(Arc<InFlight>);

impl InFlight {
    fn enter(self: &Arc<Self>) -> InFlightGuard {
        *self.count.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        InFlightGuard(self.clone())
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if *self.count.lock().unwrap_or_else(|e| e.into_inner()) == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut n = self.0.count.lock().unwrap_or_else(|e| e.into_inner());
        *n -= 1;
        if *n == 0 {
            self.0.idle.notify_waiters();
        }
    }
}

fn error(code: HelperErrorCode, message: impl Into<String>) -> ResponseResult {
    ResponseResult::Error {
        code,
        message: message.into(),
    }
}

impl From<Rejection> for ResponseResult {
    fn from(r: Rejection) -> Self {
        error(r.code, r.message)
    }
}

impl From<LaunchError> for ResponseResult {
    fn from(e: LaunchError) -> Self {
        error(e.code, e.message)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

pub struct Helper<L: Launcher, H: HostProbe> {
    cfg: HelperConfig,
    launcher: L,
    host: Arc<H>,
    executor: ExecutorFactory,
    jobs: Mutex<BTreeMap<JobId, Entry>>,
    events: EventBus,
    shutting_down: AtomicBool,
    inflight: Arc<InFlight>,
    /// Dumps and restores run one at a time.
    heavy: tokio::sync::Mutex<()>,
    op_ends: OpEnds,
}

impl<L: Launcher, H: HostProbe> Helper<L, H> {
    pub fn new(
        cfg: HelperConfig,
        launcher: L,
        host: H,
        executor: ExecutorFactory,
    ) -> io::Result<Arc<Self>> {
        for dir in [&cfg.state_dir, &cfg.jobs_dir()] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o711)
                .create(dir)?;
        }
        let events = EventBus::open(&cfg.events_dir())?;
        Ok(Arc::new(Self {
            cfg,
            launcher,
            host: Arc::new(host),
            executor,
            jobs: Mutex::new(BTreeMap::new()),
            events,
            shutting_down: AtomicBool::new(false),
            inflight: Arc::new(InFlight::default()),
            heavy: tokio::sync::Mutex::new(()),
            op_ends: OpEnds::default(),
        }))
    }

    pub fn config(&self) -> &HelperConfig {
        &self.cfg
    }

    pub fn launcher(&self) -> &L {
        &self.launcher
    }

    pub fn events(&self) -> &EventBus {
        &self.events
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    fn table(&self) -> MutexGuard<'_, BTreeMap<JobId, Entry>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, event: Event) {
        if let Err(e) = self.events.emit(event.clone()) {
            // The daemon will notice through list_running on reconnect.
            eprintln!("puzuz-helper: failed to persist event {event:?}: {e}");
        }
    }

    /// Handle one request.
    pub async fn handle(self: &Arc<Self>, req: Request) -> ResponseResult {
        match req {
            Request::Spawn(r) => self.spawn(r).await,
            Request::Signal { job, signal } => self.signal(job, signal).await,
            Request::Checkpoint {
                job,
                image_dir,
                options,
            } => self.checkpoint(job, &image_dir, options).await,
            Request::Restore(r) => self.restore(r),
            Request::DeleteCheckpoint { job, image_dir } => {
                self.delete_checkpoint(job, &image_dir).await
            }
            Request::ListRunning => self.list_running(),
            Request::AckEvents { up_to } => match self.events.ack(up_to) {
                Ok(()) => ResponseResult::Ok,
                Err(e) => error(HelperErrorCode::Internal, format!("ack: {e}")),
            },
        }
    }

    // -- spawn ------------------------------------------------------------

    async fn spawn(self: &Arc<Self>, req: SpawnRequest) -> ResponseResult {
        // Counted before the check, so a stop that saw no operation in
        // flight is seen here (see `Self::drain`).
        let _busy = self.inflight.enter();
        if self.is_shutting_down() {
            return error(HelperErrorCode::ShuttingDown, "the helper is shutting down");
        }
        let creds = match validate::check_spawn(&*self.host, &req) {
            Ok(c) => c,
            Err(r) => return r.into(),
        };
        let files = JobFiles::new(&self.cfg.jobs_dir(), req.job);
        let record = JobRecord {
            job: req.job,
            uid: req.uid,
            gid: req.gid,
            log_path: req.log_path.clone(),
            gpus: req.gpus.clone(),
            no_checkpoint: req.no_checkpoint,
            phase: RecordPhase::Live,
            unit_kind: UnitKind::Spawned,
            unit: unit_name(req.job),
            restores: 0,
        };
        {
            let mut table = self.table();
            if table.contains_key(&req.job) {
                return error(
                    HelperErrorCode::Busy,
                    format!("job {} is running", req.job.0),
                );
            }
            if files.read_record().is_ok() {
                return error(
                    HelperErrorCode::Busy,
                    format!("job {} has a checkpoint; restore or delete it", req.job.0),
                );
            }
            table.insert(
                req.job,
                Entry {
                    record: record.clone(),
                    files: files.clone(),
                    phase: Phase::Starting,
                    pid: None,
                },
            );
        }
        match self.do_spawn(&req, &creds, &files, &record).await {
            Ok(pid) => {
                let _ = fs::remove_file(files.spec());
                if let Some(e) = self.table().get_mut(&req.job) {
                    e.phase = Phase::Running;
                    e.pid = Some(pid);
                }
                ResponseResult::Spawned { pid }
            }
            Err(res) => {
                self.table().remove(&req.job);
                let _ = files.remove();
                res
            }
        }
    }

    async fn do_spawn(
        &self,
        req: &SpawnRequest,
        creds: &validate::Credentials,
        files: &JobFiles,
        record: &JobRecord,
    ) -> Result<u32, ResponseResult> {
        let internal =
            |what: &str, e: io::Error| error(HelperErrorCode::Internal, format!("{what}: {e}"));
        files
            .prepare(false)
            .map_err(|e| internal("preparing job files", e))?;
        self.ensure_log_dir(req)
            .map_err(|e| internal("preparing the log directory", e))?;

        let mut argv = req.shell.clone();
        argv.push(req.command.clone());
        let mut envs = req.envs.clone();
        envs.insert(
            "CUDA_VISIBLE_DEVICES".into(),
            req.gpus
                .iter()
                .map(|g| g.0.to_string())
                .collect::<Vec<_>>()
                .join(","),
        );
        envs.insert("CUDA_DEVICE_ORDER".into(), "PCI_BUS_ID".into());
        if let Some(lib) = &self.cfg.job_preload {
            crate::forkguard::add_to_preload(&mut envs, lib);
        }
        files
            .write_spec(&ShimSpec {
                uid: creds.uid.0,
                gid: creds.gid.0,
                groups: creds
                    .groups
                    .as_ref()
                    .map(|g| g.iter().map(|g| g.0).collect()),
                mounts: self.cfg.mounts.clone(),
                argv: argv.clone(),
                envs: envs.clone(),
                cwd: req.cwd.clone(),
                log_path: req.log_path.clone(),
                log_mode: if self.is_managed_log(req) {
                    0o660
                } else {
                    0o640
                },
            })
            .map_err(|e| internal("writing the job spec", e))?;
        files
            .write_record(record)
            .map_err(|e| internal("writing the job record", e))?;
        let spec = LaunchSpec {
            job: req.job,
            unit: record.unit.clone(),
            files: files.clone(),
            argv,
            envs,
        };
        self.launcher.spawn(&spec).await.map_err(Into::into)
    }

    fn is_managed_log(&self, req: &SpawnRequest) -> bool {
        let user_dir = self.cfg.log_base.join(req.uid.0.to_string());
        req.log_path.parent() == Some(user_dir.as_path())
    }

    /// `<log base>/<uid>/` is created by the helper: owned by the user,
    /// group `puzuz`, mode 2770. The setgid bit makes logs inherit the
    /// group, and group write lets the unprivileged daemon delete and
    /// truncate logs (remove, clean, reset, restart in place) without a
    /// helper round trip; logs there are created 0660. The daemon can thus
    /// also create files there, but the shim only ever appends to a regular
    /// file owned by the job's owner. Logs elsewhere are the owner's
    /// business.
    fn ensure_log_dir(&self, req: &SpawnRequest) -> io::Result<()> {
        let user_dir = self.cfg.log_base.join(req.uid.0.to_string());
        if !self.is_managed_log(req) {
            return Ok(());
        }
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(&self.cfg.log_base)?;
        match fs::symlink_metadata(&user_dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} is not a directory", user_dir.display()),
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // A concurrent spawn of the same user may create it first.
                match fs::DirBuilder::new().mode(0o750).create(&user_dir) {
                    Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                    _ => {}
                }
            }
            Err(e) => return Err(e),
        }
        let m = fs::symlink_metadata(&user_dir)?;
        if !m.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} is not a directory", user_dir.display()),
            ));
        }
        if m.uid() != req.uid.0 || m.gid() != self.cfg.daemon_gid {
            std::os::unix::fs::lchown(&user_dir, Some(req.uid.0), Some(self.cfg.daemon_gid))?;
        }
        fs::set_permissions(&user_dir, fs::Permissions::from_mode(0o2770))
    }

    // -- signal -----------------------------------------------------------

    async fn signal(&self, job: JobId, sig: i32) -> ResponseResult {
        if !(1..=64).contains(&sig) {
            return error(
                HelperErrorCode::InvalidRequest,
                format!("invalid signal {sig}"),
            );
        }
        let unit = match self
            .table()
            .get(&job)
            .map(|e| (e.phase, e.record.unit.clone()))
        {
            None => {
                return error(
                    HelperErrorCode::JobNotFound,
                    format!("job {} is not running", job.0),
                );
            }
            Some((Phase::Running | Phase::Stopped, unit)) => unit,
            Some(_) => return error(HelperErrorCode::Busy, format!("job {} is busy", job.0)),
        };
        if let Err(e) = self.launcher.signal(&unit, sig).await {
            return e.into();
        }
        if let Some(e) = self.table().get_mut(&job) {
            match (sig, e.phase) {
                (signal::SIGSTOP, Phase::Running) => e.phase = Phase::Stopped,
                (signal::SIGCONT, Phase::Stopped) => e.phase = Phase::Running,
                _ => {}
            }
        }
        ResponseResult::Ok
    }

    // -- list / delete ----------------------------------------------------

    fn list_running(&self) -> ResponseResult {
        let jobs = self
            .table()
            .iter()
            .map(|(id, e)| RunningJob {
                job: *id,
                pid: e.pid,
                phase: match e.phase {
                    Phase::Starting | Phase::Running => RunningPhase::Running,
                    Phase::Stopped => RunningPhase::Stopped,
                    Phase::Checkpointing => RunningPhase::Checkpointing,
                    Phase::Restoring => RunningPhase::Restoring,
                },
            })
            .collect();
        ResponseResult::Running { jobs }
    }

    async fn delete_checkpoint(&self, job: JobId, image_dir: &Path) -> ResponseResult {
        if let Err(r) = validate::check_image_dir(&self.cfg.checkpoint_base, job, image_dir) {
            return r.into();
        }
        if self.table().contains_key(&job) {
            return error(HelperErrorCode::Busy, format!("job {} is live", job.0));
        }
        let dir = CheckpointDir::for_job(&self.cfg.checkpoint_base, job.0);
        if let Err(e) = dir.remove() {
            return error(
                HelperErrorCode::Internal,
                format!("removing {}: {e}", dir.root().display()),
            );
        }
        let files = JobFiles::new(&self.cfg.jobs_dir(), job);
        if files
            .read_record()
            .is_ok_and(|r| r.phase == RecordPhase::Checkpointed)
        {
            let _ = files.remove();
        }
        ResponseResult::Ok
    }

    // -- exit monitoring --------------------------------------------------

    /// How a job ended, or `None` while it runs.
    async fn probe_exit(&self, unit: &str, files: &JobFiles) -> Option<ExitStatus> {
        if let Some(s) = files.read_exit_status() {
            return Some(s);
        }
        let unit = match self.launcher.unit_state(unit).await {
            Ok(UnitState::Running) | Err(_) => return None,
            Ok(UnitState::Exited(s)) => s,
            Ok(UnitState::Gone) => None,
        };
        Some(
            files
                .read_exit_status()
                .or_else(|| files.read_init_status())
                .or(unit)
                .unwrap_or(ExitStatus::Unknown),
        )
    }

    /// Report jobs that have exited. Called periodically.
    pub async fn poll_exits(&self) {
        let candidates: Vec<(JobId, String, JobFiles)> = self
            .table()
            .iter()
            .filter(|(_, e)| matches!(e.phase, Phase::Running | Phase::Stopped))
            .map(|(id, e)| (*id, e.record.unit.clone(), e.files.clone()))
            .collect();
        for (job, unit, files) in candidates {
            let Some(status) = self.probe_exit(&unit, &files).await else {
                continue;
            };
            {
                let mut table = self.table();
                // A checkpoint or similar may have started meanwhile.
                if !table
                    .get(&job)
                    .is_some_and(|e| matches!(e.phase, Phase::Running | Phase::Stopped))
                {
                    continue;
                }
                table.remove(&job);
            }
            self.finish_exited(job, &unit, &files, status).await;
        }
    }

    async fn finish_exited(&self, job: JobId, unit: &str, files: &JobFiles, status: ExitStatus) {
        self.emit(Event::JobExited { job, status });
        if let Err(e) = self.launcher.release(unit).await {
            eprintln!("puzuz-helper: releasing job {}: {e}", job.0);
        }
        let _ = files.remove();
    }

    /// Run [`Self::poll_exits`] every `poll_interval` until `stop` fires.
    pub async fn monitor(self: Arc<Self>, mut stop: tokio::sync::watch::Receiver<bool>) {
        let mut tick = tokio::time::interval(self.cfg.poll_interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = tick.tick() => self.poll_exits().await,
                _ = stop.changed() => return,
            }
        }
    }

    // -- recovery ---------------------------------------------------------

    /// Rebuild the job table after a (re)start: re-adopt running units,
    /// report jobs that exited while the helper was away, drop incomplete
    /// checkpoints.
    pub async fn recover(&self) -> io::Result<()> {
        let records = jobs::scan(&self.cfg.jobs_dir())?;
        let mut known = Vec::new();
        for (files, mut record) in records {
            let job = record.job;
            known.push(record.unit.clone());
            let state = self
                .launcher
                .unit_state(&record.unit)
                .await
                .map_err(|e| io::Error::other(format!("querying {}: {e}", record.unit)))?;
            if record.phase == RecordPhase::Checkpointed {
                if state == UnitState::Running && record.restores > 0 {
                    // A restore finished but the helper died before
                    // recording it.
                    record.phase = RecordPhase::Live;
                    record.unit_kind = UnitKind::Restored;
                    files.write_record(&record)?;
                } else {
                    // The dumped (failed) unit or a failed restore unit.
                    if state != UnitState::Gone {
                        let _ = self.launcher.release(&record.unit).await;
                    }
                    let dir = CheckpointDir::for_job(&self.cfg.checkpoint_base, job.0);
                    if !matches!(dir.status(), DirStatus::Complete(_)) {
                        let _ = files.remove();
                    }
                    continue;
                }
            }
            match state {
                UnitState::Running => {
                    let pid = self
                        .launcher
                        .init_pid(&record.unit, record.unit_kind)
                        .await
                        .ok()
                        .flatten();
                    self.table().insert(
                        job,
                        Entry {
                            record,
                            files,
                            phase: Phase::Running,
                            pid,
                        },
                    );
                }
                UnitState::Exited(_) | UnitState::Gone => {
                    let status = self
                        .probe_exit(&record.unit, &files)
                        .await
                        .unwrap_or(ExitStatus::Unknown);
                    self.finish_exited(job, &record.unit, &files, status).await;
                }
            }
        }
        if let Ok(units) = self.launcher.list().await {
            for unit in units.into_iter().filter(|u| !known.contains(u)) {
                eprintln!("puzuz-helper: {unit} has no helper record; leaving it alone");
            }
        }
        for (id, status) in layout::scan(&self.cfg.checkpoint_base)? {
            if let DirStatus::Incomplete(reason) = status {
                eprintln!("puzuz-helper: removing incomplete checkpoint of job {id}: {reason:?}");
                let _ = CheckpointDir::for_job(&self.cfg.checkpoint_base, id).remove();
            }
        }
        Ok(())
    }

    // -- checkpoint -------------------------------------------------------

    async fn checkpoint(
        self: &Arc<Self>,
        job: JobId,
        image_dir: &Path,
        options: CheckpointOptions,
    ) -> ResponseResult {
        if let Err(r) = validate::check_image_dir(&self.cfg.checkpoint_base, job, image_dir) {
            return r.into();
        }
        if options.leave_running {
            return error(
                HelperErrorCode::InvalidRequest,
                "leave_running is not supported",
            );
        }
        let was_stopped = match self.claim_for_checkpoint(job) {
            Ok(s) => s,
            Err(res) => return res,
        };
        let guard = self.inflight.enter();
        // Refuse up front what the dump would refuse anyway, so the caller
        // gets the reason as the answer. The dump checks again (things may
        // change while it waits for an earlier one).
        if let Err(reason) = self.precheck(job).await {
            self.unclaim(job, was_stopped);
            eprintln!(
                "puzuz-helper: refusing to checkpoint job {}: {reason}",
                job.0
            );
            return error(HelperErrorCode::CheckpointRefused, reason);
        }
        let this = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let outcome = this.checkpoint_job(job, options, was_stopped).await;
            this.emit(Event::CheckpointFinished { job, outcome });
        });
        ResponseResult::Accepted
    }

    /// The dump's pre-checks ([`precheck_dump`]) for a claimed job, without
    /// touching it.
    async fn precheck(&self, job: JobId) -> Result<(), String> {
        let Some(entry) = self.table().get(&job).cloned() else {
            return Err("job vanished".into());
        };
        let init_pid = match self
            .launcher
            .init_pid(&entry.record.unit, entry.record.unit_kind)
            .await
        {
            Ok(Some(pid)) => pid,
            Ok(None) => return Err("the job is not running".into()),
            Err(e) => return Err(e.to_string()),
        };
        let host = self.host.clone();
        let base = self.cfg.checkpoint_base.clone();
        let mode = self.cfg.checkpoint.mode;
        let record = entry.record;
        tokio::task::spawn_blocking(move || {
            precheck_dump(&*host, mode, &base, &record, init_pid).map(|_| ())
        })
        .await
        .unwrap_or_else(|e| Err(format!("checkpoint pre-check panicked: {e}")))
    }

    /// Mark a job as checkpointing; returns whether it was stopped.
    fn claim_for_checkpoint(&self, job: JobId) -> Result<bool, ResponseResult> {
        let mut table = self.table();
        let Some(e) = table.get_mut(&job) else {
            return Err(error(
                HelperErrorCode::JobNotFound,
                format!("job {} is not running", job.0),
            ));
        };
        let was_stopped = match e.phase {
            Phase::Running => false,
            Phase::Stopped => true,
            _ => {
                return Err(error(
                    HelperErrorCode::Busy,
                    format!("job {} is busy", job.0),
                ));
            }
        };
        e.phase = Phase::Checkpointing;
        Ok(was_stopped)
    }

    /// Undo [`Self::claim_for_checkpoint`] after a dump that did not happen.
    fn unclaim(&self, job: JobId, was_stopped: bool) {
        if let Some(e) = self.table().get_mut(&job) {
            e.phase = if was_stopped {
                Phase::Stopped
            } else {
                Phase::Running
            };
        }
    }

    async fn checkpoint_job(
        &self,
        job: JobId,
        options: CheckpointOptions,
        was_stopped: bool,
    ) -> CheckpointOutcome {
        let _heavy = self.heavy.lock().await;
        let _op_end = self.op_ends.track(job);
        let Some(entry) = self.table().get(&job).cloned() else {
            return CheckpointOutcome::refused("job vanished");
        };
        let unit = entry.record.unit.clone();
        // cuda-checkpoint hangs on stopped processes.
        if was_stopped && let Err(e) = self.launcher.signal(&unit, signal::SIGCONT).await {
            self.unclaim(job, was_stopped);
            return CheckpointOutcome::refused(format!("resuming the stopped job: {e}"));
        }
        let init_pid = match self.launcher.init_pid(&unit, entry.record.unit_kind).await {
            Ok(Some(pid)) => Ok(pid),
            Ok(None) => Err("the job is not running".to_owned()),
            Err(e) => Err(e.to_string()),
        };

        let result = match init_pid {
            Err(reason) => Err(DumpError::Refused(reason)),
            Ok(init_pid) => {
                let mut ck = self.cfg.checkpoint.clone();
                ck.criu.tcp_close |= options.tcp_close;
                let cap = options.timeout_secs.map(Duration::from_secs);
                let host = self.host.clone();
                let factory = self.executor.clone();
                let base = self.cfg.checkpoint_base.clone();
                let record = entry.record.clone();
                let op_ends = self.op_ends.clone();
                tokio::task::spawn_blocking(move || {
                    let mut exec = factory();
                    dump_job(
                        &*host,
                        &mut *exec,
                        &ck,
                        &base,
                        &record,
                        init_pid,
                        cap,
                        &|end| op_ends.set(job, end),
                    )
                })
                .await
                .unwrap_or_else(|e| {
                    Err(DumpError::Failed(format!("checkpoint task panicked: {e}")))
                })
            }
        };

        match result {
            Ok(meta) => {
                // Record the dump first: the dumped unit ends `failed`
                // (137), which must not be mistaken for the job failing.
                let mut record = entry.record;
                record.phase = RecordPhase::Checkpointed;
                if let Err(e) = entry.files.write_record(&record) {
                    eprintln!("puzuz-helper: recording checkpoint of job {}: {e}", job.0);
                }
                self.table().remove(&job);
                if let Err(e) = self.launcher.release(&unit).await {
                    eprintln!("puzuz-helper: releasing {unit} after dump: {e}");
                }
                CheckpointOutcome::Success {
                    image_size_bytes: meta.size_bytes,
                }
            }
            Err(e) => {
                self.unclaim(job, was_stopped);
                if was_stopped {
                    let _ = self.launcher.signal(&unit, signal::SIGSTOP).await;
                }
                match e {
                    DumpError::Refused(reason) => {
                        eprintln!(
                            "puzuz-helper: refusing to checkpoint job {}: {reason}",
                            job.0
                        );
                        CheckpointOutcome::refused(reason)
                    }
                    DumpError::Failed(reason) => {
                        eprintln!("puzuz-helper: checkpoint of job {} failed: {reason}", job.0);
                        CheckpointOutcome::failed(reason)
                    }
                }
            }
        }
    }

    // -- restore ----------------------------------------------------------

    fn restore(self: &Arc<Self>, req: RestoreRequest) -> ResponseResult {
        // Counted before the check (see `Self::spawn`).
        let guard = self.inflight.enter();
        if self.is_shutting_down() {
            return error(HelperErrorCode::ShuttingDown, "the helper is shutting down");
        }
        let job = req.job;
        let (record, meta) = match self.check_restore(&req) {
            Ok(v) => v,
            Err(res) => return res,
        };
        let files = JobFiles::new(&self.cfg.jobs_dir(), job);
        {
            let mut table = self.table();
            if table.contains_key(&job) {
                return error(HelperErrorCode::Busy, format!("job {} is live", job.0));
            }
            table.insert(
                job,
                Entry {
                    record: record.clone(),
                    files: files.clone(),
                    phase: Phase::Restoring,
                    pid: None,
                },
            );
        }
        let this = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let outcome = this.restore_job(record, files, meta, &req).await;
            this.emit(Event::RestoreFinished { job, outcome });
        });
        ResponseResult::Accepted
    }

    /// Synchronous restore checks: image, owner, GPU compatibility, VRAM.
    fn check_restore(
        &self,
        req: &RestoreRequest,
    ) -> Result<(JobRecord, CheckpointMeta), ResponseResult> {
        validate::check_image_dir(&self.cfg.checkpoint_base, req.job, &req.image_dir)?;
        let dir = CheckpointDir::for_job(&self.cfg.checkpoint_base, req.job.0);
        let meta = match dir.status() {
            DirStatus::Complete(meta) => *meta,
            other => {
                return Err(error(
                    HelperErrorCode::InvalidRequest,
                    format!("no complete checkpoint for job {}: {other:?}", req.job.0),
                ));
            }
        };
        let record: JobRecord = fs::read(dir.root().join(OWNER_FILE))
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
            .map_err(|e| {
                error(
                    HelperErrorCode::InvalidRequest,
                    format!("reading the checkpoint owner: {e}"),
                )
            })?;
        if record.job != req.job || record.uid != req.uid {
            return Err(error(
                HelperErrorCode::PermissionDenied,
                "the checkpoint belongs to another job or user",
            ));
        }
        validate::check_credentials(&*self.host, record.uid, record.gid, None)?;
        // CRIU fails with "Can't stat mountpoint" / "Failed to open_tree"
        // when a mount point of the job's namespace or the host path of an
        // external bind mount is gone (e.g. under a /run that was rebuilt
        // at boot, or hidden under another mount); say so up front.
        if let Ok(bytes) = fs::read(dir.root().join(MOUNTS_FILE))
            && let Ok(mounts) = serde_json::from_slice::<SavedMounts>(&bytes)
        {
            let missing: Vec<&str> = mounts
                .required_paths()
                .into_iter()
                .filter(|m| fs::symlink_metadata(m).is_err())
                .collect();
            if !missing.is_empty() {
                return Err(error(
                    HelperErrorCode::InvalidRequest,
                    format!(
                        "paths the checkpoint's mounts depend on are missing on this host: {missing:?}"
                    ),
                ));
            }
        }

        if !meta.gpus.is_empty() {
            let snap = self
                .host
                .gpu_snapshot()
                .map_err(|e| error(HelperErrorCode::Internal, format!("querying GPUs: {e}")))?;
            let current: Vec<GpuMeta> = snap
                .gpus
                .iter()
                .filter(|g| req.gpus.iter().any(|r| r.0 == g.index))
                .map(|g| GpuMeta {
                    index: g.index,
                    uuid: g.uuid.clone(),
                    model: g.model.clone(),
                })
                .collect();
            let problems = check_compatibility(
                &meta,
                &current,
                snap.driver_version.as_deref(),
                meta.mode == CudaMode::Plugin,
            );
            if !problems.is_empty() {
                return Err(error(
                    HelperErrorCode::InvalidRequest,
                    format!("checkpoint is not compatible with this host: {problems:?}"),
                ));
            }
            let free: u64 = snap
                .gpus
                .iter()
                .filter(|g| req.gpus.iter().any(|r| r.0 == g.index))
                .map(|g| g.free_bytes)
                .sum();
            if free < meta.vram_bytes {
                return Err(error(
                    HelperErrorCode::Busy,
                    format!(
                        "not enough free GPU memory: need {} bytes, {free} free",
                        meta.vram_bytes
                    ),
                ));
            }
        }
        Ok((record, meta))
    }

    async fn restore_job(
        &self,
        mut record: JobRecord,
        files: JobFiles,
        meta: CheckpointMeta,
        req: &RestoreRequest,
    ) -> RestoreOutcome {
        let _heavy = self.heavy.lock().await;
        let job = req.job;
        let _op_end = self.op_ends.track(job);
        let fail = |this: &Self, reason: String| {
            this.table().remove(&job);
            RestoreOutcome::failed(reason)
        };
        let dir = CheckpointDir::for_job(&self.cfg.checkpoint_base, job.0);
        if let Err(e) = files.prepare(true) {
            return fail(self, format!("preparing job files: {e}"));
        }
        // Name the new unit before starting it, so a helper crash during
        // the restore can find it again.
        record.restores += 1;
        record.unit = restore_unit_name(job, record.restores);
        record.phase = RecordPhase::Checkpointed;
        if let Err(e) = files.write_record(&record) {
            return fail(self, format!("writing the job record: {e}"));
        }
        let unit = record.unit.clone();
        let _ = fs::remove_file(dir.restore_pidfile());
        let ck = self.cfg.checkpoint.clone();
        // One deadline for `criu restore` and the CUDA restore + unlock.
        let deadline = Deadline::after(ck.timeouts.operation_deadline(meta.size_bytes));
        self.op_ends.set(job, deadline.at + OP_SLACK);
        let past_deadline = |what: String| {
            if deadline.remaining().is_zero() {
                format!(
                    "{what} (the restore did not finish within its deadline of {}s for {} \
                     of checkpoint data; the image is kept)",
                    deadline.budget.as_secs(),
                    gib(meta.size_bytes)
                )
            } else {
                what
            }
        };
        let target = RestoreTarget {
            images_dir: dir.images_dir(),
            log_file: dir.restore_log(),
            detached: true,
            root: self.cfg.criu_root.clone(),
            pidfile: Some(dir.restore_pidfile()),
            inherit_fds: Vec::new(),
        };
        let steps = match plan::restore_criu_plan(&ck, &target) {
            Ok(s) => s,
            Err(e) => return fail(self, e.to_string()),
        };
        let Some(plan::Step::Run {
            command, timeout, ..
        }) = steps.into_iter().next()
        else {
            return fail(self, "empty restore plan".into());
        };
        let spec = RestoreSpec {
            job,
            unit: unit.clone(),
            command,
            pidfile: dir.restore_pidfile(),
            timeout: timeout.min(deadline.remaining()),
            criu_root: self.cfg.criu_root.clone(),
        };
        let pid = match self.launcher.restore(&spec).await {
            Ok(pid) => pid,
            Err(e) if e.code == HelperErrorCode::ShuttingDown => {
                // Abandoned by a stopping helper before systemd started the
                // unit: nothing ran and the image is untouched. Not a
                // failure of the restore; the daemon queues the job again.
                self.table().remove(&job);
                return RestoreOutcome::Failed {
                    reason: e.to_string(),
                    code: Some(HelperErrorCode::ShuttingDown),
                };
            }
            Err(e) => {
                let summary = fs::read_to_string(dir.restore_log())
                    .map(|l| probe::summarize_criu_log(&l, 5).join("; "))
                    .unwrap_or_default();
                return fail(self, past_deadline(format!("{e}: {summary}")));
            }
        };

        if meta.mode == CudaMode::Manual && !meta.cuda_ns_pids.is_empty() {
            let host = self.host.clone();
            let factory = self.executor.clone();
            let wanted = meta.cuda_ns_pids.clone();
            let gpus = req.gpus.clone();
            let saved = meta.gpus.clone();
            let ck2 = ck.clone();
            let res = tokio::task::spawn_blocking(move || -> Result<(), String> {
                let host_pids = host.resolve_ns_pids(pid, &wanted)?;
                let map = device_map(&*host, &saved, &gpus)?;
                let steps = plan::restore_cuda_plan(&ck2, &host_pids, map.as_ref());
                let mut exec = factory();
                run_plan_until(&mut *exec, steps, Some(&deadline))
                    .map(|_| ())
                    .map_err(|f| format!("CUDA restore failed: {f}"))
            })
            .await
            .unwrap_or_else(|e| Err(format!("restore task panicked: {e}")));
            if let Err(reason) = res {
                // The tree runs without its GPU state: kill it. The image
                // is still there.
                let _ = self.launcher.signal(&unit, signal::SIGKILL).await;
                let _ = self.launcher.release(&unit).await;
                return fail(self, past_deadline(reason));
            }
        }

        record.phase = RecordPhase::Live;
        record.unit_kind = UnitKind::Restored;
        if let Err(e) = files.write_record(&record) {
            eprintln!("puzuz-helper: recording restore of job {}: {e}", job.0);
        }
        if let Some(e) = self.table().get_mut(&job) {
            e.record = record;
            e.phase = Phase::Running;
            e.pid = Some(pid);
        }
        if let Err(e) = dir.remove() {
            eprintln!("puzuz-helper: removing the image of job {}: {e}", job.0);
        }
        RestoreOutcome::Success { pid }
    }

    // -- shutdown ---------------------------------------------------------

    /// Stop of the helper alone (not of the OS): refuse new spawns and
    /// restores, then wait for the spawns, restores and checkpoints in
    /// flight to finish, so that none is cut off half-way (their answers
    /// and events still reach the daemon). Spawns and restores whose units
    /// systemd has not begun to start are abandoned instead (see
    /// [`Launcher::stop_waiting`]): they would wait for this very stop.
    /// The jobs keep running.
    pub async fn drain(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.launcher.stop_waiting();
        self.inflight.wait_idle().await;
    }

    /// First SIGTERM of an OS shutdown: [`Self::shutdown_jobs`], calling
    /// `extend` every few seconds with how much more time to ask systemd
    /// for (`EXTEND_TIMEOUT_USEC`; see [`OpEnds::stop_budget`]). Every
    /// operation it waits for has a deadline, so the extensions end.
    pub async fn shutdown(
        self: &Arc<Self>,
        before_each: impl Fn(JobId),
        extend: impl Fn(Duration),
    ) -> ShutdownManifest {
        let work = self.shutdown_jobs(before_each);
        tokio::pin!(work);
        let mut tick = tokio::time::interval(EXTEND_INTERVAL);
        loop {
            tokio::select! {
                manifest = &mut work => return manifest,
                _ = tick.tick() => {
                    if let Some(budget) = self.op_ends.stop_budget() {
                        extend(budget);
                    }
                }
            }
        }
    }

    /// Refuse new spawns/restores, wait for running operations, then dump
    /// every live job one by one (`no_checkpoint` jobs are skipped) and
    /// write the manifest. `before_each` is called before each dump.
    async fn shutdown_jobs(self: &Arc<Self>, before_each: impl Fn(JobId)) -> ShutdownManifest {
        self.shutting_down.store(true, Ordering::SeqCst);
        self.inflight.wait_idle().await;
        let live: Vec<(JobId, bool)> = self
            .table()
            .iter()
            .filter(|(_, e)| matches!(e.phase, Phase::Running | Phase::Stopped))
            .map(|(id, e)| (*id, e.record.no_checkpoint))
            .collect();
        let mut entries = Vec::new();
        for (job, no_checkpoint) in live {
            if no_checkpoint {
                entries.push(ManifestEntry {
                    job,
                    result: ManifestResult::Skipped,
                });
                continue;
            }
            let result = match self.claim_for_checkpoint(job) {
                Err(_) => continue, // exited meanwhile
                Ok(was_stopped) => {
                    before_each(job);
                    let outcome = self
                        .checkpoint_job(job, CheckpointOptions::default(), was_stopped)
                        .await;
                    self.emit(Event::CheckpointFinished {
                        job,
                        outcome: outcome.clone(),
                    });
                    let refused = outcome.is_refused();
                    match outcome {
                        CheckpointOutcome::Success { image_size_bytes } => {
                            ManifestResult::Checkpointed { image_size_bytes }
                        }
                        CheckpointOutcome::Failed { reason, .. } => {
                            ManifestResult::Failed { reason, refused }
                        }
                    }
                }
            };
            entries.push(ManifestEntry { job, result });
        }
        let manifest = ShutdownManifest {
            created_at: now_ms(),
            jobs: entries,
        };
        let path = self.cfg.checkpoint_base.join(SHUTDOWN_MANIFEST);
        let res = fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.cfg.checkpoint_base)
            .and_then(|()| {
                write_atomic(
                    &path,
                    &serde_json::to_vec_pretty(&manifest).map_err(io::Error::from)?,
                )
            });
        if let Err(e) = res {
            eprintln!("puzuz-helper: writing {}: {e}", path.display());
        }
        manifest
    }
}

/// `--device-map` for a manual restore when GPU UUIDs changed.
fn device_map<H: HostProbe + ?Sized>(
    host: &H,
    saved: &[GpuMeta],
    gpus: &[puzuz_protocol::helper::GpuIndex],
) -> Result<Option<DeviceMap>, String> {
    if saved.is_empty() {
        return Ok(None);
    }
    let snap = host
        .gpu_snapshot()
        .map_err(|e| format!("querying GPUs: {e}"))?;
    let mut pairs = Vec::new();
    for s in saved {
        let cur = snap
            .gpus
            .iter()
            .find(|g| g.index == s.index && gpus.iter().any(|r| r.0 == g.index))
            .ok_or_else(|| format!("GPU {} is not available", s.index))?;
        pairs.push((s.uuid.clone(), cur.uuid.clone()));
    }
    if pairs.iter().all(|(a, b)| a == b) {
        return Ok(None);
    }
    DeviceMap::new(pairs).map(Some).map_err(|e| e.to_string())
}

/// Why [`dump_job`] produced no image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DumpError {
    /// The pre-checks failed before anything ran against the job; it keeps
    /// running as it was.
    Refused(String),
    /// The dump was attempted and failed (CRIU rolled back). The reason
    /// names the preserved CRIU log, if any.
    Failed(String),
}

/// What [`precheck_dump`] learned, for the dump that follows.
#[derive(Debug, Clone)]
pub struct DumpPrecheck {
    procs: Vec<ProcInfo>,
    /// Without the CUDA PIDs yet.
    meta: CheckpointMeta,
    mounts: Vec<String>,
}

/// The checks before a dump, which only read the job's state: the image
/// base, the job's mounts (an image that depends on a hidden mount could
/// never be restored, not even on this host) and the free space for its
/// memory. An error means the dump is refused and the job is left alone.
pub fn precheck_dump<H: HostProbe + ?Sized>(
    host: &H,
    mode: CudaMode,
    base: &Path,
    record: &JobRecord,
    init_pid: u32,
) -> Result<DumpPrecheck, String> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(base)
        .map_err(|e| format!("creating {}: {e}", base.display()))?;
    if host.is_tmpfs(base).map_err(|e| e.to_string())? {
        return Err(format!("{} is on tmpfs", base.display()));
    }
    let procs = host
        .job_processes(init_pid)
        .map_err(|e| format!("listing the job's processes: {e}"))?;
    let rss: u64 = procs.iter().map(|p| p.rss_bytes).sum();

    let mut meta = CheckpointMeta::new(record.job.0, now_ms(), mode);
    if !record.gpus.is_empty() {
        let snap = host
            .gpu_snapshot()
            .map_err(|e| format!("querying GPUs: {e}"))?;
        meta.driver_version = snap.driver_version.clone();
        meta.gpus = snap
            .gpus
            .iter()
            .filter(|g| record.gpus.iter().any(|r| r.0 == g.index))
            .map(|g| GpuMeta {
                index: g.index,
                uuid: g.uuid.clone(),
                model: g.model.clone(),
            })
            .collect();
        // VRAM in use (for the free-space and restore checks).
        for app in &snap.compute_apps {
            if procs.iter().any(|p| p.host_pid == app.pid) {
                meta.vram_bytes += app.used_bytes;
            }
        }
    }
    let mounts = host
        .mount_points(init_pid)
        .map_err(|e| format!("reading the job's mounts: {e}"))?;
    let unreachable = host
        .unreachable_mounts(init_pid)
        .map_err(|e| format!("reading the job's mounts: {e}"))?;
    if !unreachable.is_empty() {
        return Err(format!(
            "the job's mounts {unreachable:?} are only reachable through mounts hidden under \
             other mounts, so CRIU could not restore them; restart the job to get a clean \
             mount namespace"
        ));
    }
    let available = host.available_bytes(base).map_err(|e| e.to_string())?;
    space::check_free_space(rss, meta.vram_bytes, available).map_err(|e| e.to_string())?;
    Ok(DumpPrecheck {
        procs,
        meta,
        mounts,
    })
}

/// `bytes` as GiB for messages.
fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / f64::from(1u32 << 30))
}

/// The blocking part of a checkpoint: [`precheck_dump`], the dump plan with
/// rollback, `meta.json`, owner record and the `COMPLETE` marker.
///
/// The plan runs within a [`Deadline`] of
/// [`plan::Timeouts::operation_deadline`] for the job's RSS + VRAM (at most
/// `cap`); `on_deadline` is told the latest time the dump may take, its
/// rollback included.
#[allow(clippy::too_many_arguments)]
pub fn dump_job<H: HostProbe + ?Sized>(
    host: &H,
    exec: &mut dyn Executor,
    ck: &CheckpointConfig,
    base: &Path,
    record: &JobRecord,
    init_pid: u32,
    cap: Option<Duration>,
    on_deadline: &dyn Fn(Instant),
) -> Result<CheckpointMeta, DumpError> {
    let DumpPrecheck {
        procs,
        mut meta,
        mounts,
    } = precheck_dump(host, ck.mode, base, record, init_pid).map_err(DumpError::Refused)?;
    let mut cuda_pids = Vec::new();
    if !record.gpus.is_empty() {
        // Every process of the namespace that answers
        // `cuda-checkpoint --get-state` uses CUDA (PoC: works for spawn
        // DataLoader workers, which do not).
        let all: Vec<u32> = procs.iter().map(|p| p.host_pid).collect();
        for pid in host
            .cuda_pids(&all)
            .map_err(|e| DumpError::Refused(format!("finding CUDA processes: {e}")))?
        {
            if let Some(p) = procs.iter().find(|p| p.host_pid == pid) {
                cuda_pids.push(p.host_pid);
                meta.cuda_ns_pids.push(p.ns_pid);
            }
        }
    }

    let dir = CheckpointDir::for_job(base, record.job.0);
    dir.prepare()
        .map_err(|e| DumpError::Refused(format!("preparing {}: {e}", dir.root().display())))?;
    let target = DumpTarget {
        tree_root_pid: init_pid,
        images_dir: dir.images_dir(),
        log_file: dir.dump_log(),
        leave_running: false,
    };
    let steps = plan::dump_plan(ck, &target, &cuda_pids);
    let size = procs.iter().map(|p| p.rss_bytes).sum::<u64>() + meta.vram_bytes;
    let mut budget = ck.timeouts.operation_deadline(size);
    if let Some(cap) = cap {
        budget = budget.min(cap);
    }
    let deadline = Deadline::after(budget);
    on_deadline(deadline.at + ck.timeouts.rollback_budget(cuda_pids.len()) + OP_SLACK);
    if let Err(failure) = run_with_rollback(exec, ck, steps, Some(&deadline)) {
        let mut reason = failure.to_string();
        if matches!(failure.error, StepError::DeadlineExceeded { .. }) {
            reason = format!(
                "the checkpoint did not finish within its deadline of {}s for {} of memory: \
                 {reason}",
                budget.as_secs(),
                gib(size)
            );
        }
        match failure.rollback.iter().find(|r| r.result.is_err()) {
            None if failure.rollback.is_empty() => {}
            None => reason.push_str("; the job was resumed"),
            Some(r) => match (&r.step, &r.result) {
                (plan::Step::Thaw { .. }, Err(StepError::Io(e)))
                    if e.kind() == io::ErrorKind::NotFound =>
                {
                    reason.push_str("; the job is gone (CRIU had already stopped it)");
                }
                (_, Err(e)) => reason.push_str(&format!("; rollback incomplete: {e}")),
                (_, Ok(_)) => unreachable!(),
            },
        }
        if let Ok(log) = fs::read_to_string(dir.dump_log()) {
            let lines = probe::summarize_criu_log(&log, 5);
            if !lines.is_empty() {
                reason = format!("{reason}: {}", lines.join("; "));
            }
        }
        return Err(DumpError::Failed(discard_failed_dump(base, &dir, reason)));
    }
    let finish = || -> Result<CheckpointMeta, String> {
        let mut owner = record.clone();
        owner.phase = RecordPhase::Checkpointed;
        let owner_json = serde_json::to_vec_pretty(&owner).map_err(|e| e.to_string())?;
        write_atomic(&dir.root().join(OWNER_FILE), &owner_json).map_err(|e| e.to_string())?;
        let saved = SavedMounts::Full {
            mount_points: mounts,
            external_sources: fs::read_to_string(dir.dump_log())
                .map(|log| probe::criu_external_mounts(&log))
                .unwrap_or_default(),
        };
        let mounts_json = serde_json::to_vec_pretty(&saved).map_err(|e| e.to_string())?;
        write_atomic(&dir.root().join(MOUNTS_FILE), &mounts_json).map_err(|e| e.to_string())?;
        dir.finalize(meta).map_err(|e| e.to_string())
    };
    finish().map_err(|reason| DumpError::Failed(discard_failed_dump(base, &dir, reason)))
}

/// After a failed dump: keep CRIU's log for administrators under
/// `<base>/failed-logs/` (see [`layout::preserve_failed_log`]) and remove
/// the checkpoint directory. Returns `reason` with the log's path.
fn discard_failed_dump(base: &Path, dir: &CheckpointDir, reason: String) -> String {
    match layout::preserve_failed_log(base, dir, now_ms(), layout::FAILED_LOGS_KEEP) {
        Ok(kept) => {
            if let Err(e) = dir.remove() {
                eprintln!(
                    "puzuz-helper: removing the failed checkpoint {}: {e}",
                    dir.root().display()
                );
            }
            match kept {
                Some(log) => format!("{reason} (CRIU log: {})", log.display()),
                None => reason,
            }
        }
        Err(e) => {
            // Free the space at least; dump.log stays where it is until
            // the checkpoint directory is deleted.
            eprintln!(
                "puzuz-helper: moving {} to {}: {e}",
                dir.dump_log().display(),
                base.join(layout::FAILED_LOGS_SUBDIR).display()
            );
            let _ = fs::remove_dir_all(dir.images_dir());
            reason
        }
    }
}
