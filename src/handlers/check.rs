//! `migrate::check` — static validation of the migrations directory.
//!
//! Answers "would `migrate::up` succeed?" without executing any SQL: name
//! scheme, statement splitting, empty files, checksum drift. The only
//! network call is the tracking-table read, and it degrades gracefully —
//! when the database worker is unreachable the static checks still run and
//! `db_checked: false` says the drift lists are unknown.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

use super::status::{future_dated_entries, FutureDatedEntry, MismatchedEntry};
use super::tracking::{self, AppliedRow};
use super::AppState;
use crate::error::MigrateError;
use crate::migrations::{self, InvalidFile, MigrationScan};
use crate::splitter::split_statements;

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct CheckReq {}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PendingCheck {
    pub name: String,
    pub ok: bool,
    /// Number of statements the file splits into (0 when not ok).
    pub statements: usize,
    /// Why the file would fail `migrate::up`: splitter error, or empty file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CheckResp {
    /// False when anything would block or break `migrate::up`: an invalid
    /// name, an unsplittable or empty pending file, or checksum drift.
    /// `future_dated` and `missing` are warnings and do not flip this.
    pub ok: bool,
    pub dir: String,
    /// Validation of each pending file (every file on disk when
    /// `db_checked` is false), in apply order.
    pub pending_checked: Vec<PendingCheck>,
    /// `.sql` files that do not follow the naming scheme — `migrate::up`
    /// refuses to run while any exist.
    pub invalid_names: Vec<InvalidFile>,
    pub future_dated: Vec<FutureDatedEntry>,
    /// Applied files whose content changed on disk — blocks `migrate::up`.
    pub mismatched: Vec<MismatchedEntry>,
    /// Recorded as applied but no longer on disk (warning).
    pub missing: Vec<String>,
    /// False when the database worker was unreachable: static checks only,
    /// `mismatched` and `missing` are then unknown (empty).
    pub db_checked: bool,
}

/// Validate one migration's SQL exactly like `migrate::up` would before
/// executing: it must split cleanly and contain at least one statement.
fn check_sql(name: &str, sql: &str) -> PendingCheck {
    match split_statements(sql) {
        Err(reason) => PendingCheck {
            name: name.to_string(),
            ok: false,
            statements: 0,
            error: Some(format!("cannot split migration into statements: {reason}")),
        },
        Ok(statements) if statements.is_empty() => PendingCheck {
            name: name.to_string(),
            ok: false,
            statements: 0,
            error: Some("migration file contains no SQL statements".into()),
        },
        Ok(statements) => PendingCheck {
            name: name.to_string(),
            ok: true,
            statements: statements.len(),
            error: None,
        },
    }
}

/// Pure report assembly. `contents` holds `(name, sql)` for each file to
/// validate, in apply order; `applied` is `None` when the tracking table
/// could not be read (database worker down).
fn build_report(
    dir: &str,
    scan: &MigrationScan,
    contents: &[(String, String)],
    applied: Option<&[AppliedRow]>,
    now: chrono::DateTime<chrono::Utc>,
) -> CheckResp {
    let applied_names: BTreeSet<String> = applied
        .map(|rows| rows.iter().map(|r| r.name.clone()).collect())
        .unwrap_or_default();

    let pending_checked: Vec<PendingCheck> = contents
        .iter()
        .map(|(name, sql)| check_sql(name, sql))
        .collect();

    let mut mismatched = Vec::new();
    let mut missing = Vec::new();
    if let Some(rows) = applied {
        for row in rows {
            match scan.valid.iter().find(|f| f.name == row.name) {
                None => missing.push(row.name.clone()),
                Some(file) if file.checksum != row.checksum => mismatched.push(MismatchedEntry {
                    name: file.name.clone(),
                    applied_checksum: row.checksum.clone(),
                    file_checksum: file.checksum.clone(),
                    applied_at: row.applied_at.clone(),
                }),
                Some(_) => {}
            }
        }
    }

    let ok =
        scan.invalid.is_empty() && pending_checked.iter().all(|c| c.ok) && mismatched.is_empty();

    CheckResp {
        ok,
        dir: dir.to_string(),
        pending_checked,
        invalid_names: scan.invalid.clone(),
        future_dated: future_dated_entries(&scan.valid, &applied_names, now),
        mismatched,
        missing,
        db_checked: applied.is_some(),
    }
}

