use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{JobError, JobErrorKind, Result};
use crate::error::Error;
use crate::model::{JobId, Timestamp, Uid};

fn kind_column(row: &Row<'_>, index: usize) -> rusqlite::Result<JobErrorKind> {
    let kind: String = row.get(index)?;
    JobErrorKind::parse(&kind).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            Type::Text,
            format!("unknown job error kind {kind:?}").into(),
        )
    })
}

pub(super) fn set(
    conn: &Connection,
    job_id: JobId,
    kind: JobErrorKind,
    message: &str,
    at: Timestamp,
) -> Result<()> {
    // The same error again (e.g. a replayed helper event) keeps its
    // acknowledgement, so the owner is not warned twice.
    let result = conn
        .prepare_cached(
            "INSERT INTO job_errors (job_id, kind, message, recorded_at, acknowledged)
             VALUES (?1, ?2, ?3, ?4, 0)
             ON CONFLICT (job_id) DO UPDATE SET
                 acknowledged = CASE
                     WHEN kind = excluded.kind AND message = excluded.message THEN acknowledged
                     ELSE 0
                 END,
                 kind = excluded.kind,
                 message = excluded.message,
                 recorded_at = excluded.recorded_at",
        )?
        .execute(params![job_id.0, kind.as_str(), message, at]);
    match result {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY =>
        {
            Err(Error::JobNotFound(job_id).into())
        }
        Err(e) => Err(e.into()),
    }
}

pub(super) fn get(conn: &Connection, job_id: JobId) -> Result<Option<JobError>> {
    Ok(conn
        .prepare_cached(
            "SELECT job_id, kind, message, recorded_at FROM job_errors WHERE job_id = ?1",
        )?
        .query_row([job_id.0], error_row)
        .optional()?)
}

fn error_row(row: &Row<'_>) -> rusqlite::Result<JobError> {
    Ok(JobError {
        job_id: JobId(row.get(0)?),
        kind: kind_column(row, 1)?,
        message: row.get(2)?,
        recorded_at: row.get(3)?,
    })
}

pub(super) fn clear(conn: &Connection, job_id: JobId) -> Result<bool> {
    let changed = conn
        .prepare_cached("DELETE FROM job_errors WHERE job_id = ?1")?
        .execute([job_id.0])?;
    Ok(changed > 0)
}

pub(super) fn list(conn: &Connection) -> Result<Vec<JobError>> {
    Ok(conn
        .prepare_cached(
            "SELECT job_id, kind, message, recorded_at FROM job_errors ORDER BY job_id",
        )?
        .query_map([], error_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(super) fn unacknowledged(conn: &Connection, owner: Uid) -> Result<Vec<JobError>> {
    Ok(conn
        .prepare_cached(
            "SELECT e.job_id, e.kind, e.message, e.recorded_at
             FROM job_errors e JOIN jobs j ON j.id = e.job_id
             WHERE e.acknowledged = 0 AND j.owner_uid = ?1
             ORDER BY e.job_id",
        )?
        .query_map([owner.0], error_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(super) fn acknowledge(conn: &Connection, errors: &[(JobId, Timestamp)]) -> Result<()> {
    let mut statement = conn.prepare_cached(
        "UPDATE job_errors SET acknowledged = 1 WHERE job_id = ?1 AND recorded_at = ?2",
    )?;
    for (id, recorded_at) in errors {
        statement.execute(params![id.0, recorded_at])?;
    }
    Ok(())
}
