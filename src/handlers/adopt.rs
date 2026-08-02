//! `migrate::adopt` — take over the migration history of another tool.
//!
//! v1 supports `source: "drizzle"`; the payload shape is deliberately
//! discriminated so other sources (prisma, …) can land later without
//! breaking callers.
//!
//! The flow, for drizzle:
//!
//! 1. Read `<from>/meta/_journal.json` — order and creation dates.
//! 2. Convert every entry's `.sql` file into the miiigrate directory as
//!    `YYYYMMDDHHMMSS_slug.sql` (timestamp from the journal's `when`),
//!    bytes copied verbatim. The source folder is never touched.
//! 3. Decide which converted files are already applied in the database
//!    (`mark_applied`), and record those through the baseline logic —
//!    without executing their SQL. The rest stays pending for
//!    `migrate::up`.
//!
//! In `auto` mode the drizzle tracking table (`__drizzle_migrations`) is
//! read to find what drizzle already applied; each matched row's `hash` is
//! compared to the file's SHA-256 and any divergence is reported as a
//! warning (`hash_warnings`) — a warning, not an error, because the copy
//! being adopted is the version the repository says is true.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;

use super::baseline;
use super::AppState;
use crate::config::Dialect;
use crate::db::{self, DbCallError};
use crate::drizzle::{self, PlannedItem};
use crate::error::MigrateError;
use crate::migrations::MigrationFile;

/// Migration tool whose history is being adopted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AdoptSource {
    Drizzle,
}

/// Which converted migrations to record as already applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MarkApplied {
    /// Read the source tool's own tracking table and mirror it: entries it
    /// recorded as applied are baselined, the rest stays pending. A missing
    /// tracking table means a fresh database — nothing is baselined.
    #[default]
    Auto,
    /// Baseline every converted migration (the schema is known to be fully
    /// up to date, e.g. the tracking table was already dropped).
    All,
    /// Baseline nothing; only convert the files. `migrate::up` would then
    /// try to execute all of them — for empty databases.
    None,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AdoptReq {
    /// Migration tool to adopt from. Only `drizzle` today.
    pub source: AdoptSource,
    /// Source directory — drizzle-kit's `out`. Default: `./drizzle`.
    #[serde(default)]
    pub from: Option<String>,
    /// Default: `auto`.
    #[serde(default)]
    pub mark_applied: Option<MarkApplied>,
}

/// A converted file whose content no longer matches the hash the source
/// tool recorded at apply time.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HashWarning {
    /// Converted file name.
    pub name: String,
    /// Source tag (drizzle journal entry).
    pub tag: String,
    /// Hash recorded by the source tool at apply time.
    pub recorded_hash: String,
    /// SHA-256 of the file as adopted.
    pub file_hash: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AdoptResp {
    /// Files written into the migrations directory by this call. A re-run
    /// over an already-converted folder writes nothing and reports nothing.
    pub converted: Vec<String>,
    /// Converted migrations recorded as applied without executing their SQL.
    pub baselined: Vec<String>,
    /// Converted migrations left for `migrate::up`.
    pub pending: Vec<String>,
    /// Rows already tracked before this call (idempotent re-runs).
    pub skipped: usize,
    /// Content drift between the adopted files and the hashes the source
    /// tool recorded. Non-fatal: the files on disk win.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hash_warnings: Vec<HashWarning>,
}

const DEFAULT_DRIZZLE_DIR: &str = "./drizzle";

/// A row of drizzle's own tracking table.
struct DrizzleRow {
    hash: String,
    /// Millisecond timestamp; equals the journal entry's `when`.
    created_at: i64,
}

/// Read `__drizzle_migrations`. `Ok(None)` when the table does not exist —
/// a database drizzle never migrated. Postgres keeps it in the `drizzle`
/// schema by default; SQLite has no schemas.
async fn fetch_drizzle_rows(state: &AppState) -> Result<Option<Vec<DrizzleRow>>, MigrateError> {
    let dialect = state.dialect().await?;
    let exists_sql = match dialect {
        Dialect::Postgres => {
            "SELECT 1 AS one FROM information_schema.tables \
             WHERE table_name = '__drizzle_migrations' AND table_schema = 'drizzle'"
        }
        Dialect::Sqlite => {
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = '__drizzle_migrations'"
        }
    };
    let resp = db::query(&state.iii, &state.config.db, exists_sql, vec![])
        .await
        .map_err(DbCallError::into_migrate_error)?;
    if resp.rows.is_empty() {
        return Ok(None);
    }

    let rows_sql = match dialect {
        Dialect::Postgres => {
            "SELECT hash, created_at FROM drizzle.__drizzle_migrations ORDER BY created_at"
        }
        Dialect::Sqlite => "SELECT hash, created_at FROM __drizzle_migrations ORDER BY created_at",
    };
    let resp = db::query(&state.iii, &state.config.db, rows_sql, vec![])
        .await
        .map_err(DbCallError::into_migrate_error)?;

    let mut rows = Vec::with_capacity(resp.rows.len());
    for row in resp.rows {
        let hash = row
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // `created_at` is a bigint of millis; drivers may surface it as a
        // JSON number or a string.
        let created_at = match row.get("created_at") {
            Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
            Some(Value::String(s)) => s.parse().unwrap_or(0),
            _ => 0,
        };
        rows.push(DrizzleRow { hash, created_at });
    }
    Ok(Some(rows))
}

