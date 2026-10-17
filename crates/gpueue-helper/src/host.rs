//! Everything the helper reads from the host besides systemd: processes,
//! GPUs, users, filesystems. Behind [`HostProbe`] so tests can fake it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use gpueue_checkpoint::command::{CudaCheckpointOptions, cuda_get_restore_tid, cuda_get_state};
use gpueue_checkpoint::exec::run_with_timeout;
use gpueue_checkpoint::{CommandSpec, probe, procfs, space};

use crate::access::{FileAttrs, VerifyReply, VerifyRequest};
use crate::deadmounts::{DeadMounts, DetachedMount, Placeholders};
use crate::mounts::{MountTree, SourceMount};

/// One process of a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcInfo {
    pub host_pid: u32,
    /// PID inside the job's (innermost) namespace.
    pub ns_pid: u32,
    pub rss_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuState {
    pub index: u32,
    pub uuid: String,
    pub model: String,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeApp {
    pub pid: u32,
    pub gpu_uuid: String,
    pub used_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GpuSnapshot {
    pub driver_version: Option<String>,
    pub gpus: Vec<GpuState>,
    pub compute_apps: Vec<ComputeApp>,
}

/// A user as seen through NSS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub gid: u32,
    /// Every group the user is in, including the primary one.
    pub groups: Vec<u32>,
}

pub trait HostProbe: Send + Sync + 'static {
    /// Processes in the PID namespace whose init has host PID `init_pid`.
    fn job_processes(&self, init_pid: u32) -> io::Result<Vec<ProcInfo>>;
    /// GPUs and the processes using them.
    fn gpu_snapshot(&self) -> io::Result<GpuSnapshot>;
    /// Minor numbers of the GPUs' device nodes, by GPU index (see
    /// [`crate::devices::read_gpu_minors`]).
    fn gpu_device_minors(&self) -> io::Result<Vec<u32>>;
    fn available_bytes(&self, path: &Path) -> io::Result<u64>;
    fn is_tmpfs(&self, path: &Path) -> io::Result<bool>;
    /// Map inner PIDs of the namespace of `ns_member` to host PIDs.
    fn resolve_ns_pids(&self, ns_member: u32, wanted: &[u32]) -> Result<Vec<u32>, String>;
    fn lookup_user(&self, uid: u32) -> io::Result<Option<UserInfo>>;
    /// Which of `host_pids` use CUDA: every process that answers
    /// `cuda-checkpoint --get-state` (the job must not be stopped).
    fn cuda_pids(&self, host_pids: &[u32]) -> io::Result<Vec<u32>>;
    /// Mount points in the mount namespace of `pid`; an `InvalidData`
    /// error if CRIU could not parse one (see
    /// [`crate::mounts::checked_mount_points`]).
    fn mount_points(&self, pid: u32) -> io::Result<Vec<String>>;
    /// Mounts of the namespace of `pid` that CRIU could only restore from a
    /// path that is hidden in the helper's own namespace (see
    /// [`crate::mounts::unreachable_sources`]).
    fn unreachable_mounts(&self, _pid: u32) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// FUSE and network mounts of the namespace of `init_pid` that are gone
    /// from the host or do not answer, and what of the processes
    /// `host_pids` depends on them (see [`crate::deadmounts`]). Must not
    /// block on such a filesystem.
    fn dead_mounts(&self, _init_pid: u32, _host_pids: &[u32]) -> io::Result<DeadMounts> {
        Ok(DeadMounts::default())
    }
    /// For each of `mounts` (detached at dump time), what a restore binds
    /// in its place: the host's mount at its mount point if the filesystem
    /// is mounted there again and answers, else a placeholder (see
    /// [`Placeholders::for_mount`]).
    fn detached_sources(
        &self,
        mounts: &[DetachedMount],
        placeholders: &Placeholders,
    ) -> io::Result<Vec<String>> {
        Ok(mounts
            .iter()
            .map(|m| placeholders.for_mount(m).to_string_lossy().into_owned())
            .collect())
    }
    /// Where the external mount sources `paths` live in the helper's own
    /// namespace (see [`SourceMount`]), recorded at dump time.
    fn source_mounts(&self, _paths: &[String]) -> io::Result<Vec<SourceMount>> {
        Ok(Vec::new())
    }
    /// For each of `paths` (host paths that `criu restore` binds by name),
    /// why it must not (see [`crate::mountfd::bind_source_problem`]), or
    /// `None`. The real host asks a child process with a deadline.
    fn bind_source_problems(&self, paths: &[String]) -> io::Result<Vec<Option<String>>> {
        Ok(vec![None; paths.len()])
    }
    /// Processes of the job whose namespace init is `init_pid` that are in
    /// a PID or user namespace of their own (`unshare`, `bwrap`), which
    /// CRIU cannot dump: each as "<pid> (<command>)".
    fn nested_namespace_processes(&self, _init_pid: u32) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// Which of `sources` are not mounted as they were at dump time (see
    /// [`crate::mounts::source_problem`]); triggers automount points on
    /// the way.
    fn source_problems(&self, _sources: &[SourceMount]) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// Regular files that the processes `host_pids` have open, and their
    /// working directories, that the helper reaches by the same path (the
    /// same inode): paths on the host's filesystems, which a restore needs
    /// to find again.
    fn open_files(&self, _host_pids: &[u32]) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// What a dump of the processes `host_pids` must take care of (see
    /// [`DumpHazards`]); paths at or below `private` (mount points the
    /// job's namespace has of its own) do not count.
    fn dump_hazards(&self, _host_pids: &[u32], _private: &[String]) -> io::Result<DumpHazards> {
        Ok(DumpHazards::default())
    }
    /// The ports internet sockets on the host are bound to (see
    /// [`inet_ports_in_use`]).
    fn inet_ports_in_use(&self) -> io::Result<Vec<crate::images::InetPort>> {
        Ok(Vec::new())
    }
    /// Every group id the processes run with (`Gid:` and `Groups:` of
    /// `/proc/<pid>/status`), sorted; `None` if unknown.
    fn process_groups(&self, _host_pids: &[u32]) -> io::Result<Option<Vec<u32>>> {
        Ok(None)
    }
    /// Runs an access check as the job's owner (see [`crate::access`]).
    fn verify_access(&self, _req: &VerifyRequest) -> VerifyReply {
        Ok(Vec::new())
    }
    /// Bytes in use on the tmpfs mounts at `private` in the mount namespace
    /// of `pid` (the job's own `/tmp` and `/dev/shm`, whose contents CRIU
    /// saves in the image). Mount points that are not tmpfs count nothing.
    fn private_tmpfs_bytes(&self, _pid: u32, _private: &[String]) -> io::Result<u64> {
        Ok(0)
    }
    /// Which of `paths` do not exist (`lstat` fails) on this host. May
    /// block on a filesystem that does not answer (an NFS hard mount whose
    /// server is down), so the real host asks a child process with a
    /// deadline.
    fn missing_paths(&self, paths: &[String]) -> io::Result<Vec<String>> {
        Ok(paths
            .iter()
            .filter(|p| fs::symlink_metadata(p).is_err())
            .cloned()
            .collect())
    }
    /// The mount points of those of `mounts` (detached at the dump) that
    /// are missing on this host (see
    /// [`crate::deadmounts::mount_point_missing`]); in a child process
    /// with a deadline like [`Self::missing_paths`].
    fn missing_mount_points(&self, mounts: &[DetachedMount]) -> io::Result<Vec<String>> {
        Ok(mounts
            .iter()
            .filter(|m| {
                crate::deadmounts::mount_point_missing(Path::new(&m.mount_point), m.not_dir)
            })
            .map(|m| m.mount_point.clone())
            .collect())
    }
    /// For each of `files` (held by a job at its dump), the attributes of
    /// the file its path leads to now if it is the very file (see
    /// [`crate::access::held_file_attrs`]). Opens the files, which may
    /// block on a filesystem that does not answer, so the real host asks a
    /// child process with a deadline (a file it could not look up in time
    /// has no attributes).
    fn held_file_attrs(&self, files: &[HeldFile]) -> io::Result<Vec<Option<FileAttrs>>> {
        Ok(files.iter().map(crate::access::held_file_attrs).collect())
    }
    /// Runs `restore`, the CUDA restore of the processes `cuda_pids` of a
    /// stopped job (namespace init `init_pid`), with only their CUDA restore
    /// threads running: the job stays stopped and runs none of its own code
    /// (see [`crate::traced`]).
    fn with_cuda_threads_running(
        &self,
        _init_pid: u32,
        _cuda_pids: &[u32],
        restore: &mut (dyn FnMut() -> Result<(), String> + Send),
    ) -> Result<(), String> {
        restore()
    }
    /// The threads of `init_pid` and of the processes in its PID namespace
    /// that are not stopped (`T`), as `(process, thread)`.
    fn unstopped_threads(&self, _init_pid: u32) -> io::Result<Vec<(u32, u32)>> {
        Ok(Vec::new())
    }
}

/// Things about a job that make CRIU work on host paths as root.
///
/// The helper scans for them before a dump ([`scan_dump_hazards`], in
/// [`crate::engine::precheck_dump`]) and again while CRIU has the job
/// stopped after it ([`crate::access::run_dump_hook`]); both refuse what
/// [`DumpHazards::refusal`] names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpHazards {
    /// Deleted files that the job holds open or mapped, on the host's
    /// filesystems and on its own tmpfs mounts. CRIU saves them in the
    /// image (as "ghost" files), and a restore re-creates each one, as
    /// root, with its owner and mode, next to its old path.
    pub deleted_files: Vec<DeletedFile>,
    /// Paths on the host's filesystems that Unix sockets of the job are
    /// bound to. `criu restore` binds, unlinks and chowns them as root, by
    /// path.
    pub bound_sockets: Vec<String>,
    /// Files on the host's filesystems that the job holds open or mapped
    /// (see [`HeldFile`]).
    pub held_files: Vec<HeldFile>,
    /// Internet sockets of the job with `SO_REUSEPORT` (described for
    /// messages). CRIU creates and binds them as root, before it restores
    /// the job's credentials, so the restored socket belongs to root and
    /// would join the `SO_REUSEPORT` group of a root service on the same
    /// port (and get part of its connections).
    pub reuseport_sockets: Vec<String>,
    /// inotify and fanotify descriptors of the job (described for
    /// messages). CRIU re-adds their marks as root, through paths that
    /// follow symlinks, so a restored mark could watch a directory its
    /// owner may not read.
    #[serde(default)]
    pub notify_fds: Vec<String>,
    /// Abstract names (`@name`) of listening Unix sockets of the job.
    /// CRIU sets them up as root, before it restores the job's
    /// credentials, so a client asking who listens (`SO_PEERCRED`) would
    /// be told root.
    #[serde(default)]
    pub abstract_listeners: Vec<String>,
    /// Files the job holds that the NFS client renamed to `.nfs<hex>`
    /// because they were deleted while open. They have no name of their
    /// own any more, and CRIU could only dump them by linking them back
    /// (`--link-remap`, as root, by path).
    #[serde(default)]
    pub silly_renamed: Vec<String>,
}

