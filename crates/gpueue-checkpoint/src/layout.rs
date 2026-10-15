//! On-disk layout of a job's checkpoint directory.
//!
//! ```text
//! <base>/<job id>/
//!     images/        CRIU image files (criu -D)
//!     dump.log       criu dump log
//!     restore.log    criu restore log
//!     restore.pid    host PID of the restored tree root (criu --pidfile)
//!     meta.json      CheckpointMeta (a draft before the dump, final with
//!                    the size once the dump succeeded)
//!     DUMPED         written as soon as `criu dump` succeeded
//!     COMPLETE       completion marker, written last
//! <base>/failed-logs/
//!     <job id>-<unix ms>.log   dump.log of a failed dump (newest kept)
//! ```
//!
//! Both `meta.json` and `COMPLETE` are written via write-to-temp + fsync +
//! rename + directory fsync, so a crash at any point leaves either a complete
//! directory or one without the marker, which is incomplete. An incomplete
//! directory whose dump succeeded ([`CheckpointDir::is_unfinalized_dump`])
//! holds the only copy of the job's state (its processes are gone) and is
//! finalized again rather than removed; any other incomplete directory is
//! cleaned up on start.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::command::CudaMode;

pub const IMAGES_SUBDIR: &str = "images";
pub const META_FILE: &str = "meta.json";
pub const COMPLETE_MARKER: &str = "COMPLETE";
pub const DUMP_LOG: &str = "dump.log";
pub const RESTORE_LOG: &str = "restore.log";
pub const RESTORE_PIDFILE: &str = "restore.pid";
/// File CRIU always writes into a successful image set.
pub const CRIU_INVENTORY: &str = "inventory.img";
/// Marker written right after `criu dump` succeeded (see
/// [`CheckpointDir::is_unfinalized_dump`]).
pub const DUMPED_MARKER: &str = "DUMPED";
/// What CRIU logs (at `-v3` and above) when a dump succeeded.
pub const CRIU_DUMP_SUCCESS: &str = "Dumping finished successfully";

/// Directory under the checkpoint base that keeps the CRIU logs of failed
/// dumps (the checkpoint directory itself is removed).
pub const FAILED_LOGS_SUBDIR: &str = "failed-logs";
/// How many failed dump logs [`preserve_failed_log`] keeps.
pub const FAILED_LOGS_KEEP: usize = 20;

/// Current `meta.json` format version.
pub const META_FORMAT_VERSION: u32 = 1;

/// A GPU the job was using when it was dumped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuMeta {
    /// Index in `CUDA_DEVICE_ORDER=PCI_BUS_ID` order.
    pub index: u32,
    pub uuid: String,
    pub model: String,
}

/// Contents of `meta.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointMeta {
    pub format_version: u32,
    pub job_id: u64,
    /// Unix epoch milliseconds.
    pub created_at: i64,
    pub mode: CudaMode,
    #[serde(default)]
    pub gpus: Vec<GpuMeta>,
    #[serde(default)]
    pub driver_version: Option<String>,
    #[serde(default)]
    pub criu_version: Option<String>,
    #[serde(default)]
    pub cuda_checkpoint_version: Option<String>,
    /// PIDs that used CUDA, as seen *inside* the job's PID namespace. Host
    /// PIDs change on restore; these are mapped back with
    /// [`crate::procfs::match_ns_pids`].
    #[serde(default)]
    pub cuda_ns_pids: Vec<u32>,
    /// GPU memory in use at dump time; restore needs at least this much free.
    #[serde(default)]
    pub vram_bytes: u64,
    /// Total size of the checkpoint directory, filled in by
    /// [`CheckpointDir::finalize`].
    #[serde(default)]
    pub size_bytes: u64,
}

impl CheckpointMeta {
    pub fn new(job_id: u64, created_at: i64, mode: CudaMode) -> Self {
        Self {
            format_version: META_FORMAT_VERSION,
            job_id,
            created_at,
            mode,
            gpus: Vec::new(),
            driver_version: None,
            criu_version: None,
            cuda_checkpoint_version: None,
            cuda_ns_pids: Vec::new(),
            vram_bytes: 0,
            size_bytes: 0,
        }
    }
}

