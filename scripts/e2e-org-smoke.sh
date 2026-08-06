#!/usr/bin/env bash
# Two-USER e2e smoke (gh#66): real edge (wrangler dev), two headless engines
# signed in as two DIFFERENT WorkOS users of one org, and the comet-rpc
# org_e2e_driver example proving the three org gates:
#
#   1. the teammate's WatchDevices lists the box (org device registry)
#   2. a targetDeviceId RPC from the teammate is answered by the box (relay)
#   3. the teammate opens a chat the box shared, reads it, and queues a turn
#      that the BOX runs — never their own laptop
#
# Its sibling e2e-smoke.sh covers one user on two devices; this one covers the
# case that used to be impossible — a second person in the org.
#
# Usage: scripts/e2e-org-smoke.sh
# Env:   COMET_E2E_EDGE_PORT (default 27640), COMET_E2E_KEEP_LOGS=1 to keep logs.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
EDGE_PORT="${COMET_E2E_EDGE_PORT:-27640}"
EDGE_URL="http://localhost:${EDGE_PORT}"
ORG="org1"
# Dev-mode bearers are `user@org` — the box and a teammate, same org.
BOX_TOKEN="alice@${ORG}"
MATE_TOKEN="bob@${ORG}"
A_PORT=27811
B_PORT=27812
A_DIR=/tmp/e2e-org-box
B_DIR=/tmp/e2e-org-mate
LOG_DIR="$(mktemp -d /tmp/comet-e2e-org-logs.XXXXXX)"

EDGE_PID=""
A_PID=""
B_PID=""
STATUS=1

cleanup() {
  for pid in "$A_PID" "$B_PID"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
    fi
  done
  # The edge runs in its own session (setsid) — kill the whole wrangler group
  # (npx → wrangler → workerd children).
  [[ -n "$EDGE_PID" ]] && kill -- -"$EDGE_PID" 2>/dev/null || true
  sleep 1
  for pid in "$A_PID" "$B_PID"; do
    [[ -n "$pid" ]] && kill -9 "$pid" 2>/dev/null || true
  done
  [[ -n "$EDGE_PID" ]] && kill -9 -- -"$EDGE_PID" 2>/dev/null || true
  rm -rf "$A_DIR" "$B_DIR"
  if [[ "$STATUS" -ne 0 ]]; then
    echo "--- the box's log (tail) ---"; tail -n 40 "$LOG_DIR/engine-box.log" 2>/dev/null || true
    echo "--- the teammate's log (tail) ---"; tail -n 40 "$LOG_DIR/engine-mate.log" 2>/dev/null || true
    echo "--- edge log (tail) ---"; tail -n 40 "$LOG_DIR/edge.log" 2>/dev/null || true
  fi
  if [[ "${COMET_E2E_KEEP_LOGS:-0}" != "1" ]]; then
    rm -rf "$LOG_DIR"
  else
    echo "logs kept in $LOG_DIR"
  fi
}
trap cleanup EXIT

wait_for() { # wait_for <description> <timeout_s> <command...>
  local what="$1" timeout="$2"; shift 2
  local waited=0
  until "$@" >/dev/null 2>&1; do
    sleep 1
    waited=$((waited + 1))
    if [[ "$waited" -ge "$timeout" ]]; then
      echo "FAIL: timed out waiting for $what" >&2
      exit 1
    fi
  done
}

# ── 1. Edge worker (wrangler dev, dev auth: bearer == user@org) ────────────────
if curl -sf -m 3 "$EDGE_URL/health" | grep -q '"auth":"dev"'; then
  echo "edge: reusing healthy dev-mode worker on :$EDGE_PORT"
else
  echo "edge: starting wrangler dev on :$EDGE_PORT"
  setsid bash -c "cd '$ROOT/edge' && exec npx wrangler dev --port '$EDGE_PORT' --var AUTH_MODE:dev" \
    >"$LOG_DIR/edge.log" 2>&1 &
  EDGE_PID=$!
  wait_for "edge /health" 90 curl -sf -m 3 "$EDGE_URL/health"
fi

# ── 2. Build the binaries (workspace target is warm in CI/dev) ─────────────────
echo "build: comet + org_e2e_driver"
(cd "$ROOT" && cargo build -q -p comet -p comet-rpc --example org_e2e_driver)
COMET="$ROOT/target/debug/comet"
DRIVER="$ROOT/target/debug/examples/org_e2e_driver"

# ── 3. Two headless engines: the box and a teammate's laptop, two users ────────
rm -rf "$A_DIR" "$B_DIR"
mkdir -p "$A_DIR" "$B_DIR"

start_engine() { # start_engine <data_dir> <ipc_port> <name> <token> <user> <log>
  COMET_DATA_DIR="$1" COMET_IPC_PORT="$2" COMET_DEVICE_NAME="$3" \
    COMET_EDGE_URL="$EDGE_URL" COMET_EDGE_TOKEN="$4" COMET_ORG_ID="$ORG" \
    COMET_USER_ID="$5" COMET_HARNESS=mock RUST_LOG=info \
    "$COMET" headless >"$6" 2>&1 &
}

start_engine "$A_DIR" "$A_PORT" "e2e-org-box" "$BOX_TOKEN" "alice" \
  "$LOG_DIR/engine-box.log"; A_PID=$!
start_engine "$B_DIR" "$B_PORT" "e2e-org-mate" "$MATE_TOKEN" "bob" \
  "$LOG_DIR/engine-mate.log"; B_PID=$!

wait_for "the box's ipc :$A_PORT" 60 bash -c "exec 3<>/dev/tcp/127.0.0.1/$A_PORT"
wait_for "the teammate's ipc :$B_PORT" 60 bash -c "exec 3<>/dev/tcp/127.0.0.1/$B_PORT"
echo "engines: box pid=$A_PID ipc=:$A_PORT  teammate pid=$B_PID ipc=:$B_PORT"

# ── 4. Drive the three gates through both IPCs ─────────────────────────────────
"$DRIVER" "$A_PORT" "$B_PORT" "$EDGE_URL" "$BOX_TOKEN"
STATUS=$?
exit "$STATUS"
