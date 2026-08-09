#!/usr/bin/env bash
# macOS packaging: build the release binaries for the host arch and produce
#   target/package/comet-<version>-macos-<arch>.dmg          (user download)
#   target/package/comet-<version>-macos-<arch>-app.tar.gz   (auto-updater)
# containing Comet.app.
#
# The bundle carries `comet-board` beside `comet` in Contents/MacOS (gh#156):
# they are the same release and must not be able to drift apart. Nothing on
# macOS puts either on PATH — there is no installer here, only a dmg — so this
# ships the CLI where `comet` can find it (crates/tui's sibling-then-PATH
# lookup) rather than pretending it is installed.
#
# Two signing outcomes, and they are very different for whoever downloads it:
#
#   ad-hoc (default)     `codesign --sign -`. Gatekeeper rejects the app once
#                        it has been downloaded (quarantined) and the dialog is
#                        useless. The dmg ships a README with the one-liner
#                        that unblocks it:
#                          xattr -dr com.apple.quarantine /Applications/Comet.app
#   Developer ID         set CODESIGN_IDENTITY (+ notary credentials below) and
#                        the app is hardened-runtime signed, notarized and
#                        stapled — it just opens. No README step needed.
#
# Usage: scripts/package-macos.sh
# Env:
#   CODESIGN_IDENTITY   "Developer ID Application: … (TEAMID)" — enables the
#                       real signing path. Absent → ad-hoc, unchanged.
#   Notarization (only read when CODESIGN_IDENTITY is set; pick one form):
#     App Store Connect API key —
#       MACOS_NOTARY_KEY_PATH   path to the .p8 private key
#       MACOS_NOTARY_KEY_ID
#       MACOS_NOTARY_ISSUER_ID
#     or Apple ID + app-specific password —
#       MACOS_NOTARY_APPLE_ID
#       MACOS_NOTARY_PASSWORD   app-specific password, not the account password
#       MACOS_NOTARY_TEAM_ID
#   With CODESIGN_IDENTITY set but no notary credentials the script signs and
#   warns: a signed-but-unnotarized app is still Gatekeeper-rejected on
#   download, so the README workaround still applies.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
ARCH="$(uname -m)" # arm64 on Apple silicon runners
OUT_DIR="$ROOT/target/package"
APP="$OUT_DIR/Comet.app"
DMG="$OUT_DIR/comet-$VERSION-macos-$ARCH.dmg"
APP_TARBALL="$OUT_DIR/comet-$VERSION-macos-$ARCH-app.tar.gz"
DMG_ROOT="$OUT_DIR/dmg-root"

cd "$ROOT"
cargo build --release -p comet -p comet-board-bin

rm -rf "$APP" "$DMG" "$APP_TARBALL" "$DMG_ROOT"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m 755 "$ROOT/target/release/comet" "$APP/Contents/MacOS/comet"
install -m 755 "$ROOT/target/release/comet-board" "$APP/Contents/MacOS/comet-board"
sed "s/__VERSION__/$VERSION/" "$ROOT/dist/macos/Info.plist" >"$APP/Contents/Info.plist"

# Asked to prove it, because the Linux payload shipped the engine alone from the
# first release to v0.3.4 and nothing noticed (gh#156). A missing binary is a
# failed build.
for required in comet comet-board; do
  [[ -x "$APP/Contents/MacOS/$required" ]] \
    || { echo "packaging bug: $required is missing from the bundle" >&2; exit 1; }
done

# Icon: iconset from dist/comet.png — the comet mark from the original app
# (apps/desktop/resources/icon.png in the comet repo; source dist/comet.svg).
ICONSET="$OUT_DIR/comet.iconset"
rm -rf "$ICONSET" && mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ROOT/dist/comet.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  retina=$((size * 2))
  sips -z "$retina" "$retina" "$ROOT/dist/comet.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/comet.icns"
rm -rf "$ICONSET"

# --- sign -------------------------------------------------------------------
# NOTARIZED tracks whether the shipped artifacts can survive Gatekeeper on a
# fresh download; it decides whether the dmg carries the workaround README.
NOTARIZED=0
NOTARY_ARGS=()

