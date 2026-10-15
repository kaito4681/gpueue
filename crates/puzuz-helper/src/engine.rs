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

use puzuz_checkpoint::exec::{Deadline, StepError, run_dump_with_rollback, run_plan_until};
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
use crate::deadmounts::DetachedMount;
use crate::devices::DeviceRule;
use crate::events::EventStore;
use crate::host::{DumpHazards, HostProbe, ProcInfo};
use crate::jobs::{self, JobFiles, JobRecord, RecordPhase, ShimSpec, UnitKind};
use crate::launcher::{
    LaunchError, LaunchSpec, Launcher, RestoreSpec, UnitState, restore_unit_name, unit_name,
};
use crate::mounts::SourceMount;
use crate::validate::{self, Rejection};

/// File in a checkpoint directory recording the job's owner, so a restore
/// can check it without trusting the daemon.
pub const OWNER_FILE: &str = "helper-job.json";
/// File in a checkpoint directory recording what the job's mount namespace
/// depends on ([`SavedMounts`]); a restore needs all of it to exist.
pub const MOUNTS_FILE: &str = "helper-mounts.json";
/// File in a job's directory with the request of the check CRIU runs
/// before it resumes a restored job (see [`crate::access::run_restore_hook`]);
/// its result is written next to it.
pub const RESTORE_CHECK_FILE: &str = "restore-check.json";
/// The `--action-script` that runs that check (CRIU passes no arguments).
pub const RESTORE_CHECK_SCRIPT: &str = "restore-check.sh";
/// File in a checkpoint directory with the request of the hook CRIU runs
/// after a dump, while the job is still stopped (see
/// [`crate::access::run_dump_hook`]); its result is written next to it.
pub const DUMP_HOOK_FILE: &str = "dump-hook.json";
/// The `--action-script` of `criu dump` that runs that hook.
pub const DUMP_HOOK_SCRIPT: &str = "dump-hook.sh";

