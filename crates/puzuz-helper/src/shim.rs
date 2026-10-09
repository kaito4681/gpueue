//! The main process of a job unit: `puzuz-helper shim <job dir>`.
//!
//! Follows the shim validated in `docker-systemd-poc.md` §3.1. It starts as
//! root (the unit has no `User=`) and is single-threaded (no async
//! runtime), which `unshare(CLONE_NEWNS)` requires.
//!
//! 1. Outer shim (the unit's main process, stays outside the dumped tree):
//!    unshares a PID and a mount namespace, starts
//!    `puzuz-helper shim-init <job dir>` — PID 1 of the new namespace —
//!    forwards SIGTERM/SIGINT/SIGHUP/SIGQUIT/SIGUSR1/SIGUSR2 to it, waits
//!    for it and writes `init-status`.
//! 2. Init (`shim-init`), still root:
//!    - becomes a session leader inside the namespace (CRIU refuses a
//!      session leader outside it),
//!    - opens `start.json` and `exit-status` (root-only, so the owner cannot
//!      forge them),
//!    - cleans the mount namespace so that it only holds mounts that also
//!      exist after a reboot: makes everything private, detaches the
//!      inherited `/proc` tree and mounts a fresh `/proc` (CUDA fails with
//!      error 304 without it), detaches volatile mounts (`/run/*`, `/mnt/*`,
//!      ...) and mounts private tmpfs on `/tmp` and `/dev/shm` (CRIU saves
//!      their contents in the image),
//!    - drops privileges: supplementary groups → gid → bounding set → uid →
//!      `no_new_privs`,
//!    - opens the log and enters the cwd with the owner's permissions only,
//!      starts the job in its own process group, forwards termination
//!      signals to that group, reaps every child, and when the job exits
//!      writes `exit-status`, kills what is left in the namespace and exits
//!      with the job's status (128 + signal for a signal).
//!
//! The init is the root of the dumped tree. After a restore it is the
//! restored unit's main process and still writes `exit-status` through the
//! fd it opened before the dump.
//!
//! Signals are received through a signalfd with the signals blocked, so no
//! signal handlers (and no `unsafe`) are needed. Blocked signals count as
//! handled for a namespace init, so they are delivered from the host
//! namespace too.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::CloneFlags;
use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, killpg, sigprocmask};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Gid, Pid, Uid};
use puzuz_protocol::helper::ExitStatus;

use crate::jobs::{JobFiles, MountPlan, ShimSpec, StartFailure, StartReport, write_status_to};

/// Exit code of the shim when it could not start the job.
pub const SHIM_FAILURE: i32 = 125;

/// Signals forwarded towards the job.
const FORWARDED: [Signal; 6] = [
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

    fn denied(message: impl Into<String>) -> Self {
        Self {
            kind: StartFailure::PermissionDenied,
            message: message.into(),
        }
    }
}

fn write_report(file: &mut File, report: &StartReport) -> io::Result<()> {
    use std::io::Seek;
    file.set_len(0)?;
    file.seek(io::SeekFrom::Start(0))?;
    file.write_all(&serde_json::to_vec(report)?)?;
    file.sync_all()
}

/// Report a failure through `start.json` (opened here if not yet open).
fn fail(files: &JobFiles, start: Option<&mut File>, f: Failure) -> i32 {
    eprintln!("puzuz shim: {}", f.message);
    let report = StartReport::Failed {
        kind: f.kind,
        message: f.message,
    };
    let res = match start {
        Some(file) => write_report(file, &report),
        None => OpenOptions::new()
            .write(true)
            .open(files.start())
            .and_then(|mut file| write_report(&mut file, &report)),
    };
    if let Err(e) = res {
        eprintln!("puzuz shim: writing {}: {e}", files.start().display());
    }
    SHIM_FAILURE
}

/// Block `FORWARDED` + SIGCHLD and return a signalfd for them.
fn signal_fd() -> nix::Result<SignalFd> {
    let mut set = SigSet::empty();
    for s in FORWARDED {
        set.add(s);
    }
    set.add(Signal::SIGCHLD);
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&set), None)?;
    SignalFd::with_flags(&set, SfdFlags::SFD_CLOEXEC)
}

