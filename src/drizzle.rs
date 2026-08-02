//! Read a drizzle-kit migrations folder and convert it to miiigrate's
//! on-disk format. Everything here is read-only on the source side: the
//! drizzle folder and its `meta/` snapshots are never modified.
//!
//! drizzle-kit lays out its `out` directory (default `./drizzle`) as:
//!
//! ```text
//! drizzle/
//!   0000_loud_wolverine.sql        -- statements separated by
//!   0001_add_orders.sql            --    `--> statement-breakpoint` comments
//!   meta/
//!     _journal.json                -- apply order + creation timestamps
//!     0000_snapshot.json           -- schema snapshots (ignored here)
//! ```
//!
//! The journal — not the file names — is the source of truth for order and
//! dates. Each entry's `when` (Unix millis) becomes the converted file's
//! `YYYYMMDDHHMMSS` prefix, so the adopted history keeps its real creation
//! times and its order. File contents are copied byte-for-byte: the
//! `--> statement-breakpoint` markers are line comments, which miiigrate's
//! statement splitter already ignores.

use chrono::DateTime;
use serde::Deserialize;
use std::path::{Path, PathBuf};

use crate::config::Dialect;
use crate::error::MigrateError;
use crate::migrations;

/// One entry of `meta/_journal.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct JournalEntry {
    pub idx: u64,
    /// Unix timestamp in milliseconds — drizzle's creation time, also the
    /// `created_at` value in `__drizzle_migrations`.
    pub when: i64,
    /// File stem, e.g. `0000_loud_wolverine` (file is `<tag>.sql`).
    pub tag: String,
}

/// Parsed `meta/_journal.json`.
#[derive(Debug, Deserialize)]
struct RawJournal {
    dialect: String,
    #[serde(default)]
    entries: Vec<JournalEntry>,
}

#[derive(Debug)]
pub struct Journal {
    pub dialect: Dialect,
    /// Entries sorted by `idx` (apply order).
    pub entries: Vec<JournalEntry>,
}

fn source_invalid(path: &Path, reason: impl Into<String>) -> MigrateError {
    MigrateError::AdoptSourceInvalid {
        path: path.display().to_string(),
        reason: reason.into(),
    }
}

/// Map drizzle's journal `dialect` strings onto miiigrate dialects.
/// drizzle-kit has used `pg`/`postgresql` and `sqlite`/`turso` across
/// versions; anything else (mysql, singlestore, …) is unsupported.
fn map_dialect(dialect: &str) -> Result<Dialect, MigrateError> {
    match dialect {
        "pg" | "postgresql" => Ok(Dialect::Postgres),
        "sqlite" | "turso" | "libsql" => Ok(Dialect::Sqlite),
        other => Err(MigrateError::UnsupportedDialect {
            dialect: other.to_string(),
        }),
    }
}

/// Read and validate `<from>/meta/_journal.json`.
pub fn read_journal(from: &Path) -> Result<Journal, MigrateError> {
    let journal_path = from.join("meta").join("_journal.json");
    let raw = std::fs::read_to_string(&journal_path).map_err(|e| {
        source_invalid(
            from,
            format!(
                "not a drizzle migrations folder ({}: {e}); pass `from` if \
                 drizzle-kit's `out` is not ./drizzle",
                journal_path.display()
            ),
        )
    })?;
    let parsed: RawJournal = serde_json::from_str(&raw)
        .map_err(|e| source_invalid(from, format!("malformed meta/_journal.json: {e}")))?;
    let dialect = map_dialect(&parsed.dialect)?;
    let mut entries = parsed.entries;
    entries.sort_by_key(|e| e.idx);
    Ok(Journal { dialect, entries })
}

/// One drizzle migration ready to be written in miiigrate's format.
#[derive(Debug, Clone)]
pub struct PlannedItem {
    /// Journal tag, e.g. `0000_loud_wolverine`.
    pub tag: String,
    /// Journal `when` — Unix millis; matches `created_at` in
    /// `__drizzle_migrations`.
    pub when: i64,
    pub source: PathBuf,
    /// Converted file name, e.g. `20260719143000_loud_wolverine.sql`.
    pub target_name: String,
    /// File bytes, copied verbatim into the target.
    pub bytes: Vec<u8>,
    /// SHA-256 of the raw bytes — comparable to `__drizzle_migrations.hash`
    /// (drizzle hashes the file content as-is).
    pub sha256_raw: String,
    /// miiigrate checksum (CRLF-normalized SHA-256) of the same bytes — the
    /// value that lands in `_iii_migrations`.
    pub checksum: String,
}

