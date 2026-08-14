#!/usr/bin/env bash
# The stacks rig (gh#337): an isolated board pointed at a scratch repo, for
# exercising the stack handling against real GitHub payloads rather than
# fixtures.
#
# What this does for you:
#   - builds `comet` and `comet-board` from the tree you are standing in
#   - starts a headless engine on its own COMET_IPC_PORT / COMET_DATA_DIR /
#     COMET_WORKTREES_DIR, so it can never attach to the live board
#   - writes a routing.toml polling one repo, with the mock runtime, and
#     onboards that repo (clone + space + route)
#   - prints the env line every `comet-board` verb against this rig needs
#
# What it deliberately leaves to you, because GitHub is the thing under test:
# building the stacks (`gh stack init/add/submit --auto --open`), reviewing,
# and merging. gh#337's write-up in docs/board/ is the log of one full run and
# the order the cases want.
#
# Usage:  scripts/stacks-rig.sh [owner/repo]        # default: the scratch repo
#         scripts/stacks-rig.sh --stop              # stop the rig engine
# Env:    COMET_RIG_DIR   (default /tmp/comet-stacks-rig)
#         COMET_RIG_PORT  (default 27931)
#
# Two prerequisites the rig cannot install for you:
#   gh extension install github/gh-stack
#   a GITHUB_TOKEN in the rig's .env (taken from `gh auth token` if unset)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"

RIG="${COMET_RIG_DIR:-/tmp/comet-stacks-rig}"
PORT="${COMET_RIG_PORT:-27931}"
REPO="${1:-bredebjorhovd/board-scratch}"

DATA="$RIG/data"
BOARD="$DATA/board"
PIDFILE="$RIG/engine.pid"
LOG="$RIG/engine.log"

stop_engine() {
  if [[ -f "$PIDFILE" ]] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
    kill "$(cat "$PIDFILE")" 2>/dev/null || true
    echo "rig engine stopped (pid $(cat "$PIDFILE"))"
  else
    echo "no rig engine running"
  fi
  rm -f "$PIDFILE"
}

if [[ "$REPO" == "--stop" ]]; then
  stop_engine
  exit 0
fi

# The whole point of the rig is that it is not the live board. Refuse the
# default data dir and the default port outright rather than trusting the
# caller to have exported the right thing.
if [[ "$DATA" == "$HOME/.comet-native" || "$DATA" == "$HOME/.comet-native/" ]]; then
  echo "refusing: COMET_RIG_DIR would put the rig on the live board's data dir" >&2
  exit 1
fi

command -v gh >/dev/null 2>&1 || { echo "gh is not installed" >&2; exit 1; }
gh extension list 2>/dev/null | grep -q "gh-stack" || {
  echo "gh-stack is missing — run: gh extension install github/gh-stack" >&2
  exit 1
}

echo "==> building comet + comet-board"
(cd "$ROOT" && cargo build -q -p comet -p comet-board-bin)
COMET="$ROOT/target/debug/comet"
BOARD_CLI="$ROOT/target/debug/comet-board"

mkdir -p "$BOARD/state" "$RIG"

if [[ ! -f "$BOARD/routing.toml" ]]; then
  echo "==> writing routing.toml"
  cat > "$BOARD/routing.toml" <<TOML
# gh#337 stacks rig — an isolated board that polls one scratch repo.
[sync]
interval = "20s"

[defaults]
max_concurrent_per_workspace = 3
branch_template = "board/{identifier_lower}"
base = "origin/HEAD"
# Off while the rig runs: a swept checkout is evidence gone. The GC case turns
# this down to "1s" on purpose, and turns it back after.
retain_worktrees = "off"
retain_build_output = "off"
archive_chats = "off"

[github]
repos = ["$REPO"]
labels = []
pull_requests = true
# Never on for a rig: the board would comment on and close somebody's issues.
writeback = false
deliver_reviews = true
TOML
fi

if [[ ! -s "$BOARD/.env" ]]; then
  echo "==> writing .env from \`gh auth token\`"
  printf 'GITHUB_TOKEN=%s\n' "${GITHUB_TOKEN:-$(gh auth token)}" > "$BOARD/.env"
  chmod 600 "$BOARD/.env"
fi

if [[ -f "$PIDFILE" ]] && kill -0 "$(cat "$PIDFILE")" 2>/dev/null; then
  echo "==> rig engine already up (pid $(cat "$PIDFILE"), port $PORT)"
else
  echo "==> starting the rig engine on :$PORT"
  COMET_DATA_DIR="$DATA" COMET_IPC_PORT="$PORT" COMET_EDGE_URL=off \
    COMET_DEVICE_NAME=stacks-rig COMET_HARNESS=mock \
    COMET_WORKTREES_DIR="$RIG/worktrees" RUST_LOG=info \
    nohup "$COMET" headless > "$LOG" 2>&1 &
  echo $! > "$PIDFILE"
  for _ in $(seq 1 60); do
    if (exec 3<>/dev/tcp/127.0.0.1/"$PORT") 2>/dev/null; then break; fi
    sleep 1
  done
fi

export COMET_DATA_DIR="$DATA" COMET_IPC_PORT="$PORT"

echo "==> onboarding $REPO"
"$BOARD_CLI" onboard "$REPO" --all-issues || true

# `onboard` writes the route with the box's default runtime. A rig dispatches
# to `mock`: the point is the branch, the chat and the attempt, never an agent.
python3 - "$BOARD/routing.toml" <<'PY'
import sys
p = sys.argv[1]
lines = open(p).read().splitlines()
for i, l in enumerate(lines):
    if l.strip().startswith("runtime =") and not l.strip().startswith("#"):
        lines[i] = 'runtime = "mock"'
open(p, "w").write("\n".join(lines) + "\n")
PY
echo "==> route runtime set to mock"

cat <<EOF

the rig is up.

  export COMET_DATA_DIR="$DATA" COMET_IPC_PORT=$PORT
  $BOARD_CLI list
  tail -f $BOARD/state/syncd.log

worktrees   $RIG/worktrees
engine log  $LOG
stop        scripts/stacks-rig.sh --stop

next, on $REPO and NOT from here — the GitHub half is what is under test:
  gh stack init <branch> && gh stack add <branch>-2 && gh stack submit --auto --open
  gh stack link <pr> <pr>          # when the chain was built with --onto
  gh api -X PUT /repos/$REPO/pulls/<n>/merge-async -f merge_method=merge
EOF
