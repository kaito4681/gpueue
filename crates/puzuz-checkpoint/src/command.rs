//! Command-line builders for `criu` and `cuda-checkpoint`.
//!
//! Every builder returns a [`CommandSpec`] (program + arguments) so the exact
//! invocation can be asserted in tests and logged before it is run. The
//! defaults mirror the flags that worked in the CRIU and cuda-checkpoint
//! proofs of concept.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// CRIU's built-in plugin directory (`CR_PLUGIN_DEFAULT`), used when no
/// `-L` is given.
pub const DEFAULT_CRIU_PLUGIN_DIR: &str = "/usr/lib/criu";
/// File name of CRIU's CUDA plugin (built with `make cuda_plugin`).
pub const CUDA_PLUGIN_FILE: &str = "cuda_plugin.so";

/// How GPU state is handled during a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CudaMode {
    /// CRIU's `cuda_plugin.so` drives `cuda-checkpoint` itself.
    #[default]
    Plugin,
    /// The helper calls `cuda-checkpoint` explicitly around `criu dump` /
    /// `criu restore`, and CRIU is pointed at an empty plugin directory.
    Manual,
}

impl fmt::Display for CudaMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CudaMode::Plugin => "plugin",
            CudaMode::Manual => "manual",
        })
    }
}

/// A program and its arguments, ready to be turned into a [`Command`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for a in args {
            self.arg(a);
        }
        self
    }

    /// Arguments as UTF-8 strings (lossy). Intended for tests and logs.
    pub fn args_lossy(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    /// Whether `flag` appears verbatim among the arguments.
    pub fn has_arg(&self, flag: &str) -> bool {
        self.args.iter().any(|a| a == flag)
    }

    /// The argument following the first occurrence of `flag`, if any.
    pub fn value_of(&self, flag: &str) -> Option<&OsStr> {
        let pos = self.args.iter().position(|a| a == flag)?;
        self.args.get(pos + 1).map(OsString::as_os_str)
    }

    /// Build a [`Command`] with stdin set to `/dev/null`. stdout/stderr are
    /// left for the caller to configure.
    pub fn to_command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args).stdin(Stdio::null());
        cmd
    }
}

impl fmt::Display for CommandSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.program.display())?;
        for a in &self.args {
            let a = a.to_string_lossy();
            if a.is_empty() || a.contains(|c: char| c.is_whitespace() || "'\"[]$\\".contains(c)) {
                write!(f, " '{}'", a.replace('\'', r"'\''"))?;
            } else {
                write!(f, " {a}")?;
            }
        }
        Ok(())
    }
}

/// Flags and paths for invoking `criu`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CriuOptions {
    /// Path to the `criu` binary.
    pub criu_path: PathBuf,
    /// `-v<N>` log verbosity (0-4).
    pub log_level: u8,
    /// `--link-remap`: needed for unlinked-but-open files such as the POSIX
    /// semaphores Python multiprocessing creates under `/dev/shm`.
    pub link_remap: bool,
    /// `--tcp-close` on dump and restore: TCP connections are closed rather
    /// than refusing to dump.
    pub tcp_close: bool,
    /// `--external mnt[] --enable-external-masters` on dump and restore, needed
    /// when the job runs in its own mount namespace.
    pub external_mounts: bool,
    /// Plugin directory passed as `-L` in plugin mode. `None` keeps CRIU's
    /// built-in default (usually `/usr/lib/criu`).
    pub plugin_dir: Option<PathBuf>,
    /// An empty directory passed as `-L` in manual mode so that
    /// `cuda_plugin.so` is not loaded.
    pub empty_plugin_dir: PathBuf,
    /// Extra arguments appended to every `criu dump`.
    pub extra_dump_args: Vec<OsString>,
    /// Extra arguments appended to every `criu restore`.
    pub extra_restore_args: Vec<OsString>,
}

impl Default for CriuOptions {
    fn default() -> Self {
        Self {
            criu_path: PathBuf::from("criu"),
            log_level: 4,
            link_remap: true,
            tcp_close: true,
            external_mounts: true,
            plugin_dir: None,
            empty_plugin_dir: PathBuf::from("/var/lib/puzuz/criu-no-plugins"),
            extra_dump_args: Vec::new(),
            extra_restore_args: Vec::new(),
        }
    }
}

impl CriuOptions {
    /// Where CRIU looks for `cuda_plugin.so` in plugin mode.
    pub fn cuda_plugin_path(&self) -> PathBuf {
        self.plugin_dir
            .as_deref()
            .unwrap_or(Path::new(DEFAULT_CRIU_PLUGIN_DIR))
            .join(CUDA_PLUGIN_FILE)
    }