/// A reason a stored checkpoint cannot be restored on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Incompatibility {
    GpuCount {
        saved: usize,
        current: usize,
    },
    GpuModel {
        index: u32,
        saved: String,
        current: Option<String>,
    },
    GpuUuid {
        index: u32,
        saved: String,
        current: Option<String>,
    },
    DriverVersion {
        saved: String,
        current: Option<String>,
    },
}

/// Compare the saved environment against the current one. Restore requires
/// the same GPU indices, models and driver version. A UUID change is only
/// reported when `require_same_uuid` is set (CRIU 4.2.1's plugin requires
/// it; manual mode can remap with `--device-map`).
pub fn check_compatibility(
    meta: &CheckpointMeta,
    current_gpus: &[GpuMeta],
    current_driver: Option<&str>,
    require_same_uuid: bool,
) -> Vec<Incompatibility> {
    let mut out = Vec::new();
    if meta.gpus.len() != current_gpus.len() {
        out.push(Incompatibility::GpuCount {
            saved: meta.gpus.len(),
            current: current_gpus.len(),
        });
    }
    for saved in &meta.gpus {
        let cur = current_gpus.iter().find(|g| g.index == saved.index);
        if cur.map(|g| &g.model) != Some(&saved.model) {
            out.push(Incompatibility::GpuModel {
                index: saved.index,
                saved: saved.model.clone(),
                current: cur.map(|g| g.model.clone()),
            });
        }
        if require_same_uuid && cur.map(|g| &g.uuid) != Some(&saved.uuid) {
            out.push(Incompatibility::GpuUuid {
                index: saved.index,
                saved: saved.uuid.clone(),
                current: cur.map(|g| g.uuid.clone()),
            });
        }
    }
    if let Some(saved) = &meta.driver_version
        && current_driver != Some(saved.as_str())
    {
        out.push(Incompatibility::DriverVersion {
            saved: saved.clone(),
            current: current_driver.map(str::to_owned),
        });
    }
    out
}

/// Why a directory is not a complete checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncompleteReason {
    NoMarker,
    NoMeta,
    BadMeta(String),
    JobIdMismatch { expected: u64, found: u64 },
    NoImages,
}

/// Result of validating a checkpoint directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirStatus {
    Missing,
    Incomplete(IncompleteReason),
    Complete(Box<CheckpointMeta>),
}

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("checkpoint directory {0} already holds a complete checkpoint")]
    AlreadyComplete(PathBuf),
    #[error(
        "checkpoint directory {0} holds a successful dump that is not finalized yet; \
         restore or delete it first"
    )]
    Unfinalized(PathBuf),
    #[error("meta.json job id {found} does not match directory job id {expected}")]
    JobIdMismatch { expected: u64, found: u64 },
    #[error("invalid meta.json: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Paths and operations for one job's checkpoint directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckpointDir {
    root: PathBuf,
    job_id: u64,
}

impl CheckpointDir {
    /// `<base>/<job_id>`.
    pub fn for_job(base: &Path, job_id: u64) -> Self {
        Self {
            root: base.join(job_id.to_string()),
            job_id,
        }
    }

