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

## Coming from drizzle

A project that already uses drizzle-kit migrates in one call — on the
database that drizzle already migrated **and** on fresh environments:

```sh
# 1. adopt: convert ./drizzle into ./migrations and mirror what drizzle
#    already applied (their SQL is NOT re-executed)
iii trigger migrate::adopt --json '{"source":"drizzle"}'
#    -> { "converted": [...], "baselined": [...], "pending": [], "skipped": 0 }

# 2. verify: everything applied, nothing pending, no drift
iii trigger migrate::status --json '{}'

# 3. generate the types that replace drizzle's schema.ts inference
iii trigger migrate::codegen --json '{}'
```

- File names are derived from the drizzle journal's `when` timestamps, so
  the adopted history keeps its real dates and order. Contents are copied
  byte-for-byte (`--> statement-breakpoint` markers are comments the
  splitter ignores).
- The `./drizzle` folder is never modified. Once `migrate::status` is
  clean, delete it along with `drizzle.config.ts` and the `drizzle-kit`
  dependency; dropping the `drizzle.__drizzle_migrations` table is
  optional.
- On a **fresh** database (CI, new dev machine) the same adopted files are
  simply pending: `migrate::up` executes them for real. One history, both
  cases.
- `hash_warnings` in the response means a file no longer matches what
  drizzle recorded at apply time — the file on disk wins, but review the
  diff before trusting it.
- Runtime code that used drizzle-orm's query builder needs a separate
  decision: either keep it (direct DB connection, outside the database
  worker) or move to `database::query` typed by the generated
  `db.types.ts`.

From then on the daily workflow is the standard one: `migrate::create` →
write SQL → `migrate::up` → `migrate::codegen`. You write `ALTER TABLE`
statements yourself now — there is no TS-schema diffing anymore, and that
is by design (plain SQL, reviewed in the diff).

## Adopting a hand-built schema

No drizzle, just a database that predates miiigrate: dump the schema into a
single migration and baseline it.

```sh
iii trigger migrate::create --json '{"name":"baseline"}'
# fill the file with the schema dump (pg_dump --schema-only, cleaned up)

# on the existing database: record it as applied WITHOUT executing
iii trigger migrate::baseline --json '{}'

# fresh environments run it for real
iii trigger migrate::up --json '{}'
```

Once baselined, the file is applied history: never edit it again.

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

### Postgres recreate pattern (column reorder, incompatible type change)

Postgres has no `ALTER TABLE … REORDER`; when the column order matters or a
type change is beyond `ALTER COLUMN TYPE`, recreate the table **inside one
migration** (single atomic batch — the swap is all-or-nothing). The DDL is
straightforward; what bites are the objects attached to the *old* table
that silently die with it. Canonical example, reordering `users`:

```sql
-- 1. New shape, everything declared up front (defaults, constraints).
CREATE TABLE users_new (
    id bigint PRIMARY KEY DEFAULT nextval('users_id_seq'),
    first_name text,
    last_name text,
    email text UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now()
);

-- 2. Copy the data, naming columns explicitly on both sides.
INSERT INTO users_new (id, first_name, last_name, email, created_at)
SELECT id, first_name, last_name, email, created_at FROM users;

-- 3. Re-point the sequence: it is OWNED BY the old table and would be
--    dropped with it otherwise.
ALTER SEQUENCE users_id_seq OWNED BY users_new.id;

-- 4. Incoming foreign keys (other tables referencing users) must be
--    dropped before the old table can go, and recreated against the new.
ALTER TABLE reservations DROP CONSTRAINT reservations_user_id_fkey;

-- 5. Swap.
DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

-- 6. Recreate what lived on (or pointed at) the old table:
ALTER TABLE reservations
    ADD CONSTRAINT reservations_user_id_fkey
    FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE;
CREATE INDEX idx_users_mood ON users (mood);
CREATE TRIGGER users_audit AFTER INSERT OR UPDATE ON users
    FOR EACH ROW EXECUTE FUNCTION audit_users();
```

Checklist of the classic omissions — each dies silently with `DROP TABLE`:

- **Incoming foreign keys** from other tables (step 4/6) — list them first:
  they do not show up on the table you are recreating.
- **The sequence** behind `bigserial`/`serial` (step 3) — without
  `OWNED BY` it is dropped and inserts start failing.
- **Indexes** beyond the primary key, **triggers**, and non-inline
  **constraints** (step 6).
- **Column defaults** — declare them in step 1, they are not copied.

Verify the result with `migrate::schema` (columns in order, foreign keys,
indexes, triggers) before writing the next migration.
