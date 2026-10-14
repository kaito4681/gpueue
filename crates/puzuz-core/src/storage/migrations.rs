//! Schema migrations, keyed by `PRAGMA user_version`.
//!
//! `MIGRATIONS[i]` upgrades a database from version `i` to version `i + 1`.
//! Migrations are append-only: never edit one that has been released, add a
//! new one instead.

use rusqlite::Connection;

use crate::error::StorageError;

/// Version 1: the initial schema.
///
/// Notes on representation:
/// - Every table is `STRICT`, so SQLite enforces the declared column types.
/// - `jobs.state` holds [`JobState::name`](crate::model::JobState::name) for
///   filtering and indexing; `jobs.state_data` holds the whole `JobState`
///   (including its payload and, for finished jobs, the `JobResult`) as
///   serde JSON and is the source of truth when reading a job back.
/// - Paths (`jobs.cwd`, `checkpoints.path`) are stored as raw bytes (`BLOB`)
///   because Unix paths need not be valid UTF-8.
/// - `job_gpus` and `job_deps` keep a `position` column so the order given at
///   submission is preserved. Neither references `gpus` / `jobs` with a
///   foreign key: a GPU may disappear and a dependency may be removed while
///   the dependent job is still recorded.
/// - `meta.next_job_id` is the job id counter. It only ever grows, so ids are
///   never reused, even after the job with the highest id is removed.
const V1: &str = r#"
CREATE TABLE meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value INTEGER NOT NULL
) STRICT;
INSERT INTO meta (key, value) VALUES ('next_job_id', 0);

CREATE TABLE groups (
    name     TEXT PRIMARY KEY NOT NULL,
    parallel INTEGER NOT NULL CHECK (parallel >= 0),
    paused   INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1))
) STRICT;
INSERT INTO groups (name, parallel, paused) VALUES ('default', 1, 0);

CREATE TABLE jobs (
    id            INTEGER PRIMARY KEY NOT NULL CHECK (id >= 0),
    owner_uid     INTEGER NOT NULL,
    command       TEXT NOT NULL,
    cwd           BLOB NOT NULL,
    group_name    TEXT NOT NULL REFERENCES groups (name) ON DELETE RESTRICT,
    priority      INTEGER NOT NULL DEFAULT 0,
    label         TEXT,
    no_checkpoint INTEGER NOT NULL DEFAULT 0 CHECK (no_checkpoint IN (0, 1)),
    state         TEXT NOT NULL,
    state_data    TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    started_at    INTEGER,
    finished_at   INTEGER
) STRICT;
CREATE INDEX jobs_by_owner ON jobs (owner_uid);
CREATE INDEX jobs_by_group ON jobs (group_name);
CREATE INDEX jobs_by_state ON jobs (state);

CREATE TABLE job_env (
    job_id INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    key    TEXT NOT NULL,
    value  TEXT NOT NULL,
    PRIMARY KEY (job_id, key)
) STRICT;

CREATE TABLE job_gpus (
    job_id    INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    position  INTEGER NOT NULL,
    gpu_index INTEGER NOT NULL,
    PRIMARY KEY (job_id, position),
    UNIQUE (job_id, gpu_index)
) STRICT;
CREATE INDEX job_gpus_by_gpu ON job_gpus (gpu_index);

CREATE TABLE job_deps (
    job_id     INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    position   INTEGER NOT NULL,
    depends_on INTEGER NOT NULL,
    PRIMARY KEY (job_id, position),
    UNIQUE (job_id, depends_on)
) STRICT;
CREATE INDEX job_deps_by_dependency ON job_deps (depends_on);

CREATE TABLE gpus (
    gpu_index INTEGER PRIMARY KEY NOT NULL,
    uuid      TEXT NOT NULL,
    name      TEXT NOT NULL,
    parallel  INTEGER NOT NULL DEFAULT 1 CHECK (parallel >= 0)
) STRICT;

CREATE TABLE checkpoints (
    job_id         INTEGER PRIMARY KEY NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    path           BLOB NOT NULL,
    size_bytes     INTEGER CHECK (size_bytes >= 0),
    gpu_model      TEXT,
    driver_version TEXT,
    created_at     INTEGER NOT NULL,
    status         TEXT NOT NULL CHECK (status IN ('in_progress', 'complete', 'failed')),
    failure_reason TEXT
) STRICT;
"#;

/// Version 2: `checkpoints.failure_acknowledged`.
///
/// Set once the owner has been told about a failed dump (on their next
/// command). Replacing a record (a new dump) resets it, and so does a status
/// change; see `checkpoints::set_status`.
const V2: &str = r#"
ALTER TABLE checkpoints
    ADD COLUMN failure_acknowledged INTEGER NOT NULL DEFAULT 0
    CHECK (failure_acknowledged IN (0, 1));
"#;

