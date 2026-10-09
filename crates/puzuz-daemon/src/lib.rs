//! The puzuz daemon: HTTP API, scheduler and persistent state.
//!
//! The daemon runs as the unprivileged `puzuz` system user. It serves the
//! `/v1` API on a Unix socket (identifying callers with `SO_PEERCRED`),
//! keeps all state in SQLite, decides when jobs start, stop, checkpoint and
//! restore, and asks the root helper to carry that out.
//!
//! [`run`] is the `puzuz daemon` entry point. Tests (and embedders) use
//! [`Daemon::start`] with injected [`Helper`], [`UserDb`] and
//! [`GpuDetector`] implementations.
//!
//! Stopping the daemon (SIGTERM, [`Daemon::stop`]) only stops serving: jobs
//! live in their own systemd units and keep running, and the next daemon
//! reconciles with the helper. `puzuz shutdown` (`POST /v1/daemon/shutdown`)
//! is the way to checkpoint or stop every job first.
//!
//! Known gaps, for integration:
//! - GPU detection is static (configuration file); NVML is not used yet.
//! - Removing or truncating a log file (`remove`, `clean`, `restart
//!   --in-place`) needs write access to `<logs_dir>/<uid>/`, which the
//!   helper creates; the daemon only tries and logs failures.
//! - The checkpoint size limit is parsed but not enforced or reported.

mod actions;
pub mod api;
pub mod config;
mod error;
pub mod gpu;
pub mod helper;
mod state;
pub mod users;

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use puzuz_core::storage::{Store, StoreOps};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;

pub use crate::config::Config;
pub use crate::gpu::{DetectedGpu, GpuDetector, StaticGpuDetector};
pub use crate::helper::{FakeHelper, Helper, HelperClient};
use crate::state::Core;
pub use crate::users::{StaticUserDb, SystemUserDb, UserDb};

/// Everything [`Daemon::start`] needs.
pub struct DaemonOptions {
    pub config: Config,
    pub helper: Arc<dyn Helper>,
    pub users: Arc<dyn UserDb>,
    pub gpus: Arc<dyn GpuDetector>,
    /// Testing only: honour the [`api::TEST_UID_HEADER`] header instead of
    /// `SO_PEERCRED`. Must never be enabled in production.
    pub trust_uid_header: bool,
}

impl DaemonOptions {
    /// The production setup: the real helper connection, NSS user lookups
    /// and GPUs from the configuration.
    pub fn new(config: Config) -> Self {
        Self {
            helper: Arc::new(HelperClient::new(&config.helper_socket)),
            users: Arc::new(SystemUserDb),
            gpus: Arc::new(StaticGpuDetector::from_config(&config.gpus)),
            config,
            trust_uid_header: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(transparent)]
    Storage(#[from] puzuz_core::storage::StorageError),
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
            trust_uid_header,
        } = options;

        let db_path = config.state_db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StartError::CreateDir {
                path: parent.to_owned(),
                source,
            })?;
        }
        let store = Store::open(&db_path)?;

        let mut gpu_memory = HashMap::new();
        match gpus.detect() {
            Ok(detected) => {
                for gpu in detected {
                    store.upsert_gpu(gpu.index, &gpu.uuid, &gpu.name)?;
                    if let Some(bytes) = gpu.memory_total_bytes {
                        gpu_memory.insert(gpu.index, bytes);
                    }
                }
            }
            Err(e) => tracing::warn!("GPU detection failed: {e}"),
        }

        let listener = bind(&config.api_socket).map_err(|source| StartError::Listen {
            path: config.api_socket.clone(),
            source,
        })?;
        let socket = config.api_socket.clone();

        let core = Core::new(config, store, helper, users, gpu_memory, trust_uid_header);
        let notices = core.helper.connect();
        let tasks = vec![
            tokio::spawn(core.clone().notice_loop(notices)),
            tokio::spawn(core.clone().scheduler_loop()),
        ];

        let app = api::router(core.clone()).into_make_service_with_connect_info::<api::PeerCred>();
        let cancel = core.cancel.clone();
        let server = tokio::spawn(async move {
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(cancel.cancelled_owned())
                .await;
            if let Err(e) = result {
                tracing::error!("API server failed: {e}");
            }
        });
        tracing::info!(socket = %socket.display(), "puzuz daemon listening");
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

    /// Resolves once `puzuz shutdown` has checkpointed or stopped every
    /// job; the caller should then [`stop`](Daemon::stop) the daemon.
    pub async fn shutdown_requested(&self) {
        self.core.shutdown_complete.cancelled().await;
    }

    /// Stops serving and disconnects from the helper. Jobs are not touched.
    pub async fn stop(self) {
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

/// `puzuz daemon`: runs until SIGTERM/SIGINT or until `puzuz shutdown` has
/// finished.
pub async fn run(config: Config) -> Result<(), StartError> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(io::stderr)
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
