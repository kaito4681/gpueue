use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{JobFilter, Result};
use crate::error::{Error, StorageError};
use crate::model::{GpuIndex, Job, JobId, JobSpec, JobState, Timestamp, Uid};

pub(super) fn path_to_blob(path: &Path) -> &[u8] {
    path.as_os_str().as_bytes()
}

pub(super) fn blob_to_path(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(OsStr::from_bytes(&bytes))
}

pub(super) fn next_id(conn: &Connection) -> Result<JobId> {
    let next: u64 = conn.query_row(
        "SELECT value FROM meta WHERE key = 'next_job_id'",
        [],
        |row| row.get(0),
    )?;
    Ok(JobId(next))
}

fn group_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached("SELECT 1 FROM groups WHERE name = ?1")?
        .exists([name])?)
}

/// Must run inside a savepoint or transaction.
pub(super) fn insert(
    conn: &Connection,
    owner: Uid,
    spec: &JobSpec,
    state: &JobState,
    created_at: Timestamp,
) -> Result<Job> {
    if !group_exists(conn, &spec.group)? {
        return Err(Error::GroupNotFound(spec.group.clone()).into());
    }
    let id = next_id(conn)?;
    conn.execute(
        "UPDATE meta SET value = value + 1 WHERE key = 'next_job_id'",
        [],
    )?;
    let state_data = serde_json::to_string(state)
        .map_err(|e| StorageError::Corrupt(format!("cannot encode job state: {e}")))?;
    conn.prepare_cached(
        "INSERT INTO jobs (id, owner_uid, command, cwd, group_name, priority, label,
                           no_checkpoint, state, state_data, created_at, started_at, finished_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, NULL)",
    )?
    .execute(params![
        id.0,
        owner.0,
        spec.command,
        path_to_blob(&spec.cwd),
        spec.group,
        spec.priority,
        spec.label,
        spec.no_checkpoint,
        state.name(),
        state_data,
        created_at,
    ])?;

    let mut env =
        conn.prepare_cached("INSERT INTO job_env (job_id, key, value) VALUES (?1, ?2, ?3)")?;
    for (key, value) in &spec.envs {
        env.execute(params![id.0, key, value])?;
    }
    let mut gpus = conn
        .prepare_cached("INSERT INTO job_gpus (job_id, position, gpu_index) VALUES (?1, ?2, ?3)")?;
    for (position, gpu) in spec.gpus.iter().enumerate() {
        gpus.execute(params![id.0, position, gpu.0])?;
    }
    let mut deps = conn.prepare_cached(
        "INSERT INTO job_deps (job_id, position, depends_on) VALUES (?1, ?2, ?3)",
    )?;
    for (position, dep) in spec.dependencies.iter().enumerate() {
        deps.execute(params![id.0, position, dep.0])?;
    }

    Ok(Job {
        id,
        owner,
        spec: spec.clone(),
        state: state.clone(),
        created_at,
        started_at: None,
        finished_at: None,
    })
}

const JOB_COLUMNS: &str = "id, owner_uid, command, cwd, group_name, priority, label, \
                           no_checkpoint, state, state_data, created_at, started_at, finished_at";

/// A `jobs` row before its child tables are loaded.
struct JobRow {
    job: Job,
    state_name: String,
    state_data: String,
}

fn job_row(row: &Row<'_>) -> rusqlite::Result<JobRow> {
    Ok(JobRow {
        job: Job {
            id: JobId(row.get(0)?),
            owner: Uid(row.get(1)?),
            spec: JobSpec {
                command: row.get(2)?,
                cwd: blob_to_path(row.get(3)?),
                envs: BTreeMap::new(),
                group: row.get(4)?,
                gpus: Vec::new(),
                dependencies: Vec::new(),
                priority: row.get(5)?,
                label: row.get(6)?,
                no_checkpoint: row.get(7)?,
            },
            // Placeholder; decoded from `state_data` in `finish_job`.
            state: JobState::Running,
            created_at: row.get(10)?,
            started_at: row.get(11)?,
            finished_at: row.get(12)?,
        },
        state_name: row.get(8)?,
        state_data: row.get(9)?,
    })
}

