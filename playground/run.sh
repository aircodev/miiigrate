#!/usr/bin/env bash
# SQLite playground: engine + database worker (engine-managed) + miiigrate.
#
# Scenario:
#   1. migrate::status reports the two fixture migrations as pending
#   2. migrate::up applies both
#   3. _iii_migrations rows verified through database::query
#   4. second migrate::up is a no-op (idempotence)
#   5. tampering with an applied file makes migrate::up fail CHECKSUM_MISMATCH
#      and migrate::status report it as mismatched
#
# Usage: ./run.sh [--keep] [--no-build]
#   --keep      leave engine + miiigrate running after the checks
#   --no-build  skip cargo build
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "$ROOT_DIR/.." && pwd)"
RUN_DIR="$ROOT_DIR/.run"
III_BIN="${III_BIN:-$(command -v iii 2>/dev/null || echo "$HOME/.local/bin/iii")}"
MIIIGRATE_BIN="$CRATE_DIR/target/debug/miiigrate"
WS_PORT="${WS_PORT:-49134}"

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
MIIIGRATE_PID=""
cleanup() {
  if [[ "$KEEP" == 1 ]]; then
    echo "--keep: engine pid=$ENGINE_PID, miiigrate pid=$MIIIGRATE_PID left running (logs in $RUN_DIR)"
    return
  fi
  [[ -n "$MIIIGRATE_PID" ]] && kill "$MIIIGRATE_PID" 2>/dev/null || true
  [[ -n "$ENGINE_PID" ]] && kill "$ENGINE_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }

trigger() { # trigger <function> <json> — stdout: response, non-zero on error
  "$III_BIN" trigger "$1" --json "$2" --port "$WS_PORT" 2>&1
}

# ---------- preflight ----------
[[ -x "$III_BIN" ]] || fail "iii engine binary not found (install: curl -fsSL https://install.iii.dev/iii/main/install.sh | sh)"
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
[[ -x "$MIIIGRATE_BIN" ]] || fail "miiigrate binary missing at $MIIIGRATE_BIN"

# ---------- fresh run dir ----------
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
cp "$ROOT_DIR/engine.config.yaml" "$RUN_DIR/config.yaml"
cp -R "$ROOT_DIR/migrations" "$RUN_DIR/migrations"
cat >"$RUN_DIR/miiigrate.seed.yaml" <<EOF
db: primary
dir: ./migrations
auto: false
types_out: ./db.types.ts
EOF

# ---------- engine (spawns the database worker itself) ----------
(cd "$RUN_DIR" && exec "$III_BIN" --no-update-check -c config.yaml) \
  >"$RUN_DIR/engine.log" 2>&1 &
ENGINE_PID=$!

echo "waiting for engine on :$WS_PORT..."
for _ in $(seq 1 60); do
  lsof -iTCP:"$WS_PORT" -sTCP:LISTEN >/dev/null 2>&1 && break
  kill -0 "$ENGINE_PID" 2>/dev/null || { cat "$RUN_DIR/engine.log"; fail "engine exited early"; }
  sleep 0.5
done
lsof -iTCP:"$WS_PORT" -sTCP:LISTEN >/dev/null 2>&1 || fail "engine did not open :$WS_PORT"

# ---------- miiigrate (spawned here; not in the registry yet) ----------
(cd "$RUN_DIR" && exec "$MIIIGRATE_BIN" \
    --config ./miiigrate.seed.yaml --url "ws://127.0.0.1:$WS_PORT") \
  >"$RUN_DIR/miiigrate.log" 2>&1 &
MIIIGRATE_PID=$!

# ---------- wait until migrate::status answers ----------
echo "waiting for migrate::status..."
STATUS=""
for _ in $(seq 1 40); do
  if STATUS=$(trigger migrate::status '{}' 2>/dev/null); then
    break
  fi
  kill -0 "$MIIIGRATE_PID" 2>/dev/null || { cat "$RUN_DIR/miiigrate.log"; fail "miiigrate exited early"; }
  sleep 0.5
done
[[ -n "$STATUS" ]] || { cat "$RUN_DIR/miiigrate.log"; fail "migrate::status never answered"; }

# ---------- 1. everything pending ----------
python3 - "$STATUS" <<'PY'
import json, sys
s = json.loads(sys.argv[1])
assert s["db"] == "primary" and s["dialect"] == "sqlite", s
assert [p["name"] for p in s["pending"]] == [
    "20260101120000_create_users.sql",
    "20260102090000_add_email.sql",
], s["pending"]
assert s["applied"] == [] and s["mismatched"] == [] and s["missing"] == [], s
print("OK 1: status -> 2 pending")
PY

# ---------- 2. up applies both ----------
UP1=$(trigger migrate::up '{}') || { echo "$UP1"; fail "migrate::up errored"; }
python3 - "$UP1" <<'PY'
import json, sys
u = json.loads(sys.argv[1])
assert u["applied"] == [
    "20260101120000_create_users.sql",
    "20260102090000_add_email.sql",
], u
assert u["skipped"] == 0, u
assert isinstance(u["duration_ms"], int), u
print(f"OK 2: up applied 2 in {u['duration_ms']}ms")
PY

# ---------- 3. tracking table has both rows ----------
ROWS=$(trigger database::query '{"db":"primary","sql":"SELECT name, checksum, applied_at FROM _iii_migrations ORDER BY name"}')
python3 - "$ROWS" <<'PY'
import json, sys
r = json.loads(sys.argv[1])
names = [row["name"] for row in r["rows"]]
assert names == ["20260101120000_create_users.sql", "20260102090000_add_email.sql"], r
for row in r["rows"]:
    assert len(row["checksum"]) == 64, row
    assert row["applied_at"], row
print("OK 3: _iii_migrations has 2 rows with checksums and timestamps")
PY

# ---------- schema really migrated? ----------
trigger database::execute '{"db":"primary","sql":"INSERT INTO users (name, email) VALUES (?, ?)","params":["ada","ada@example.com"]}' >/dev/null \
  || fail "schema not usable after migration"

# ---------- 4. idempotence ----------
UP2=$(trigger migrate::up '{}') || { echo "$UP2"; fail "second migrate::up errored"; }
python3 - "$UP2" <<'PY'
import json, sys
u = json.loads(sys.argv[1])
assert u["applied"] == [] and u["skipped"] == 2, u
print("OK 4: second up is a no-op (skipped 2)")
PY

# ---------- 5. tamper an applied migration ----------
echo "-- tampered after apply" >> "$RUN_DIR/migrations/20260101120000_create_users.sql"

set +e
UP3=$(trigger migrate::up '{}')
UP3_RC=$?
set -e
[[ $UP3_RC -ne 0 ]] || fail "migrate::up should fail after tampering, got: $UP3"
echo "$UP3" | grep -q "CHECKSUM_MISMATCH" || fail "expected CHECKSUM_MISMATCH, got: $UP3"
echo "OK 5a: up fails with CHECKSUM_MISMATCH"

STATUS2=$(trigger migrate::status '{}')
python3 - "$STATUS2" <<'PY'
import json, sys
s = json.loads(sys.argv[1])
assert [m["name"] for m in s["mismatched"]] == ["20260101120000_create_users.sql"], s
m = s["mismatched"][0]
assert m["applied_checksum"] != m["file_checksum"], m
assert [a["name"] for a in s["applied"]] == ["20260102090000_add_email.sql"], s
print("OK 5b: status reports the tampered file as mismatched")
PY

echo "PLAYGROUND OK"