if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  # Inside-out, and it has to be in that order: since gh#156 the bundle carries
  # a second Mach-O (Contents/MacOS/comet-board), and nested code is not covered
  # by signing the bundle around it — notarization rejects the whole submission
  # over one unsigned helper. Signing the helper first, then the bundle, is what
  # seals it; still no --deep, which Apple discourages for anything destined for
  # notarization. --timestamp and the hardened runtime are both prerequisites.
  codesign --force --options runtime --timestamp \
    --sign "$CODESIGN_IDENTITY" "$APP/Contents/MacOS/comet-board"
  codesign --force --options runtime --timestamp \
    --sign "$CODESIGN_IDENTITY" "$APP"

  if [[ -n "${MACOS_NOTARY_KEY_PATH:-}" ]]; then
    NOTARY_ARGS=(--key "$MACOS_NOTARY_KEY_PATH"
      --key-id "${MACOS_NOTARY_KEY_ID:?MACOS_NOTARY_KEY_ID required with MACOS_NOTARY_KEY_PATH}"
      --issuer "${MACOS_NOTARY_ISSUER_ID:?MACOS_NOTARY_ISSUER_ID required with MACOS_NOTARY_KEY_PATH}")
  elif [[ -n "${MACOS_NOTARY_APPLE_ID:-}" ]]; then
    NOTARY_ARGS=(--apple-id "$MACOS_NOTARY_APPLE_ID"
      --password "${MACOS_NOTARY_PASSWORD:?MACOS_NOTARY_PASSWORD required with MACOS_NOTARY_APPLE_ID}"
      --team-id "${MACOS_NOTARY_TEAM_ID:?MACOS_NOTARY_TEAM_ID required with MACOS_NOTARY_APPLE_ID}")
  fi
else
  # Ad-hoc signature so the app launches when it was built locally. A
  # downloaded copy is quarantined and Gatekeeper rejects it regardless —
  # see the README this script puts in the dmg.
  codesign --deep --force --sign - "$APP"
fi

# notarize <path> — submit, wait for the verdict, fail loudly on rejection.
# Called for the app bundle and again for the finished dmg: stapling the app
# covers the updater tarball, stapling the dmg covers the download itself.
notarize() {
  local target="$1" submit="$1"
  if [[ -d "$target" ]]; then
    submit="$OUT_DIR/notarize-$(basename "$target").zip"
    rm -f "$submit"
    ditto -c -k --keepParent "$target" "$submit"
  fi
  echo "notarizing: $(basename "$target")"
  xcrun notarytool submit "$submit" "${NOTARY_ARGS[@]}" --wait --timeout 30m
  xcrun stapler staple "$target"
  [[ "$submit" == "$target" ]] || rm -f "$submit"
}

if [[ ${#NOTARY_ARGS[@]} -gt 0 ]]; then
  notarize "$APP"
  NOTARIZED=1
elif [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  echo "warning: CODESIGN_IDENTITY set but no notary credentials — the app is" >&2
  echo "         signed but NOT notarized, so Gatekeeper still rejects a" >&2
  echo "         downloaded copy. Shipping the quarantine README." >&2
fi

# The auto-updater artifact: the signed bundle as a plain tarball (the in-app
# updater downloads + extracts it, then swaps /Applications/Comet.app —
# crates/update stage_mac_app/apply_mac_app). Built after stapling so the
# updater lands a bundle with the notarization ticket attached.
tar -czf "$APP_TARBALL" -C "$OUT_DIR" Comet.app
echo "packaged: $APP_TARBALL"

# --- dmg --------------------------------------------------------------------
# Staged in its own directory so the disk image carries the drag-to-install
# Applications symlink and, on unsigned builds, the first-launch README. ditto
# rather than cp: it preserves the code signature and extended attributes.
mkdir -p "$DMG_ROOT"
ditto "$APP" "$DMG_ROOT/Comet.app"
ln -s /Applications "$DMG_ROOT/Applications"
if [[ "$NOTARIZED" -eq 0 ]]; then
  cp "$ROOT/dist/macos/DMG-README.txt" "$DMG_ROOT/READ ME FIRST.txt"
fi

hdiutil create -volname Comet -srcfolder "$DMG_ROOT" -ov -format UDZO "$DMG"
rm -rf "$DMG_ROOT"

if [[ -n "${CODESIGN_IDENTITY:-}" ]]; then
  codesign --force --sign "$CODESIGN_IDENTITY" --timestamp "$DMG"
fi
if [[ "$NOTARIZED" -eq 1 ]]; then
  notarize "$DMG"
fi
echo "packaged: $DMG"

# --- report -----------------------------------------------------------------
# The thing the issue was actually about: say plainly whether a downloaded
# copy of this build will open. `spctl --assess` is the same check Gatekeeper
# runs, so this is the build's own verdict on itself, not a guess.
echo
if spctl --assess --type execute "$APP" >/dev/null 2>&1; then
  echo "gatekeeper: accepted — downloads open with no extra step."
else
  echo "gatekeeper: REJECTED ($(codesign -dv "$APP" 2>&1 | sed -n 's/^Signature=//p'))."
  echo "            A downloaded copy needs, once:"
  echo "              xattr -dr com.apple.quarantine /Applications/Comet.app"
  echo "            Set CODESIGN_IDENTITY + MACOS_NOTARY_* to remove this step."
fi