/// Decodes the state and loads env, GPUs and dependencies.
fn finish_job(conn: &Connection, row: JobRow) -> Result<Job> {
    let JobRow {
        mut job,
        state_name,
        state_data,
    } = row;
    let id = job.id;
    job.state = serde_json::from_str(&state_data)
        .map_err(|e| StorageError::Corrupt(format!("job {id}: bad state_data: {e}")))?;
    if job.state.name() != state_name {
        return Err(StorageError::Corrupt(format!(
            "job {id}: state column {state_name:?} disagrees with state_data {:?}",
            job.state.name()
        )));
    }

    job.spec.envs = conn
        .prepare_cached("SELECT key, value FROM job_env WHERE job_id = ?1")?
        .query_map([id.0], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    job.spec.gpus = conn
        .prepare_cached("SELECT gpu_index FROM job_gpus WHERE job_id = ?1 ORDER BY position")?
        .query_map([id.0], |row| row.get(0).map(GpuIndex))?
        .collect::<rusqlite::Result<_>>()?;
    job.spec.dependencies = conn
        .prepare_cached("SELECT depends_on FROM job_deps WHERE job_id = ?1 ORDER BY position")?
        .query_map([id.0], |row| row.get(0).map(JobId))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(job)
}

pub(super) fn get(conn: &Connection, id: JobId) -> Result<Option<Job>> {
    let row = conn
        .prepare_cached(&format!("SELECT {JOB_COLUMNS} FROM jobs WHERE id = ?1"))?
        .query_row([id.0], job_row)
        .optional()?;
    row.map(|row| finish_job(conn, row)).transpose()
}

pub(super) fn list(conn: &Connection, filter: &JobFilter<'_>) -> Result<Vec<Job>> {
    let rows = conn
        .prepare_cached(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs
             WHERE (?1 IS NULL OR owner_uid = ?1)
               AND (?2 IS NULL OR group_name = ?2)
               AND (?3 IS NULL OR state = ?3)
             ORDER BY id"
        ))?
        .query_map(
            params![filter.owner.map(|u| u.0), filter.group, filter.state],
            job_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(|row| finish_job(conn, row)).collect()
}

pub(super) fn update_state(
    conn: &Connection,
    id: JobId,
    state: &JobState,
    started_at: Option<Timestamp>,
    finished_at: Option<Timestamp>,
) -> Result<()> {
    let state_data = serde_json::to_string(state)
        .map_err(|e| StorageError::Corrupt(format!("cannot encode job state: {e}")))?;
    let changed = conn
        .prepare_cached(
            "UPDATE jobs SET state = ?2, state_data = ?3, started_at = ?4, finished_at = ?5
             WHERE id = ?1",
        )?
        .execute(params![
            id.0,
            state.name(),
            state_data,
            started_at,
            finished_at
        ])?;
    if changed == 0 {
        return Err(Error::JobNotFound(id).into());
    }
    Ok(())
}

pub(super) fn remove(conn: &Connection, id: JobId) -> Result<()> {
    // Child rows go with it through ON DELETE CASCADE.
    let changed = conn
        .prepare_cached("DELETE FROM jobs WHERE id = ?1")?
        .execute([id.0])?;
    if changed == 0 {
        return Err(Error::JobNotFound(id).into());
    }
    Ok(())
}

/// Swaps the ids of two jobs (pueue's `switch`): every row keyed by one job
/// id moves to the other, and dependency references follow. Must run inside
/// a savepoint or transaction.
pub(super) fn swap_ids(conn: &Connection, a: JobId, b: JobId) -> Result<()> {
    for id in [a, b] {
        if get(conn, id)?.is_none() {
            return Err(Error::JobNotFound(id).into());
        }
    }
    if a == b {
        return Ok(());
    }
    // The id counter has not been handed out yet, so it is free to use as a
    // temporary. Rows briefly point at it; foreign keys are checked when the
    // surrounding transaction commits (the pragma resets itself then).
    let tmp = next_id(conn)?;
    conn.execute_batch("PRAGMA defer_foreign_keys = ON")?;
    for (table, column) in [
        ("jobs", "id"),
        ("job_env", "job_id"),
        ("job_gpus", "job_id"),
        ("job_deps", "job_id"),
        ("job_deps", "depends_on"),
        ("checkpoints", "job_id"),
    ] {
        let sql = format!("UPDATE {table} SET {column} = ?2 WHERE {column} = ?1");
        let mut stmt = conn.prepare_cached(&sql)?;
        stmt.execute(params![a.0, tmp.0])?;
        stmt.execute(params![b.0, a.0])?;
        stmt.execute(params![tmp.0, b.0])?;
    }
    Ok(())
}
