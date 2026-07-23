//! `migrate::create` — scaffold a new migration file.

use chrono::Utc;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use super::AppState;
use crate::error::MigrateError;
use crate::migrations;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateReq {
    /// Slug for the migration (`[A-Za-z0-9_-]+`), e.g. `add_users`.
    pub name: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CreateResp {
    /// Full migration file name, e.g. `20260719143000_add_users.sql`.
    pub name: String,
    /// Path of the created file (config `dir` + name).
    pub path: String,
}

pub async fn handle(state: &AppState, req: CreateReq) -> Result<CreateResp, MigrateError> {
    let slug = req.name.trim();
    if slug.is_empty()
        || !slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(MigrateError::InvalidMigrationName {
            file: format!("<timestamp>_{slug}.sql"),
            reason: "slug must be non-empty and only contain [A-Za-z0-9_-]".into(),
        });
    }

    let dir = Path::new(&state.config.dir);
    // Authoring convenience: create the directory if it does not exist yet
    // (up/status keep failing with DIR_NOT_FOUND — they must not guess).
    std::fs::create_dir_all(dir).map_err(|e| MigrateError::ConfigError {
        message: format!("creating {}: {e}", dir.display()),
    })?;

    // UTC timestamp, kept monotonic with the files already on disk: a
    // hand-written future-dated migration must not make new names sort
    // before it (lexicographic order is the apply order). Then bump by one
    // second on collision (two creates within the same second) so names
    // stay unique and ordered.
    let mut ts = Utc::now();
    if let Some(latest) = latest_migration_timestamp(dir) {
        let after_latest = latest + chrono::Duration::seconds(1);
        if after_latest > ts {
            tracing::warn!(
                latest = %latest.format("%Y%m%d%H%M%S"),
                "latest migration on disk is ahead of the clock (hand-written \
                 name?); continuing after it to keep ordering monotonic"
            );
            ts = after_latest;
        }
    }
    let (name, path): (String, PathBuf) = loop {
        let name = format!("{}_{slug}.sql", ts.format("%Y%m%d%H%M%S"));
        let path = dir.join(&name);
        if !path.exists() {
            break (name, path);
        }
        ts += chrono::Duration::seconds(1);
    };
    debug_assert!(migrations::validate_name(&name).is_ok());

    let header = format!(
        "-- migration: {name}\n\
         -- created:   {} by miiigrate\n\
         --\n\
         -- miiigrate is forward-only: once this file has been applied, never\n\
         -- edit it (checksum is enforced) — write a new migration instead.\n\n",
        ts.format("%Y-%m-%d %H:%M:%S UTC")
    );
    std::fs::write(&path, header).map_err(|e| MigrateError::ConfigError {
        message: format!("writing {}: {e}", path.display()),
    })?;

    tracing::info!(migration = %name, "created migration file");
    Ok(CreateResp {
        name,
        path: path.display().to_string(),
    })
}

/// Largest timestamp among the well-formed migration files in `dir`.
/// Lenient on purpose: unreadable entries and stray files are ignored here —
/// `migrate::up`/`status` are the loud gate for those — so `create` always
/// stays usable for authoring.
fn latest_migration_timestamp(dir: &Path) -> Option<chrono::DateTime<Utc>> {
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| migrations::validate_name(name).is_ok())
        .filter_map(|name| migrations::timestamp_of(&name))
        .max()
}

#[cfg(test)]
mod tests {
    // The handler needs an AppState (IIIClient) so filesystem behavior is
    // covered by the pure helpers below plus the playground.
    use super::latest_migration_timestamp;
    use crate::migrations::{timestamp_of, validate_name};

    #[test]
    fn generated_names_validate() {
        for slug in ["add_users", "a", "Big-Change_2"] {
            let name = format!("20260719143000_{slug}.sql");
            assert!(validate_name(&name).is_ok(), "{name}");
        }
    }

    #[test]
    fn latest_timestamp_picks_max_and_ignores_strays() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("20260101000000_a.sql"), "").unwrap();
        std::fs::write(tmp.path().join("20991231235959_future.sql"), "").unwrap();
        std::fs::write(tmp.path().join("notes.sql"), "").unwrap(); // stray: ignored
        std::fs::write(tmp.path().join("README.md"), "").unwrap();

        assert_eq!(
            latest_migration_timestamp(tmp.path()),
            timestamp_of("20991231235959_future.sql")
        );
    }

    #[test]
    fn latest_timestamp_is_none_for_empty_or_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(latest_migration_timestamp(tmp.path()), None);
        assert_eq!(
            latest_migration_timestamp(std::path::Path::new("/nonexistent/miiigrate")),
            None
        );
    }
}
