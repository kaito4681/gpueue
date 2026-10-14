use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, Row, params};

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
    // `--after 1 --after 1` names one dependency; keep the first mention.
    let mut spec = spec.clone();
    let mut seen = std::collections::HashSet::new();
    spec.dependencies.retain(|dep| seen.insert(*dep));
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
        spec,
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
            // Placeholder; decoded from `state_data` in `decode_state`.
            state: JobState::Running,
            created_at: row.get(10)?,
            started_at: row.get(11)?,
            finished_at: row.get(12)?,
        },
        state_name: row.get(8)?,
        state_data: row.get(9)?,
    })
}

/// Decodes the state (the child tables are loaded by [`load_where`]).
fn decode_state(row: JobRow) -> Result<Job> {
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
    Ok(job)
}

/// The jobs matching `condition` (an SQL expression over the `jobs` table
/// that qualifies its columns with `jobs.`), ordered by id, with env, GPUs
/// and dependencies. Four queries however many jobs match: each child table
/// is read once for all of them.
fn load_where(
    conn: &Connection,
    condition: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Job>> {
    let rows = conn
        .prepare_cached(&format!(
            "SELECT {JOB_COLUMNS} FROM jobs WHERE {condition} ORDER BY jobs.id"
        ))?
        .query_map(params, job_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut jobs = rows
        .into_iter()
        .map(decode_state)
        .collect::<Result<Vec<_>>>()?;
    let index: HashMap<JobId, usize> = jobs.iter().enumerate().map(|(i, j)| (j.id, i)).collect();

    let mut env = conn.prepare_cached(&format!(
        "SELECT c.job_id, c.key, c.value FROM job_env c JOIN jobs ON jobs.id = c.job_id
         WHERE {condition}"
    ))?;
    let mut rows = env.query(params)?;
    while let Some(row) = rows.next()? {
        if let Some(&i) = index.get(&JobId(row.get(0)?)) {
            jobs[i].spec.envs.insert(row.get(1)?, row.get(2)?);
        }
    }
    let mut gpus = conn.prepare_cached(&format!(
        "SELECT c.job_id, c.gpu_index FROM job_gpus c JOIN jobs ON jobs.id = c.job_id
         WHERE {condition} ORDER BY c.job_id, c.position"
    ))?;
    let mut rows = gpus.query(params)?;
    while let Some(row) = rows.next()? {
        if let Some(&i) = index.get(&JobId(row.get(0)?)) {
            jobs[i].spec.gpus.push(GpuIndex(row.get(1)?));
        }
    }
    let mut deps = conn.prepare_cached(&format!(
        "SELECT c.job_id, c.depends_on FROM job_deps c JOIN jobs ON jobs.id = c.job_id
         WHERE {condition} ORDER BY c.job_id, c.position"
    ))?;
    let mut rows = deps.query(params)?;
    while let Some(row) = rows.next()? {
        if let Some(&i) = index.get(&JobId(row.get(0)?)) {
            jobs[i].spec.dependencies.push(JobId(row.get(1)?));
        }
    }
    Ok(jobs)
}

pub(super) fn get(conn: &Connection, id: JobId) -> Result<Option<Job>> {
    Ok(load_where(conn, "jobs.id = ?1", &[&id.0])?.pop())
}

pub(super) fn list(conn: &Connection, filter: &JobFilter<'_>) -> Result<Vec<Job>> {
    load_where(
        conn,
        "(?1 IS NULL OR jobs.owner_uid = ?1)
         AND (?2 IS NULL OR jobs.group_name = ?2)
         AND (?3 IS NULL OR jobs.state = ?3)",
        &[&filter.owner.map(|u| u.0), &filter.group, &filter.state],
    )
}

/// What scheduling looks at: every job that has not finished (`done`), and
/// the finished jobs those depend on (their result decides whether the
/// dependants may start). Other finished jobs are not read, so a long
/// history does not slow scheduling down.
pub(super) fn list_unfinished(conn: &Connection) -> Result<Vec<Job>> {
    load_where(
        conn,
        "jobs.state != 'done'
         OR jobs.id IN (SELECT d.depends_on FROM job_deps d
                        JOIN jobs w ON w.id = d.job_id
                        WHERE w.state != 'done')",
        &[],
    )
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
    // temporary. Rows briefly point at it, so foreign keys are only checked
    // at the end of the swap, when every reference is valid again.
    let tmp = next_id(conn)?;
    conn.execute_batch("PRAGMA defer_foreign_keys = ON")?;
    let swapped = swap_rows(conn, a, b, tmp);
    // The pragma only resets itself at the end of the outermost
    // transaction; later statements of a caller's transaction must be
    // checked immediately again.
    let reset = conn.execute_batch("PRAGMA defer_foreign_keys = OFF");
    swapped?;
    Ok(reset?)
}

fn swap_rows(conn: &Connection, a: JobId, b: JobId, tmp: JobId) -> Result<()> {
    for (table, column) in [
        ("jobs", "id"),
        ("job_env", "job_id"),
        ("job_gpus", "job_id"),
        ("job_deps", "job_id"),
        ("job_deps", "depends_on"),
        ("checkpoints", "job_id"),
        ("job_errors", "job_id"),
    ] {
        let sql = format!("UPDATE {table} SET {column} = ?2 WHERE {column} = ?1");
        let mut stmt = conn.prepare_cached(&sql)?;
        stmt.execute(params![a.0, tmp.0])?;
        stmt.execute(params![b.0, a.0])?;
        stmt.execute(params![tmp.0, b.0])?;
    }
    Ok(())
}
