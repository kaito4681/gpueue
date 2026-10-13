//! Runtime configuration of the helper.

use std::path::{Path, PathBuf};
use std::time::Duration;

use puzuz_checkpoint::{CheckpointConfig, CudaMode};
use puzuz_config::CudaCheckpointMode;

use crate::jobs::MountPlan;

/// Manual CUDA mode (the helper calls `cuda-checkpoint` itself) works with
/// the distribution's criu without the CUDA plugin; `--manage-cgroups=ignore`
/// keeps a restored tree in its restore unit's cgroup.
pub fn default_checkpoint_config() -> CheckpointConfig {
    let mut c = CheckpointConfig {
        mode: CudaMode::Manual,
        ..CheckpointConfig::default()
    };
    c.criu
        .extra_restore_args
        .push("--manage-cgroups=ignore".into());
    c
}

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
/// Default directory of the fork guard library: `<data_dir>/lib`, readable
/// by every user and persistent (restored jobs map it again).
pub const DEFAULT_FORK_GUARD_DIR: &str = "/var/lib/puzuz/lib";
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
    /// How the shim cleans each job's mount namespace.
    pub mounts: MountPlan,
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
    /// Where the fork guard is installed at start-up (see
    /// [`crate::forkguard`]); `None` disables it.
    pub fork_guard_dir: Option<PathBuf>,
    /// The installed fork guard, put into every job's `LD_PRELOAD`. Set at
    /// start-up when the installation succeeded.
    pub job_preload: Option<PathBuf>,
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
            checkpoint: default_checkpoint_config(),
            mounts: MountPlan::default(),
            poll_interval: Duration::from_secs(1),
            spawn_timeout: Duration::from_secs(30),
            job_stop_timeout: Duration::from_secs(30),
            helper_exe: PathBuf::from("/usr/bin/puzuz-helper"),
            nvidia_smi: PathBuf::from("nvidia-smi"),
            fork_guard_dir: Some(PathBuf::from(DEFAULT_FORK_GUARD_DIR)),
            job_preload: None,
        }
    }
}

/// Settings given on the command line. Each one that is set overrides the
/// configuration file.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub socket: Option<PathBuf>,
    pub daemon_user: Option<String>,
    pub state_dir: Option<PathBuf>,
    pub checkpoint_dir: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
    /// An empty path disables `criu restore --root`.
    pub criu_root: Option<PathBuf>,
    pub cuda_mode: Option<CudaMode>,
    pub criu: Option<PathBuf>,
    pub cuda_checkpoint: Option<PathBuf>,
    pub nvidia_smi: Option<PathBuf>,
    pub job_stop_timeout: Option<Duration>,
}

/// The helper's effective settings: [`HelperConfig`] plus the daemon's user
/// name (looked up by the caller).
#[derive(Debug, Clone)]
pub struct Resolved {
    pub config: HelperConfig,
    pub daemon_user: String,
}

impl HelperConfig {
    /// Combines the shared configuration file with command-line overrides:
    /// flag > file > built-in default (the file's defaults are the
    /// built-in ones). `daemon_uid`/`daemon_gid` and `helper_exe` are left
    /// at their defaults for the caller to fill in.
    pub fn resolve(file: &puzuz_config::Config, o: Overrides) -> Resolved {
        let mut cfg = HelperConfig {
            socket_path: o.socket.unwrap_or_else(|| file.helper_socket.clone()),
            state_dir: o.state_dir.unwrap_or_else(|| file.helper_state_dir()),
            checkpoint_base: o.checkpoint_dir.unwrap_or_else(|| file.checkpoints_dir()),
            log_base: o.log_dir.unwrap_or_else(|| file.logs_dir()),
            criu_root: match o.criu_root {
                Some(root) if root.as_os_str().is_empty() => None,
                Some(root) => Some(root),
                None => Some(PathBuf::from(DEFAULT_CRIU_ROOT)),
            },
            nvidia_smi: o.nvidia_smi.unwrap_or_else(|| file.nvidia_smi_path.clone()),
            job_stop_timeout: o
                .job_stop_timeout
                .unwrap_or(Duration::from_secs(file.grace_secs)),
            fork_guard_dir: file.fork_guard.then(|| file.data_dir.join("lib")),
            ..HelperConfig::default()
        };
        let ck = &mut cfg.checkpoint;
        ck.mode = o.cuda_mode.unwrap_or(match file.cuda_checkpoint_mode {
            CudaCheckpointMode::Manual => CudaMode::Manual,
            CudaCheckpointMode::Plugin => CudaMode::Plugin,
        });
        ck.criu.criu_path = o.criu.unwrap_or_else(|| file.criu_path.clone());
        ck.criu.empty_plugin_dir = file.data_dir.join("criu-no-plugins");
        // The daemon also asks for `--tcp-close` per checkpoint; this is the
        // helper's own default for dumps it starts (e.g. at shutdown).
        ck.criu.tcp_close = file.checkpoint_tcp_close;
        ck.cuda.path = o
            .cuda_checkpoint
            .unwrap_or_else(|| file.cuda_checkpoint_path.clone());
        ck.timeouts.criu_dump = file.criu_dump_timeout_secs.map(Duration::from_secs);
        ck.timeouts.criu_restore = file.criu_restore_timeout_secs.map(Duration::from_secs);
        ck.timeouts.deadline_base = Duration::from_secs(file.checkpoint_deadline_base_secs);
        ck.timeouts.deadline_per_gib = Duration::from_secs(file.checkpoint_deadline_secs_per_gib);
        Resolved {
            config: cfg,
            daemon_user: o.daemon_user.unwrap_or_else(|| file.daemon_user.clone()),
        }
    }

