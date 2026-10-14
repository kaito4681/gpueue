//! Fair-share state: shares and decayed usage per user, and when usage
//! was last calculated and reset (in `meta`).

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};

use super::Result;
use crate::model::{Timestamp, Uid};

/// `meta` key: when usage was last calculated.
const USAGE_CALC_AT: &str = "usage_calc_at";
/// `meta` key: when usage was last reset (or first recorded).
const USAGE_RESET_AT: &str = "usage_reset_at";

fn get_meta(conn: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row([key], |row| row.get(0))
        .optional()?)
}

fn set_meta(conn: &Connection, key: &str, value: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
    )?
    .execute(params![key, value])?;
    Ok(())
}

pub(super) fn usage_calc_at(conn: &Connection) -> Result<Option<Timestamp>> {
    get_meta(conn, USAGE_CALC_AT)
}

pub(super) fn set_usage_calc_at(conn: &Connection, at: Timestamp) -> Result<()> {
    set_meta(conn, USAGE_CALC_AT, at)
}

pub(super) fn usage_reset_at(conn: &Connection) -> Result<Option<Timestamp>> {
    get_meta(conn, USAGE_RESET_AT)
}

pub(super) fn set_usage_reset_at(conn: &Connection, at: Timestamp) -> Result<()> {
    set_meta(conn, USAGE_RESET_AT, at)
}

pub(super) fn list_shares(conn: &Connection) -> Result<BTreeMap<Uid, u32>> {
    let mut stmt = conn.prepare_cached("SELECT uid, shares FROM user_shares")?;
    let rows = stmt.query_map([], |row| Ok((Uid(row.get(0)?), row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn set_shares(conn: &Connection, uid: Uid, shares: u32) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO user_shares (uid, shares) VALUES (?1, ?2)
         ON CONFLICT (uid) DO UPDATE SET shares = excluded.shares",
    )?
    .execute(params![uid.0, shares])?;
    Ok(())
}

pub(super) fn list_usage(conn: &Connection) -> Result<BTreeMap<Uid, f64>> {
    let mut stmt = conn.prepare_cached("SELECT uid, usage FROM user_usage")?;
    let rows = stmt.query_map([], |row| Ok((Uid(row.get(0)?), row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Must run inside a savepoint or transaction.
pub(super) fn replace_usage(conn: &Connection, usage: &BTreeMap<Uid, f64>) -> Result<()> {
    conn.prepare_cached("DELETE FROM user_usage")?.execute([])?;
    let mut insert = conn.prepare_cached("INSERT INTO user_usage (uid, usage) VALUES (?1, ?2)")?;
    for (uid, value) in usage {
        if value.is_finite() && *value > 0.0 {
            insert.execute(params![uid.0, value])?;
        }
    }
    Ok(())
}
