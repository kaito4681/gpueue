//! The main process of a job unit: `gpueue-helper shim <job dir>`.
//!
//! It starts as root (the unit has no `User=`) and is single-threaded (no async
//! runtime), which `unshare(CLONE_NEWNS)` requires.
//!
//! 1. Outer shim (the unit's main process, stays outside the dumped tree):
//!    triggers the host's automount points, unshares a PID and a mount
//!    namespace, starts
//!    `gpueue-helper shim-init <job dir>` — PID 1 of the new namespace —
//!    forwards the signals of [`FORWARDED`] (SIGTERM, SIGINT, SIGTSTP,
//!    SIGALRM, ...) to it, waits for it and writes `init-status`.
//! 2. Init (`shim-init`), still root:
//!    - becomes a session leader inside the namespace (CRIU refuses a
//!      session leader outside it),
//!    - opens `start.json` and `exit-status` (root-only, so the owner cannot
//!      forge them),
//!    - looks up the owner's groups (NSS loads libraries by path, so only
//!      while the namespace still has the host's mounts),
//!    - cleans the mount namespace so that it only holds mounts that also
//!      exist after a reboot: makes everything private, detaches the
//!      inherited `/proc` tree and mounts a fresh `/proc` (CUDA fails with
//!      error 304 without it), removes mounts
//!      hidden under later mounts (see [`crate::mounts`]), detaches volatile
//!      mounts (`/run/*`, `/media/*`, ...), replaces each automounted
//!      filesystem stacked on its `autofs` mount with a plain copy, and
//!      mounts private tmpfs on `/tmp` and `/dev/shm` (CRIU saves their
//!      contents in the image),
//!    - drops privileges: supplementary groups → gid → bounding set → uid →
//!      `no_new_privs`,
//!    - opens the log and enters the cwd with the owner's permissions only,
//!      starts the job in its own process group, forwards the same
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

use gpueue_protocol::helper::ExitStatus;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::CloneFlags;
use nix::sys::signal::{SigSet, SigmaskHow, Signal, kill, killpg, sigprocmask};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Gid, Pid, Uid};

use crate::jobs::{JobFiles, MountPlan, ShimSpec, StartFailure, StartReport, write_status_to};
use crate::mountfd;
use crate::mounts::{MountTree, triggered_automounts, untriggered_automounts};
pub use crate::mounts::{path_under, unescape_octal};

/// Exit code of the shim when it could not start the job.
pub const SHIM_FAILURE: i32 = 125;

/// Signals the outer shim and the namespace init forward towards the job.
///
/// Every signal the helper accepts for a job is either here or sent by
/// systemd to every process of the unit (STOP, CONT, KILL; see
/// [`crate::launcher::systemd::signal_target`]); the helper refuses the
/// rest ([`is_forwardable`]). A signal missing here would hit the root
/// outer shim itself (TSTP would stop it, ALRM end it and with it the
/// job's unit), and a namespace init without a handler drops it.
/// Synchronous fault signals (SEGV, BUS, ...) and the real-time signals
/// are not forwarded.
pub const FORWARDED: [Signal; 19] = [
    Signal::SIGHUP,
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGABRT,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
    Signal::SIGPIPE,
    Signal::SIGALRM,
    Signal::SIGTERM,
    Signal::SIGTSTP,
    Signal::SIGTTIN,
    Signal::SIGTTOU,
    Signal::SIGURG,
    Signal::SIGXCPU,
    Signal::SIGXFSZ,
    Signal::SIGVTALRM,
    Signal::SIGPROF,
    Signal::SIGWINCH,
    Signal::SIGPWR,
];

/// Whether the shim forwards `sig` to the job (see [`FORWARDED`]).
pub fn is_forwardable(sig: i32) -> bool {
    FORWARDED.iter().any(|s| *s as i32 == sig)
}

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
    eprintln!("gpueue shim: {}", f.message);
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
        eprintln!("gpueue shim: writing {}: {e}", files.start().display());
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

/// `gpueue-helper shim <job dir>`. Returns the process exit code.
pub fn run_outer(job_dir: PathBuf) -> i32 {
    let files = JobFiles::from_dir(job_dir);
    match outer(&files) {
        Ok(code) => code,
        Err(f) => fail(&files, None, f),
    }
}

