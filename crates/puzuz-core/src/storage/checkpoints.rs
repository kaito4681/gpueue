use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::jobs::{blob_to_path, path_to_blob};
use super::{CheckpointRecord, CheckpointStatus, Result};
use crate::error::{Error, StorageError};
use crate::model::JobId;

const COLUMNS: &str =
    "job_id, path, size_bytes, gpu_model, driver_version, created_at, status, failure_reason";

fn checkpoint_row(row: &Row<'_>) -> rusqlite::Result<CheckpointRecord> {
    let status: String = row.get(6)?;
    let status = CheckpointStatus::parse(&status).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            6,
            Type::Text,
            format!("unknown checkpoint status {status:?}").into(),
        )
    })?;
    Ok(CheckpointRecord {
        job_id: JobId(row.get(0)?),
        path: blob_to_path(row.get(1)?),
        size_bytes: row.get(2)?,
        gpu_model: row.get(3)?,
        driver_version: row.get(4)?,
        created_at: row.get(5)?,
        status,
        failure_reason: row.get(7)?,
    })
}

pub(super) fn put(conn: &Connection, r: &CheckpointRecord) -> Result<()> {
    let result = conn
        .prepare_cached(&format!(
            "INSERT OR REPLACE INTO checkpoints ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
        ))?
        .execute(params![
            r.job_id.0,
            path_to_blob(&r.path),
            r.size_bytes,
            r.gpu_model,
            r.driver_version,
            r.created_at,
            r.status.as_str(),
            r.failure_reason,
        ]);
    match result {
        Ok(_) => Ok(()),
        // The only foreign key is the job; report it as a missing job.
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY =>
        {
            Err(Error::JobNotFound(r.job_id).into())
        }
        Err(e) => Err(e.into()),
    }
}

pub(super) fn get(conn: &Connection, job_id: JobId) -> Result<Option<CheckpointRecord>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM checkpoints WHERE job_id = ?1"
        ))?
        .query_row([job_id.0], checkpoint_row)
        .optional()?)
}

pub(super) fn list(conn: &Connection) -> Result<Vec<CheckpointRecord>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT {COLUMNS} FROM checkpoints ORDER BY job_id"
        ))?
        .query_map([], checkpoint_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(super) fn set_status(
    conn: &Connection,
    job_id: JobId,
    status: CheckpointStatus,
    size_bytes: Option<u64>,
    failure_reason: Option<&str>,
) -> Result<()> {
    let changed = conn
        .prepare_cached(
            "UPDATE checkpoints SET status = ?2, size_bytes = ?3, failure_reason = ?4
             WHERE job_id = ?1",
        )?
        .execute(params![
            job_id.0,
            status.as_str(),
            size_bytes,
            failure_reason
        ])?;
    if changed == 0 {
        return Err(StorageError::CheckpointNotFound(job_id));
    }
    Ok(())
}

pub(super) fn delete(conn: &Connection, job_id: JobId) -> Result<bool> {
    let changed = conn
        .prepare_cached("DELETE FROM checkpoints WHERE job_id = ?1")?
        .execute([job_id.0])?;
    Ok(changed > 0)
}
