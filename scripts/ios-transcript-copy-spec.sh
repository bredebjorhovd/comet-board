#!/usr/bin/env bash
# Focused Swift simulator check for complete-message copy formatting (gh#459).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SIM="${1:-${COMET_SPEC_SIM:-iPhone 17 Pro}}"
DERIVED="$(mktemp -d /tmp/comet-ios-transcript-copy-spec.XXXXXX)"

cleanup() { rm -rf "$DERIVED"; }
trap cleanup EXIT

echo "transcript-copy-spec: building for $SIM"
xcodebuild -project "$ROOT/apps/ios/Comet.xcodeproj" -scheme Comet \
  -destination "platform=iOS Simulator,name=$SIM" \
  -derivedDataPath "$DERIVED" build >"$DERIVED/build.log" 2>&1 || {
    echo "FAIL: build failed" >&2
    grep -E "error:" "$DERIVED/build.log" | head -20 >&2
    exit 1
  }

APP="$DERIVED/Build/Products/Debug-iphonesimulator/Comet.app"
APP_ID="$(plutil -extract CFBundleIdentifier raw "$APP/Info.plist")"

xcrun simctl boot "$SIM" >/dev/null 2>&1 || true
xcrun simctl bootstatus "$SIM" -b >/dev/null 2>&1 || true
xcrun simctl install "$SIM" "$APP"
xcrun simctl terminate "$SIM" "$APP_ID" >/dev/null 2>&1 || true

LOG="$(xcrun simctl get_app_container "$SIM" "$APP_ID" data)/Documents/transcript-copy-spec.log"
rm -f "$LOG"
xcrun simctl launch "$SIM" "$APP_ID" -transcript-copy-spec >/dev/null

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