fn outer(files: &JobFiles) -> Result<i32, Failure> {
    let sfd = signal_fd().map_err(|e| Failure::other(format!("signalfd: {e}")))?;
    // In the host's namespace, so that the job's starts with the
    // automounted filesystems. A spec that cannot be read is reported by
    // the init.
    if let Ok(spec) = std::fs::read(files.spec())
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice::<ShimSpec>(&b).map_err(|e| e.to_string()))
    {
        trigger_automounts(&spec.mounts).map_err(Failure::other)?;
    }
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

/// `gpueue-helper shim-init <job dir>`. Returns the process exit code.
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

    // NSS (which loads libraries) only before the mounts change: the
    // namespace is still the host's here.
    let groups = resolve_groups(&spec)
        .map_err(|e| Failure::other(format!("looking up the groups of uid {}: {e}", spec.uid)))?;
    setup_mounts(&spec.mounts).map_err(Failure::other)?;
    drop_privileges(&spec, &groups)
        .map_err(|e| Failure::other(format!("dropping privileges: {e}")))?;

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
        eprintln!("gpueue shim: writing the start report: {e}");
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
                            eprintln!("gpueue shim: writing exit status: {e}");
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

/// Mount points from `/proc/self/mountinfo` matching `plan`, in mountinfo
/// order (parents first).
pub fn mounts_to_prune(mountinfo: &str, plan: &MountPlan) -> Vec<String> {
    mountinfo
        .lines()
        .filter_map(|l| l.split_whitespace().nth(4))
        .map(unescape_octal)
        .filter(|m| plan.prunes(m))
        .collect()
}

fn read_mount_tree() -> Result<MountTree, String> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|mi| MountTree::parse(&mi))
        .map_err(|e| format!("reading mountinfo: {e}"))
}

/// Remove every mount that is hidden under a later mount (see
/// [`crate::mounts`]). The job cannot reach such a mount, but CRIU dumps it
/// as an external bind of a path that does not lead to it, and the restore
/// fails. It cannot be unmounted by path while it is covered, so the
/// covering mount `C` is set aside first:
///
/// 1. `open_tree(C, OPEN_TREE_CLONE | AT_RECURSIVE)`: a detached copy of
///    `C` with its submounts (same superblocks, so the same contents),
/// 2. detach `C`, which makes the mounts it hid reachable,
/// 3. unmount those,
/// 4. `move_mount` the copy back onto `C`'s mount point.
///
/// Everything happens in the job's private namespace before the job runs,
/// so nothing else observes the gap. Loops because a mount can be hidden
/// under several layers. Every step works on descriptors (see
/// [`mountfd`]): `C`'s mount point is reached once, through root-owned
/// directories only, and each mount is checked by its id before it is
/// detached. A covering mount below a directory that someone else can
/// change is left alone (the job then cannot be checkpointed).
fn remove_hidden_mounts() -> Result<(), String> {
    use rustix::mount::{MoveMountFlags, OpenTreeFlags, move_mount, open_tree};

    let not_restorable = |what: &str, e: String| {
        eprintln!("gpueue shim: cannot remove {what}: {e}; the job will not be restorable");
    };
    for _ in 0..16 {
        let tree = read_mount_tree()?;
        let hidden = tree.hidden();
        let Some(first) = hidden.first() else {
            return Ok(());
        };
        let cover = first.covered_by_path.clone();
        if cover == "/" {
            eprintln!(
                "gpueue shim: cannot remove {} (hidden under the root mount); \
                 the job will not be restorable",
                first.mount_point
            );
            return Ok(());
        }
        let ids: Vec<u64> = hidden
            .iter()
            .filter(|h| h.covered_by == first.covered_by)
            .map(|h| h.id)
            .collect();
        let what = format!("{} (hidden under {cover})", first.mount_point);
        let (parent, name) = match mountfd::open_parent(&cover) {
            Ok(v) => v,
            Err(e) => {
                not_restorable(&what, e);
                return Ok(());
            }
        };
        let cover_root = match mountfd::open_at(&parent, &name).and_then(|fd| {
            let id = mountfd::mnt_id(&fd)?;
            if id == first.covered_by {
                Ok(fd)
            } else {
                Err(format!(
                    "{cover} leads to mount {id}, not {}",
                    first.covered_by
                ))
            }
        }) {
            Ok(fd) => fd,
            Err(e) => {
                not_restorable(&what, e);
                return Ok(());
            }
        };
        let copy = match open_tree(
            &cover_root,
            "",
            OpenTreeFlags::OPEN_TREE_CLONE
                | OpenTreeFlags::AT_RECURSIVE
                | OpenTreeFlags::AT_EMPTY_PATH
                | OpenTreeFlags::OPEN_TREE_CLOEXEC,
        ) {
            Ok(fd) => fd,
            Err(e) => {
                // Old kernel (< 5.2): keep going, the dump will refuse.
                not_restorable(&what, format!("copying {cover}: {e}"));
                return Ok(());
            }
        };
        mountfd::detach(&cover_root)
            .map_err(|e| format!("detaching {cover} to uncover hidden mounts: {e}"))?;
        drop(cover_root);
        // Unmount what is reachable now; anything still covered (several
        // layers) is handled in the next round. Each by descriptor, after
        // checking that the path leads to that very mount.
        let res = read_mount_tree().map(|now| {
            for id in ids {
                if let Some(m) = now.get(id)
                    && now.resolve(&m.mount_point).last() == Some(&id)
                    && let Ok(fd) = mountfd::open_nofollow(&m.mount_point)
                    && mountfd::mnt_id(&fd) == Ok(id)
                {
                    let _ = mountfd::detach(&fd);
                }
            }
        });
        // `name` in the pinned parent: now the directory underneath.
        let target =
            mountfd::open_at(&parent, &name).map_err(|e| format!("re-attaching {cover}: {e}"))?;
        move_mount(
            &copy,
            "",
            &target,
            "",
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
        )
        .map_err(|e| format!("re-attaching {cover}: {e}"))?;
        res?;
    }
    let left = read_mount_tree()?.hidden();
    if !left.is_empty() {
        eprintln!("gpueue shim: hidden mounts left: {left:?}; the job will not be restorable");
    }
    Ok(())
}

