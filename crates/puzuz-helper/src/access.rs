//! Checks that a restore gives the job's owner nothing they could not get
//! themselves.
//!
//! CRIU runs as root and re-opens every file of a restored job by its
//! dump-time path. After the dump the owner can change what such a path
//! leads to (e.g. replace a directory of theirs with a symlink to `/etc`),
//! and CRIU would hand the job whatever root finds there, opened with the
//! dump-time mode (`/etc/passwd` read-write). So:
//!
//! - **Before the restore** ([`PathCheck`]): every path the image makes
//!   CRIU open, create or link (see [`crate::images`]) must resolve, as the
//!   owner, without a symlink anywhere on the way; a path that does not
//!   exist yet only needs its existing part to. This refuses the attack
//!   early, but it is a check of paths that the owner can still change
//!   before CRIU opens them.
//! - **Before the restored job runs** ([`RestoredCheck`]): CRIU runs
//!   [`run_restore_hook`] as its `pre-resume` action script, while the
//!   restored tasks are complete but still stopped under its control
//!   (`--leave-stopped` cannot be used: CRIU's CUDA plugin then fails to
//!   find the tasks' restore threads). Every file object they hold (open
//!   descriptors, working and root directories, executable, file
//!   mappings) is pinned through `/proc`, and then, in the job's mount
//!   namespace with the owner's credentials (those of the restored tasks,
//!   which must be the owner's and groups the owner is still in):
//!   - the kernel must allow the owner the access the object is open for
//!     (`faccessat2(fd, "", mode, AT_EMPTY_PATH)`), and
//!   - unless the file is deleted, its path must lead the owner to the very
//!     same inode without symlinks (`openat2` with `RESOLVE_NO_SYMLINKS`).
//!
//!   Only then does CRIU resume the job; otherwise it kills it. This check looks
//!   at what CRIU actually opened, so a race with the owner changing paths
//!   cannot get past it. Objects on the namespace's own mounts (`/proc`,
//!   the private tmpfs) come from the image and are skipped.
//!
//! Both run in a child process (`puzuz-helper verify-access`, request on
//! stdin, result on stdout), which pins what it needs as root, enters the
//! job's mount namespace and then drops to the owner for good.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::path::Path;

use rustix::fs::{AtFlags, Mode, OFlags, ResolveFlags, StatxFlags};
use serde::{Deserialize, Serialize};

/// The access a restored object is open for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Want {
    /// `O_PATH`, or the executable: only that it exists and is reachable.
    Path,
    Read,
    Write,
    ReadWrite,
    /// A working or root directory.
    Search,
    /// A private file mapping: readable, or (a program) executable.
    ReadOrExec,
}

impl Want {
    fn describe(self) -> &'static str {
        match self {
            Want::Path => "reach",
            Want::Read => "read",
            Want::Write => "write",
            Want::ReadWrite => "read and write",
            Want::Search => "enter",
            Want::ReadOrExec => "read or execute",
        }
    }
}

/// Credentials to check as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

/// Before a restore: paths from the image (see [`crate::images`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathCheck {
    pub owner: Owner,
    pub paths: Vec<String>,
}

/// The check of the restored tasks before CRIU resumes them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoredCheck {
    /// Host PID of the restored namespace init.
    pub init_pid: u32,
    pub owner_uid: u32,
    /// Every group the owner is in now (primary group included).
    pub allowed_gids: Vec<u32>,
    /// Mount points (in the job's namespace) of tmpfs mounts the namespace
    /// has of its own; `/proc` is always one.
    pub private_tmpfs: Vec<String>,
    /// Files the shim opened as root before dropping privileges (device,
    /// inode).
    pub exempt: Vec<(u64, u64)>,
    /// Files the job held at the dump (see [`HeldFile`]).
    #[serde(default)]
    pub held: Vec<HeldFile>,
}

/// Before a restore: which of these paths do not exist (looked up as
/// root, without following a final symlink). In the child only so that a
/// filesystem that does not answer cannot hold up the helper beyond the
/// child's deadline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExistCheck {
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "check", rename_all = "snake_case")]
pub enum VerifyRequest {
    Paths(PathCheck),
    Restored(RestoredCheck),
    /// The reply lists the missing paths, unlimited.
    Exist(ExistCheck),
}

/// The problems found (empty: all is well), or why the check could not be
/// done.
pub type VerifyReply = Result<Vec<String>, String>;

/// At most this many problems are reported.
const MAX_PROBLEMS: usize = 20;

/// `puzuz-helper verify-access`: reads a [`VerifyRequest`] from stdin and
/// writes the [`VerifyReply`] to stdout. Single-threaded (`setns` of a mount
/// namespace requires it).
pub fn run_verify_access() -> i32 {
    let mut input = Vec::new();
    let reply: VerifyReply = io::stdin()
        .read_to_end(&mut input)
        .map_err(|e| format!("reading the request: {e}"))
        .and_then(|_| {
            serde_json::from_slice::<VerifyRequest>(&input)
                .map_err(|e| format!("decoding the request: {e}"))
        })
        .and_then(|req| match req {
            VerifyRequest::Paths(c) => check_paths(&c).map(truncate),
            VerifyRequest::Restored(c) => check_restored(&c).map(truncate),
            VerifyRequest::Exist(c) => Ok(c
                .paths
                .into_iter()
                .filter(|p| fs::symlink_metadata(p).is_err())
                .collect()),
        });
    match serde_json::to_vec(&reply) {
        Ok(out) => {
            let mut stdout = io::stdout();
            if stdout.write_all(&out).and_then(|()| stdout.flush()).is_ok() {
                0
            } else {
                1
            }
        }
        Err(_) => 1,
    }
}

fn truncate(mut problems: Vec<String>) -> Vec<String> {
    problems.truncate(MAX_PROBLEMS);
    problems
}

