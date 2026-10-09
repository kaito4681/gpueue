//! Persistent daemon state in SQLite (`state.sqlite`).
//!
//! [`Store`] owns the single connection the daemon uses. It is synchronous;
//! the daemon drives it from a dedicated thread.
//!
//! All queries live on the [`StoreOps`] trait, implemented both by [`Store`]
//! and by [`Tx`]. Called on a `Store`, every method is atomic on its own (a
//! method touching several tables runs inside a savepoint). To group several
//! changes into one atomic unit, use [`Store::transaction`]:
//!
//! ```
//! use puzuz_core::storage::{Store, StoreOps};
//!
//! let mut store = Store::open_in_memory().unwrap();
//! store
//!     .transaction(|tx| {
//!         tx.create_group("gpu", 2)?;
//!         tx.set_group_paused("gpu", true)
//!     })
//!     .unwrap();
//! assert!(store.get_group("gpu").unwrap().unwrap().paused);
//! ```
//!
//! See [`migrations`] for the schema and how values are represented.

mod checkpoints;
mod gpus;
mod groups;
mod jobs;
pub mod migrations;

#[cfg(test)]
mod tests;

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};

pub use crate::error::StorageError;
use crate::model::{Gpu, GpuIndex, Group, Job, JobId, JobSpec, JobState, Timestamp, Uid};
pub use migrations::SCHEMA_VERSION;

pub type Result<T, E = StorageError> = std::result::Result<T, E>;

/// How long to wait for a lock held by another connection (e.g. an admin
/// inspecting the database with `sqlite3`) before failing with `SQLITE_BUSY`.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// The daemon's state database.
pub struct Store {
    conn: Connection,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.conn.path())
            .finish()
    }
}

impl Store {
    /// Opens (creating if needed) the database at `path`, configures it and
    /// applies pending migrations.
    ///
    /// A newly created file gets mode `0600`. An existing file's mode is left
    /// alone. SQLite gives the `-wal` and `-shm` files the same mode as the
    /// database file.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        create_private_file(path).map_err(|source| StorageError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let conn = Connection::open(path)?;
        Self::init(conn, true)
    }

    /// Opens a fresh, private in-memory database (for tests).
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?, false)
    }

    fn init(mut conn: Connection, on_disk: bool) -> Result<Self> {
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // Must be set outside a transaction; it is per connection.
        conn.pragma_update(None, "foreign_keys", true)?;
        if on_disk {
            let mode: String =
                conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                return Err(StorageError::Corrupt(format!(
                    "cannot enable WAL journal mode (got {mode:?})"
                )));
            }
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        migrations::migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// The schema version of the open database (always [`SCHEMA_VERSION`]
    /// after a successful open).
    pub fn schema_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Runs `f` inside one immediate transaction: it is committed if `f`
    /// returns `Ok` and rolled back otherwise (including on panic).
    pub fn transaction<T, E>(&mut self, f: impl FnOnce(&Tx<'_>) -> Result<T, E>) -> Result<T, E>
    where
        E: From<StorageError>,
    {
        let tx = Tx {
            tx: self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(StorageError::from)?,
        };
        let value = f(&tx)?;
        tx.tx.commit().map_err(StorageError::from)?;
        Ok(value)
    }
}

/// An open transaction, handed to the closure of [`Store::transaction`].
pub struct Tx<'a> {
    tx: rusqlite::Transaction<'a>,
}

/// Creates `path` with mode `0600` if it does not exist yet.
fn create_private_file(path: &Path) -> io::Result<()> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => {
            // `mode` is filtered through the umask; make sure it is exact.
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Progress of a checkpoint image on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointStatus {
    /// The dump is being written.
    InProgress,
    /// The dump finished and its completion marker was written.
    Complete,
    /// The dump failed; the image (if any) is unusable.
    Failed,
}

impl CheckpointStatus {
    /// The value stored in `checkpoints.status`.
    pub fn as_str(self) -> &'static str {
        match self {
            CheckpointStatus::InProgress => "in_progress",
            CheckpointStatus::Complete => "complete",
            CheckpointStatus::Failed => "failed",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "in_progress" => CheckpointStatus::InProgress,
            "complete" => CheckpointStatus::Complete,
            "failed" => CheckpointStatus::Failed,
            _ => return None,
        })
    }
}