/// A deleted file a job holds (see [`DumpHazards::deleted_files`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletedFile {
    pub path: String,
    pub size: u64,
    pub uid: u32,
    pub mode: u32,
    /// Hard links left. A file deleted while another link remains cannot be
    /// saved as a ghost: CRIU could only link it back (`--link-remap`).
    pub nlink: u64,
    /// On one of the job's own tmpfs mounts (its `/tmp`, `/dev/shm`),
    /// whose contents are saved in the image anyway.
    pub private: bool,
    /// Has file capabilities (a `security.capability` extended attribute).
    #[serde(default)]
    pub file_caps: bool,
}

impl DeletedFile {
    /// Whether running the file could grant privileges: it has the setuid
    /// or setgid bit, or file capabilities.
    pub fn grants_privileges(&self) -> bool {
        self.mode & 0o6000 != 0 || self.file_caps
    }
}

/// At most this many paths are named in a refusal.
const SHOWN_PATHS: usize = 5;

/// `items` (paths the job chose) for a message, at most [`SHOWN_PATHS`] of
/// them, with control characters escaped.
fn list_some<T: AsRef<str>>(items: &[T]) -> String {
    let escape = |item: &str| {
        item.chars()
            .flat_map(|c| {
                let escaped: Vec<char> = if c.is_control() {
                    c.escape_default().collect()
                } else {
                    vec![c]
                };
                escaped
            })
            .collect::<String>()
    };
    let mut list = items
        .iter()
        .take(SHOWN_PATHS)
        .map(|i| escape(i.as_ref()))
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > SHOWN_PATHS {
        list.push_str(&format!(" and {} more", items.len() - SHOWN_PATHS));
    }
    list
}

impl DumpHazards {
    /// Why a job with these hazards must not be dumped, if it must not.
    /// Checked before the dump and again by the post-dump hook, which makes
    /// CRIU abandon the dump and resume the job.
    ///
    /// Deleted files of other owners (a library a package upgrade replaced
    /// while the job ran) are fine: a restore re-creates them, as root,
    /// with their owner and mode and the contents the job could read
    /// anyway. Not so a privileged program: re-created setuid, setgid or
    /// with file capabilities, it could be executed through the restored
    /// descriptor.
    pub fn refusal(&self) -> Option<String> {
        // Every reason, so that each can be dealt with at once.
        let mut reasons = Vec::new();
        if !self.bound_sockets.is_empty() {
            reasons.push(format!(
                "the job has Unix sockets bound to {:?} on the host's filesystems (or to \
                 relative names, which lead there too); a restore would make CRIU create, \
                 unlink and chown those paths as root (bind sockets to an absolute path below \
                 /tmp, which is the job's own, or to an abstract name)",
                self.bound_sockets
            ));
        }
        if !self.reuseport_sockets.is_empty() {
            reasons.push(format!(
                "the job has internet sockets with SO_REUSEPORT ({}); CRIU would restore them \
                 as root's, which lets them share the port with a root service and receive \
                 part of its connections (do not set SO_REUSEPORT)",
                self.reuseport_sockets.join(", ")
            ));
        }
        if !self.notify_fds.is_empty() {
            reasons.push(format!(
                "the job has inotify or fanotify descriptors ({}); CRIU would re-add their \
                 watches as root, by paths (close them before the checkpoint)",
                self.notify_fds.join(", ")
            ));
        }
        if !self.abstract_listeners.is_empty() {
            reasons.push(format!(
                "the job has Unix sockets listening on the abstract names {:?}; CRIU would set \
                 them up as root, so their clients would be told that root listens there \
                 (listen on a path below /tmp instead)",
                self.abstract_listeners
            ));
        }
        let linked: Vec<&str> = self
            .deleted_files
            .iter()
            .filter(|f| f.nlink > 0)
            .map(|f| f.path.as_str())
            .collect();
        if !linked.is_empty() || !self.silly_renamed.is_empty() {
            let mut what = Vec::new();
            if !linked.is_empty() {
                what.push(format!(
                    "deleted files that still have other hard links ({})",
                    list_some(&linked)
                ));
            }
            if !self.silly_renamed.is_empty() {
                what.push(format!(
                    "files deleted on NFS while open ({})",
                    list_some(&self.silly_renamed)
                ));
            }
            reasons.push(format!(
                "the job holds {}; CRIU could only checkpoint them by linking them back into \
                 their directories as root (close those files before the checkpoint)",
                what.join(" and ")
            ));
        }
        let privileged: Vec<String> = self
            .deleted_files
            .iter()
            .filter(|f| f.grants_privileges())
            .map(|f| {
                let caps = if f.file_caps {
                    ", file capabilities"
                } else {
                    ""
                };
                format!(
                    "{} (uid {}, mode {:o}{caps})",
                    f.path,
                    f.uid,
                    f.mode & 0o7777
                )
            })
            .collect();
        if !privileged.is_empty() {
            reasons.push(format!(
                "the job holds deleted files with the setuid or setgid bit or file \
                 capabilities ({}); a restore would make CRIU re-create them as root with \
                 that owner and mode (close those files, or restart the job)",
                list_some(&privileged)
            ));
        }
        if reasons.is_empty() {
            return None;
        }
        Some(format!(
            "{}; so the job cannot be checkpointed (or run it with --no-checkpoint)",
            reasons.join("; ")
        ))
    }
}

/// Whether the file `path` leads to has a `security.capability` extended
/// attribute. One whose attributes cannot be read has none.
fn has_file_caps(path: &Path) -> bool {
    let mut empty: [u8; 0] = [];
    rustix::fs::getxattr(path, "security.capability", &mut empty[..]).is_ok_and(|n| n > 0)
}

/// Whether the last component of `path` is a name the Linux NFS client
/// gives a file deleted while open (`.nfs` and hex digits: its file id and
/// a counter).
pub fn is_nfs_silly_name(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .and_then(|name| name.strip_prefix(".nfs"))
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub use crate::access::HeldFile;

/// Whether `path` is absolute and normalized: no empty, `.` or `..`
/// components (the kernel's paths of open files are; a socket's name is
/// kept as given to `bind(2)`).
pub fn is_normalized_path(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    rest.is_empty()
        || rest
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

/// Whether a job's path names something the job's namespace has of its own
/// (or no file on any filesystem), rather than a host file. Only an
/// absolute, normalized path (see [`is_normalized_path`]) can.
pub fn is_private_path(path: &str, private: &[String]) -> bool {
    use crate::mounts::path_under;
    if !is_normalized_path(path) {
        return false;
    }
    path_under(path, "/proc")
        || private.iter().any(|p| path_under(path, p))
        || path.starts_with("/memfd:")
        || path.starts_with("/SYSV")
        || path.starts_with("/dev/zero")
        || path.starts_with("/[aio]")
}

/// The mount point in `private` that a Unix socket's name `name` lies
/// below by its name alone: an absolute, normalized path below one of
/// them. CRIU binds the name, as root, exactly as the job gave it to
/// `bind(2)`, so a name with `..` in it, or one below `/proc` (whose
/// `root` and `cwd` links lead anywhere), is never the job's own.
pub fn private_socket_mount<'a>(name: &str, private: &'a [String]) -> Option<&'a str> {
    use crate::mounts::path_under;
    if !is_normalized_path(name) || path_under(name, "/proc") {
        return None;
    }
    private
        .iter()
        .filter(|p| path_under(name, p))
        .max_by_key(|p| p.len())
        .map(String::as_str)
}

/// Whether a Unix socket name from `/proc/net/unix` may name a path on the
/// host's filesystems: anything but an abstract name (`@...`) and a name
/// below `private` (see [`private_socket_mount`]). A relative name
/// (`bind("ctl.sock")`) counts too: it was resolved against the binding
/// process's working directory at the time, which `/proc` does not record
/// (the process may have changed directory since), and CRIU resolves it to
/// a host path that a restore would bind as root.
pub fn is_host_socket_name(name: &str, private: &[String]) -> bool {
    if name.starts_with('@') {
        return false;
    }
    private_socket_mount(name, private).is_none()
}

/// Whether the socket name `name`, below the private mount `mount` by its
/// name (see [`private_socket_mount`]), also leads there in the namespace
/// of process `pid`: the directory the name is bound in must be reached
/// from `mount` without symlinks and without crossing into another mount
/// (looked at by descriptor, from the process's root). The name was
/// resolved when the job bound it, and a symlink on the way (or a mount
/// of a host directory below the private one) would have put it on the
/// host's filesystems.
pub fn socket_dir_is_private(proc_root: &Path, pid: u32, name: &str, mount: &str) -> bool {
    use rustix::fs::{Mode, OFlags, ResolveFlags};
    let open = || -> rustix::io::Result<bool> {
        let dir = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC;
        let root = rustix::fs::open(proc_root.join(format!("{pid}/root")), dir, Mode::empty())?;
        let mount_fd = rustix::fs::openat2(
            &root,
            mount,
            dir,
            Mode::empty(),
            ResolveFlags::IN_ROOT | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
        )?;
        let rest = name[mount.len()..].trim_start_matches('/');
        let parent = match rest.rsplit_once('/') {
            Some((parent, _)) => parent,
            None => return Ok(true),
        };
        rustix::fs::openat2(
            &mount_fd,
            parent,
            dir,
            Mode::empty(),
            ResolveFlags::BENEATH
                | ResolveFlags::NO_SYMLINKS
                | ResolveFlags::NO_MAGICLINKS
                | ResolveFlags::NO_XDEV,
        )?;
        Ok(true)
    };
    open().unwrap_or(false)
}

/// `IPPROTO_TCP`, `IPPROTO_UDP` and `IPPROTO_UDPLITE`, and the tables of
/// `/proc/net` that list their sockets.
const INET_TABLES: [(u8, &str, [&str; 2]); 3] = [
    (6, "TCP", ["tcp", "tcp6"]),
    (17, "UDP", ["udp", "udp6"]),
    (136, "UDP-Lite", ["udplite", "udplite6"]),
];

/// The name of IP protocol `proto` if it has ports gpueue knows of.
pub fn inet_proto_name(proto: u8) -> Option<&'static str> {
    INET_TABLES
        .iter()
        .find(|(p, ..)| *p == proto)
        .map(|(_, name, _)| *name)
}

/// `(local port, inode)` of each socket in a table of `/proc/net` (`tcp`,
/// `udp6`, ...). Sockets without an inode (in `TIME_WAIT`, or closed by
/// their process) belong to no one any more and are left out.
pub fn parse_net_inet(table: &str) -> Vec<(u16, u64)> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            // sl local_address rem_address st tx:rx tr:when retrnsmt uid timeout inode
            let f: Vec<&str> = l.split_whitespace().collect();
            let port = u16::from_str_radix(f.get(1)?.rsplit_once(':')?.1, 16).ok()?;
            let ino: u64 = f.get(9)?.parse().ok()?;
            (ino != 0).then_some((port, ino))
        })
        .collect()
}

