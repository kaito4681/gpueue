//! Single values kept in the `meta` table.

use rusqlite::{Connection, OptionalExtension, params};

use super::Result;

/// The key of the sequence number of the last helper event applied.
const HELPER_EVENT_SEQ: &str = "helper_event_seq";

/// The key of the epoch of the helper's event numbering.
const HELPER_EVENT_EPOCH: &str = "helper_event_epoch";

/// The value stored under `key`, if any.
pub(super) fn get(conn: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row([key], |row| row.get(0))
        .optional()?)
}

/// Stores `value` under `key`, replacing any previous value.
pub(super) fn set(conn: &Connection, key: &str, value: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )?
    .execute(params![key, value])?;
    Ok(())
}

pub(super) fn helper_event_seq(conn: &Connection) -> Result<u64> {
    Ok(get(conn, HELPER_EVENT_SEQ)?.map_or(0, |v| v.max(0) as u64))
}

pub(super) fn helper_event_epoch(conn: &Connection) -> Result<Option<u64>> {
    Ok(get(conn, HELPER_EVENT_EPOCH)?.map(|v| v.max(0) as u64))
}

pub(super) fn set_helper_event_epoch(conn: &Connection, epoch: u64) -> Result<()> {
    set(
        conn,
        HELPER_EVENT_EPOCH,
        i64::try_from(epoch).unwrap_or(i64::MAX),
    )
}

pub(super) fn set_helper_event_seq(conn: &Connection, seq: u64) -> Result<()> {
    // SQLite integers are signed; sequence numbers never get that far.
    set(
        conn,
        HELPER_EVENT_SEQ,
        i64::try_from(seq).unwrap_or(i64::MAX),
    )
}
