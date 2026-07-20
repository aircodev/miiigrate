# Workflows

## Local development

Keep `auto: false` locally: you decide when migrations run.

```sh
# 1. scaffold
iii trigger migrate::create --json '{"name":"create_users"}'
#    -> ./migrations/20260719143000_create_users.sql

# 2. write your SQL in the file, then apply
iii trigger migrate::up --json '{}'

# 3. inspect at any time (read-only)
iii trigger migrate::status --json '{}'

# 4. regenerate types after every schema change
iii trigger migrate::codegen --json '{}'
```

Commit `migrations/` and the generated `db.types.ts` together — reviewers see
the schema change and its type impact in one diff.

## Production

Set `auto: true` in the production config: every replica runs `migrate::up`
once at startup, concurrently and safely.

```
 replica A ──┐
 replica B ──┼──▶ take lock ──▶ apply pending ──▶ record in _iii_migrations
 replica C ──┘         │
                       └─▶ losers wait, re-check, report "skipped"
```

- **Postgres** — each migration batch takes
  `pg_advisory_xact_lock(<constant key>)` first. Concurrent migrators
  serialize on the lock; a replica that loses the race detects the migration
  was applied (same checksum in `_iii_migrations`) and counts it as
  *skipped*, not failed.
- **SQLite** — batches run `BEGIN IMMEDIATE` (via the database worker's
  `serializable` isolation), which serializes writers.

A failed migration rolls back atomically and the worker stays up, so
`migrate::status` remains available for diagnosis. See
[errors.md](errors.md) for the recovery steps.

## Recipes

### Add a table

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

### Rename or drop a table

Updating or deleting is a **new** migration, never an edit of an applied
file:

```sql
-- migrations/20260721090000_rename_orders.sql
ALTER TABLE orders RENAME TO purchases;
```

```sql
-- migrations/20260722080000_drop_legacy_logs.sql
DROP TABLE legacy_logs;
```

Then `migrate::up` + `migrate::codegen` — the new `db.types.ts` gains,
renames, or loses the matching interface, and your TypeScript stops compiling
wherever the old shape was used. That is the point.

### Columns

```sql
-- add (safe on both dialects)
ALTER TABLE users ADD COLUMN email text;

-- rename (Postgres and SQLite >= 3.25)
ALTER TABLE users RENAME COLUMN email TO primary_email;

-- change type (Postgres only)
ALTER TABLE users ALTER COLUMN amount TYPE numeric(14, 2);

-- drop (Postgres; SQLite >= 3.35)
ALTER TABLE users DROP COLUMN legacy_flag;
```

### SQLite type change (recreate pattern)

SQLite cannot `ALTER COLUMN TYPE`. Use the documented recreate pattern
**inside one migration** — the whole file runs as a single atomic batch, so
the table swap is all-or-nothing:

```sql
CREATE TABLE users_new (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score REAL NOT NULL DEFAULT 0);
INSERT INTO users_new (id, name, score) SELECT id, name, CAST(score AS REAL) FROM users;
DROP TABLE users;
ALTER TABLE users_new RENAME TO users;
```
