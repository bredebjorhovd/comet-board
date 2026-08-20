#!/usr/bin/env bash
# What the phone puts on the clipboard and into the share sheet (gh#534).
#
# `apps/ios/Comet/Transcript/TranscriptText.swift` rebuilds markdown out of the
# parsed block model, because the phone never keeps the source: a transcript
# row is a `MDBlock`, and a `Text` cannot be asked what it is showing. A
# serializer like that fails silently — a fence that does not outlast the code
# inside it, a list item whose second paragraph escapes it, an error string that
# comes back wearing a label the reader never saw — and every one of those
# failures looks fine on screen and only shows up in the paste.
#
# This builds the app, launches it with `-copy-spec`, and reads the runner's
# verdict off the simulator container.
#
#   scripts/ios-copy-spec.sh          the Swift half, in the simulator
#
# Unlike ios-stats-spec.sh / ios-review-spec.sh there is no Rust half to
# disagree with: the desktop copies rendered glyphs out of its own selection
# model (crates/ui/src/markdown/selection.rs) and never serializes a block tree.
# The cases in CopySpecRunner ARE the specification.
#
# Not in CI — it needs a simulator, same as its siblings. The desktop half of
# gh#534 is covered by `cargo test -p comet-ui`.
#
# Usage: scripts/ios-copy-spec.sh [simulator name]
# Env:   COMET_SPEC_SIM (default "iPhone 17 Pro")

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SIM="${1:-${COMET_SPEC_SIM:-iPhone 17 Pro}}"
DERIVED="$(mktemp -d /tmp/comet-ios-copy-spec.XXXXXX)"

cleanup() { rm -rf "$DERIVED"; }
trap cleanup EXIT

echo "copy-spec: building for $SIM"
xcodebuild -project "$ROOT/apps/ios/Comet.xcodeproj" -scheme Comet \
  -destination "platform=iOS Simulator,name=$SIM" \
  -derivedDataPath "$DERIVED" build >"$DERIVED/build.log" 2>&1 || {
    echo "FAIL: build failed" >&2
    grep -E "error:" "$DERIVED/build.log" | head -20 >&2
    exit 1
  }

APP="$DERIVED/Build/Products/Debug-iphonesimulator/Comet.app"

# Read the bundle id off the app that was just built rather than hardcoding it:
# apps/ios/Signing.local.xcconfig (gh#196) overrides PRODUCT_BUNDLE_IDENTIFIER
# and applies to the simulator too.
APP_ID="$(plutil -extract CFBundleIdentifier raw "$APP/Info.plist")"

echo "copy-spec: booting $SIM ($APP_ID)"
xcrun simctl boot "$SIM" >/dev/null 2>&1 || true
xcrun simctl bootstatus "$SIM" -b >/dev/null 2>&1 || true

xcrun simctl install "$SIM" "$APP"
xcrun simctl terminate "$SIM" "$APP_ID" >/dev/null 2>&1 || true

# Remove the previous run's log BEFORE launching, not after: the container
# outlives an install, and polling for a file that is already there reads the
# last run's verdict — a green light that means nothing.
LOG="$(xcrun simctl get_app_container "$SIM" "$APP_ID" data)/Documents/copy-spec.log"
rm -f "$LOG"
xcrun simctl launch "$SIM" "$APP_ID" -copy-spec >/dev/null

# Pure string building — it is done long before this poll gives up.
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
