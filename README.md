# miiigrate

[![Release](https://img.shields.io/github/v/release/aircodev/miiigrate?sort=semver&label=release)](https://github.com/aircodev/miiigrate/releases)
[![Tag](https://img.shields.io/github/v/tag/aircodev/miiigrate?sort=semver&label=tag)](https://github.com/aircodev/miiigrate/tags)
[![CI](https://github.com/aircodev/miiigrate/actions/workflows/ci.yml/badge.svg)](https://github.com/aircodev/miiigrate/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/aircodev/miiigrate/blob/main/LICENSE)

> Forward-only SQL migrations for [iii](https://iii.dev). Apply, track, and inspect `.sql` migrations through the `database` worker — no direct database connection — and generate TypeScript types that describe your schema **as it actually crosses the wire**.

| field | value |
|-------|-------|
| version | 0.1.0 |
| type | binary |
| supported_targets | x86_64-apple-darwin, aarch64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu |
| author | [heryr](https://github.com/heryr) @ [aircodev](https://github.com/aircodev) |

## Install in an iii project

miiigrate **requires the [`database` worker](https://workers.iii.dev/workers/database)** at runtime: it opens no database connection of its own — every read goes through `database::query`, every write through `database::execute`, every migration through one atomic `database::transaction` batch.

```sh
iii worker add database
iii worker add miiigrate
```

Declare both in your project's `config.yaml` (the `config:` block is a first-boot seed; afterwards settings live in the `configuration` worker under id `miiigrate`):

```yaml
workers:
  - name: database
    config:
      databases:
        primary:
          url: sqlite:./data/app.db        # or postgres://…
  - name: miiigrate
    config:
      db: primary            # database name in the database worker's config
      dir: ./migrations      # folder of YYYYMMDDHHMMSS_slug.sql files
      auto: false            # true = run migrate::up at worker startup
      types_out: ./db.types.ts   # codegen output (optional)
```

Start the engine (`iii`), and you have four functions: `migrate::up`, `migrate::status`, `migrate::create`, `migrate::codegen`.

## Use cases

### Local development flow

```sh
# 1. scaffold a migration
iii trigger migrate::create --json '{"name":"create_users"}'
#    -> ./migrations/20260719143000_create_users.sql

# 2. write your SQL in the file, then apply
iii trigger migrate::up --json '{}'
#    -> { "applied": ["20260719143000_create_users.sql"], "skipped": 0, "duration_ms": 6 }

# 3. inspect at any time (read-only)
iii trigger migrate::status --json '{}'
#    -> { "applied": [...], "pending": [...], "mismatched": [], "missing": [] }

# 4. regenerate the TypeScript types after every schema change
iii trigger migrate::codegen --json '{}'
#    -> ./db.types.ts, one interface per table + aggregate Database type
```

Keep `auto: false` locally: you decide when migrations run. Commit `migrations/` and the generated `db.types.ts` together — reviewers see the schema change and its type impact in one diff.

### Production flow

Set `auto: true` in the production config: every replica runs `migrate::up` once at startup, **concurrently and safely**:

- **Postgres** — each migration batch takes `pg_advisory_xact_lock(<constant key>)` first; concurrent migrators serialize on the lock, and a replica that loses the race detects the migration was applied (same checksum in `_iii_migrations`) and counts it as *skipped*, not failed.
- **SQLite** — batches run `BEGIN IMMEDIATE` (via the database worker's `serializable` isolation), which serializes writers.

A failed migration rolls back atomically (`MIGRATION_FAILED` carries the file name, the 0-based failing statement index, and the database worker's structured error) and the worker stays up so `migrate::status` remains available for diagnosis. Files are **forward-only**: editing an applied file blocks the whole run with `CHECKSUM_MISMATCH` before anything is applied. Checksums normalize line endings (`\r\n` → `\n`), so a CRLF rewrite by a Windows machine is not drift.

### Add / update / delete a table

```sh
iii trigger migrate::create --json '{"name":"add_orders"}'
```

```sql
-- migrations/20260720100000_add_orders.sql
CREATE TABLE orders (
    id bigserial PRIMARY KEY,
    user_id bigint NOT NULL REFERENCES users (id),
    amount numeric(12, 2) NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now()
);
```

Updating or deleting is a **new** migration, never an edit of an applied file:

```sql
-- migrations/20260721090000_rename_orders.sql
ALTER TABLE orders RENAME TO purchases;
```

```sql
-- migrations/20260722080000_drop_legacy_logs.sql
DROP TABLE legacy_logs;
```

Then `migrate::up` + `migrate::codegen` — the new `db.types.ts` gains/renames/loses the matching interface, and your TypeScript stops compiling wherever the old shape was used. That is the point.

### Add / update / delete columns

```sql
-- add (safe on both dialects)
ALTER TABLE users ADD COLUMN email text;

-- update: rename works on Postgres and SQLite >= 3.25
ALTER TABLE users RENAME COLUMN email TO primary_email;

-- update: type changes are Postgres-only
ALTER TABLE users ALTER COLUMN amount TYPE numeric(14, 2);

-- delete (Postgres; SQLite >= 3.35)
ALTER TABLE users DROP COLUMN legacy_flag;
```

SQLite cannot `ALTER COLUMN TYPE`; use the documented recreate pattern **inside one migration** — the whole file runs in a single atomic batch, so the table swap is all-or-nothing:

```sql
CREATE TABLE users_new (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL NOT NULL DEFAULT 0);
INSERT INTO users_new (id, name, score) SELECT id, name, CAST(score AS REAL) FROM users;
DROP TABLE users;
ALTER TABLE users_new RENAME TO users;
```

Statement splitting is dialect-aware: a `;` inside `'strings'`, `E'…'` escapes, `"identifiers"`, `$tag$…$tag$` bodies (plpgsql functions!), or nested `/* comments */` never splits a statement.

## Functions

| Function | Purpose |
|---|---|
| `migrate::up` | Apply all pending migrations. Returns `{ applied: [names], skipped, duration_ms }`. Blocks on checksum drift before applying anything. |
| `migrate::status` | Read-only report: `{ applied, pending, mismatched, missing }` with names, checksums, and apply dates. |
| `migrate::create` | Payload `{ name: "add_users" }` → creates `<dir>/<UTC timestamp>_add_users.sql` with a forward-only header. Returns `{ name, path }`. |
| `migrate::codegen` | Introspects via `database::query` (`information_schema`/`pg_catalog` on Postgres, `pragma_table_info` on SQLite) and writes TypeScript types to `types_out` (or payload `out`). Returns `{ path, tables, enums }`. |

### Codegen type mapping

Types describe values **as they arrive through `database::query` (JSON)** — types JSON cannot carry (`Date`, `Buffer`) are never emitted:

- `int2`/`int4`/`float4`/`float8` → `number`; `int8`/`numeric` → `string` (loss-free)
- `timestamp`/`timestamptz`/`date` → `string` (RFC 3339 on the wire)
- `bytea`/`BLOB` → `string` (base64 on the wire)
- `json`/`jsonb` → emitted `Json` alias; Postgres enums → label unions (`export type Mood = 'happy' | 'sad'`); arrays → `T[]`
- nullable columns → `| null`; `_iii_migrations` is excluded

## Configuration

| key | default | meaning |
|-----|---------|---------|
| `db` | `primary` | Logical database name in the `database` worker's configuration. |
| `dir` | `./migrations` | Migration folder, resolved against the worker's working directory. |
| `auto` | `false` | Run `migrate::up` once at startup (best-effort: failure is logged, the worker stays up). |
| `types_out` | — | Output path for `migrate::codegen`; can be overridden per call with `out`. |
| `dialect` | auto | `postgres` \| `sqlite`; detected via `database::listDatabases` by default. |

Config is read **once at startup** (no hot reload — a running migration must not change target mid-flight).

## Errors

Handler error bodies carry a stable `code` field:

| Code | Meaning |
|---|---|
| `CHECKSUM_MISMATCH` | An applied migration's file changed on disk. Nothing was applied. |
| `MIGRATION_FAILED` | A migration's SQL failed; carries `name`, `statement_index`, and the database worker's error under `database_error`. The batch rolled back. |
| `CONFIG_ERROR` | Bad or missing configuration (unknown `db`, missing `types_out`, …). |
| `DIR_NOT_FOUND` | The `dir` folder does not exist (only `migrate::create` creates it). |
| `INVALID_MIGRATION_NAME` | A `.sql` file does not match `YYYYMMDDHHMMSS_slug.sql`, or a `create` slug is invalid. |
| `DATABASE_WORKER_UNAVAILABLE` | The `database` worker is not registered on the engine, or the invocation timed out. |
| `UNSUPPORTED_DIALECT` | The target database is neither Postgres nor SQLite (e.g. MySQL). |

## Changelog

### 0.1.0 — 2026-07-19

Initial release.

- `migrate::up` — per-migration atomic `database::transaction` batches, `pg_advisory_xact_lock` serialization on Postgres, `BEGIN IMMEDIATE` on SQLite, concurrent-migrator detection (skip, not failure), checksum gate.
- `migrate::status` — applied / pending / mismatched / missing, read-only.
- `migrate::create` — UTC-timestamped scaffold with forward-only header.
- `migrate::codegen` — Postgres (full, incl. enums and arrays) and SQLite (basic) → TypeScript; types match the JSON wire format (timestamps RFC 3339 strings, binary base64 strings).
- SQL splitter aware of strings, `E'…'`, quoted identifiers, dollar quoting, nested comments.
- Checksums SHA-256 with CRLF→LF normalization.
- Playgrounds: SQLite end-to-end scenario; Postgres 16 two-instance concurrency proof (docker compose).

## Next steps

Planned, roughly in order:

- **Drift-check cron** (v2) — periodic `migrate::status` via the engine's cron trigger, publishing `mismatched`/`missing` findings on pubsub for alerting.
- **`-- migrate:no-transaction` directive** — opt-out for statements that refuse to run inside a transaction (`CREATE INDEX CONCURRENTLY`, …). Until then the per-migration ceiling is the 300 s `database::transaction` invocation timeout.
- **Registry publication** — the repo is package-ready (`iii.worker.yaml`, per-target tar.gz + sha256); publishing to [workers.iii.dev](https://workers.iii.dev) lands as soon as the registry's publish flow for external repos is documented.
- **MySQL** — needs a `GET_LOCK`-based serialization strategy; rejected with `UNSUPPORTED_DIALECT` today.
- **Baseline / squash** — adopt an existing database as migration zero.

Not planned: down migrations (forward-only by design — write a new migration to undo) and a schema DSL (migrations are plain SQL).

## Development

The [`playground/`](https://github.com/aircodev/miiigrate/tree/main/playground) directory boots a real engine + database worker and runs the full scenario: [`run.sh`](https://github.com/aircodev/miiigrate/blob/main/playground/run.sh) (SQLite: up ×2 idempotence, CRLF rewrite no-op, tamper → `CHECKSUM_MISMATCH`, create, codegen) and [`run-postgres.sh`](https://github.com/aircodev/miiigrate/blob/main/playground/run-postgres.sh) (Postgres 16, two concurrent instances proving advisory-lock serialization, enum codegen).

## License

Apache-2.0