/// The internet sockets in `/proc/<pid>/net` (the network namespace of
/// `pid`; `self` for the caller's): `(port, inode)` for each protocol of
/// [`INET_TABLES`]. A table that does not exist (no IPv6, no UDP-Lite) is
/// empty.
fn inet_sockets(proc_root: &Path, pid: &str) -> io::Result<Vec<(crate::images::InetPort, u64)>> {
    let mut out = Vec::new();
    for (proto, _, tables) in INET_TABLES {
        for table in tables {
            let text = match fs::read_to_string(proc_root.join(pid).join("net").join(table)) {
                Ok(t) => t,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            out.extend(
                parse_net_inet(&text)
                    .into_iter()
                    .map(|(port, ino)| (crate::images::InetPort { proto, port }, ino)),
            );
        }
    }
    Ok(out)
}

/// The ports internet sockets in the caller's network namespace are bound
/// to (by protocol; any address, any family).
pub fn inet_ports_in_use(proc_root: &Path) -> io::Result<Vec<crate::images::InetPort>> {
    let mut out: Vec<crate::images::InetPort> = Vec::new();
    for (port, _) in inet_sockets(proc_root, "self")? {
        if !out.contains(&port) {
            out.push(port);
        }
    }
    Ok(out)
}

/// The ports of the restored job's internet sockets (held by the
/// processes `procs`, in the network namespace of `init_pid`) that a
/// socket of someone else is bound to as well. CRIU binds the job's
/// sockets as root, with `SO_REUSEADDR` and `SO_REUSEPORT`, so such a
/// socket may share a root service's port.
pub fn inet_port_conflicts(
    proc_root: &Path,
    init_pid: u32,
    procs: &[u32],
) -> io::Result<Vec<String>> {
    let mut own = std::collections::HashSet::new();
    for pid in procs {
        for e in fs::read_dir(proc_root.join(format!("{pid}/fd")))
            .into_iter()
            .flatten()
            .flatten()
        {
            if let Some(ino) = fs::read_link(e.path()).ok().and_then(|t| {
                t.to_str()?
                    .strip_prefix("socket:[")?
                    .strip_suffix(']')?
                    .parse::<u64>()
                    .ok()
            }) {
                own.insert(ino);
            }
        }
    }
    let sockets = inet_sockets(proc_root, &init_pid.to_string())?;
    Ok(port_conflicts(&sockets, &own))
}

/// See [`inet_port_conflicts`]: of `sockets` (port, inode), those of the
/// ports of the job's sockets (`own`) held by other sockets too.
pub fn port_conflicts(
    sockets: &[(crate::images::InetPort, u64)],
    own: &std::collections::HashSet<u64>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (port, _) in sockets.iter().filter(|(_, ino)| own.contains(ino)) {
        let what = format!("the job's socket on {port} shares the port with another socket");
        if sockets
            .iter()
            .any(|(p, ino)| p == port && !own.contains(ino))
            && !out.contains(&what)
        {
            out.push(what);
        }
    }
    out
}

/// The inodes of listening sockets in `/proc/net/unix` (`__SO_ACCEPTCON`
/// in `Flags`).
pub fn parse_net_unix_listening(table: &str) -> std::collections::HashSet<u64> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let flags = u32::from_str_radix(f.get(3)?, 16).ok()?;
            (flags & 0x0001_0000 != 0).then(|| f.get(6)?.parse().ok())?
        })
        .collect()
}

impl<T: HostProbe> HostProbe for std::sync::Arc<T> {
    fn job_processes(&self, init_pid: u32) -> io::Result<Vec<ProcInfo>> {
        (**self).job_processes(init_pid)
    }
    fn gpu_snapshot(&self) -> io::Result<GpuSnapshot> {
        (**self).gpu_snapshot()
    }
    fn gpu_device_minors(&self) -> io::Result<Vec<u32>> {
        (**self).gpu_device_minors()
    }
    fn available_bytes(&self, path: &Path) -> io::Result<u64> {
        (**self).available_bytes(path)
    }
    fn is_tmpfs(&self, path: &Path) -> io::Result<bool> {
        (**self).is_tmpfs(path)
    }
    fn resolve_ns_pids(&self, ns_member: u32, wanted: &[u32]) -> Result<Vec<u32>, String> {
        (**self).resolve_ns_pids(ns_member, wanted)
    }
    fn lookup_user(&self, uid: u32) -> io::Result<Option<UserInfo>> {
        (**self).lookup_user(uid)
    }
    fn cuda_pids(&self, host_pids: &[u32]) -> io::Result<Vec<u32>> {
        (**self).cuda_pids(host_pids)
    }
    fn mount_points(&self, pid: u32) -> io::Result<Vec<String>> {
        (**self).mount_points(pid)
    }
    fn unreachable_mounts(&self, pid: u32) -> io::Result<Vec<String>> {
        (**self).unreachable_mounts(pid)
    }
    fn dead_mounts(&self, init_pid: u32, host_pids: &[u32]) -> io::Result<DeadMounts> {
        (**self).dead_mounts(init_pid, host_pids)
    }
    fn detached_sources(
        &self,
        mounts: &[DetachedMount],
        placeholders: &Placeholders,
    ) -> io::Result<Vec<String>> {
        (**self).detached_sources(mounts, placeholders)
    }
    fn source_mounts(&self, paths: &[String]) -> io::Result<Vec<SourceMount>> {
        (**self).source_mounts(paths)
    }
    fn source_problems(&self, sources: &[SourceMount]) -> io::Result<Vec<String>> {
        (**self).source_problems(sources)
    }
    fn open_files(&self, host_pids: &[u32]) -> io::Result<Vec<String>> {
        (**self).open_files(host_pids)
    }
    fn with_cuda_threads_running(
        &self,
        init_pid: u32,
        cuda_pids: &[u32],
        restore: &mut (dyn FnMut() -> Result<(), String> + Send),
    ) -> Result<(), String> {
        (**self).with_cuda_threads_running(init_pid, cuda_pids, restore)
    }
    fn unstopped_threads(&self, init_pid: u32) -> io::Result<Vec<(u32, u32)>> {
        (**self).unstopped_threads(init_pid)
    }
    fn dump_hazards(&self, host_pids: &[u32], private: &[String]) -> io::Result<DumpHazards> {
        (**self).dump_hazards(host_pids, private)
    }
    fn inet_ports_in_use(&self) -> io::Result<Vec<crate::images::InetPort>> {
        (**self).inet_ports_in_use()
    }
    fn process_groups(&self, host_pids: &[u32]) -> io::Result<Option<Vec<u32>>> {
        (**self).process_groups(host_pids)
    }
    fn verify_access(&self, req: &VerifyRequest) -> VerifyReply {
        (**self).verify_access(req)
    }
    fn private_tmpfs_bytes(&self, pid: u32, private: &[String]) -> io::Result<u64> {
        (**self).private_tmpfs_bytes(pid, private)
    }
    fn missing_paths(&self, paths: &[String]) -> io::Result<Vec<String>> {
        (**self).missing_paths(paths)
    }
    fn bind_source_problems(&self, paths: &[String]) -> io::Result<Vec<Option<String>>> {
        (**self).bind_source_problems(paths)
    }
    fn nested_namespace_processes(&self, init_pid: u32) -> io::Result<Vec<String>> {
        (**self).nested_namespace_processes(init_pid)
    }
    fn missing_mount_points(&self, mounts: &[DetachedMount]) -> io::Result<Vec<String>> {
        (**self).missing_mount_points(mounts)
    }
    fn held_file_attrs(&self, files: &[HeldFile]) -> io::Result<Vec<Option<FileAttrs>>> {
        (**self).held_file_attrs(files)
    }
}

/// Whether `/proc/<pid>/maps` contents map an NVIDIA device node
/// (`/dev/nvidia0`, `/dev/nvidiactl`, `/dev/nvidia-uvm`, ...).
pub fn maps_nvidia_device(maps: &str) -> bool {
    maps.lines().any(|line| {
        line.split_whitespace()
            .nth(5)
            .is_some_and(|path| path.starts_with("/dev/nvidia"))
    })
}

/// The real host.
#[derive(Debug, Clone)]
pub struct SystemHost {
    pub proc_root: PathBuf,
    pub nvidia_smi: PathBuf,
    pub nvidia_smi_timeout: Duration,
    pub cuda_checkpoint: CudaCheckpointOptions,
    /// Outer timeout of one `cuda-checkpoint --get-state`.
    pub get_state_timeout: Duration,
    /// This binary, run as `<exe> verify-access` for access checks.
    pub helper_exe: PathBuf,
    /// Timeout of one access check.
    pub verify_timeout: Duration,
    /// Whose directories may lie on the way to a host path that CRIU
    /// binds (see [`crate::mountfd::bind_source_problem`]): root's only
    /// unless set (the helper sets [`OwnerTrust::system`] from its job
    /// policy).
    ///
    /// [`OwnerTrust::system`]: crate::mountfd::OwnerTrust::system
    pub bind_trust: crate::mountfd::OwnerTrust,
}

impl SystemHost {
    pub fn new(
        nvidia_smi: PathBuf,
        cuda_checkpoint: CudaCheckpointOptions,
        helper_exe: PathBuf,
    ) -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            nvidia_smi,
            nvidia_smi_timeout: Duration::from_secs(30),
            cuda_checkpoint,
            get_state_timeout: Duration::from_secs(10),
            helper_exe,
            verify_timeout: Duration::from_secs(120),
            bind_trust: crate::mountfd::OwnerTrust::ROOT_ONLY,
        }
    }

    /// `<helper> verify-access` with `req` on stdin.
    fn run_verify(&self, req: &VerifyRequest) -> VerifyReply {
        crate::access::run_verify_child(&self.helper_exe, req, self.verify_timeout)
    }

    fn nvidia_query(&self, args: &[&str]) -> io::Result<String> {
        let mut spec = CommandSpec::new(&self.nvidia_smi);
        spec.args(args);
        run_with_timeout(&spec, self.nvidia_smi_timeout, Duration::from_millis(20))
            .map(|o| o.stdout)
            .map_err(|e| io::Error::other(format!("{spec}: {e}")))
    }
}

impl SystemHost {
    /// [`crate::deadmounts::probe_paths`] in a `verify-access` child, with
    /// a deadline: one entry per path, `None` if its filesystem answered.
    fn probe(&self, paths: &[String]) -> io::Result<Vec<Option<String>>> {
        self.probe_within(paths, crate::deadmounts::PROBE_TIMEOUT)
    }

    /// [`Self::probe`], waiting up to `timeout` for the paths.
    fn probe_within(&self, paths: &[String], timeout: Duration) -> io::Result<Vec<Option<String>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let req = VerifyRequest::Probe(crate::access::ProbeCheck {
            paths: paths.to_vec(),
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
        });
        let reply = crate::access::run_verify_child(
            &self.helper_exe,
            &req,
            timeout + Duration::from_secs(10),
        )
        .map_err(io::Error::other)?;
        if reply.len() != paths.len() {
            return Err(io::Error::other(format!(
                "the mount probe answered {} of {} paths",
                reply.len(),
                paths.len()
            )));
        }
        Ok(reply
            .into_iter()
            .map(|r| (!r.is_empty()).then_some(r))
            .collect())
    }
}