    pub fn job_id(&self) -> u64 {
        self.job_id
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn images_dir(&self) -> PathBuf {
        self.root.join(IMAGES_SUBDIR)
    }
    pub fn meta_path(&self) -> PathBuf {
        self.root.join(META_FILE)
    }
    pub fn marker_path(&self) -> PathBuf {
        self.root.join(COMPLETE_MARKER)
    }
    pub fn dump_log(&self) -> PathBuf {
        self.root.join(DUMP_LOG)
    }
    pub fn restore_log(&self) -> PathBuf {
        self.root.join(RESTORE_LOG)
    }
    pub fn restore_pidfile(&self) -> PathBuf {
        self.root.join(RESTORE_PIDFILE)
    }
    pub fn dumped_marker_path(&self) -> PathBuf {
        self.root.join(DUMPED_MARKER)
    }

    /// Record that `criu dump` succeeded (before anything else is written
    /// after it).
    pub fn mark_dumped(&self) -> io::Result<()> {
        write_atomic(&self.dumped_marker_path(), b"criu dump succeeded\n")
    }

    /// Whether the directory holds a dump that succeeded but was not
    /// finalized (no `COMPLETE` marker): the job's processes are gone, so
    /// the image must be kept and finalized again. That is the case when
    /// CRIU's `inventory.img` and the draft `meta.json` exist and either
    /// the [`DUMPED_MARKER`] does (written right after CRIU exited
    /// successfully) or CRIU's log says the dump finished successfully (a
    /// helper that stopped before it wrote the marker).
    pub fn is_unfinalized_dump(&self) -> bool {
        if !matches!(self.status(), DirStatus::Incomplete(_)) {
            return false;
        }
        if !self.images_dir().join(CRIU_INVENTORY).is_file()
            || !self.read_meta().is_ok_and(|m| m.job_id == self.job_id)
        {
            return false;
        }
        self.dumped_marker_path().is_file()
            || fs::read_to_string(self.dump_log()).is_ok_and(|l| l.contains(CRIU_DUMP_SUCCESS))
    }

    /// Create a fresh, empty directory (mode 0700) for a new dump. Refuses to
    /// touch a complete checkpoint or an unfinalized successful dump
    /// ([`Self::is_unfinalized_dump`]); anything else incomplete is wiped
    /// first.
    pub fn prepare(&self) -> Result<(), LayoutError> {
        match self.status() {
            DirStatus::Complete(_) => return Err(LayoutError::AlreadyComplete(self.root.clone())),
            DirStatus::Incomplete(_) if self.is_unfinalized_dump() => {
                return Err(LayoutError::Unfinalized(self.root.clone()));
            }
            DirStatus::Incomplete(_) => fs::remove_dir_all(&self.root)?,
            DirStatus::Missing => {}
        }
        let mut b = fs::DirBuilder::new();
        b.recursive(true).mode(0o700);
        b.create(&self.root)?;
        b.create(self.images_dir())?;
        Ok(())
    }

    /// Validate the directory.
    pub fn status(&self) -> DirStatus {
        if !self.root.is_dir() {
            return DirStatus::Missing;
        }
        if !self.marker_path().is_file() {
            return DirStatus::Incomplete(IncompleteReason::NoMarker);
        }
        let meta = match fs::read(self.meta_path()) {
            Ok(b) => b,
            Err(_) => return DirStatus::Incomplete(IncompleteReason::NoMeta),
        };
        let meta: CheckpointMeta = match serde_json::from_slice(&meta) {
            Ok(m) => m,
            Err(e) => return DirStatus::Incomplete(IncompleteReason::BadMeta(e.to_string())),
        };
        if meta.job_id != self.job_id {
            return DirStatus::Incomplete(IncompleteReason::JobIdMismatch {
                expected: self.job_id,
                found: meta.job_id,
            });
        }
        if !self.images_dir().join(CRIU_INVENTORY).is_file() {
            return DirStatus::Incomplete(IncompleteReason::NoImages);
        }
        DirStatus::Complete(Box::new(meta))
    }

    /// Read `meta.json` regardless of the marker.
    pub fn read_meta(&self) -> Result<CheckpointMeta, LayoutError> {
        Ok(serde_json::from_slice(&fs::read(self.meta_path())?)?)
    }

    /// Mark a successful dump complete: compute the directory size, write
    /// `meta.json`, then write the `COMPLETE` marker last. Returns the meta
    /// as written.
    pub fn finalize(&self, mut meta: CheckpointMeta) -> Result<CheckpointMeta, LayoutError> {
        if meta.job_id != self.job_id {
            return Err(LayoutError::JobIdMismatch {
                expected: self.job_id,
                found: meta.job_id,
            });
        }
        // A stale marker must never vouch for new contents.
        remove_if_exists(&self.marker_path())?;
        meta.size_bytes = self.size_bytes()?;
        let json = serde_json::to_vec_pretty(&meta)?;
        write_atomic(&self.meta_path(), &json)?;
        write_atomic(
            &self.marker_path(),
            format!("gpueue-checkpoint {META_FORMAT_VERSION}\n").as_bytes(),
        )?;
        Ok(meta)
    }

    /// Total size in bytes of all regular files under the directory.
    pub fn size_bytes(&self) -> io::Result<u64> {
        dir_size(&self.root)
    }

    /// Delete the checkpoint. The marker goes first so that a crash during
    /// removal leaves an incomplete directory, never a "complete" partial one.
    /// The restore log and pidfile go last, so that a removal that stops
    /// half-way still shows that a job was restored from the image (which
    /// makes the rest of it safe to remove later).
    pub fn remove(&self) -> io::Result<()> {
        self.remove_with(|path| {
            if fs::symlink_metadata(path)?.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            }
        })
    }

