//! `migrate::baseline` — record migrations as applied without executing
//! their SQL.
//!
//! The primitive behind adopting a database whose schema was built by
//! another tool (drizzle, prisma, by hand): the schema already exists, so
//! replaying the files would fail — instead the tracking rows are inserted
//! directly, and `migrate::up` treats those files as applied from now on.
//!
//! All inserts run in one `database::transaction` batch, serialized against
//! concurrent migrators exactly like `migrate::up`: Postgres takes the same
//! advisory lock, SQLite runs the batch as `serializable` (`BEGIN
//! IMMEDIATE`). Idempotent: a file already tracked with the same checksum is
//! a skip; a different checksum is `CHECKSUM_MISMATCH`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;

use super::tracking;
use super::up::advisory_lock_key;
use super::AppState;
use crate::config::Dialect;
use crate::db::{self, DbCallError, TxStatement};
use crate::error::MigrateError;
use crate::migrations::{self, MigrationFile};

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct BaselineReq {
    /// Migration file names to record as applied (e.g.
    /// `["20260719143000_baseline.sql"]`). Each must exist in the
    /// migrations directory. Default: every pending migration.
    #[serde(default)]
    pub names: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BaselineResp {
    /// Migrations recorded as applied by this call, in order. Their SQL was
    /// NOT executed.
    pub baselined: Vec<String>,
    /// Targets already tracked with an identical checksum.
    pub skipped: usize,
}

/// Build the single transaction batch: advisory-lock preamble on Postgres,
/// then one tracking INSERT per file.
fn build_batch(dialect: Dialect, files: &[MigrationFile]) -> Vec<TxStatement> {
    let mut batch = Vec::with_capacity(files.len() + 1);
    if dialect == Dialect::Postgres {
        batch.push(TxStatement::new(format!(
            "SELECT pg_advisory_xact_lock({})::text AS locked",
            advisory_lock_key()
        )));
    }
    for file in files {
        batch.push(TxStatement::with_params(
            tracking::insert_sql(dialect),
            vec![json!(file.name), json!(file.checksum)],
        ));
    }
    batch
}

/// Record `targets` as applied without executing them. Shared with
/// `migrate::adopt`. Returns `(baselined, skipped)`.
pub async fn baseline_files(
    state: &AppState,
    targets: &[MigrationFile],
) -> Result<(Vec<String>, usize), MigrateError> {
    let dialect = state.dialect().await?;
    tracking::ensure_table(state).await?;
    let applied = tracking::fetch_applied(state).await?;

    let mut to_insert: Vec<&MigrationFile> = Vec::new();
    let mut skipped = 0usize;
    for file in targets {
        match applied.iter().find(|r| r.name == file.name) {
            Some(row) if row.checksum == file.checksum => skipped += 1,
            Some(row) => {
                return Err(MigrateError::ChecksumMismatch {
                    name: file.name.clone(),
                    applied_checksum: row.checksum.clone(),
                    file_checksum: file.checksum.clone(),
                })
            }
            None => to_insert.push(file),
        }
    }
    if to_insert.is_empty() {
        return Ok((Vec::new(), skipped));
    }

    let batch = build_batch(
        dialect,
        &to_insert.iter().map(|f| (*f).clone()).collect::<Vec<_>>(),
    );
    let isolation = match dialect {
        Dialect::Sqlite => Some("serializable"), // BEGIN IMMEDIATE
        Dialect::Postgres => None,               // advisory lock serializes
    };
    match db::transaction(&state.iii, &state.config.db, &batch, isolation).await {
        Ok(_) => {
            let baselined: Vec<String> = to_insert.iter().map(|f| f.name.clone()).collect();
            for name in &baselined {
                tracing::info!(migration = %name, "baselined (recorded as applied, SQL not executed)");
            }
            Ok((baselined, skipped))
        }
        Err(err) => classify_failure(state, &to_insert, err, skipped).await,
    }
}

/// A failed batch either means the database rejected the inserts, or a
/// concurrent baseliner recorded the same files while we waited on the lock
/// (our INSERT then hits the primary key). The tracking table decides, same
/// policy as `migrate::up`.
async fn classify_failure(
    state: &AppState,
    to_insert: &[&MigrationFile],
    err: DbCallError,
    skipped_before: usize,
) -> Result<(Vec<String>, usize), MigrateError> {
    if let DbCallError::Worker { .. } = &err {
        let rows = tracking::fetch_applied(state).await?;
        let mut all_present = true;
        for file in to_insert {
            match rows.iter().find(|r| r.name == file.name) {
                Some(row) if row.checksum == file.checksum => {}
                Some(row) => {
                    return Err(MigrateError::ChecksumMismatch {
                        name: file.name.clone(),
                        applied_checksum: row.checksum.clone(),
                        file_checksum: file.checksum.clone(),
                    })
                }
                None => {
                    all_present = false;
                    break;
                }
            }
        }
        if all_present {
            tracing::info!("baseline rows already recorded by a concurrent migrator; skipping");
            return Ok((Vec::new(), skipped_before + to_insert.len()));
        }
    }
    Err(err.into_migrate_error())
}

pub async fn handle(state: &AppState, req: BaselineReq) -> Result<BaselineResp, MigrateError> {
    let files = migrations::list_migrations(Path::new(&state.config.dir))?;

    let targets: Vec<MigrationFile> = match &req.names {
        None => files,
        Some(names) => {
            let mut targets = Vec::with_capacity(names.len());
            for name in names {
                let file = files.iter().find(|f| &f.name == name).ok_or_else(|| {
                    MigrateError::ConfigError {
                        message: format!(
                            "baseline target `{name}` not found in {}",
                            state.config.dir
                        ),
                    }
                })?;
                targets.push(file.clone());
            }
            // Tracking rows land in apply order regardless of payload order.
            targets.sort_by(|a, b| a.name.cmp(&b.name));
            targets.dedup_by(|a, b| a.name == b.name);
            targets
        }
    };

    let (baselined, skipped) = baseline_files(state, &targets).await?;
    Ok(BaselineResp { baselined, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, checksum: &str) -> MigrationFile {
        MigrationFile {
            name: name.into(),
            path: name.into(),
            checksum: checksum.into(),
        }
    }

    #[test]
    fn postgres_batch_is_lock_then_inserts() {
        let files = vec![
            file("20260101120000_a.sql", "aaa"),
            file("20260102120000_b.sql", "bbb"),
        ];
        let batch = build_batch(Dialect::Postgres, &files);
        assert_eq!(batch.len(), 3);
        assert!(batch[0].sql.starts_with("SELECT pg_advisory_xact_lock("));
        assert!(batch[1].sql.starts_with("INSERT INTO _iii_migrations"));
        assert_eq!(
            batch[1].params,
            vec![json!("20260101120000_a.sql"), json!("aaa")]
        );
        assert_eq!(
            batch[2].params,
            vec![json!("20260102120000_b.sql"), json!("bbb")]
        );
    }

    #[test]
    fn sqlite_batch_has_no_lock_preamble() {
        let files = vec![file("20260101120000_a.sql", "aaa")];
        let batch = build_batch(Dialect::Sqlite, &files);
        assert_eq!(batch.len(), 1);
        assert!(batch[0].sql.contains("datetime('now')"));
    }

    #[test]
    fn no_migration_sql_ever_enters_the_batch() {
        // The whole point of baseline: only tracking INSERTs (and the pg
        // lock) reach the database.
        let files = vec![file("20260101120000_a.sql", "aaa")];
        for dialect in [Dialect::Postgres, Dialect::Sqlite] {
            for stmt in build_batch(dialect, &files) {
                assert!(
                    stmt.sql.starts_with("INSERT INTO _iii_migrations")
                        || stmt.sql.starts_with("SELECT pg_advisory_xact_lock"),
                    "unexpected statement: {}",
                    stmt.sql
                );
            }
        }
    }
}