/// Split the converted items into (already applied per drizzle, pending),
/// collecting hash warnings for the applied ones. Matching key is the
/// journal `when` — drizzle writes it as `created_at` when it applies an
/// entry.
fn partition_applied<'a>(
    items: &'a [PlannedItem],
    rows: &[DrizzleRow],
) -> (Vec<&'a PlannedItem>, Vec<&'a PlannedItem>, Vec<HashWarning>) {
    let mut applied = Vec::new();
    let mut pending = Vec::new();
    let mut warnings = Vec::new();
    for item in items {
        match rows.iter().find(|r| r.created_at == item.when) {
            Some(row) => {
                if row.hash != item.sha256_raw {
                    warnings.push(HashWarning {
                        name: item.target_name.clone(),
                        tag: item.tag.clone(),
                        recorded_hash: row.hash.clone(),
                        file_hash: item.sha256_raw.clone(),
                    });
                }
                applied.push(item);
            }
            None => pending.push(item),
        }
    }
    (applied, pending, warnings)
}

pub async fn handle(state: &AppState, req: AdoptReq) -> Result<AdoptResp, MigrateError> {
    // Exhaustive on purpose: a future `prisma` variant must be routed here.
    match req.source {
        AdoptSource::Drizzle => {}
    }
    let from = req.from.as_deref().unwrap_or(DEFAULT_DRIZZLE_DIR);
    let from = Path::new(from);
    let mark = req.mark_applied.unwrap_or_default();

    let journal = drizzle::read_journal(from)?;
    if journal.entries.is_empty() {
        return Err(MigrateError::AdoptSourceInvalid {
            path: from.display().to_string(),
            reason: "the drizzle journal has no entries — nothing to adopt".into(),
        });
    }

    // The journal's dialect must match the target database: adopting a
    // postgres history into a sqlite db (or vice versa) can only mislead.
    let db_dialect = state.dialect().await?;
    if journal.dialect != db_dialect {
        return Err(MigrateError::AdoptSourceInvalid {
            path: from.display().to_string(),
            reason: format!(
                "journal dialect is {} but the target database is {db_dialect}",
                journal.dialect
            ),
        });
    }

    let items = drizzle::plan_conversion(from, &journal)?;
    let converted = drizzle::write_converted(Path::new(&state.config.dir), &items)?;

    let (to_baseline, pending_items, hash_warnings) = match mark {
        MarkApplied::All => (items.iter().collect(), Vec::new(), Vec::new()),
        MarkApplied::None => (Vec::new(), items.iter().collect(), Vec::new()),
        MarkApplied::Auto => match fetch_drizzle_rows(state).await? {
            None => {
                tracing::info!(
                    "no __drizzle_migrations table found — fresh database, nothing to baseline"
                );
                (Vec::new(), items.iter().collect(), Vec::new())
            }
            Some(rows) => partition_applied(&items, &rows),
        },
    };
    for w in &hash_warnings {
        tracing::warn!(
            file = %w.name,
            tag = %w.tag,
            recorded_hash = %w.recorded_hash,
            file_hash = %w.file_hash,
            "adopted file differs from the content drizzle recorded at apply \
             time; the file on disk is what miiigrate will trust"
        );
    }

    let targets: Vec<MigrationFile> = to_baseline
        .iter()
        .map(|item| MigrationFile {
            name: item.target_name.clone(),
            path: Path::new(&state.config.dir).join(&item.target_name),
            checksum: item.checksum.clone(),
        })
        .collect();
    let (baselined, skipped) = if targets.is_empty() {
        (Vec::new(), 0)
    } else {
        baseline::baseline_files(state, &targets).await?
    };

    Ok(AdoptResp {
        converted,
        baselined,
        pending: pending_items
            .iter()
            .map(|i| i.target_name.clone())
            .collect(),
        skipped,
        hash_warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(tag: &str, when: i64, target: &str, sha: &str) -> PlannedItem {
        PlannedItem {
            tag: tag.into(),
            when,
            source: format!("{tag}.sql").into(),
            target_name: target.into(),
            bytes: Vec::new(),
            sha256_raw: sha.into(),
            checksum: format!("norm-{sha}"),
        }
    }

    #[test]
    fn partition_matches_on_created_at_and_flags_hash_drift() {
        let items = vec![
            item("0000_a", 1000, "20240101000000_a.sql", "hash-a"),
            item("0001_b", 2000, "20240101000001_b.sql", "hash-b"),
            item("0002_c", 3000, "20240101000002_c.sql", "hash-c"),
        ];
        let rows = vec![
            DrizzleRow {
                hash: "hash-a".into(),
                created_at: 1000,
            },
            DrizzleRow {
                hash: "OTHER".into(), // drifted content
                created_at: 2000,
            },
            // 3000 never applied by drizzle
        ];
        let (applied, pending, warnings) = partition_applied(&items, &rows);
        assert_eq!(applied.len(), 2);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].tag, "0002_c");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].tag, "0001_b");
        assert_eq!(warnings[0].recorded_hash, "OTHER");
        assert_eq!(warnings[0].file_hash, "hash-b");
    }

    #[test]
    fn adopt_req_parses_the_documented_payload() {
        let req: AdoptReq =
            serde_json::from_str(r#"{"source": "drizzle", "from": "./db/drizzle"}"#).unwrap();
        assert_eq!(req.source, AdoptSource::Drizzle);
        assert_eq!(req.from.as_deref(), Some("./db/drizzle"));
        assert_eq!(req.mark_applied, None);

        let req: AdoptReq =
            serde_json::from_str(r#"{"source": "drizzle", "mark_applied": "all"}"#).unwrap();
        assert_eq!(req.mark_applied, Some(MarkApplied::All));

        // Unknown sources fail loudly — prisma will be a new enum variant.
        assert!(serde_json::from_str::<AdoptReq>(r#"{"source": "prisma"}"#).is_err());
        assert!(serde_json::from_str::<AdoptReq>(r#"{}"#).is_err());
    }
}
