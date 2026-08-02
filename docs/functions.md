# Functions

miiigrate registers eight functions on the engine. Call them with
`iii trigger <function> --json '<payload>'` or `iii.trigger(...)` from any
SDK. Full JSON Schemas are attached to each registration — retrieve them
with `iii get function info <function>`.

```
migrate::create ──▶ write your SQL ──▶ migrate::up ──▶ migrate::codegen
                                            │
                        migrate::status ◀───┘  (read-only, anytime)

migrate::adopt ──▶ take over another tool's history (drizzle)
migrate::baseline ──▶ record as applied without executing
```

## `migrate::up`

Apply every pending migration, in file-name order.

- **Payload**: `{}`
- **Returns**: `{ "applied": ["<file names in order>"], "skipped": <n>, "duration_ms": <n>, "types_path": "…" }`
  — `types_path` is present only when the automatic post-run codegen wrote it
  (see `codegen_on_up` in the configuration).

Behaviour:

- Refuses to run while any applied file has drifted on disk
  (`CHECKSUM_MISMATCH`) — nothing is applied in that case.
- Each file runs as one atomic `database::transaction` batch. On failure the
  batch rolls back and `MIGRATION_FAILED` reports the file, the 0-based
  failing statement index, and the database worker's structured error.
- `skipped` counts migrations already recorded at the start of the run, plus
  migrations applied concurrently by another replica while this call ran
  (detected by matching checksum in `_iii_migrations`).
- Statement splitting is dialect-aware: a `;` inside `'strings'`, `E'…'`
  escapes, `"identifiers"`, `$tag$…$tag$` bodies (plpgsql functions), or
  nested `/* comments */` never splits a statement.
- When `codegen_on_up` is enabled (default whenever `types_out` is set) and
  at least one migration was applied, `migrate::codegen` runs afterwards so
  the generated types never drift from the schema. A codegen failure is
  logged and reported by the absent `types_path`, never rolled into the run:
  the migrations stay applied.
- A pending file whose timestamp prefix is ahead of the wall clock (beyond a
  5-minute skew tolerance) is applied but logged as a warning: it is almost
  certainly a hand-written name — scaffold with `migrate::create` instead.

## `migrate::status`

Read-only report of the migration state. Never writes; safe to call at any
time, including while `migrate::up` is failing.

- **Payload**: `{}`
- **Returns**:

```json
{
  "db": "primary",
  "dialect": "postgres",
  "dir": "./migrations",
  "applied":    [{ "name": "…", "checksum": "…", "applied_at": "…" }],
  "pending":    [{ "name": "…", "checksum": "…" }],
  "mismatched": [{ "name": "…", "applied_checksum": "…", "file_checksum": "…", "applied_at": "…" }],
  "missing":    [{ "name": "…", "checksum": "…", "applied_at": "…" }],
  "future_dated": [{ "name": "…", "applied": false, "hint": "pending: recreate it via migrate::create …" }]
}
```

- `applied` — recorded in `_iii_migrations` and unchanged on disk.
- `pending` — on disk, not yet applied.
- `mismatched` — applied, but the file content changed since.
  `migrate::up` refuses to run while this list is non-empty.
- `missing` — recorded as applied but the file no longer exists on disk.
- `future_dated` — files on disk whose timestamp prefix is ahead of the wall
  clock beyond a 5-minute skew tolerance: hand-written names. Each entry says
  whether the file is `applied` and carries an explicit `hint`: applied ones
  are harmless (`migrate::create` keeps new timestamps monotonic with them),
  pending ones should be re-scaffolded via `migrate::create` before apply.

## `migrate::create`

Scaffold a new migration file. This is the only function that creates the
migrations directory if it does not exist yet.

- **Payload**: `{ "name": "add_users" }` — slug of `[A-Za-z0-9_-]+`
- **Returns**: `{ "name": "20260719143000_add_users.sql", "path": "./migrations/20260719143000_add_users.sql" }`

The timestamp is the current UTC time, kept monotonic with the files already
on disk: when the latest existing migration is dated ahead of the clock (a
hand-written name), the new file is stamped one second after it so
lexicographic order — which is the apply order — is preserved. Never write
migration file names by hand; always scaffold here. The file starts with a
forward-only header comment; write your SQL below it.

## `migrate::codegen`

Introspect the live schema through `database::query`
(`information_schema` / `pg_catalog` on Postgres, `pragma_table_info` on
SQLite) and write a TypeScript definition file.

- **Payload**: `{}` or `{ "out": "./src/db.types.ts" }` — `out` overrides
  the configured `types_out`
- **Returns**: `{ "path": "./db.types.ts", "tables": 4, "enums": 1 }`

One `export interface` per table, plus an aggregate `Database` type. The
`_iii_migrations` tracking table is excluded. See [codegen.md](codegen.md)
for the exact type mapping.

## `migrate::check`

Statically validate the migrations directory — "would `migrate::up`
succeed?" — without executing any SQL. Read-only; the only network call is
the tracking-table read, and it degrades gracefully.

- **Payload**: `{}`
- **Returns**:

```json
{
  "ok": true,
  "dir": "./migrations",
  "pending_checked": [{ "name": "…", "ok": true, "statements": 3 }],
  "invalid_names":   [{ "file": "notes.sql", "reason": "…" }],
  "future_dated":    [{ "name": "…", "applied": false, "hint": "…" }],
  "mismatched":      [],
  "missing":         [],
  "db_checked": true
}
```

- Every pending file is parsed with the same splitter as `migrate::up`;
  `pending_checked[].error` carries the exact message `up` would fail with
  (unsplittable SQL, empty file).