pub async fn handle(state: &AppState, _req: CheckReq) -> Result<CheckResp, MigrateError> {
    let dir = &state.config.dir;
    let scan = migrations::scan_migrations(Path::new(dir))?;

    // Best-effort tracking read — the single network call of this handler.
    // A missing database worker must not take static validation down with
    // it; real errors (bad config, unsupported dialect) still propagate.
    let applied = match tracking::fetch_applied(state).await {
        Ok(rows) => Some(rows),
        Err(MigrateError::DatabaseWorkerUnavailable { message }) => {
            tracing::warn!(error = %message, "database worker unreachable; static checks only");
            None
        }
        Err(e) => return Err(e),
    };

    let applied_names: BTreeSet<&str> = applied
        .as_deref()
        .map(|rows| rows.iter().map(|r| r.name.as_str()).collect())
        .unwrap_or_default();

    let mut contents = Vec::new();
    for file in &scan.valid {
        if applied_names.contains(file.name.as_str()) {
            continue; // applied files are covered by the checksum gate
        }
        let sql = std::fs::read_to_string(&file.path).map_err(|e| MigrateError::ConfigError {
            message: format!("reading {}: {e}", file.path.display()),
        })?;
        contents.push((file.name.clone(), sql));
    }

    Ok(build_report(
        dir,
        &scan,
        &contents,
        applied.as_deref(),
        chrono::Utc::now(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::scan_migrations;
    use serde_json::json;

    fn row(name: &str, checksum: &str) -> AppliedRow {
        AppliedRow {
            name: name.into(),
            checksum: checksum.into(),
            applied_at: json!("2026-07-23T00:00:00Z"),
        }
    }

    #[test]
    fn valid_pending_file_reports_statement_count() {
        let report = check_sql("20260101000000_a.sql", "CREATE TABLE a (id int); SELECT 1;");
        assert!(report.ok);
        assert_eq!(report.statements, 2);
        assert!(report.error.is_none());
    }

    #[test]
    fn unterminated_dollar_quote_and_empty_files_fail() {
        let bad = check_sql("20260101000000_a.sql", "CREATE FUNCTION f() AS $$ BEGIN");
        assert!(!bad.ok);
        assert!(bad.error.unwrap().contains("cannot split"));

        let empty = check_sql("20260101000000_b.sql", "-- only a comment\n");
        assert!(!empty.ok);
        assert!(empty.error.unwrap().contains("no SQL statements"));
    }

    #[test]
    fn stray_file_flips_ok_but_does_not_abort() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("20260101000000_a.sql"), "SELECT 1;").unwrap();
        std::fs::write(tmp.path().join("notes.sql"), "x").unwrap();
        let scan = scan_migrations(tmp.path()).unwrap();

        let now = crate::migrations::timestamp_of("20260723080000_now.sql").unwrap();
        let contents = vec![("20260101000000_a.sql".into(), "SELECT 1;".into())];
        let report = build_report("./migrations", &scan, &contents, Some(&[]), now);

        assert!(!report.ok);
        assert_eq!(report.invalid_names.len(), 1);
        assert_eq!(report.pending_checked.len(), 1);
        assert!(report.pending_checked[0].ok);
        assert!(report.db_checked);
    }

    #[test]
    fn drift_and_missing_are_classified() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("20260101000000_a.sql"), "SELECT 1;").unwrap();
        let scan = scan_migrations(tmp.path()).unwrap();
        let on_disk = scan.valid[0].checksum.clone();

        let applied = [
            row("20260101000000_a.sql", "different-checksum"),
            row("20260102000000_gone.sql", "whatever"),
        ];
        let now = crate::migrations::timestamp_of("20260723080000_now.sql").unwrap();
        let report = build_report("./migrations", &scan, &[], Some(&applied), now);

        assert!(!report.ok); // mismatched blocks up
        assert_eq!(report.mismatched.len(), 1);
        assert_eq!(report.mismatched[0].file_checksum, on_disk);
        assert_eq!(report.missing, vec!["20260102000000_gone.sql".to_string()]);
    }

    #[test]
    fn database_down_degrades_to_static_checks() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("20260723120001_future.sql"), "SELECT 1;").unwrap();
        let scan = scan_migrations(tmp.path()).unwrap();

        let now = crate::migrations::timestamp_of("20260723080000_now.sql").unwrap();
        let contents = vec![("20260723120001_future.sql".into(), "SELECT 1;".into())];
        let report = build_report("./migrations", &scan, &contents, None, now);

        assert!(!report.db_checked);
        assert!(report.mismatched.is_empty() && report.missing.is_empty());
        // future_dated is a warning: ok stays true.
        assert!(report.ok);
        assert_eq!(report.future_dated.len(), 1);
        assert!(!report.future_dated[0].applied);
    }
}