/// Runs `<exe> verify-access` with `req` on stdin and returns its reply.
pub fn run_verify_child(
    exe: &Path,
    req: &VerifyRequest,
    timeout: std::time::Duration,
) -> VerifyReply {
    use std::process::{Command, Stdio};
    let input = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .arg("verify-access")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("running {} verify-access: {e}", exe.display()))?;
    let res = child
        .stdin
        .take()
        .map(|mut s| s.write_all(&input))
        .unwrap_or(Ok(()));
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut out);
        }
        out
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "the access check did not finish within {timeout:?}"
                ));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(e) => break Err(format!("waiting for the access check: {e}")),
        }
    }?;
    let out = reader.join().unwrap_or_default();
    res.map_err(|e| format!("writing the access check request: {e}"))?;
    if !status.success() {
        return Err(format!("the access check failed ({status})"));
    }
    serde_json::from_slice::<VerifyReply>(&out)
        .map_err(|e| format!("reading the access check result: {e}"))?
}

/// The CRIU action script that runs [`RestoredCheck`]:
/// `puzuz-helper verify-restore-hook <request>`, passed to
/// `criu restore --action-script`.
///
/// CRIU runs it at every stage; at `pre-resume` the restored tasks are
/// complete (files, mappings, credentials; in plugin mode their CUDA state
/// too) but still stopped under CRIU's control. The hook reads the
/// [`RestoredCheck`] from `<request>` (written by the helper, root-only),
/// takes the restored init from `CRTOOLS_INIT_PID`, runs the check in a
/// `verify-access` child and writes the [`VerifyReply`] next to the
/// request ([`hook_result_path`]). Any problem fails the hook, and CRIU
/// then kills the restored tasks: they never run.
pub fn run_restore_hook(request: &Path) -> i32 {
    if std::env::var("CRTOOLS_SCRIPT_ACTION").as_deref() != Ok("pre-resume") {
        return 0;
    }
    let init_pid: Option<u32> = std::env::var("CRTOOLS_INIT_PID")
        .ok()
        .and_then(|p| p.parse().ok());
    let reply: VerifyReply = (|| {
        let mut check: RestoredCheck = serde_json::from_slice(
            &fs::read(request).map_err(|e| format!("reading {}: {e}", request.display()))?,
        )
        .map_err(|e| format!("decoding {}: {e}", request.display()))?;
        check.init_pid = init_pid.ok_or("CRIU did not set CRTOOLS_INIT_PID")?;
        let exe = std::env::current_exe().map_err(|e| format!("locating myself: {e}"))?;
        run_verify_child(
            &exe,
            &VerifyRequest::Restored(check),
            std::time::Duration::from_secs(120),
        )
    })();
    let ok = matches!(&reply, Ok(p) if p.is_empty());
    // CRIU only logs a failed pre-resume script and resumes the tasks
    // anyway, so kill them here, while they are still stopped under its
    // control: they never run. CRIU then fails to detach from them, but
    // hangs in its clean-up; it is killed too, so the restore unit fails
    // at once.
    if !ok && let Some(init) = init_pid {
        kill_tree(init);
        let parent = nix::unistd::getppid();
        if fs::read_to_string(format!("/proc/{parent}/comm")).is_ok_and(|c| c.trim() == "criu") {
            let _ = nix::sys::signal::kill(parent, nix::sys::signal::Signal::SIGKILL);
        }
    }
    if let Err(e) = serde_json::to_vec(&reply)
        .map_err(io::Error::other)
        .and_then(|b| fs::write(hook_result_path(request), b))
    {
        eprintln!("puzuz-helper: writing the restore check result: {e}");
        return 1;
    }
    match &reply {
        Ok(p) if !p.is_empty() => eprintln!("puzuz-helper: restore refused: {}", p.join("; ")),
        Err(e) => eprintln!("puzuz-helper: restore check failed: {e}"),
        Ok(_) => {}
    }
    if ok { 0 } else { 1 }
}

/// The request of [`run_dump_hook`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpHookRequest {
    /// Host PID of the dumped namespace init.
    pub init_pid: u32,
    /// Mount points of the namespace's own tmpfs mounts.
    pub private: Vec<String>,
}

/// The CRIU action script of a dump: `puzuz-helper dump-hook <request>`,
/// passed to `criu dump --action-script`.
///
/// At `post-dump` CRIU has dumped the job but not killed it yet: its tasks
/// are stopped under CRIU's control, holding exactly the files the image
/// has. The hook reads the [`DumpHookRequest`] from `<request>` and writes
/// the job's [`HeldFile`]s ([`crate::host::held_files`]) next to it
/// ([`hook_result_path`]). The list the helper made before the dump misses
/// files the job opened after it (e.g. a file it creates every few
/// moments). The hook never fails the dump (CRIU would resume the job):
/// without its result, the helper's earlier list is used.
pub fn run_dump_hook(request: &Path) -> i32 {
    if std::env::var("CRTOOLS_SCRIPT_ACTION").as_deref() != Ok("post-dump") {
        return 0;
    }
    let result = (|| -> Result<(), String> {
        let req: DumpHookRequest = serde_json::from_slice(
            &fs::read(request).map_err(|e| format!("reading {}: {e}", request.display()))?,
        )
        .map_err(|e| format!("decoding {}: {e}", request.display()))?;
        let proc_root = Path::new("/proc");
        let pids = crate::host::ns_processes(proc_root, req.init_pid)
            .map_err(|e| format!("listing the job's processes: {e}"))?;
        let held = crate::host::held_files(proc_root, &pids, &req.private);
        let out = hook_result_path(request);
        let tmp = out.with_extension("tmp");
        serde_json::to_vec(&held)
            .map_err(io::Error::other)
            .and_then(|b| fs::write(&tmp, b))
            .and_then(|()| fs::rename(&tmp, &out))
            .map_err(|e| format!("writing {}: {e}", out.display()))
    })();
    if let Err(e) = result {
        eprintln!("puzuz-helper: listing the files of the dumped job: {e}");
    }
    0
}