/// How long the shim waits for the automounter to mount the automount
/// points (an NFS server that does not answer).
const AUTOMOUNT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Look up every automount point that nothing is mounted on yet, so that
/// the automounter mounts it. Runs in the host's namespace before the
/// job's is created. The lookups run in child processes
/// (`gpueue-helper shim-automount <path>`), all at once: threads would
/// prevent `unshare(CLONE_NEWNS)`, and a lookup can hang. Those still
/// running after [`AUTOMOUNT_TIMEOUT`] are killed, and the job's copy of
/// that automount point stays unusable. Automount points that `plan` prunes anyway
/// are skipped.
fn trigger_automounts(plan: &MountPlan) -> Result<(), String> {
    let paths: Vec<String> = untriggered_automounts(&read_mount_tree()?)
        .into_iter()
        .filter(|p| !plan.prunes(p))
        .collect();
    if paths.is_empty() {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| format!("locating the helper binary: {e}"))?;
    let mut children = Vec::new();
    for path in paths {
        match Command::new(&exe)
            .arg("shim-automount")
            .arg(&path)
            .stdin(Stdio::null())
            .spawn()
        {
            Ok(child) => children.push((path, child)),
            Err(e) => eprintln!("gpueue shim: automount {path}: {e}"),
        }
    }
    let deadline = std::time::Instant::now() + AUTOMOUNT_TIMEOUT;
    while !children.is_empty() {
        children.retain_mut(|(path, child)| match child.try_wait() {
            Ok(None) if std::time::Instant::now() >= deadline => {
                eprintln!("gpueue shim: automount {path}: timed out");
                let _ = child.kill();
                let _ = child.wait();
                false
            }
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => false,
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(())
}

/// `gpueue-helper shim-automount <path>`: see [`trigger_automounts`].
pub fn run_automount(path: PathBuf) -> i32 {
    match std::fs::metadata(path.join(".")) {
        Ok(_) => 0,
        Err(e) => {
            eprintln!("gpueue shim: automount {}: {e}", path.display());
            1
        }
    }
}

/// Replace each filesystem mounted on an automount point with a copy of
/// it directly on the mount point (see [`triggered_automounts`]): copy the
/// top mount with its submounts, detach the stack, attach the copy.
///
/// Like [`remove_hidden_mounts`], this works on descriptors only (see
/// [`mountfd`]) and skips automount points below a directory that someone
/// other than root can change.
fn flatten_automounts() -> Result<(), String> {
    use rustix::mount::{MoveMountFlags, OpenTreeFlags, move_mount, open_tree};

    for path in triggered_automounts(&read_mount_tree()?) {
        let not_restorable = |e: String| {
            eprintln!(
                "gpueue shim: cannot flatten the automounted {path}: {e}; \
                 the job will not be restorable"
            );
        };
        let (parent, name) = match mountfd::open_parent(&path) {
            Ok(v) => v,
            Err(e) => {
                not_restorable(e);
                continue;
            }
        };
        let copy = match mountfd::open_at(&parent, &name).and_then(|top| {
            open_tree(
                &top,
                "",
                OpenTreeFlags::OPEN_TREE_CLONE
                    | OpenTreeFlags::AT_RECURSIVE
                    | OpenTreeFlags::AT_EMPTY_PATH
                    | OpenTreeFlags::OPEN_TREE_CLOEXEC,
            )
            .map_err(|e| format!("copying: {e}"))
        }) {
            Ok(fd) => fd,
            Err(e) => {
                not_restorable(e);
                continue;
            }
        };
        // Detach the layers on `path` (filesystem, then autofs), each one
        // checked against mountinfo by its id.
        for _ in 0..8 {
            let tree = read_mount_tree()?;
            let Some(top) = tree.top(&path).filter(|t| t.mount_point == path) else {
                break;
            };
            let fd = mountfd::open_at(&parent, &name)
                .map_err(|e| format!("detaching the automount {path}: {e}"))?;
            if mountfd::mnt_id(&fd)? != top.id {
                return Err(format!("{path} changed while detaching its automount"));
            }
            mountfd::detach(&fd).map_err(|e| format!("detaching the automount {path}: {e}"))?;
        }
        let target =
            mountfd::open_at(&parent, &name).map_err(|e| format!("re-attaching {path}: {e}"))?;
        move_mount(
            &copy,
            "",
            &target,
            "",
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH | MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
        )
        .map_err(|e| format!("re-attaching {path}: {e}"))?;
    }
    Ok(())
}

/// Detach the mounts that `plan` prunes (volatile ones such as `/run/*`
/// and `/media/*`), deepest first.
///
/// Like [`remove_hidden_mounts`], by descriptor only (see [`mountfd`]):
/// mount points such as `/run/user/<uid>/doc` lie below directories that
/// their owner can change, and a path resolved again by `umount2` could be
/// redirected (`UMOUNT_NOFOLLOW` only covers the last component). Each
/// mount is reached once through directories only root controls and
/// detached only if it is the mount mountinfo listed. One below a
/// directory that someone else controls is left alone, but it goes with
/// its parent mount when that is pruned too (detaching is recursive).
fn prune_mounts(plan: &MountPlan) -> Result<(), String> {
    let targets: Vec<(u64, String)> = read_mount_tree()?
        .entries()
        .iter()
        .filter(|m| plan.prunes(&m.mount_point))
        .map(|m| (m.id, m.mount_point.clone()))
        .collect();
    if targets.is_empty() {
        return Ok(());
    }
    // Mountinfo lists parents (and the lower of stacked mounts) first. A
    // mount already gone with its parent no longer resolves to its id.
    for (id, path) in targets.iter().rev() {
        let Ok((parent, name)) = mountfd::open_parent(path) else {
            continue;
        };
        if let Ok(fd) = mountfd::open_at(&parent, &name)
            && mountfd::mnt_id(&fd) == Ok(*id)
            && let Err(e) = mountfd::detach(&fd)
        {
            eprintln!("gpueue shim: detaching {path}: {e}");
        }
    }
    let now = read_mount_tree()?;
    let left: Vec<&str> = targets
        .iter()
        .filter(|(id, _)| now.get(*id).is_some())
        .map(|(_, p)| p.as_str())
        // Those the private tmpfs covers are removed once it is mounted
        // (see `setup_mounts`).
        .filter(|p| !plan.tmpfs.iter().any(|t| crate::mounts::path_under(p, t)))
        .collect();
    if !left.is_empty() {
        eprintln!(
            "gpueue shim: left the volatile mounts {left:?} in place (below directories that \
             someone other than root can change); the job may not be restorable after a reboot"
        );
    }
    Ok(())
}

fn setup_mounts(plan: &MountPlan) -> Result<(), String> {
    // From here on the namespace is fixed: later host mounts do not show
    // up, so the job's view is the one that is dumped.
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
    remove_hidden_mounts()?;
    prune_mounts(plan)?;
    flatten_automounts()?;
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
    // Mounts that the pruning had to leave in place, because they lie
    // below a directory that someone other than root can change (alice's
    // FUSE mount in `/tmp/alice-dir/` when `/tmp` is a plain directory),
    // are hidden under the private tmpfs now: set it aside and detach them
    // by descriptor (see `remove_hidden_mounts`), or the job could not be
    // restored.
    remove_hidden_mounts()?;
    hide_dirs(&plan.hidden_dirs)?;
    if let Some(keep) = &plan.systemd_run_keep {
        mask_systemd_run(keep)?;
    }
    Ok(())
}

/// An empty, root-owned tmpfs on `path` (`mode=0755`), so that nothing
/// below the host's directory is reachable in the job, also what is
/// created there later.
fn mount_empty_tmpfs(path: &Path) -> Result<(), String> {
    mount(
        Some("tmpfs"),
        path,
        Some("tmpfs"),
        MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_NOEXEC,
        Some("mode=0755,size=1m"),
    )
    .map_err(|e| format!("hiding {}: {e}", path.display()))
}

/// Hide each directory of `dirs` (see [`MountPlan::hidden_dirs`]) behind
/// an empty tmpfs, creating it first if only it is missing. Two names of
/// one directory (`/var/run/dbus` when `/var/run` links to `/run`) are
/// hidden once; a directory whose parent does not exist is skipped (there
/// is no path to it). Each is reached through directories only root can
/// change, without following symlinks, and mounted on by descriptor (see
/// [`mountfd::ensure_root_dir`]).
fn hide_dirs(dirs: &[String]) -> Result<(), String> {
    let mut hidden: Vec<String> = Vec::new();
    for dir in dirs {
        let Some((fd, real)) =
            mountfd::ensure_root_dir(dir).map_err(|e| format!("hiding {dir}: {e}"))?
        else {
            continue;
        };
        if hidden.contains(&real) {
            continue;
        }
        mount_empty_tmpfs(Path::new(&mountfd::fd_path(&fd)))
            .map_err(|e| format!("{e} ({real})"))?;
        hidden.push(real);
    }
    Ok(())
}

/// Hide `/run/systemd` behind an empty, root-owned tmpfs holding only the
/// host's entries named in `keep` (relative paths of files or
/// directories, bound from the host where they exist) and an empty
/// `system` directory, so that nothing in the job reaches systemd's
/// Varlink sockets (see [`MountPlan::systemd_run_keep`]). CRIU saves the
/// tmpfs (its directories and mount-point files) in the image and binds
/// the kept entries from the host again on restore.
fn mask_systemd_run(keep: &[String]) -> Result<(), String> {
    use rustix::fs::CWD;
    use rustix::mount::{MoveMountFlags, OpenTreeFlags, move_mount, open_tree};
    const DIR: &str = "/run/systemd";
    if !std::fs::symlink_metadata(DIR).is_ok_and(|m| m.is_dir()) {
        return Ok(());
    }
    // Copies of what stays, taken before the tmpfs covers them.
    let mut kept = Vec::new();
    for name in keep {
        let rel = Path::new(name);
        if name.is_empty()
            || rel.is_absolute()
            || rel
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(format!("{DIR}: invalid entry to keep {name:?}"));
        }
        let path = format!("{DIR}/{name}");
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let copy = open_tree(
            CWD,
            path.as_str(),
            OpenTreeFlags::OPEN_TREE_CLONE
                | OpenTreeFlags::AT_RECURSIVE
                | OpenTreeFlags::AT_SYMLINK_NOFOLLOW
                | OpenTreeFlags::OPEN_TREE_CLOEXEC,
        )
        .map_err(|e| format!("copying {path}: {e}"))?;
        kept.push((path, meta.is_dir(), copy));
    }
    mount_empty_tmpfs(Path::new(DIR))?;
    // `sd_booted()` checks for this directory.
    std::fs::create_dir(format!("{DIR}/system")).map_err(|e| format!("{DIR}/system: {e}"))?;
    for (path, is_dir, copy) in kept {
        if let Some(parent) = Path::new(&path).parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        if is_dir {
            std::fs::create_dir(&path)
        } else {
            File::create(&path).map(drop)
        }
        .map_err(|e| format!("{path}: {e}"))?;
        move_mount(
            &copy,
            "",
            CWD,
            path.as_str(),
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        )
        .map_err(|e| format!("binding {path}: {e}"))?;
    }
    Ok(())
}

/// The job's supplementary groups: the spec's, or (when it has none) what
/// `initgroups` would set for the owner and the job's gid. Must run before
/// [`setup_mounts`]: NSS loads libraries by path, and the job's mount
/// namespace is still the host's then.
fn resolve_groups(spec: &ShimSpec) -> io::Result<Vec<Gid>> {
    if let Some(groups) = &spec.groups {
        return Ok(groups.iter().map(|g| Gid::from_raw(*g)).collect());
    }
    let user = nix::unistd::User::from_uid(Uid::from_raw(spec.uid))?
        .ok_or_else(|| io::Error::other(format!("uid {} has no user entry", spec.uid)))?;
    let name = std::ffi::CString::new(user.name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(nix::unistd::getgrouplist(&name, Gid::from_raw(spec.gid))?)
}

/// Switch to the owner, with `groups` (see [`resolve_groups`]). No NSS
/// lookups here: the mounts have changed already.
fn drop_privileges(spec: &ShimSpec, groups: &[Gid]) -> io::Result<()> {
    use rustix::thread::{
        CapabilitySet, clear_ambient_capability_set, remove_capability_from_bounding_set,
        set_no_new_privs,
    };
    let gid = Gid::from_raw(spec.gid);
    nix::unistd::setgroups(groups)?;
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

/// `gpueue-helper criu-wrap <root> <criu argv...>`: the `ExecStart` of a
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
        Ok(e) => eprintln!("gpueue criu-wrap: exec: {e}"),
        Err(e) => eprintln!("gpueue criu-wrap: {e}"),
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
5 1 0:5 / /media/usb\\040disk rw - ext4 /dev/y rw
8 1 0:8 / /mnt/data rw - ext4 /dev/w rw
6 1 0:6 / /dev/shm rw - tmpfs tmpfs rw
7 1 0:7 / /runner rw - ext4 /dev/z rw
";
        assert_eq!(
            mounts_to_prune(mi, &MountPlan::default()),
            ["/run/user/1000", "/media/usb disk", "/dev/shm"]
        );
        assert!(path_under("/run/a", "/run/"));
        assert!(!path_under("/run", "/run/"));
        assert!(path_under("/dev/shm", "/dev/shm"));
        assert!(!path_under("/dev/shmx", "/dev/shm"));
    }

    /// What the private tmpfs on /tmp covers: the host's /tmp (a tmpfs on
    /// Ubuntu 26.04) and an AppImage's FUSE mount on it, which refuses root.
    #[test]
    fn prunes_what_the_private_tmpfs_covers() {
        let mi = "\
1 0 0:1 / / rw - ext4 /dev/x rw
2 1 0:2 / /tmp rw - tmpfs tmpfs rw
3 2 0:68 / /tmp/.mount_orca-liellLH ro - fuse.orca-linux.AppImage orca-linux.AppImage ro,user_id=1000
4 1 0:4 / /tmpdata rw - ext4 /dev/y rw
";
        assert_eq!(
            mounts_to_prune(mi, &MountPlan::default()),
            ["/tmp", "/tmp/.mount_orca-liellLH"]
        );
    }
}
