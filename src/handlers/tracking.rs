//! The `_iii_migrations` tracking table: DDL, inserts, and read access.
//!
//! Schema (created by `migrate::up` / `migrate::baseline`, idempotently):
//!   name        TEXT PRIMARY KEY   — full migration file name
//!   checksum    TEXT NOT NULL      — SHA-256 hex of the file at apply time
//!   applied_at  TEXT / timestamptz — apply timestamp

use serde::Serialize;
use serde_json::Value;

use super::AppState;
use crate::config::Dialect;
use crate::db::{self, DbCallError};
use crate::error::MigrateError;
use crate::TRACKING_TABLE;

/// One row of the tracking table.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AppliedRow {
    pub name: String,
    pub checksum: String,
    pub applied_at: Value,
}

/// Dialect-specific DDL for the tracking table. Idempotent.
pub fn create_table_sql(dialect: Dialect) -> String {
    match dialect {
        Dialect::Postgres => format!(
            "CREATE TABLE IF NOT EXISTS {TRACKING_TABLE} (\
             name text PRIMARY KEY, \
             checksum text NOT NULL, \
             applied_at timestamptz NOT NULL DEFAULT now())"
        ),
        Dialect::Sqlite => format!(
            "CREATE TABLE IF NOT EXISTS {TRACKING_TABLE} (\
             name TEXT PRIMARY KEY, \
             checksum TEXT NOT NULL, \
             applied_at TEXT NOT NULL DEFAULT (datetime('now')))"
        ),
    }
}

/// Parameterized INSERT recording one migration as applied.
pub fn insert_sql(dialect: Dialect) -> String {
    match dialect {
        Dialect::Postgres => format!(
            "INSERT INTO {TRACKING_TABLE} (name, checksum, applied_at) VALUES ($1, $2, now())"
        ),
        Dialect::Sqlite => format!(
            "INSERT INTO {TRACKING_TABLE} (name, checksum, applied_at) VALUES (?, ?, datetime('now'))"
        ),
    }
}

/// Create the tracking table if it does not exist. The DDL is idempotent,
/// but two migrators racing on `CREATE TABLE IF NOT EXISTS` can still
/// collide inside Postgres (duplicate key on pg_type/pg_class) — if the
/// statement fails and the table exists anyway, the goal is met.
pub async fn ensure_table(state: &AppState) -> Result<(), MigrateError> {
    let dialect = state.dialect().await?;
    if let Err(e) = db::execute(
        &state.iii,
        &state.config.db,
        &create_table_sql(dialect),
        vec![],
    )
    .await
    {
        if !table_exists(state).await.unwrap_or(false) {
            return Err(e.into_migrate_error());
        }
        tracing::debug!(
            "tracking-table DDL raced a concurrent migrator; table exists — continuing"
        );
    }
    Ok(())
}

/// Does the tracking table exist? Dialect-specific introspection; both
/// queries are parameter-free so no placeholder-style divergence.
pub async fn table_exists(state: &AppState) -> Result<bool, MigrateError> {
    let dialect = state.dialect().await?;
    let sql = match dialect {
        Dialect::Sqlite => format!(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = '{TRACKING_TABLE}'"
        ),
        Dialect::Postgres => format!(
            "SELECT 1 AS one FROM information_schema.tables \
             WHERE table_name = '{TRACKING_TABLE}' AND table_schema = current_schema()"
        ),
    };
    let resp = db::query(&state.iii, &state.config.db, &sql, vec![])
        .await
        .map_err(DbCallError::into_migrate_error)?;
    Ok(!resp.rows.is_empty())
}

/// Fetch all applied migrations, sorted by name. Returns an empty list when
/// the tracking table does not exist yet (nothing was ever applied).
pub async fn fetch_applied(state: &AppState) -> Result<Vec<AppliedRow>, MigrateError> {
    if !table_exists(state).await? {
        return Ok(Vec::new());
    }
    let sql = format!("SELECT name, checksum, applied_at FROM {TRACKING_TABLE} ORDER BY name");
    let resp = db::query(&state.iii, &state.config.db, &sql, vec![])
        .await
        .map_err(DbCallError::into_migrate_error)?;

    let mut rows = Vec::with_capacity(resp.rows.len());
    for row in resp.rows {
        let name = row
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| MigrateError::ConfigError {
                message: format!("{TRACKING_TABLE} row missing `name`: {row:?}"),
            })?
            .to_string();
        let checksum = row
            .get("checksum")
            .and_then(Value::as_str)
            .ok_or_else(|| MigrateError::ConfigError {
                message: format!("{TRACKING_TABLE} row missing `checksum`: {row:?}"),
            })?
            .to_string();
        let applied_at = row.get("applied_at").cloned().unwrap_or(Value::Null);
        rows.push(AppliedRow {
            name,
            checksum,
            applied_at,
        });
    }
    Ok(rows)
}
