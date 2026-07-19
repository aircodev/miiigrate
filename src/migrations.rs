//! Migration files on disk: naming, ordering, checksums.
//!
//! A migration file is `YYYYMMDDHHMMSS_slug.sql`. The full file name (with
//! extension) is the migration's identity — it is the `name` primary key in
//! the `_iii_migrations` tracking table. Files sort lexicographically, which
//! matches chronological order thanks to the fixed-width timestamp prefix.
//!
//! Checksums are SHA-256 over the file bytes with line endings normalized
//! (`\r\n` → `\n`), hex-encoded lowercase. The normalization means a file
//! rewritten with CRLF endings (Windows editors, `core.autocrlf`) keeps its
//! identity; any other edit to an applied migration is a mismatch, by design.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

use crate::error::MigrateError;

/// A migration file found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationFile {
    /// Full file name, e.g. `20260719143000_add_users.sql`. Tracking-table key.
    pub name: String,
    pub path: PathBuf,
    /// SHA-256 of the file bytes, lowercase hex.
    pub checksum: String,
}

/// Validate a migration file name: 14-digit timestamp, `_`, non-empty slug of
/// `[A-Za-z0-9_-]`, `.sql` extension.
pub fn validate_name(file_name: &str) -> Result<(), String> {
    let stem = file_name
        .strip_suffix(".sql")
        .ok_or("missing .sql extension")?;
    let (ts, rest) = stem.split_at_checked(14).ok_or("name too short")?;
    if !ts.chars().all(|c| c.is_ascii_digit()) {
        return Err("prefix must be a 14-digit timestamp (YYYYMMDDHHMMSS)".into());
    }
    let slug = rest
        .strip_prefix('_')
        .ok_or("timestamp must be followed by `_slug`")?;
    if slug.is_empty() {
        return Err("slug must not be empty".into());
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err("slug may only contain [A-Za-z0-9_-]".into());
    }
    Ok(())
}

/// SHA-256 lowercase hex of `bytes`, as-is.
pub fn checksum_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Checksum of a migration file: SHA-256 over the bytes with `\r\n`
/// normalized to `\n`, so a CRLF rewrite by a Windows machine does not
/// change a migration's identity. Lone `\r` bytes are preserved — they are
/// content (e.g. inside a string literal), not a line-ending convention.
pub fn checksum_migration(bytes: &[u8]) -> String {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1; // skip the \r, keep the \n
            continue;
        }
        normalized.push(bytes[i]);
        i += 1;
    }
    checksum_bytes(&normalized)
}

/// List and checksum all `*.sql` files in `dir`, sorted by name.
///
/// Errors: `DIR_NOT_FOUND` when the directory is missing,
/// `INVALID_MIGRATION_NAME` when a `.sql` file does not follow the naming
/// scheme (a stray file must fail loudly, not be silently skipped — its
/// ordering relative to the valid files would be undefined). Non-`.sql`
/// entries and subdirectories are ignored.
pub fn list_migrations(dir: &Path) -> Result<Vec<MigrationFile>, MigrateError> {
    let entries = std::fs::read_dir(dir).map_err(|_| MigrateError::DirNotFound {
        dir: dir.display().to_string(),
    })?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| MigrateError::ConfigError {
            message: format!("reading {}: {e}", dir.display()),
        })?;
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("sql") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| MigrateError::InvalidMigrationName {
                file: path.display().to_string(),
                reason: "non-UTF-8 file name".into(),
            })?
            .to_string();
        validate_name(&name).map_err(|reason| MigrateError::InvalidMigrationName {
            file: name.clone(),
            reason,
        })?;
        let bytes = std::fs::read(&path).map_err(|e| MigrateError::ConfigError {
            message: format!("reading {}: {e}", path.display()),
        })?;
        files.push(MigrationFile {
            name,
            checksum: checksum_migration(&bytes),
            path,
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_names_pass() {
        for name in [
            "20260719143000_add_users.sql",
            "20260101000000_a.sql",
            "20991231235959_snake_and-dash_09.sql",
        ] {
            assert!(validate_name(name).is_ok(), "{name} should be valid");
        }
    }

    #[test]
    fn invalid_names_fail() {
        for name in [
            "add_users.sql",                // no timestamp
            "2026071914300_add_users.sql",  // 13 digits
            "20260719143000-add_users.sql", // dash instead of underscore
            "20260719143000_.sql",          // empty slug
            "20260719143000_add users.sql", // space in slug
            "20260719143000_add_users.SQL", // wrong extension case
            "20260719143000_add_users",     // no extension
            "20260719143000_héllo.sql",     // non-ascii slug
        ] {
            assert!(validate_name(name).is_err(), "{name} should be invalid");
        }
    }

    #[test]
    fn checksum_is_sha256_lowercase_hex() {
        // sha256("hello") — well-known vector.
        assert_eq!(
            checksum_bytes(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn migration_checksum_is_line_ending_invariant() {
        let lf = b"CREATE TABLE a (\n  id int\n);\n";
        let crlf = b"CREATE TABLE a (\r\n  id int\r\n);\r\n";
        assert_eq!(checksum_migration(lf), checksum_migration(crlf));
        // Normalized CRLF equals the plain LF hash.
        assert_eq!(checksum_migration(crlf), checksum_bytes(lf));
        // Any real edit still changes the checksum.
        assert_ne!(
            checksum_migration(lf),
            checksum_migration(b"CREATE TABLE a (\n  id text\n);\n")
        );
    }

    #[test]
    fn lone_carriage_return_is_content_not_line_ending() {
        // \r inside a string literal is data; stripping it would let two
        // semantically different files collide.
        assert_ne!(
            checksum_migration(b"SELECT 'a\rb';"),
            checksum_migration(b"SELECT 'ab';")
        );
    }

    #[test]
    fn list_sorts_by_name_and_ignores_non_sql() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("20260202000000_b.sql"), "b").unwrap();
        std::fs::write(tmp.path().join("20260101000000_a.sql"), "a").unwrap();
        std::fs::write(tmp.path().join("README.md"), "ignored").unwrap();
        std::fs::create_dir(tmp.path().join("sub.sql")).unwrap();

        let files = list_migrations(tmp.path()).unwrap();
        let names: Vec<_> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["20260101000000_a.sql", "20260202000000_b.sql"]);
    }

    #[test]
    fn stray_sql_file_is_a_hard_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("notes.sql"), "x").unwrap();
        assert!(matches!(
            list_migrations(tmp.path()),
            Err(MigrateError::InvalidMigrationName { .. })
        ));
    }

    #[test]
    fn missing_dir_is_dir_not_found() {
        assert!(matches!(
            list_migrations(Path::new("/nonexistent/miiigrate-test")),
            Err(MigrateError::DirNotFound { .. })
        ));
    }
}
