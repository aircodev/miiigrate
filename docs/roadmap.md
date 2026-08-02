# Roadmap

Planned, roughly in order:

- **Drift-check cron** (v2) — periodic `migrate::status` via the engine's
  cron trigger, publishing `mismatched` / `missing` findings on pubsub for
  alerting.
- **`-- migrate:no-transaction` directive** — opt-out for statements that
  refuse to run inside a transaction (`CREATE INDEX CONCURRENTLY`, …). Until
  then the per-migration ceiling is the 300 s `database::transaction`
  invocation timeout.
- **Registry publication** — the repo is package-ready (`iii.worker.yaml`,
  per-target tar.gz + sha256); publishing to
  [workers.iii.dev](https://workers.iii.dev) lands as soon as the registry's
  publish flow for external repos is documented.
- **MySQL** — needs a `GET_LOCK`-based serialization strategy; rejected with
  `UNSUPPORTED_DIALECT` today.
- **Squash** — flatten a long applied history into one baseline file for
  fresh environments. (`migrate::baseline` and `migrate::adopt` shipped in
  0.1.5; squash is the remaining piece.)
- **`migrate::adopt` sources** — prisma (`prisma/migrations`), golang-migrate,
  on the model of the drizzle adopter.

## Not planned

- **Down migrations** — forward-only by design; write a new migration to
  undo.
- **Schema DSL** — migrations are plain SQL.