/// SIGKILL to every process in the PID namespace of `init`, and to `init`.
fn kill_tree(init: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let pids = crate::host::ns_processes(Path::new("/proc"), init).unwrap_or_default();
    for pid in pids.into_iter().chain([init]) {
        if let Ok(pid) = i32::try_from(pid) {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
    }
}

/// Where [`run_restore_hook`] writes its result for `request`.
pub fn hook_result_path(request: &Path) -> std::path::PathBuf {
    let mut name = request.as_os_str().to_owned();
    name.push(".result");
    name.into()
}

/// Become `owner` for good (only as root; an unprivileged caller, e.g. a
/// test, must already be the owner).
pub fn become_owner(owner: &Owner) -> Result<(), String> {
    use nix::unistd::{Gid, Uid};
    if !nix::unistd::geteuid().is_root() {
        if nix::unistd::geteuid().as_raw() != owner.uid {
            return Err(format!("cannot become uid {} without root", owner.uid));
        }
        return Ok(());
    }
    if owner.uid == 0 {
        return Err("refusing to check as root".into());
    }
    let groups: Vec<Gid> = owner.groups.iter().map(|g| Gid::from_raw(*g)).collect();
    let gid = Gid::from_raw(owner.gid);
    let uid = Uid::from_raw(owner.uid);
    (|| -> nix::Result<()> {
        nix::unistd::setgroups(&groups)?;
        nix::unistd::setresgid(gid, gid, gid)?;
        // Every uid non-zero: the capability sets are cleared.
        nix::unistd::setresuid(uid, uid, uid)
    })()
    .map_err(|e| format!("switching to uid {}: {e}", owner.uid))?;
    rustix::thread::set_no_new_privs(true).map_err(|e| format!("no_new_privs: {e}"))?;
    if nix::unistd::geteuid().is_root() || nix::unistd::getuid().is_root() {
        return Err("still root".into());
    }
    Ok(())
}

const RESOLVE: ResolveFlags = ResolveFlags::NO_SYMLINKS.union(ResolveFlags::NO_MAGICLINKS);

/// Opens `path` (`O_PATH`, the final component not followed) without
/// following any symlink or magic link.
fn open_no_symlinks(path: &str) -> rustix::io::Result<OwnedFd> {
    rustix::fs::openat2(
        rustix::fs::CWD,
        path,
        OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        RESOLVE,
    )
}

/// One path of a [`PathCheck`], as the current (owner's) credentials.
pub fn check_path_as_owner(path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("{path}: not an absolute path"));
    }
    match open_no_symlinks(path) {
        Ok(fd) => {
            let st = rustix::fs::fstat(&fd).map_err(|e| format!("{path}: {e}"))?;
            if st.st_mode & 0o170000 == 0o120000 {
                return Err(format!("{path} is a symlink"));
            }
            Ok(())
        }
        // Resolution stops at the first missing component, after every
        // existing one was checked: CRIU creates the rest.
        Err(rustix::io::Errno::NOENT) => Ok(()),
        // CRIU's open fails there too.
        Err(rustix::io::Errno::NOTDIR) => Ok(()),
        Err(rustix::io::Errno::LOOP) => Err(format!(
            "{path}: a directory on the way is a symlink (the job's files must be reached \
             through the same directories as when it was checkpointed)"
        )),
        Err(e) => Err(format!("{path}: the job's owner cannot reach it ({e})")),
    }
}

fn check_paths(c: &PathCheck) -> VerifyReply {
    become_owner(&c.owner)?;
    Ok(c.paths
        .iter()
        .filter_map(|p| check_path_as_owner(p).err())
        .collect())
}

/// A file object of a restored task, pinned by the root part of the check.
#[derive(Debug)]
pub struct Pinned {
    /// For messages, e.g. "fd 3 of process 1234".
    pub what: String,
    /// The path `/proc` shows for it (without " (deleted)").
    pub path: String,
    pub deleted: bool,
    pub want: Want,
    /// The job held this very file by this path at the dump (see
    /// [`HeldFile`]): only its path is checked, not the owner's
    /// permissions.
    pub held: bool,
    pub fd: OwnedFd,
}

/// A file the job held open (or mapped) at the dump: its path, device and
/// inode, and its attributes once the dump is done ([`complete_held_files`]).
///
/// A job may hold a file with an access its owner could not get by opening
/// it again, e.g. one it created read-only, or on a filesystem that shows
/// files as root's (sshfs). When CRIU reopened that very file (same path,
/// no symlinks), the restore gives the job nothing new. It is that very
/// file if
///
/// - its device and inode are those of the dump (the same mounted
///   filesystem, e.g. no reboot in between), or
/// - its owner, group, mode, size, modification and change times are those
///   of the dump, to the nanosecond. Device numbers change when a
///   filesystem is mounted again, and inode numbers are not stable on every
///   filesystem: FUSE filesystems such as sshfs number inodes in the order
///   they are looked up, so after a reboot another file may have the old
///   number. The change time cannot be set by users: renaming or linking
///   another file into the path, or changing it, gives it a new one (on
///   sshfs, which reports the modification time as the change time, the
///   server lets only the file's owner set it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldFile {
    pub path: String,
    pub dev: u64,
    pub ino: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attrs: Option<FileAttrs>,
}

/// See [`HeldFile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileAttrs {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub size: i64,
    pub mtime: (i64, i64),
    pub ctime: (i64, i64),
}

