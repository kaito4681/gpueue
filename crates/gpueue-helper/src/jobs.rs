//! Per-job files kept by the helper under `<state dir>/jobs/<id>/`.
//!
//! ```text
//! job.json      JobRecord: owner, log path, phase, current unit
//! spawn.json    ShimSpec for the shim: credentials, argv, env, cwd, log
//! start.json    StartReport written by the shim once the job runs
//! exit-status   ExitStatus written by the job's PID-namespace init
//! init-status   ExitStatus of the init, written by the outer shim
//! ```
//!
//! Everything is root-only (directory 0700, files 0600). The shim starts as
//! root and opens `exit-status` and `start.json` before dropping to the
//! job's owner, so the owner cannot forge them.
//!
//! `exit-status` is opened by the init and therefore part of the dumped
//! process tree: CRIU reopens it by path on restore, so it must survive
//! reboots (the state dir is under `/var/lib`) and must exist before a
//! restore.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use gpueue_checkpoint::layout::write_atomic;
use gpueue_protocol::helper::{ExitStatus, Gid, GpuIndex, JobId, Uid};
use serde::{Deserialize, Serialize};

pub const RECORD_FILE: &str = "job.json";
pub const SPEC_FILE: &str = "spawn.json";
pub const START_FILE: &str = "start.json";
pub const EXIT_STATUS_FILE: &str = "exit-status";
pub const INIT_STATUS_FILE: &str = "init-status";

/// What the helper remembers about a job across restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub job: JobId,
    pub uid: Uid,
    pub gid: Gid,
    pub log_path: PathBuf,
    #[serde(default)]
    pub gpus: Vec<GpuIndex>,
    #[serde(default)]
    pub no_checkpoint: bool,
    pub phase: RecordPhase,
    /// How the job's current unit was started; decides where its init is.
    pub unit_kind: UnitKind,
    /// The job's current unit (`gpueue-job-<id>.service`, or
    /// `gpueue-job-<id>-r<n>.service` after the n-th restore). While a
    /// restore is in progress this already names the restore unit.
    pub unit: String,
    /// How many times the job has been restored.
    #[serde(default)]
    pub restores: u32,
    /// Every group id the job ran with when it was dumped (`Gid:` and
    /// `Groups:` of its processes): CRIU restores them, so the owner must
    /// still be in each. Only in the copy kept with the image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dump_groups: Option<Vec<u32>>,
    /// Set (phase `Live`) while `criu dump` may run: a helper that stops
    /// before it recorded the outcome finds out at its next start whether
    /// the dump succeeded (see [`gpueue_checkpoint::layout::CheckpointDir::is_unfinalized_dump`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dumping: bool,
    /// Set (phase `Checkpointed`) while a restore runs: a helper that stops
    /// before it recorded the outcome reports the restore as failed at its
    /// next start.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restoring: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordPhase {
    /// A unit exists (or existed) for the job.
    Live,
    /// Dumped; the image is in the checkpoint directory and no unit exists.
    Checkpointed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// Started by `spawn`: the unit's main process is the outer shim and
    /// the job's init is its only child.
    Spawned,
    /// Started by `restore`: the unit's main process is the restored init.
    Restored,
}

/// Which mounts the shim removes or replaces in the job's mount namespace,
/// so that it only contains mounts that also exist after a reboot (CRIU
/// recreates the namespace on restore and fails on missing mount points or
/// bind sources).
///
/// `/mnt` is not pruned: administrators mount persistent filesystems there
/// (`/mnt/data`), and a job whose cwd is below a pruned mount would write
/// to the directory underneath it on the root filesystem. A restore checks
/// that such filesystems are mounted again (see
/// [`crate::mounts::SourceMount`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountPlan {
    /// Detach every mount at or below these paths. A trailing `/` means
    /// "strictly below" (keep the path itself).
    pub prune: Vec<String>,
    /// Exceptions to `prune` (same syntax).
    pub keep: Vec<String>,
    /// Mount a private tmpfs here (its contents are saved in the image).
    pub tmpfs: Vec<String>,
    /// Hide each of these directories behind an empty, root-owned tmpfs,
    /// after pruning: by default the system D-Bus directory under both of
    /// its names (`/run/dbus`, `/var/run/dbus`; left out with
    /// `job_system_dbus = true`) and the entries of
    /// `job_hidden_socket_dirs` (`/run/libvirt`, `/run/cups`), so that a
    /// job cannot reach the sockets there, also when they appear only
    /// later (the host's directory is not visible in the job at all). A
    /// missing directory is created first (as root, `0755`) if its parent
    /// exists; two names of the same directory (`/var/run` is usually a
    /// link to `/run`) are hidden once. CRIU saves the tmpfs; the helper
    /// creates missing mount points before a restore (see
    /// [`ensure_restore_mount_points`]).
    #[serde(default)]
    pub hidden_dirs: Vec<String>,
    /// `Some`: hide `/run/systemd` behind an empty, root-owned tmpfs that
    /// holds only these entries of the host's (relative paths, bound from
    /// the host where they exist) and an empty `system` directory
    /// (`sd_booted()`), so that a job cannot reach systemd's Varlink
    /// services (`io.systemd.Manager`, `io.systemd.Credentials`, mountfsd,
    /// nsresourced, machined, logind, networkd, ...). They authorize
    /// unprivileged callers through polkit, which takes group membership
    /// from the user database, and they do not go through the D-Bus
    /// socket. Sockets the host creates later in `/run/systemd` (or in
    /// `journal/` and `resolve/`, of which only single files are bound)
    /// do not show up in the job. `None` with `job_systemd_varlink = true`.
    #[serde(default)]
    pub systemd_run_keep: Option<Vec<String>>,
}