fn status_of(ws: WaitStatus) -> Option<(Pid, ExitStatus)> {
    match ws {
        WaitStatus::Exited(pid, code) => Some((pid, ExitStatus::Exited { code })),
        WaitStatus::Signaled(pid, sig, _) => {
            Some((pid, ExitStatus::Signaled { signal: sig as i32 }))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Outer shim
// ---------------------------------------------------------------------------

/// `puzuz-helper shim <job dir>`. Returns the process exit code.
pub fn run_outer(job_dir: PathBuf) -> i32 {
    let files = JobFiles::from_dir(job_dir);
    match outer(&files) {
        Ok(code) => code,
        Err(f) => fail(&files, None, f),
    }
}

fn outer(files: &JobFiles) -> Result<i32, Failure> {
    let sfd = signal_fd().map_err(|e| Failure::other(format!("signalfd: {e}")))?;
    nix::sched::unshare(CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS)
        .map_err(|e| Failure::other(format!("creating PID/mount namespaces: {e}")))?;
    let exe = std::env::current_exe()
        .map_err(|e| Failure::other(format!("locating the helper binary: {e}")))?;
    // std resets the signal mask in the child.
    let init = Command::new(exe)
        .arg("shim-init")
        .arg(files.dir())
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| Failure::other(format!("starting the namespace init: {e}")))?;
    let init = Pid::from_raw(init.id() as i32);

    let status = loop {
        let Ok(Some(info)) = sfd.read_signal() else {
            continue;
        };
        let signo = info.ssi_signo as i32;
        if signo == Signal::SIGCHLD as i32 {
            match waitpid(init, Some(WaitPidFlag::WNOHANG))
                .ok()
                .and_then(status_of)
            {
                Some((_, st)) => break st,
                None => continue,
            }
        }
        if let Ok(sig) = Signal::try_from(signo) {
            let _ = kill(init, sig);
        }
    };
    if let Ok(mut f) = OpenOptions::new().write(true).open(files.init_status()) {
        let _ = write_status_to(&mut f, &status);
    }
    Ok(exit_code(status))
}

// ---------------------------------------------------------------------------
// Namespace init
// ---------------------------------------------------------------------------

/// `puzuz-helper shim-init <job dir>`. Returns the process exit code.
pub fn run_init(job_dir: PathBuf) -> i32 {
    let files = JobFiles::from_dir(job_dir);
    let mut start = match OpenOptions::new().write(true).open(files.start()) {
        Ok(f) => Some(f),
        Err(e) => {
            return fail(
                &files,
                None,
                Failure::other(format!("opening {}: {e}", files.start().display())),
            );
        }
    };
    match init(&files, &mut start) {
        Ok(code) => code,
        Err(f) => fail(&files, start.as_mut(), f),
    }
}

/// `start` is closed once the job runs: only `exit-status` may stay open in
/// the dumped tree (CRIU reopens open files by path on restore and checks
/// their size).
fn init(files: &JobFiles, start: &mut Option<File>) -> Result<i32, Failure> {
    let sfd = signal_fd().map_err(|e| Failure::other(format!("signalfd: {e}")))?;
    nix::unistd::setsid().map_err(|e| Failure::other(format!("setsid: {e}")))?;
    // Still the host's /proc: its `self` link names our host PID.
    let init_host_pid = std::fs::read_link("/proc/self")
        .ok()
        .and_then(|p| p.to_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0);
    let spec: ShimSpec = std::fs::read(files.spec())
        .and_then(|b| serde_json::from_slice(&b).map_err(io::Error::from))
        .map_err(|e| Failure::other(format!("reading {}: {e}", files.spec().display())))?;
    if spec.uid == 0 {
        return Err(Failure::other("refusing to run a job as root"));
    }
    let mut exit_file = OpenOptions::new()
        .write(true)
        .open(files.exit_status())
        .map_err(|e| Failure::other(format!("opening {}: {e}", files.exit_status().display())))?;

    setup_mounts(&spec.mounts).map_err(Failure::other)?;
    drop_privileges(&spec).map_err(|e| Failure::other(format!("dropping privileges: {e}")))?;

    // From here on everything happens with the owner's permissions only.
    let log = open_log(&spec.log_path, spec.log_mode)?;
    std::env::set_current_dir(&spec.cwd)
        .map_err(|e| Failure::denied(format!("cannot enter {}: {e}", spec.cwd.display())))?;
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
        .process_group(0)
        .spawn()
        .map_err(|e| Failure::other(format!("running {program}: {e}")))?;
    let job = Pid::from_raw(child.id() as i32);
    let report = StartReport::Started {
        ns_pid: child.id(),
        init_host_pid,
    };
    if let Some(mut file) = start.take()
        && let Err(e) = write_report(&mut file, &report)
    {
        eprintln!("puzuz shim: writing the start report: {e}");
    }
    // Our stdout/stderr are the unit's journal socket, which CRIU cannot
    // dump ("External socket is used"); the init is part of the dumped
    // tree, so let go of it. Later problems surface through the files.
    if let Ok(null) = OpenOptions::new().write(true).open("/dev/null") {
        let _ = nix::unistd::dup2_stdout(&null);
        let _ = nix::unistd::dup2_stderr(&null);
    }

    let mut status = None;
    loop {
        let Ok(Some(info)) = sfd.read_signal() else {
            continue;
        };
        let signo = info.ssi_signo as i32;
        if signo != Signal::SIGCHLD as i32 {
            if status.is_none()
                && let Ok(sig) = Signal::try_from(signo)
            {
                let _ = killpg(job, sig);
            }
            continue;
        }
        // Reap everything that has exited.
        loop {
            match waitpid(None::<Pid>, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::StillAlive) => break,
                Ok(ws) => {
                    if let Some((pid, st)) = status_of(ws)
                        && pid == job
                    {
                        if let Err(e) = write_status_to(&mut exit_file, &st) {
                            eprintln!("puzuz shim: writing exit status: {e}");
                        }
                        status = Some(st);
                        // Tear down leftovers in the namespace.
                        let _ = kill(Pid::from_raw(-1), Signal::SIGKILL);
                    }
                }
                Err(nix::errno::Errno::EINTR) => {}
                Err(_) => {
                    // ECHILD: nothing left.
                    return Ok(exit_code(status.unwrap_or(ExitStatus::Unknown)));
                }
            }
        }
    }
}

/// Whether `path` is `prefix` or below it; a trailing `/` on `prefix` means
/// strictly below.
pub fn path_under(path: &str, prefix: &str) -> bool {
    if prefix.len() > 1 && prefix.ends_with('/') {
        return path.starts_with(prefix);
    }
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// Mount points from `/proc/self/mountinfo` matching `plan`, in mountinfo
/// order (parents first).
pub fn mounts_to_prune(mountinfo: &str, plan: &MountPlan) -> Vec<String> {
    mountinfo
        .lines()
        .filter_map(|l| l.split_whitespace().nth(4))
        .map(unescape_octal)
        .filter(|m| {
            plan.prune.iter().any(|p| path_under(m, p))
                && !plan.keep.iter().any(|k| path_under(m, k))
        })
        .collect()
}

/// Undo mountinfo's `\ooo` escapes.
pub fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn setup_mounts(plan: &MountPlan) -> Result<(), String> {
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(|e| format!("making mounts private: {e}"))?;
    // Drop the inherited /proc tree (with submounts such as
    // /proc/driver/nvidia/params) and mount one for the new namespace.
    let _ = umount2("/proc", MntFlags::MNT_DETACH);
    mount(
        Some("proc"),
        "/proc",
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        None::<&str>,
    )
    .map_err(|e| format!("mounting /proc: {e}"))?;
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("reading mountinfo: {e}"))?;
    // Deepest first; mounts already gone with a parent fail harmlessly.
    for m in mounts_to_prune(&mountinfo, plan).iter().rev() {
        let _ = umount2(m.as_str(), MntFlags::MNT_DETACH);
    }
    for t in &plan.tmpfs {
        mount(
            Some("tmpfs"),
            t.as_str(),
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some("mode=1777"),
        )
        .map_err(|e| format!("mounting a tmpfs on {t}: {e}"))?;
    }
    Ok(())
}

fn drop_privileges(spec: &ShimSpec) -> io::Result<()> {
    use rustix::thread::{
        CapabilitySet, clear_ambient_capability_set, remove_capability_from_bounding_set,
        set_no_new_privs,
    };
    let gid = Gid::from_raw(spec.gid);
    match &spec.groups {
        Some(groups) => {
            let groups: Vec<Gid> = groups.iter().map(|g| Gid::from_raw(*g)).collect();
            nix::unistd::setgroups(&groups)?;
        }
        None => {
            let user = nix::unistd::User::from_uid(Uid::from_raw(spec.uid))?
                .ok_or_else(|| io::Error::other(format!("uid {} has no user entry", spec.uid)))?;
            let name = std::ffi::CString::new(user.name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            nix::unistd::initgroups(&name, gid)?;
        }
    }
    nix::unistd::setresgid(gid, gid, gid)?;
    clear_ambient_capability_set()?;
    for cap in CapabilitySet::all().iter() {
        match remove_capability_from_bounding_set(cap) {
            // EINVAL: unknown to this kernel.
            Ok(()) | Err(rustix::io::Errno::INVAL) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let uid = Uid::from_raw(spec.uid);
    // Changing every uid away from 0 also clears the permitted and
    // effective capability sets.
    nix::unistd::setresuid(uid, uid, uid)?;
    set_no_new_privs(true)?;
    if nix::unistd::getuid().is_root() || nix::unistd::geteuid().is_root() {
        return Err(io::Error::other("still root after dropping privileges"));
    }
    Ok(())
}

/// Open (creating) the log for appending, as the owner, refusing symlinks
/// and anything that is not a regular file owned by the owner.
fn open_log(path: &Path, mode: u32) -> Result<File, Failure> {
    let denied = |e: io::Error| Failure::denied(format!("cannot open log {}: {e}", path.display()));
    let f = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(mode & 0o660)
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
    // The unit's umask would strip group write; set the mode explicitly.
    use std::os::unix::fs::PermissionsExt;
    f.set_permissions(std::fs::Permissions::from_mode(mode & 0o660))
        .map_err(denied)?;
    Ok(f)
}

/// The shell convention for a status: the code, or 128 + signal.
pub fn exit_code(status: ExitStatus) -> i32 {
    match status {
        ExitStatus::Exited { code } => code,
        ExitStatus::Signaled { signal } => 128 + signal,
        ExitStatus::Unknown => SHIM_FAILURE,
    }
}

// ---------------------------------------------------------------------------
// criu restore wrapper
// ---------------------------------------------------------------------------

/// `puzuz-helper criu-wrap <root> <criu argv...>`: the `ExecStart` of a
/// restore unit. In a private mount namespace (so the bind mount never
/// shows up on the host or in later jobs' namespaces), recursively
/// bind-mounts `/` on `<root>` for `criu restore --root`, then execs criu.
/// Returns only on failure.
pub fn run_criu_wrap(root: PathBuf, argv: Vec<String>) -> i32 {
    let res = (|| -> Result<io::Error, String> {
        let (program, args) = argv.split_first().ok_or("no criu command")?;
        if root.as_os_str().is_empty() {
            return Ok(Command::new(program).args(args).exec());
        }
        nix::sched::unshare(CloneFlags::CLONE_NEWNS).map_err(|e| format!("unshare: {e}"))?;
        let private = MsFlags::MS_REC | MsFlags::MS_PRIVATE;
        mount(None::<&str>, "/", None::<&str>, private, None::<&str>)
            .map_err(|e| format!("making mounts private: {e}"))?;
        std::fs::create_dir_all(&root).map_err(|e| format!("creating {}: {e}", root.display()))?;
        mount(
            Some("/"),
            &root,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(|e| format!("bind-mounting / on {}: {e}", root.display()))?;
        mount(None::<&str>, &root, None::<&str>, private, None::<&str>)
            .map_err(|e| format!("making {} private: {e}", root.display()))?;
        Ok(Command::new(program).args(args).exec())
    })();
    match res {
        Ok(e) => eprintln!("puzuz criu-wrap: exec: {e}"),
        Err(e) => eprintln!("puzuz criu-wrap: {e}"),
    }
    SHIM_FAILURE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prunes_volatile_mounts_but_keeps_exceptions() {
        let mi = "\
1 0 0:1 / / rw - ext4 /dev/x rw
2 1 0:2 / /run rw - tmpfs tmpfs rw
3 2 0:3 / /run/user/1000 rw - tmpfs tmpfs rw
4 2 0:4 / /run/nvidia-persistenced/socket rw - tmpfs tmpfs rw
5 1 0:5 / /mnt/data\\040disk rw - ext4 /dev/y rw
6 1 0:6 / /dev/shm rw - tmpfs tmpfs rw
7 1 0:7 / /runner rw - ext4 /dev/z rw
";
        assert_eq!(
            mounts_to_prune(mi, &MountPlan::default()),
            ["/run/user/1000", "/mnt/data disk", "/dev/shm"]
        );
        assert!(path_under("/run/a", "/run/"));
        assert!(!path_under("/run", "/run/"));
        assert!(path_under("/dev/shm", "/dev/shm"));
        assert!(!path_under("/dev/shmx", "/dev/shm"));
    }
}
