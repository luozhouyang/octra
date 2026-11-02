use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::PluginStorageError;

pub(crate) fn get(conn: &Connection, key: &str) -> Result<Option<Vec<u8>>, PluginStorageError> {
    let now = now_unix();
    let row: Option<(SqlValue, Option<i64>)> = conn
        .query_row(
            "SELECT value, expires_at FROM _octra_kv WHERE key = ?1",
            [key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    match row {
        None => Ok(None),
        Some((_, Some(expires_at))) if expires_at <= now => {
            conn.execute("DELETE FROM _octra_kv WHERE key = ?1", [key])?;
            Ok(None)
        }
        Some((value, _)) => Ok(Some(sql_value_to_bytes(value))),
    }
}

pub(crate) fn put(
    conn: &Connection,
    key: &str,
    value: &[u8],
    ttl: Option<Duration>,
) -> Result<(), PluginStorageError> {
    let expires_at = ttl.map(|ttl| now_unix() + ttl.as_secs() as i64);
    conn.execute(
        "INSERT INTO _octra_kv (key, value, expires_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET \
            value = excluded.value, \
            expires_at = excluded.expires_at",
        params![key, SqlValue::Blob(value.to_vec()), expires_at],
    )?;
    Ok(())
}

pub(crate) fn delete(conn: &Connection, key: &str) -> Result<bool, PluginStorageError> {
    let changed = conn.execute("DELETE FROM _octra_kv WHERE key = ?1", [key])?;
    Ok(changed > 0)
}

pub(crate) fn incr(
    conn: &Connection,
    key: &str,
    delta: i64,
    ttl: Option<Duration>,
) -> Result<i64, PluginStorageError> {
    let now = now_unix();
    conn.execute(
        "DELETE FROM _octra_kv WHERE key = ?1 AND expires_at IS NOT NULL AND expires_at <= ?2",
        params![key, now],
    )?;

    let expires_at = ttl.map(|ttl| now + ttl.as_secs() as i64);
    let value = conn.query_row(
        "INSERT INTO _octra_kv (key, value, expires_at) VALUES (?1, ?2, ?3) \
         ON CONFLICT(key) DO UPDATE SET \
            value = CAST(_octra_kv.value AS INTEGER) + excluded.value, \
            expires_at = COALESCE(excluded.expires_at, _octra_kv.expires_at) \
         RETURNING CAST(value AS INTEGER)",
        params![key, SqlValue::Integer(delta), expires_at],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(value)
}

pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn sql_value_to_bytes(value: SqlValue) -> Vec<u8> {
    match value {
        SqlValue::Null => Vec::new(),
        SqlValue::Integer(value) => value.to_string().into_bytes(),
        SqlValue::Real(value) => value.to_string().into_bytes(),
        SqlValue::Text(value) => value.into_bytes(),
        SqlValue::Blob(value) => value,
    }
}