impl FileAttrs {
    pub fn of(st: &rustix::fs::Stat) -> Self {
        #[allow(clippy::unnecessary_cast)]
        Self {
            uid: st.st_uid,
            gid: st.st_gid,
            mode: st.st_mode as u32,
            size: st.st_size as i64,
            mtime: (st.st_mtime as i64, st.st_mtime_nsec as i64),
            ctime: (st.st_ctime as i64, st.st_ctime_nsec as i64),
        }
    }
}

/// After a successful dump (nothing of the job runs any more): records the
/// attributes of each held file that its path still leads to, without
/// symlinks, as the same device and inode as before the dump (a file the
/// job closed or replaced meanwhile is left without them). Files that have
/// their attributes already keep them (a dump finalized again later, after
/// a reboot, must not lose what was recorded right after the dump).
pub fn complete_held_files(files: &mut [HeldFile]) {
    for f in files.iter_mut().filter(|f| f.attrs.is_none()) {
        f.attrs = open_no_symlinks(&f.path)
            .and_then(|fd| rustix::fs::fstat(&fd))
            .ok()
            .filter(|st| (st.st_dev, st.st_ino) == (f.dev, f.ino))
            .map(|st| FileAttrs::of(&st));
    }
}

impl HeldFile {
    /// Whether `st`, found at this path, is this file (see [`HeldFile`]).
    pub fn is(&self, st: &rustix::fs::Stat) -> bool {
        (st.st_dev, st.st_ino) == (self.dev, self.ino)
            || self.attrs.is_some_and(|a| a == FileAttrs::of(st))
    }
}

/// The owner's part for one object (run with the owner's credentials, in
/// the job's mount namespace).
pub fn check_pinned(p: &Pinned) -> Result<(), String> {
    use nix::fcntl::AtFlags as NixAt;
    use nix::unistd::AccessFlags as A;
    let access = |mode: A| nix::unistd::faccessat(&p.fd, "", mode, NixAt::AT_EMPTY_PATH);
    let allowed = match p.want {
        Want::Path => access(A::F_OK),
        Want::Read => access(A::R_OK),
        Want::Write => access(A::W_OK),
        Want::ReadWrite => access(A::R_OK | A::W_OK),
        Want::Search => access(A::X_OK),
        Want::ReadOrExec => access(A::R_OK).or_else(|_| access(A::X_OK)),
    };
    if let Err(e) = allowed
        && !p.held
    {
        return Err(format!(
            "{} ({}): its owner may not {} it ({e})",
            p.what,
            p.path,
            p.want.describe()
        ));
    }
    if p.deleted {
        return Ok(());
    }
    let pinned = rustix::fs::fstat(&p.fd).map_err(|e| format!("{}: {e}", p.what))?;
    match open_no_symlinks(&p.path) {
        Ok(fd) => {
            let st = rustix::fs::fstat(&fd).map_err(|e| format!("{}: {e}", p.what))?;
            if (st.st_dev, st.st_ino) != (pinned.st_dev, pinned.st_ino) {
                return Err(format!(
                    "{} ({}): that path leads its owner to another file",
                    p.what, p.path
                ));
            }
            Ok(())
        }
        Err(rustix::io::Errno::LOOP) => Err(format!(
            "{} ({}): its owner reaches it only through a symlink",
            p.what, p.path
        )),
        Err(e) => Err(format!(
            "{} ({}): its owner cannot reach it ({e})",
            p.what, p.path
        )),
    }
}

/// Credentials of a process from `/proc/<pid>/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcCreds {
    pub uids: Vec<u32>,
    pub gids: Vec<u32>,
    pub groups: Vec<u32>,
    /// `CapPrm`, `CapEff`, `CapAmb` all zero.
    pub no_caps: bool,
}

pub fn parse_creds(status: &str) -> Option<ProcCreds> {
    let field = |name: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':').map(str::trim))
    };
    let nums =
        |s: &str| -> Option<Vec<u32>> { s.split_whitespace().map(|n| n.parse().ok()).collect() };
    let no_caps = ["CapPrm", "CapEff", "CapAmb"]
        .iter()
        .all(|c| field(c).is_some_and(|v| u64::from_str_radix(v, 16) == Ok(0)));
    Some(ProcCreds {
        uids: nums(field("Uid")?)?,
        gids: nums(field("Gid")?)?,
        groups: nums(field("Groups").unwrap_or(""))?,
        no_caps,
    })
}

/// Whether `/proc/<pid>/status` is that of a zombie (`Z`) or a dead task
/// (`X`): it has exited and cannot run again.
pub fn is_dead(status: &str) -> bool {
    status
        .lines()
        .find_map(|l| l.strip_prefix("State:"))
        .and_then(|s| s.trim_start().chars().next())
        .is_some_and(|c| c == 'Z' || c == 'X')
}

/// The access an open descriptor is for, from its `flags:` in fdinfo.
pub fn want_of_fd_flags(flags: u32, is_dir: bool) -> Want {
    const O_PATH: u32 = 0o10000000;
    if flags & O_PATH != 0 {
        return Want::Path;
    }
    match flags & 0o3 {
        0 if is_dir => Want::Read,
        0 => Want::Read,
        1 => Want::Write,
        _ => Want::ReadWrite,
    }
}

/// `VmFlags` of each mapping in `/proc/<pid>/smaps`, by start address.
pub fn parse_smaps_flags(smaps: &str) -> BTreeMap<u64, Vec<String>> {
    let mut out = BTreeMap::new();
    let mut start = None;
    for l in smaps.lines() {
        if let Some(range) = l.split_whitespace().next()
            && let Some((a, _)) = range.split_once('-')
            && let Ok(a) = u64::from_str_radix(a, 16)
            && !l.starts_with("VmFlags")
            && range.contains('-')
            && l.split_whitespace().count() >= 5
        {
            start = Some(a);
            continue;
        }
        if let Some(flags) = l.strip_prefix("VmFlags:")
            && let Some(a) = start
        {
            out.insert(a, flags.split_whitespace().map(str::to_owned).collect());
        }
    }
    out
}

