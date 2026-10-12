//! Single values kept in the `meta` table.

use rusqlite::{Connection, OptionalExtension, params};

use super::Result;

/// The key of the sequence number of the last helper event applied.
const HELPER_EVENT_SEQ: &str = "helper_event_seq";

pub(super) fn helper_event_seq(conn: &Connection) -> Result<u64> {
    let value: Option<i64> = conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row([HELPER_EVENT_SEQ], |row| row.get(0))
        .optional()?;
    Ok(value.map_or(0, |v| v.max(0) as u64))
}

pub(super) fn set_helper_event_seq(conn: &Connection, seq: u64) -> Result<()> {
    // SQLite integers are signed; sequence numbers never get that far.
    let value = i64::try_from(seq).unwrap_or(i64::MAX);
    conn.prepare_cached(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )?
    .execute(params![HELPER_EVENT_SEQ, value])?;
    Ok(())
}
