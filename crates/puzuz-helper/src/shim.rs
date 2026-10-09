//! The main process of a job unit: `puzuz-helper shim <job dir>`.
//!
//! It runs as the job's owner (systemd's `User=`) with only the ambient
//! capabilities `CAP_SYS_ADMIN` and `CAP_SETPCAP`, and is single-threaded
//! (no async runtime), which `unshare(CLONE_NEWNS)` requires.
//!
//! 1. Outer shim: blocks termination signals (a `KillUnit` aimed at the job
//!    also reaches it), unshares a PID and a mount namespace, makes all
//!    mounts private and starts `puzuz-helper shim-init <job dir>`, which
//!    becomes PID 1 of the new namespace. It waits for the init and writes
//!    `init-status`.
//! 2. Init (`shim-init`): unblocks signals, becomes a session leader inside
//!    the namespace (CRIU refuses a session leader outside it), mounts a
//!    fresh `/proc` (CUDA fails with error 304 without it), drops every
//!    capability, then — now with only the owner's permissions — opens the
//!    log file and enters the cwd, starts the job via the shell, reports
//!    `start.json`, reaps children until the job exits, writes
//!    `exit-status` and exits with the job's status.
//!
//! The init is the root of the dumped process tree. After a restore it is
//! the restored unit's main process and still writes `exit-status` (the
//! file stays open across checkpoint/restore).
//!
//! Signals: the init has no handlers, so signals from the host namespace
//! other than SIGKILL/SIGSTOP are not delivered to it (pid_namespaces(7));
//! they still reach the job.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use nix::sched::CloneFlags;
use nix::sys::signal::{SigSet, SigmaskHow, Signal, sigprocmask};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::Pid;
use puzuz_protocol::helper::ExitStatus;

use crate::jobs::{JobFiles, ShimSpec, StartFailure, StartReport, write_status_to};

/// Exit code of the shim when it could not start the job.
pub const SHIM_FAILURE: i32 = 125;

/// Signals the outer shim must survive (systemd's `KillUnit` and
/// `systemctl stop` send them to every process of the unit).
const BLOCKED: [Signal; 6] = [
    Signal::SIGTERM,
    Signal::SIGINT,
    Signal::SIGHUP,
    Signal::SIGQUIT,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
];

#[derive(Debug)]
struct Failure {
    kind: StartFailure,
    message: String,
}

impl Failure {
    fn other(message: impl Into<String>) -> Self {
        Self {
            kind: StartFailure::Other,
            message: message.into(),
        }
    }
}

fn report(files: &JobFiles, report: &StartReport) {
    // Best effort: if even this fails the helper times out and the journal
    // has the reason.
    let res = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(files.start())
        .and_then(|mut f| {
            f.write_all(&serde_json::to_vec(report)?)?;
            f.sync_all()
        });
    if let Err(e) = res {
        eprintln!("puzuz shim: writing {}: {e}", files.start().display());
    }
}

fn fail(files: &JobFiles, f: Failure) -> i32 {
    eprintln!("puzuz shim: {}", f.message);
    report(
        files,
        &StartReport::Failed {
            kind: f.kind,
            message: f.message,
        },
    );
    SHIM_FAILURE
}

/// `puzuz-helper shim <job dir>`. Returns the process exit code.
pub fn run_outer(job_dir: PathBuf) -> i32 {
    let files = JobFiles::from_dir(job_dir);
    match outer(&files) {
        Ok(code) => code,
        Err(f) => fail(&files, f),
    }
}

fn outer(files: &JobFiles) -> Result<i32, Failure> {
    let mut blocked = SigSet::empty();
    for s in BLOCKED {
        blocked.add(s);
    }
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&blocked), None)
        .map_err(|e| Failure::other(format!("blocking signals: {e}")))?;

    nix::sched::unshare(CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS)
        .map_err(|e| Failure::other(format!("creating PID/mount namespaces: {e}")))?;
    // Like `unshare --mount`: nothing mounted in the job's namespace (its
    // /proc) propagates back to the host.
    nix::mount::mount(
        None::<&str>,
        "/",
        None::<&str>,
        nix::mount::MsFlags::MS_REC | nix::mount::MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(|e| Failure::other(format!("making mounts private: {e}")))?;

    let exe = std::env::current_exe()
        .map_err(|e| Failure::other(format!("locating the helper binary: {e}")))?;
    let mut init = Command::new(exe)
        .arg("shim-init")
        .arg(files.dir())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| Failure::other(format!("starting the namespace init: {e}")))?;

    let status = init.wait().map_err(|e| Failure::other(e.to_string()))?;
    let status = match (status.code(), status.signal()) {
        (Some(code), _) => ExitStatus::Exited { code },
        (None, Some(signal)) => ExitStatus::Signaled { signal },
        _ => ExitStatus::Unknown,
    };
    if let Ok(mut f) = OpenOptions::new().write(true).open(files.init_status()) {
        let _ = write_status_to(&mut f, &status);
    }
    Ok(exit_code(status))
}

