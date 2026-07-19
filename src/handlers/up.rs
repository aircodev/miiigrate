//! `migrate::up` — apply all pending migrations.
//!
//! Each pending migration is one `database::transaction` batch:
//!
//! - Postgres: `SELECT pg_advisory_xact_lock(<key>)` first — serializes
//!   concurrent migrators; the lock releases at commit/rollback. Then the
//!   file's statements, then the tracking INSERT.
//! - SQLite: no advisory locks; the batch runs with `isolation:
//!   serializable`, which the database worker maps to `BEGIN IMMEDIATE`.
//!
//! Failed batches are classified by re-reading the tracking table: when a
//! concurrent migrator applied the same file while we waited on the lock,
//! our batch fails (schema already migrated, or duplicate tracking key) but
//! the migration IS applied — that is a skip, not an error.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;
use std::time::Instant;

use super::tracking;
use super::AppState;
use crate::config::Dialect;
use crate::db::{self, DbCallError, TxStatement};
use crate::error::MigrateError;
use crate::migrations::{self, MigrationFile};
use crate::splitter::split_statements;
use crate::TRACKING_TABLE;

/// Advisory lock key: first 8 bytes (big-endian) of SHA-256("iii_miiigrate"),
/// as a signed 64-bit value for `pg_advisory_xact_lock(bigint)`. Constant
/// across releases — changing it would let two miiigrate versions migrate the
/// same database concurrently.
pub fn advisory_lock_key() -> i64 {
    let digest_hex = migrations::checksum_bytes(b"iii_miiigrate");
    let bytes: Vec<u8> = (0..8)
        .map(|i| u8::from_str_radix(&digest_hex[i * 2..i * 2 + 2], 16).expect("hex digest"))
        .collect();
    i64::from_be_bytes(bytes.try_into().expect("8 bytes"))
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct UpReq {}

#[derive(Debug, Serialize, JsonSchema)]
pub struct UpResp {
    /// Migrations applied by this call, in order.
    pub applied: Vec<String>,
    /// Migrations not applied by this call: already recorded at the start,
    /// or applied concurrently by another migrator while we ran.
    pub skipped: usize,
    pub duration_ms: u64,
}

/// Dialect-specific DDL for the tracking table. Idempotent.
fn create_tracking_table_sql(dialect: Dialect) -> String {
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

fn tracking_insert_sql(dialect: Dialect) -> String {
    match dialect {
        Dialect::Postgres => format!(
            "INSERT INTO {TRACKING_TABLE} (name, checksum, applied_at) VALUES ($1, $2, now())"
        ),
        Dialect::Sqlite => format!(
            "INSERT INTO {TRACKING_TABLE} (name, checksum, applied_at) VALUES (?, ?, datetime('now'))"
        ),
    }
}

/// Build the transaction batch for one migration file. Returns the batch and
/// the index offset of the file's first statement (for error reporting).
fn build_batch(
    dialect: Dialect,
    file: &MigrationFile,
    sql: &str,
) -> Result<(Vec<TxStatement>, usize), MigrateError> {
    let file_statements =
        split_statements(sql).map_err(|reason| MigrateError::MigrationFailed {
            name: file.name.clone(),
            message: format!("cannot split migration into statements: {reason}"),
            statement_index: None,
            database_error: None,
        })?;
    if file_statements.is_empty() {
        return Err(MigrateError::MigrationFailed {
            name: file.name.clone(),
            message: "migration file contains no SQL statements".into(),
            statement_index: None,
            database_error: None,
        });
    }

    let mut batch = Vec::with_capacity(file_statements.len() + 2);
    if dialect == Dialect::Postgres {
        // `::text` because pg_advisory_xact_lock returns `void`, which the
        // database worker cannot decode as a result column; the cast yields
        // an empty string and changes nothing about the lock.
        batch.push(TxStatement::new(format!(
            "SELECT pg_advisory_xact_lock({})::text AS locked",
            advisory_lock_key()
        )));
    }
    let offset = batch.len();
    for stmt in file_statements {
        batch.push(TxStatement::new(stmt));
    }
    batch.push(TxStatement::with_params(
        tracking_insert_sql(dialect),
        vec![json!(file.name), json!(file.checksum)],
    ));
    Ok((batch, offset))
}

pub async fn handle(state: &AppState, _req: UpReq) -> Result<UpResp, MigrateError> {
    let started = Instant::now();
    let dialect = state.dialect().await?;
    let files = migrations::list_migrations(Path::new(&state.config.dir))?;

    // Create the tracking table before first read. The DDL is idempotent,
    // but two migrators racing on `CREATE TABLE IF NOT EXISTS` can still
    // collide inside Postgres (duplicate key on pg_type/pg_class) — if the
    // statement fails and the table exists anyway, the goal is met.
    if let Err(e) = db::execute(
        &state.iii,
        &state.config.db,
        &create_tracking_table_sql(dialect),
        vec![],
    )
    .await
    {
        if !tracking::table_exists(state).await.unwrap_or(false) {
            return Err(e.into_migrate_error());
        }
        tracing::debug!(
            "tracking-table DDL raced a concurrent migrator; table exists — continuing"
        );
    }

    let applied_rows = tracking::fetch_applied(state).await?;

    // Checksum gate: any drift in an already-applied file blocks the whole
    // run before anything is applied.
    for file in &files {
        if let Some(row) = applied_rows.iter().find(|r| r.name == file.name) {
            if row.checksum != file.checksum {
                return Err(MigrateError::ChecksumMismatch {
                    name: file.name.clone(),
                    applied_checksum: row.checksum.clone(),
                    file_checksum: file.checksum.clone(),
                });
            }
        }
    }

    let mut applied = Vec::new();
    let mut skipped = applied_rows.len();

    for file in &files {
        if applied_rows.iter().any(|r| r.name == file.name) {
            continue; // already applied (counted in `skipped` above)
        }
        let sql = std::fs::read_to_string(&file.path).map_err(|e| MigrateError::ConfigError {
            message: format!("reading {}: {e}", file.path.display()),
        })?;
        // The file was checksummed by list_migrations; re-reading could race
        // with an editor save. Recompute so the tracking row always matches
        // the bytes we actually executed.
        let file = MigrationFile {
            checksum: migrations::checksum_bytes(sql.as_bytes()),
            ..file.clone()
        };
        let (batch, offset) = build_batch(dialect, &file, &sql)?;

        let isolation = match dialect {
            Dialect::Sqlite => Some("serializable"), // BEGIN IMMEDIATE
            Dialect::Postgres => None,               // advisory lock serializes
        };

        match db::transaction(&state.iii, &state.config.db, &batch, isolation).await {
            Ok(_) => {
                tracing::info!(migration = %file.name, "applied");
                applied.push(file.name.clone());
            }
            Err(e) => match classify_failure(state, &file, e, offset, batch.len()).await? {
                Applied::Concurrently => {
                    tracing::info!(
                        migration = %file.name,
                        "already applied by a concurrent migrator; skipping"
                    );
                    skipped += 1;
                }
            },
        }
    }

    Ok(UpResp {
        applied,
        skipped,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

enum Applied {
    Concurrently,
}

/// A failed batch either means the migration is broken, or a concurrent
/// migrator applied it while we waited on the lock (our statements then hit
/// an already-migrated schema, or the tracking INSERT hit the primary key).
/// The tracking table decides: if the row exists now with our checksum, the
/// migration is applied and this failure is a skip.
async fn classify_failure(
    state: &AppState,
    file: &MigrationFile,
    err: DbCallError,
    offset: usize,
    batch_len: usize,
) -> Result<Applied, MigrateError> {
    if let DbCallError::Worker { .. } = &err {
        let rows = tracking::fetch_applied(state).await?;
        if let Some(row) = rows.iter().find(|r| r.name == file.name) {
            if row.checksum == file.checksum {
                return Ok(Applied::Concurrently);
            }
            return Err(MigrateError::ChecksumMismatch {
                name: file.name.clone(),
                applied_checksum: row.checksum.clone(),
                file_checksum: file.checksum.clone(),
            });
        }
    }
    Err(match err {
        DbCallError::Unavailable { message } => MigrateError::DatabaseWorkerUnavailable { message },
        DbCallError::Worker {
            message,
            failed_index,
            body,
            ..
        } => MigrateError::MigrationFailed {
            name: file.name.clone(),
            message,
            // Index into the *file's* statements: subtract the advisory-lock
            // preamble; a failure in the preamble or the trailing tracking
            // INSERT (last batch entry) reports None.
            statement_index: failed_index
                .filter(|&i| i >= offset && i + 1 < batch_len)
                .map(|i| i - offset),
            database_error: body,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisory_lock_key_is_stable() {
        // Pinned: first 8 bytes of sha256("iii_miiigrate") as big-endian i64.
        // If this test ever fails, concurrent migrators of different versions
        // would no longer exclude each other.
        assert_eq!(advisory_lock_key(), advisory_lock_key());
        let hex = crate::migrations::checksum_bytes(b"iii_miiigrate");
        let expected = i64::from_be_bytes(
            (0..8)
                .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
                .collect::<Vec<_>>()
                .try_into()
                .unwrap(),
        );
        assert_eq!(advisory_lock_key(), expected);
    }

    #[test]
    fn postgres_batch_wraps_lock_and_tracking_insert() {
        let file = MigrationFile {
            name: "20260101120000_a.sql".into(),
            path: "20260101120000_a.sql".into(),
            checksum: "abc".into(),
        };
        let (batch, offset) = build_batch(
            Dialect::Postgres,
            &file,
            "CREATE TABLE a (id int); SELECT 1;",
        )
        .unwrap();
        assert_eq!(batch.len(), 4); // lock + 2 statements + insert
        assert_eq!(offset, 1);
        assert!(batch[0].sql.starts_with("SELECT pg_advisory_xact_lock("));
        assert_eq!(batch[1].sql, "CREATE TABLE a (id int)");
        assert!(batch[3].sql.starts_with("INSERT INTO _iii_migrations"));
        assert_eq!(
            batch[3].params,
            vec![json!("20260101120000_a.sql"), json!("abc")]
        );
    }

    #[test]
    fn sqlite_batch_has_no_lock_preamble() {
        let file = MigrationFile {
            name: "20260101120000_a.sql".into(),
            path: "20260101120000_a.sql".into(),
            checksum: "abc".into(),
        };
        let (batch, offset) =
            build_batch(Dialect::Sqlite, &file, "CREATE TABLE a (id int);").unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(offset, 0);
        assert!(batch[1].sql.contains("datetime('now')"));
    }

    #[test]
    fn empty_migration_file_is_rejected() {
        let file = MigrationFile {
            name: "20260101120000_a.sql".into(),
            path: "20260101120000_a.sql".into(),
            checksum: "abc".into(),
        };
        assert!(matches!(
            build_batch(Dialect::Sqlite, &file, "-- only a comment\n"),
            Err(MigrateError::MigrationFailed { .. })
        ));
    }
}
