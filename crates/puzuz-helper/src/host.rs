//! Everything the helper reads from the host besides systemd: processes,
//! GPUs, users, filesystems. Behind [`HostProbe`] so tests can fake it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use puzuz_checkpoint::command::{CudaCheckpointOptions, cuda_get_state};
use puzuz_checkpoint::exec::run_with_timeout;
use puzuz_checkpoint::{CommandSpec, probe, procfs, space};

use crate::access::{VerifyReply, VerifyRequest};
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
    /// Mount points in the mount namespace of `pid`.
    fn mount_points(&self, pid: u32) -> io::Result<Vec<String>>;
    /// Mounts of the namespace of `pid` that CRIU could only restore from a
    /// path that is hidden in the helper's own namespace (see
    /// [`crate::mounts::unreachable_sources`]).
    fn unreachable_mounts(&self, _pid: u32) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// Where the external mount sources `paths` live in the helper's own
    /// namespace (see [`SourceMount`]), recorded at dump time.
    fn source_mounts(&self, _paths: &[String]) -> io::Result<Vec<SourceMount>> {
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
}

/// Things about a job that make CRIU work on host paths as root.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DumpHazards {
    /// Deleted files on the host's filesystems that the job holds open or
    /// mapped, with their sizes. `criu dump --link-remap` would link each
    /// back into its directory, as root, by path.
    pub deleted_files: Vec<(String, u64)>,
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
}

pub use crate::access::HeldFile;

/// Whether a job's path names something the job's namespace has of its own
/// (or no file on any filesystem), rather than a host file.
pub fn is_private_path(path: &str, private: &[String]) -> bool {
    use crate::mounts::path_under;
    path_under(path, "/proc")
        || private.iter().any(|p| path_under(path, p))
        || path.starts_with("/memfd:")
        || path.starts_with("/SYSV")
        || path.starts_with("/dev/zero")
}

