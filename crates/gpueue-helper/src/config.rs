//! Runtime configuration of the helper.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpueue_checkpoint::{CheckpointConfig, CudaMode};
use gpueue_config::CudaCheckpointMode;

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
pub const DEFAULT_STATE_DIR: &str = "/var/lib/gpueue/helper";
/// Default base directory of checkpoint images.
pub const DEFAULT_CHECKPOINT_BASE: &str = "/var/lib/gpueue/checkpoints";
/// Default base directory of job logs (`<base>/<uid>/<id>.log`).
pub const DEFAULT_LOG_BASE: &str = "/var/lib/gpueue/logs";
/// Default bind mount of `/` passed to `criu restore --root`. Not under
/// `/run/gpueue`, which the daemon can write to: the helper must never mount
/// onto a path a less privileged user controls.
pub const DEFAULT_CRIU_ROOT: &str = "/run/gpueue-helper/criu-root";
/// Default directory of the fork guard library: `<data_dir>/lib`, readable
/// by every user and persistent (restored jobs map it again).
pub const DEFAULT_FORK_GUARD_DIR: &str = "/var/lib/gpueue/lib";
/// Stands in for the directory of a program that was not found in `PATH`
/// at start-up (see [`HelperConfig::resolve_programs`]); it does not exist,
/// so running the program fails instead of looking it up again.
pub const MISSING_PROGRAM_DIR: &str = "/nonexistent/gpueue-helper";
/// Name of the shutdown manifest written into the checkpoint base.
pub const SHUTDOWN_MANIFEST: &str = "shutdown-manifest.json";

#[derive(Debug, Clone)]
pub struct HelperConfig {
    /// Where the daemon connects.
    pub socket_path: PathBuf,
    /// The only uid allowed to connect (the `gpueue` system user).
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
    /// Nix GC roots that keep `helper_exe` while jobs need it (see
    /// [`crate::gcroots`]); disabled unless it runs from the Nix store.
    pub gc_roots: crate::gcroots::GcRoots,
    /// Where the copies of the helper binary that job units run are kept
    /// (see [`crate::helperbin`]); `None` disables them. `<data_dir>/lib`
    /// once resolved from the configuration file.
    pub helper_copy_dir: Option<PathBuf>,
    /// What new job units run as `<exe> shim <job dir>`: the copy of the
    /// helper in `helper_copy_dir`, set at start-up when it could be made;
    /// `None` runs `helper_exe` itself (NixOS, tests).
    pub shim_exe: Option<PathBuf>,
    /// `nvidia-smi`, used to find CUDA processes and GPU memory.
    pub nvidia_smi: PathBuf,
    /// Where the fork guard is installed at start-up (see
    /// [`crate::forkguard`]); `None` disables it.
    pub fork_guard_dir: Option<PathBuf>,
    /// The installed fork guard, put into every job's `LD_PRELOAD`. Set at
    /// start-up when the installation succeeded.
    pub job_preload: Option<PathBuf>,
    /// Restrict every job unit's device access to its assigned GPUs (see
    /// [`crate::devices`]).
    pub gpu_device_isolation: bool,
    /// Who jobs may run as. The default is
    /// [`JobPolicy::permissive`](crate::validate::JobPolicy::permissive)
    /// (for tests); [`HelperConfig::resolve`] sets the configured one.
    pub job_policy: crate::validate::JobPolicy,
}