/// What process `pid` holds (see [`crate::deadmounts::ProcessFiles`]),
/// from `/proc` only: links are read, never followed, so a filesystem that
/// does not answer cannot block this. A process that is gone holds
/// nothing.
pub fn process_files(proc_root: &Path, pid: u32) -> crate::deadmounts::ProcessFiles {
    use crate::deadmounts::{ProcessFiles, parse_fdinfo_mnt_id, parse_maps};
    let dir = proc_root.join(pid.to_string());
    let link = |name: &str| {
        fs::read_link(dir.join(name))
            .ok()
            .map(|t| t.to_string_lossy().into_owned())
    };
    let mut out = ProcessFiles {
        cwd: link("cwd"),
        root: link("root"),
        exe: link("exe"),
        maps: fs::read_to_string(dir.join("maps"))
            .map(|m| parse_maps(&m))
            .unwrap_or_default(),
        ..ProcessFiles::default()
    };
    let mut sockets = Vec::new();
    for entry in fs::read_dir(dir.join("fd")).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let Some(fd) = name.to_str().and_then(|n| n.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        let target = target.to_string_lossy().into_owned();
        if let Some(ino) = target
            .strip_prefix("socket:[")
            .and_then(|s| s.strip_suffix(']'))
            .and_then(|s| s.parse::<u64>().ok())
        {
            sockets.push(ino);
            continue;
        }
        let mnt_id = fs::read_to_string(dir.join("fdinfo").join(&name))
            .ok()
            .and_then(|t| parse_fdinfo_mnt_id(&t));
        out.fds.push((fd, mnt_id, target));
    }
    if !sockets.is_empty()
        && let Ok(table) = fs::read_to_string(dir.join("net/unix"))
    {
        for (ino, path) in parse_net_unix(&table) {
            if sockets.contains(&ino) && path.starts_with('/') && !out.sockets.contains(&path) {
                out.sockets.push(path);
            }
        }
    }
    out
}

/// The files on the host's filesystems that the processes `host_pids`
/// hold open or mapped (see [`HeldFile`]; without their attributes yet).
pub fn held_files(proc_root: &Path, host_pids: &[u32], private: &[String]) -> Vec<HeldFile> {
    use std::os::unix::fs::MetadataExt;
    let mut out: Vec<HeldFile> = Vec::new();
    for pid in host_pids {
        let dir = proc_root.join(pid.to_string());
        let links = ["fd", "map_files"].iter().flat_map(|sub| {
            fs::read_dir(dir.join(sub))
                .map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
                .unwrap_or_default()
        });
        for link in links {
            let Ok(target) = fs::read_link(&link) else {
                continue;
            };
            let Some(target) = target.to_str() else {
                continue;
            };
            // Not deleted files (CRIU saves them), nor pipes, sockets and
            // the like.
            if !target.starts_with('/')
                || target.ends_with(" (deleted)")
                || is_private_path(target, private)
            {
                continue;
            }
            let Ok(m) = fs::metadata(&link) else {
                continue;
            };
            let held = HeldFile {
                path: target.to_owned(),
                dev: m.dev(),
                ino: m.ino(),
                attrs: None,
            };
            if !out.contains(&held) {
                out.push(held);
            }
        }
    }
    out
}

/// Regular files that the processes `host_pids` have open, and their
/// working directories, that this process reaches by the same path (see
/// [`HostProbe::open_files`]). Stats the files, which may block on a
/// filesystem that does not answer: the real host runs it in a
/// `verify-access` child with a deadline.
pub fn scan_open_files(proc_root: &Path, host_pids: &[u32]) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;
    let mut out: Vec<String> = Vec::new();
    for pid in host_pids {
        let dir = proc_root.join(pid.to_string());
        let fds = fs::read_dir(dir.join("fd"))
            .map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
            .unwrap_or_default();
        for link in fds
            .iter()
            .map(|p| (p, false))
            .chain([(&dir.join("cwd"), true)])
        {
            let (link, is_cwd) = link;
            let (Ok(target), Ok(meta)) = (fs::read_link(link), fs::metadata(link)) else {
                continue;
            };
            if !(meta.is_file() || is_cwd && meta.is_dir()) {
                continue;
            }
            let Some(path) = target.to_str() else {
                continue;
            };
            // Deleted files (CRIU saves them) and files of mounts the
            // helper does not have (the job's private /tmp) are not
            // reached by the same path.
            let same =
                fs::metadata(path).is_ok_and(|m| m.dev() == meta.dev() && m.ino() == meta.ino());
            if same && !out.iter().any(|p| p == path) {
                out.push(path.to_owned());
            }
        }
    }
    out
}

/// What a dump of the processes `host_pids` must take care of (see
/// [`DumpHazards`]). Stats the files they hold, which may block on a
/// filesystem that does not answer: the real host runs it in a
/// `verify-access` child with a deadline (see [`HostProbe::dump_hazards`]
/// and [`crate::access::run_dump_hook`]).
pub fn scan_dump_hazards(
    proc_root: &Path,
    host_pids: &[u32],
    private: &[String],
) -> io::Result<DumpHazards> {
    use std::os::unix::fs::MetadataExt;
    let mut out = DumpHazards::default();
    let mut deleted = std::collections::HashSet::new();
    let mut sockets = std::collections::HashSet::new();
    let mut socket_fds: Vec<(u32, i32)> = Vec::new();
    for pid in host_pids {
        let dir = proc_root.join(pid.to_string());
        let links = ["fd", "map_files"].iter().flat_map(|sub| {
            fs::read_dir(dir.join(sub))
                .map(|d| d.flatten().map(|e| e.path()).collect::<Vec<_>>())
                .unwrap_or_default()
        });
        for link in links {
            let Ok(target) = fs::read_link(&link) else {
                continue;
            };
            let target = target.to_string_lossy();
            if target == "anon_inode:inotify" || target == "anon_inode:[fanotify]" {
                let what = format!(
                    "{} of process {pid}",
                    target
                        .trim_start_matches("anon_inode:")
                        .trim_matches(['[', ']'])
                );
                if !out.notify_fds.contains(&what) {
                    out.notify_fds.push(what);
                }
                continue;
            }
            if let Some(ino) = target
                .strip_prefix("socket:[")
                .and_then(|s| s.strip_suffix(']'))
                .and_then(|s| s.parse::<u64>().ok())
            {
                sockets.insert(ino);
                if let Some(fd) = link
                    .strip_prefix(dir.join("fd"))
                    .ok()
                    .and_then(|n| n.to_str()?.parse::<i32>().ok())
                {
                    socket_fds.push((*pid, fd));
                }
                continue;
            }
            let Some(path) = target.strip_suffix(" (deleted)") else {
                // A held file (see `held_files`), or one the NFS client
                // renamed because it was deleted while open.
                if target.starts_with('/')
                    && !is_private_path(&target, private)
                    && is_nfs_silly_name(&target)
                    && !out.silly_renamed.iter().any(|p| *p == target)
                {
                    out.silly_renamed.push(target.into_owned());
                }
                continue;
            };
            // Not files on any filesystem: /proc, memfd, SysV shared
            // memory and the like.
            if !path.starts_with('/') || is_private_path(path, &[]) {
                continue;
            }
            // A process that exited meanwhile holds nothing.
            let Ok(m) = fs::metadata(&link) else {
                continue;
            };
            // By inode: two deleted files may have had the same path.
            if !deleted.insert((m.dev(), m.ino())) {
                continue;
            }
            out.deleted_files.push(DeletedFile {
                path: path.to_owned(),
                size: m.len(),
                uid: m.uid(),
                mode: m.mode(),
                nlink: m.nlink(),
                private: is_private_path(path, private),
                file_caps: has_file_caps(&link),
            });
        }
    }
    out.held_files = held_files(proc_root, host_pids, private);
    for (pid, fd) in socket_fds {
        if let Some(addr) = reuseport_inet_socket(pid, fd)
            && !out.reuseport_sockets.contains(&addr)
        {
            out.reuseport_sockets.push(addr);
        }
    }
    if !sockets.is_empty()
        && let Some(pid) = host_pids.first()
    {
        // The job shares the host's network namespace; `net/unix` lists
        // the bound path of each socket by inode.
        let table = fs::read_to_string(proc_root.join(pid.to_string()).join("net/unix"))?;
        let listening = parse_net_unix_listening(&table);
        for (ino, path) in parse_net_unix(&table) {
            if !sockets.contains(&ino) {
                continue;
            }
            if path.starts_with('@') {
                if listening.contains(&ino) && !out.abstract_listeners.contains(&path) {
                    out.abstract_listeners.push(path);
                }
                continue;
            }
            // A name below a private mount must lead there too.
            let host = match private_socket_mount(&path, private) {
                Some(mount) => !socket_dir_is_private(proc_root, *pid, &path, mount),
                None => true,
            };
            if host && !out.bound_sockets.contains(&path) {
                out.bound_sockets.push(path);
            }
        }
    }
    Ok(out)
}

/// Host PIDs of the processes in the PID namespace whose init has host PID
/// `init_pid`.
pub fn ns_processes(proc_root: &Path, init_pid: u32) -> io::Result<Vec<u32>> {
    Ok(ns_process_infos(proc_root, init_pid)?
        .into_iter()
        .map(|p| p.host_pid)
        .collect())
}