/// Whether a Unix socket name from `/proc/net/unix` may name a path on the
/// host's filesystems: anything but an abstract name (`@...`) and an
/// absolute path below `private`. A relative name (`bind("ctl.sock")`)
/// counts too: it was resolved against the binding process's working
/// directory at the time, which `/proc` does not record (the process may
/// have changed directory since), and CRIU resolves it to a host path
/// that a restore would bind as root.
pub fn is_host_socket_name(name: &str, private: &[String]) -> bool {
    if name.starts_with('@') {
        return false;
    }
    !(name.starts_with('/') && is_private_path(name, private))
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
    fn source_mounts(&self, paths: &[String]) -> io::Result<Vec<SourceMount>> {
        (**self).source_mounts(paths)
    }
    fn source_problems(&self, sources: &[SourceMount]) -> io::Result<Vec<String>> {
        (**self).source_problems(sources)
    }
    fn open_files(&self, host_pids: &[u32]) -> io::Result<Vec<String>> {
        (**self).open_files(host_pids)
    }
    fn dump_hazards(&self, host_pids: &[u32], private: &[String]) -> io::Result<DumpHazards> {
        (**self).dump_hazards(host_pids, private)
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

/// Host PIDs of the processes in the PID namespace whose init has host PID
/// `init_pid`.
pub fn ns_processes(proc_root: &Path, init_pid: u32) -> io::Result<Vec<u32>> {
    Ok(ns_process_infos(proc_root, init_pid)?
        .into_iter()
        .map(|p| p.host_pid)
        .collect())
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
            if let Ok(o) =
                run_with_timeout(&spec, self.get_state_timeout, Duration::from_millis(20))
                && probe::parse_cuda_state(&o.stdout).is_some()
            {
                out.push(pid);
            }
        }
        Ok(out)
    }

    fn mount_points(&self, pid: u32) -> io::Result<Vec<String>> {
        let mi = fs::read_to_string(self.proc_root.join(pid.to_string()).join("mountinfo"))?;
        Ok(mi
            .lines()
            .filter_map(|l| l.split_whitespace().nth(4))
            .map(crate::mounts::unescape_octal)
            .collect())
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
        // which is the host's). In a thread, so that an NFS server that
        // does not answer only delays the check.
        let mut triggered = false;
        for s in sources {
            if source_problem(s, &tree).is_some() && ends_on_automount(&s.path, &tree) {
                let (tx, rx) = std::sync::mpsc::channel();
                let path = format!("{}/.", s.path);
                std::thread::spawn(move || {
                    let _ = tx.send(fs::metadata(path));
                });
                let _ = rx.recv_timeout(std::time::Duration::from_secs(20));
                triggered = true;
            }
        }
        if triggered {
            tree = read()?;
        }
        Ok(sources
            .iter()
            .filter_map(|s| source_problem(s, &tree))
            .collect())
    }

    fn open_files(&self, host_pids: &[u32]) -> io::Result<Vec<String>> {
        use std::os::unix::fs::MetadataExt;
        let mut out: Vec<String> = Vec::new();
        for pid in host_pids {
            let dir = self.proc_root.join(pid.to_string());
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
                let same = fs::metadata(path)
                    .is_ok_and(|m| m.dev() == meta.dev() && m.ino() == meta.ino());
                if same && !out.iter().any(|p| p == path) {
                    out.push(path.to_owned());
                }
            }
        }
        Ok(out)
    }

    fn dump_hazards(&self, host_pids: &[u32], private: &[String]) -> io::Result<DumpHazards> {
        let mut out = DumpHazards::default();
        let mut sockets = std::collections::HashSet::new();
        let mut socket_fds: Vec<(u32, i32)> = Vec::new();
        for pid in host_pids {
            let dir = self.proc_root.join(pid.to_string());
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
                    continue; // a held file, see `held_files`
                };
                if !path.starts_with('/') || is_private_path(path, private) {
                    continue;
                }
                let size = fs::metadata(&link).map_or(0, |m| m.len());
                if !out.deleted_files.iter().any(|(p, _)| p == path) {
                    out.deleted_files.push((path.to_owned(), size));
                }
            }
        }
        out.held_files = held_files(&self.proc_root, host_pids, private);
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
            let table = fs::read_to_string(self.proc_root.join(pid.to_string()).join("net/unix"))?;
            for (ino, path) in parse_net_unix(&table) {
                if sockets.contains(&ino)
                    && is_host_socket_name(&path, private)
                    && !out.bound_sockets.contains(&path)
                {
                    out.bound_sockets.push(path);
                }
            }
        }
        Ok(out)
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
        }))
        .map_err(io::Error::other)
    }
}

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
        .filter_map(|l| l.split_whitespace().nth(4))
        .any(|mp| crate::mounts::unescape_octal(mp) == want)
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
        let mi = "22 1 0:21 / /run/puzuz/criu\\040root rw - tmpfs tmpfs rw\n";
        assert!(is_mount_point(mi, Path::new("/run/puzuz/criu root")));
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
        let h = host
            .dump_hazards(&[std::process::id()], &["/tmp-private".into()])
            .unwrap();
        let f = f.to_str().unwrap();
        assert!(h.deleted_files.iter().any(|(p, _)| p == f), "{h:?}");
        assert!(
            h.bound_sockets.contains(&sock.to_str().unwrap().to_owned()),
            "{h:?}"
        );
        let tmp_root = tmp.path().parent().unwrap().to_str().unwrap().to_owned();
        let h = host
            .dump_hazards(&[std::process::id()], &[tmp_root])
            .unwrap();
        assert!(!h.deleted_files.iter().any(|(p, _)| p == f), "{h:?}");
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
        let h = host
            .dump_hazards(&[std::process::id()], &["/tmp-private".into()])
            .unwrap();
        // pidfd_getfd needs ptrace access to ourselves, which a sandbox may
        // deny; then nothing is found.
        if !h.reuseport_sockets.is_empty() {
            assert_eq!(h.reuseport_sockets, [format!("127.0.0.1:{port}")], "{h:?}");
        }
        drop((plain, reuse));
        assert!(groups.contains(&nix::unistd::getegid().as_raw()));
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
