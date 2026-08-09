#!/usr/bin/env bash
# The Swift half of the cross-language stats fixture (gh#157).
#
# `apps/ios/Comet/Board/StatsModels.swift` is a second implementation of the
# rules in `comet_proto::view::stats` — no Rust runs on that device — and two
# implementations of one rule is how a phone comes to disagree with a laptop
# about a number somebody is deciding on. So the cases live outside both:
#
#   cargo test -p comet-proto stats      the Rust half, and the guard that the
#                                        checked-in fixture still matches it
#   scripts/ios-stats-spec.sh            this: the Swift half against the same
#                                        file, in the simulator
#
# Whichever side moves is the side that fails. After changing a rule in the
# Rust, regenerate with `UPDATE_STATS_SPEC=1 cargo test -p comet-proto stats`
# and run this to find out what the Swift now disagrees about.
#
# Usage: scripts/ios-stats-spec.sh [simulator name]
# Env:   COMET_SPEC_SIM (default "iPhone 17 Pro")

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SIM="${1:-${COMET_SPEC_SIM:-iPhone 17 Pro}}"
APP_ID="dev.cometnative.Comet"
DERIVED="$(mktemp -d /tmp/comet-ios-spec.XXXXXX)"

cleanup() { rm -rf "$DERIVED"; }
trap cleanup EXIT

echo "spec: building for $SIM"
xcodebuild -project "$ROOT/apps/ios/Comet.xcodeproj" -scheme Comet \
  -destination "platform=iOS Simulator,name=$SIM" \
  -derivedDataPath "$DERIVED" build >"$DERIVED/build.log" 2>&1 || {
    echo "FAIL: build failed" >&2
    grep -E "error:" "$DERIVED/build.log" | head -20 >&2
    exit 1
  }

echo "spec: booting $SIM"
xcrun simctl boot "$SIM" >/dev/null 2>&1 || true
xcrun simctl bootstatus "$SIM" -b >/dev/null 2>&1 || true

xcrun simctl install "$SIM" "$DERIVED/Build/Products/Debug-iphonesimulator/Comet.app"
xcrun simctl terminate "$SIM" "$APP_ID" >/dev/null 2>&1 || true

# Remove the previous run's log BEFORE launching, not after: the container
# outlives an install, and polling for a file that is already there reads the
# last run's verdict — which is a green light that means nothing.
LOG="$(xcrun simctl get_app_container "$SIM" "$APP_ID" data)/Documents/spec.log"
rm -f "$LOG"
xcrun simctl launch "$SIM" "$APP_ID" -spec >/dev/null

# The runner is arithmetic against a bundled file — no network, no session —
# so it is done long before this poll gives up.
for _ in $(seq 1 30); do
  if [[ -f "$LOG" ]] && grep -q "^done$" "$LOG" 2>/dev/null; then break; fi
  sleep 1
done

if [[ ! -f "$LOG" ]]; then
  echo "FAIL: the runner wrote no log — did the app launch?" >&2
  exit 1
fi

cat "$LOG"
xcrun simctl terminate "$SIM" "$APP_ID" >/dev/null 2>&1 || true
grep -q "^OK " "$LOG"