/// The descendants of `init_pid` (by `PPid`, which in a PID namespace
/// always leads back to its init) that are in another PID or user
/// namespace than it, as "<pid> (<command>)". CRIU refuses to dump nested
/// PID and user namespaces ("Can't dump nested pid namespace"), and the
/// helper's scans of a job (see [`ns_processes`]) do not see the processes
/// in a nested PID namespace.
pub fn nested_namespace_processes(proc_root: &Path, init_pid: u32) -> io::Result<Vec<String>> {
    use std::collections::{HashMap, HashSet};
    let ns_of =
        |pid: u32, ns: &str| fs::read_link(proc_root.join(pid.to_string()).join("ns").join(ns));
    let pid_ns = ns_of(init_pid, "pid")?;
    let user_ns = ns_of(init_pid, "user")?;
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for entry in fs::read_dir(proc_root)? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let ppid = status
            .lines()
            .find_map(|l| l.strip_prefix("PPid:"))
            .and_then(|v| v.trim().parse::<u32>().ok());
        if let Some(ppid) = ppid {
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut out = Vec::new();
    let mut seen = HashSet::from([init_pid]);
    let mut todo = vec![init_pid];
    while let Some(pid) = todo.pop() {
        for &child in children.get(&pid).into_iter().flatten() {
            if !seen.insert(child) {
                continue;
            }
            todo.push(child);
            // Exited meanwhile: nothing to report.
            let (Ok(p), Ok(u)) = (ns_of(child, "pid"), ns_of(child, "user")) else {
                continue;
            };
            if p != pid_ns || u != user_ns {
                let comm = fs::read_to_string(proc_root.join(child.to_string()).join("comm"))
                    .map(|c| printable(c.trim_end_matches('\n')))
                    .unwrap_or_default();
                out.push(format!("{child} ({comm})"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// `name` (a process name, which its process chooses) for a message:
/// control characters escaped, at most 64 characters.
pub fn printable(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().take(64) {
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

fn ns_process_infos(proc_root: &Path, init_pid: u32) -> io::Result<Vec<ProcInfo>> {
    let ns_of = |pid: &str| fs::read_link(proc_root.join(pid).join("ns/pid"));
    let target = ns_of(&init_pid.to_string())?;
    let mut out = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        if ns_of(name).ok().as_ref() != Some(&target) {
            continue;
        }
        // Processes may exit while scanning.
        let Ok(status) = fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let Some(nspid) = procfs::parse_nspid(&status) else {
            continue;
        };
        out.push(ProcInfo {
            host_pid: nspid[0],
            ns_pid: *nspid.last().unwrap_or(&nspid[0]),
            rss_bytes: parse_vm_rss(&status).unwrap_or(0),
        });
    }
    out.sort_by_key(|p| p.ns_pid);
    Ok(out)
}

impl HostProbe for SystemHost {
    fn job_processes(&self, init_pid: u32) -> io::Result<Vec<ProcInfo>> {
        ns_process_infos(&self.proc_root, init_pid)
    }

    fn gpu_snapshot(&self) -> io::Result<GpuSnapshot> {
        let gpus = self.nvidia_query(&[
            "--query-gpu=index,uuid,name,driver_version,memory.free",
            "--format=csv,noheader,nounits",
        ])?;
        let apps = self.nvidia_query(&[
            "--query-compute-apps=pid,gpu_uuid,used_memory",
            "--format=csv,noheader,nounits",
        ])?;
        Ok(parse_nvidia_smi(&gpus, &apps))
    }

    fn gpu_device_minors(&self) -> io::Result<Vec<u32>> {
        crate::devices::read_gpu_minors(&self.proc_root.join("driver/nvidia/gpus"))
    }

    fn available_bytes(&self, path: &Path) -> io::Result<u64> {
        space::available_bytes(path)
    }

    fn is_tmpfs(&self, path: &Path) -> io::Result<bool> {
        space::is_tmpfs(path)
    }

    fn resolve_ns_pids(&self, ns_member: u32, wanted: &[u32]) -> Result<Vec<u32>, String> {
        match procfs::resolve_ns_pids(&self.proc_root, ns_member, wanted) {
            Ok(Ok(pids)) => Ok(pids),
            Ok(Err(missing)) => Err(format!("restored CUDA PIDs not found: {missing:?}")),
            Err(e) => Err(format!("scanning /proc: {e}")),
        }
    }

    fn lookup_user(&self, uid: u32) -> io::Result<Option<UserInfo>> {
        let user = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
            .map_err(io::Error::from)?;
        let Some(user) = user else { return Ok(None) };
        let name = std::ffi::CString::new(user.name.clone())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let groups = nix::unistd::getgrouplist(&name, user.gid).map_err(io::Error::from)?;
        Ok(Some(UserInfo {
            name: user.name,
            gid: user.gid.as_raw(),
            groups: groups.into_iter().map(|g| g.as_raw()).collect(),
        }))
    }

    fn cuda_pids(&self, host_pids: &[u32]) -> io::Result<Vec<u32>> {
        // Without it every process would look like one without CUDA, and
        // CRIU would fail on the NVIDIA device mappings it cannot dump.
        let tool = &self.cuda_checkpoint.path;
        if !host_pids.is_empty() && !tool.is_file() {
            return Err(io::Error::other(
                if tool.starts_with(crate::config::MISSING_PROGRAM_DIR) {
                    "the helper did not find cuda-checkpoint when it started (install it and \
                     restart gpueue-helper)"
                        .to_owned()
                } else {
                    format!("cuda-checkpoint {} is missing", tool.display())
                },
            ));
        }
        let mut out = Vec::new();
        for &pid in host_pids {
            // cuda-checkpoint hangs on stopped/frozen processes.
            if procfs::check_runnable(&self.proc_root, Path::new("/sys/fs/cgroup"), pid)
                .ok()
                .flatten()
                .is_some()
            {
                return Err(io::Error::other(format!(
                    "process {pid} is stopped or frozen"
                )));
            }
            let spec = cuda_get_state(&self.cuda_checkpoint, pid);
            // Processes without CUDA make cuda-checkpoint fail.
            let state = run_with_timeout(&spec, self.get_state_timeout, Duration::from_millis(20));
            match &state {
                Ok(o) if probe::parse_cuda_state(&o.stdout).is_some() => out.push(pid),
                // One that maps an NVIDIA device does use the GPU: CRIU
                // cannot dump it unless cuda-checkpoint can.
                _ if fs::read_to_string(self.proc_root.join(pid.to_string()).join("maps"))
                    .is_ok_and(|maps| maps_nvidia_device(&maps)) =>
                {
                    let why = match state {
                        Ok(o) => o
                            .stderr
                            .lines()
                            .chain(o.stdout.lines())
                            .next()
                            .map_or_else(|| "no state".to_owned(), |l| l.trim().to_owned()),
                        Err(e) => e.to_string(),
                    };
                    return Err(io::Error::other(format!(
                        "process {pid} uses an NVIDIA device but `cuda-checkpoint --get-state` \
                         does not know its CUDA state: {why}"
                    )));
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn with_cuda_threads_running(
        &self,
        init_pid: u32,
        cuda_pids: &[u32],
        restore: &mut (dyn FnMut() -> Result<(), String> + Send),
    ) -> Result<(), String> {
        let mut threads = Vec::with_capacity(cuda_pids.len());
        for &pid in cuda_pids {
            // The PIDs come from a scan: right before root's cuda-checkpoint
            // acts on one, it must still be a process of the job.
            crate::traced::check_process(init_pid, pid)?;
            let what = format!("cuda-checkpoint --get-restore-tid of process {pid}");
            let spec = cuda_get_restore_tid(&self.cuda_checkpoint, pid);
            let out = run_with_timeout(&spec, self.get_state_timeout, Duration::from_millis(20))
                .map_err(|e| format!("{what}: {e}"))?;
            let tid = out
                .stdout
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("{what} printed {:?}", out.stdout.trim()))?;
            threads.push((pid, tid));
        }
        crate::traced::with_threads_running(init_pid, &threads, restore)?
    }

    fn unstopped_threads(&self, init_pid: u32) -> io::Result<Vec<(u32, u32)>> {
        let mut out = Vec::new();
        for pid in [init_pid]
            .into_iter()
            .chain(ns_processes(&self.proc_root, init_pid)?)
        {
            let Ok(tasks) = fs::read_dir(self.proc_root.join(pid.to_string()).join("task")) else {
                continue;
            };
            for task in tasks.flatten() {
                let Ok(stat) = fs::read_to_string(task.path().join("stat")) else {
                    continue;
                };
                let tid = task.file_name().to_str().and_then(|t| t.parse().ok());
                match (tid, procfs::parse_stat_state(&stat)) {
                    // Gone, or exiting.
                    (_, Some('Z' | 'X')) | (None, _) => {}
                    (Some(_), Some('T')) => {}
                    (Some(tid), _) => out.push((pid, tid)),
                }
            }
        }
        Ok(out)
    }

    fn mount_points(&self, pid: u32) -> io::Result<Vec<String>> {
        let mi = fs::read(self.proc_root.join(pid.to_string()).join("mountinfo"))?;
        crate::mounts::checked_mount_points(&mi)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    fn unreachable_mounts(&self, pid: u32) -> io::Result<Vec<String>> {
        use crate::mounts::unreachable_sources;
        let job = fs::read_to_string(self.proc_root.join(pid.to_string()).join("mountinfo"))?;
        let own = fs::read_to_string(self.proc_root.join("self/mountinfo"))?;
        Ok(unreachable_sources(
            &MountTree::parse(&job),
            &MountTree::parse(&own),
        ))
    }

    fn dead_mounts(&self, init_pid: u32, host_pids: &[u32]) -> io::Result<DeadMounts> {
        use crate::deadmounts::{dead_mount_users, dead_mounts, remote_mounts};
        let job = MountTree::parse(&fs::read_to_string(
            self.proc_root.join(init_pid.to_string()).join("mountinfo"),
        )?);
        let own = MountTree::parse(&fs::read_to_string(self.proc_root.join("self/mountinfo"))?);
        let remote = remote_mounts(&job, &own);
        // Only the ones the host still has need asking; `/proc/<pid>/root`
        // leads to them through the job's namespace.
        let asked: Vec<String> = remote
            .iter()
            .filter(|m| m.on_host)
            .map(|m| {
                self.proc_root
                    .join(init_pid.to_string())
                    .join("root")
                    .join(m.mount_point.trim_start_matches('/'))
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        let mut answers = self.probe(&asked)?.into_iter();
        let answers: Vec<Option<String>> = remote
            .iter()
            .map(|m| {
                if m.on_host {
                    answers.next().flatten()
                } else {
                    None
                }
            })
            .collect();
        // Mounts that CRIU could only bind again by a host path below a
        // directory that a job's owner could change are treated the same
        // way (see `crate::deadmounts::add_unsafe_binds`).
        let candidates = crate::mounts::bind_source_candidates(&job, &own);
        let mut paths: Vec<String> = Vec::new();
        for (_, c) in &candidates {
            for p in c {
                if !paths.contains(p) {
                    paths.push(p.clone());
                }
            }
        }
        let problems: Vec<(String, String)> = paths
            .iter()
            .cloned()
            .zip(self.bind_source_problems(&paths)?)
            .filter_map(|(p, why)| why.map(|w| (p, w)))
            .collect();
        let (mut remote, mut answers) = (remote, answers);
        crate::deadmounts::add_unsafe_binds(
            &job,
            &mut remote,
            &mut answers,
            &candidates,
            &problems,
        );
        let mut dead = dead_mounts(&job, &remote, &answers);
        // What a restore needs at each mount point (see
        // `crate::deadmounts::describe_mount_points`). Only filesystems
        // that cannot block are looked at: FUSE and network mounts are
        // mounted on directories (as assumed before).
        let root = self.proc_root.join(init_pid.to_string()).join("root");
        crate::deadmounts::describe_mount_points(&job, &own, &mut dead, |m| {
            !crate::deadmounts::is_remote_fstype(&m.fstype)
                && fs::symlink_metadata(root.join(m.mount_point.trim_start_matches('/')))
                    .is_ok_and(|meta| !meta.is_dir())
        });
        if !dead.dead.is_empty() {
            let procs: Vec<_> = host_pids
                .iter()
                .map(|pid| process_files(&self.proc_root, *pid))
                .collect();
            dead.used = dead_mount_users(&job, &dead, &procs);
        }
        Ok(dead)
    }

    fn detached_sources(
        &self,
        mounts: &[DetachedMount],
        placeholders: &Placeholders,
    ) -> io::Result<Vec<String>> {
        use crate::deadmounts::is_back;
        let own = MountTree::parse(&fs::read_to_string(self.proc_root.join("self/mountinfo"))?);
        let back: Vec<String> = mounts
            .iter()
            .filter(|m| is_back(m, &own))
            .map(|m| m.mount_point.clone())
            .collect();
        let answers = self.probe(&back)?;
        // CRIU binds the host's mount by its path, as root: only one that
        // no job's owner can redirect (see `crate::mountfd`).
        let unsafe_paths = self.bind_source_problems(&back)?;
        Ok(mounts
            .iter()
            .map(|m| {
                let placeholder = placeholders.for_mount(m).to_string_lossy().into_owned();
                let i = back.iter().position(|p| p == &m.mount_point);
                let alive = i.is_some_and(|i| answers.get(i).is_some_and(Option::is_none));
                match i.and_then(|i| unsafe_paths.get(i).cloned().flatten()) {
                    Some(why) if alive => {
                        eprintln!(
                            "gpueue-helper: {} is mounted again, but not bound into the \
                             restored job: {why}",
                            m.mount_point
                        );
                        placeholder
                    }
                    _ if alive => m.mount_point.clone(),
                    _ => placeholder,
                }
            })
            .collect())
    }

    fn source_mounts(&self, paths: &[String]) -> io::Result<Vec<SourceMount>> {
        let own = fs::read_to_string(self.proc_root.join("self/mountinfo"))?;
        Ok(crate::mounts::describe_sources(
            paths,
            &MountTree::parse(&own),
        ))
    }

    fn source_problems(&self, sources: &[SourceMount]) -> io::Result<Vec<String>> {
        use crate::mounts::{ends_on_automount, source_problem};
        let read = || -> io::Result<MountTree> {
            Ok(MountTree::parse(&fs::read_to_string(
                self.proc_root.join("self/mountinfo"),
            )?))
        };
        let mut tree = read()?;
        // An automount point recorded with its filesystem mounted: a
        // lookup makes the automounter mount it (in the helper's namespace,
        // which is the host's). In a `verify-access` child with a deadline
        // (killed when it passes), so that an NFS server that does not
        // answer only delays the check and leaves nothing stuck behind in
        // the helper.
        let triggers: Vec<String> = sources
            .iter()
            .filter(|s| source_problem(s, &tree).is_some() && ends_on_automount(&s.path, &tree))
            .map(|s| format!("{}/.", s.path))
            .collect();
        if !triggers.is_empty() {
            // Only the mounts it leaves behind count (read again below).
            if let Err(e) = self.probe_within(&triggers, AUTOMOUNT_TIMEOUT) {
                eprintln!("gpueue-helper: triggering automount points {triggers:?}: {e}");
            }
            tree = read()?;
        }
        Ok(sources
            .iter()
            .filter_map(|s| source_problem(s, &tree))
            .collect())
    }

    fn open_files(&self, host_pids: &[u32]) -> io::Result<Vec<String>> {
        if host_pids.is_empty() {
            return Ok(Vec::new());
        }
        let req = VerifyRequest::OpenFiles(crate::access::ScanCheck {
            proc_root: self.proc_root.clone(),
            pids: host_pids.to_vec(),
            private: Vec::new(),
        });
        crate::access::run_child(&self.helper_exe, &req, OPEN_FILES_TIMEOUT)
            .map_err(io::Error::other)
    }

    fn dump_hazards(&self, host_pids: &[u32], private: &[String]) -> io::Result<DumpHazards> {
        let req = VerifyRequest::DumpHazards(crate::access::ScanCheck {
            proc_root: self.proc_root.clone(),
            pids: host_pids.to_vec(),
            private: private.to_vec(),
        });
        crate::access::run_child(&self.helper_exe, &req, DUMP_SCAN_TIMEOUT)
            .map_err(io::Error::other)
    }

    fn inet_ports_in_use(&self) -> io::Result<Vec<crate::images::InetPort>> {
        inet_ports_in_use(&self.proc_root)
    }

    fn process_groups(&self, host_pids: &[u32]) -> io::Result<Option<Vec<u32>>> {
        let mut groups = Vec::new();
        for pid in host_pids {
            let Ok(status) =
                fs::read_to_string(self.proc_root.join(pid.to_string()).join("status"))
            else {
                continue; // exited
            };
            let creds = crate::access::parse_creds(&status).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, format!("status of {pid}"))
            })?;
            groups.extend(creds.gids);
            groups.extend(creds.groups);
        }
        groups.sort_unstable();
        groups.dedup();
        Ok(Some(groups))
    }

    fn verify_access(&self, req: &VerifyRequest) -> VerifyReply {
        self.run_verify(req)
    }

    fn private_tmpfs_bytes(&self, pid: u32, private: &[String]) -> io::Result<u64> {
        // `/proc/<pid>/root` leads into the job's mount namespace, where
        // these paths are its own tmpfs mounts.
        let root = self.proc_root.join(pid.to_string()).join("root");
        let mut total = 0u64;
        for p in private {
            let path = root.join(p.trim_start_matches('/'));
            match space::tmpfs_used_bytes(&path) {
                Ok(used) => total = total.saturating_add(used.unwrap_or(0)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(io::Error::other(format!("{}: {e}", path.display()))),
            }
        }
        Ok(total)
    }

    fn missing_paths(&self, paths: &[String]) -> io::Result<Vec<String>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        self.run_verify(&VerifyRequest::Exist(crate::access::ExistCheck {
            paths: paths.to_vec(),
            mount_points: false,
            not_dirs: Vec::new(),
        }))
        .map_err(io::Error::other)
    }

    fn bind_source_problems(&self, paths: &[String]) -> io::Result<Vec<Option<String>>> {
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        let timeout = crate::deadmounts::PROBE_TIMEOUT;
        let req = VerifyRequest::BindSources(crate::access::BindSourceCheck {
            paths: paths.to_vec(),
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            trust: self.bind_trust,
        });
        let reply = crate::access::run_verify_child(
            &self.helper_exe,
            &req,
            timeout + Duration::from_secs(10),
        )
        .map_err(io::Error::other)?;
        if reply.len() != paths.len() {
            return Err(io::Error::other(format!(
                "the bind source check answered {} of {} paths",
                reply.len(),
                paths.len()
            )));
        }
        Ok(reply
            .into_iter()
            .map(|r| (!r.is_empty()).then_some(r))
            .collect())
    }

    fn nested_namespace_processes(&self, init_pid: u32) -> io::Result<Vec<String>> {
        nested_namespace_processes(&self.proc_root, init_pid)
    }

    fn missing_mount_points(&self, mounts: &[DetachedMount]) -> io::Result<Vec<String>> {
        if mounts.is_empty() {
            return Ok(Vec::new());
        }
        self.run_verify(&VerifyRequest::Exist(crate::access::ExistCheck {
            paths: mounts.iter().map(|m| m.mount_point.clone()).collect(),
            mount_points: true,
            not_dirs: mounts
                .iter()
                .filter(|m| m.not_dir)
                .map(|m| m.mount_point.clone())
                .collect(),
        }))
        .map_err(io::Error::other)
    }

    fn held_file_attrs(&self, files: &[HeldFile]) -> io::Result<Vec<Option<FileAttrs>>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let req = VerifyRequest::HeldAttrs(crate::access::HeldAttrsCheck {
            files: files.to_vec(),
            timeout_ms: u64::try_from(HELD_ATTRS_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
        });
        let attrs: Vec<Option<FileAttrs>> = crate::access::run_child(
            &self.helper_exe,
            &req,
            HELD_ATTRS_TIMEOUT + Duration::from_secs(10),
        )
        .map_err(io::Error::other)?;
        if attrs.len() != files.len() {
            return Err(io::Error::other(format!(
                "the lookup of held files answered {} of {} files",
                attrs.len(),
                files.len()
            )));
        }
        Ok(attrs)
    }
}

/// How long [`SystemHost::source_problems`] waits for automount points to
/// be mounted.
const AUTOMOUNT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long [`SystemHost::held_file_attrs`] waits for the files' lookups.
const HELD_ATTRS_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a scan of a job's files ([`scan_dump_hazards`], before a dump
/// and in the post-dump hook) may take.
pub const DUMP_SCAN_TIMEOUT: Duration = Duration::from_secs(15);

/// How long [`SystemHost::open_files`] waits for [`scan_open_files`].
const OPEN_FILES_TIMEOUT: Duration = Duration::from_secs(10);

/// The local address of socket `fd` of process `pid` if it is an internet
/// socket with `SO_REUSEPORT` (looked at through a duplicate taken with
/// `pidfd_getfd`, which leaves the job's socket alone). A process or
/// descriptor that is gone counts as none.
fn reuseport_inet_socket(pid: u32, fd: i32) -> Option<String> {
    use rustix::net::{AddressFamily, sockopt};
    let pidfd = rustix::process::pidfd_open(
        rustix::process::Pid::from_raw(i32::try_from(pid).ok()?)?,
        rustix::process::PidfdFlags::empty(),
    )
    .ok()?;
    let sock =
        rustix::process::pidfd_getfd(&pidfd, fd, rustix::process::PidfdGetfdFlags::empty()).ok()?;
    let family = sockopt::socket_domain(&sock).ok()?;
    if family != AddressFamily::INET && family != AddressFamily::INET6 {
        return None;
    }
    if !sockopt::socket_reuseport(&sock).ok()? {
        return None;
    }
    let addr = rustix::net::getsockname(&sock)
        .ok()
        .and_then(|a| std::net::SocketAddr::try_from(a).ok())
        .map_or_else(|| "an unknown address".to_owned(), |a| a.to_string());
    Some(addr)
}

/// `(inode, path)` of each bound socket in `/proc/net/unix`.
pub fn parse_net_unix(table: &str) -> Vec<(u64, String)> {
    table
        .lines()
        .skip(1)
        .filter_map(|l| {
            // Num RefCount Protocol Flags Type St Inode [Path]
            let mut f = l.split_whitespace();
            let ino = f.nth(6)?.parse().ok()?;
            let path: Vec<&str> = f.collect();
            (!path.is_empty()).then(|| (ino, path.join(" ")))
        })
        .collect()
}

/// `VmRSS:` from `/proc/<pid>/status`, in bytes.
pub fn parse_vm_rss(status: &str) -> Option<u64> {
    let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

/// Whether `path` is a mount point according to `/proc/self/mountinfo`.
pub fn is_mount_point(mountinfo: &str, path: &Path) -> bool {
    let want = path.to_string_lossy();
    mountinfo
        .lines()
        .filter_map(crate::mounts::mount_point_field)
        .any(|mp| mp == want)
}

/// Parse the two `nvidia-smi --format=csv,noheader,nounits` queries (memory
/// is in MiB).
pub fn parse_nvidia_smi(gpus: &str, apps: &str) -> GpuSnapshot {
    const MIB: u64 = 1024 * 1024;
    let fields = |l: &str| {
        l.split(',')
            .map(|f| f.trim().to_owned())
            .collect::<Vec<_>>()
    };
    let mut snap = GpuSnapshot::default();
    for line in gpus.lines().filter(|l| !l.trim().is_empty()) {
        let f = fields(line);
        if f.len() < 5 {
            continue;
        }
        let Ok(index) = f[0].parse() else { continue };
        snap.driver_version.get_or_insert_with(|| f[3].clone());
        snap.gpus.push(GpuState {
            index,
            uuid: f[1].clone(),
            model: f[2].clone(),
            free_bytes: f[4].parse::<u64>().unwrap_or(0) * MIB,
        });
    }
    for line in apps.lines().filter(|l| !l.trim().is_empty()) {
        let f = fields(line);
        if f.len() < 3 {
            continue;
        }
        let Ok(pid) = f[0].parse() else { continue };
        snap.compute_apps.push(ComputeApp {
            pid,
            gpu_uuid: f[1].clone(),
            used_bytes: f[2].parse::<u64>().unwrap_or(0) * MIB,
        });
    }
    snap
}

/// Host PIDs whose parent is `ppid`, from `/proc/*/stat`.
pub fn children_of(proc_root: &Path, ppid: u32) -> io::Result<Vec<u32>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(proc_root)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if parse_stat_ppid(&stat) == Some(ppid) {
            out.push(pid);
        }
    }
    out.sort_unstable();
    Ok(out)
}

/// Field 4 (ppid) of `/proc/<pid>/stat`.
pub fn parse_stat_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Processes below the job's init in a PID or user namespace of their
    /// own, found by `PPid` (a nested PID namespace's processes have other
    /// `ns/pid` links than the init); other processes of the host are not
    /// looked at.
    #[test]
    fn finds_nested_namespaces_below_the_init() {
        let tmp = tempfile::tempdir().unwrap();
        let proc_ = |pid: u32, ppid: u32, pidns: &str, userns: &str, comm: &str| {
            let d = tmp.path().join(pid.to_string());
            fs::create_dir_all(d.join("ns")).unwrap();
            fs::write(d.join("status"), format!("Name:\t{comm}\nPPid:\t{ppid}\n")).unwrap();
            fs::write(d.join("comm"), format!("{comm}\n")).unwrap();
            std::os::unix::fs::symlink(pidns, d.join("ns/pid")).unwrap();
            std::os::unix::fs::symlink(userns, d.join("ns/user")).unwrap();
        };
        proc_(1, 0, "pid:[1]", "user:[1]", "systemd");
        proc_(100, 1, "pid:[2]", "user:[1]", "gpueue-helper");
        proc_(101, 100, "pid:[2]", "user:[1]", "sh");
        proc_(102, 101, "pid:[2]", "user:[3]", "podman");
        proc_(103, 101, "pid:[4]", "user:[3]", "unshare");
        proc_(104, 103, "pid:[4]", "user:[3]", "sleep");
        proc_(200, 1, "pid:[9]", "user:[9]", "other");
        assert_eq!(
            nested_namespace_processes(tmp.path(), 100).unwrap(),
            ["102 (podman)", "103 (unshare)", "104 (sleep)"]
        );
        assert!(
            nested_namespace_processes(tmp.path(), 103)
                .unwrap()
                .is_empty()
        );
    }

    /// Without cuda-checkpoint a GPU job's dump is refused instead of
    /// taking every process for one without CUDA.
    #[test]
    fn cuda_pids_without_cuda_checkpoint() {
        let host = |path: &Path| {
            SystemHost::new(
                PathBuf::from("nvidia-smi"),
                CudaCheckpointOptions {
                    path: path.to_owned(),
                    ..CudaCheckpointOptions::default()
                },
                PathBuf::from("/nonexistent/gpueue-helper"),
            )
        };
        let me = std::process::id();
        let missing = Path::new(crate::config::MISSING_PROGRAM_DIR).join("cuda-checkpoint");
        let e = host(&missing).cuda_pids(&[me]).unwrap_err().to_string();
        assert!(e.contains("restart gpueue-helper"), "{e}");
        let e = host(Path::new("/nonexistent/cc"))
            .cuda_pids(&[me])
            .unwrap_err()
            .to_string();
        assert!(e.contains("/nonexistent/cc is missing"), "{e}");
        // Nothing to ask about.
        assert_eq!(host(&missing).cuda_pids(&[]).unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn finds_nvidia_device_mappings() {
        // What CRIU failed on when cuda-checkpoint had not run.
        let cuda = "\
55d0c0a00000-55d0c0a01000 r--p 00000000 103:02 1234   /home/u/check
200200000-200400000 rw-s 00000000 00:07 1046                             /dev/nvidia0
7ffd1000-7ffd2000 rw-p 00000000 00:00 0                                  [stack]
";
        assert!(maps_nvidia_device(cuda));
        let plain = "\
55d0c0a00000-55d0c0a01000 r--p 00000000 103:02 1234   /home/u/dev/nvidia-notes
7ffd1000-7ffd2000 rw-p 00000000 00:00 0                                  [stack]
7ffd3000-7ffd4000 rw-p 00000000 00:00 0
";
        assert!(!maps_nvidia_device(plain));
    }

    #[test]
    fn parses_nvidia_smi() {
        let snap = parse_nvidia_smi(
            "0, GPU-aaaa, NVIDIA GeForce RTX 4080, 595.91.07, 15000\n",
            "1234, GPU-aaaa, 7000\n4321, GPU-aaaa, [N/A]\n",
        );
        assert_eq!(snap.driver_version.as_deref(), Some("595.91.07"));
        assert_eq!(snap.gpus[0].model, "NVIDIA GeForce RTX 4080");
        assert_eq!(snap.gpus[0].free_bytes, 15000 * 1024 * 1024);
        assert_eq!(snap.compute_apps[0].used_bytes, 7000 * 1024 * 1024);
        assert_eq!(snap.compute_apps[1].used_bytes, 0);
    }

    /// [`held_files`] lists the files a process holds open, not pipes,
    /// deleted files or files under the namespace's own mounts.
    #[test]
    fn lists_held_files() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let gone = dir.path().join("gone.txt");
        fs::write(&gone, "x").unwrap();
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exec 4<\"$1\"; rm \"$1\"; echo ready; exec sleep 30")
            .arg("sh")
            .arg(&gone)
            .stdout(std::process::Stdio::piped())
            .stderr(fs::File::create(&path).unwrap())
            .spawn()
            .unwrap();
        let mut ready = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut ready,
        )
        .unwrap();
        let proc_root = Path::new("/proc");
        let held = held_files(proc_root, &[child.id()], &[]);
        let private = vec![dir.path().to_str().unwrap().to_owned()];
        let held_private = held_files(proc_root, &[child.id()], &private);
        let _ = child.kill();
        let _ = child.wait();
        let m = fs::metadata(&path).unwrap();
        let p = path.to_str().unwrap();
        assert!(
            held.iter()
                .any(|h| h.path == p && (h.dev, h.ino) == (m.dev(), m.ino())),
            "{held:?}"
        );
        assert!(
            held.iter()
                .all(|h| !h.path.contains("gone.txt") && h.path.starts_with('/')),
            "{held:?}"
        );
        assert!(held_private.iter().all(|h| h.path != p), "{held_private:?}");
    }

    #[test]
    fn parses_proc_fields() {
        assert_eq!(parse_vm_rss("Name:\tx\nVmRSS:\t  2048 kB\n"), Some(2 << 20));
        assert_eq!(parse_stat_ppid("42 (a b) c) S 7 42 42 0"), Some(7));
        let mi = "22 1 0:21 / /run/gpueue/criu\\040root rw - tmpfs tmpfs rw\n";
        assert!(is_mount_point(mi, Path::new("/run/gpueue/criu root")));
        assert!(!is_mount_point(mi, Path::new("/run")));
    }

    #[test]
    fn parses_net_unix_and_private_paths() {
        let t = "Num       RefCount Protocol Flags    Type St Inode Path\n\
                 0000000000000000: 00000002 00000000 00010000 0001 01 4242 /home/a/s ock\n\
                 0000000000000000: 00000003 00000000 00000000 0001 03 4243\n\
                 0000000000000000: 00000002 00000000 00010000 0001 01 4244 @abstract\n";
        assert_eq!(
            parse_net_unix(t),
            [
                (4242, "/home/a/s ock".to_owned()),
                (4244, "@abstract".to_owned())
            ]
        );
        let private = ["/tmp".to_owned(), "/dev/shm".to_owned()];
        assert!(is_host_socket_name("/home/a/s ock", &private));
        assert!(is_host_socket_name("ctl.sock", &private));
        assert!(is_host_socket_name("run/ctl.sock", &private));
        assert!(!is_host_socket_name("@abstract", &private));
        assert!(!is_host_socket_name("/tmp/ctl.sock", &private));
        assert!(is_private_path("/tmp/x", &private));
        assert!(is_private_path("/proc/1/fd", &private));
        assert!(is_private_path("/memfd:buf", &private));
        assert!(!is_private_path("/tmpx/y", &private));
        assert!(!is_private_path("/home/a/f", &private));
    }

    #[test]
    fn process_names_are_printable() {
        assert_eq!(printable("python3"), "python3");
        assert_eq!(printable("a\nb\x1b[2J"), "a\\nb\\u{1b}[2J");
        assert_eq!(printable(&"x".repeat(100)).len(), 64);
    }

    /// Our own process: a deleted file it holds and a socket it bound.
    #[test]
    fn finds_dump_hazards() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("gone");
        let held = fs::File::create(&f).unwrap();
        fs::remove_file(&f).unwrap();
        let sock = tmp.path().join("sock");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let host = SystemHost::new("nvidia-smi".into(), Default::default(), "x".into());
        let proc_root = Path::new("/proc");
        let me = std::process::id();
        let h = scan_dump_hazards(proc_root, &[me], &["/tmp-private".into()]).unwrap();
        let f = f.to_str().unwrap();
        let found = h.deleted_files.iter().find(|d| d.path == f);
        let found = found.unwrap_or_else(|| panic!("{h:?}"));
        assert_eq!(found.uid, nix::unistd::geteuid().as_raw(), "{h:?}");
        assert_eq!((found.nlink, found.private), (0, false), "{h:?}");
        // Only this file (other tests of this process may hold others).
        let only_deleted = DumpHazards {
            deleted_files: vec![found.clone()],
            ..Default::default()
        };
        assert_eq!(only_deleted.refusal(), None, "{h:?}");
        // Another deleted file that had the same path is another file.
        fs::write(f, b"0123456789").unwrap();
        let held2 = fs::File::open(f).unwrap();
        fs::remove_file(f).unwrap();
        let h2 = scan_dump_hazards(proc_root, &[me], &["/tmp-private".into()]).unwrap();
        let mut sizes: Vec<u64> = h2
            .deleted_files
            .iter()
            .filter(|d| d.path == f)
            .map(|d| d.size)
            .collect();
        sizes.sort_unstable();
        assert_eq!(sizes, [0, 10], "{h2:?}");
        drop(held2);
        assert!(
            h.bound_sockets.contains(&sock.to_str().unwrap().to_owned()),
            "{h:?}"
        );
        let tmp_root = tmp.path().parent().unwrap().to_str().unwrap().to_owned();
        let h = scan_dump_hazards(proc_root, &[me], &[tmp_root]).unwrap();
        let found = h.deleted_files.iter().find(|d| d.path == f);
        assert!(found.is_some_and(|d| d.private), "{h:?}");
        assert!(h.bound_sockets.is_empty(), "{h:?}");
        drop(held);
        let groups = host.process_groups(&[std::process::id()]).unwrap().unwrap();
        // An internet socket with SO_REUSEPORT (and one without).
        use rustix::net::{AddressFamily, SocketType, sockopt};
        let plain = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let reuse = rustix::net::socket(AddressFamily::INET, SocketType::STREAM, None).unwrap();
        sockopt::set_socket_reuseport(&reuse, true).unwrap();
        rustix::net::bind(&reuse, &std::net::SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        rustix::net::listen(&reuse, 1).unwrap();
        let port = std::net::SocketAddr::try_from(rustix::net::getsockname(&reuse).unwrap())
            .unwrap()
            .port();
        let h = scan_dump_hazards(proc_root, &[me], &["/tmp-private".into()]).unwrap();
        // pidfd_getfd needs ptrace access to ourselves, which a sandbox may
        // deny; then nothing is found.
        if !h.reuseport_sockets.is_empty() {
            assert_eq!(h.reuseport_sockets, [format!("127.0.0.1:{port}")], "{h:?}");
        }
        drop((plain, reuse));
        assert!(groups.contains(&nix::unistd::getegid().as_raw()));
    }

    /// Regression (audit V2, S1-2/S2-1): the kernel keeps a socket's name
    /// as given to bind(2), unnormalized, and CRIU binds that very name as
    /// root on restore. A name that only looks like it is below a private
    /// mount (`..`, `.`, `//`, a magic link below `/proc`) must count as a
    /// host name.
    #[test]
    fn unnormalized_socket_names_count_as_host_names() {
        let private = ["/tmp".to_owned(), "/dev/shm".to_owned()];
        for name in [
            "/tmp/../home/a/s",
            "/tmp/x/../../etc/s",
            "/dev/shm/../../run/s",
            "/tmp/./../home/a/s",
            "/proc/self/root/home/a/s",
            "/proc/1/cwd/s",
            "/proc/thread-self/root/run/s",
        ] {
            assert!(is_host_socket_name(name, &private), "{name}");
        }
        // Names CRIU would resolve below the private tmpfs stay private.
        assert!(!is_host_socket_name("/tmp/a/s", &private));
    }

    /// Regression (audit V2, S1-2/S2-1): a socket the job binds through
    /// `..` or through a symlink inside its private tmpfs lies on a host
    /// filesystem, and the dump must report it.
    #[test]
    fn sockets_bound_out_of_a_private_dir_are_hazards() {
        let tmp = tempfile::tempdir().unwrap();
        let private_dir = tmp.path().join("private");
        let host_dir = tmp.path().join("host");
        fs::create_dir(&private_dir).unwrap();
        fs::create_dir(&host_dir).unwrap();
        std::os::unix::fs::symlink(&host_dir, private_dir.join("link")).unwrap();
        let via_dotdot = private_dir.join("../host/a.sock");
        let via_link = private_dir.join("link/b.sock");
        let _a = std::os::unix::net::UnixListener::bind(&via_dotdot).unwrap();
        let _b = std::os::unix::net::UnixListener::bind(&via_link).unwrap();
        assert!(host_dir.join("a.sock").exists() && host_dir.join("b.sock").exists());
        let private = [private_dir.to_str().unwrap().to_owned()];
        let h = scan_dump_hazards(Path::new("/proc"), &[std::process::id()], &private).unwrap();
        for name in [&via_dotdot, &via_link] {
            assert!(
                h.bound_sockets.iter().any(|s| s == name.to_str().unwrap()),
                "{name:?} not reported: {h:?}"
            );
        }
    }

    #[test]
    fn normalized_paths() {
        for p in ["/", "/a", "/a/b.c", "/a/..b"] {
            assert!(is_normalized_path(p), "{p}");
        }
        for p in ["", "a/b", "/a/", "//a", "/a//b", "/a/./b", "/a/../b", "/.."] {
            assert!(!is_normalized_path(p), "{p}");
        }
        let private = ["/tmp".to_owned(), "/tmp/x".to_owned()];
        assert!(!is_private_path("/tmp/../etc/passwd", &private));
        assert_eq!(private_socket_mount("/tmp/x/s", &private), Some("/tmp/x"));
        assert_eq!(private_socket_mount("/tmp/s", &private), Some("/tmp"));
        assert_eq!(private_socket_mount("/proc/1/root/tmp/s", &private), None);
    }

    #[test]
    fn parses_net_inet_and_listening_unix_sockets() {
        let t = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                 0: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4242 1 0 100 0 0 10 0\n\
                 1: 0100007F:1F91 0100007F:9C40 06 00000000:00000000 03:00000F9A 00000000     0        0 0 3 0\n";
        assert_eq!(parse_net_inet(t), [(8080, 4242)]);
        let t6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
                  0: 00000000000000000000000000000000:0016 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 77 1 0 100 0 0 10 0\n";
        assert_eq!(parse_net_inet(t6), [(22, 77)]);
        let u = "Num       RefCount Protocol Flags    Type St Inode Path\n\
                 0000000000000000: 00000002 00000000 00010000 0001 01 4242 @listen\n\
                 0000000000000000: 00000002 00000000 00000000 0002 01 4243 @dgram\n";
        assert_eq!(
            parse_net_unix_listening(u),
            std::collections::HashSet::from([4242])
        );
    }

    /// Our own process: an inotify descriptor, a listening socket with an
    /// abstract name, and an internet socket sharing its port.
    #[test]
    fn finds_notify_fds_abstract_listeners_and_port_conflicts() {
        use std::os::linux::net::SocketAddrExt;
        let _ify = rustix::fs::inotify::init(rustix::fs::inotify::CreateFlags::CLOEXEC).unwrap();
        let name = format!("gpueue-test-{}", std::process::id());
        let addr = std::os::unix::net::SocketAddr::from_abstract_name(&name).unwrap();
        let _l = std::os::unix::net::UnixListener::bind_addr(&addr).unwrap();
        let host = SystemHost::new("nvidia-smi".into(), Default::default(), "x".into());
        let h =
            scan_dump_hazards(Path::new("/proc"), &[std::process::id()], &["/tmp".into()]).unwrap();
        assert_eq!(
            h.notify_fds,
            [format!("inotify of process {}", std::process::id())]
        );
        assert_eq!(h.abstract_listeners, [format!("@{name}")]);

        // Two sockets of ours on one UDP port: no conflict while both are
        // the job's, one when only one of them is.
        use rustix::net::{AddressFamily, SocketType, sockopt};
        let a = rustix::net::socket(AddressFamily::INET, SocketType::DGRAM, None).unwrap();
        sockopt::set_socket_reuseport(&a, true).unwrap();
        rustix::net::bind(&a, &std::net::SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let port = std::net::SocketAddr::try_from(rustix::net::getsockname(&a).unwrap())
            .unwrap()
            .port();
        let b = rustix::net::socket(AddressFamily::INET, SocketType::DGRAM, None).unwrap();
        sockopt::set_socket_reuseport(&b, true).unwrap();
        rustix::net::bind(&b, &std::net::SocketAddr::from(([127, 0, 0, 1], port))).unwrap();
        let in_use = host.inet_ports_in_use().unwrap();
        assert!(in_use.contains(&crate::images::InetPort { proto: 17, port }));
        let me = std::process::id();
        assert!(
            inet_port_conflicts(Path::new("/proc"), me, &[me])
                .unwrap()
                .is_empty()
        );
        drop((a, b));
        let p = |proto, port| crate::images::InetPort { proto, port };
        let sockets = [(p(6, 80), 1), (p(6, 80), 2), (p(17, 80), 3), (p(6, 81), 4)];
        let own = std::collections::HashSet::from([1, 3]);
        assert_eq!(
            port_conflicts(&sockets, &own),
            ["the job's socket on TCP port 80 shares the port with another socket"]
        );
        let own = std::collections::HashSet::from([1, 2, 3]);
        assert!(port_conflicts(&sockets, &own).is_empty());
    }

    /// A deleted file that keeps another hard link, and one the NFS client
    /// renamed: CRIU could only link them back, so both are refused.
    #[test]
    fn finds_deleted_files_with_links_and_nfs_silly_names() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("f");
        fs::write(&f, b"x").unwrap();
        fs::hard_link(&f, tmp.path().join("keep")).unwrap();
        let linked = fs::File::open(&f).unwrap();
        fs::remove_file(&f).unwrap();
        let silly = tmp.path().join(".nfs000000000012340000000a");
        fs::write(&silly, b"y").unwrap();
        let silly_held = fs::File::open(&silly).unwrap();
        let me = std::process::id();
        let h = scan_dump_hazards(Path::new("/proc"), &[me], &[]).unwrap();
        let f = f.to_str().unwrap();
        let found = h.deleted_files.iter().find(|d| d.path == f);
        assert!(found.is_some_and(|d| d.nlink == 1), "{h:?}");
        assert!(
            h.silly_renamed
                .contains(&silly.to_str().unwrap().to_owned()),
            "{h:?}"
        );
        // Only these (other tests of this process may hold other hazards).
        let h = DumpHazards {
            deleted_files: h.deleted_files,
            silly_renamed: h.silly_renamed,
            ..Default::default()
        };
        let why = h.refusal().unwrap();
        assert!(why.contains("other hard links") && why.contains(f), "{why}");
        assert!(why.contains("deleted on NFS"), "{why}");
        drop((linked, silly_held));
    }

    #[test]
    fn paths_in_refusals_are_printable() {
        assert_eq!(list_some(&["/tmp/a\nb", "/x"]), "/tmp/a\\nb, /x");
    }

    #[test]
    fn nfs_silly_names() {
        assert!(is_nfs_silly_name("/home/a/.nfs000000000012340000000a"));
        assert!(is_nfs_silly_name(".nfsABCDEF"));
        assert!(!is_nfs_silly_name("/home/a/.nfs"));
        assert!(!is_nfs_silly_name("/home/a/.nfsrc"));
        assert!(!is_nfs_silly_name("/home/a/.nfs0/x"));
    }

    #[test]
    fn refuses_deleted_privileged_programs_only() {
        let file = |path: &str, uid: u32, mode: u32| DeletedFile {
            path: path.into(),
            size: 1,
            uid,
            mode,
            nlink: 0,
            private: false,
            file_caps: false,
        };
        // The job's own files, and files of others such as libraries a
        // package upgrade replaced while the job ran.
        let mut h = DumpHazards {
            deleted_files: vec![
                file("/home/a/tmp", 1000, 0o100600),
                file("/dev/shm/sem.x", 1000, 0o100644),
                file("/usr/lib/x86_64-linux-gnu/libc.so.6", 0, 0o100755),
                file("/home/b/f", 1001, 0o100600),
            ],
            ..Default::default()
        };
        assert_eq!(h.refusal(), None);
        h.deleted_files[0].private = true;
        assert_eq!(h.refusal(), None);
        let mut caps = file("/usr/bin/ping", 0, 0o100755);
        caps.file_caps = true;
        let mut private_suid = file("/tmp/suid", 0, 0o104755);
        private_suid.private = true;
        for bad in [
            file("/usr/bin/sudo", 0, 0o104755),
            file("/home/a/sgid", 1000, 0o102755),
            caps,
            private_suid,
        ] {
            let mut h = h.clone();
            let path = bad.path.clone();
            h.deleted_files.push(bad);
            let why = h.refusal().unwrap_or_else(|| panic!("{h:?}"));
            assert!(why.contains(&path) && why.contains("setuid"), "{why}");
        }
    }

    #[test]
    fn finds_own_children() {
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .spawn()
            .unwrap();
        let kids = children_of(Path::new("/proc"), std::process::id()).unwrap();
        assert!(kids.contains(&child.id()));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
