//! Runtime configuration of the helper.

use std::path::PathBuf;
use std::time::Duration;

use puzuz_checkpoint::CheckpointConfig;

/// Default directory for the helper's own state (event queue, job records).
pub const DEFAULT_STATE_DIR: &str = "/var/lib/puzuz/helper";
/// Default base directory of checkpoint images.
pub const DEFAULT_CHECKPOINT_BASE: &str = "/var/lib/puzuz/checkpoints";
/// Default base directory of job logs (`<base>/<uid>/<id>.log`).
pub const DEFAULT_LOG_BASE: &str = "/var/lib/puzuz/logs";
/// Default bind mount of `/` passed to `criu restore --root`. Not under
/// `/run/puzuz`, which the daemon can write to: the helper must never mount
/// onto a path a less privileged user controls.
pub const DEFAULT_CRIU_ROOT: &str = "/run/puzuz-helper/criu-root";
/// Name of the shutdown manifest written into the checkpoint base.
pub const SHUTDOWN_MANIFEST: &str = "shutdown-manifest.json";

#[derive(Debug, Clone)]
pub struct HelperConfig {
    /// Where the daemon connects.
    pub socket_path: PathBuf,
    /// The only uid allowed to connect (the `puzuz` system user).
    pub daemon_uid: u32,
    /// Group given to per-user log directories so the daemon can read logs.
    pub daemon_gid: u32,
    /// Event queue and per-job records. Must be root-owned.
    pub state_dir: PathBuf,
    /// `<base>/<job id>/` holds a job's checkpoint. Must be root-only.
    pub checkpoint_base: PathBuf,
    /// Per-user log directories under this base are created by the helper.
    pub log_base: PathBuf,
    /// `criu restore --root`: a bind mount of `/` (needed to restore the
    /// job's mount namespace). `None` disables `--root`.
    pub criu_root: Option<PathBuf>,
    pub checkpoint: CheckpointConfig,
    /// How often running jobs are polled for exit.
    pub poll_interval: Duration,
    /// How long a spawn may take until the job's shim reports back.
    pub spawn_timeout: Duration,
    /// `TimeoutStopSec` of job units: SIGTERM → SIGKILL grace when systemd
    /// stops a job (e.g. a `no_checkpoint` job at shutdown).
    pub job_stop_timeout: Duration,
    /// Path of this binary, run as `<exe> shim <job dir>` in job units.
    pub helper_exe: PathBuf,
    /// `nvidia-smi`, used to find CUDA processes and GPU memory.
    pub nvidia_smi: PathBuf,
}

impl Default for HelperConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from(puzuz_protocol::helper::DEFAULT_SOCKET_PATH),
            daemon_uid: u32::MAX,
            daemon_gid: u32::MAX,
            state_dir: PathBuf::from(DEFAULT_STATE_DIR),
            checkpoint_base: PathBuf::from(DEFAULT_CHECKPOINT_BASE),
            log_base: PathBuf::from(DEFAULT_LOG_BASE),
            criu_root: Some(PathBuf::from(DEFAULT_CRIU_ROOT)),
            checkpoint: CheckpointConfig::default(),
            poll_interval: Duration::from_secs(1),
            spawn_timeout: Duration::from_secs(30),
            job_stop_timeout: Duration::from_secs(30),
            helper_exe: PathBuf::from("/usr/bin/puzuz-helper"),
            nvidia_smi: PathBuf::from("nvidia-smi"),
        }
    }
}

impl HelperConfig {
    pub fn jobs_dir(&self) -> PathBuf {
        self.state_dir.join("jobs")
    }

    pub fn events_dir(&self) -> PathBuf {
        self.state_dir.join("events")
    }
}