    /// Paths root writes to, executes or loads code from: they and their
    /// existing ancestors must not be changeable by anyone but root
    /// (programs given by name are looked up in `PATH`). Checked at
    /// start-up, before anything is created there.
    pub fn paths_to_check(&self) -> Vec<&Path> {
        let mut paths = vec![
            self.state_dir.as_path(),
            &self.checkpoint_base,
            &self.log_base,
        ];
        if let Some(root) = &self.criu_root {
            paths.push(root);
        }
        // Passed to root's `criu -L`: whoever could put a library there
        // would run code as root. Its parent is `data_dir`.
        paths.push(&self.checkpoint.criu.empty_plugin_dir);
        if let Some(dir) = &self.checkpoint.criu.plugin_dir {
            paths.push(dir);
        }
        // Installed by root and preloaded into every job.
        if let Some(dir) = &self.fork_guard_dir {
            paths.push(dir);
        }
        for program in [
            &self.checkpoint.criu.criu_path,
            &self.checkpoint.cuda.path,
            &self.nvidia_smi,
        ] {
            if program.is_absolute() {
                paths.push(program);
            }
        }
        paths
    }

    pub fn jobs_dir(&self) -> PathBuf {
        self.state_dir.join("jobs")
    }

    pub fn events_dir(&self) -> PathBuf {
        self.state_dir.join("events")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(text: &str) -> puzuz_config::Config {
        puzuz_config::Config::parse(text).unwrap()
    }

    #[test]
    fn built_in_defaults_without_file_or_flags() {
        let r = HelperConfig::resolve(&puzuz_config::Config::default(), Overrides::default());
        let c = &r.config;
        let d = HelperConfig::default();
        assert_eq!(r.daemon_user, "puzuz");
        assert_eq!(c.socket_path, d.socket_path);
        assert_eq!(c.state_dir, d.state_dir);
        assert_eq!(c.checkpoint_base, d.checkpoint_base);
        assert_eq!(c.log_base, d.log_base);
        assert_eq!(c.criu_root, d.criu_root);
        assert_eq!(c.nvidia_smi, d.nvidia_smi);
        assert_eq!(c.job_stop_timeout, d.job_stop_timeout);
        assert_eq!(c.checkpoint, d.checkpoint);
        assert_eq!(c.fork_guard_dir, d.fork_guard_dir);
    }

    #[test]
    fn file_overrides_defaults() {
        let f = file(
            r#"
            data_dir = "/srv/pz"
            logs_dir = "/logs"
            helper_socket = "/run/x/helper.sock"
            daemon_user = "svc"
            grace_secs = 7
            checkpoint_tcp_close = false
            criu_path = "/opt/criu"
            cuda_checkpoint_path = "/opt/cuda-checkpoint"
            nvidia_smi_path = "/opt/nvidia-smi"
            cuda_checkpoint_mode = "plugin"
            criu_dump_timeout_secs = 11
            criu_restore_timeout_secs = 12
            checkpoint_deadline_base_secs = 13
            checkpoint_deadline_secs_per_gib = 14
            daemon_only_key = "ignored"
            "#,
        );
        let r = HelperConfig::resolve(&f, Overrides::default());
        let c = &r.config;
        assert_eq!(r.daemon_user, "svc");
        assert_eq!(c.socket_path, PathBuf::from("/run/x/helper.sock"));
        assert_eq!(c.state_dir, PathBuf::from("/srv/pz/helper"));
        assert_eq!(c.fork_guard_dir, Some(PathBuf::from("/srv/pz/lib")));
        assert_eq!(c.checkpoint_base, PathBuf::from("/srv/pz/checkpoints"));
        assert_eq!(c.log_base, PathBuf::from("/logs"));
        assert_eq!(c.job_stop_timeout, Duration::from_secs(7));
        assert_eq!(c.nvidia_smi, PathBuf::from("/opt/nvidia-smi"));
        assert_eq!(c.checkpoint.mode, CudaMode::Plugin);
        assert_eq!(c.checkpoint.criu.criu_path, PathBuf::from("/opt/criu"));
        assert!(!c.checkpoint.criu.tcp_close);
        assert_eq!(
            c.checkpoint.criu.empty_plugin_dir,
            PathBuf::from("/srv/pz/criu-no-plugins")
        );
        assert_eq!(
            c.checkpoint.cuda.path,
            PathBuf::from("/opt/cuda-checkpoint")
        );
        assert_eq!(
            c.checkpoint.timeouts.criu_dump,
            Some(Duration::from_secs(11))
        );
        assert_eq!(
            c.checkpoint.timeouts.criu_restore,
            Some(Duration::from_secs(12))
        );
        assert_eq!(c.checkpoint.timeouts.deadline_base, Duration::from_secs(13));
        assert_eq!(
            c.checkpoint.timeouts.deadline_per_gib,
            Duration::from_secs(14)
        );
        // Settings the file does not have keep their defaults.
        assert!(
            c.checkpoint
                .criu
                .extra_restore_args
                .contains(&"--manage-cgroups=ignore".into())
        );
        assert_eq!(c.criu_root, Some(PathBuf::from(DEFAULT_CRIU_ROOT)));
    }

    #[test]
    fn flags_override_the_file() {
        let f = file(
            r#"
            helper_socket = "/run/x/helper.sock"
            daemon_user = "svc"
            grace_secs = 7
            criu_path = "/opt/criu"
            cuda_checkpoint_path = "/opt/cuda-checkpoint"
            nvidia_smi_path = "/opt/nvidia-smi"
            cuda_checkpoint_mode = "plugin"
            checkpoints_dir = "/ckpt"
            "#,
        );
        let o = Overrides {
            socket: Some("/flag/helper.sock".into()),
            daemon_user: Some("flaguser".into()),
            state_dir: Some("/flag/state".into()),
            checkpoint_dir: Some("/flag/ckpt".into()),
            log_dir: Some("/flag/logs".into()),
            criu_root: Some(PathBuf::new()),
            cuda_mode: Some(CudaMode::Manual),
            criu: Some("/flag/criu".into()),
            cuda_checkpoint: Some("/flag/cc".into()),
            nvidia_smi: Some("/flag/smi".into()),
            job_stop_timeout: Some(Duration::from_secs(3)),
        };
        let r = HelperConfig::resolve(&f, o);
        let c = &r.config;
        assert_eq!(r.daemon_user, "flaguser");
        assert_eq!(c.socket_path, PathBuf::from("/flag/helper.sock"));
        assert_eq!(c.state_dir, PathBuf::from("/flag/state"));
        assert_eq!(c.checkpoint_base, PathBuf::from("/flag/ckpt"));
        assert_eq!(c.log_base, PathBuf::from("/flag/logs"));
        assert_eq!(c.criu_root, None);
        assert_eq!(c.checkpoint.mode, CudaMode::Manual);
        assert_eq!(c.checkpoint.criu.criu_path, PathBuf::from("/flag/criu"));
        assert_eq!(c.checkpoint.cuda.path, PathBuf::from("/flag/cc"));
        assert_eq!(c.nvidia_smi, PathBuf::from("/flag/smi"));
        assert_eq!(c.job_stop_timeout, Duration::from_secs(3));
    }

    #[test]
    fn fork_guard_can_be_disabled() {
        let r = HelperConfig::resolve(&file("fork_guard = false\n"), Overrides::default());
        assert_eq!(r.config.fork_guard_dir, None);
    }

    #[test]
    fn absolute_programs_are_checked_but_path_lookups_are_not() {
        let r = HelperConfig::resolve(&file("criu_path = \"/opt/criu\"\n"), Overrides::default());
        let paths = r.config.paths_to_check();
        assert!(paths.contains(&Path::new("/opt/criu")));
        assert!(paths.contains(&Path::new(DEFAULT_CHECKPOINT_BASE)));
        assert!(!paths.contains(&Path::new("cuda-checkpoint")));
    }

    #[test]
    fn data_dir_plugin_dir_and_fork_guard_dir_are_checked() {
        let r = HelperConfig::resolve(&file("data_dir = \"/srv/pz\"\n"), Overrides::default());
        let paths = r.config.paths_to_check();
        assert!(paths.contains(&Path::new("/srv/pz/criu-no-plugins")));
        assert!(paths.contains(&Path::new("/srv/pz/lib")));
        let r = HelperConfig::resolve(&file("fork_guard = false\n"), Overrides::default());
        let paths = r.config.paths_to_check();
        assert!(!paths.contains(&Path::new(DEFAULT_FORK_GUARD_DIR)));
    }
}