    /// [`Self::remove`], deleting each entry of the directory with
    /// `remove_entry`.
    fn remove_with(&self, mut remove_entry: impl FnMut(&Path) -> io::Result<()>) -> io::Result<()> {
        if !self.root.exists() {
            return Ok(());
        }
        remove_if_exists(&self.marker_path())?;
        fsync_dir(&self.root)?;
        let last = [self.restore_log(), self.restore_pidfile()];
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if !last.contains(&path) {
                remove_entry(&path)?;
            }
        }
        for path in &last {
            if fs::symlink_metadata(path).is_ok() {
                remove_entry(path)?;
            }
        }
        fs::remove_dir(&self.root)
    }
}

/// List `(job id, status)` for every `<base>/<number>` directory. Other
/// entries are ignored. A missing base yields an empty list.
pub fn scan(base: &Path) -> io::Result<Vec<(u64, DirStatus)>> {
    let entries = match fs::read_dir(base) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u64>().ok())
        else {
            continue;
        };
        out.push((id, CheckpointDir::for_job(base, id).status()));
    }
    out.sort_by_key(|(id, _)| *id);
    Ok(out)
}

/// Sum of regular-file sizes under `path`, without following symlinks.
pub fn dir_size(path: &Path) -> io::Result<u64> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_file() {
        return Ok(meta.len());
    }
    if !meta.is_dir() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            total = total.saturating_add(dir_size(&entry.path())?);
        } else if ft.is_file() {
            total = total.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(total)
}

/// Move the dump log of a failed dump of `dir` to
/// `<base>/failed-logs/<job>-<created_at>.log` (directory mode 0700, so
/// only root can read it), then delete the oldest logs there beyond
/// `keep`. Returns the new path, or `None` when there was no log.
pub fn preserve_failed_log(
    base: &Path,
    dir: &CheckpointDir,
    created_at: i64,
    keep: usize,
) -> io::Result<Option<PathBuf>> {
    let log = dir.dump_log();
    if !log.exists() {
        return Ok(None);
    }
    let logs = base.join(FAILED_LOGS_SUBDIR);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&logs)?;
    let mut dest = logs.join(format!("{}-{created_at}.log", dir.job_id()));
    let mut n = 1;
    while dest.exists() {
        dest = logs.join(format!("{}-{created_at}.{n}.log", dir.job_id()));
        n += 1;
    }
    fs::rename(&log, &dest)?;
    prune_failed_logs(&logs, keep)?;
    Ok(Some(dest))
}

/// Delete the oldest `<job>-<ms>[.<n>].log` files in `logs` beyond `keep`.
fn prune_failed_logs(logs: &Path, keep: usize) -> io::Result<()> {
    let mut found: Vec<(i64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(logs)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(stamp) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".log"))
            .and_then(|n| n.split_once('-'))
            .and_then(|(_, rest)| rest.split('.').next())
            .and_then(|ms| ms.parse::<i64>().ok())
        else {
            continue;
        };
        found.push((stamp, entry.path()));
    }
    if found.len() <= keep {
        return Ok(());
    }
    found.sort();
    let excess = found.len() - keep;
    for (_, path) in found.into_iter().take(excess) {
        remove_if_exists(&path)?;
    }
    Ok(())
}

/// Write `data` to `path` atomically: `<path>.tmp` → fsync → rename → fsync
/// of the parent directory.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let mut tmp_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_os_string();
    tmp_name.push(".tmp");
    let tmp = parent.join(tmp_name);
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    fsync_dir(parent)
}

fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TempDir;

    fn fake_dump(dir: &CheckpointDir) {
        fs::write(dir.images_dir().join(CRIU_INVENTORY), b"inv").unwrap();
        fs::write(dir.images_dir().join("pages-1.img"), vec![0u8; 1000]).unwrap();
        fs::write(dir.dump_log(), b"log").unwrap();
    }

    #[test]
    fn failed_logs_are_moved_and_pruned() {
        let tmp = TempDir::new();
        let base = tmp.path();
        let dir = CheckpointDir::for_job(base, 7);
        // No log: nothing to keep.
        assert_eq!(preserve_failed_log(base, &dir, 1, 3).unwrap(), None);

        for stamp in [10, 20, 30, 40] {
            dir.prepare().unwrap();
            fs::write(dir.dump_log(), format!("log {stamp}")).unwrap();
            let kept = preserve_failed_log(base, &dir, stamp, 3).unwrap().unwrap();
            assert_eq!(
                kept,
                base.join(FAILED_LOGS_SUBDIR).join(format!("7-{stamp}.log"))
            );
            assert_eq!(fs::read_to_string(&kept).unwrap(), format!("log {stamp}"));
            assert!(!dir.dump_log().exists());
        }
        // The same timestamp twice does not overwrite.
        fs::write(dir.dump_log(), "again").unwrap();
        let again = preserve_failed_log(base, &dir, 40, 3).unwrap().unwrap();
        assert_eq!(again, base.join(FAILED_LOGS_SUBDIR).join("7-40.1.log"));

        let mut left: Vec<String> = fs::read_dir(base.join(FAILED_LOGS_SUBDIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["7-30.log", "7-40.1.log", "7-40.log"]);
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(base.join(FAILED_LOGS_SUBDIR))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        // The log directory is not mistaken for a checkpoint.
        assert!(scan(base).unwrap().iter().all(|(id, _)| *id == 7));
    }

    fn meta(job: u64) -> CheckpointMeta {
        let mut m = CheckpointMeta::new(job, 1_760_000_000_000, CudaMode::Manual);
        m.gpus = vec![GpuMeta {
            index: 0,
            uuid: "GPU-1".into(),
            model: "RTX 5070 Ti".into(),
        }];
        m.driver_version = Some("595.91.07".into());
        m.criu_version = Some("4.2.1".into());
        m.cuda_checkpoint_version = Some("595.91.07".into());
        m.cuda_ns_pids = vec![2];
        m.vram_bytes = 1 << 30;
        m
    }

    #[test]
    fn paths() {
        let d = CheckpointDir::for_job(Path::new("/var/lib/gpueue/checkpoints"), 12);
        assert_eq!(d.root(), Path::new("/var/lib/gpueue/checkpoints/12"));
        assert_eq!(
            d.images_dir(),
            Path::new("/var/lib/gpueue/checkpoints/12/images")
        );
        assert_eq!(
            d.meta_path(),
            Path::new("/var/lib/gpueue/checkpoints/12/meta.json")
        );
        assert_eq!(
            d.marker_path(),
            Path::new("/var/lib/gpueue/checkpoints/12/COMPLETE")
        );
        assert_eq!(
            d.dump_log(),
            Path::new("/var/lib/gpueue/checkpoints/12/dump.log")
        );
        assert_eq!(
            d.restore_log(),
            Path::new("/var/lib/gpueue/checkpoints/12/restore.log")
        );
        assert_eq!(
            d.restore_pidfile(),
            Path::new("/var/lib/gpueue/checkpoints/12/restore.pid")
        );
        assert_eq!(d.job_id(), 12);
    }

    #[test]
    fn lifecycle_missing_incomplete_complete_removed() {
        let tmp = TempDir::new();
        let d = CheckpointDir::for_job(tmp.path(), 3);
        assert_eq!(d.status(), DirStatus::Missing);

        d.prepare().unwrap();
        assert!(d.images_dir().is_dir());
        assert_eq!(
            d.status(),
            DirStatus::Incomplete(IncompleteReason::NoMarker)
        );

        fake_dump(&d);
        let written = d.finalize(meta(3)).unwrap();
        assert_eq!(written.size_bytes, 1006);
        match d.status() {
            DirStatus::Complete(m) => assert_eq!(*m, written),
            other => panic!("unexpected {other:?}"),
        }
        assert!(!d.root().join("meta.json.tmp").exists());
        assert!(!d.root().join("COMPLETE.tmp").exists());

        assert!(matches!(d.prepare(), Err(LayoutError::AlreadyComplete(_))));

        d.remove().unwrap();
        assert_eq!(d.status(), DirStatus::Missing);
        d.remove().unwrap();
    }

    /// A removal that stops half-way (here at `images/`, whatever order the
    /// directory lists its entries in) keeps the restore log and pidfile,
    /// which show that a job was restored from the image; they go last.
    #[test]
    fn a_removal_that_stops_half_way_keeps_the_restore_evidence() {
        let tmp = TempDir::new();
        let d = CheckpointDir::for_job(tmp.path(), 3);
        d.prepare().unwrap();
        fake_dump(&d);
        d.finalize(meta(3)).unwrap();
        fs::write(d.restore_log(), "(00.1) Restore finished successfully\n").unwrap();
        fs::write(d.restore_pidfile(), "10001\n").unwrap();

        let images = d.images_dir();
        let mut removed = Vec::new();
        let err = d
            .remove_with(|path| {
                if path == images {
                    return Err(io::Error::from(io::ErrorKind::PermissionDenied));
                }
                removed.push(path.to_owned());
                fs::remove_file(path)
            })
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(!d.marker_path().exists());
        assert!(d.restore_log().exists(), "removed: {removed:?}");
        assert!(d.restore_pidfile().exists(), "removed: {removed:?}");

        // Once it can be removed, everything goes, the evidence last.
        let mut order = Vec::new();
        d.remove_with(|path| {
            order.push(path.file_name().unwrap().to_owned());
            if fs::symlink_metadata(path)?.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            }
        })
        .unwrap();
        assert_eq!(d.status(), DirStatus::Missing);
        assert_eq!(order[order.len() - 2..], [RESTORE_LOG, RESTORE_PIDFILE]);
    }

    #[test]
    fn size_counts_files_before_meta() {
        let tmp = TempDir::new();
        let d = CheckpointDir::for_job(tmp.path(), 1);
        d.prepare().unwrap();
        fake_dump(&d);
        let m = d.finalize(meta(1)).unwrap();
        // inventory (3) + pages (1000) + dump.log (3); meta and marker are
        // written after the size is taken.
        assert_eq!(m.size_bytes, 1006);
        assert!(d.size_bytes().unwrap() > 1006);
    }

    #[test]
    fn incomplete_reasons() {
        let tmp = TempDir::new();
        let d = CheckpointDir::for_job(tmp.path(), 5);
        d.prepare().unwrap();

        fs::write(d.marker_path(), b"x").unwrap();
        assert_eq!(d.status(), DirStatus::Incomplete(IncompleteReason::NoMeta));

        fs::write(d.meta_path(), b"{not json").unwrap();
        assert!(matches!(
            d.status(),
            DirStatus::Incomplete(IncompleteReason::BadMeta(_))
        ));

        fs::write(d.meta_path(), serde_json::to_vec(&meta(6)).unwrap()).unwrap();
        assert_eq!(
            d.status(),
            DirStatus::Incomplete(IncompleteReason::JobIdMismatch {
                expected: 5,
                found: 6
            })
        );

        fs::write(d.meta_path(), serde_json::to_vec(&meta(5)).unwrap()).unwrap();
        assert_eq!(
            d.status(),
            DirStatus::Incomplete(IncompleteReason::NoImages)
        );

        fake_dump(&d);
        assert!(matches!(d.status(), DirStatus::Complete(_)));
    }

    #[test]
    fn unfinalized_dumps_are_recognized_and_kept() {
        let tmp = TempDir::new();
        let d = CheckpointDir::for_job(tmp.path(), 4);
        d.prepare().unwrap();
        // A draft meta and CRIU's images, but no sign the dump succeeded:
        // an ordinary incomplete directory.
        fs::write(d.meta_path(), serde_json::to_vec(&meta(4)).unwrap()).unwrap();
        fake_dump(&d);
        assert!(!d.is_unfinalized_dump());
        // CRIU says it succeeded (a helper that stopped before the marker).
        fs::write(
            d.dump_log(),
            "(00.1) Writing stats\n(00.2) Dumping finished successfully\n",
        )
        .unwrap();
        assert!(d.is_unfinalized_dump());
        assert!(matches!(d.prepare(), Err(LayoutError::Unfinalized(_))));
        // The marker alone is enough too.
        fs::write(d.dump_log(), "log").unwrap();
        assert!(!d.is_unfinalized_dump());
        d.mark_dumped().unwrap();
        assert!(d.is_unfinalized_dump());
        // Without CRIU's inventory it is not a usable image.
        fs::remove_file(d.images_dir().join(CRIU_INVENTORY)).unwrap();
        assert!(!d.is_unfinalized_dump());
        fake_dump(&d);
        // Once finalized it is complete, no longer "unfinalized".
        d.finalize(meta(4)).unwrap();
        assert!(matches!(d.status(), DirStatus::Complete(_)));
        assert!(!d.is_unfinalized_dump());
    }

    #[test]
    fn prepare_wipes_incomplete() {
        let tmp = TempDir::new();
        let d = CheckpointDir::for_job(tmp.path(), 2);
        d.prepare().unwrap();
        fs::write(d.images_dir().join("pages-1.img"), b"stale").unwrap();
        d.prepare().unwrap();
        assert_eq!(fs::read_dir(d.images_dir()).unwrap().count(), 0);
    }

    #[test]
    fn finalize_rejects_wrong_job_and_clears_stale_marker() {
        let tmp = TempDir::new();
        let d = CheckpointDir::for_job(tmp.path(), 2);
        d.prepare().unwrap();
        assert!(matches!(
            d.finalize(meta(3)),
            Err(LayoutError::JobIdMismatch {
                expected: 2,
                found: 3
            })
        ));
        assert!(!d.marker_path().exists());
    }

    #[test]
    fn scan_lists_numeric_dirs() {
        let tmp = TempDir::new();
        assert!(scan(&tmp.path().join("nope")).unwrap().is_empty());
        let a = CheckpointDir::for_job(tmp.path(), 10);
        a.prepare().unwrap();
        fake_dump(&a);
        a.finalize(meta(10)).unwrap();
        CheckpointDir::for_job(tmp.path(), 2).prepare().unwrap();
        fs::create_dir(tmp.path().join("lost+found")).unwrap();
        fs::write(tmp.path().join("7"), b"file").unwrap();
        let s = scan(tmp.path()).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], (2, DirStatus::Incomplete(IncompleteReason::NoMarker)));
        assert_eq!(s[1].0, 10);
        assert!(matches!(s[1].1, DirStatus::Complete(_)));
    }

    #[test]
    fn dir_size_ignores_symlinks() {
        let tmp = TempDir::new();
        fs::write(tmp.path().join("a"), vec![1u8; 10]).unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub/b"), vec![1u8; 5]).unwrap();
        std::os::unix::fs::symlink(tmp.path().join("a"), tmp.path().join("link")).unwrap();
        assert_eq!(dir_size(tmp.path()).unwrap(), 15);
        assert_eq!(dir_size(&tmp.path().join("a")).unwrap(), 10);
    }

    #[test]
    fn meta_json_roundtrip_and_defaults() {
        let m = meta(4);
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"mode\":\"manual\""));
        assert_eq!(serde_json::from_str::<CheckpointMeta>(&json).unwrap(), m);
        let minimal: CheckpointMeta = serde_json::from_str(
            r#"{"format_version":1,"job_id":4,"created_at":0,"mode":"plugin"}"#,
        )
        .unwrap();
        assert_eq!(minimal, CheckpointMeta::new(4, 0, CudaMode::Plugin));
    }

    #[test]
    fn compatibility() {
        let m = meta(1);
        let same = m.gpus.clone();
        assert!(check_compatibility(&m, &same, Some("595.91.07"), true).is_empty());

        let mut other_uuid = same.clone();
        other_uuid[0].uuid = "GPU-2".into();
        assert!(check_compatibility(&m, &other_uuid, Some("595.91.07"), false).is_empty());
        assert_eq!(
            check_compatibility(&m, &other_uuid, Some("595.91.07"), true),
            [Incompatibility::GpuUuid {
                index: 0,
                saved: "GPU-1".into(),
                current: Some("GPU-2".into())
            }]
        );

        let mut other_model = same.clone();
        other_model[0].model = "RTX 4090".into();
        let r = check_compatibility(&m, &other_model, Some("600.1"), false);
        assert_eq!(r.len(), 2);
        assert!(matches!(r[0], Incompatibility::GpuModel { index: 0, .. }));
        assert!(matches!(r[1], Incompatibility::DriverVersion { .. }));

        let r = check_compatibility(&m, &[], None, true);
        assert_eq!(
            r,
            [
                Incompatibility::GpuCount {
                    saved: 1,
                    current: 0
                },
                Incompatibility::GpuModel {
                    index: 0,
                    saved: "RTX 5070 Ti".into(),
                    current: None
                },
                Incompatibility::GpuUuid {
                    index: 0,
                    saved: "GPU-1".into(),
                    current: None
                },
                Incompatibility::DriverVersion {
                    saved: "595.91.07".into(),
                    current: None
                },
            ]
        );
    }

    #[test]
    fn write_atomic_replaces() {
        let tmp = TempDir::new();
        let p = tmp.path().join("f");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        assert!(!tmp.path().join("f.tmp").exists());
    }
}
