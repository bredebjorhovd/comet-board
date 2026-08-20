#!/usr/bin/env bash
# The Swift half of the cross-language attachment-transport fixture (gh#535).
#
# `apps/ios/Comet/Composer/Attachments.swift` is a second implementation of the
# trailer rules in `comet_proto::view::attachments` — no Rust runs on that
# device — and two implementations of "where does the prompt end and the
# machine trailer begin" is how one surface comes to show somebody their own
# message with the trailer in it. So the cases live outside both:
#
#   cargo test -p comet-proto attachments   the Rust half, and the guard that
#                                           the checked-in fixture matches it
#   scripts/ios-attachments-spec.sh         this: the Swift half against the
#                                           same file, in the simulator
#
# Whichever side moves is the side that fails. After changing a rule in the
# Rust, regenerate with `UPDATE_ATTACHMENTS_SPEC=1 cargo test -p comet-proto
# attachments` and run this to find out what the Swift now disagrees about.
#
# Only the cargo half runs in CI — this one needs a simulator — so regenerating
# the fixture without running this turns the build green while leaving the phone
# wrong about the rule that just changed. Nothing will catch that but a person.
#
# Usage: scripts/ios-attachments-spec.sh [simulator name]
# Env:   COMET_SPEC_SIM (default "iPhone 17 Pro")

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SIM="${1:-${COMET_SPEC_SIM:-iPhone 17 Pro}}"
DERIVED="$(mktemp -d /tmp/comet-ios-attach-spec.XXXXXX)"

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

APP="$DERIVED/Build/Products/Debug-iphonesimulator/Comet.app"

# Read the bundle id off the app that was just built rather than hardcoding it
# (apps/ios/Signing.local.xcconfig overrides it on any machine set up for
# device builds, and that override applies to the simulator too).
APP_ID="$(plutil -extract CFBundleIdentifier raw "$APP/Info.plist")"

echo "spec: booting $SIM ($APP_ID)"
xcrun simctl boot "$SIM" >/dev/null 2>&1 || true
xcrun simctl bootstatus "$SIM" -b >/dev/null 2>&1 || true

xcrun simctl install "$SIM" "$APP"
xcrun simctl terminate "$SIM" "$APP_ID" >/dev/null 2>&1 || true

# Remove the previous run's log BEFORE launching: the container outlives an
# install, and polling for a file that is already there reads the last run's
# verdict — a green light that means nothing.
LOG="$(xcrun simctl get_app_container "$SIM" "$APP_ID" data)/Documents/spec.log"
rm -f "$LOG"
xcrun simctl launch "$SIM" "$APP_ID" -attachments-spec >/dev/null

# String work against a bundled file — no network, no session — so it is done
# long before this poll gives up.
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
