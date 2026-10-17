//! The gpueue daemon: HTTP API, scheduler and persistent state.
//!
//! The daemon runs as the unprivileged `gpueue` system user. It serves the
//! `/v1` API on a Unix socket (identifying callers with `SO_PEERCRED`),
//! keeps all state in SQLite, decides when jobs start, stop, checkpoint and
//! restore, and asks the root helper to carry that out.
//!
//! [`run`] is the `gpueue daemon` entry point. Tests (and embedders) use
//! [`Daemon::start`] with injected [`Helper`], [`UserDb`] and
//! [`GpuDetector`] implementations.
//!
//! Stopping the daemon (SIGTERM, [`Daemon::stop`]) only stops serving: jobs
//! live in their own systemd units and keep running, and the next daemon
//! reconciles with the helper. Launches decided on but not sent to the
//! helper yet are put back in line first. `gpueue shutdown`
//! (`POST /v1/daemon/shutdown`) is the way to checkpoint or stop every job
//! first.
//!
//! GPUs are detected once at start-up (NVML, see [`gpu`]); restart the
//! daemon after adding or removing GPUs.
//!
//! Checkpoint bookkeeping: each record gets the GPU model and driver version
//! of the job's GPUs, and the image size the helper reports. When all
//! complete images together exceed `checkpoint_size_limit_bytes`, the daemon
//! logs a warning (at start-up and after each dump) and warns the owner of
//! the new checkpoint and administrators on their commands (see [`usage`]);
//! images are never deleted automatically and dumps are not refused. The
//! owner of a job whose dump failed is told on their next command (see
//! [`gpueue_protocol::api::WARNINGS_HEADER`]). `GET /v1/version` reports the
//! usage for `gpueue doctor`.
//!
//! Launches asked for explicitly (`add --immediate`, `start` of a queued
//! or stashed job, `restore --immediate`, `restart --immediate`) are
//! immediate only for administrators (root or `gpueue-admin`): their job
//! starts right away, ignoring the group's and the GPUs' slots, a paused
//! group and dependencies. Anybody else's job is queued (or queued for
//! restore) first among their own jobs, like `gpueue top`, and then competes
//! with other users' jobs by priority under every limit, dependencies
//! included; with `enable_user_top = false` or `priority_type = "basic"`
//! it is just queued. The reply's `queued` says which.
//!
//! When the helper is not connected, an administrator's explicit launches
//! do not fail: the job is queued (or queued for restore) and launched as
//! soon as the helper is back, still ignoring slots and dependencies; the
//! reply's `launch_pending` says so. Actions on
//! a live process (`pause`, `kill`, `checkpoint`, resuming a paused job)
//! still fail with `helper_unavailable`. A spawn or restore the helper did
//! not carry out because it is stopping (refused with `shutting_down`, or
//! accepted and then abandoned before systemd started its unit), or whose
//! answer was lost while the helper never started it, is put back in line
//! the same way and retried; no restore error is recorded for it. The "launch
//! regardless of slots" mark is kept in memory only: after a daemon restart
//! such a job waits for a slot like any other.
//!
//! Job logs live in `<logs_dir>/<uid>/`, which the helper creates owned by
//! the job's owner and writable by the daemon's group. The owner controls
//! its contents, so the daemon opens logs without following symlinks and
//! only reads or truncates regular files of the owner (see `logfile.rs`).
//! Removing a log (`remove`, `clean`) is tried and failures are logged; a
//! log `restart --in-place` cannot empty fails the restart.

mod actions;
pub mod api;
pub mod clock;
/// The configuration file's schema lives in `gpueue-config`, which the root
/// helper shares.
pub use gpueue_config as config;
mod error;
pub mod gpu;
pub mod helper;
mod launch;
mod limits;
mod logfile;
mod priority;
mod server;
mod state;
mod usage;
pub mod users;

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gpueue_core::model::GpuIndex;
use gpueue_core::storage::{Store, StoreOps};
use gpueue_protocol::api::GpuSource;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

pub use crate::clock::{Clock, ManualClock, SystemClock};
pub use crate::config::Config;
pub use crate::gpu::{
    DetectedGpu, Detection, FallbackGpuDetector, GpuDetector, NvmlGpuDetector, StaticGpuDetector,
};
pub use crate::helper::{FakeHelper, Helper, HelperClient};
use crate::state::Core;
#[doc(hidden)]
pub use crate::state::LaunchCheckHook;
pub use crate::users::{StaticUserDb, SystemUserDb, UserDb};

