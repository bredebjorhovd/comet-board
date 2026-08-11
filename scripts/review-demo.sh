#!/usr/bin/env bash
# Open the review window on a seeded attempt, offline (gh#276).
#
#   scripts/review-demo.sh              # dark
#   scripts/review-demo.sh light        # light
#
# What it takes to photograph this surface, and why each piece is here:
#
#   * `seed_review` writes the board AND the attempt's checkout — the review is
#     assembled from git, so a board row alone draws an empty card.
#   * a headless daemon hosts that board, and the app attaches to it over IPC
#     with its own data dir, so one sqlite file has one writer (dev-demo.sh's
#     shape).
#   * the chat the attempt names is created under the fixture's fixed id, so the
#     column beside the review is a session and not "the session is gone".
#   * `COMET_OPEN_REVIEW=1` opens the route on the first board frame — `r` on a
#     row is a keypress a capture script cannot rely on.
#
# Everything lives under /tmp/comet-review-*; re-runs reuse it, Ctrl-C cleans up.
# Bash 3.2 safe: macOS is the machine this is judged on.
set -euo pipefail
cd "$(dirname "$0")/.."

THEME="${1:-dark}"
BOARD_DIR=/tmp/comet-review-board
UI_DIR=/tmp/comet-review-ui
IPC=27846
# The attempt's `pane_id` in `seed_review.rs`. The two halves of a review — the
# board's attempt and the engine's chat — are joined by this constant.
CHAT_ID=5b7f1a30-1b2c-4c4e-9a8f-2f0f2f2d1c00

echo "▸ building (first run takes a few minutes)…"
cargo build -p comet -q

if [[ ! -f "$BOARD_DIR/.seeded" ]]; then
  echo "▸ seeding the board + the attempt's checkout"
  rm -rf "$BOARD_DIR"
  COMET_DATA_DIR="$BOARD_DIR" cargo run -q -p comet-board --example seed_review
  touch "$BOARD_DIR/.seeded"
fi

echo "▸ starting engine daemon on :$IPC"
env COMET_EDGE_URL=off COMET_DATA_DIR="$BOARD_DIR" COMET_IPC_PORT=$IPC \
  COMET_HARNESS=mock COMET_DEVICE_NAME=the-box RUST_LOG=warn \
  ./target/debug/comet headless &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null || true' EXIT
for _ in $(seq 1 40); do
  (exec 3<>/dev/tcp/127.0.0.1/$IPC) 2>/dev/null && { exec 3>&-; break; }
  sleep 0.25
done

probe() { cargo run -q -p comet-rpc --example rpc_probe -- "ws://127.0.0.1:$IPC" "$@"; }

if [[ ! -f "$BOARD_DIR/.chatted" ]]; then
  echo "▸ creating the authoring chat"
  DEV=$(probe LocalDevice '{}' | python3 -c 'import json,sys;print(json.load(sys.stdin)["deviceId"])')
  SPACE=$(uuidgen | tr 'A-Z' 'a-z')
  probe Mutate "{\"op\":\"createSpace\",\"spaceId\":\"$SPACE\",\"deviceId\":\"$DEV\",\"path\":\"$BOARD_DIR/checkout\"}" >/dev/null
  probe Mutate "{\"op\":\"createChat\",\"chatId\":\"$CHAT_ID\",\"spaceId\":\"$SPACE\",\"config\":{\"harness\":\"mock\",\"model\":\"fable-5\",\"reasoning\":null,\"sandbox\":\"workspace-write\"}}" >/dev/null
  probe Mutate "{\"op\":\"renameChat\",\"chatId\":\"$CHAT_ID\",\"title\":\"Active owns a chat's row\"}" >/dev/null
  probe Mutate "{\"op\":\"setChatBranch\",\"chatId\":\"$CHAT_ID\",\"branch\":\"board/gh-138\"}" >/dev/null
  probe QueueCommand "{\"chatId\":\"$CHAT_ID\",\"command\":{\"kind\":\"run\",\"messageId\":\"$(uuidgen)\",\"request\":{\"prompt\":\"Both lists showed the same chat when it went idle mid-frame. Active should own the row while the session is live.\",\"model\":null,\"reasoning\":null,\"modelOptions\":{},\"cwd\":\"$BOARD_DIR/checkout\",\"sandbox\":\"workspace-write\",\"autoApprove\":true,\"resume\":null}}}" >/dev/null
  sleep 2
  touch "$BOARD_DIR/.chatted"
fi

echo "▸ opening the review window ($THEME)"
COMET_EDGE_URL=off COMET_DATA_DIR="$UI_DIR" COMET_IPC_PORT=$IPC \
  COMET_OPEN_REVIEW=1 COMET_THEME="$THEME" RUST_LOG=warn \
  ./target/debug/comet