    fn push_common(&self, cmd: &mut CommandSpec, mode: CudaMode) {
        cmd.arg(format!("-v{}", self.log_level.min(4)));
        if self.external_mounts {
            cmd.args(["--external", "mnt[]", "--enable-external-masters"]);
        }
        if self.tcp_close {
            cmd.arg("--tcp-close");
        }
        match mode {
            CudaMode::Plugin => {
                if let Some(dir) = &self.plugin_dir {
                    cmd.arg("-L").arg(dir);
                }
            }
            CudaMode::Manual => {
                cmd.arg("-L").arg(&self.empty_plugin_dir);
            }
        }
    }
}

/// What to dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpTarget {
    /// Host PID of the root of the dumped tree (the job's PID-namespace init).
    pub tree_root_pid: u32,
    pub images_dir: PathBuf,
    /// Absolute path of the CRIU log file.
    pub log_file: PathBuf,
    /// `--leave-running`: keep the tree running after the dump (periodic
    /// checkpoints; out of the initial scope).
    pub leave_running: bool,
}

/// What to restore and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreTarget {
    pub images_dir: PathBuf,
    /// Absolute path of the CRIU log file.
    pub log_file: PathBuf,
    /// `--restore-detached`: `criu restore` exits once the tree runs.
    pub detached: bool,
    /// `--root`: required to restore a mount namespace (a bind mount of `/`).
    pub root: Option<PathBuf>,
    /// `--pidfile`: where CRIU writes the restored root's host PID.
    pub pidfile: Option<PathBuf>,
    /// `--inherit-fd fd[N]:<resource>` pairs.
    pub inherit_fds: Vec<(i32, String)>,
}

/// `criu dump -t <pid> -D <dir> -o <log> ...`.
pub fn criu_dump(opts: &CriuOptions, mode: CudaMode, target: &DumpTarget) -> CommandSpec {
    let mut cmd = CommandSpec::new(&opts.criu_path);
    cmd.arg("dump")
        .arg("-t")
        .arg(target.tree_root_pid.to_string())
        .arg("-D")
        .arg(&target.images_dir)
        .arg("-o")
        .arg(&target.log_file);
    opts.push_common(&mut cmd, mode);
    if opts.link_remap {
        cmd.arg("--link-remap");
    }
    if target.leave_running {
        cmd.arg("--leave-running");
    }
    cmd.args(&opts.extra_dump_args);
    cmd
}

/// `criu restore -D <dir> -o <log> ...`.
pub fn criu_restore(opts: &CriuOptions, mode: CudaMode, target: &RestoreTarget) -> CommandSpec {
    let mut cmd = CommandSpec::new(&opts.criu_path);
    cmd.arg("restore")
        .arg("-D")
        .arg(&target.images_dir)
        .arg("-o")
        .arg(&target.log_file);
    opts.push_common(&mut cmd, mode);
    if target.detached {
        cmd.arg("--restore-detached");
    }
    if let Some(root) = &target.root {
        cmd.arg("--root").arg(root);
    }
    if let Some(pidfile) = &target.pidfile {
        cmd.arg("--pidfile").arg(pidfile);
    }
    for (fd, resource) in &target.inherit_fds {
        cmd.arg("--inherit-fd").arg(format!("fd[{fd}]:{resource}"));
    }
    cmd.args(&opts.extra_restore_args);
    cmd
}

/// `criu --version`.
pub fn criu_version(opts: &CriuOptions) -> CommandSpec {
    let mut cmd = CommandSpec::new(&opts.criu_path);
    cmd.arg("--version");
    cmd
}

/// Flags and paths for invoking `cuda-checkpoint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CudaCheckpointOptions {
    /// Path to the `cuda-checkpoint` binary.
    pub path: PathBuf,
    /// `--timeout` (ms) for the lock action. A lock that cannot quiesce CUDA
    /// work in time fails with "device not ready" and leaves the process
    /// running. Note that this does *not* bound a hang on a stopped or frozen
    /// process; callers must also apply an outer timeout.
    pub lock_timeout: Option<Duration>,
}

impl Default for CudaCheckpointOptions {
    fn default() -> Self {
        Self {
            path: PathBuf::from("cuda-checkpoint"),
            lock_timeout: Some(Duration::from_secs(30)),
        }
    }
}

/// The four steps of `cuda-checkpoint --action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CudaAction {
    Lock,
    Checkpoint,
    Restore,
    Unlock,
}

impl CudaAction {
    pub fn as_str(self) -> &'static str {
        match self {
            CudaAction::Lock => "lock",
            CudaAction::Checkpoint => "checkpoint",
            CudaAction::Restore => "restore",
            CudaAction::Unlock => "unlock",
        }
    }
}

