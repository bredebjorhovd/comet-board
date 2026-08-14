#!/usr/bin/env bash
# The phone's redial schedule, asserted in the simulator (gh#405).
#
# `apps/ios/Comet/Sync/ReconnectBackoff.swift` is the port of the backoff ladder
# in `crates/sync/src/room.rs` — the healthy-session gate on the reset, and the
# jitter on every wait, that gh#396 put there. This builds the app and runs
# `SyncSpecRunner` (`-sync-spec`) against it: no network, no session, no edge.
#
# That last part is deliberate. A reconnect loop is precisely the thing that
# cannot be checked against an edge that is failing every request, which is the
# state this account's edge is in whenever the Durable Objects free-tier
# duration cap is tripped — the outage the schedule exists for.
#
# The other half of the check needs no simulator and runs in CI:
#
#   cargo test -p comet-sync --test ios_room   the constants and the shape, read
#                                              out of both sources as text
#   scripts/ios-sync-spec.sh                   this: the schedule those
#                                              constants produce
#
# Usage: scripts/ios-sync-spec.sh [simulator name]
# Env:   COMET_SPEC_SIM (default "iPhone 17 Pro")

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SIM="${1:-${COMET_SPEC_SIM:-iPhone 17 Pro}}"
DERIVED="$(mktemp -d /tmp/comet-ios-sync-spec.XXXXXX)"

cleanup() { rm -rf "$DERIVED"; }
trap cleanup EXIT

echo "sync spec: building for $SIM"
xcodebuild -project "$ROOT/apps/ios/Comet.xcodeproj" -scheme Comet \
  -destination "platform=iOS Simulator,name=$SIM" \
  -derivedDataPath "$DERIVED" build >"$DERIVED/build.log" 2>&1 || {
    echo "FAIL: build failed" >&2
    grep -E "error:" "$DERIVED/build.log" | head -20 >&2
    exit 1
  }

APP="$DERIVED/Build/Products/Debug-iphonesimulator/Comet.app"

# Read the bundle id off the app that was just built rather than hardcoding it:
# apps/ios/Signing.local.xcconfig (gh#196) overrides it on any machine set up
# for device builds, and it applies to the simulator too.
APP_ID="$(plutil -extract CFBundleIdentifier raw "$APP/Info.plist")"

echo "sync spec: booting $SIM ($APP_ID)"
xcrun simctl boot "$SIM" >/dev/null 2>&1 || true
xcrun simctl bootstatus "$SIM" -b >/dev/null 2>&1 || true

xcrun simctl install "$SIM" "$APP"
xcrun simctl terminate "$SIM" "$APP_ID" >/dev/null 2>&1 || true

# Remove the previous run's log BEFORE launching: the container outlives an
# install, so polling for a file that is already there reads the last run's
# verdict — a green light that means nothing.
LOG="$(xcrun simctl get_app_container "$SIM" "$APP_ID" data)/Documents/sync-spec.log"
rm -f "$LOG"
xcrun simctl launch "$SIM" "$APP_ID" -sync-spec >/dev/null

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
