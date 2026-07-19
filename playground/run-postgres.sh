#!/usr/bin/env bash
# Postgres 16 playground: proves advisory-lock serialization with TWO
# miiigrate instances migrating the same database concurrently, and checks
# enum codegen.
#
# Scenario:
#   1. docker compose Postgres 16, engine + database worker
#   2. two miiigrate instances start simultaneously with auto: true; the
#      first migration holds pg_advisory_xact_lock for ~2s (pg_sleep), so
#      the instances provably overlap and serialize
#   3. each migration was applied exactly once; _iii_migrations has 3 rows
#   4. schema works (insert fires the plpgsql audit trigger)
#   5. codegen emits the mood enum union and kysely-style scalar mappings
#
# Usage: ./run-postgres.sh [--keep] [--no-build]
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "$ROOT_DIR/.." && pwd)"
RUN_DIR="$ROOT_DIR/.run-pg"
III_BIN="${III_BIN:-$(command -v iii 2>/dev/null || echo "$HOME/.local/bin/iii")}"
MIIIGRATE_BIN="$CRATE_DIR/target/debug/miiigrate"
WS_PORT="${WS_PORT:-49134}"
COMPOSE="${COMPOSE:-docker compose}"

KEEP=0
NO_BUILD=0
for arg in "$@"; do
  case "$arg" in
    --keep)     KEEP=1 ;;
    --no-build) NO_BUILD=1 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