/// The access a file mapping needs: CRIU opens a shared mapping that may
/// become writable (`mw`) read-write.
pub fn want_of_mapping(vmflags: &[String]) -> Want {
    let has = |f: &str| vmflags.iter().any(|v| v == f);
    if has("sh") && has("mw") {
        Want::ReadWrite
    } else {
        Want::ReadOrExec
    }
}

fn read_fd_flags(proc_root: &Path, pid: u32, fd: &str) -> Option<u32> {
    let info = fs::read_to_string(proc_root.join(format!("{pid}/fdinfo/{fd}"))).ok()?;
    let v = info.lines().find_map(|l| l.strip_prefix("flags:"))?.trim();
    u32::from_str_radix(v, 8).ok()
}

/// Opens `link` (a `/proc` link) `O_PATH`, following it to the object.
fn pin(link: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        link,
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

fn check_restored(c: &RestoredCheck) -> VerifyReply {
    let procs = crate::host::ns_processes(Path::new("/proc"), c.init_pid)
        .map_err(|e| format!("listing the restored processes: {e}"))?;
    check_restored_procs(c, &procs)
}

/// [`check_restored`] for the processes `procs` of the job.
fn check_restored_procs(c: &RestoredCheck, procs: &[u32]) -> VerifyReply {
    let proc_root = Path::new("/proc");
    raise_nofile_limit();
    // CRIU restores a job's zombies (e.g. a `sleep` its shell had not
    // reaped yet at the dump) as tasks that exit again during the restore:
    // at pre-resume they are zombies with the credentials CRIU forked them
    // with (root) and no namespaces. They hold no files and never run
    // again; the init reaps them.
    let procs: Vec<u32> = procs
        .iter()
        .copied()
        .filter(|pid| {
            let dead = pid != &c.init_pid
                && fs::read_to_string(proc_root.join(format!("{pid}/status")))
                    .is_ok_and(|s| is_dead(&s));
            if dead {
                eprintln!("puzuz-helper: process {pid} of the restored job is a zombie; skipped");
            }
            !dead
        })
        .collect();
    let procs = procs.as_slice();
    if procs.is_empty() {
        return Err("the restored job has no processes".into());
    }
    let mut problems = Vec::new();
    let ns_of = |pid: u32| fs::read_link(proc_root.join(format!("{pid}/ns/mnt")));
    let init_ns = ns_of(c.init_pid).map_err(|e| format!("reading the job's namespace: {e}"))?;
    // Credentials: the owner's, groups the owner is still in, no
    // capabilities, the same in every process.
    let mut owner: Option<Owner> = None;
    for &pid in procs {
        let status = fs::read_to_string(proc_root.join(format!("{pid}/status")))
            .map_err(|e| format!("reading process {pid}: {e}"))?;
        let creds = parse_creds(&status).ok_or(format!("process {pid}: unreadable status"))?;
        if creds.uids.iter().any(|u| *u != c.owner_uid) {
            problems.push(format!(
                "process {pid} runs as uids {:?}, not as the owner {}",
                creds.uids, c.owner_uid
            ));
        }
        let foreign: Vec<u32> = creds
            .gids
            .iter()
            .chain(&creds.groups)
            .copied()
            .filter(|g| !c.allowed_gids.contains(g))
            .collect();
        if !foreign.is_empty() {
            problems.push(format!(
                "process {pid} has groups {foreign:?} that its owner is no longer in"
            ));
        }
        if !creds.no_caps {
            problems.push(format!("process {pid} has capabilities"));
        }
        if ns_of(pid).ok().as_ref() != Some(&init_ns) {
            problems.push(format!(
                "process {pid} is in a mount namespace of its own (not supported)"
            ));
        }
        let mut groups = creds.groups.clone();
        groups.sort_unstable();
        let this = Owner {
            uid: c.owner_uid,
            gid: creds.gids.get(1).copied().unwrap_or(u32::MAX),
            groups,
        };
        match &owner {
            None => owner = Some(this),
            Some(o) if *o != this => {
                problems.push(format!(
                    "process {pid} has other credentials than the job's init"
                ));
            }
            Some(_) => {}
        }
    }
    if !problems.is_empty() {
        return Ok(problems);
    }
    let owner = owner.ok_or("no credentials")?;

    // The namespace's own mounts.
    let mountinfo = fs::read_to_string(proc_root.join(format!("{}/mountinfo", c.init_pid)))
        .map_err(|e| format!("reading the job's mounts: {e}"))?;
    let private: HashSet<u64> = crate::mounts::parse_mountinfo(&mountinfo)
        .into_iter()
        .filter(|m| {
            (m.fstype == "proc" && crate::mounts::path_under(&m.mount_point, "/proc"))
                || (m.fstype == "tmpfs" && c.private_tmpfs.contains(&m.mount_point))
        })
        .map(|m| m.id)
        .collect();

    let mut objects = Vec::new();
    let mut seen = HashSet::new();
    let mut add = |what: String, link: &Path, want: Want, problems: &mut Vec<String>| {
        let Ok(target) = fs::read_link(link) else {
            return; // closed or exited meanwhile: the tasks are stopped
        };
        let Some(target) = target.to_str().map(str::to_owned) else {
            problems.push(format!("{what}: a path that is not UTF-8"));
            return;
        };
        if !target.starts_with('/') {
            return; // pipe:, socket:, anon_inode:
        }
        let fd = match pin(link) {
            Ok(fd) => fd,
            // Without root, `map_files` cannot be opened (tests only).
            Err(_) if !nix::unistd::geteuid().is_root() => return,
            Err(e) => {
                problems.push(format!("{what} ({target}): {e}"));
                return;
            }
        };
        let Ok(st) = rustix::fs::fstat(&fd) else {
            return;
        };
        if c.exempt.contains(&(st.st_dev, st.st_ino)) {
            return;
        }
        let mnt = rustix::fs::statx(&fd, "", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
            .map(|s| s.stx_mnt_id)
            .unwrap_or(0);
        if private.contains(&mnt) {
            return;
        }
        let (path, deleted) = match target.strip_suffix(" (deleted)") {
            Some(p) => (p.to_owned(), true),
            None => (target.clone(), st.st_nlink == 0),
        };
        if !seen.insert((st.st_dev, st.st_ino, want, deleted, path.clone())) {
            return;
        }
        let held = !deleted && c.held.iter().any(|h| h.path == path && h.is(&st));
        objects.push(Pinned {
            what,
            path,
            deleted,
            want,
            held,
            fd,
        });
    };
    for &pid in procs {
        let dir = proc_root.join(pid.to_string());
        if let Ok(fds) = fs::read_dir(dir.join("fd")) {
            for e in fds.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let flags = read_fd_flags(proc_root, pid, &name).unwrap_or(0o2);
                let is_dir = fs::metadata(e.path()).is_ok_and(|m| m.is_dir());
                add(
                    format!("fd {name} of process {pid}"),
                    &e.path(),
                    want_of_fd_flags(flags, is_dir),
                    &mut problems,
                );
            }
        }
        add(
            format!("the working directory of process {pid}"),
            &dir.join("cwd"),
            Want::Search,
            &mut problems,
        );
        add(
            format!("the root directory of process {pid}"),
            &dir.join("root"),
            Want::Search,
            &mut problems,
        );
        add(
            format!("the executable of process {pid}"),
            &dir.join("exe"),
            Want::Path,
            &mut problems,
        );
        let vmflags = parse_smaps_flags(&fs::read_to_string(dir.join("smaps")).unwrap_or_default());
        if let Ok(maps) = fs::read_dir(dir.join("map_files")) {
            for e in maps.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                let start = name
                    .split_once('-')
                    .and_then(|(a, _)| u64::from_str_radix(a, 16).ok());
                let want = start
                    .and_then(|a| vmflags.get(&a))
                    .map_or(Want::ReadWrite, |f| want_of_mapping(f));
                add(
                    format!("the mapping {name} of process {pid}"),
                    &e.path(),
                    want,
                    &mut problems,
                );
            }
        }
    }
    if !problems.is_empty() {
        return Ok(problems);
    }

    // Into the job's mount namespace (its paths are the job's), then the
    // owner for good.
    let own_ns = fs::read_link("/proc/self/ns/mnt").ok();
    if own_ns.as_ref() != Some(&init_ns) {
        let ns = fs::File::open(proc_root.join(format!("{}/ns/mnt", c.init_pid)))
            .map_err(|e| format!("opening the job's mount namespace: {e}"))?;
        nix::sched::setns(&ns, nix::sched::CloneFlags::CLONE_NEWNS)
            .map_err(|e| format!("entering the job's mount namespace: {e}"))?;
    }
    become_owner(&owner)?;
    Ok(objects
        .iter()
        .filter_map(|p| check_pinned(p).err())
        .collect())
}