/// Everything [`Daemon::start`] needs.
pub struct DaemonOptions {
    pub config: Config,
    pub helper: Arc<dyn Helper>,
    pub users: Arc<dyn UserDb>,
    pub gpus: Arc<dyn GpuDetector>,
    /// Where the daemon takes the time from ([`SystemClock`] but in tests).
    pub clock: Arc<dyn Clock>,
    /// Testing only: honour the [`api::TEST_UID_HEADER`] header instead of
    /// `SO_PEERCRED`. The field exists only with the `test-uid-header`
    /// cargo feature, so a production build cannot enable it.
    #[cfg(feature = "test-uid-header")]
    pub trust_uid_header: bool,
}

impl DaemonOptions {
    /// The production setup: the real helper connection, NSS user lookups
    /// and NVML GPU detection (falling back to the configuration).
    pub fn new(config: Config) -> Self {
        Self {
            helper: Arc::new(HelperClient::new(&config.helper_socket)),
            users: Arc::new(SystemUserDb),
            gpus: Arc::from(gpu::detector_for(&config)),
            clock: Arc::new(SystemClock),
            config,
            #[cfg(feature = "test-uid-header")]
            trust_uid_header: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Storage(#[from] gpueue_core::storage::StorageError),
    #[error("cannot create {}: {source}", path.display())]
    CreateDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cannot listen on {}: {source}", path.display())]
    Listen {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// A running daemon.
pub struct Daemon {
    core: Arc<Core>,
    server: JoinHandle<()>,
    tasks: Vec<JoinHandle<()>>,
    socket: PathBuf,
}

impl Daemon {
    pub async fn start(options: DaemonOptions) -> Result<Self, StartError> {
        let DaemonOptions {
            config,
            helper,
            users,
            gpus,
            clock,
            #[cfg(feature = "test-uid-header")]
            trust_uid_header,
        } = options;
        #[cfg(not(feature = "test-uid-header"))]
        let trust_uid_header = false;

        let db_path = config.state_db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StartError::CreateDir {
                path: parent.to_owned(),
                source,
            })?;
        }
        let store = Store::open(&db_path)?;

        let failed = |error: String| Detection {
            gpus: Vec::new(),
            source: GpuSource::Config,
            nvml_error: Some(error),
        };
        let detection = match tokio::task::spawn_blocking(move || gpus.detection()).await {
            Ok(Ok(detection)) => detection,
            Ok(Err(e)) => {
                tracing::warn!("GPU detection failed: {e}; running without GPUs");
                failed(e)
            }
            Err(e) => {
                tracing::warn!("GPU detection panicked: {e}; running without GPUs");
                failed(format!("GPU detection panicked: {e}"))
            }
        };
        if config.gpu_detection == config::GpuDetection::Static {
            tracing::info!(
                "gpu_detection = \"static\": using the {} GPU(s) of the [[gpus]] list",
                detection.gpus.len()
            );
        }
        let gpu_detection = detection.status(config.gpu_detection);
        let detected = record_gpus(&store, detection.gpus)?;

        let listener = bind(&config.api_socket).map_err(|source| StartError::Listen {
            path: config.api_socket.clone(),
            source,
        })?;
        let socket = config.api_socket.clone();

        let core = Core::new(
            config,
            store,
            helper,
            users,
            detected,
            gpu_detection,
            trust_uid_header,
            clock,
        );
        core.check_checkpoint_usage().await;
        core.start_usage().await;
        let notices = core.helper.connect();
        let tasks = vec![
            tokio::spawn(core.clone().notice_loop(notices)),
            tokio::spawn(core.clone().scheduler_loop()),
            tokio::spawn(core.clone().usage_loop()),
        ];

        let app = api::router(core.clone());
        let server = tokio::spawn(server::serve(listener, app, core.cancel.clone()));
        tracing::info!(socket = %socket.display(), "gpueue daemon listening");
        Ok(Self {
            core,
            server,
            tasks,
            socket,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Testing only: `hook` is awaited by an action whose spawn or restore
    /// request failed, before it finds out whether the launch is retried,
    /// so tests can let the retry run (or finish) first.
    #[doc(hidden)]
    /// Whether the daemon can launch jobs: the helper is connected and the
    /// connection reconciled (see `Core::helper_ready`). For tests.
    #[doc(hidden)]
    pub fn helper_ready(&self) -> bool {
        self.core.helper_ready()
    }

    pub fn set_launch_check_hook(&self, hook: Option<LaunchCheckHook>) {
        self.core.set_launch_check_hook(hook);
    }

    /// Resolves once `gpueue shutdown` has checkpointed or stopped every
    /// job; the caller should then [`stop`](Daemon::stop) the daemon.
    pub async fn shutdown_requested(&self) {
        self.core.shutdown_complete.cancelled().await;
    }

    /// Stops serving and disconnects from the helper. Jobs are not touched.
    ///
    /// Launches the scheduler had decided on but not sent to the helper yet
    /// are put back in line first (the next daemon would find their jobs
    /// gone and mark them lost), and the helper gets a few seconds to
    /// answer the spawn and restore requests already sent (see
    /// `Core::stop_launches`).
    pub async fn stop(self) {
        self.core.stop_launches(state::STOP_LAUNCH_WAIT).await;
        self.core.cancel.cancel();
        self.core.helper.close();
        if tokio::time::timeout(Duration::from_secs(10), self.server)
            .await
            .is_err()
        {
            tracing::warn!("open API connections did not close in time");
        }
        for task in self.tasks {
            task.abort();
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Records the detected GPUs in the store and reports changes against the
/// GPUs it knew before. Returns the detected GPUs by index (the available
/// ones; see [`gpu`] for what happens to the others).
fn record_gpus(
    store: &Store,
    detected: Vec<DetectedGpu>,
) -> Result<HashMap<GpuIndex, DetectedGpu>, StartError> {
    let known: BTreeMap<GpuIndex, _> = store
        .list_gpus()?
        .into_iter()
        .map(|gpu| (gpu.index, gpu))
        .collect();
    let mut by_index = HashMap::new();
    for gpu in detected {
        if by_index.contains_key(&gpu.index) {
            tracing::warn!(index = %gpu.index, "GPU detected twice; ignoring the second one");
            continue;
        }
        if let Some(old) = known.get(&gpu.index)
            && !old.uuid.is_empty()
            && old.uuid != gpu.uuid
        {
            tracing::warn!(
                index = %gpu.index,
                old = %old.uuid,
                new = %gpu.uuid,
                "a different GPU is now at this index; it keeps the index's slot limit"
            );
        }
        store.upsert_gpu(gpu.index, &gpu.uuid, &gpu.name)?;
        tracing::info!(
            index = %gpu.index,
            uuid = %gpu.uuid,
            name = %gpu.name,
            memory = ?gpu.memory_total_bytes,
            driver = ?gpu.driver_version,
            "GPU detected"
        );
        by_index.insert(gpu.index, gpu);
    }
    for (index, gpu) in &known {
        if !by_index.contains_key(index) {
            tracing::warn!(
                %index,
                uuid = %gpu.uuid,
                "GPU was not detected; it starts no jobs, and jobs that need it stay queued"
            );
        }
    }
    Ok(by_index)
}

/// Binds the API socket, replacing a stale socket file, and makes it
/// connectable by everyone (callers are authorized per request).
fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && meta.file_type().is_socket()
    {
        std::fs::remove_file(path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// Tells systemd the daemon is ready (`Type=notify`); a no-op otherwise.
fn notify_ready() {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let Some(target) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(target.as_os_str());
    let addr = match bytes.strip_prefix(b"@") {
        Some(name) => SocketAddr::from_abstract_name(name),
        None => SocketAddr::from_pathname(Path::new(&target)),
    };
    let result = addr.and_then(|addr| {
        let socket = UnixDatagram::unbound()?;
        socket.send_to_addr(b"READY=1", &addr).map(|_| ())
    });
    if let Err(e) = result {
        tracing::warn!("cannot notify systemd: {e}");
    }
}

/// `gpueue daemon`: runs until SIGTERM/SIGINT or until `gpueue shutdown` has
/// finished.
pub async fn run(config: Config) -> Result<(), StartError> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(io::stderr)
        // No color codes in the journal.
        .with_ansi(io::IsTerminal::is_terminal(&io::stderr()))
        .try_init();

    let daemon = Daemon::start(DaemonOptions::new(config)).await?;
    notify_ready();

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|source| StartError::Listen {
        path: daemon.socket_path().to_owned(),
        source,
    })?;
    tokio::select! {
        _ = terminate.recv() => tracing::info!("SIGTERM received; stopping (jobs keep running)"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT received; stopping (jobs keep running)"),
        _ = daemon.shutdown_requested() => {}
    }
    daemon.stop().await;
    Ok(())
}
