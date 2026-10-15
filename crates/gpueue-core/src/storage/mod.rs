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
//! use gpueue_core::storage::{Store, StoreOps};
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
mod job_errors;
mod jobs;
mod meta;
pub mod migrations;
mod priority;

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
use std::collections::BTreeMap;

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
        // FULL, not NORMAL: in WAL mode NORMAL may lose the last commits on
        // a power failure, and the daemon acknowledges helper events (which
        // the helper then forgets) right after the commit that applied
        // them. Writes are few, so the extra fsync per commit is cheap.
        conn.pragma_update(None, "synchronous", "FULL")?;
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
    /// Why the dump failed (a summary of the CRIU log). Restore errors are
    /// kept as a [`JobError`] instead.
    pub failure_reason: Option<String>,
    /// The job was paused when the dump began. CRIU and cuda-checkpoint
    /// need it running, so it is resumed for the dump; it is paused again
    /// once restored (or when the dump is refused), see
    /// [`crate::lifecycle::keep_paused`].
    pub paused: bool,
}

/// What a [`JobError`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobErrorKind {
    /// Restoring the job's checkpoint failed; the job stays checkpointed.
    Restore,
}

impl JobErrorKind {
    /// The value stored in `job_errors.kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            JobErrorKind::Restore => "restore",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "restore" => JobErrorKind::Restore,
            _ => return None,
        })
    }
}