/// Writes the root-only `--action-script` `<script>` that runs
/// `<exe> <subcommand> <request>` (CRIU passes no arguments) and returns
/// its path.
fn write_action_script(
    script: &Path,
    exe: &Path,
    subcommand: &str,
    request: &Path,
) -> Result<String, String> {
    let quote = |p: &Path| -> Result<String, String> {
        let s = p
            .to_str()
            .ok_or_else(|| format!("{} is not UTF-8", p.display()))?;
        if s.contains('\'') {
            return Err(format!("{s} contains a quote"));
        }
        Ok(format!("'{s}'"))
    };
    let body = format!(
        "#!/bin/sh\nexec {} {subcommand} {}\n",
        quote(exe)?,
        quote(request)?
    );
    write_atomic(script, body.as_bytes())
        .and_then(|()| fs::set_permissions(script, fs::Permissions::from_mode(0o700)))
        .map_err(|e| format!("writing {}: {e}", script.display()))?;
    script
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{} is not UTF-8", script.display()))
}

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
        /// The filesystems those paths were on (in the helper's
        /// namespace); a restore needs them mounted again.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        source_mounts: Vec<SourceMount>,
        /// Regular files on the host's filesystems that the job had open,
        /// and its working directories; CRIU reopens them by path.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        open_files: Vec<String>,
        /// Files the job held at the dump, for the check after a restore
        /// (see [`crate::access::HeldFile`]).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        held_files: Vec<crate::access::HeldFile>,
        /// FUSE and network mounts that were gone or did not answer at the
        /// dump, which the job did not use (see [`crate::deadmounts`]).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        detached_mounts: Vec<DetachedMount>,
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
                ..
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

    /// See [`SavedMounts::Full`].
    pub fn source_mounts(&self) -> &[SourceMount] {
        match self {
            SavedMounts::Full { source_mounts, .. } => source_mounts,
            SavedMounts::MountPoints(_) => &[],
        }
    }

    /// See [`SavedMounts::Full`].
    pub fn open_files(&self) -> &[String] {
        match self {
            SavedMounts::Full { open_files, .. } => open_files,
            SavedMounts::MountPoints(_) => &[],
        }
    }

    /// See [`SavedMounts::Full`].
    pub fn held_files(&self) -> &[crate::access::HeldFile] {
        match self {
            SavedMounts::Full { held_files, .. } => held_files,
            SavedMounts::MountPoints(_) => &[],
        }
    }

    /// See [`SavedMounts::Full`].
    pub fn detached_mounts(&self) -> &[DetachedMount] {
        match self {
            SavedMounts::Full {
                detached_mounts, ..
            } => detached_mounts,
            SavedMounts::MountPoints(_) => &[],
        }
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
            Request::Restore(r) => self.restore(r).await,
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
        let creds = match validate::check_spawn(&*self.host, &self.cfg.job_policy, &req) {
            Ok(c) => c,
            Err(r) => return r.into(),
        };
        if !creds.dropped.is_empty() {
            eprintln!(
                "puzuz-helper: job {} of {}: leaving out the supplementary groups {:?} \
                 (job_denied_groups)",
                req.job.0, creds.user.name, creds.dropped
            );
        }
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
            dump_groups: None,
            dumping: false,
            restoring: false,
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
                self.cfg.gc_roots.remove(req.job);
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

        let devices = self
            .unit_devices(req.job, &req.gpus)
            .map_err(|e| error(HelperErrorCode::SpawnFailed, e))?;
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
        // A new job: its shim is this helper (a stale root of the id goes).
        self.cfg.gc_roots.remove(req.job);
        self.cfg.gc_roots.add(req.job);
        let spec = LaunchSpec {
            job: req.job,
            unit: record.unit.clone(),
            files: files.clone(),
            argv,
            envs,
            devices,
        };
        self.launcher.spawn(&spec).await.map_err(Into::into)
    }

    /// The device access of a unit of `job` with `gpus` (see
    /// [`crate::devices`]): `None` without `gpu_device_isolation`.
    fn unit_devices(
        &self,
        job: JobId,
        gpus: &[puzuz_protocol::helper::GpuIndex],
    ) -> Result<Option<Vec<DeviceRule>>, String> {
        if !self.cfg.gpu_device_isolation {
            return Ok(None);
        }
        if gpus.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let minors = self.host.gpu_device_minors().map_err(|e| {
            format!(
                "finding the device nodes of the GPUs for gpu_device_isolation \
                 ({}): {e}",
                crate::devices::NVIDIA_GPUS_DIR
            )
        })?;
        let unknown: Vec<String> = gpus
            .iter()
            .filter(|g| g.0 as usize >= minors.len())
            .map(|g| g.0.to_string())
            .collect();
        if !unknown.is_empty() {
            eprintln!(
                "puzuz-helper: job {}: the NVIDIA driver knows no GPU {}; the job cannot open it \
                 (gpu_device_isolation)",
                job.0,
                unknown.join(", ")
            );
        }
        Ok(Some(crate::devices::job_device_allow(gpus, &minors)))
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
        // Anything else would reach the job's root shim instead of the job
        // (see `crate::shim::FORWARDED`).
        if !matches!(sig, signal::SIGSTOP | signal::SIGCONT | signal::SIGKILL)
            && !crate::shim::is_forwardable(sig)
        {
            return error(
                HelperErrorCode::InvalidRequest,
                format!("signal {sig} cannot be sent to a job"),
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
                uid: Some(e.record.uid),
                gpus: Some(e.record.gpus.clone()),
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
        // Not live and no image: nothing needs the binary that ran it.
        self.cfg.gc_roots.remove(job);
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
        self.cfg.gc_roots.remove(job);
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
    /// report jobs that exited while the helper was away and restores and
    /// dumps it did not see to the end, finalize successful dumps it left
    /// unfinalized, drop other incomplete checkpoints.
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
            let dir = CheckpointDir::for_job(&self.cfg.checkpoint_base, job.0);
            if record.phase == RecordPhase::Live && record.dumping {
                // The helper stopped while `criu dump` might run. If the
                // dump succeeded, the job's processes are gone and the
                // image is all that is left of it, even if systemd does not
                // report its unit ended yet (deactivating). A new dump never
                // replaces a checkpoint (see `CheckpointDir::prepare`), so a
                // successful dump in the directory is this dump unless it
                // is the image the job was restored from.
                record.dumping = false;
                let dumped = match dir.status() {
                    _ if was_restored_from(&dir) => None,
                    DirStatus::Complete(meta) => Some(Ok(meta.size_bytes)),
                    _ if dir.is_unfinalized_dump() => {
                        Some(finish_dump(&*self.host, &dir, None).map(|m| m.size_bytes))
                    }
                    _ => None,
                };
                if let Some(result) = dumped {
                    eprintln!(
                        "puzuz-helper: job {} was dumped when the helper stopped; it is \
                         checkpointed",
                        job.0
                    );
                    record.phase = RecordPhase::Checkpointed;
                    files.write_record(&record)?;
                    if state != UnitState::Gone {
                        let _ = self.launcher.release(&record.unit).await;
                    }
                    let outcome = match result {
                        Ok(size) => CheckpointOutcome::Success {
                            image_size_bytes: size,
                        },
                        Err(e) => {
                            let reason = unfinalized_reason(&dir, &e);
                            eprintln!("puzuz-helper: checkpoint of job {}: {reason}", job.0);
                            CheckpointOutcome::unfinalized(reason)
                        }
                    };
                    self.emit(Event::CheckpointFinished { job, outcome });
                    continue;
                }
                // The dump did not succeed: the job ran on (or exited).
                files.write_record(&record)?;
            }
            if record.phase == RecordPhase::Checkpointed {
                // The helper stopped during a restore, before it recorded
                // the outcome (older helpers did not set `restoring`; a
                // restore unit that still runs tells for them).
                if record.restoring || state == UnitState::Running && record.restores > 0 {
                    // criu may still be running, or the restored tasks may
                    // not have been checked yet (see
                    // `crate::access::run_restore_hook`), or their CUDA
                    // state not restored and unlocked. Kill the tree and
                    // report the restore as failed; the image is kept for
                    // another try.
                    eprintln!(
                        "puzuz-helper: job {} was being restored when the helper stopped; \
                         killing {} (the image is kept)",
                        job.0, record.unit
                    );
                    if state == UnitState::Running {
                        let _ = self.launcher.signal(&record.unit, signal::SIGKILL).await;
                    }
                    record.restoring = false;
                    if let Err(e) = files.write_record(&record) {
                        eprintln!("puzuz-helper: updating the record of job {}: {e}", job.0);
                    }
                    self.emit(Event::RestoreFinished {
                        job,
                        outcome: RestoreOutcome::failed(
                            "the helper stopped while the job was being restored; the \
                             restored processes were killed and the image is kept",
                        ),
                    });
                }
                // The dumped (failed) unit, a failed or interrupted restore
                // unit.
                if state != UnitState::Gone {
                    let _ = self.launcher.release(&record.unit).await;
                }
                match dir.status() {
                    DirStatus::Complete(_) => {}
                    // Kept even if it cannot be finalized now: a restore
                    // tries again.
                    _ if dir.is_unfinalized_dump() => {
                        if let Err(e) = finish_dump(&*self.host, &dir, None) {
                            eprintln!(
                                "puzuz-helper: finalizing the checkpoint of job {}: {e}; the \
                                 image is kept",
                                job.0
                            );
                        }
                    }
                    _ => {
                        let _ = files.remove();
                    }
                }
                continue;
            }
            match state {
                UnitState::Running => {
                    // A restore finished, but the helper stopped before it
                    // removed the image (or removing it failed). Only that
                    // image: anything else may be the only copy of the job.
                    if record.unit_kind == UnitKind::Restored
                        && matches!(dir.status(), DirStatus::Complete(_))
                        && was_restored_from(&dir)
                    {
                        eprintln!(
                            "puzuz-helper: removing the image of the restored job {}",
                            job.0
                        );
                        let _ = dir.remove();
                    }
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
                let dir = CheckpointDir::for_job(&self.cfg.checkpoint_base, id);
                // A successful dump holds the only copy of its job: never
                // removed here.
                if dir.is_unfinalized_dump() {
                    match finish_dump(&*self.host, &dir, None) {
                        Ok(_) => eprintln!("puzuz-helper: finalized the checkpoint of job {id}"),
                        Err(e) => eprintln!(
                            "puzuz-helper: finalizing the checkpoint of job {id}: {e}; the \
                             image is kept"
                        ),
                    }
                    continue;
                }
                eprintln!("puzuz-helper: removing incomplete checkpoint of job {id}: {reason:?}");
                let _ = dir.remove();
            }
        }
        // GC roots of jobs that have no record any more (see
        // `crate::gcroots`).
        let left: Vec<JobId> = jobs::scan(&self.cfg.jobs_dir())?
            .into_iter()
            .map(|(_, record)| record.job)
            .collect();
        self.cfg.gc_roots.retain(|job| left.contains(&job));
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
        let private = self.cfg.mounts.tmpfs.clone();
        tokio::task::spawn_blocking(move || {
            precheck_dump(&*host, mode, &base, &record, init_pid, &private).map(|_| ())
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

        // A helper that stops while `criu dump` runs finds out at its next
        // start whether the dump succeeded (see `Self::recover`).
        let mut dumping = entry.record.clone();
        dumping.dumping = true;
        let init_pid = init_pid.and_then(|pid| {
            entry
                .files
                .write_record(&dumping)
                .map(|()| pid)
                .map_err(|e| format!("writing the job record: {e}"))
        });
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
                let private = self.cfg.mounts.tmpfs.clone();
                let helper_exe = self.cfg.helper_exe.clone();
                tokio::task::spawn_blocking(move || {
                    let mut exec = factory();
                    dump_job(
                        &*host,
                        &mut *exec,
                        &ck,
                        &base,
                        &record,
                        init_pid,
                        &private,
                        Some(&helper_exe),
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

        let dumped = match result {
            Ok(meta) => Ok(CheckpointOutcome::Success {
                image_size_bytes: meta.size_bytes,
            }),
            Err(DumpError::Unfinalized(reason)) => {
                eprintln!("puzuz-helper: checkpoint of job {}: {reason}", job.0);
                Ok(CheckpointOutcome::unfinalized(reason))
            }
            Err(e) => Err(e),
        };
        match dumped {
            Ok(outcome) => {
                // Record the dump first: the dumped unit ends `failed`
                // (137), which must not be mistaken for the job failing.
                let mut record = entry.record;
                record.phase = RecordPhase::Checkpointed;
                record.dumping = false;
                if let Err(e) = entry.files.write_record(&record) {
                    eprintln!("puzuz-helper: recording checkpoint of job {}: {e}", job.0);
                }
                self.table().remove(&job);
                if let Err(e) = self.launcher.release(&unit).await {
                    eprintln!("puzuz-helper: releasing {unit} after dump: {e}");
                }
                outcome
            }
            Err(e) => {
                if let Err(e) = entry.files.write_record(&entry.record) {
                    eprintln!("puzuz-helper: updating the record of job {}: {e}", job.0);
                }
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
                    DumpError::Unfinalized(_) => unreachable!("handled above"),
                }
            }
        }
    }

    // -- restore ----------------------------------------------------------

    async fn restore(self: &Arc<Self>, req: RestoreRequest) -> ResponseResult {
        // Counted before the check (see `Self::spawn`).
        let guard = self.inflight.enter();
        if self.is_shutting_down() {
            return error(HelperErrorCode::ShuttingDown, "the helper is shutting down");
        }
        let job = req.job;
        if let Err(r) = validate::check_image_dir(&self.cfg.checkpoint_base, job, &req.image_dir) {
            return r.into();
        }
        let files = JobFiles::new(&self.cfg.jobs_dir(), job);
        // Claimed before the checks, so that the checkpoint cannot be
        // deleted (which refuses live jobs) while it is checked or
        // restored; the entry's record is replaced by the image's once the
        // checks passed.
        {
            let mut table = self.table();
            if table.contains_key(&job) {
                return error(HelperErrorCode::Busy, format!("job {} is live", job.0));
            }
            table.insert(
                job,
                Entry {
                    record: JobRecord {
                        job,
                        uid: req.uid,
                        gid: puzuz_protocol::helper::Gid(u32::MAX),
                        log_path: Default::default(),
                        gpus: req.gpus.clone(),
                        no_checkpoint: false,
                        phase: RecordPhase::Checkpointed,
                        unit_kind: UnitKind::Restored,
                        unit: String::new(),
                        restores: 0,
                        dump_groups: None,
                        dumping: false,
                        restoring: true,
                    },
                    files: files.clone(),
                    phase: Phase::Restoring,
                    pid: None,
                },
            );
        }
        // The checks block (on the GPU tools, the access-check child, the
        // mounts), so they run off the async workers.
        let this = self.clone();
        let checked_req = req.clone();
        let checked = tokio::task::spawn_blocking(move || this.check_restore(&checked_req))
            .await
            .unwrap_or_else(|e| {
                Err(error(
                    HelperErrorCode::Internal,
                    format!("the restore checks panicked: {e}"),
                ))
            });
        let (record, meta, allowed_gids) = match checked {
            Ok(v) => v,
            Err(res) => {
                self.table().remove(&job);
                return res;
            }
        };
        if let Some(e) = self.table().get_mut(&job) {
            e.record = record.clone();
        }
        let this = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let outcome = this
                .restore_job(record, files, meta, allowed_gids, &req)
                .await;
            this.emit(Event::RestoreFinished { job, outcome });
        });
        ResponseResult::Accepted
    }

    /// Synchronous restore checks: image, owner and groups, the paths CRIU
    /// will open (see [`Self::check_image_paths`]), GPU compatibility,
    /// VRAM. Returns the owner's groups too.
    fn check_restore(
        &self,
        req: &RestoreRequest,
    ) -> Result<(JobRecord, CheckpointMeta, Vec<u32>), ResponseResult> {
        validate::check_image_dir(&self.cfg.checkpoint_base, req.job, &req.image_dir)?;
        let dir = CheckpointDir::for_job(&self.cfg.checkpoint_base, req.job.0);
        let meta = match dir.status() {
            DirStatus::Complete(meta) => *meta,
            // A successful dump the helper could not finalize then.
            _ if dir.is_unfinalized_dump() => {
                finish_dump(&*self.host, &dir, None).map_err(|e| {
                    error(
                        HelperErrorCode::Internal,
                        format!(
                            "the checkpoint of job {} was dumped but not finalized, and \
                             finalizing it failed again: {e}; the image is kept",
                            req.job.0
                        ),
                    )
                })?
            }
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
        let creds = validate::check_credentials(
            &*self.host,
            &self.cfg.job_policy,
            record.uid,
            record.gid,
            None,
        )?;
        // CRIU restores the groups the job had at the dump (older images
        // did not record them; the check after the restore covers those,
        // denied groups included: they are not in `allowed_gids`).
        if let Some(groups) = &record.dump_groups {
            let denied: Vec<String> = groups
                .iter()
                .filter(|g| creds.is_denied(**g))
                .map(|g| creds.denied_name(*g))
                .collect();
            if !denied.is_empty() {
                return Err(validate::Rejection::policy(
                    &creds.user.name,
                    format!(
                        "the checkpointed job ran with the groups {denied:?}, which are in \
                         job_denied_groups; it cannot be restored"
                    ),
                )
                .into());
            }
            let gone: Vec<u32> = groups
                .iter()
                .copied()
                .filter(|g| *g != creds.user.gid && !creds.user.groups.contains(g))
                .collect();
            if !gone.is_empty() {
                return Err(error(
                    HelperErrorCode::PermissionDenied,
                    format!(
                        "the checkpointed job ran with groups {gone:?}, which user {} is no \
                         longer in; it cannot be restored",
                        creds.user.name
                    ),
                ));
            }
        }
        let allowed_gids = creds.allowed_gids();
        self.check_image_paths(&dir, &record, &allowed_gids)?;
        // CRIU fails with "Can't stat mountpoint" / "Failed to open_tree"
        // when a mount point of the job's namespace or the host path of an
        // external bind mount is gone (e.g. under a /run that was rebuilt
        // at boot, or hidden under another mount); say so up front.
        if let Ok(bytes) = fs::read(dir.root().join(MOUNTS_FILE))
            && let Ok(mounts) = serde_json::from_slice::<SavedMounts>(&bytes)
        {
            let required: Vec<String> = mounts
                .required_paths()
                .into_iter()
                .map(str::to_owned)
                .collect();
            // The list is from before the dump; the image says which of
            // those files the job still had open when it was dumped.
            let in_image = crate::images::read_paths(&dir.images_dir())
                .ok()
                .flatten()
                .map(|p| p.files);
            let used: Vec<String> = mounts
                .open_files()
                .iter()
                .filter(|f| in_image.as_ref().is_none_or(|i| i.contains(f)))
                .cloned()
                .collect();
            // Looked up in a child process with a deadline: a filesystem
            // that does not answer (an NFS server that is down) must not
            // hold up the helper.
            // The directories the shim hid behind a tmpfs (/run/libvirt,
            // ...) are created again first: after a reboot only the shim
            // had made them, and they are not missing in that sense.
            crate::jobs::ensure_restore_mount_points(&self.cfg.mounts);
            let mut lookup = required.clone();
            lookup.extend(used.iter().filter(|f| !required.contains(f)).cloned());
            let absent = self.host.missing_paths(&lookup).map_err(|e| {
                error(
                    HelperErrorCode::Internal,
                    format!("checking the paths the checkpoint depends on: {e}"),
                )
            })?;
            let missing: Vec<&String> = required.iter().filter(|p| absent.contains(p)).collect();
            if !missing.is_empty() {
                return Err(error(
                    HelperErrorCode::InvalidRequest,
                    format!(
                        "paths the checkpoint's mounts depend on are missing on this host: {missing:?}"
                    ),
                ));
            }
            // A filesystem that is not mounted again leaves its (empty)
            // mount point directory: CRIU would bind that instead, and the
            // job would go on writing to the filesystem underneath.
            let problems = self
                .host
                .source_problems(mounts.source_mounts())
                .map_err(|e| {
                    error(
                        HelperErrorCode::Internal,
                        format!("reading the helper's mounts: {e}"),
                    )
                })?;
            if !problems.is_empty() {
                return Err(error(
                    HelperErrorCode::InvalidRequest,
                    format!(
                        "filesystems the checkpoint depends on are not mounted as at the dump \
                         (mount them and restore again): {}",
                        problems.join("; ")
                    ),
                ));
            }
            // A mount left out of the checkpoint (see `crate::deadmounts`)
            // is replaced by the restore, but it needs its mount point.
            let detached: Vec<String> = mounts
                .detached_mounts()
                .iter()
                .map(|m| m.mount_point.clone())
                .collect();
            let no_mount_point = self.host.missing_mount_points(&detached).map_err(|e| {
                error(
                    HelperErrorCode::Internal,
                    format!("checking the mount points the checkpoint needs: {e}"),
                )
            })?;
            if !no_mount_point.is_empty() {
                return Err(error(
                    HelperErrorCode::InvalidRequest,
                    format!(
                        "the mount points {no_mount_point:?} of filesystems that were left out \
                         of the checkpoint (they were gone or did not answer at the dump) are \
                         missing on this host; create these directories, or mount the \
                         filesystems there, and restore again"
                    ),
                ));
            }
            let gone: Vec<&String> = used.iter().filter(|f| absent.contains(f)).collect();
            if !gone.is_empty() {
                return Err(error(
                    HelperErrorCode::InvalidRequest,
                    format!(
                        "files or directories the job used are missing on this host: \
                         {gone:?} (the contents of a tmpfs, such as an administrator's \
                         /scratch, do not survive a reboot)"
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
        Ok((record, meta, allowed_gids))
    }

    /// The paths in the image that CRIU will open, create or bind as root
    /// (see [`crate::images`]): no Unix socket may be bound on the host's
    /// filesystems, and the job's owner must reach every file through the
    /// same directories as at the dump, without symlinks (see
    /// [`crate::access`]). The job's own mounts and the helper's
    /// `exit-status` file are left out.
    fn check_image_paths(
        &self,
        dir: &CheckpointDir,
        record: &JobRecord,
        allowed_gids: &[u32],
    ) -> Result<(), ResponseResult> {
        let paths = crate::images::read_paths(&dir.images_dir())
            .map_err(|e| {
                error(
                    HelperErrorCode::InvalidRequest,
                    format!("reading the checkpoint's images: {e}"),
                )
            })?
            .unwrap_or_default();
        let private = &self.cfg.mounts.tmpfs;
        let sockets: Vec<&String> = paths
            .unix_sockets
            .iter()
            .filter(|p| !crate::host::is_private_path(p, private))
            .collect();
        if !sockets.is_empty() {
            return Err(error(
                HelperErrorCode::PermissionDenied,
                format!(
                    "the checkpointed job has Unix sockets bound to {sockets:?} on the host's \
                     filesystems, which CRIU would create, unlink and chown as root; it \
                     cannot be restored"
                ),
            ));
        }
        let exit_status = JobFiles::new(&self.cfg.jobs_dir(), record.job).exit_status();
        let files: Vec<String> = paths
            .files
            .into_iter()
            .filter(|p| !crate::host::is_private_path(p, private))
            .filter(|p| Path::new(p) != exit_status)
            .collect();
        if files.is_empty() {
            return Ok(());
        }
        let owner = crate::access::Owner {
            uid: record.uid.0,
            gid: record.gid.0,
            groups: record
                .dump_groups
                .clone()
                .unwrap_or_else(|| allowed_gids.to_vec()),
        };
        let req = crate::access::VerifyRequest::Paths(crate::access::PathCheck {
            owner,
            paths: files,
        });
        match self.host.verify_access(&req) {
            Ok(problems) if problems.is_empty() => Ok(()),
            Ok(problems) => Err(error(
                HelperErrorCode::PermissionDenied,
                format!(
                    "the paths of the job's files have changed since the checkpoint, so \
                     restoring it could give it files its owner may not open: {}",
                    problems.join("; ")
                ),
            )),
            Err(e) => Err(error(
                HelperErrorCode::Internal,
                format!("checking the checkpoint's paths: {e}"),
            )),
        }
    }

    /// Writes the request of [`crate::access::run_restore_hook`] for this
    /// restore and returns the `--action-script` that runs it. `stop`: the
    /// hook also stops the tasks before CRIU lets them go.
    fn prepare_restore_check(
        &self,
        record: &JobRecord,
        files: &JobFiles,
        allowed_gids: Vec<u32>,
        stop: bool,
    ) -> Result<String, String> {
        use std::os::unix::fs::MetadataExt;
        let exempt = fs::metadata(files.exit_status())
            .map(|m| vec![(m.dev(), m.ino())])
            .unwrap_or_default();
        let check = crate::access::RestoredCheck {
            init_pid: 0, // CRTOOLS_INIT_PID
            owner_uid: record.uid.0,
            allowed_gids,
            private_tmpfs: self.cfg.mounts.tmpfs.clone(),
            exempt,
            held: fs::read(
                CheckpointDir::for_job(&self.cfg.checkpoint_base, record.job.0)
                    .root()
                    .join(MOUNTS_FILE),
            )
            .ok()
            .and_then(|b| serde_json::from_slice::<SavedMounts>(&b).ok())
            .map(|m| m.held_files().to_vec())
            .unwrap_or_default(),
            stop,
        };
        let request = files.dir().join(RESTORE_CHECK_FILE);
        let _ = fs::remove_file(crate::access::hook_result_path(&request));
        write_atomic(
            &request,
            &serde_json::to_vec(&check).map_err(|e| e.to_string())?,
        )
        .map_err(|e| format!("writing {}: {e}", request.display()))?;
        // CRIU executes the action script without arguments: a root-only
        // wrapper next to the request.
        write_action_script(
            &files.dir().join(RESTORE_CHECK_SCRIPT),
            &self.cfg.helper_exe,
            "verify-restore-hook",
            &request,
        )
    }

    /// What [`crate::access::run_restore_hook`] found: `Ok` only if it ran
    /// and found nothing.
    fn restore_check_outcome(files: &JobFiles) -> Result<(), RestoreCheckError> {
        let result = crate::access::hook_result_path(&files.dir().join(RESTORE_CHECK_FILE));
        let bytes = fs::read(&result).map_err(|e| {
            RestoreCheckError::NotRun(format!("the check of the restored job did not run ({e})"))
        })?;
        let reply: crate::access::VerifyReply = serde_json::from_slice(&bytes).map_err(|e| {
            RestoreCheckError::Failed(format!(
                "reading the result of the check of the restored job failed: {e}"
            ))
        })?;
        match reply {
            Ok(problems) if problems.is_empty() => Ok(()),
            Ok(problems) => Err(RestoreCheckError::Refused(format!(
                "the restored job would have files or credentials its owner may not have \
                 (paths or groups changed since the checkpoint?); it was killed before it ran \
                 and the image is kept: {}",
                problems.join("; ")
            ))),
            Err(e) => Err(RestoreCheckError::Failed(format!(
                "checking the restored job failed; it was killed before it ran and the image \
                 is kept: {e}"
            ))),
        }
    }

    async fn restore_job(
        &self,
        mut record: JobRecord,
        files: JobFiles,
        meta: CheckpointMeta,
        allowed_gids: Vec<u32>,
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
        record.restoring = true;
        if let Err(e) = files.write_record(&record) {
            return fail(self, format!("writing the job record: {e}"));
        }
        // From here on a failure also clears the record's `restoring` flag
        // (an interrupted restore is reported by `Self::recover`).
        let mut cleared = record.clone();
        cleared.restoring = false;
        let fail = |this: &Self, reason: String| {
            if let Err(e) = files.write_record(&cleared) {
                eprintln!("puzuz-helper: updating the record of job {}: {e}", job.0);
            }
            this.table().remove(&job);
            RestoreOutcome::failed(reason)
        };
        let unit = record.unit.clone();
        let _ = fs::remove_file(dir.restore_pidfile());
        // Restore in the mode the image was dumped in, even if the
        // configuration changed since: a plugin-mode image needs CRIU's
        // CUDA plugin, a manual-mode one the helper's CUDA steps (and no
        // plugin).
        let mut ck = self.cfg.checkpoint.clone();
        ck.mode = meta.mode;
        // CRIU resumes the restored tasks only after this hook has checked
        // what it opened for them (see `crate::access`). A job that stays
        // paused is stopped by the hook instead, so it does not run at all:
        // CRIU's own `--leave-stopped` would stop the tasks before its CUDA
        // plugin restores them, which then cannot work.
        let stopped = req.leave_stopped;
        match self.prepare_restore_check(&record, &files, allowed_gids, stopped) {
            Ok(script) => {
                ck.criu.extra_restore_args.push("--action-script".into());
                ck.criu.extra_restore_args.push(script.into());
            }
            Err(e) => return fail(self, format!("preparing the restore check: {e}")),
        }
        // Mounts detached at the dump: the host's filesystem if it is back,
        // else an empty, inaccessible directory (see `crate::deadmounts`).
        let detached: Vec<DetachedMount> = fs::read(dir.root().join(MOUNTS_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice::<SavedMounts>(&b).ok())
            .map(|m| m.detached_mounts().to_vec())
            .unwrap_or_default();
        if !detached.is_empty() {
            let placeholder = self
                .cfg
                .checkpoint_base
                .join(crate::deadmounts::PLACEHOLDER_DIR);
            let host = self.host.clone();
            let mounts = detached.clone();
            let sources = tokio::task::spawn_blocking(move || {
                crate::deadmounts::ensure_placeholder(&placeholder)?;
                host.detached_sources(&mounts, &placeholder)
            })
            .await
            .map_err(io::Error::other)
            .and_then(|r| r);
            match sources {
                Ok(sources) => {
                    for (m, src) in detached.iter().zip(&sources) {
                        let what = if src == &m.mount_point {
                            "the host's filesystem, which is back"
                        } else {
                            "an empty, inaccessible directory (the filesystem is not back)"
                        };
                        eprintln!(
                            "puzuz-helper: job {}: {} ({}) was left out of the checkpoint; \
                             restoring it as {what}",
                            job.0, m.mount_point, m.fstype
                        );
                    }
                    ck.criu.extra_restore_args.extend(
                        crate::deadmounts::restore_args(&detached, &sources)
                            .into_iter()
                            .map(Into::into),
                    );
                }
                Err(e) => {
                    return fail(
                        self,
                        format!("preparing the mounts left out of the checkpoint: {e}"),
                    );
                }
            }
        }
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
        // The GPUs of this restore (the restored processes reopen their
        // device nodes there, and CRIU's CUDA plugin runs in the unit).
        let devices = match self.unit_devices(job, &req.gpus) {
            Ok(d) => d,
            Err(e) => return fail(self, e),
        };
        // The directories the shim hid behind a tmpfs must exist again as
        // mount points (`/run` is empty after a reboot).
        crate::jobs::ensure_restore_mount_points(&self.cfg.mounts);
        let spec = RestoreSpec {
            job,
            unit: unit.clone(),
            command,
            pidfile: dir.restore_pidfile(),
            timeout: timeout.min(deadline.remaining()),
            criu_root: self.cfg.criu_root.clone(),
            devices,
        };
        let pid = match self.launcher.restore(&spec).await {
            Ok(pid) => pid,
            Err(e) if e.code == HelperErrorCode::ShuttingDown => {
                // Abandoned by a stopping helper before systemd started the
                // unit: nothing ran and the image is untouched. Not a
                // failure of the restore; the daemon queues the job again.
                let _ = fail(self, String::new());
                return RestoreOutcome::Failed {
                    reason: e.to_string(),
                    code: Some(HelperErrorCode::ShuttingDown),
                };
            }
            Err(e) => {
                // Refused by the check of the restored tasks, or another
                // failure of criu.
                let reason = match Self::restore_check_outcome(&files) {
                    Err(RestoreCheckError::Refused(reason) | RestoreCheckError::Failed(reason)) => {
                        reason
                    }
                    Ok(()) | Err(RestoreCheckError::NotRun(_)) => {
                        let summary = fs::read_to_string(dir.restore_log())
                            .map(|l| probe::summarize_criu_log(&l, 5).join("; "))
                            .unwrap_or_default();
                        past_deadline(format!("{e}: {summary}"))
                    }
                };
                return fail(self, reason);
            }
        };

        // CRIU only resumes the tasks when the hook succeeded; make sure it
        // did run.
        if let Err(e) = Self::restore_check_outcome(&files) {
            let reason = e.into_reason();
            eprintln!(
                "puzuz-helper: refusing the restore of job {}: {reason}",
                job.0
            );
            let _ = self.launcher.signal(&unit, signal::SIGKILL).await;
            let _ = self.launcher.release(&unit).await;
            return fail(self, reason);
        }

        if meta.mode == CudaMode::Manual && !meta.cuda_ns_pids.is_empty() {
            let host = self.host.clone();
            let factory = self.executor.clone();
            let wanted = meta.cuda_ns_pids.clone();
            let gpus = req.gpus.clone();
            let saved = meta.gpus.clone();
            let ck2 = ck.clone();
            let res = tokio::task::spawn_blocking(move || -> Result<(), String> {
                let mut host_pids = Vec::new();
                for p in host.resolve_ns_pids(pid, &wanted)? {
                    if !host_pids.contains(&p) {
                        host_pids.push(p);
                    }
                }
                let map = device_map(&*host, &saved, &gpus)?;
                let run = |steps| {
                    let mut exec = factory();
                    run_plan_until(&mut *exec, steps, Some(&deadline))
                        .map(|_| ())
                        .map_err(|f| format!("CUDA restore failed: {f}"))
                };
                if !stopped {
                    return run(plan::restore_cuda_plan(&ck2, &host_pids, map.as_ref()));
                }
                // The tree is stopped (by the restore hook): only the
                // threads that serve cuda-checkpoint run meanwhile.
                let mut steps = Some(plan::restore_cuda_plan_stopped(
                    &ck2,
                    &host_pids,
                    map.as_ref(),
                ));
                host.with_cuda_threads_running(pid, &host_pids, &mut || {
                    run(steps.take().unwrap_or_default())
                })
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

        // The job was to stay paused; should it run after all, it is not
        // passed off as restored stopped: it is killed and the image kept.
        if stopped && let Err(reason) = self.confirm_stopped(pid).await {
            eprintln!("puzuz-helper: restore of job {}: {reason}", job.0);
            let _ = self.launcher.signal(&unit, signal::SIGKILL).await;
            let _ = self.launcher.release(&unit).await;
            return fail(
                self,
                format!("{reason}; the restored job was killed and the image is kept"),
            );
        }

        record.phase = RecordPhase::Live;
        record.unit_kind = UnitKind::Restored;
        record.restoring = false;
        if let Err(e) = files.write_record(&record) {
            // Without this record, the next start of the helper takes the
            // job for an interrupted restore and kills it (see
            // `Self::recover`): undo the restore now, while the image is
            // still there to restore from again.
            eprintln!("puzuz-helper: recording restore of job {}: {e}", job.0);
            let _ = self.launcher.signal(&unit, signal::SIGKILL).await;
            let _ = self.launcher.release(&unit).await;
            return fail(
                self,
                format!(
                    "recording that the job runs again failed ({e}); the restored job was \
                     killed and the image is kept"
                ),
            );
        }
        if let Some(e) = self.table().get_mut(&job) {
            e.record = record;
            e.phase = if stopped {
                Phase::Stopped
            } else {
                Phase::Running
            };
            e.pid = Some(pid);
        }
        if let Err(e) = dir.remove() {
            eprintln!("puzuz-helper: removing the image of job {}: {e}", job.0);
        }
        RestoreOutcome::Success { pid, stopped }
    }

    /// A restore that was to leave the job stopped: every thread of it
    /// should be stopped by now (the restore hook stopped the tree before
    /// CRIU let it go). `Err` if one still is not after a moment to settle
    /// (then job code has run), or if this cannot be checked.
    async fn confirm_stopped(&self, init: u32) -> Result<(), String> {
        let mut running = Vec::new();
        for _ in 0..100 {
            running = self
                .host
                .unstopped_threads(init)
                .map_err(|e| format!("checking that the restored job is stopped failed: {e}"))?;
            if running.is_empty() {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(format!(
            "threads {running:?} of the restored job did not stop although it was paused"
        ))
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
    /// The dump succeeded (the job's processes are gone), but the
    /// checkpoint could not be finalized; the image is kept (see
    /// [`finish_dump`]).
    Unfinalized(String),
}

/// What [`precheck_dump`] learned, for the dump that follows.
#[derive(Debug, Clone)]
pub struct DumpPrecheck {
    procs: Vec<ProcInfo>,
    /// Without the CUDA PIDs yet.
    meta: CheckpointMeta,
    mounts: Vec<String>,
    /// See [`SavedMounts::Full`].
    open_files: Vec<String>,
    hazards: DumpHazards,
    /// See [`JobRecord::dump_groups`].
    groups: Option<Vec<u32>>,
    /// Dead mounts the job does not use, and the mounts below them (see
    /// [`crate::deadmounts`]).
    detached: Vec<DetachedMount>,
    below_detached: Vec<String>,
}

/// The checks before a dump, which only read the job's state: the image
/// base, the job's mounts (an image that depends on a hidden mount could
/// never be restored, not even on this host), Unix sockets bound on the
/// host's filesystems (see [`DumpHazards`]), FUSE and network mounts that
/// are gone or do not answer (see [`crate::deadmounts`]) and the free space
/// for its memory. `private` are the mount points of the tmpfs mounts the job's
/// namespace has of its own. An error means the dump is refused and the job
/// is left alone.
pub fn precheck_dump<H: HostProbe + ?Sized>(
    host: &H,
    mode: CudaMode,
    base: &Path,
    record: &JobRecord,
    init_pid: u32,
    private: &[String],
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
    let host_pids: Vec<u32> = procs.iter().map(|p| p.host_pid).collect();
    // Before anything that looks at the job's files: those on a filesystem
    // that does not answer could block the helper.
    let dead = host
        .dead_mounts(init_pid, &host_pids)
        .map_err(|e| format!("checking the job's FUSE and network mounts: {e}"))?;
    if let Some(reason) = crate::deadmounts::refusal(&dead) {
        return Err(reason);
    }
    let detached = DetachedMount::for_dump(&dead);
    let below_detached = dead.below.clone();
    // CRIU leaves the mounts below a detached one out of the image, so a
    // restore does not need their mount points; a detached mount point is
    // looked at by the restore itself (the host's mount there may be as
    // dead as at the dump, or gone).
    let mounts: Vec<String> = mounts
        .into_iter()
        .filter(|m| !below_detached.contains(m) && !detached.iter().any(|d| &d.mount_point == m))
        .collect();
    let hazards = host
        .dump_hazards(&host_pids, private)
        .map_err(|e| format!("reading the job's open files: {e}"))?;
    if !hazards.bound_sockets.is_empty() {
        return Err(format!(
            "the job has Unix sockets bound to {:?} on the host's filesystems (or to relative \
             names, which lead there too); a restore would make CRIU create, unlink and chown \
             those paths as root, so such a job cannot be checkpointed (bind sockets to an \
             absolute path below /tmp, which is the job's own, or to an abstract name)",
            hazards.bound_sockets
        ));
    }
    if !hazards.reuseport_sockets.is_empty() {
        return Err(format!(
            "the job has internet sockets with SO_REUSEPORT ({}); CRIU would restore them as \
             root's, which lets them share the port with a root service and receive part of \
             its connections, so such a job cannot be checkpointed (do not set SO_REUSEPORT, \
             or run the job with --no-checkpoint)",
            hazards.reuseport_sockets.join(", ")
        ));
    }
    // Deleted files are saved in the image (see `dump_job`), and so are the
    // contents of the job's own tmpfs mounts.
    let ghosts: u64 = hazards.deleted_files.iter().map(|(_, s)| s).sum();
    let tmpfs = host
        .private_tmpfs_bytes(init_pid, private)
        .map_err(|e| format!("measuring the job's /tmp and /dev/shm: {e}"))?;
    let available = host.available_bytes(base).map_err(|e| e.to_string())?;
    space::check_free_space(
        rss.saturating_add(ghosts).saturating_add(tmpfs),
        meta.vram_bytes,
        available,
    )
    .map_err(|e| {
        if tmpfs > 0 {
            format!("{e} (including {tmpfs} bytes in the job's private tmpfs mounts {private:?})")
        } else {
            e.to_string()
        }
    })?;
    let groups = host
        .process_groups(&host_pids)
        .map_err(|e| format!("reading the job's credentials: {e}"))?;
    // Only for the restore's pre-check; never a reason to refuse the dump.
    let open_files = host.open_files(&host_pids).unwrap_or_default();
    Ok(DumpPrecheck {
        procs,
        meta,
        mounts,
        open_files,
        hazards,
        groups,
        detached,
        below_detached,
    })
}

/// CRIU's default `--ghost-limit` (largest deleted file saved in an image).
const DEFAULT_GHOST_LIMIT: u64 = 1 << 20;

/// The CRIU options for a dump of a job with `hazards`: deleted files on
/// the host's filesystems are saved in the image (as "ghost" files) rather
/// than linked back into their directory with `--link-remap`, which CRIU
/// does as root by a path the job's owner may have redirected (to `/etc`,
/// say) while the dump runs. The job's own tmpfs mounts keep
/// `--link-remap` when nothing else needs it.
pub fn dump_options(ck: &CheckpointConfig, hazards: &DumpHazards) -> CheckpointConfig {
    let mut ck = ck.clone();
    if let Some(largest) = hazards.deleted_files.iter().map(|(_, s)| *s).max() {
        ck.criu.link_remap = false;
        let limit = (largest + 1).max(DEFAULT_GHOST_LIMIT);
        ck.criu.extra_dump_args.push("--ghost-limit".into());
        ck.criu.extra_dump_args.push(limit.to_string().into());
    }
    ck
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
///
/// With `hook_exe` (this binary), `criu dump` runs
/// [`crate::access::run_dump_hook`] once it has dumped the job, to record
/// the files the job held at that moment (see [`finish_dump`]).
#[allow(clippy::too_many_arguments)]
pub fn dump_job<H: HostProbe + ?Sized>(
    host: &H,
    exec: &mut dyn Executor,
    ck: &CheckpointConfig,
    base: &Path,
    record: &JobRecord,
    init_pid: u32,
    private: &[String],
    hook_exe: Option<&Path>,
    cap: Option<Duration>,
    on_deadline: &dyn Fn(Instant),
) -> Result<CheckpointMeta, DumpError> {
    let DumpPrecheck {
        procs,
        mut meta,
        mounts,
        open_files,
        hazards,
        groups,
        detached,
        below_detached,
    } = precheck_dump(host, ck.mode, base, record, init_pid, private)
        .map_err(DumpError::Refused)?;
    let mut ck = dump_options(ck, &hazards);
    for d in &detached {
        eprintln!(
            "puzuz-helper: job {}: {} ({}) {}; the job does not use it, so it is checkpointed \
             without it",
            record.job.0, d.mount_point, d.fstype, d.why
        );
    }
    ck.criu.extra_dump_args.extend(
        crate::deadmounts::dump_args(&detached, &below_detached)
            .into_iter()
            .map(Into::into),
    );
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
    // Without its CUDA plugin CRIU cannot dump the NVIDIA device mappings
    // and the failed dump would end the job; refuse instead.
    if ck.mode == CudaMode::Plugin && !cuda_pids.is_empty() {
        let plugin = ck.criu.cuda_plugin_path();
        if !plugin.is_file() {
            return Err(DumpError::Refused(format!(
                "cuda_checkpoint_mode is \"plugin\" but CRIU's CUDA plugin {} is missing; \
                 install it or use the manual mode",
                plugin.display()
            )));
        }
    }

    let dir = CheckpointDir::for_job(base, record.job.0);
    dir.prepare()
        .map_err(|e| DumpError::Refused(format!("preparing {}: {e}", dir.root().display())))?;
    // What a restore needs besides CRIU's images, written before the dump:
    // once it succeeded, the image is all that is left of the job, and it
    // can always be finalized from these (see `finish_dump`).
    let pre = || -> Result<(), String> {
        let mut owner = record.clone();
        owner.phase = RecordPhase::Checkpointed;
        owner.dump_groups = groups;
        owner.dumping = false;
        owner.restoring = false;
        let owner_json = serde_json::to_vec_pretty(&owner).map_err(|e| e.to_string())?;
        write_atomic(&dir.root().join(OWNER_FILE), &owner_json).map_err(|e| e.to_string())?;
        let saved = SavedMounts::Full {
            mount_points: mounts,
            external_sources: Vec::new(),
            source_mounts: Vec::new(),
            open_files,
            held_files: hazards.held_files.clone(),
            detached_mounts: detached,
        };
        let mounts_json = serde_json::to_vec_pretty(&saved).map_err(|e| e.to_string())?;
        write_atomic(&dir.root().join(MOUNTS_FILE), &mounts_json).map_err(|e| e.to_string())?;
        // A draft without the size; `finalize` rewrites it.
        let meta_json = serde_json::to_vec_pretty(&meta).map_err(|e| e.to_string())?;
        write_atomic(&dir.meta_path(), &meta_json).map_err(|e| e.to_string())
    };
    if let Err(e) = pre() {
        let _ = dir.remove();
        return Err(DumpError::Refused(format!(
            "writing the checkpoint's metadata to {}: {e}",
            dir.root().display()
        )));
    }
    // The held files above were listed while the job ran: it may open
    // others before CRIU stops it. The hook lists them again while CRIU
    // has it stopped, after the dump.
    if let Some(exe) = hook_exe {
        let request = dir.root().join(DUMP_HOOK_FILE);
        let hook = crate::access::DumpHookRequest {
            init_pid,
            private: private.to_vec(),
        };
        let script = serde_json::to_vec(&hook)
            .map_err(|e| e.to_string())
            .and_then(|b| {
                write_atomic(&request, &b)
                    .map_err(|e| format!("writing {}: {e}", request.display()))
            })
            .and_then(|()| {
                write_action_script(
                    &dir.root().join(DUMP_HOOK_SCRIPT),
                    exe,
                    "dump-hook",
                    &request,
                )
            });
        match script {
            Ok(script) => {
                ck.criu.extra_dump_args.push("--action-script".into());
                ck.criu.extra_dump_args.push(script.into());
            }
            Err(e) => {
                let _ = dir.remove();
                return Err(DumpError::Refused(format!(
                    "writing the checkpoint's metadata to {}: {e}",
                    dir.root().display()
                )));
            }
        }
    }
    let ck = &ck;
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
    if let Err(failure) = run_dump_with_rollback(exec, ck, steps, &cuda_pids, Some(&deadline)) {
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
            None => {
                reason.push_str("; the job was resumed");
                if failure.criu_dump_killed() {
                    reason.push_str(
                        " (criu was killed mid-dump, which can still crash processes it was \
                         dumping)",
                    );
                }
            }
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
    // The dump succeeded and the job's processes are gone: from here on the
    // image is never deleted. Writing the rest of the metadata is retried;
    // if it still fails, the image stays as an unfinalized dump that a
    // restore or the helper's next start finalizes (see `finish_dump`).
    let mut last = String::new();
    for attempt in 0..FINALIZE_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(FINALIZE_RETRY_DELAY);
        }
        match finish_dump(host, &dir, Some(meta.clone())) {
            Ok(meta) => return Ok(meta),
            Err(e) => {
                eprintln!(
                    "puzuz-helper: finalizing the checkpoint of job {} (attempt {}): {e}",
                    record.job.0,
                    attempt + 1
                );
                last = e;
            }
        }
    }
    Err(DumpError::Unfinalized(unfinalized_reason(&dir, &last)))
}

/// How often [`dump_job`] tries to finalize a successful dump.
const FINALIZE_ATTEMPTS: u32 = 3;
const FINALIZE_RETRY_DELAY: Duration = Duration::from_secs(1);

/// The reason reported for a dump that succeeded but could not be finalized.
fn unfinalized_reason(dir: &CheckpointDir, error: &str) -> String {
    format!(
        "the job was dumped (its processes are stopped), but finalizing the checkpoint failed: \
         {error}; the image is kept in {} and is finalized before it is restored or when the \
         helper starts again",
        dir.root().display()
    )
}

/// Why the check of restored tasks (see [`crate::access::run_restore_hook`])
/// did not pass. Each holds the reason reported for the restore.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RestoreCheckError {
    /// It left no result: CRIU did not get as far as running it.
    NotRun(String),
    /// It found files or credentials the owner may not have.
    Refused(String),
    /// It ran but could not check, or its result is unreadable.
    Failed(String),
}

impl RestoreCheckError {
    fn into_reason(self) -> String {
        match self {
            Self::NotRun(r) | Self::Refused(r) | Self::Failed(r) => r,
        }
    }
}

/// Whether a job was restored from the image in `dir`: `criu restore`
/// writes its log and pidfile into it. A new dump starts in a fresh
/// directory (see [`CheckpointDir::prepare`]), so its image never has them.
fn was_restored_from(dir: &CheckpointDir) -> bool {
    dir.restore_log().exists() || dir.restore_pidfile().exists()
}

/// Finalize a dump that succeeded: the [`layout::DUMPED_MARKER`], the
/// external mounts from CRIU's log and the attributes of the held files in
/// [`MOUNTS_FILE`], then `meta.json` with the image's size and the
/// `COMPLETE` marker. `meta` defaults to the draft `meta.json` written
/// before the dump. Idempotent, so it also repairs a dump that a helper
/// left unfinalized ([`CheckpointDir::is_unfinalized_dump`]); it never
/// removes anything.
pub fn finish_dump<H: HostProbe + ?Sized>(
    host: &H,
    dir: &CheckpointDir,
    meta: Option<CheckpointMeta>,
) -> Result<CheckpointMeta, String> {
    if let DirStatus::Complete(meta) = dir.status() {
        return Ok(*meta);
    }
    let meta = match meta {
        Some(m) => m,
        None => dir
            .read_meta()
            .map_err(|e| format!("reading the draft {}: {e}", layout::META_FILE))?,
    };
    if !dir.dumped_marker_path().is_file() {
        dir.mark_dumped()
            .map_err(|e| format!("writing {}: {e}", layout::DUMPED_MARKER))?;
    }
    if !dir.root().join(OWNER_FILE).is_file() {
        return Err(format!("{OWNER_FILE} is missing"));
    }
    let path = dir.root().join(MOUNTS_FILE);
    let saved = match fs::read(&path) {
        Ok(b) => serde_json::from_slice::<SavedMounts>(&b)
            .map_err(|e| format!("reading {MOUNTS_FILE}: {e}"))?,
        Err(e) => return Err(format!("reading {MOUNTS_FILE}: {e}")),
    };
    let saved = match saved {
        SavedMounts::Full {
            mount_points,
            mut external_sources,
            mut source_mounts,
            open_files,
            mut held_files,
            detached_mounts,
        } => {
            if external_sources.is_empty() {
                external_sources = fs::read_to_string(dir.dump_log())
                    .map(|log| probe::criu_external_mounts(&log))
                    .unwrap_or_default();
            }
            if source_mounts.is_empty() && !external_sources.is_empty() {
                source_mounts = host.source_mounts(&external_sources).unwrap_or_default();
            }
            // The files the job held when CRIU had stopped it, which it may
            // have opened after the list above was made (see `dump_job`).
            if let Ok(b) = fs::read(crate::access::hook_result_path(
                &dir.root().join(DUMP_HOOK_FILE),
            )) && let Ok(at_dump) = serde_json::from_slice::<Vec<crate::access::HeldFile>>(&b)
            {
                for f in at_dump {
                    if !held_files
                        .iter()
                        .any(|h| (&h.path, h.dev, h.ino) == (&f.path, f.dev, f.ino))
                    {
                        held_files.push(f);
                    }
                }
            }
            // The job is gone now: its files no longer change.
            crate::access::complete_held_files(&mut held_files);
            SavedMounts::Full {
                mount_points,
                external_sources,
                source_mounts,
                open_files,
                held_files,
                detached_mounts,
            }
        }
        old @ SavedMounts::MountPoints(_) => old,
    };
    let mounts_json = serde_json::to_vec_pretty(&saved).map_err(|e| e.to_string())?;
    write_atomic(&path, &mounts_json).map_err(|e| format!("writing {MOUNTS_FILE}: {e}"))?;
    dir.finalize(meta).map_err(|e| e.to_string())
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
