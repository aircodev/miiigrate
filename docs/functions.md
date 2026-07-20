# Functions

miiigrate registers four functions on the engine. Call them with
`iii trigger <function> --json '<payload>'` or `iii.trigger(...)` from any
SDK. Full JSON Schemas are attached to each registration — retrieve them
with `iii get function info <function>`.

```
migrate::create ──▶ write your SQL ──▶ migrate::up ──▶ migrate::codegen
                                            │
                        migrate::status ◀───┘  (read-only, anytime)
```

## `migrate::up`

Apply every pending migration, in file-name order.

- **Payload**: `{}`
- **Returns**: `{ "applied": ["<file names in order>"], "skipped": <n>, "duration_ms": <n> }`

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
  "missing":    [{ "name": "…", "checksum": "…", "applied_at": "…" }]
}
```

- `applied` — recorded in `_iii_migrations` and unchanged on disk.
- `pending` — on disk, not yet applied.
- `mismatched` — applied, but the file content changed since.
  `migrate::up` refuses to run while this list is non-empty.
- `missing` — recorded as applied but the file no longer exists on disk.

## `migrate::create`

Scaffold a new migration file. This is the only function that creates the
migrations directory if it does not exist yet.

- **Payload**: `{ "name": "add_users" }` — slug of `[A-Za-z0-9_-]+`
- **Returns**: `{ "name": "20260719143000_add_users.sql", "path": "./migrations/20260719143000_add_users.sql" }`

The timestamp is the current UTC time. The file starts with a forward-only
header comment; write your SQL below it.

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

## Checksums

Checksums are SHA-256 over the file content with line endings normalized
(`\r\n` → `\n`) — a CRLF rewrite by a Windows machine is not drift.
