# Changelog

## Unreleased

- `auto: true` now retries the startup migration run while the `database`
  worker is unavailable (capped exponential backoff, 2-minute budget).
  Previously the run was attempted exactly once: when miiigrate registered
  before the database worker — the common case under docker compose or an
  engine-managed boot — the auto run failed with
  `DATABASE_WORKER_UNAVAILABLE` and was never retried, silently leaving
  migrations unapplied. Other errors still fail fast, and the worker stays up
  either way.

## 0.1.1 — 2026-07-20

Documentation release — no functional change.

- Agent skill: `skills/SKILL.md` following the iii skill guidelines, so
  agents (Claude Code, Cursor, …) know when and how to drive miiigrate.
  Install with `npx skills add aircodev/miiigrate`.
- README rewritten to the iii worker README structure: summary with
  architecture and lifecycle diagrams, Install, Quickstart, Configuration.
- Reference documentation moved to `docs/`: functions, configuration,
  workflows, codegen mapping, errors and recovery, changelog, roadmap.

## 0.1.0 — 2026-07-19

Initial release.

- `migrate::up` — per-migration atomic `database::transaction` batches,
  `pg_advisory_xact_lock` serialization on Postgres, `BEGIN IMMEDIATE` on
  SQLite, concurrent-migrator detection (skip, not failure), checksum gate.
- `migrate::status` — applied / pending / mismatched / missing, read-only.
- `migrate::create` — UTC-timestamped scaffold with forward-only header.
- `migrate::codegen` — Postgres (full, including enums and arrays) and SQLite
  (basic) to TypeScript; types match the JSON wire format (timestamps as
  RFC 3339 strings, binary as base64 strings).
- SQL splitter aware of strings, `E'…'`, quoted identifiers, dollar quoting,
  and nested comments.
- Checksums SHA-256 with CRLF-to-LF normalization.
- Playgrounds: SQLite end-to-end scenario; Postgres 16 two-instance
  concurrency proof (docker compose).
