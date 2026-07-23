//! `migrate::status` — read-only report of applied / pending / mismatched
//! migrations. Never writes; safe to call at any time.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

use std::collections::BTreeSet;

use super::tracking::{self, AppliedRow};
use super::AppState;
use crate::error::MigrateError;
use crate::migrations::{self, MigrationFile};

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct StatusReq {}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PendingEntry {
    pub name: String,
    /// SHA-256 of the file as it exists on disk right now.
    pub checksum: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct MismatchedEntry {
    pub name: String,
    /// Checksum recorded in `_iii_migrations` at apply time.
    pub applied_checksum: String,
    /// Checksum of the file on disk now.
    pub file_checksum: String,
    pub applied_at: Value,
}

/// A migration file whose timestamp prefix is ahead of the wall clock —
/// almost certainly a hand-written name instead of `migrate::create`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FutureDatedEntry {
    pub name: String,
    /// Recorded in the tracking table (a drifted file counts as applied).
    pub applied: bool,
    /// What to do about it, spelled out.
    pub hint: String,
}

/// Remediation hints, keyed on whether the file was already applied.
const FUTURE_APPLIED_HINT: &str =
    "applied and unchanged: harmless, expires once the clock catches up";
const FUTURE_PENDING_HINT: &str = "pending: recreate it via migrate::create — its timestamps \
     are monotonic with existing files — or rename it by hand before apply";

/// Which of `files` are future-dated at `now`, and were they applied?
fn future_dated_entries(
    files: &[MigrationFile],
    applied_names: &BTreeSet<String>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<FutureDatedEntry> {
    files
        .iter()
        .filter(|f| migrations::is_future_dated(&f.name, now))
        .map(|f| {
            let applied = applied_names.contains(&f.name);
            FutureDatedEntry {
                name: f.name.clone(),
                applied,
                hint: if applied {
                    FUTURE_APPLIED_HINT.to_string()
                } else {
                    FUTURE_PENDING_HINT.to_string()
                },
            }
        })
        .collect()
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusResp {
    pub db: String,
    pub dialect: String,
    pub dir: String,
    /// Applied and unchanged since apply.
    pub applied: Vec<AppliedRow>,
    /// On disk, not yet applied.
    pub pending: Vec<PendingEntry>,
    /// Applied, but the file content changed since — `migrate::up` refuses
    /// to run while this list is non-empty.
    pub mismatched: Vec<MismatchedEntry>,
    /// Recorded as applied but the file no longer exists on disk.
    pub missing: Vec<AppliedRow>,
    /// Files on disk whose timestamp prefix is ahead of the wall clock
    /// (beyond skew tolerance), with an explicit remediation hint each:
    /// applied ones are harmless, pending ones should be re-scaffolded via
    /// `migrate::create` before apply.
    pub future_dated: Vec<FutureDatedEntry>,
}

pub async fn handle(state: &AppState, _req: StatusReq) -> Result<StatusResp, MigrateError> {
    let dialect = state.dialect().await?;
    let files = migrations::list_migrations(Path::new(&state.config.dir))?;
    let applied_rows = tracking::fetch_applied(state).await?;

    // The applied-name set survives the by-name map below, which the
    // classification loop consumes entry by entry.
    let applied_names: BTreeSet<String> = applied_rows.iter().map(|r| r.name.clone()).collect();
    let mut applied_by_name: BTreeMap<String, AppliedRow> = applied_rows
        .into_iter()
        .map(|r| (r.name.clone(), r))
        .collect();

    let mut applied = Vec::new();
    let mut pending = Vec::new();
    let mut mismatched = Vec::new();

    let future_dated = future_dated_entries(&files, &applied_names, chrono::Utc::now());
    if !future_dated.is_empty() {
        tracing::warn!(
            count = future_dated.len(),
            "migration files are dated in the future (hand-written names?)"
        );
    }

    for file in files {
        match applied_by_name.remove(&file.name) {
            None => pending.push(PendingEntry {
                name: file.name,
                checksum: file.checksum,
            }),
            Some(row) if row.checksum == file.checksum => applied.push(row),
            Some(row) => mismatched.push(MismatchedEntry {
                name: file.name,
                applied_checksum: row.checksum,
                file_checksum: file.checksum,
                applied_at: row.applied_at,
            }),
        }
    }

    // Whatever is left in the map was applied but has no file on disk.
    let missing: Vec<AppliedRow> = applied_by_name.into_values().collect();
    if !missing.is_empty() {
        tracing::warn!(
            count = missing.len(),
            "applied migrations have no file on disk (renamed or deleted?)"
        );
    }

    Ok(StatusResp {
        db: state.config.db.clone(),
        dialect: dialect.to_string(),
        dir: state.config.dir.clone(),
        applied,
        pending,
        mismatched,
        missing,
        future_dated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::timestamp_of;

    fn file(name: &str) -> MigrationFile {
        MigrationFile {
            name: name.into(),
            path: name.into(),
            checksum: "abc".into(),
        }
    }

    #[test]
    fn future_entries_carry_state_specific_hints() {
        let now = timestamp_of("20260723080000_now.sql").unwrap();
        let files = [
            file("20260101000000_past.sql"),
            file("20260723080400_within_skew.sql"),
            file("20260723120000_applied_future.sql"),
            file("20260723120001_pending_future.sql"),
        ];
        let applied: BTreeSet<String> = [
            "20260101000000_past.sql",
            "20260723120000_applied_future.sql",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let entries = future_dated_entries(&files, &applied, now);
        // Past and within-tolerance files are not flagged.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "20260723120000_applied_future.sql");
        assert!(entries[0].applied);
        assert!(entries[0].hint.contains("harmless"));
        assert_eq!(entries[1].name, "20260723120001_pending_future.sql");
        assert!(!entries[1].applied);
        assert!(entries[1].hint.contains("migrate::create"));
    }
}