/// What [`MountPlan::systemd_run_keep`] keeps by default: journald's log
/// sockets (`/dev/log` links to `journal/dev-log`), resolved's
/// resolution socket and the `resolv.conf` files `/etc/resolv.conf` may
/// link to (not its query monitor), and the user database directory,
/// whose services only answer user and group lookups
/// (`io.systemd.UserDatabase`).
pub const SYSTEMD_RUN_KEEP: [&str; 7] = [
    "journal/socket",
    "journal/stdout",
    "journal/dev-log",
    "resolve/io.systemd.Resolve",
    "resolve/stub-resolv.conf",
    "resolve/resolv.conf",
    "userdb",
];

/// The system D-Bus directory under both names (see
/// [`MountPlan::hidden_dirs`]).
pub const SYSTEM_BUS_DIRS: [&str; 2] = ["/run/dbus", "/var/run/dbus"];

/// Create the mount points a restore of a job hidden with `plan` needs
/// on the host: the directories of [`MountPlan::hidden_dirs`] (the shim
/// may have created them, and `/run` is empty after a reboot). Only where
/// the parent exists; errors are left to the restore.
pub fn ensure_restore_mount_points(plan: &MountPlan) {
    for dir in &plan.hidden_dirs {
        let path = std::path::Path::new(dir);
        if path.parent().is_some_and(|p| p.is_dir()) && !path.exists() {
            let _ = std::fs::DirBuilder::new().mode(0o755).create(path);
        }
    }
}

impl MountPlan {
    /// Whether the mount point `m` is detached.
    pub fn prunes(&self, m: &str) -> bool {
        use crate::mounts::path_under;
        self.prune.iter().any(|p| path_under(m, p)) && !self.keep.iter().any(|k| path_under(m, k))
    }
}

impl Default for MountPlan {
    fn default() -> Self {
        Self {
            prune: vec!["/run/".into(), "/media/".into(), "/dev/shm".into()],
            keep: vec!["/run/nvidia-persistenced".into()],
            tmpfs: vec!["/tmp".into(), "/dev/shm".into()],
            hidden_dirs: SYSTEM_BUS_DIRS
                .iter()
                .chain(gpueue_config::job_policy::DEFAULT_JOB_HIDDEN_SOCKET_DIRS)
                .map(|p| (*p).to_owned())
                .collect(),
            systemd_run_keep: Some(SYSTEMD_RUN_KEEP.iter().map(|p| (*p).to_owned()).collect()),
        }
    }
}

/// Input of the shim, written by the helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShimSpec {
    /// The job's owner; the shim refuses 0.
    pub uid: u32,
    pub gid: u32,
    /// Supplementary groups; `None` means `initgroups` for `uid`.
    pub groups: Option<Vec<u32>>,
    #[serde(default)]
    pub mounts: MountPlan,
    pub argv: Vec<String>,
    pub envs: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub log_path: PathBuf,
    /// Mode of the log file (0660 in the helper-managed log directory so
    /// the daemon can truncate it; 0640 elsewhere).
    #[serde(default = "default_log_mode")]
    pub log_mode: u32,
}

fn default_log_mode() -> u32 {
    0o640
}

/// Written by the shim once the job has been started (or failed to).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum StartReport {
    Started {
        /// The job's PID inside its namespace.
        ns_pid: u32,
        /// The init's PID in the host namespace (read before `/proc` is
        /// remounted). Only used when the job is already over by the time
        /// the helper looks for the init.
        #[serde(default)]
        init_host_pid: u32,
    },
    Failed {
        kind: StartFailure,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartFailure {
    /// The owner may not use the cwd or log file.
    PermissionDenied,
    /// Anything else (namespace setup, exec, ...).
    Other,
}

/// The files of one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFiles {
    dir: PathBuf,
}

