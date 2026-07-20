# miiigrate documentation

Reference documentation for the miiigrate worker. The
[README](../README.md) gets you from zero to a first applied migration;
these pages hold the details.

| Page | What it covers |
|---|---|
| [functions.md](functions.md) | The four `migrate::*` functions — payloads, responses, behaviour |
| [configuration.md](configuration.md) | Every config key, defaults, seed vs runtime configuration |
| [workflows.md](workflows.md) | Local development, production `auto: true`, table and column recipes |
| [codegen.md](codegen.md) | TypeScript type mapping and wire-format rules |
| [errors.md](errors.md) | Every error code, what it means, how to recover |
| [changelog.md](changelog.md) | Release history |
| [roadmap.md](roadmap.md) | Planned work — and what is deliberately not planned |

## The model in one page

miiigrate never connects to a database. It talks to the
[`database` worker](https://workers.iii.dev/workers/database) over the iii
engine:

```
┌──────────────┐    database::query / execute / transaction    ┌──────────┐
│  miiigrate   │ ────────────────────────────────────────────▶ │ database │──▶ Postgres
└──────────────┘                                               │  worker  │──▶ SQLite
```

Migrations are plain `.sql` files named `YYYYMMDDHHMMSS_slug.sql`, applied in
name order. Each applied file is recorded in a `_iii_migrations` tracking
table with a SHA-256 checksum. Three invariants follow:

1. **Forward-only** — an applied file must never change. If it does,
   `migrate::up` refuses to run (`CHECKSUM_MISMATCH`) before applying
   anything. Undo means writing a new migration.
2. **Atomic** — each file runs as one `database::transaction` batch: all its
   statements apply, or none do.
3. **Concurrency-safe** — concurrent migrators serialize on
   `pg_advisory_xact_lock` (Postgres) or `BEGIN IMMEDIATE` (SQLite); a
   replica that loses the race reports the migration as *skipped*.
