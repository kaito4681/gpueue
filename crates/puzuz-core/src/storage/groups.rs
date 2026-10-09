use rusqlite::{Connection, OptionalExtension, Row, params};

use super::Result;
use crate::error::{Error, StorageError};
use crate::model::{DEFAULT_GROUP, Group};

fn group_row(row: &Row<'_>) -> rusqlite::Result<Group> {
    Ok(Group {
        name: row.get(0)?,
        parallel: row.get(1)?,
        paused: row.get(2)?,
    })
}

pub(super) fn list(conn: &Connection) -> Result<Vec<Group>> {
    Ok(conn
        .prepare_cached("SELECT name, parallel, paused FROM groups ORDER BY name")?
        .query_map([], group_row)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(super) fn get(conn: &Connection, name: &str) -> Result<Option<Group>> {
    Ok(conn
        .prepare_cached("SELECT name, parallel, paused FROM groups WHERE name = ?1")?
        .query_row([name], group_row)
        .optional()?)
}

pub(super) fn create(conn: &Connection, name: &str, parallel: u32) -> Result<Group> {
    let inserted = conn
        .prepare_cached(
            "INSERT INTO groups (name, parallel, paused) VALUES (?1, ?2, 0)
             ON CONFLICT (name) DO NOTHING",
        )?
        .execute(params![name, parallel])?;
    if inserted == 0 {
        return Err(StorageError::GroupExists(name.to_owned()));
    }
    Ok(Group {
        name: name.to_owned(),
        parallel,
        paused: false,
    })
}

fn not_found_if_unchanged(changed: usize, name: &str) -> Result<()> {
    if changed == 0 {
        return Err(Error::GroupNotFound(name.to_owned()).into());
    }
    Ok(())
}

pub(super) fn set_parallel(conn: &Connection, name: &str, parallel: u32) -> Result<()> {
    let changed = conn
        .prepare_cached("UPDATE groups SET parallel = ?2 WHERE name = ?1")?
        .execute(params![name, parallel])?;
    not_found_if_unchanged(changed, name)
}

pub(super) fn set_paused(conn: &Connection, name: &str, paused: bool) -> Result<()> {
    let changed = conn
        .prepare_cached("UPDATE groups SET paused = ?2 WHERE name = ?1")?
        .execute(params![name, paused])?;
    not_found_if_unchanged(changed, name)
}

/// Must run inside a savepoint or transaction.
pub(super) fn remove(conn: &Connection, name: &str) -> Result<()> {
    if name == DEFAULT_GROUP {
        return Err(StorageError::DefaultGroupRemoval);
    }
    let in_use = conn
        .prepare_cached("SELECT 1 FROM jobs WHERE group_name = ?1 LIMIT 1")?
        .exists([name])?;
    if in_use {
        return Err(StorageError::GroupNotEmpty(name.to_owned()));
    }
    let changed = conn
        .prepare_cached("DELETE FROM groups WHERE name = ?1")?
        .execute([name])?;
    not_found_if_unchanged(changed, name)
}
