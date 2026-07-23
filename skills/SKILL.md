---
name: miiigrate
description: >-
  Database schema management for iii projects: forward-only SQL migrations
  and generated TypeScript types, driven through the iii database worker.
  Use this skill whenever a task touches the database in any way — creating,
  altering, or dropping tables, columns, indexes, enums, or constraints;
  checking which migrations are applied; or typing rows returned by
  database::query. Load it BEFORE writing any SQL or DDL, creating any file
  in the migrations directory, or calling the database worker for a schema
  change.
---

# miiigrate

miiigrate manages plain `.sql` migration files for an iii project. It never
opens a database connection of its own: every read goes through
`database::query`, every migration batch through one atomic
`database::transaction`. The [`database` worker](https://workers.iii.dev/workers/database)
must be installed and configured with the target database.

Migrations are forward-only. Applied files must never be edited — a change to
an applied file blocks the whole run with `CHECKSUM_MISMATCH` before anything
is applied. To change the schema again, create a new migration. Concurrent
replicas are safe: batches serialize on `pg_advisory_xact_lock` (Postgres) or
`BEGIN IMMEDIATE` (SQLite), and a replica that loses the race counts the
migration as skipped, not failed.

## Rules

- ALWAYS scaffold with `migrate::create`. NEVER write a migration file name
  by hand: the apply order is the lexicographic file-name order, and only
  `migrate::create` guarantees unique, monotonic timestamps. A hand-written
  timestamp (e.g. a rounded "120000") lands in the future and corrupts the
  ordering for every migration that follows.
- One migration per logical change ("add events table", "add user names") —
  not one file bundling every change of the session.
- NEVER edit or rename an applied migration file. To undo or amend, create a
  new forward migration.
- Apply with `migrate::up`, then check the response: when `types_path` is
  present the TypeScript types were already regenerated; when it is absent
  and the project has a `types_out`, run `migrate::codegen` yourself.
- Before any schema work, call `migrate::status` and resolve `mismatched`,
  `missing`, or `future_dated` entries before adding new migrations.

## When to Use

- Any request to add, change, or drop a table, column, index, enum, or
  constraint — scaffold with `migrate::create`, write the SQL, apply with
  `migrate::up`.
- You need the current schema state: applied, pending, drifted, or
  future-dated files (`migrate::status`, read-only, safe anytime).
- You wrote migration SQL and want it validated before applying —
  `migrate::check` runs the exact `migrate::up` parsing without executing
  anything, and lists every problem at once.
- You need to verify what a migration actually did — `migrate::schema`
  reports ordered columns, primary keys, foreign keys, indexes, and
  triggers; never hand-write `information_schema` queries for that.
- TypeScript code reads rows from `database::query` and needs types that
  match the JSON wire format (`migrate::codegen`).
- A `migrate::up` run failed and you need the failing file and statement
  index (`MIGRATION_FAILED` carries both, plus the database worker's error).

## Boundaries

- Not an ORM or schema DSL — migrations are plain SQL you write yourself.
- No down migrations by design. To undo, write a new forward migration.
- Postgres and SQLite only; MySQL is rejected with `UNSUPPORTED_DIALECT`.
- Dialect-specific SQL is your responsibility (e.g. SQLite cannot
  `ALTER COLUMN TYPE`; use the table-recreate pattern inside one migration —
  the file runs as a single atomic batch).
- For ad-hoc queries or data edits, call the `database` worker directly; for
  file or shell operations, use the `shell` worker.

## Functions

- `migrate::up` — apply all pending migrations in order; returns
  `{ applied, skipped, duration_ms, types_path? }`. Refuses to run while any
  applied file has drifted on disk. With `codegen_on_up` (default when
  `types_out` is set), regenerates types after a run that applied anything.
- `migrate::status` — read-only report `{ applied, pending, mismatched,
  missing, future_dated }` with names, checksums, and apply dates.
- `migrate::check` — static validation of pending migrations (splitting,
  empty files, naming, checksum drift) without executing any SQL; returns
  `{ ok, pending_checked, invalid_names, mismatched, … }`. Run it after
  writing SQL, before `migrate::up`.
- `migrate::create` — payload `{ name: "add_users" }`; creates
  `<dir>/<UTC timestamp>_add_users.sql` with a unique, monotonic timestamp
  and returns `{ name, path }`. Write the SQL into that file before calling
  `migrate::up`.
- `migrate::codegen` — introspect the live schema through `database::query`
  and write TypeScript types to the configured `types_out` (or payload
  `out`); returns `{ path, tables, enums }`.
- `migrate::schema` — read-only structured report of the live schema
  (ordered columns with defaults, primary keys, foreign keys, indexes,
  triggers, Postgres enums); payload `{}` or `{ table: "users" }`. Use it
  to verify a migration's effect instead of raw `information_schema` SQL.

Configuration (`db`, `dir`, `auto`, `types_out`, `codegen_on_up`, `dialect`)
lives in the `configuration` worker under id `miiigrate` and is read once at
startup. For payload and response schemas, call `get function info` on the
function id.