/// A job may have many files open; the check pins each one.
fn raise_nofile_limit() {
    use rustix::process::{Resource, getrlimit, setrlimit};
    let mut lim = getrlimit(Resource::Nofile);
    if lim.current != lim.maximum {
        lim.current = lim.maximum;
        let _ = setrlimit(Resource::Nofile, lim);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;

    /// Polls `cond` until it holds. The deadline only matters when it never
    /// does: a loaded machine may take a while to schedule a child.
    fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !cond() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn pinned(path: &Path, want: Want) -> Pinned {
        Pinned {
            what: "test".into(),
            path: path.to_str().unwrap().to_owned(),
            deleted: false,
            want,
            held: false,
            fd: pin(path).unwrap(),
        }
    }

    /// A job with a zombie (a child its parent has not reaped) when it is
    /// restored: the zombie has no namespaces left (and after a restore,
    /// the root credentials CRIU forked it with), but it holds nothing and
    /// never runs again, so it does not fail the check.
    #[test]
    fn zombies_are_not_checked() {
        assert!(is_dead("Name:\tsleep\nState:\tZ (zombie)\n"));
        assert!(is_dead("State:\tX (dead)\n"));
        assert!(!is_dead("State:\tS (sleeping)\n"));
        assert!(!is_dead("State:\tt (tracing stop)\n"));
        let me = nix::unistd::geteuid().as_raw();
        if me == 0 {
            eprintln!("skipped: needs an unprivileged user");
            return;
        }
        // The job's init: a process that is just running. `spawn` returns
        // once `sleep` has been exec'd. Its stdio and working directory are
        // set here, not inherited from the test runner (whose terminal may
        // be unreachable, e.g. a `/dev/pts/N` outside a Nix build sandbox).
        let tmp = tempfile::tempdir().unwrap();
        let spawn = |program: &str, args: &[&str]| {
            std::process::Command::new(program)
                .args(args)
                .current_dir(tmp.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        };
        let mut child = spawn("sleep", &["30"]);
        let parent = child.id();
        // The zombie: a child of the test that exits and that the test does
        // not reap until the end (`Child` only reaps in `wait`/`try_wait`).
        // Its parent does not matter to the check, which is given the
        // processes; a shell (`sh -c 'true & exec sleep 30'`) does not
        // reliably leave one, as it may reap `true` before the `exec`.
        let mut exited = spawn("true", &[]);
        let zombie = exited.id();
        wait_until("the child to become a zombie", || {
            fs::read_to_string(format!("/proc/{zombie}/status")).is_ok_and(|s| is_dead(&s))
        });
        // What made the check fail: a zombie has no mount namespace.
        assert!(fs::read_link(format!("/proc/{zombie}/ns/mnt")).is_err());
        let gid = nix::unistd::getegid().as_raw();
        let req = RestoredCheck {
            init_pid: parent,
            owner_uid: me,
            allowed_gids: nix::unistd::getgroups()
                .unwrap()
                .iter()
                .map(|g| g.as_raw())
                .chain([gid])
                .collect(),
            private_tmpfs: vec![],
            exempt: vec![],
            held: vec![],
        };
        assert_eq!(check_restored_procs(&req, &[parent, zombie]), Ok(vec![]));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(exited.wait().unwrap().success());
    }

    #[test]
    fn parses_proc_files() {
        let status = "Name:\tx\nUid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n\
                      Groups:\t27 1000 \nCapPrm:\t0000000000000000\nCapEff:\t0000000000000000\n\
                      CapAmb:\t0000000000000000\n";
        let c = parse_creds(status).unwrap();
        assert_eq!(c.uids, [1000; 4]);
        assert_eq!(c.groups, [27, 1000]);
        assert!(c.no_caps);
        let c =
            parse_creds(&status.replace("CapEff:\t0000000000000000", "CapEff:\t00000000000000c0"))
                .unwrap();
        assert!(!c.no_caps);
        assert_eq!(parse_creds("Uid:\t1\n"), None);

        assert_eq!(want_of_fd_flags(0o100002, false), Want::ReadWrite);
        assert_eq!(want_of_fd_flags(0o2001, false), Want::Write);
        assert_eq!(want_of_fd_flags(0o2, false), Want::ReadWrite);
        assert_eq!(want_of_fd_flags(0o10000000, false), Want::Path);
        assert_eq!(want_of_fd_flags(0o200000, true), Want::Read);

        let smaps = "\
7f0000000000-7f0000001000 rw-s 00000000 08:01 1234 /home/a/f
Size:                  4 kB
VmFlags: rd wr sh mr mw me ms
7f0000002000-7f0000003000 r-xp 00000000 08:01 99 /usr/lib/libc.so.6
Size:                  4 kB
VmFlags: rd ex mr mw me
";
        let f = parse_smaps_flags(smaps);
        assert_eq!(want_of_mapping(&f[&0x7f00_0000_0000]), Want::ReadWrite);
        assert_eq!(want_of_mapping(&f[&0x7f00_0000_2000]), Want::ReadOrExec);
    }

    #[test]
    fn paths_with_a_symlink_on_the_way_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("f"), b"x").unwrap();
        let link = tmp.path().join("x");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let p = |p: &Path| p.to_str().unwrap().to_owned();
        assert_eq!(check_path_as_owner(&p(&real.join("f"))), Ok(()));
        // Not there yet (CRIU creates it): fine, if the way there is.
        assert_eq!(check_path_as_owner(&p(&real.join("new/g"))), Ok(()));
        let e = check_path_as_owner(&p(&link.join("f"))).unwrap_err();
        assert!(e.contains("symlink"), "{e}");
        let e = check_path_as_owner(&p(&link.join("missing"))).unwrap_err();
        assert!(e.contains("symlink"), "{e}");
        // The final component itself.
        let e = check_path_as_owner(&p(&link)).unwrap_err();
        assert!(e.contains("symlink"), "{e}");
    }

    /// The post-restore check of one object, as an unprivileged owner: what
    /// CRIU (root) opened is refused when the owner could not open it the
    /// same way.
    #[test]
    fn restored_objects_must_be_the_owners_to_open() {
        let me = nix::unistd::geteuid();
        if me.is_root() {
            eprintln!("skipped: needs an unprivileged user");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("x");
        fs::create_dir(&dir).unwrap();
        let f = dir.join("passwd");
        fs::write(&f, b"x").unwrap();
        assert_eq!(check_pinned(&pinned(&f, Want::ReadWrite)), Ok(()));
        // A system file opened read-write (the exploit: CRIU followed a
        // symlink to /etc/passwd): the owner may not write it. A fixture
        // the owner may read but not write stands in for it (in a Nix build
        // sandbox, /etc/passwd is the build user's).
        let sys = tmp.path().join("etc-passwd");
        fs::write(&sys, b"root:x:0:0::/root:/bin/sh\n").unwrap();
        fs::set_permissions(&sys, fs::Permissions::from_mode(0o444)).unwrap();
        let e = check_pinned(&pinned(&sys, Want::ReadWrite)).unwrap_err();
        assert!(e.contains("may not read and write"), "{e}");
        assert_eq!(check_pinned(&pinned(&sys, Want::Read)), Ok(()));
        // The real one, where it is root's and not a symlink (in the
        // nixos/nix image it is one into the store, which the check refuses
        // for that reason).
        if fs::symlink_metadata("/etc/passwd").is_ok_and(|m| {
            m.is_file()
                && std::os::unix::fs::MetadataExt::uid(&m) == 0
                && m.permissions().mode() & 0o002 == 0
        }) {
            let etc = Path::new("/etc/passwd");
            let e = check_pinned(&pinned(etc, Want::ReadWrite)).unwrap_err();
            assert!(e.contains("may not read and write"), "{e}");
            assert_eq!(check_pinned(&pinned(etc, Want::Read)), Ok(()));
        }
        // Lost write permission since.
        fs::set_permissions(&f, fs::Permissions::from_mode(0o444)).unwrap();
        assert!(check_pinned(&pinned(&f, Want::Write)).is_err());
        assert_eq!(check_pinned(&pinned(&f, Want::Read)), Ok(()));
        // The path now leads elsewhere or through a symlink.
        let held = pinned(&f, Want::Read);
        fs::rename(&dir, tmp.path().join("y")).unwrap();
        fs::create_dir(&dir).unwrap();
        fs::write(&f, b"other").unwrap();
        let e = check_pinned(&held).unwrap_err();
        assert!(e.contains("another file"), "{e}");
        fs::remove_file(&f).unwrap();
        fs::remove_dir(&dir).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("y"), &dir).unwrap();
        let e = check_pinned(&held).unwrap_err();
        assert!(e.contains("symlink"), "{e}");
        // A deleted file is checked by its permissions only.
        let mut gone = pinned(&tmp.path().join("y/passwd"), Want::Read);
        fs::remove_file(tmp.path().join("y/passwd")).unwrap();
        gone.deleted = true;
        assert_eq!(check_pinned(&gone), Ok(()));
        // Directories: entering.
        assert_eq!(check_pinned(&pinned(tmp.path(), Want::Search)), Ok(()));
        // One the owner may not enter (like /root).
        let closed = tmp.path().join("closed");
        fs::create_dir(&closed).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(check_pinned(&pinned(&closed, Want::Search)).is_err());
        if fs::metadata("/root").is_ok_and(|m| {
            std::os::unix::fs::MetadataExt::uid(&m) == 0 && m.permissions().mode() & 0o001 == 0
        }) {
            assert!(check_pinned(&pinned(Path::new("/root"), Want::Search)).is_err());
        }
    }

    /// The whole post-restore check on a live process of our own (as the
    /// owner, without privileges): its files pass; one it holds through a
    /// path that has since become a symlink does not.
    #[test]
    fn checks_every_object_of_a_process_tree() {
        let me = nix::unistd::geteuid().as_raw();
        if me == 0 {
            eprintln!("skipped: needs an unprivileged user");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("x");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("data"), b"x").unwrap();
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exec 3<>\"$1\"; exec sleep 30")
            .arg("sh")
            .arg(dir.join("data"))
            // Not the test runner's stdio and working directory (see
            // `zombies_are_not_checked`).
            .current_dir(tmp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // By its name: `exe` may be a multicall binary (`coreutils` in Nix).
        wait_until("the shell to exec `sleep` with fd 3 open", || {
            fs::read_link(format!("/proc/{}/fd/3", child.id())).is_ok()
                && fs::read_to_string(format!("/proc/{}/comm", child.id()))
                    .is_ok_and(|c| c.trim_end() == "sleep")
        });
        let gid = nix::unistd::getegid().as_raw();
        let groups: Vec<u32> = nix::unistd::getgroups()
            .unwrap()
            .iter()
            .map(|g| g.as_raw())
            .chain([gid])
            .collect();
        let req = RestoredCheck {
            init_pid: child.id(),
            owner_uid: me,
            allowed_gids: groups,
            private_tmpfs: vec![],
            exempt: vec![],
            held: vec![],
        };
        // `ns_processes` lists the PID namespace of the init: in a test that
        // is the host's, so restrict it to the child.
        let check = |req: &RestoredCheck| check_restored_procs(req, &[req.init_pid]);
        assert_eq!(check(&req), Ok(vec![]));
        // The directory is swapped for a symlink to where it went.
        fs::rename(&dir, tmp.path().join("y")).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("y"), &dir).unwrap();
        // /proc shows the new canonical path, which is fine...
        assert_eq!(check(&req), Ok(vec![]));
        // ...but a group the owner left is not.
        // The file held read-write is no longer writable for its owner:
        // refused, unless it is the very file the job held at the dump.
        let data = tmp.path().join("y/data");
        fs::set_permissions(&data, fs::Permissions::from_mode(0o444)).unwrap();
        let p = check(&req).unwrap();
        assert!(
            p.iter().any(|p| p.contains("may not read and write")),
            "{p:?}"
        );
        let st = fs::metadata(&data).unwrap();
        let mut held = req.clone();
        held.held = vec![HeldFile {
            path: data.to_str().unwrap().to_owned(),
            dev: std::os::unix::fs::MetadataExt::dev(&st),
            ino: std::os::unix::fs::MetadataExt::ino(&st),
            attrs: None,
        }];
        assert_eq!(check(&held), Ok(vec![]));
        // After a remount the device and inode numbers may differ (sshfs
        // numbers inodes in lookup order): the attributes recorded after
        // the dump identify the file then.
        complete_held_files(&mut held.held);
        assert!(held.held[0].attrs.is_some());
        held.held[0].dev += 1;
        held.held[0].ino += 1000;
        assert_eq!(check(&held), Ok(vec![]));
        // Another file at that path is not that file, even with the same
        // owner, mode, size and times (renaming it there changed its
        // change time).
        let other = tmp.path().join("y/other");
        fs::write(&other, vec![b'x'; usize::try_from(st.len()).unwrap()]).unwrap();
        let mtime = fs::metadata(&data).unwrap().modified().unwrap();
        fs::File::options()
            .write(true)
            .open(&other)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(mtime).set_accessed(mtime))
            .unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(fs::metadata(&other).unwrap().modified().unwrap(), mtime);
        fs::rename(&data, tmp.path().join("y/data.old")).unwrap();
        fs::rename(&other, &data).unwrap();
        assert!(!check(&held).unwrap().is_empty());
        // Nor is it with the device and inode of the dump unrecorded.
        held.held[0].attrs = None;
        assert!(!check(&held).unwrap().is_empty());
        let mut req2 = req.clone();
        req2.allowed_gids = vec![];
        let p = check(&req2).unwrap();
        assert!(p.iter().any(|p| p.contains("no longer in")), "{p:?}");
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