/// The last error recorded for a job that its state does not carry by
/// itself (a failed spawn or dump is part of the job's result and of its
/// checkpoint record). A job has at most one; it is removed with the job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobError {
    pub job_id: JobId,
    pub kind: JobErrorKind,
    /// The reason, as reported by the helper or found by the daemon.
    pub message: String,
    pub recorded_at: Timestamp,
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
                    .execute_batch("ROLLBACK TO gpueue_op; RELEASE gpueue_op");
            }
        }
    }

    conn.execute_batch("SAVEPOINT gpueue_op")?;
    let mut guard = Guard { conn, armed: true };
    let value = f(conn)?;
    guard.armed = false;
    conn.execute_batch("RELEASE gpueue_op")?;
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

    /// The jobs scheduling needs, ordered by id: every job that is not
    /// `done`, plus the `done` jobs that one of those depends on. Reads no
    /// other finished job.
    fn list_unfinished_jobs(&self) -> Result<Vec<Job>> {
        jobs::list_unfinished(self.conn(), false)
    }

    /// [`StoreOps::list_unfinished_jobs`] without what deciding about jobs
    /// does not need: commands and environments are left empty. Read a job
    /// to be launched with [`StoreOps::get_job`].
    fn list_unfinished_jobs_brief(&self) -> Result<Vec<Job>> {
        jobs::list_unfinished(self.conn(), true)
    }

    /// How many jobs of `owner` have not finished (are not `done`).
    fn count_unfinished_jobs_of(&self, owner: Uid) -> Result<u64> {
        jobs::count_unfinished_of(self.conn(), owner)
    }

    /// The jobs that have not finished and depend on `id`, by id.
    fn unfinished_dependants(&self, id: JobId) -> Result<Vec<JobId>> {
        jobs::unfinished_dependants(self.conn(), id)
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

    /// Removes a job together with its env, GPUs, dependencies, checkpoint
    /// record and recorded error. Its id is never handed out again.
    fn remove_job(&self, id: JobId) -> Result<()> {
        jobs::remove(self.conn(), id)
    }

    /// Swaps the ids of two jobs (`switch`): each job keeps its spec, state,
    /// checkpoint record and recorded error but takes the other's id, and dependencies on
    /// either job are swapped too. Fails with
    /// [`Error::JobNotFound`](crate::Error::JobNotFound) if either is missing.
    fn swap_job_ids(&self, a: JobId, b: JobId) -> Result<()> {
        atomic(self.conn(), |c| jobs::swap_ids(c, a, b))
    }

    /// Sets a job's nice value. Fails with
    /// [`Error::JobNotFound`](crate::Error::JobNotFound) if it does not exist.
    fn set_job_nice(&self, id: JobId, nice: i32) -> Result<()> {
        jobs::set_nice(self.conn(), id, nice)
    }

    /// Sets a job's site factor.
    fn set_job_site_factor(&self, id: JobId, site: i32) -> Result<()> {
        jobs::set_site_factor(self.conn(), id, site)
    }

    /// Moves jobs to the top of their owners' queues (`gpueue top`): each
    /// gets a `top_seq` above every earlier one, the first id the highest.
    /// Fails (changing nothing) if a job does not exist.
    fn move_jobs_to_top(&self, ids: &[JobId]) -> Result<()> {
        atomic(self.conn(), |c| jobs::move_to_top(c, ids))
    }

    /// Resets what an administrator granted a job (a negative nice, the
    /// site factor) and its `top` position, for `restart --in-place`.
    fn clear_job_priority_grants(&self, id: JobId) -> Result<()> {
        priority::clear_priority_grants(self.conn(), id)
    }

    /// Records when a running job's run time was last charged (`None` when
    /// it is not running).
    fn set_job_run_since(&self, id: JobId, since: Option<Timestamp>) -> Result<()> {
        priority::set_run_since(self.conn(), id, since)
    }

    fn job_run_since(&self, id: JobId) -> Result<Option<Timestamp>> {
        priority::run_since(self.conn(), id)
    }

    /// Every job's `run_since` that is set.
    fn list_job_run_since(&self) -> Result<std::collections::HashMap<JobId, Timestamp>> {
        priority::list_run_since(self.conn())
    }

    /// Owners of the jobs that are not `done`.
    fn list_active_owners(&self) -> Result<std::collections::BTreeSet<Uid>> {
        priority::active_owners(self.conn())
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

    fn set_group_priority(&self, name: &str, priority: u32) -> Result<()> {
        groups::set_priority(self.conn(), name, priority)
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

    /// Jobs of `owner` whose last dump failed and whose owner has not been
    /// told yet, ordered by id, with the `created_at` of the failed record
    /// (which identifies the dump, for
    /// [`acknowledge_checkpoint_failures`]). A new record
    /// ([`put_checkpoint`]) or a status change ([`set_checkpoint_status`])
    /// makes a failure unacknowledged again.
    ///
    /// [`acknowledge_checkpoint_failures`]: StoreOps::acknowledge_checkpoint_failures
    /// [`put_checkpoint`]: StoreOps::put_checkpoint
    /// [`set_checkpoint_status`]: StoreOps::set_checkpoint_status
    fn unacknowledged_checkpoint_failures(&self, owner: Uid) -> Result<Vec<(JobId, Timestamp)>> {
        checkpoints::unacknowledged_failures(self.conn(), owner)
    }

    /// Marks the failed dumps `(job id, created_at)` as reported to their
    /// owner, as read by [`unacknowledged_checkpoint_failures`]. A record
    /// that has been replaced since (another dump, with another
    /// `created_at`), or is not failed, is left alone.
    ///
    /// [`unacknowledged_checkpoint_failures`]: StoreOps::unacknowledged_checkpoint_failures
    fn acknowledge_checkpoint_failures(&self, failures: &[(JobId, Timestamp)]) -> Result<()> {
        atomic(self.conn(), |c| {
            checkpoints::acknowledge_failures(c, failures)
        })
    }

    // ----- fair share -----

    /// Shares set by administrators, per user.
    fn list_user_shares(&self) -> Result<BTreeMap<Uid, u32>> {
        priority::list_shares(self.conn())
    }

    fn set_user_shares(&self, uid: Uid, shares: u32) -> Result<()> {
        priority::set_shares(self.conn(), uid, shares)
    }

    /// Decayed usage per user (billing-seconds).
    fn list_user_usage(&self) -> Result<BTreeMap<Uid, f64>> {
        priority::list_usage(self.conn())
    }

    /// Replaces every user's usage (users missing from `usage` have none).
    fn replace_user_usage(&self, usage: &BTreeMap<Uid, f64>) -> Result<()> {
        atomic(self.conn(), |c| priority::replace_usage(c, usage))
    }

    /// Adds `amount` to a user's usage.
    fn add_user_usage(&self, uid: Uid, amount: f64) -> Result<()> {
        priority::add_usage(self.conn(), uid, amount)
    }

    /// When usage was last calculated, if ever.
    fn usage_calc_at(&self) -> Result<Option<Timestamp>> {
        priority::usage_calc_at(self.conn())
    }

    fn set_usage_calc_at(&self, at: Timestamp) -> Result<()> {
        priority::set_usage_calc_at(self.conn(), at)
    }

    /// When usage was last reset (or first calculated), if ever.
    fn usage_reset_at(&self) -> Result<Option<Timestamp>> {
        priority::usage_reset_at(self.conn())
    }

    /// The local day (days since 1970-01-01) on which the reset period
    /// that usage was last reset for began, if recorded (see
    /// [`crate::priority::period_start_day`]).
    fn usage_reset_day(&self) -> Result<Option<i64>> {
        priority::usage_reset_day(self.conn())
    }

    fn set_usage_reset_day(&self, day: i64) -> Result<()> {
        priority::set_usage_reset_day(self.conn(), day)
    }

    fn set_usage_reset_at(&self, at: Timestamp) -> Result<()> {
        priority::set_usage_reset_at(self.conn(), at)
    }

    // ----- helper events -----

    /// The sequence number of the last helper event the daemon applied (0
    /// if none). Replayed events at or below it are duplicates.
    fn helper_event_seq(&self) -> Result<u64> {
        meta::helper_event_seq(self.conn())
    }

    /// Records `seq` as the last helper event applied (see
    /// [`helper_event_seq`](StoreOps::helper_event_seq)). Set it in the
    /// transaction that applies the event, so a crash cannot separate them.
    fn set_helper_event_seq(&self, seq: u64) -> Result<()> {
        meta::set_helper_event_seq(self.conn(), seq)
    }

    /// The epoch of the helper's event numbering that
    /// [`helper_event_seq`](StoreOps::helper_event_seq) belongs to, if the
    /// helper reported one (see `gpueue_protocol::helper::Hello::event_epoch`).
    fn helper_event_epoch(&self) -> Result<Option<u64>> {
        meta::helper_event_epoch(self.conn())
    }

    fn set_helper_event_epoch(&self, epoch: u64) -> Result<()> {
        meta::set_helper_event_epoch(self.conn(), epoch)
    }

    // ----- job errors -----

    /// Records `message` as the last error of `job_id`, replacing any
    /// previous one. A different error is reported to the owner again (see
    /// [`unacknowledged_job_errors`]); recording the same error twice (e.g.
    /// from a replayed helper event) is not. Fails with
    /// [`Error::JobNotFound`](crate::Error::JobNotFound) if the job does not
    /// exist.
    ///
    /// [`unacknowledged_job_errors`]: StoreOps::unacknowledged_job_errors
    fn set_job_error(
        &self,
        job_id: JobId,
        kind: JobErrorKind,
        message: &str,
        at: Timestamp,
    ) -> Result<()> {
        job_errors::set(self.conn(), job_id, kind, message, at)
    }

    fn get_job_error(&self, job_id: JobId) -> Result<Option<JobError>> {
        job_errors::get(self.conn(), job_id)
    }

    /// Forgets the job's error; returns whether there was one.
    fn clear_job_error(&self, job_id: JobId) -> Result<bool> {
        job_errors::clear(self.conn(), job_id)
    }

    /// Every recorded job error, ordered by job id.
    fn list_job_errors(&self) -> Result<Vec<JobError>> {
        job_errors::list(self.conn())
    }

    /// The errors of `owner`'s jobs the owner has not been told about yet,
    /// ordered by job id.
    fn unacknowledged_job_errors(&self, owner: Uid) -> Result<Vec<JobError>> {
        job_errors::unacknowledged(self.conn(), owner)
    }

    /// Marks the errors `(job id, recorded_at)` as reported to their owner,
    /// as read by [`unacknowledged_job_errors`]. An error recorded since
    /// (with another `recorded_at`) stays unacknowledged; jobs without an
    /// error are ignored.
    ///
    /// [`unacknowledged_job_errors`]: StoreOps::unacknowledged_job_errors
    fn acknowledge_job_errors(&self, errors: &[(JobId, Timestamp)]) -> Result<()> {
        atomic(self.conn(), |c| job_errors::acknowledge(c, errors))
    }
}

impl StoreOps for Store {}
impl StoreOps for Tx<'_> {}