impl JobFiles {
    pub fn new(jobs_dir: &Path, job: JobId) -> Self {
        Self {
            dir: jobs_dir.join(job.0.to_string()),
        }
    }

    pub fn from_dir(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
    pub fn record(&self) -> PathBuf {
        self.dir.join(RECORD_FILE)
    }
    pub fn spec(&self) -> PathBuf {
        self.dir.join(SPEC_FILE)
    }
    pub fn start(&self) -> PathBuf {
        self.dir.join(START_FILE)
    }
    pub fn exit_status(&self) -> PathBuf {
        self.dir.join(EXIT_STATUS_FILE)
    }
    pub fn init_status(&self) -> PathBuf {
        self.dir.join(INIT_STATUS_FILE)
    }

    /// Create the directory and the status files (empty, root-only).
    /// Existing files are truncated, except `exit-status` when
    /// `keep_exit_status` (a restore must reopen the very file the init had
    /// open).
    pub fn prepare(&self, keep_exit_status: bool) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        for path in [self.start(), self.init_status(), self.exit_status()] {
            let keep = keep_exit_status && path == self.exit_status();
            let f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(!keep)
                .mode(0o600)
                .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
                .open(&path)?;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    /// Write the shim's input (root-only: it holds the job's environment).
    pub fn write_spec(&self, spec: &ShimSpec) -> io::Result<()> {
        let path = self.spec();
        let _ = fs::remove_file(&path);
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(&serde_json::to_vec(spec)?)?;
        f.sync_all()?;
        Ok(())
    }

    pub fn write_record(&self, record: &JobRecord) -> io::Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        write_atomic(&self.record(), &serde_json::to_vec_pretty(record)?)
    }

    pub fn read_record(&self) -> io::Result<JobRecord> {
        let bytes = fs::read(self.record())?;
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// The init's report of the job's exit, if written.
    pub fn read_exit_status(&self) -> Option<ExitStatus> {
        read_json(&self.exit_status())
    }

    /// The outer shim's report of how the init ended, if written.
    pub fn read_init_status(&self) -> Option<ExitStatus> {
        read_json(&self.init_status())
    }

    pub fn read_start(&self) -> Option<StartReport> {
        read_json(&self.start())
    }

    pub fn remove(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Parse a small JSON file; `None` when missing, empty or partial.
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Every job directory under `jobs_dir` with a readable record.
pub fn scan(jobs_dir: &Path) -> io::Result<Vec<(JobFiles, JobRecord)>> {
    let entries = match fs::read_dir(jobs_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        let files = JobFiles::new(jobs_dir, JobId(id));
        match files.read_record() {
            Ok(rec) if rec.job == JobId(id) => out.push((files, rec)),
            // A directory without a record is a spawn that never completed.
            _ => {
                let _ = files.remove();
            }
        }
    }
    out.sort_by_key(|(_, r)| r.job);
    Ok(out)
}

/// Write `status` as the content of an already-open status file
/// (truncating it first).
pub fn write_status_to(file: &mut fs::File, status: &ExitStatus) -> io::Result<()> {
    use std::io::Seek;
    file.set_len(0)?;
    file.seek(io::SeekFrom::Start(0))?;
    file.write_all(&serde_json::to_vec(status)?)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn me() -> (Uid, Gid) {
        (
            Uid(nix::unistd::getuid().as_raw()),
            Gid(nix::unistd::getgid().as_raw()),
        )
    }

    #[test]
    fn prepare_record_and_status_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        let files = JobFiles::new(tmp.path(), JobId(5));
        files.prepare(false).unwrap();
        assert_eq!(files.read_exit_status(), None);
        let rec = JobRecord {
            job: JobId(5),
            uid,
            gid,
            log_path: "/tmp/x.log".into(),
            gpus: vec![GpuIndex(0)],
            no_checkpoint: false,
            phase: RecordPhase::Live,
            unit_kind: UnitKind::Spawned,
            unit: "gpueue-job-5.service".into(),
            restores: 0,
            dump_groups: None,
            dumping: false,
            restoring: false,
        };
        files.write_record(&rec).unwrap();

        let mut f = OpenOptions::new()
            .write(true)
            .open(files.exit_status())
            .unwrap();
        write_status_to(&mut f, &ExitStatus::Exited { code: 3 }).unwrap();
        assert_eq!(
            files.read_exit_status(),
            Some(ExitStatus::Exited { code: 3 })
        );
        // keep_exit_status leaves it alone; otherwise it is truncated.
        files.prepare(true).unwrap();
        assert!(files.read_exit_status().is_some());
        files.prepare(false).unwrap();
        assert!(files.read_exit_status().is_none());

        let found = scan(tmp.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, rec);
    }
}