- Unlike `migrate::up`/`status`, an ill-named `.sql` file does not abort the
  report — all problems are listed at once under `invalid_names`.
- `ok` is false when anything would block or break `migrate::up`: an invalid
  name, a broken pending file, or checksum drift. `future_dated` and
  `missing` are warnings and do not flip it.
- Works while the database worker is down: `db_checked: false`, drift lists
  are then unknown (empty).

## `migrate::schema`

Read-only structured report of the live schema — the verification companion
of `migrate::up`. Everything an agent or human needs to confirm what a
migration actually did, without hand-writing `information_schema` queries.

- **Payload**: `{}` or `{ "table": "users" }` (exact name; an unknown table
  yields an empty `tables` list, not an error)
- **Returns**:

```json
{
  "db": "primary",
  "dialect": "postgres",
  "tables": [{
    "name": "reservations",
    "columns": [{ "name": "id", "data_type": "bigint", "nullable": false,
                  "default": "nextval('…')", "position": 1 }],
    "primary_key": ["id"],
    "foreign_keys": [{ "name": "…_fkey", "columns": ["event_id"],
                       "references_table": "events", "references_columns": ["id"],
                       "on_delete": "CASCADE", "on_update": "NO ACTION" }],
    "indexes":  [{ "name": "…", "unique": true, "columns": ["…"], "definition": "CREATE …" }],
    "triggers": [{ "name": "…", "timing": "AFTER", "events": ["INSERT", "UPDATE"] }]
  }],
  "enums": [{ "name": "mood", "labels": ["happy", "sad", "curious"] }]
}
```

- Columns come in ordinal (DDL) order with defaults; multi-column foreign
  keys are paired column-by-column; `enums` is Postgres-only.
- `indexes[].columns` is best-effort: empty for expression indexes — read
  `definition` there. SQLite auto-indexes backing PRIMARY KEY / UNIQUE have
  no `definition`.
- The `_iii_migrations` tracking table is excluded.

## `migrate::baseline`

Record migrations as applied **without executing their SQL**. The primitive
for adopting a database whose schema already exists — built by another tool
or by hand — where replaying the files would fail.

- **Payload**: `{}` (every pending migration) or
  `{ "names": ["20260719143000_baseline.sql"] }` (a subset; each name must
  exist in the migrations directory)
- **Returns**: `{ "baselined": ["<file names in order>"], "skipped": <n> }`

Behaviour:

- All tracking inserts run in one atomic `database::transaction`,
  serialized against concurrent migrators exactly like `migrate::up`
  (advisory lock on Postgres, `BEGIN IMMEDIATE` on SQLite).
- Idempotent: a file already tracked with the same checksum counts in
  `skipped`; a different checksum is `CHECKSUM_MISMATCH` and nothing is
  recorded.
- Typical use: dump the existing schema into one
  `migrate::create`-scaffolded file, then baseline it on the already-built
  database. Fresh environments simply run `migrate::up`, which executes the
  same file for real.

## `migrate::adopt`

Take over the migration history of another tool. `source: "drizzle"` is
supported today; the discriminated payload leaves room for other sources
(prisma, …) later.

- **Payload**: `{ "source": "drizzle" }`, optionally with
  `"from": "./drizzle"` (drizzle-kit's `out` directory) and
  `"mark_applied": "auto" | "all" | "none"` (default `auto`)
- **Returns**:

```json
{
  "converted": ["20240701135820_loud_wolverine.sql"],
  "baselined": ["20240701135820_loud_wolverine.sql"],
  "pending":   [],
  "skipped":   0,
  "hash_warnings": [{ "name": "…", "tag": "0000_…", "recorded_hash": "…", "file_hash": "…" }]
}
```

Behaviour:

- Reads `<from>/meta/_journal.json` — the journal, not the file names, is
  the source of truth for order and dates. Each entry's `when` becomes the
  converted file's `YYYYMMDDHHMMSS` prefix (bumped by one second on
  same-second collisions), so the adopted history keeps its real creation
  times and its order.
- File contents are copied byte-for-byte into the migrations directory; the
  `--> statement-breakpoint` markers are line comments the splitter already
  ignores. The drizzle folder and its `meta/` snapshots are **never
  modified** — delete them yourself once satisfied.
- Idempotent: a re-run skips targets that already exist with identical
  content; a divergent target is `ADOPT_CONFLICT`, never overwritten.
- The journal dialect must match the target database
  (`ADOPT_SOURCE_INVALID` otherwise); mysql journals are
  `UNSUPPORTED_DIALECT`.
- `mark_applied: "auto"` reads drizzle's own tracking table
  (`drizzle.__drizzle_migrations` on Postgres, `__drizzle_migrations` on
  SQLite): entries drizzle recorded as applied are baselined — their SQL is
  not executed — and the rest stays pending for `migrate::up`. A missing
  table means a fresh database: everything stays pending. Each matched
  row's `hash` is compared to the adopted file's SHA-256; drift is reported
  under `hash_warnings` (and logged) but does not fail the adopt — the
  files on disk are what the repository says is true.
- `"all"` baselines every converted file (schema known up to date, e.g. the
  drizzle table was already dropped); `"none"` only converts files.
- After adopting: `migrate::status` should show a clean state, and the
  drizzle dependency (`drizzle-kit`, `drizzle.config.ts`) can be removed.
  Dropping the `__drizzle_migrations` table is optional.

## Checksums

Checksums are SHA-256 over the file content with line endings normalized
(`\r\n` → `\n`) — a CRLF rewrite by a Windows machine is not drift.
