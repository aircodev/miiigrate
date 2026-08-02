//! End-to-end conversion of a realistic drizzle-kit folder — no engine, no
//! database. The fixture mirrors real drizzle output: `--> statement-
//! breakpoint` markers (both on their own line and glued after the `;`),
//! quoted identifiers, a dollar-quoted plpgsql function, and a `meta/`
//! folder with journal + snapshot.

use std::path::Path;

use miiigrate::drizzle::{plan_conversion, read_journal, write_converted};
use miiigrate::migrations::{checksum_bytes, list_migrations};
use miiigrate::splitter::split_statements;

fn fixture() -> &'static Path {
    Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/drizzle"
    ))
}

#[test]
fn full_conversion_yields_a_valid_miiigrate_directory() {
    let journal = read_journal(fixture()).unwrap();
    let items = plan_conversion(fixture(), &journal).unwrap();
    let out = tempfile::tempdir().unwrap();
    let written = write_converted(out.path(), &items).unwrap();

    assert_eq!(
        written,
        [
            "20250723080000_wild_sabretooth.sql",
            "20250724080000_melodic_vulture.sql",
            "20250725080000_true_vermin.sql",
        ]
    );

    // The converted directory passes miiigrate's own scan: names valid,
    // lexicographic order = journal order, checksums match the plan.
    let files = list_migrations(out.path()).unwrap();
    assert_eq!(files.len(), 3);
    for (file, item) in files.iter().zip(&items) {
        assert_eq!(file.name, item.target_name);
        assert_eq!(file.checksum, item.checksum);
        // Byte-for-byte copy of the drizzle source.
        assert_eq!(std::fs::read(&file.path).unwrap(), item.bytes);
    }
}

#[test]
fn drizzle_content_splits_into_the_expected_statements() {
    let journal = read_journal(fixture()).unwrap();
    let items = plan_conversion(fixture(), &journal).unwrap();

    let split = |i: usize| split_statements(std::str::from_utf8(&items[i].bytes).unwrap()).unwrap();

    // Breakpoint markers are line comments: they disappear, they never glue
    // two statements together and never survive inside one.
    let first = split(0);
    assert_eq!(first.len(), 3);
    assert!(first[0].starts_with("CREATE TABLE \"users\""));
    assert!(first[2].starts_with("ALTER TABLE \"orders\""));

    // Marker glued right after the `;` (real drizzle output shape).
    let second = split(1);
    assert_eq!(second.len(), 2);
    assert!(second[1].starts_with("CREATE INDEX"));

    // The `;` inside the dollar-quoted plpgsql body does not split.
    let third = split(2);
    assert_eq!(third.len(), 2);
    assert!(third[0].starts_with("CREATE FUNCTION set_updated_at"));
    assert!(third[0].contains("$$"));

    for stmt in first.iter().chain(&second).chain(&third) {
        assert!(
            !stmt.contains("statement-breakpoint"),
            "breakpoint marker leaked into: {stmt}"
        );
    }
}

#[test]
fn raw_hash_matches_what_drizzle_would_have_recorded() {
    // `__drizzle_migrations.hash` is the SHA-256 of the file content as-is;
    // `sha256_raw` must be comparable to it for the adopt-time drift check.
    let journal = read_journal(fixture()).unwrap();
    let items = plan_conversion(fixture(), &journal).unwrap();
    for item in &items {
        let bytes = std::fs::read(&item.source).unwrap();
        assert_eq!(item.sha256_raw, checksum_bytes(&bytes));
    }
}

#[test]
fn rerun_writes_nothing_and_the_source_is_untouched() {
    let journal = read_journal(fixture()).unwrap();
    let items = plan_conversion(fixture(), &journal).unwrap();
    let out = tempfile::tempdir().unwrap();

    assert_eq!(write_converted(out.path(), &items).unwrap().len(), 3);
    assert!(write_converted(out.path(), &items).unwrap().is_empty());
    assert_eq!(list_migrations(out.path()).unwrap().len(), 3);

    // Adoption is read-only on the drizzle side: same files, same bytes.
    let entries: Vec<_> = std::fs::read_dir(fixture())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(entries.iter().filter(|n| n.ends_with(".sql")).count(), 3);
}
