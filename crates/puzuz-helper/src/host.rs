//! Everything the helper reads from the host besides systemd: processes,
//! GPUs, users, filesystems. Behind [`HostProbe`] so tests can fake it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use puzuz_checkpoint::command::{CudaCheckpointOptions, cuda_get_state};
use puzuz_checkpoint::exec::run_with_timeout;
use puzuz_checkpoint::{CommandSpec, probe, procfs, space};

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
}

impl<T: HostProbe> HostProbe for std::sync::Arc<T> {
    fn job_processes(&self, init_pid: u32) -> io::Result<Vec<ProcInfo>> {
        (**self).job_processes(init_pid)
    }
    fn gpu_snapshot(&self) -> io::Result<GpuSnapshot> {
        (**self).gpu_snapshot()
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
}

impl SystemHost {
    pub fn new(nvidia_smi: PathBuf, cuda_checkpoint: CudaCheckpointOptions) -> Self {
        Self {
            proc_root: PathBuf::from("/proc"),
            nvidia_smi,
            nvidia_smi_timeout: Duration::from_secs(30),
            cuda_checkpoint,
            get_state_timeout: Duration::from_secs(10),
        }
    }

    fn nvidia_query(&self, args: &[&str]) -> io::Result<String> {
        let mut spec = CommandSpec::new(&self.nvidia_smi);
        spec.args(args);
        run_with_timeout(&spec, self.nvidia_smi_timeout, Duration::from_millis(20))
            .map(|o| o.stdout)
            .map_err(|e| io::Error::other(format!("{spec}: {e}")))
    }
}

impl HostProbe for SystemHost {
    fn job_processes(&self, init_pid: u32) -> io::Result<Vec<ProcInfo>> {
        let ns_of = |pid: &str| fs::read_link(self.proc_root.join(pid).join("ns/pid"));
        let target = ns_of(&init_pid.to_string())?;
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.proc_root)? {
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
        use crate::mounts::{MountTree, unreachable_sources};
        let job = fs::read_to_string(self.proc_root.join(pid.to_string()).join("mountinfo"))?;
        let own = fs::read_to_string(self.proc_root.join("self/mountinfo"))?;
        Ok(unreachable_sources(
            &MountTree::parse(&job),
            &MountTree::parse(&own),
        ))
    }
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

    #[test]
    fn parses_proc_fields() {
        assert_eq!(parse_vm_rss("Name:\tx\nVmRSS:\t  2048 kB\n"), Some(2 << 20));
        assert_eq!(parse_stat_ppid("42 (a b) c) S 7 42 42 0"), Some(7));
        let mi = "22 1 0:21 / /run/puzuz/criu\\040root rw - tmpfs tmpfs rw\n";
        assert!(is_mount_point(mi, Path::new("/run/puzuz/criu root")));
        assert!(!is_mount_point(mi, Path::new("/run")));
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
