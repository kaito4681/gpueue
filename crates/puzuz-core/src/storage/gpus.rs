use rusqlite::{Connection, OptionalExtension, Row, params};

use super::Result;
use crate::error::{Error, StorageError};
use crate::model::{Gpu, GpuIndex};

fn gpu_row(row: &Row<'_>) -> rusqlite::Result<Gpu> {
    Ok(Gpu {
        index: GpuIndex(row.get(0)?),
        uuid: row.get(1)?,
        name: row.get(2)?,
        parallel: row.get(3)?,
    })
}

pub(super) fn list(conn: &Connection) -> Result<Vec<Gpu>> {
    Ok(conn
        .prepare_cached("SELECT gpu_index, uuid, name, parallel FROM gpus ORDER BY gpu_index")?
        .query_map([], gpu_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(super) fn get(conn: &Connection, index: GpuIndex) -> Result<Option<Gpu>> {
    Ok(conn
        .prepare_cached("SELECT gpu_index, uuid, name, parallel FROM gpus WHERE gpu_index = ?1")?
        .query_row([index.0], gpu_row)
        .optional()?)
}

/// Must run inside a savepoint or transaction.
pub(super) fn upsert(conn: &Connection, index: GpuIndex, uuid: &str, name: &str) -> Result<Gpu> {
    conn.prepare_cached(
        "INSERT INTO gpus (gpu_index, uuid, name, parallel) VALUES (?1, ?2, ?3, 1)
         ON CONFLICT (gpu_index) DO UPDATE SET uuid = excluded.uuid, name = excluded.name",
    )?
    .execute(params![index.0, uuid, name])?;
    get(conn, index)?.ok_or_else(|| StorageError::Corrupt(format!("GPU {index} vanished")))
}

pub(super) fn set_parallel(conn: &Connection, index: GpuIndex, parallel: u32) -> Result<()> {
    let changed = conn
        .prepare_cached("UPDATE gpus SET parallel = ?2 WHERE gpu_index = ?1")?
        .execute(params![index.0, parallel])?;
    if changed == 0 {
        return Err(Error::GpuNotFound(index).into());
    }
    Ok(())
}