/// Error building a [`DeviceMap`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeviceMapError {
    #[error("device map is empty")]
    Empty,
    #[error("invalid GPU UUID in device map: {0:?}")]
    InvalidUuid(String),
    #[error("GPU {0} is mapped more than once")]
    Duplicate(String),
}

/// `--device-map old=new,...` for `cuda-checkpoint --action restore`.
///
/// When given, it must contain every checkpointed device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceMap(Vec<(String, String)>);

impl DeviceMap {
    pub fn new<I, A, B>(pairs: I) -> Result<Self, DeviceMapError>
    where
        I: IntoIterator<Item = (A, B)>,
        A: Into<String>,
        B: Into<String>,
    {
        let pairs: Vec<(String, String)> = pairs
            .into_iter()
            .map(|(a, b)| (a.into(), b.into()))
            .collect();
        if pairs.is_empty() {
            return Err(DeviceMapError::Empty);
        }
        let valid = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        };
        for (i, (old, new)) in pairs.iter().enumerate() {
            for u in [old, new] {
                if !valid(u) {
                    return Err(DeviceMapError::InvalidUuid(u.clone()));
                }
            }
            if pairs[..i].iter().any(|(o, _)| o == old) {
                return Err(DeviceMapError::Duplicate(old.clone()));
            }
        }
        Ok(Self(pairs))
    }

    pub fn pairs(&self) -> &[(String, String)] {
        &self.0
    }

    /// The `old=new,old=new` argument string.
    pub fn to_arg(&self) -> String {
        self.0
            .iter()
            .map(|(o, n)| format!("{o}={n}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// `cuda-checkpoint --action <action> --pid <pid> [--timeout ms] [--device-map ...]`.
///
/// The timeout is only added for [`CudaAction::Lock`] and the device map only
/// for [`CudaAction::Restore`]; the CLI ignores or rejects them otherwise.
pub fn cuda_action(
    opts: &CudaCheckpointOptions,
    action: CudaAction,
    pid: u32,
    device_map: Option<&DeviceMap>,
) -> CommandSpec {
    let mut cmd = CommandSpec::new(&opts.path);
    cmd.arg("--action")
        .arg(action.as_str())
        .arg("--pid")
        .arg(pid.to_string());
    if action == CudaAction::Lock
        && let Some(t) = opts.lock_timeout
    {
        cmd.arg("--timeout").arg(t.as_millis().to_string());
    }
    if action == CudaAction::Restore
        && let Some(map) = device_map
    {
        cmd.arg("--device-map").arg(map.to_arg());
    }
    cmd
}

pub fn cuda_lock(opts: &CudaCheckpointOptions, pid: u32) -> CommandSpec {
    cuda_action(opts, CudaAction::Lock, pid, None)
}

pub fn cuda_checkpoint(opts: &CudaCheckpointOptions, pid: u32) -> CommandSpec {
    cuda_action(opts, CudaAction::Checkpoint, pid, None)
}

pub fn cuda_restore(
    opts: &CudaCheckpointOptions,
    pid: u32,
    device_map: Option<&DeviceMap>,
) -> CommandSpec {
    cuda_action(opts, CudaAction::Restore, pid, device_map)
}

pub fn cuda_unlock(opts: &CudaCheckpointOptions, pid: u32) -> CommandSpec {
    cuda_action(opts, CudaAction::Unlock, pid, None)
}

/// `cuda-checkpoint --get-state --pid <pid>`.
pub fn cuda_get_state(opts: &CudaCheckpointOptions, pid: u32) -> CommandSpec {
    pid_command(opts, "--get-state", pid)
}

/// `cuda-checkpoint --toggle --pid <pid>`.
pub fn cuda_toggle(opts: &CudaCheckpointOptions, pid: u32) -> CommandSpec {
    pid_command(opts, "--toggle", pid)
}

/// `cuda-checkpoint --get-restore-tid --pid <pid>`.
pub fn cuda_get_restore_tid(opts: &CudaCheckpointOptions, pid: u32) -> CommandSpec {
    pid_command(opts, "--get-restore-tid", pid)
}

/// `cuda-checkpoint --help` (also prints the version line).
pub fn cuda_help(opts: &CudaCheckpointOptions) -> CommandSpec {
    let mut cmd = CommandSpec::new(&opts.path);
    cmd.arg("--help");
    cmd
}

fn pid_command(opts: &CudaCheckpointOptions, op: &str, pid: u32) -> CommandSpec {
    let mut cmd = CommandSpec::new(&opts.path);
    cmd.arg(op).arg("--pid").arg(pid.to_string());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dump_target() -> DumpTarget {
        DumpTarget {
            tree_root_pid: 4242,
            images_dir: "/ckpt/7/images".into(),
            log_file: "/ckpt/7/dump.log".into(),
            leave_running: false,
        }
    }

    fn restore_target() -> RestoreTarget {
        RestoreTarget {
            images_dir: "/ckpt/7/images".into(),
            log_file: "/ckpt/7/restore.log".into(),
            detached: true,
            root: Some("/run/puzuz/root".into()),
            pidfile: Some("/ckpt/7/restore.pid".into()),
            inherit_fds: vec![],
        }
    }

    #[test]
    fn dump_plugin_defaults_match_poc() {
        let cmd = criu_dump(&CriuOptions::default(), CudaMode::Plugin, &dump_target());
        assert_eq!(cmd.program, PathBuf::from("criu"));
        assert_eq!(
            cmd.args_lossy(),
            [
                "dump",
                "-t",
                "4242",
                "-D",
                "/ckpt/7/images",
                "-o",
                "/ckpt/7/dump.log",
                "-v4",
                "--external",
                "mnt[]",
                "--enable-external-masters",
                "--tcp-close",
                "--link-remap",
            ]
        );
        assert!(!cmd.has_arg("-L"));
    }

    #[test]
    fn dump_plugin_with_explicit_plugin_dir() {
        let opts = CriuOptions {
            plugin_dir: Some("/usr/local/lib/criu".into()),
            ..Default::default()
        };
        let cmd = criu_dump(&opts, CudaMode::Plugin, &dump_target());
        assert_eq!(cmd.value_of("-L"), Some(OsStr::new("/usr/local/lib/criu")));
    }

    #[test]
    fn dump_manual_uses_empty_plugin_dir() {
        let opts = CriuOptions {
            plugin_dir: Some("/usr/lib/criu".into()),
            empty_plugin_dir: "/var/lib/puzuz/empty".into(),
            ..Default::default()
        };
        let cmd = criu_dump(&opts, CudaMode::Manual, &dump_target());
        assert_eq!(cmd.value_of("-L"), Some(OsStr::new("/var/lib/puzuz/empty")));
        assert_eq!(cmd.args.iter().filter(|a| *a == "-L").count(), 1);
    }

    #[test]
    fn dump_flags_can_be_disabled() {
        let opts = CriuOptions {
            link_remap: false,
            tcp_close: false,
            external_mounts: false,
            log_level: 9,
            extra_dump_args: vec!["--shell-job".into()],
            ..Default::default()
        };
        let mut t = dump_target();
        t.leave_running = true;
        let cmd = criu_dump(&opts, CudaMode::Plugin, &t);
        assert!(!cmd.has_arg("--link-remap"));
        assert!(!cmd.has_arg("--tcp-close"));
        assert!(!cmd.has_arg("--external"));
        assert!(!cmd.has_arg("--enable-external-masters"));
        assert!(cmd.has_arg("-v4"), "log level is clamped");
        assert!(cmd.has_arg("--leave-running"));
        assert_eq!(cmd.args_lossy().last().unwrap(), "--shell-job");
    }

    #[test]
    fn restore_detached_with_root_and_pidfile() {
        let cmd = criu_restore(&CriuOptions::default(), CudaMode::Plugin, &restore_target());
        assert_eq!(
            cmd.args_lossy(),
            [
                "restore",
                "-D",
                "/ckpt/7/images",
                "-o",
                "/ckpt/7/restore.log",
                "-v4",
                "--external",
                "mnt[]",
                "--enable-external-masters",
                "--tcp-close",
                "--restore-detached",
                "--root",
                "/run/puzuz/root",
                "--pidfile",
                "/ckpt/7/restore.pid",
            ]
        );
        assert!(!cmd.has_arg("--link-remap"), "link-remap is a dump option");
    }

    #[test]
    fn restore_attached_manual_with_inherit_fd() {
        let mut t = restore_target();
        t.detached = false;
        t.root = None;
        t.pidfile = None;
        t.inherit_fds = vec![(0, "pipe:[1234]".into())];
        let cmd = criu_restore(&CriuOptions::default(), CudaMode::Manual, &t);
        assert!(!cmd.has_arg("--restore-detached"));
        assert!(!cmd.has_arg("--root"));
        assert!(!cmd.has_arg("--pidfile"));
        assert_eq!(
            cmd.value_of("--inherit-fd"),
            Some(OsStr::new("fd[0]:pipe:[1234]"))
        );
        assert!(cmd.has_arg("-L"));
    }

    #[test]
    fn cuda_lock_has_timeout_in_ms() {
        let cmd = cuda_lock(&CudaCheckpointOptions::default(), 99);
        assert_eq!(cmd.program, PathBuf::from("cuda-checkpoint"));
        assert_eq!(
            cmd.args_lossy(),
            ["--action", "lock", "--pid", "99", "--timeout", "30000"]
        );
        let no_timeout = CudaCheckpointOptions {
            lock_timeout: None,
            ..Default::default()
        };
        assert!(!cuda_lock(&no_timeout, 99).has_arg("--timeout"));
    }

    #[test]
    fn cuda_other_actions() {
        let o = CudaCheckpointOptions::default();
        assert_eq!(
            cuda_checkpoint(&o, 5).args_lossy(),
            ["--action", "checkpoint", "--pid", "5"]
        );
        assert_eq!(
            cuda_unlock(&o, 5).args_lossy(),
            ["--action", "unlock", "--pid", "5"]
        );
        assert_eq!(
            cuda_restore(&o, 5, None).args_lossy(),
            ["--action", "restore", "--pid", "5"]
        );
        assert_eq!(
            cuda_get_state(&o, 5).args_lossy(),
            ["--get-state", "--pid", "5"]
        );
        assert_eq!(cuda_toggle(&o, 5).args_lossy(), ["--toggle", "--pid", "5"]);
        assert_eq!(
            cuda_get_restore_tid(&o, 5).args_lossy(),
            ["--get-restore-tid", "--pid", "5"]
        );
        assert_eq!(cuda_help(&o).args_lossy(), ["--help"]);
        // Timeout and device map are only attached to the actions that take them.
        let map = DeviceMap::new([("GPU-a", "GPU-b")]).unwrap();
        assert!(!cuda_action(&o, CudaAction::Checkpoint, 5, Some(&map)).has_arg("--device-map"));
        assert!(!cuda_action(&o, CudaAction::Unlock, 5, None).has_arg("--timeout"));
    }

    #[test]
    fn cuda_restore_with_device_map() {
        let map = DeviceMap::new([
            ("GPU-34b92e76-cfbc", "GPU-aaaa-bbbb"),
            ("GPU-1111", "GPU-2222"),
        ])
        .unwrap();
        let cmd = cuda_restore(&CudaCheckpointOptions::default(), 7, Some(&map));
        assert_eq!(
            cmd.value_of("--device-map"),
            Some(OsStr::new(
                "GPU-34b92e76-cfbc=GPU-aaaa-bbbb,GPU-1111=GPU-2222"
            ))
        );
    }

    #[test]
    fn device_map_validation() {
        assert_eq!(
            DeviceMap::new(Vec::<(String, String)>::new()),
            Err(DeviceMapError::Empty)
        );
        assert_eq!(
            DeviceMap::new([("GPU-a", "")]),
            Err(DeviceMapError::InvalidUuid(String::new()))
        );
        assert_eq!(
            DeviceMap::new([("GPU-a=b", "GPU-c")]),
            Err(DeviceMapError::InvalidUuid("GPU-a=b".into()))
        );
        assert_eq!(
            DeviceMap::new([("GPU-a", "GPU-b,GPU-c")]),
            Err(DeviceMapError::InvalidUuid("GPU-b,GPU-c".into()))
        );
        assert_eq!(
            DeviceMap::new([("GPU-a", "GPU-b"), ("GPU-a", "GPU-c")]),
            Err(DeviceMapError::Duplicate("GPU-a".into()))
        );
    }

    #[test]
    fn display_quotes_special_args() {
        let cmd = criu_dump(&CriuOptions::default(), CudaMode::Plugin, &dump_target());
        let s = cmd.to_string();
        assert!(s.starts_with("criu dump -t 4242 "));
        assert!(s.contains(" 'mnt[]' "));
    }

    #[test]
    fn to_command_preserves_program_and_args() {
        let spec = cuda_get_state(&CudaCheckpointOptions::default(), 1);
        let cmd = spec.to_command();
        assert_eq!(cmd.get_program(), "cuda-checkpoint");
        assert_eq!(
            cmd.get_args().collect::<Vec<_>>(),
            ["--get-state", "--pid", "1"]
        );
    }

    #[test]
    fn cuda_mode_serde() {
        assert_eq!(
            serde_json::to_string(&CudaMode::Manual).unwrap(),
            "\"manual\""
        );
        assert_eq!(
            serde_json::from_str::<CudaMode>("\"plugin\"").unwrap(),
            CudaMode::Plugin
        );
        assert_eq!(CudaMode::Manual.to_string(), "manual");
    }
}
