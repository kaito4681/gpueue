//! Fair-share state: shares and decayed usage per user, when usage was
//! last calculated and reset (in `meta`), and when each running job was
//! last charged (`jobs.run_since`).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::{Connection, params};

use super::{Result, meta};
use crate::error::Error;
use crate::model::{JobId, Timestamp, Uid};

/// `meta` key: when usage was last calculated.
const USAGE_CALC_AT: &str = "usage_calc_at";
/// `meta` key: when usage was last reset (or first recorded).
const USAGE_RESET_AT: &str = "usage_reset_at";
/// `meta` key: the local day the reset period that usage was last reset
/// for began on (see `priority::period_start_day`).
const USAGE_RESET_DAY: &str = "usage_reset_day";

pub(super) fn usage_reset_day(conn: &Connection) -> Result<Option<i64>> {
    meta::get(conn, USAGE_RESET_DAY)
}

pub(super) fn set_usage_reset_day(conn: &Connection, day: i64) -> Result<()> {
    meta::set(conn, USAGE_RESET_DAY, day)
}

pub(super) fn usage_calc_at(conn: &Connection) -> Result<Option<Timestamp>> {
    meta::get(conn, USAGE_CALC_AT)
}

pub(super) fn set_usage_calc_at(conn: &Connection, at: Timestamp) -> Result<()> {
    meta::set(conn, USAGE_CALC_AT, at)
}

pub(super) fn usage_reset_at(conn: &Connection) -> Result<Option<Timestamp>> {
    meta::get(conn, USAGE_RESET_AT)
}

pub(super) fn set_usage_reset_at(conn: &Connection, at: Timestamp) -> Result<()> {
    meta::set(conn, USAGE_RESET_AT, at)
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

pub(super) fn add_usage(conn: &Connection, uid: Uid, amount: f64) -> Result<()> {
    if !amount.is_finite() || amount <= 0.0 {
        return Ok(());
    }
    conn.prepare_cached(
        "INSERT INTO user_usage (uid, usage) VALUES (?1, ?2)
         ON CONFLICT (uid) DO UPDATE SET usage = usage + excluded.usage",
    )?
    .execute(params![uid.0, amount])?;
    Ok(())
}

/// Owners of jobs that are not `done`.
pub(super) fn active_owners(conn: &Connection) -> Result<BTreeSet<Uid>> {
    let mut stmt =
        conn.prepare_cached("SELECT DISTINCT owner_uid FROM jobs WHERE state != 'done'")?;
    let rows = stmt.query_map([], |row| Ok(Uid(row.get(0)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn set_run_since(conn: &Connection, id: JobId, since: Option<Timestamp>) -> Result<()> {
    let changed = conn
        .prepare_cached("UPDATE jobs SET run_since = ?2 WHERE id = ?1")?
        .execute(params![id.0, since])?;
    if changed == 0 {
        return Err(Error::JobNotFound(id).into());
    }
    Ok(())
}

pub(super) fn run_since(conn: &Connection, id: JobId) -> Result<Option<Timestamp>> {
    let mut stmt = conn.prepare_cached("SELECT run_since FROM jobs WHERE id = ?1")?;
    let mut rows = stmt.query([id.0])?;
    match rows.next()? {
        Some(row) => Ok(row.get(0)?),
        None => Err(Error::JobNotFound(id).into()),
    }
}

pub(super) fn list_run_since(conn: &Connection) -> Result<HashMap<JobId, Timestamp>> {
    let mut stmt =
        conn.prepare_cached("SELECT id, run_since FROM jobs WHERE run_since IS NOT NULL")?;
    let rows = stmt.query_map([], |row| Ok((JobId(row.get(0)?), row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn clear_priority_grants(conn: &Connection, id: JobId) -> Result<()> {
    let changed = conn
        .prepare_cached(
            "UPDATE jobs SET nice = max(nice, 0), site_factor = 0, top_seq = NULL WHERE id = ?1",
        )?
        .execute([id.0])?;
    if changed == 0 {
        return Err(Error::JobNotFound(id).into());
    }
    Ok(())
}