/// `puzuz-helper shim-init <job dir>`. Returns the process exit code.
pub fn run_init(job_dir: PathBuf) -> i32 {
    let files = JobFiles::from_dir(job_dir);
    match init(&files) {
        Ok(code) => code,
        Err(f) => fail(&files, f),
    }
}

fn init(files: &JobFiles) -> Result<i32, Failure> {
    sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
        .map_err(|e| Failure::other(format!("unblocking signals: {e}")))?;
    nix::unistd::setsid().map_err(|e| Failure::other(format!("setsid: {e}")))?;
    nix::mount::mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        nix::mount::MsFlags::MS_NOSUID
            | nix::mount::MsFlags::MS_NODEV
            | nix::mount::MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .map_err(|e| Failure::other(format!("mounting /proc: {e}")))?;
    drop_capabilities().map_err(|e| Failure::other(format!("dropping capabilities: {e}")))?;
    if nix::unistd::getuid().is_root() || nix::unistd::geteuid().is_root() {
        return Err(Failure::other("refusing to run a job as root"));
    }

    // From here on everything happens with the owner's permissions only.
    let spec: ShimSpec = std::fs::read(files.spec())
        .and_then(|b| serde_json::from_slice(&b).map_err(io::Error::from))
        .map_err(|e| Failure::other(format!("reading {}: {e}", files.spec().display())))?;
    let mut exit_file = OpenOptions::new()
        .write(true)
        .open(files.exit_status())
        .map_err(|e| Failure::other(format!("opening {}: {e}", files.exit_status().display())))?;

    let log = open_log(&spec.log_path)?;
    std::env::set_current_dir(&spec.cwd).map_err(|e| Failure {
        kind: StartFailure::PermissionDenied,
        message: format!("cannot enter {}: {e}", spec.cwd.display()),
    })?;
    let (program, args) = spec
        .argv
        .split_first()
        .ok_or_else(|| Failure::other("empty command line"))?;
    let stderr = log
        .try_clone()
        .map_err(|e| Failure::other(format!("duplicating the log fd: {e}")))?;
    let child = Command::new(program)
        .args(args)
        .env_clear()
        .envs(&spec.envs)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(stderr)
        .spawn()
        .map_err(|e| Failure::other(format!("running {program}: {e}")))?;
    let job_pid = Pid::from_raw(child.id() as i32);
    report(files, &StartReport::Started { ns_pid: child.id() });

    let status = reap_until(job_pid);
    if let Err(e) = write_status_to(&mut exit_file, &status) {
        eprintln!("puzuz shim: writing exit status: {e}");
    }
    Ok(exit_code(status))
}

/// Open (creating) the log for appending, as the owner, refusing symlinks
/// and anything that is not a regular file owned by the owner.
fn open_log(path: &Path) -> Result<File, Failure> {
    let denied = |e: io::Error| Failure {
        kind: StartFailure::PermissionDenied,
        message: format!("cannot open log {}: {e}", path.display()),
    };
    let f = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o640)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(path)
        .map_err(denied)?;
    let meta = f.metadata().map_err(denied)?;
    if !meta.is_file() || meta.uid() != nix::unistd::getuid().as_raw() {
        return Err(denied(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not a regular file owned by the job's owner",
        )));
    }
    Ok(f)
}

/// Reap children (as PID 1 must) until `job` exits; return its status.
fn reap_until(job: Pid) -> ExitStatus {
    loop {
        match waitpid(None::<Pid>, None) {
            Ok(WaitStatus::Exited(pid, code)) if pid == job => {
                return ExitStatus::Exited { code };
            }
            Ok(WaitStatus::Signaled(pid, sig, _)) if pid == job => {
                return ExitStatus::Signaled { signal: sig as i32 };
            }
            Ok(_) | Err(nix::errno::Errno::EINTR) => {}
            Err(_) => return ExitStatus::Unknown,
        }
    }
}

fn drop_capabilities() -> io::Result<()> {
    use rustix::thread::{
        CapabilitySet, CapabilitySets, clear_ambient_capability_set,
        remove_capability_from_bounding_set, set_capabilities, set_no_new_privs,
    };
    // Dropping from the bounding set needs CAP_SETPCAP, so it goes first.
    for cap in CapabilitySet::all().iter() {
        match remove_capability_from_bounding_set(cap) {
            Ok(()) => {}
            // Capabilities unknown to the running kernel.
            Err(rustix::io::Errno::INVAL) => {}
            Err(e) => return Err(e.into()),
        }
    }
    clear_ambient_capability_set()?;
    set_capabilities(
        None,
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::empty(),
            inheritable: CapabilitySet::empty(),
        },
    )?;
    set_no_new_privs(true)?;
    Ok(())
}

/// The shell convention for a status: the code, or 128 + signal.
pub fn exit_code(status: ExitStatus) -> i32 {
    match status {
        ExitStatus::Exited { code } => code,
        ExitStatus::Signaled { signal } => 128 + signal,
        ExitStatus::Unknown => SHIM_FAILURE,
    }
}