/// The record of a job's checkpoint image. A job has at most one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub job_id: JobId,
    /// Directory holding the CRIU images.
    pub path: PathBuf,
    /// Total image size, once known.
    pub size_bytes: Option<u64>,
    /// GPU model the job ran on (`None` for CPU-only jobs).
    pub gpu_model: Option<String>,
    /// NVIDIA driver version at dump time.
    pub driver_version: Option<String>,
    pub created_at: Timestamp,
    pub status: CheckpointStatus,
    /// Why the dump failed (a summary of the CRIU log).
    pub failure_reason: Option<String>,
}

/// Criteria for [`StoreOps::list_jobs`]. Unset fields match everything;
/// set fields are combined with AND.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobFilter<'a> {
    pub owner: Option<Uid>,
    pub group: Option<&'a str>,
    /// A [`JobState::name`], e.g. `"queued"`.
    pub state: Option<&'a str>,
}

impl<'a> JobFilter<'a> {
    /// Matches every job.
    pub fn all() -> Self {
        Self::default()
    }

    pub fn owner(mut self, owner: Uid) -> Self {
        self.owner = Some(owner);
        self
    }

    pub fn group(mut self, group: &'a str) -> Self {
        self.group = Some(group);
        self
    }

    pub fn state(mut self, state: &'a str) -> Self {
        self.state = Some(state);
        self
    }
}

mod sealed {
    pub trait Sealed {
        fn conn(&self) -> &rusqlite::Connection;
    }
}

impl sealed::Sealed for Store {
    fn conn(&self) -> &Connection {
        &self.conn
    }
}

impl sealed::Sealed for Tx<'_> {
    fn conn(&self) -> &Connection {
        &self.tx
    }
}

/// Runs `f` inside a savepoint so that a multi-statement operation is atomic
/// whether or not the caller already opened a transaction. The savepoint is
/// rolled back if `f` fails or panics.
fn atomic<T>(conn: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    struct Guard<'c> {
        conn: &'c Connection,
        armed: bool,
    }
    impl Drop for Guard<'_> {
        fn drop(&mut self) {
            if self.armed {
                // Best effort: there is nothing more useful to do with an
                // error while already unwinding from another one.
                let _ = self
                    .conn
                    .execute_batch("ROLLBACK TO puzuz_op; RELEASE puzuz_op");
            }
        }
    }

    conn.execute_batch("SAVEPOINT puzuz_op")?;
    let mut guard = Guard { conn, armed: true };
    let value = f(conn)?;
    guard.armed = false;
    conn.execute_batch("RELEASE puzuz_op")?;
    Ok(value)
}

/// Every query on the state database. Implemented by [`Store`] (each call is
/// atomic by itself) and [`Tx`] (calls join the surrounding transaction).
pub trait StoreOps: sealed::Sealed {
    // ----- jobs -----

    /// The id the next inserted job will get.
    fn next_job_id(&self) -> Result<JobId> {
        jobs::next_id(self.conn())
    }

    /// Inserts a job, allocating its id from the persistent counter.
    ///
    /// Fails with [`Error::GroupNotFound`](crate::Error::GroupNotFound) if
    /// `spec.group` does not exist.
    fn insert_job(
        &self,
        owner: Uid,
        spec: &JobSpec,
        state: &JobState,
        created_at: Timestamp,
    ) -> Result<Job> {
        atomic(self.conn(), |c| {
            jobs::insert(c, owner, spec, state, created_at)
        })
    }

    fn get_job(&self, id: JobId) -> Result<Option<Job>> {
        jobs::get(self.conn(), id)
    }

    /// Jobs matching `filter`, ordered by id.
    fn list_jobs(&self, filter: &JobFilter<'_>) -> Result<Vec<Job>> {
        jobs::list(self.conn(), filter)
    }

