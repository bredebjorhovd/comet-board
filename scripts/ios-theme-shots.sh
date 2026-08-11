#!/usr/bin/env bash
# Every screen the canvas draws, in both variants, at the phone's own size.
#
# The canvas (`docs/design/canvas/comet-ios.dc.html`) draws three: Home, Board
# and the review sheet. This photographs all three in dark and in light, which
# is six screenshots, one command, no Xcode window.
#
# The half a text scan cannot check is what the paint LOOKS like:
# `crates/ui/tests/ios_theme.rs` proves the ported table equals
# `docs/design/tokens.md` and that no view mixes its own colour; these prove
# somebody looked at the result. The claims they are read against are
# `docs/design/ios.md` — and the ones marked [manual] there are not in these
# files, because `simctl` has no touch input and cannot hold a row down.
#
# The variant is passed as a launch arg (`-theme light`) rather than by flipping
# the simulator's own appearance, for a reason worth knowing: `Info.plist` still
# carries `UIUserInterfaceStyle = Dark`, which forces every window in the app
# and beats the device setting. The explicit `-theme` choice installs a
# window-level override after attachment, which still wins. So
# `xcrun simctl ui <sim> appearance light` would produce four DARK screenshots
# and look like a bug in the theme. See `Comet/Theme/Appearance.swift`.
#
# The six land in docs/screenshots/ by default, where the desktop's live —
# they are the evidence for the PR, so they are meant to be committed. Captures
# stage under /tmp and are copied there only after all six prove they are the
# required 1179x2556 pixels (393x852pt at 3x).
#
# If the named simulator does not exist, create it — the size is the contract,
# not the name, and a newer iPhone is a DIFFERENT canvas:
#
#   xcrun simctl create 'iPhone 15 Pro' \
#     com.apple.CoreSimulator.SimDeviceType.iPhone-15-Pro \
#     "$(xcrun simctl list runtimes | awk '/iOS/ { print $NF; exit }')"
#
# Usage: scripts/ios-theme-shots.sh [outdir] [simulator name]
# Env:   COMET_SPEC_SIM (default "iPhone 15 Pro" — 393x852pt)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/docs/screenshots}"
SIM="${2:-${COMET_SPEC_SIM:-iPhone 15 Pro}}"
DERIVED="$(mktemp -d /tmp/comet-ios-shots.XXXXXX)"
STAGED="$DERIVED/screenshots"
EXPECTED_WIDTH=1179
EXPECTED_HEIGHT=2556

cleanup() { rm -rf "$DERIVED"; }
trap cleanup EXIT

mkdir -p "$OUT"
mkdir -p "$STAGED"
# `simctl io screenshot` resolves its destination outside this shell's cwd;
# normalize a caller-provided relative directory before handing it across.
OUT="$(cd "$OUT" && pwd)"

assert_required_size() {
  local file="$1" label="$2" width height
  width="$(sips -g pixelWidth "$file" | awk '/pixelWidth/ { print $2 }')"
  height="$(sips -g pixelHeight "$file" | awk '/pixelHeight/ { print $2 }')"
  if [[ "$width" != "$EXPECTED_WIDTH" || "$height" != "$EXPECTED_HEIGHT" ]]; then
    echo "FAIL: $label is ${width}x${height}; required evidence is ${EXPECTED_WIDTH}x${EXPECTED_HEIGHT} (393x852pt at 3x)" >&2
    echo "      existing files in $OUT were not changed" >&2
    exit 1
  fi
}

# Reject the wrong device before a build or any committed screenshot can be
# touched. Names are user-configurable; dimensions are the actual contract.
echo "shots: checking $SIM is 393x852pt"
xcrun simctl boot "$SIM" >/dev/null 2>&1 || true
xcrun simctl bootstatus "$SIM" -b >/dev/null 2>&1
xcrun simctl io "$SIM" screenshot --type=png "$DERIVED/size-probe.png" >/dev/null 2>&1
assert_required_size "$DERIVED/size-probe.png" "$SIM"

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

echo "shots: installing on $SIM ($APP_ID)"
# `simctl install` exits 4 when this bundle is still running (for example after
# a previously interrupted capture), so make reruns self-healing.
xcrun simctl terminate "$SIM" "$APP_ID" >/dev/null 2>&1 || true
sleep 1
xcrun simctl install "$SIM" "$APP"

# Demo mode: the offline dataset, so the four frames show the same rows every
# time and nothing depends on what a live board happens to be doing.
shot() {
  local name="$1" theme="$2"
  shift 2
  # Make the handoff atomic. A separate terminate followed immediately by
  # launch occasionally leaves SpringBoard foregrounded long enough for the
  # capture, producing a valid PNG of the Home screen instead of the app.
  xcrun simctl launch --terminate-running-process \
    "$SIM" "$APP_ID" -demo -theme "$theme" "$@" >/dev/null
  sleep 4
  local file="$STAGED/ios-$name-$theme.png"
  xcrun simctl io "$SIM" screenshot --type=png "$file" >/dev/null 2>&1
  assert_required_size "$file" "$name/$theme"
}

for theme in dark light; do
  shot home "$theme"
  shot board "$theme" -route board
  # The third screen the canvas draws. `-sheet review` opens the first
  # reviewable row's panel, preferring one parked in `review` — the state the
  # canvas draws — over any other ended attempt.
  shot review "$theme" -route board -sheet review
done

xcrun simctl terminate "$SIM" "$APP_ID" >/dev/null 2>&1 || true
for file in "$STAGED"/*.png; do
  cp "$file" "$OUT/$(basename "$file")"
  echo "  $OUT/$(basename "$file")"
done
echo "shots: done"