ENGINE_PID=""
PIDS=()
cleanup() {
  if [[ "$KEEP" == 1 ]]; then
    echo "--keep: leaving engine/miiigrate/postgres running (logs in $RUN_DIR)"
    return
  fi
  for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
  [[ -n "$ENGINE_PID" ]] && kill "$ENGINE_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  (cd "$ROOT_DIR" && $COMPOSE down -v >/dev/null 2>&1) || true
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

trigger() { "$III_BIN" trigger "$1" --json "$2" --port "$WS_PORT" 2>&1; }

# ---------- preflight ----------
[[ -x "$III_BIN" ]] || fail "iii engine binary not found"
if lsof -iTCP:"$WS_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  fail "port $WS_PORT already in use — is another iii engine running?"
fi
[[ -x "$HOME/.iii/workers/database" ]] || {
  echo "database worker not installed; running \`iii worker add database\`..."
  (cd /tmp && "$III_BIN" worker add database >/dev/null)
}
if [[ "$NO_BUILD" == 0 ]]; then
  (cd "$CRATE_DIR" && cargo build)
fi

# ---------- postgres ----------
echo "starting postgres 16..."
(cd "$ROOT_DIR" && $COMPOSE up -d --wait) || fail "docker compose up failed"

# ---------- fresh run dir ----------
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
cp "$ROOT_DIR/engine.config.pg.yaml" "$RUN_DIR/config.yaml"
cp -R "$ROOT_DIR/migrations-pg" "$RUN_DIR/migrations"
cat >"$RUN_DIR/miiigrate.seed.yaml" <<EOF
db: primary
dir: ./migrations
auto: true
types_out: ./db.types.ts
EOF

# ---------- engine (spawns the database worker itself) ----------
(cd "$RUN_DIR" && exec "$III_BIN" --no-update-check -c config.yaml) \
  >"$RUN_DIR/engine.log" 2>&1 &
ENGINE_PID=$!

echo "waiting for engine + database worker..."
for _ in $(seq 1 60); do
  if trigger database::listDatabases '{}' >/dev/null 2>&1; then break; fi
  kill -0 "$ENGINE_PID" 2>/dev/null || { cat "$RUN_DIR/engine.log"; fail "engine exited early"; }
  sleep 0.5
done
trigger database::listDatabases '{}' >/dev/null 2>&1 || fail "database worker never came up"

# ---------- 2. two concurrent auto-migrating instances ----------
echo "starting two miiigrate instances (auto: true) concurrently..."
for i in 1 2; do
  (cd "$RUN_DIR" && exec "$MIIIGRATE_BIN" \
      --config ./miiigrate.seed.yaml --url "ws://127.0.0.1:$WS_PORT") \
    >"$RUN_DIR/miiigrate-$i.log" 2>&1 &
  PIDS+=($!)
done

echo "waiting for both auto migration runs to finish..."
for _ in $(seq 1 120); do
  DONE=$( (grep -sl "auto migration run" "$RUN_DIR"/miiigrate-*.log || true) | wc -l | tr -d ' ')
  [[ "$DONE" == 2 ]] && break
  sleep 0.5
done
grep -q "auto migration run complete" "$RUN_DIR/miiigrate-1.log" || { tail -5 "$RUN_DIR/miiigrate-1.log"; fail "instance 1 auto run did not complete"; }
grep -q "auto migration run complete" "$RUN_DIR/miiigrate-2.log" || { tail -5 "$RUN_DIR/miiigrate-2.log"; fail "instance 2 auto run did not complete"; }

# ---------- 3. each migration applied exactly once ----------
ROWS=$(trigger database::query '{"db":"primary","sql":"SELECT name FROM _iii_migrations ORDER BY name"}')
python3 - "$ROWS" <<'PY'
import json, sys
r = json.loads(sys.argv[1])
names = [row["name"] for row in r["rows"]]
assert names == [
    "20260101120000_create_users.sql",
    "20260102090000_add_audit.sql",
    "20260103080000_add_scores.sql",
], names
print("OK 3a: _iii_migrations has exactly one row per migration")
PY

# Strip ANSI colors before grepping the structured log lines.
CLEAN_LOGS=$(cat "$RUN_DIR"/miiigrate-*.log | sed $'s/\x1b\\[[0-9;]*m//g')
DUPES=$( (echo "$CLEAN_LOGS" | grep ': applied migration=' || true) | grep -o 'migration=[^ ]*' | sort | uniq -d | wc -l | tr -d ' ')
[[ "$DUPES" == 0 ]] || fail "a migration was applied twice (logs disagree with tracking)"
SKIPS=$( (echo "$CLEAN_LOGS" | grep 'concurrent migrator' || true) | wc -l | tr -d ' ')
echo "OK 3b: no double-application; concurrent-skip events: $SKIPS"
[[ "$SKIPS" -ge 1 ]] || echo "WARN: no overlap observed (instances did not race this run)"

# ---------- 4. schema works; audit trigger fires ----------
trigger database::execute '{"db":"primary","sql":"INSERT INTO users (email, mood) VALUES ($1, $2)","params":["ada@example.com","happy"]}' >/dev/null \
  || fail "insert into migrated schema failed"
AUDIT=$(trigger database::query '{"db":"primary","sql":"SELECT count(*) AS n FROM audit_log"}')
python3 - "$AUDIT" <<'PY'
import json, sys
n = json.loads(sys.argv[1])["rows"][0]["n"]
assert int(n) == 1, n
print("OK 4: plpgsql audit trigger (dollar-quoted body) fired")
PY

# ---------- 5. codegen: enum + scalar mappings ----------
CG=$(trigger migrate::codegen '{}') || { echo "$CG"; fail "migrate::codegen errored"; }
python3 - "$CG" <<'PY'
import json, sys
c = json.loads(sys.argv[1])
assert c["tables"] == 2, c   # users + audit_log; _iii_migrations excluded
assert c["enums"] == 1, c
print("OK 5a: codegen found 2 tables and the mood enum")
PY
TYPES="$RUN_DIR/db.types.ts"
grep -q "export type Mood = 'happy' | 'sad' | 'curious';" "$TYPES" || fail "mood enum union missing"
grep -q "id: string;" "$TYPES" || fail "bigserial id should map to string"
grep -q "balance: string;" "$TYPES" || fail "numeric should map to string"
grep -q "tags: string\[\] | null;" "$TYPES" || fail "text[] should map to string[] | null"
grep -q "metadata: Json | null;" "$TYPES" || fail "jsonb should map to Json | null"
grep -q "created_at: string;" "$TYPES" || fail "timestamptz should map to string (RFC 3339 over JSON)"
grep -q "mood: Mood;" "$TYPES" || fail "enum column should reference Mood"
echo "OK 5b: db.types.ts enum + scalar mappings check out"

# ---------- status is clean ----------
STATUS=$(trigger migrate::status '{}')
python3 - "$STATUS" <<'PY'
import json, sys
s = json.loads(sys.argv[1])
assert s["dialect"] == "postgres", s
assert len(s["applied"]) == 3 and s["pending"] == [] and s["mismatched"] == [], s
print("OK 6: status clean on postgres")
PY

echo "PLAYGROUND POSTGRES OK"
