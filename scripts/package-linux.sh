#!/usr/bin/env bash
# Linux packaging: build the release binary and produce
#   target/package/comet-<version>-linux-<arch>.tar.gz
# containing the binary, the .desktop entry, and the icon, plus an install.sh
# that drops them into ~/.local (XDG) paths.
#
# Usage: scripts/package-linux.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/comet-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p comet
  BIN="$ROOT/target/release/comet"
else
  cargo build -p comet
  BIN="$ROOT/target/debug/comet"
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN" "$STAGE/comet"
install -m 644 "$ROOT/dist/comet.desktop" "$STAGE/comet.desktop"
install -m 644 "$ROOT/dist/comet.png" "$STAGE/comet.png"
install -m 644 "$ROOT/dist/comet.svg" "$STAGE/comet.svg"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Comet into ~/.local (no root needed).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install -Dm755 "$HERE/comet" "$HOME/.local/bin/comet"
install -Dm644 "$HERE/comet.desktop" "$HOME/.local/share/applications/comet.desktop"
# `Icon=comet` in the desktop entry is an icon-theme *name*, resolved by lookup.
# The scalable SVG is the one that always resolves: hicolor's index.theme lists
# `scalable`, while 1024x1024 is above the largest size most distributions'
# hicolor ships, so a themer that only walks indexed directories never finds the
# png. Install both — same mark either way — and let lookup pick.
install -Dm644 "$HERE/comet.svg" "$HOME/.local/share/icons/hicolor/scalable/apps/comet.svg"
install -Dm644 "$HERE/comet.png" "$HOME/.local/share/icons/hicolor/1024x1024/apps/comet.png"
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$HOME/.local/share/applications" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 \
  && gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" >/dev/null 2>&1 || true
echo "Installed. Make sure ~/.local/bin is on your PATH."
INSTALL
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
