#!/usr/bin/env bash
# SQLite playground: engine + database worker (engine-managed) + miiigrate.
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

# ---------- preflight ----------
[[ -x "$III_BIN" ]] || fail "iii engine binary not found (install: curl -fsSL https://install.iii.dev/iii/main/install.sh | sh)"
if lsof -iTCP:"$WS_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  fail "port $WS_PORT already in use — is another iii engine running?"
fi
[[ -x "$HOME/.iii/workers/database" ]] || {
  echo "database worker not installed; running \`iii worker add database\`..."
  (cd "$RUN_DIR" 2>/dev/null || cd /tmp; "$III_BIN" worker add database >/dev/null)
}

if [[ "$NO_BUILD" == 0 ]]; then
  (cd "$CRATE_DIR" && cargo build)
fi
[[ -x "$MIIIGRATE_BIN" ]] || fail "miiigrate binary missing at $MIIIGRATE_BIN"

# ---------- fresh run dir ----------
rm -rf "$RUN_DIR"
mkdir -p "$RUN_DIR"
cp "$ROOT_DIR/engine.config.yaml" "$RUN_DIR/config.yaml"

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
(cd "$ROOT_DIR" && exec "$MIIIGRATE_BIN" \
    --config ./miiigrate.seed.yaml --url "ws://127.0.0.1:$WS_PORT") \
  >"$RUN_DIR/miiigrate.log" 2>&1 &
MIIIGRATE_PID=$!

# ---------- wait until migrate::status answers ----------
echo "waiting for migrate::status..."
STATUS=""
for _ in $(seq 1 40); do
  if STATUS=$("$III_BIN" trigger migrate::status --json '{}' --port "$WS_PORT" 2>/dev/null); then
    break
  fi
  kill -0 "$MIIIGRATE_PID" 2>/dev/null || { cat "$RUN_DIR/miiigrate.log"; fail "miiigrate exited early"; }
  sleep 0.5
done
[[ -n "$STATUS" ]] || { cat "$RUN_DIR/miiigrate.log"; fail "migrate::status never answered"; }

echo "migrate::status -> $STATUS"

# ---------- assertions (phase 1: everything pending) ----------
python3 - "$STATUS" <<'PY'
import json, sys
s = json.loads(sys.argv[1])
assert s["db"] == "primary", s
assert s["dialect"] == "sqlite", s
assert [p["name"] for p in s["pending"]] == [
    "20260101120000_create_users.sql",
    "20260102090000_add_email.sql",
], s["pending"]
assert s["applied"] == [] and s["mismatched"] == [] and s["missing"] == [], s
print("OK: status reports 2 pending, 0 applied, sqlite dialect")
PY

echo "PLAYGROUND OK"
