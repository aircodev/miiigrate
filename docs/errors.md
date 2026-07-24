# Errors

Every handler error body carries a stable `code` field plus context fields.

## Codes

| Code | Meaning | Recovery |
|---|---|---|
| `CHECKSUM_MISMATCH` | An applied migration's file changed on disk. Nothing was applied. | Restore the original file content (checksums normalize CRLF, so line endings are never the cause). To change the schema, write a new migration instead. |
| `MIGRATION_FAILED` | A migration's SQL failed. The batch rolled back; earlier migrations in the same run stay applied. | Read `name`, `statement_index` (0-based), and the database worker's structured error under `database_error`. When the driver reported a native code, `message` names it — `… (SQLSTATE 42703)` on Postgres — so the cause is searchable without digging into the body. Fix the SQL in the failing file — it was never recorded as applied, so a plain re-run of `migrate::up` picks it up again. |
| `CONFIG_ERROR` | Bad or missing configuration — unknown `db`, codegen without `types_out` or `out`, etc. | Fix the `configuration` entry `miiigrate` and restart the worker (config is read once at startup). |
| `DIR_NOT_FOUND` | The `dir` folder does not exist. | Only `migrate::create` creates the directory; run it once, or create the folder. |
| `INVALID_MIGRATION_NAME` | A `.sql` file does not match `YYYYMMDDHHMMSS_slug.sql`, or a `create` slug is not `[A-Za-z0-9_-]+`. | Rename the file or fix the slug. |
| `DATABASE_WORKER_UNAVAILABLE` | The `database` worker is not registered on the engine, or the invocation timed out. | `iii worker add database`, check the engine, retry. |
| `UNSUPPORTED_DIALECT` | The target database is neither Postgres nor SQLite (e.g. MySQL). | Point `db` at a supported database. |

## How errors reach a caller

Handler errors are serialized as JSON in the error message. Through
`iii.trigger` they arrive as an invocation failure whose message embeds the
JSON body — parse from the first `{`:

```json
{
  "code": "MIGRATION_FAILED",
  "name": "20260720100000_add_orders.sql",
  "statement_index": 2,
  "database_error": { "...": "database worker's structured error" }
}
```

## Diagnosing a failed `auto: true` startup

With `auto: true`, a failed startup migration is logged but the worker stays
up. `migrate::status` keeps working, so the fastest diagnosis is:

```sh
iii trigger migrate::status --json '{}'
# -> "pending" shows what did not apply; worker logs carry the MIGRATION_FAILED body
```