impl Default for HelperConfig {
    fn default() -> Self {
        Self {
            socket_path: PathBuf::from(gpueue_protocol::helper::DEFAULT_SOCKET_PATH),
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
            helper_exe: PathBuf::from("/usr/bin/gpueue-helper"),
            gc_roots: crate::gcroots::GcRoots::disabled(),
            helper_copy_dir: None,
            shim_exe: None,
            nvidia_smi: PathBuf::from("nvidia-smi"),
            fork_guard_dir: Some(PathBuf::from(DEFAULT_FORK_GUARD_DIR)),
            job_preload: None,
            gpu_device_isolation: true,
            job_policy: crate::validate::JobPolicy::permissive(),
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
    pub fn resolve(file: &gpueue_config::Config, o: Overrides) -> Resolved {
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
            helper_copy_dir: Some(file.data_dir.join("lib")),
            gpu_device_isolation: file.gpu_device_isolation,
            job_policy: crate::validate::JobPolicy::from_config(
                file,
                Path::new(gpueue_config::job_policy::LOGIN_DEFS_PATH),
                Path::new(gpueue_config::job_policy::GROUP_FILE_PATH),
            ),
            ..HelperConfig::default()
        };
        // The system D-Bus directory unless jobs may reach it, then the
        // daemon socket directories of `job_hidden_socket_dirs`.
        cfg.mounts.hidden_dirs = if file.job_system_dbus {
            Vec::new()
        } else {
            crate::jobs::SYSTEM_BUS_DIRS.map(String::from).to_vec()
        };
        cfg.mounts
            .hidden_dirs
            .extend(file.job_hidden_socket_dirs.iter().cloned());
        if file.job_systemd_varlink {
            // Jobs may reach systemd's Varlink services: /run/systemd stays.
            cfg.mounts.systemd_run_keep = None;
        }
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

    /// Replaces the programs given by name (`criu`, `cuda-checkpoint`,
    /// `nvidia-smi`, the defaults) with where they are in `path_env` (the
    /// service's `PATH`), so that they are checked like absolute ones (see
    /// [`Self::paths_to_check`]) and never looked up again. A program that
    /// is not found is set to [`MISSING_PROGRAM_DIR`]`/<name>`, which does
    /// not exist (installing it later needs a restart of the helper), and
    /// reported in the returned warnings. A relative path with a directory
    /// is refused.
    pub fn resolve_programs(&mut self, path_env: &std::ffi::OsStr) -> Result<Vec<String>, String> {
        let mut warnings = Vec::new();
        for (key, program) in [
            ("criu_path", &mut self.checkpoint.criu.criu_path),
            ("cuda_checkpoint_path", &mut self.checkpoint.cuda.path),
            ("nvidia_smi_path", &mut self.nvidia_smi),
        ] {
            if program.is_absolute() {
                continue;
            }
            if program.components().count() != 1 {
                return Err(format!(
                    "{key} {} must be an absolute path or a program name",
                    program.display()
                ));
            }
            match gpueue_config::trust::find_program(program, path_env) {
                Some(found) => *program = found,
                None => {
                    let missing = Path::new(MISSING_PROGRAM_DIR).join(&*program);
                    warnings.push(format!(
                        "{key}: {} is not in PATH ({}); it is unavailable until the helper is \
                         restarted after it was installed",
                        program.display(),
                        path_env.to_string_lossy()
                    ));
                    *program = missing;
                }
            }
        }
        Ok(warnings)
    }

    /// The programs [`Self::resolve_programs`] did not find (by name), for
    /// the helper's Hello: `gpueue doctor` reports them.
    pub fn missing_programs(&self) -> Vec<String> {
        [
            &self.checkpoint.criu.criu_path,
            &self.checkpoint.cuda.path,
            &self.nvidia_smi,
        ]
        .into_iter()
        .filter_map(|p| p.strip_prefix(MISSING_PROGRAM_DIR).ok())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
    }

    /// The programs [`Self::resolve_programs`] found (or that were given
    /// as absolute paths), with where they are, for the helper's Hello:
    /// `gpueue doctor` checks that they are still there.
    pub fn resolved_programs(&self) -> Vec<gpueue_protocol::helper::HelperProgram> {
        [
            ("criu", &self.checkpoint.criu.criu_path),
            ("cuda-checkpoint", &self.checkpoint.cuda.path),
            ("nvidia-smi", &self.nvidia_smi),
        ]
        .into_iter()
        .filter(|(_, path)| path.is_absolute() && !path.starts_with(MISSING_PROGRAM_DIR))
        .map(|(name, path)| gpueue_protocol::helper::HelperProgram {
            name: name.to_owned(),
            path: path.clone(),
        })
        .collect()
    }

    /// Paths root writes to, executes or loads code from: they and their
    /// existing ancestors must not be changeable by anyone but root.
    /// Programs given by name are only checked once
    /// [`Self::resolve_programs`] made them absolute. Checked at start-up,
    /// before anything is created there.
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
        // The copies of the helper that job units run as root.
        if let Some(dir) = &self.helper_copy_dir
            && self.fork_guard_dir.as_ref() != Some(dir)
        {
            paths.push(dir);
        }
        // Created and mounted on as root in every job's namespace and before
        // restores (see `crate::mountfd::ensure_root_dir`): whoever could
        // change a directory on the way could have root create a directory
        // or mount a tmpfs somewhere else.
        for dir in &self.mounts.hidden_dirs {
            paths.push(Path::new(dir));
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

    /// What a new job unit runs as `<exe> shim <job dir>` (see
    /// [`Self::shim_exe`]).
    pub fn job_exe(&self) -> &Path {
        self.shim_exe.as_deref().unwrap_or(&self.helper_exe)
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

    fn file(text: &str) -> gpueue_config::Config {
        gpueue_config::Config::parse(text).unwrap()
    }

    #[test]
    fn built_in_defaults_without_file_or_flags() {
        let r = HelperConfig::resolve(&gpueue_config::Config::default(), Overrides::default());
        let c = &r.config;
        let d = HelperConfig::default();
        assert_eq!(r.daemon_user, "gpueue");
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
        assert_eq!(c.helper_copy_dir, Some(PathBuf::from("/srv/pz/lib")));
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
    fn the_system_bus_is_covered_unless_allowed() {
        let r = HelperConfig::resolve(&file(""), Overrides::default());
        assert_eq!(
            r.config.mounts.hidden_dirs,
            ["/run/dbus", "/var/run/dbus", "/run/libvirt", "/run/cups"]
        );
        assert_eq!(r.config.mounts, crate::jobs::MountPlan::default());
        let r = HelperConfig::resolve(&file("job_system_dbus = true\n"), Overrides::default());
        assert_eq!(r.config.mounts.hidden_dirs, ["/run/libvirt", "/run/cups"]);
        assert!(r.config.mounts.systemd_run_keep.is_some());
    }

    #[test]
    fn daemon_socket_dirs_follow_the_configuration() {
        let r = HelperConfig::resolve(&file("job_hidden_socket_dirs = []\n"), Overrides::default());
        assert_eq!(r.config.mounts.hidden_dirs, crate::jobs::SYSTEM_BUS_DIRS);
        let r = HelperConfig::resolve(
            &file(
                "job_system_dbus = true\njob_hidden_socket_dirs = [\"/nix/var/nix/daemon-socket\"]\n",
            ),
            Overrides::default(),
        );
        assert_eq!(r.config.mounts.hidden_dirs, ["/nix/var/nix/daemon-socket"]);
    }

    #[test]
    fn run_systemd_is_hidden_unless_allowed() {
        let r = HelperConfig::resolve(&file(""), Overrides::default());
        assert_eq!(
            r.config.mounts.systemd_run_keep.as_deref(),
            Some(&crate::jobs::SYSTEMD_RUN_KEEP.map(String::from)[..])
        );
        let r = HelperConfig::resolve(&file("job_systemd_varlink = true\n"), Overrides::default());
        assert_eq!(r.config.mounts.systemd_run_keep, None);
        assert_eq!(
            r.config.mounts.hidden_dirs[..2],
            crate::jobs::SYSTEM_BUS_DIRS
        );
    }

    #[test]
    fn gpu_device_isolation_is_on_unless_disabled() {
        let r = HelperConfig::resolve(&file(""), Overrides::default());
        assert!(r.config.gpu_device_isolation);
        assert!(HelperConfig::default().gpu_device_isolation);
        let r = HelperConfig::resolve(
            &file("gpu_device_isolation = false\n"),
            Overrides::default(),
        );
        assert!(!r.config.gpu_device_isolation);
    }

    #[test]
    fn job_policy_comes_from_the_file() {
        let r = HelperConfig::resolve(
            &file(
                "job_min_uid = 1500\njob_denied_groups = [\"docker\"]\njob_allowed_users = [\"a\"]\n",
            ),
            Overrides::default(),
        );
        let p = &r.config.job_policy;
        assert_eq!(p.min_uid, 1500);
        assert_eq!(p.denied_groups, ["docker"]);
        assert_eq!(p.allowed_users, ["a"]);
        assert_eq!(p.group_file, Path::new("/etc/group"));
        let r = HelperConfig::resolve(&gpueue_config::Config::default(), Overrides::default());
        assert_eq!(
            r.config.job_policy.denied_groups,
            gpueue_config::job_policy::DEFAULT_JOB_DENIED_GROUPS
        );
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
    fn programs_given_by_name_are_resolved_once_and_then_checked() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let criu = bin.join("criu");
        std::fs::write(&criu, "").unwrap();
        std::fs::set_permissions(&criu, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut c =
            HelperConfig::resolve(&gpueue_config::Config::default(), Overrides::default()).config;
        let warnings = c.resolve_programs(bin.as_os_str()).unwrap();
        assert_eq!(c.checkpoint.criu.criu_path, criu);
        assert_eq!(
            c.checkpoint.cuda.path,
            Path::new(MISSING_PROGRAM_DIR).join("cuda-checkpoint")
        );
        assert_eq!(
            c.nvidia_smi,
            Path::new(MISSING_PROGRAM_DIR).join("nvidia-smi")
        );
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].contains("cuda-checkpoint"), "{warnings:?}");
        assert_eq!(c.missing_programs(), ["cuda-checkpoint", "nvidia-smi"]);
        assert_eq!(
            c.resolved_programs(),
            [gpueue_protocol::helper::HelperProgram {
                name: "criu".to_owned(),
                path: criu.clone(),
            }]
        );
        let paths = c.paths_to_check();
        assert!(paths.contains(&criu.as_path()));
        // Absolute ones stay as they are; relative paths are refused.
        let mut c =
            HelperConfig::resolve(&file("criu_path = \"/opt/criu\"\n"), Overrides::default())
                .config;
        c.resolve_programs(bin.as_os_str()).unwrap();
        assert_eq!(c.checkpoint.criu.criu_path, Path::new("/opt/criu"));
        // (The file refuses those; a flag is not checked before.)
        let mut c = HelperConfig::resolve(
            &gpueue_config::Config::default(),
            Overrides {
                criu: Some("bin/criu".into()),
                ..Overrides::default()
            },
        )
        .config;
        let e = c.resolve_programs(dir.path().as_os_str()).unwrap_err();
        assert!(e.contains("criu_path"), "{e}");
    }

    #[test]
    fn data_dir_plugin_dir_and_fork_guard_dir_are_checked() {
        let r = HelperConfig::resolve(&file("data_dir = \"/srv/pz\"\n"), Overrides::default());
        let paths = r.config.paths_to_check();
        assert!(paths.contains(&Path::new("/srv/pz/criu-no-plugins")));
        assert!(paths.contains(&Path::new("/srv/pz/lib")));
        assert_eq!(
            paths
                .iter()
                .filter(|p| **p == Path::new("/srv/pz/lib"))
                .count(),
            1
        );
        // Without the fork guard, the directory still holds the copies of
        // the helper that job units run.
        let r = HelperConfig::resolve(&file("fork_guard = false\n"), Overrides::default());
        assert_eq!(
            r.config.helper_copy_dir,
            Some(PathBuf::from(DEFAULT_FORK_GUARD_DIR))
        );
        let paths = r.config.paths_to_check();
        assert!(paths.contains(&Path::new(DEFAULT_FORK_GUARD_DIR)));
    }
}
