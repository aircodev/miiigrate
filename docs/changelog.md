# Changelog

## Unreleased

- New function `migrate::check`: static validation of the migrations
  directory — statement splitting, empty files, naming scheme, checksum
  drift — without executing any SQL. Reports every problem at once instead
  of failing on the first, and keeps working while the database worker is
  down (`db_checked: false`). Forward-only migrations cannot be rolled
  back, so validate before you apply.
- Breaking (0.x): `migrate::status`'s `future_dated` entries are now objects
  `{ name, applied, hint }` instead of bare names — the hint states the
  remediation explicitly (applied: harmless; pending: re-scaffold via
  `migrate::create`, whose timestamps are monotonic).
- Database errors now name the driver-native code in their message —
  `… (SQLSTATE 42703)` on Postgres, `… (sqlite error code 1555)` on SQLite —
  instead of burying it inside `database_error`. The full structured body is
  still attached unchanged.

## 0.1.3 — 2026-07-23

- `migrate::create` keeps timestamps monotonic with the files already on
  disk: when the latest existing migration is dated ahead of the clock (a
  hand-written name), the new file is stamped one second after it instead of
  sorting before an applied migration.
- Future-dated migration files are surfaced: `migrate::status` returns a new
  `future_dated` list and `migrate::up` logs a warning before applying such
  a file (5-minute clock-skew tolerance).
- New `codegen_on_up` configuration: after a `migrate::up` that applied at
  least one migration, `migrate::codegen` runs automatically so generated
  types never drift from the schema. Enabled by default when `types_out` is
  set; codegen failures are logged and never fail the run. `migrate::up` now
  returns the written path as `types_path`.
- Agent skill rewritten: triggers on any database/schema work, and states
  the hard rules (always scaffold with `migrate::create`, one migration per
  logical change, never edit applied files).

- Revert the default log filter introduced in 0.1.2 that silenced
  `iii-helpers`' OTel connection module. Those transient pre-connection
  ERROR logs originate in `iii-helpers` (out of miiigrate's scope) and
  should be addressed there; deployments that want them quiet can set
  `RUST_LOG=info,iii_helpers::observability::telemetry::connection=off`.

## 0.1.2 — 2026-07-21

- `auto: true` now retries the startup migration run while the `database`
  worker is unavailable (capped exponential backoff, 2-minute budget).
  Previously the run was attempted exactly once: when miiigrate registered
  before the database worker — the common case under docker compose or an
  engine-managed boot — the auto run failed with
  `DATABASE_WORKER_UNAVAILABLE` and was never retried, silently leaving
  migrations unapplied. Other errors still fail fast, and the worker stays up
  either way.
- Clean startup when the engine comes up after miiigrate. The
  `configuration::register`/`get` calls now retry transient failures
  (timeout, not connected, function not registered yet) with capped backoff
  under the same 2-minute budget instead of crashing the worker after ~16 s;
  real errors from the configuration worker still fail fast. The default log
  filter also silences `iii-helpers`' OTel connection module, which logged
  every pre-connection attempt at ERROR while iii-sdk already reports the
  same condition as a WARN retry (set `RUST_LOG` to override).

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
