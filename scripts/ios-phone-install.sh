#!/usr/bin/env bash
# Build, install, and launch the current checkout on a connected iPhone.
#
# Usage: scripts/ios-phone-install.sh <device name-or-udid> [chat-id]
#
# The optional chat id is passed through the app's existing `-route chat:...`
# launch argument, which makes the gh#450 stale-session check one command after
# the phone has trusted this Mac and Signing.local.xcconfig is configured.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEVICE="${1:-}"
CHAT_ID="${2:-}"

if [[ -z "$DEVICE" ]]; then
  echo "usage: $0 <device name-or-udid> [chat-id]" >&2
  echo "connected devices:" >&2
  xcrun devicectl list devices >&2 || true
  exit 2
fi

if [[ ! -f "$ROOT/apps/ios/Signing.local.xcconfig" ]]; then
  echo "missing apps/ios/Signing.local.xcconfig" >&2
  echo "copy Signing.local.xcconfig.example and set DEVELOPMENT_TEAM and PRODUCT_BUNDLE_IDENTIFIER" >&2
  exit 2
fi

DERIVED="$(mktemp -d /tmp/comet-ios-phone.XXXXXX)"
cleanup() { rm -rf "$DERIVED"; }
trap cleanup EXIT

echo "phone install: building a signed Debug app"
xcodebuild -project "$ROOT/apps/ios/Comet.xcodeproj" -scheme Comet \
  -configuration Debug -destination 'generic/platform=iOS' \
  -derivedDataPath "$DERIVED" build >"$DERIVED/build.log" 2>&1 || {
    echo "FAIL: device build failed" >&2
    grep -E "error:|requires a development team|No profiles for" "$DERIVED/build.log" | head -30 >&2
    exit 1
  }

APP="$DERIVED/Build/Products/Debug-iphoneos/Comet.app"
APP_ID="$(plutil -extract CFBundleIdentifier raw "$APP/Info.plist")"

echo "phone install: installing $APP_ID on $DEVICE"
xcrun devicectl device install app --device "$DEVICE" "$APP"

echo "phone install: launching $APP_ID"
if [[ -n "$CHAT_ID" ]]; then
  xcrun devicectl device process launch --terminate-existing \
    --device "$DEVICE" "$APP_ID" -route "chat:$CHAT_ID"
else
  xcrun devicectl device process launch --terminate-existing \
    --device "$DEVICE" "$APP_ID"
fi

echo "phone install: done"
echo "For live recovery diagnostics, filter the device console for subsystem dev.cometnative.Comet, category sync."