/// Drop the `NNNN_` index prefix of a drizzle tag and keep only characters
/// the miiigrate name scheme allows.
fn slug_of_tag(tag: &str) -> String {
    let stem = tag
        .split_once('_')
        .filter(|(idx, _)| !idx.is_empty() && idx.chars().all(|c| c.is_ascii_digit()))
        .map(|(_, rest)| rest)
        .unwrap_or(tag);
    let slug: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if slug.is_empty() {
        "migration".to_string()
    } else {
        slug
    }
}

/// Read every journal entry's file and derive its miiigrate name. Timestamps
/// come from `when` (UTC) and are kept strictly increasing — two entries
/// created within the same second (or a non-monotonic journal) bump forward
/// by one second, mirroring `migrate::create`'s collision rule, so
/// lexicographic order always equals journal order.
pub fn plan_conversion(from: &Path, journal: &Journal) -> Result<Vec<PlannedItem>, MigrateError> {
    let mut items = Vec::with_capacity(journal.entries.len());
    let mut prev_secs: i64 = 0;
    for entry in &journal.entries {
        let source = from.join(format!("{}.sql", entry.tag));
        let bytes = std::fs::read(&source).map_err(|e| {
            source_invalid(
                from,
                format!(
                    "journal entry `{}` has no file ({}: {e})",
                    entry.tag,
                    source.display()
                ),
            )
        })?;

        let secs = entry.when.div_euclid(1000).max(prev_secs + 1);
        // Format as YYYYMMDDHHMMSS; `when` values outside chrono's range are
        // a corrupt journal, not a reason to panic.
        let ts = DateTime::from_timestamp(secs, 0)
            .ok_or_else(|| {
                source_invalid(
                    from,
                    format!(
                        "journal entry `{}` has an invalid `when`: {}",
                        entry.tag, entry.when
                    ),
                )
            })?
            .format("%Y%m%d%H%M%S")
            .to_string();
        prev_secs = secs;

        let target_name = format!("{ts}_{}.sql", slug_of_tag(&entry.tag));
        migrations::validate_name(&target_name).map_err(|reason| {
            source_invalid(
                from,
                format!("cannot derive a valid name for `{}`: {reason}", entry.tag),
            )
        })?;

        items.push(PlannedItem {
            tag: entry.tag.clone(),
            when: entry.when,
            source,
            target_name,
            sha256_raw: migrations::checksum_bytes(&bytes),
            checksum: migrations::checksum_migration(&bytes),
            bytes,
        });
    }
    Ok(items)
}