/// Version 3: `job_errors`, the last error recorded for a job (currently
/// a failed restore), independent of its checkpoint record.
///
/// Restore errors used to be written into `checkpoints.failure_reason` of
/// the (complete) record, so they were lost for a job without one. They are
/// moved here; `checkpoints.failure_reason` now only explains failed dumps.
/// `acknowledged` is set once the owner has been told (like
/// `checkpoints.failure_acknowledged`); moved errors were never reported.
/// `kind` is checked by the code, not by SQL, so new kinds need no table
/// rebuild.
const V3: &str = r#"
CREATE TABLE job_errors (
    job_id       INTEGER PRIMARY KEY NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    kind         TEXT NOT NULL,
    message      TEXT NOT NULL,
    recorded_at  INTEGER NOT NULL,
    acknowledged INTEGER NOT NULL DEFAULT 0 CHECK (acknowledged IN (0, 1))
) STRICT;
INSERT INTO job_errors (job_id, kind, message, recorded_at, acknowledged)
    SELECT job_id, 'restore', failure_reason, created_at, 0 FROM checkpoints
    WHERE status = 'complete' AND failure_reason IS NOT NULL;
UPDATE checkpoints SET failure_reason = NULL WHERE status = 'complete';
"#;

/// Version 4: flat JSON for finished jobs in `jobs.state_data`.
///
/// The `JobResult` of a `done` or `terminating` state used to be nested
/// under `result` (`{"state":"done","result":{"result":"failed","exit_code":1}}`);
/// its fields are now flattened into the state object
/// (`{"state":"done","result":"failed","exit_code":1}`, see
/// [`JobState`](crate::model::JobState)). Rows whose `result` is an object are
/// rewritten by merging that object into the state; rows already flat (or
/// without a result) are left alone. `state_data` is the only column holding
/// serialized `JobState`/`JobResult` values.
const V4: &str = r#"
UPDATE jobs
    SET state_data = json_patch(json_remove(state_data, '$.result'),
                                json_extract(state_data, '$.result'))
    WHERE json_valid(state_data)
      AND json_type(state_data, '$.result') = 'object';
"#;

/// Version 5: `checkpoints.paused`, whether the job was paused when its
/// dump began, so that it comes back paused after the restore.
const V5: &str = r#"
ALTER TABLE checkpoints
    ADD COLUMN paused INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1));
"#;

/// Version 6: multifactor priority (see [`crate::priority`]).
///
/// - `jobs.nice` (subtracted from the priority), `jobs.site_factor` (added
///   by administrators) and `jobs.top_seq` (set by `puzuz top`; `NULL` for
///   jobs never moved to the top). Existing jobs keep their `priority`,
///   which now orders their owner's jobs only.
/// - `groups.priority`, the partition factor's input.
/// - `user_shares`: shares set by administrators (users without a row have
///   the default, 1).
/// - `user_usage`: decayed usage per user in billing-seconds. `REAL`
///   because decay makes it fractional.
/// - `meta` gains `usage_calc_at`, `usage_reset_at` and `next_top_seq`
///   when they are first needed.
/// - `jobs_by_finished_at` finds the jobs that finished since the last
///   usage calculation without reading the whole history.
const V6: &str = r#"
ALTER TABLE jobs ADD COLUMN nice INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN site_factor INTEGER NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN top_seq INTEGER CHECK (top_seq >= 0);
CREATE INDEX jobs_by_finished_at ON jobs (finished_at);
ALTER TABLE groups ADD COLUMN priority INTEGER NOT NULL DEFAULT 0 CHECK (priority >= 0);
CREATE TABLE user_shares (
    uid    INTEGER PRIMARY KEY NOT NULL,
    shares INTEGER NOT NULL CHECK (shares >= 0)
) STRICT;
CREATE TABLE user_usage (
    uid   INTEGER PRIMARY KEY NOT NULL,
    usage REAL NOT NULL CHECK (usage >= 0)
) STRICT;
"#;

/// All migrations, in order. The schema version after migrating is
/// `MIGRATIONS.len()`.
pub(crate) const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5, V6];

/// The schema version this build of puzuz writes.
pub const SCHEMA_VERSION: i64 = MIGRATIONS.len() as i64;

fn user_version(conn: &Connection) -> Result<i64, StorageError> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

/// Brings the schema up to [`SCHEMA_VERSION`].
///
/// All pending migrations run inside one immediate transaction together with
/// the `user_version` bump, so a crash leaves either the old or the new
/// schema. A database written by a newer puzuz is rejected untouched.
pub(crate) fn migrate(conn: &mut Connection) -> Result<(), StorageError> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let current = user_version(&tx)?;
    if current > SCHEMA_VERSION {
        return Err(StorageError::UnsupportedSchemaVersion {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    if current < 0 {
        return Err(StorageError::Corrupt(format!(
            "negative schema version {current}"
        )));
    }
    for sql in &MIGRATIONS[current as usize..] {
        tx.execute_batch(sql)?;
    }
    if current != SCHEMA_VERSION {
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    tx.commit()?;
    Ok(())
}
