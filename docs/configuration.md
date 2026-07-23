# Configuration

## Where configuration lives

The `config:` block under the worker entry in the engine's `config.yaml` is a
**first-boot seed** only. After the first start, the runtime source of truth
is the [`configuration` worker](https://workers.iii.dev/workers/configuration)
entry with id `miiigrate` (persisted at `./data/configuration/miiigrate.yaml`
in a default engine setup).

```
config.yaml  ──(first boot seed)──▶  configuration worker, id "miiigrate"
                                              │
                                              ▼  read once at startup
                                         miiigrate
```

Configuration is read **once at worker startup**. There is no hot reload — a
running migration must not change its target database mid-flight. To apply a
config change, restart the worker (or the engine).

## Keys

| key | default | meaning |
|-----|---------|---------|
| `db` | `primary` | Logical database name in the `database` worker's configuration. |
| `dir` | `./migrations` | Migration folder, resolved against the worker's working directory. Only `migrate::create` creates it. |
| `auto` | `false` | Run `migrate::up` once at startup. If the `database` worker is not registered yet (no ordering guarantee under docker compose or engine-managed boots), the run is retried with capped backoff for up to 2 minutes. Best-effort: any other failure is logged and the worker stays up, so `migrate::status` remains available for diagnosis. |
| `types_out` | — | Output path for `migrate::codegen`. Optional; without it, codegen requires `out` in the payload. |
| `codegen_on_up` | follows `types_out` | Run `migrate::codegen` after a `migrate::up` that applied at least one migration. Defaults to `true` when `types_out` is set, `false` otherwise; set explicitly to override. Codegen failures are logged and never fail the up run. |
| `dialect` | auto | `postgres` or `sqlite`. By default the dialect is detected from the `database` worker's `database::listDatabases` driver field. |

## Example

```yaml
workers:
  - name: database
    config:
      databases:
        primary:
          url: postgres://app:secret@db:5432/app
  - name: miiigrate
    config:
      db: primary
      dir: ./migrations
      auto: true                 # production: every replica migrates at startup
      types_out: ./db.types.ts
```

A commented seed file lives at
[`config.yaml.example`](../config.yaml.example).

## Requirements

- The [`database` worker](https://workers.iii.dev/workers/database) must be
  registered on the same engine and configured with the database named by
  `db`. If it is not, calls fail with `DATABASE_WORKER_UNAVAILABLE`.
- The target must be Postgres or SQLite. Anything else is rejected with
  `UNSUPPORTED_DIALECT`.