/// Write the planned files into `dir` (created if missing). Idempotent: a
/// target that already exists with identical content is skipped; different
/// content is `ADOPT_CONFLICT` — never overwritten. Returns the names
/// actually written by this call.
pub fn write_converted(dir: &Path, items: &[PlannedItem]) -> Result<Vec<String>, MigrateError> {
    std::fs::create_dir_all(dir).map_err(|e| MigrateError::ConfigError {
        message: format!("creating {}: {e}", dir.display()),
    })?;
    let mut written = Vec::new();
    for item in items {
        let target = dir.join(&item.target_name);
        if target.exists() {
            let existing = std::fs::read(&target).map_err(|e| MigrateError::ConfigError {
                message: format!("reading {}: {e}", target.display()),
            })?;
            if migrations::checksum_migration(&existing) == item.checksum {
                continue; // already converted by a previous run
            }
            return Err(MigrateError::AdoptConflict {
                file: item.target_name.clone(),
                reason: format!(
                    "target already exists with different content (source `{}`); \
                     move it away or clean the migrations directory before adopting",
                    item.tag
                ),
            });
        }
        std::fs::write(&target, &item.bytes).map_err(|e| MigrateError::ConfigError {
            message: format!("writing {}: {e}", target.display()),
        })?;
        written.push(item.target_name.clone());
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_drizzle_dir(files: &[(&str, &str)], journal: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("meta")).unwrap();
        std::fs::write(tmp.path().join("meta").join("_journal.json"), journal).unwrap();
        for (name, content) in files {
            std::fs::write(tmp.path().join(name), content).unwrap();
        }
        tmp
    }

    const JOURNAL: &str = r#"{
        "version": "7",
        "dialect": "postgresql",
        "entries": [
            {"idx": 0, "version": "7", "when": 1719842300123, "tag": "0000_loud_wolverine", "breakpoints": true},
            {"idx": 1, "version": "7", "when": 1719842300999, "tag": "0001_add_orders", "breakpoints": true}
        ]
    }"#;

    #[test]
    fn journal_is_parsed_and_sorted() {
        let tmp = write_drizzle_dir(&[], JOURNAL);
        let journal = read_journal(tmp.path()).unwrap();
        assert_eq!(journal.dialect, Dialect::Postgres);
        assert_eq!(journal.entries.len(), 2);
        assert_eq!(journal.entries[0].tag, "0000_loud_wolverine");
        assert_eq!(journal.entries[1].when, 1719842300999);
    }

    #[test]
    fn missing_journal_is_adopt_source_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            read_journal(tmp.path()),
            Err(MigrateError::AdoptSourceInvalid { .. })
        ));
    }

    #[test]
    fn mysql_journal_is_unsupported() {
        let tmp = write_drizzle_dir(&[], r#"{"dialect": "mysql", "entries": []}"#);
        assert!(matches!(
            read_journal(tmp.path()),
            Err(MigrateError::UnsupportedDialect { .. })
        ));
    }

    #[test]
    fn plan_derives_names_from_when_and_keeps_order() {
        let tmp = write_drizzle_dir(
            &[
                ("0000_loud_wolverine.sql", "CREATE TABLE users (id int);"),
                ("0001_add_orders.sql", "CREATE TABLE orders (id int);"),
            ],
            JOURNAL,
        );
        let journal = read_journal(tmp.path()).unwrap();
        let items = plan_conversion(tmp.path(), &journal).unwrap();

        // 1719842300 s = 2024-07-01T13:58:20Z.
        assert_eq!(items[0].target_name, "20240701135820_loud_wolverine.sql");
        // Same second in the journal: bumped by one to preserve order.
        assert_eq!(items[1].target_name, "20240701135821_add_orders.sql");
        assert!(items[0].target_name < items[1].target_name);
        for item in &items {
            migrations::validate_name(&item.target_name).unwrap();
        }
    }

    #[test]
    fn plan_fails_when_a_journal_file_is_missing() {
        let tmp = write_drizzle_dir(
            &[("0000_loud_wolverine.sql", "CREATE TABLE users (id int);")],
            JOURNAL, // references 0001_add_orders too
        );
        let journal = read_journal(tmp.path()).unwrap();
        assert!(matches!(
            plan_conversion(tmp.path(), &journal),
            Err(MigrateError::AdoptSourceInvalid { .. })
        ));
    }

    #[test]
    fn checksums_cover_raw_and_normalized_content() {
        let tmp = write_drizzle_dir(
            &[
                (
                    "0000_loud_wolverine.sql",
                    "CREATE TABLE a (\r\n id int\r\n);",
                ),
                ("0001_add_orders.sql", "SELECT 1;"),
            ],
            JOURNAL,
        );
        let journal = read_journal(tmp.path()).unwrap();
        let items = plan_conversion(tmp.path(), &journal).unwrap();
        // Raw hash (drizzle's) sees the CRLF; miiigrate's checksum does not.
        assert_eq!(
            items[0].sha256_raw,
            migrations::checksum_bytes(b"CREATE TABLE a (\r\n id int\r\n);")
        );
        assert_eq!(
            items[0].checksum,
            migrations::checksum_bytes(b"CREATE TABLE a (\n id int\n);")
        );
    }

    #[test]
    fn write_is_idempotent_and_refuses_divergent_targets() {
        let tmp = write_drizzle_dir(
            &[
                ("0000_loud_wolverine.sql", "CREATE TABLE users (id int);"),
                ("0001_add_orders.sql", "CREATE TABLE orders (id int);"),
            ],
            JOURNAL,
        );
        let journal = read_journal(tmp.path()).unwrap();
        let items = plan_conversion(tmp.path(), &journal).unwrap();

        let out = tempfile::tempdir().unwrap();
        let written = write_converted(out.path(), &items).unwrap();
        assert_eq!(written.len(), 2);

        // Re-run: identical targets are skipped, nothing rewritten.
        let written_again = write_converted(out.path(), &items).unwrap();
        assert!(written_again.is_empty());

        // A divergent target is never overwritten.
        std::fs::write(
            out.path().join(&items[0].target_name),
            "ALTER TABLE users ADD COLUMN x int;",
        )
        .unwrap();
        assert!(matches!(
            write_converted(out.path(), &items),
            Err(MigrateError::AdoptConflict { .. })
        ));
    }

    #[test]
    fn tag_slugs_are_sanitized() {
        assert_eq!(slug_of_tag("0000_loud_wolverine"), "loud_wolverine");
        assert_eq!(slug_of_tag("0001_ajout-commandes"), "ajout-commandes");
        assert_eq!(slug_of_tag("no_index_prefix"), "no_index_prefix");
        assert_eq!(slug_of_tag("0002_"), "migration");
        assert_eq!(slug_of_tag("0003_héllo wörld"), "h_llo_w_rld");
    }
}
