#!/usr/bin/env bash
# Home and Board, in both variants, at the phone's own size (gh#257).
#
# The theme has two variants now, and the half a text scan cannot check is what
# the paint LOOKS like: `crates/ui/tests/ios_theme.rs` proves the numbers still
# match the desktop's, and this proves somebody looked at the result. Four
# screenshots, one command, no Xcode window.
#
# The variant is passed as a launch arg (`-theme light`) rather than by flipping
# the simulator's own appearance, for a reason worth knowing: `Info.plist` still
# carries `UIUserInterfaceStyle = Dark`, which forces every window in the app
# and beats the device setting. A window-level override — which is exactly what
# `preferredColorScheme` installs, and what `-theme` drives — still wins. So
# `xcrun simctl ui <sim> appearance light` would produce four DARK screenshots
# and look like a bug in the theme. See `Comet/Theme/Appearance.swift`.
#
# The four land in docs/screenshots/ by default, where the desktop's live —
# they are the evidence for the PR, so they are meant to be committed.
#
# Usage: scripts/ios-theme-shots.sh [outdir] [simulator name]
# Env:   COMET_SPEC_SIM (default "iPhone 17 Pro" — 393x852pt)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/docs/screenshots}"
SIM="${2:-${COMET_SPEC_SIM:-iPhone 17 Pro}}"
DERIVED="$(mktemp -d /tmp/comet-ios-shots.XXXXXX)"

cleanup() { rm -rf "$DERIVED"; }
trap cleanup EXIT

mkdir -p "$OUT"

echo "shots: building for $SIM"
xcodebuild -project "$ROOT/apps/ios/Comet.xcodeproj" -scheme Comet \
  -destination "platform=iOS Simulator,name=$SIM" \
  -derivedDataPath "$DERIVED" build >"$DERIVED/build.log" 2>&1 || {
    echo "FAIL: build failed" >&2
    grep -E "error:" "$DERIVED/build.log" | head -20 >&2
    exit 1
  }

APP="$DERIVED/Build/Products/Debug-iphonesimulator/Comet.app"
# Read the id off the app just built — apps/ios/Signing.local.xcconfig (gh#196)
# overrides it, and it applies to simulator builds too.
APP_ID="$(plutil -extract CFBundleIdentifier raw "$APP/Info.plist")"

echo "shots: booting $SIM ($APP_ID)"
xcrun simctl boot "$SIM" >/dev/null 2>&1 || true
xcrun simctl bootstatus "$SIM" -b >/dev/null 2>&1 || true
xcrun simctl install "$SIM" "$APP"

# Demo mode: the offline dataset, so the four frames show the same rows every
# time and nothing depends on what a live board happens to be doing.
shot() {
  local name="$1" theme="$2"
  shift 2
  xcrun simctl terminate "$SIM" "$APP_ID" >/dev/null 2>&1 || true
  xcrun simctl launch "$SIM" "$APP_ID" -demo -theme "$theme" "$@" >/dev/null
  sleep 4
  xcrun simctl io "$SIM" screenshot --type=png "$OUT/ios-$name-$theme.png" >/dev/null 2>&1
  echo "  $OUT/ios-$name-$theme.png"
}

for theme in dark light; do
  shot home "$theme"
  shot board "$theme" -route board
done

xcrun simctl terminate "$SIM" "$APP_ID" >/dev/null 2>&1 || true
echo "shots: done"
