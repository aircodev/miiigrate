#!/usr/bin/env bash
# Drizzle-adoption playground: engine + database worker + miiigrate (SQLite).
#
# Simulates a project that drizzle already migrated (schema of migrations
# 0000+0001 applied, __drizzle_migrations populated; 0002 exists on disk but
# was never applied), then proves the takeover:
#   1. migrate::adopt converts ./drizzle -> ./migrations, baselines the two
#      applied migrations WITHOUT executing them, leaves 0002 pending
#   2. migrate::status shows 2 applied / 1 pending, no drift
#   3. migrate::up applies only the pending one; the schema is then usable
#   4. re-running adopt is a no-op (idempotence)
#   5. a drifted hash in __drizzle_migrations surfaces as hash_warnings
#   6. a mysql journal is rejected before touching anything
#
# Usage: ./run-adopt.sh [--keep] [--no-build]
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE_DIR="$(cd "$ROOT_DIR/.." && pwd)"
RUN_DIR="$ROOT_DIR/.run-adopt"
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

sha256_of() { python3 -c "import hashlib,sys; print(hashlib.sha256(open(sys.argv[1],'rb').read()).hexdigest())" "$1"; }

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
mkdir -p "$RUN_DIR/migrations"
cp "$ROOT_DIR/engine.config.yaml" "$RUN_DIR/config.yaml"
cp -R "$ROOT_DIR/drizzle-sqlite" "$RUN_DIR/drizzle"
cat >"$RUN_DIR/miiigrate.seed.yaml" <<EOF
db: primary
dir: ./migrations
auto: false
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

# ---------- miiigrate ----------
(cd "$RUN_DIR" && exec "$MIIIGRATE_BIN" \
    --config ./miiigrate.seed.yaml --url "ws://127.0.0.1:$WS_PORT") \
  >"$RUN_DIR/miiigrate.log" 2>&1 &
MIIIGRATE_PID=$!

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

# ---------- simulate the drizzle-migrated state ----------
# Schema of 0000 + 0001 as drizzle would have left it (0002 never applied).
trigger database::execute '{"db":"primary","sql":"CREATE TABLE `users` (`id` integer PRIMARY KEY AUTOINCREMENT NOT NULL, `email` text NOT NULL, `name` text)"}' >/dev/null
trigger database::execute '{"db":"primary","sql":"CREATE UNIQUE INDEX `users_email_unique` ON `users` (`email`)"}' >/dev/null
trigger database::execute '{"db":"primary","sql":"CREATE TABLE `orders` (`id` integer PRIMARY KEY AUTOINCREMENT NOT NULL, `user_id` integer NOT NULL, `amount` real NOT NULL, FOREIGN KEY (`user_id`) REFERENCES `users`(`id`) ON UPDATE no action ON DELETE no action)"}' >/dev/null
trigger database::execute '{"db":"primary","sql":"CREATE INDEX `orders_user_id_idx` ON `orders` (`user_id`)"}' >/dev/null

# Drizzle's own tracking table, exactly as its sqlite migrator writes it.
H0=$(sha256_of "$RUN_DIR/drizzle/0000_create_users.sql")
H1=$(sha256_of "$RUN_DIR/drizzle/0001_add_orders.sql")
trigger database::execute '{"db":"primary","sql":"CREATE TABLE __drizzle_migrations (id INTEGER PRIMARY KEY AUTOINCREMENT, hash text NOT NULL, created_at numeric)"}' >/dev/null
trigger database::execute "{\"db\":\"primary\",\"sql\":\"INSERT INTO __drizzle_migrations (hash, created_at) VALUES (?, ?)\",\"params\":[\"$H0\",1753257600000]}" >/dev/null
trigger database::execute "{\"db\":\"primary\",\"sql\":\"INSERT INTO __drizzle_migrations (hash, created_at) VALUES (?, ?)\",\"params\":[\"$H1\",1753344000123]}" >/dev/null
echo "drizzle-migrated state simulated (2 applied, 1 on disk only)"

# ---------- 1. adopt ----------
ADOPT=$(trigger migrate::adopt '{"source":"drizzle","from":"./drizzle"}') \
  || { echo "$ADOPT"; fail "migrate::adopt errored"; }
