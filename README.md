# miiigrate

[![Release](https://img.shields.io/github/v/release/aircodev/miiigrate?sort=semver&label=release)](https://github.com/aircodev/miiigrate/releases)
[![CI](https://github.com/aircodev/miiigrate/actions/workflows/ci.yml/badge.svg)](https://github.com/aircodev/miiigrate/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](https://github.com/aircodev/miiigrate/blob/main/LICENSE)

Forward-only SQL migrations for [iii](https://iii.dev). You write plain `.sql`
files, miiigrate applies and tracks them through the
[`database` worker](https://workers.iii.dev/workers/database) — it never opens
a database connection of its own. It can also generate TypeScript types that
describe your schema exactly as it arrives over the wire.

```
        you / an agent / CI
                │
                │   iii trigger migrate::up | status | create | codegen
                ▼
        ┌──────────────┐    database::query / execute / transaction
        │  miiigrate   │ ──────────────────────────────────────────▶ ┌──────────┐
        └──────────────┘           (no direct DB connection)         │ database │──▶ Postgres
          │          │                                               │  worker  │──▶ SQLite
          │ reads    │ writes                                        └──────────┘
          ▼          ▼
  migrations/*.sql   db.types.ts
```

The whole workflow is four functions, used in this order:

```
migrate::create ──▶ write your SQL ──▶ migrate::up ──▶ migrate::codegen
   (scaffold)        (plain .sql)        (apply)        (TypeScript)
```

## Install

```bash
iii worker add database
iii worker add miiigrate
```

miiigrate requires the `database` worker: every read goes through
`database::query`, every migration through one atomic
`database::transaction`. Point the `database` worker at your database
(SQLite or Postgres), start the engine with `iii`, and the four
`migrate::*` functions are available.

Using an AI agent (Claude Code, Cursor, …)? Install the agent skill so it
knows when and how to drive miiigrate:

```bash
npx skills add aircodev/miiigrate
```

## Quickstart

```sh
# 1. scaffold a migration file
iii trigger migrate::create --json '{"name":"create_users"}'
#    -> ./migrations/20260719143000_create_users.sql

# 2. write your SQL in that file, then apply everything pending
iii trigger migrate::up --json '{}'
#    -> { "applied": ["20260719143000_create_users.sql"], "skipped": 0, "duration_ms": 6 }

# 3. check the state at any time (read-only)
iii trigger migrate::status --json '{}'
#    -> { "applied": [...], "pending": [...], "mismatched": [], "missing": [] }

# 4. regenerate TypeScript types after every schema change
iii trigger migrate::codegen --json '{}'
#    -> ./db.types.ts — one interface per table
```

**Payloads with quotes.** SQL regularly contains single quotes and `$$`
blocks, which fight the shell when inlined into `--json '…'` (the iii CLI
has no `@file`/stdin form). Put the payload in a file and let the shell do
the reading — no escaping needed:

```sh
cat > /tmp/payload.json <<'EOF'
{ "db": "primary", "sql": "UPDATE users SET motto = 'don''t panic' WHERE id = $1", "params": [1] }
EOF
iii trigger database::execute --json "$(cat /tmp/payload.json)"
```

Simple scalar fields can also skip JSON entirely: `iii trigger
migrate::create name=add_users` (key=value pairs merge over `--json`).

Three rules keep it simple and safe:

- **Forward-only.** Never edit an applied file — write a new migration to
  change the schema again. Edits are caught by checksum before anything runs.
- **Atomic.** Each file runs as one transaction: it fully applies or fully
  rolls back.
- **Concurrency-safe.** With `auto: true`, every replica can run
  `migrate::up` at startup; they serialize on a lock and losers report
  *skipped*, not *failed*.

## Configuration

```yaml
workers:
  - name: database
    config:
      databases:
        primary:
          url: sqlite:./data/app.db    # or postgres://…
  - name: miiigrate
    config:
      db: primary                # database name in the database worker
      dir: ./migrations          # folder of YYYYMMDDHHMMSS_slug.sql files
      auto: false                # true = run migrate::up at startup
      types_out: ./db.types.ts   # codegen output (optional); when set,
                                 # types regenerate after each migrate::up
                                 # (opt out with codegen_on_up: false)
```

The `config:` block is a first-boot seed; afterwards settings live in the
`configuration` worker under id `miiigrate`. Details and defaults:
[docs/configuration.md](https://github.com/aircodev/miiigrate/blob/main/docs/configuration.md).

## Documentation

| Page | What it covers |
|---|---|
| [Functions](https://github.com/aircodev/miiigrate/blob/main/docs/functions.md) | `migrate::up`, `status`, `create`, `codegen` — payloads and responses |
| [Configuration](https://github.com/aircodev/miiigrate/blob/main/docs/configuration.md) | Every key, defaults, seed vs runtime config |
| [Workflows](https://github.com/aircodev/miiigrate/blob/main/docs/workflows.md) | Local dev, production `auto: true`, table and column recipes |
| [Codegen](https://github.com/aircodev/miiigrate/blob/main/docs/codegen.md) | TypeScript type mapping, wire-format rules |
| [Errors](https://github.com/aircodev/miiigrate/blob/main/docs/errors.md) | Every error code and how to recover |
| [Changelog](https://github.com/aircodev/miiigrate/blob/main/docs/changelog.md) | Releases |
| [Roadmap](https://github.com/aircodev/miiigrate/blob/main/docs/roadmap.md) | What is planned, and what is not |

## Development

The [`playground/`](https://github.com/aircodev/miiigrate/tree/main/playground)
directory boots a real engine plus database worker and runs the full scenario:
[`run.sh`](https://github.com/aircodev/miiigrate/blob/main/playground/run.sh)
(SQLite end-to-end) and
[`run-postgres.sh`](https://github.com/aircodev/miiigrate/blob/main/playground/run-postgres.sh)
(Postgres 16, two concurrent instances proving lock serialization).

## License

Apache-2.0