    /// Sets a job's state and both timestamps, exactly as given, in one
    /// statement. Fails with [`Error::JobNotFound`](crate::Error::JobNotFound)
    /// if the job does not exist.
    fn update_job_state(
        &self,
        id: JobId,
        state: &JobState,
        started_at: Option<Timestamp>,
        finished_at: Option<Timestamp>,
    ) -> Result<()> {
        jobs::update_state(self.conn(), id, state, started_at, finished_at)
    }

    /// Removes a job together with its env, GPUs, dependencies and
    /// checkpoint record. Its id is never handed out again.
    fn remove_job(&self, id: JobId) -> Result<()> {
        jobs::remove(self.conn(), id)
    }

    /// Swaps the ids of two jobs (`switch`): each job keeps its spec, state
    /// and checkpoint record but takes the other's id, and dependencies on
    /// either job are swapped too. Fails with
    /// [`Error::JobNotFound`](crate::Error::JobNotFound) if either is missing.
    fn swap_job_ids(&self, a: JobId, b: JobId) -> Result<()> {
        atomic(self.conn(), |c| jobs::swap_ids(c, a, b))
    }

    // ----- groups -----

    fn list_groups(&self) -> Result<Vec<Group>> {
        groups::list(self.conn())
    }

    fn get_group(&self, name: &str) -> Result<Option<Group>> {
        groups::get(self.conn(), name)
    }

    /// Creates an unpaused group. Fails with [`StorageError::GroupExists`]
    /// if it already exists.
    fn create_group(&self, name: &str, parallel: u32) -> Result<Group> {
        groups::create(self.conn(), name, parallel)
    }

    fn set_group_parallel(&self, name: &str, parallel: u32) -> Result<()> {
        groups::set_parallel(self.conn(), name, parallel)
    }

    fn set_group_paused(&self, name: &str, paused: bool) -> Result<()> {
        groups::set_paused(self.conn(), name, paused)
    }

    /// Removes an empty group. The default group cannot be removed.
    fn remove_group(&self, name: &str) -> Result<()> {
        atomic(self.conn(), |c| groups::remove(c, name))
    }

    // ----- GPUs -----

    fn list_gpus(&self) -> Result<Vec<Gpu>> {
        gpus::list(self.conn())
    }

    fn get_gpu(&self, index: GpuIndex) -> Result<Option<Gpu>> {
        gpus::get(self.conn(), index)
    }

    /// Records a detected GPU. A new GPU gets `parallel = 1`; for a known
    /// index the uuid and name are refreshed and the configured `parallel`
    /// is kept. GPUs that are no longer detected are not removed.
    fn upsert_gpu(&self, index: GpuIndex, uuid: &str, name: &str) -> Result<Gpu> {
        atomic(self.conn(), |c| gpus::upsert(c, index, uuid, name))
    }

    fn set_gpu_parallel(&self, index: GpuIndex, parallel: u32) -> Result<()> {
        gpus::set_parallel(self.conn(), index, parallel)
    }

    // ----- checkpoints -----

    /// Inserts or replaces the checkpoint record of `record.job_id` (a job
    /// keeps one generation only).
    fn put_checkpoint(&self, record: &CheckpointRecord) -> Result<()> {
        checkpoints::put(self.conn(), record)
    }

    fn get_checkpoint(&self, job_id: JobId) -> Result<Option<CheckpointRecord>> {
        checkpoints::get(self.conn(), job_id)
    }

    /// All checkpoint records, ordered by job id.
    fn list_checkpoints(&self) -> Result<Vec<CheckpointRecord>> {
        checkpoints::list(self.conn())
    }

    /// Updates status, size and failure reason of an existing record.
    fn set_checkpoint_status(
        &self,
        job_id: JobId,
        status: CheckpointStatus,
        size_bytes: Option<u64>,
        failure_reason: Option<&str>,
    ) -> Result<()> {
        checkpoints::set_status(self.conn(), job_id, status, size_bytes, failure_reason)
    }

    /// Deletes a checkpoint record; returns whether one existed.
    fn delete_checkpoint(&self, job_id: JobId) -> Result<bool> {
        checkpoints::delete(self.conn(), job_id)
    }
}

impl StoreOps for Store {}
impl StoreOps for Tx<'_> {}