python3 - "$ADOPT" <<'PY'
import json, sys
a = json.loads(sys.argv[1])
assert a["converted"] == [
    "20250723080000_create_users.sql",
    "20250724080000_add_orders.sql",
    "20250725080000_add_scores.sql",
], a
assert a["baselined"] == [
    "20250723080000_create_users.sql",
    "20250724080000_add_orders.sql",
], a
assert a["pending"] == ["20250725080000_add_scores.sql"], a
assert a["skipped"] == 0, a
assert "hash_warnings" not in a, a
print("OK 1: adopt converted 3, baselined 2, left 1 pending")
PY
for f in 20250723080000_create_users.sql 20250724080000_add_orders.sql 20250725080000_add_scores.sql; do
  [[ -f "$RUN_DIR/migrations/$f" ]] || fail "converted file missing: $f"
done
[[ -f "$RUN_DIR/drizzle/0000_create_users.sql" ]] || fail "source drizzle folder was modified"

# ---------- 2. status is coherent ----------
STATUS=$(trigger migrate::status '{}')
python3 - "$STATUS" <<'PY'
import json, sys
s = json.loads(sys.argv[1])
assert [a["name"] for a in s["applied"]] == [
    "20250723080000_create_users.sql",
    "20250724080000_add_orders.sql",
], s
assert [p["name"] for p in s["pending"]] == ["20250725080000_add_scores.sql"], s
assert s["mismatched"] == [] and s["missing"] == [], s
print("OK 2: status -> 2 applied (baselined), 1 pending, no drift")
PY

# ---------- 3. up applies only the pending one ----------
UP=$(trigger migrate::up '{}') || { echo "$UP"; fail "migrate::up errored"; }
python3 - "$UP" <<'PY'
import json, sys
u = json.loads(sys.argv[1])
assert u["applied"] == ["20250725080000_add_scores.sql"], u
assert u["skipped"] == 2, u
print("OK 3: up applied only the pending migration (2 skipped)")
PY
# The baselined DDL was not re-executed (no "table users already exists")
# and the new column exists: the full schema is usable.
trigger database::execute '{"db":"primary","sql":"INSERT INTO users (email, name, score) VALUES (?, ?, ?)","params":["ada@example.com","ada",42]}' >/dev/null \
  || fail "schema not usable after adopt + up"
echo "OK 3b: schema usable (baselined tables + new column)"

# ---------- 4. re-running adopt is a no-op ----------
ADOPT2=$(trigger migrate::adopt '{"source":"drizzle","from":"./drizzle"}') \
  || { echo "$ADOPT2"; fail "second adopt errored"; }
python3 - "$ADOPT2" <<'PY'
import json, sys
a = json.loads(sys.argv[1])
assert a["converted"] == [], a       # files already there, identical
assert a["baselined"] == [], a
assert a["skipped"] == 2, a          # both rows already tracked
print("OK 4: second adopt is a no-op")
PY

# ---------- 5. hash drift surfaces as a warning ----------
trigger database::execute '{"db":"primary","sql":"UPDATE __drizzle_migrations SET hash = ? WHERE created_at = ?","params":["0000000000000000000000000000000000000000000000000000000000000000",1753257600000]}' >/dev/null
ADOPT3=$(trigger migrate::adopt '{"source":"drizzle","from":"./drizzle"}') \
  || { echo "$ADOPT3"; fail "third adopt errored"; }
python3 - "$ADOPT3" <<'PY'
import json, sys
a = json.loads(sys.argv[1])
w = a.get("hash_warnings", [])
assert len(w) == 1 and w[0]["tag"] == "0000_create_users", a
assert w[0]["recorded_hash"].startswith("0000"), a
print("OK 5: drifted drizzle hash reported as hash_warnings (non-fatal)")
PY

# ---------- 6. mysql journal is rejected ----------
mkdir -p "$RUN_DIR/drizzle-mysql/meta"
cat >"$RUN_DIR/drizzle-mysql/meta/_journal.json" <<'EOF'
{"version": "7", "dialect": "mysql", "entries": [{"idx": 0, "when": 1753257600000, "tag": "0000_x"}]}
EOF
set +e
BAD=$(trigger migrate::adopt '{"source":"drizzle","from":"./drizzle-mysql"}')
BAD_RC=$?
set -e
[[ $BAD_RC -ne 0 ]] && echo "$BAD" | grep -q "UNSUPPORTED_DIALECT" \
  || fail "expected UNSUPPORTED_DIALECT for a mysql journal, got: $BAD"
echo "OK 6: mysql journal rejected"

echo "ADOPT PLAYGROUND OK"
