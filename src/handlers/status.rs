//! `migrate::status` — read-only report of applied / pending / mismatched
//! migrations. Never writes; safe to call at any time.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

use super::tracking::{self, AppliedRow};
use super::AppState;
use crate::error::MigrateError;
use crate::migrations;

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
}

pub async fn handle(state: &AppState, _req: StatusReq) -> Result<StatusResp, MigrateError> {
    let dialect = state.dialect().await?;
    let files = migrations::list_migrations(Path::new(&state.config.dir))?;
    let applied_rows = tracking::fetch_applied(state).await?;

    let mut applied_by_name: BTreeMap<String, AppliedRow> = applied_rows
        .into_iter()
        .map(|r| (r.name.clone(), r))
        .collect();

    let mut applied = Vec::new();
    let mut pending = Vec::new();
    let mut mismatched = Vec::new();

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
    })
}
