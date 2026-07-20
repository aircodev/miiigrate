---
name: miiigrate
description: >-
  Apply, track, and scaffold forward-only SQL migrations through the iii
  database worker, and generate TypeScript types that match the schema as it
  crosses the wire. Reach for it whenever a schema change is needed.
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

## When to Use

- The user asks to add, change, or drop a table, column, or index — scaffold
  with `migrate::create`, write the SQL, apply with `migrate::up`.
- You need to know the current schema state: what is applied, pending, or has
  drifted (`migrate::status`, read-only, safe anytime).
- TypeScript code reads rows from `database::query` and needs types that match
  the JSON wire format (`migrate::codegen`).
- A `migrate::up` run failed and you need the failing file and statement index
  (`MIGRATION_FAILED` carries both, plus the database worker's error).

## Boundaries

- Not an ORM or schema DSL — migrations are plain SQL you write yourself.
- No down migrations by design. To undo, write a new forward migration.
- Never edit an applied migration file; always create a new one.
- Postgres and SQLite only; MySQL is rejected with `UNSUPPORTED_DIALECT`.
- Dialect-specific SQL is your responsibility (e.g. SQLite cannot
  `ALTER COLUMN TYPE`; use the table-recreate pattern inside one migration —
  the file runs as a single atomic batch).
- For ad-hoc queries or data edits, call the `database` worker directly; for
  file or shell operations, use the `shell` worker.

## Functions

- `migrate::up` — apply all pending migrations in order; returns
  `{ applied, skipped, duration_ms }`. Refuses to run while any applied file
  has drifted on disk.
- `migrate::status` — read-only report `{ applied, pending, mismatched,
  missing }` with names, checksums, and apply dates.
- `migrate::create` — payload `{ name: "add_users" }`; creates
  `<dir>/<UTC timestamp>_add_users.sql` and returns `{ name, path }`. Write
  the SQL into that file before calling `migrate::up`.
- `migrate::codegen` — introspect the live schema through `database::query`
  and write TypeScript types to the configured `types_out` (or payload
  `out`); returns `{ path, tables, enums }`. Regenerate after every applied
  migration.

Configuration (`db`, `dir`, `auto`, `types_out`, `dialect`) lives in the
`configuration` worker under id `miiigrate` and is read once at startup. For
payload and response schemas, call `get function info` on the function id.
