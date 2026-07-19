# miiigrate

> Forward-only SQL migrations for iii. Apply, track, and inspect `.sql` migrations through the `database` worker — no direct database connection — and generate TypeScript types from your schema.

| field | value |
|-------|-------|
| version | 0.1.0 |
| type | binary |
| supported_targets | x86_64-apple-darwin, aarch64-apple-darwin, x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu |
| author | hery |

## Install

```sh
iii worker add miiigrate
```

## Requires: database worker

miiigrate opens **no database connection of its own** — no SQL driver, no pool. Every operation is an engine invocation of the [`database` worker](https://workers.iii.dev/workers/database)'s functions:

- `database::query` for reads (migration state, schema introspection),
- `database::execute` for simple writes (tracking-table DDL),
- `database::transaction` for applying each migration as one atomic batch.

The `database` worker is a **runtime dependency**: it must be installed (`iii worker add database`) and configured with the database you point `db:` at. Postgres and SQLite are supported in v1; MySQL is rejected with `UNSUPPORTED_DIALECT`.

## Quick start (SQLite)

```sh
iii worker add database
iii worker add miiigrate
```

`config.yaml`:

```yaml
workers:
  - name: database
    config:
      databases:
        primary:
          url: sqlite:./data/app.db
  - name: miiigrate
    config:
      db: primary            # database name in the database worker's config
      dir: ./migrations      # folder of YYYYMMDDHHMMSS_slug.sql files
      auto: false            # true = run migrate::up at worker startup
      types_out: ./db.types.ts   # codegen output (optional)
```

Then:

```sh
iii trigger migrate::create --json '{"name":"create_users"}'
# edit migrations/<timestamp>_create_users.sql
iii trigger migrate::up --json '{}'
iii trigger migrate::status --json '{}'
iii trigger migrate::codegen --json '{}'
```

## Configure

Runtime settings live in the **`configuration` worker** under id **`miiigrate`**. The worker registers its JSON Schema at startup and reads the live value via `configuration::get`. The inline `config:` block above is a first-boot seed; afterwards edit `./data/configuration/miiigrate.yaml` or call `configuration::set`. Config is read **once at startup** (no hot reload — a running migration must not change target mid-flight; restart the worker to pick up changes).

| key | default | meaning |
|-----|---------|---------|
| `db` | `primary` | Logical database name in the `database` worker's configuration. |
| `dir` | `./migrations` | Migration folder, resolved against the worker's working directory. |
| `auto` | `false` | Run `migrate::up` once at startup (best-effort: failure is logged, the worker stays up). |
| `types_out` | — | Output path for `migrate::codegen`; can be overridden per call with `out`. |
| `dialect` | auto | `postgres` \| `sqlite`; by default detected via `database::listDatabases`. |

## Migration files

`<dir>/YYYYMMDDHHMMSS_slug.sql`, applied in file-name order. The full file name is the primary key in the tracking table `_iii_migrations` (`name`, `checksum` SHA-256 hex, `applied_at`), created automatically. Files are **forward-only**: once applied, a file must never change — `migrate::up` verifies checksums and refuses to run on drift (`CHECKSUM_MISMATCH`).

Each migration runs as **one atomic `database::transaction` batch**:

- **Postgres** — the batch is prefixed with `SELECT pg_advisory_xact_lock(<constant key>)`, serializing concurrent migrators (multiple replicas with `auto: true` are safe); the lock releases at commit/rollback.
- **SQLite** — the batch runs with `isolation: serializable`, which the database worker maps to `BEGIN IMMEDIATE`.

A migrator that loses the race does not fail: it detects the migration was applied concurrently (tracking row present with the same checksum) and counts it as skipped.

Statement splitting understands `'…'` strings (with `''`), Postgres `E'…'` escape strings, `"…"` identifiers, `$tag$…$tag$` dollar quoting, and nested block comments — a `;` inside any of those does not split.

## Functions

| Function | Purpose |
|---|---|
| `migrate::up` | Apply all pending migrations. Returns `{ applied: [names], skipped, duration_ms }`. Blocks on checksum drift before applying anything. |
| `migrate::status` | Read-only report: `{ applied, pending, mismatched, missing }` with names, checksums, and apply dates. `missing` lists tracking rows whose file disappeared from `dir`. |
| `migrate::create` | Payload `{ name: "add_users" }` → creates `<dir>/<UTC timestamp>_add_users.sql` with a forward-only header. Returns `{ name, path }`. |
| `migrate::codegen` | Introspects the schema via `database::query` (`information_schema`/`pg_catalog` on Postgres, `pragma_table_info` on SQLite) and writes TypeScript types to `types_out` (or payload `out`). Returns `{ path, tables, enums }`. |

### Codegen type mapping

One `export interface` per table plus an aggregate `Database` type, `generated by miiigrate, do not edit` header. Mappings follow kysely-codegen's conventions, simplified to plain interfaces:

- `int2`/`int4`/`float4`/`float8` → `number`; `int8`/`numeric` → `string` (loss-free)
- `timestamp`/`timestamptz`/`date` → `Date | string` (over the iii wire values arrive as strings; drivers elsewhere may hand you `Date`)
- `json`/`jsonb` → emitted `Json` alias; `bytea`/`BLOB` → `Buffer`
- Postgres enums → label-union type aliases (`export type Mood = 'happy' | 'sad'`); arrays → `T[]`
- nullable columns → `| null`; the `_iii_migrations` table is excluded

Postgres coverage is complete for common types; SQLite is basic (declared-type affinity). Unknown types map to `unknown`.

## Errors

Handler error bodies carry a stable `code` field:

| Code | Meaning |
|---|---|
| `CHECKSUM_MISMATCH` | An applied migration's file changed on disk. Nothing was applied. Restore the file (or write a new migration); never edit applied files. |
| `MIGRATION_FAILED` | A migration's SQL failed; carries the file `name`, the failing `statement_index` (0-based within the file), and the database worker's structured error under `database_error` (e.g. `DRIVER_ERROR` with a Postgres SQLSTATE `inner_code`). The batch rolled back — the database is untouched by that file. |
| `CONFIG_ERROR` | Bad or missing configuration (unknown `db`, unreadable file, missing `types_out`, …). |
| `DIR_NOT_FOUND` | The `dir` migration folder does not exist (only `migrate::create` creates it). |
| `INVALID_MIGRATION_NAME` | A `.sql` file in `dir` does not match `YYYYMMDDHHMMSS_slug.sql`, or a `create` slug is invalid. Stray files fail loudly instead of being silently skipped. |
| `DATABASE_WORKER_UNAVAILABLE` | The `database` worker is not registered on the engine, or the invocation timed out. |
| `UNSUPPORTED_DIALECT` | The target database is neither Postgres nor SQLite (e.g. MySQL). |

## Not in v1

Deliberately out of scope for now (see the playground for what IS covered):

- **Down migrations** — miiigrate is forward-only by design; write a new migration to undo.
- **Schema DSL** — migrations are plain SQL files.
- **MySQL** — no advisory-lock strategy wired; the dialect is rejected.
- **Periodic drift-check** (cron + pubsub alerting on `mismatched`) — planned for v2.
- **`-- migrate:no-transaction` directive** — every migration currently runs inside one `database::transaction` invocation, so statements that refuse to run in a transaction (`CREATE INDEX CONCURRENTLY`, …) are not supported yet. The practical ceiling for one migration is the invocation timeout miiigrate sets on `database::transaction` (300 s, aligned with the database worker's interactive-transaction maximum).

## Development

The `playground/` directory of the repository boots a real engine + database worker and runs the full scenario: `./playground/run.sh` (SQLite: up ×2 idempotence, tamper → `CHECKSUM_MISMATCH`, create, codegen) and `./playground/run-postgres.sh` (Postgres 16 via docker compose, two concurrent instances proving advisory-lock serialization, enum codegen).

## License

Apache-2.0
