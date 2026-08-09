#!/usr/bin/env bash
# Linux packaging: build the release binaries and produce
#   target/package/comet-<version>-linux-<arch>.tar.gz
# containing both binaries, the .desktop entry, and the icon, plus an install.sh
# that drops them into ~/.local (XDG) paths.
#
# BOTH binaries, since gh#156. The payload used to be the engine alone, so every
# release upgraded `comet` and left whatever `comet-board` a box happened to have
# frozen where it was — on the team box, a source build symlinked by hand in
# August and three weeks of verbs behind the board it was driving. They come off
# the same `cargo build`; shipping one of them was the whole bug.
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
  cargo build --release -p comet -p comet-board-bin
  BIN_DIR="$ROOT/target/release"
else
  cargo build -p comet -p comet-board-bin
  BIN_DIR="$ROOT/target/debug"
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN_DIR/comet" "$STAGE/comet"
install -m 755 "$BIN_DIR/comet-board" "$STAGE/comet-board"
install -m 644 "$ROOT/dist/comet.desktop" "$STAGE/comet.desktop"
install -m 644 "$ROOT/dist/comet.png" "$STAGE/comet.png"
install -m 644 "$ROOT/dist/comet.svg" "$STAGE/comet.svg"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Comet into ~/.local (no root needed).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# What this run put down, and what it refused to touch (see install_bin). Both
# reported at the end, because "installed" is a claim about PATH and not about
# the payload: naming a binary as installed while something else answers to that
# name is the same silence gh#156 was about, one step further along.
installed=()
held=()

# install_bin <name> — copy one binary into ~/.local/bin, unless something this
# installer did not put there is standing in the way.
#
# This installer copies, so a regular file is its own previous run and gets
# overwritten without ceremony. A *symlink* is not ours: it is either the
# curl|sh installer's link into ~/.comet-native/app/current, or — the gh#156
# case — a source build somebody wired up by hand in August and forgot about.
# Writing through it would overwrite the build tree it points at; unlinking it
# would silently reverse a decision a human made. So name what is there, name
# what removes it, and leave it standing.
# What a binary says it is, so the warning below names the version it leaves
# standing rather than gesturing at it. A copy too old to know `--version`
# dates itself by failing: the flag ships with the first release that carries
# comet-board at all.
#
# Under `timeout` where there is one: the binary being run is by definition one
# nobody vouches for, and a copy that hangs instead of answering must not wedge
# the install.
version_of() {
  local v
  if command -v timeout >/dev/null 2>&1; then
    v="$(timeout 5 "$1" --version 2>/dev/null | head -1 | awk '{print $NF}')" || v=""
  else
    v="$("$1" --version 2>/dev/null | head -1 | awk '{print $NF}')" || v=""
  fi
  case "$v" in
    [0-9]*) echo "v$v" ;;
    *) echo 'version unknown — too old to answer `--version`' ;;
  esac
}

install_bin() {
  local name="$1" dest="$HOME/.local/bin/$1"
  if [[ -L "$dest" ]]; then
    held+=("$dest ($(version_of "$dest")) -> $(readlink "$dest")")
    return 0
  fi
  install -Dm755 "$HERE/$name" "$dest"
  installed+=("$name")
}

install_bin comet
install_bin comet-board
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
if [[ ${#installed[@]} -gt 0 ]]; then
  echo "Installed: ${installed[*]}. Make sure ~/.local/bin is on your PATH."
fi

# Loud, and last, so it is the line still on screen: a binary left behind here
# is a binary that stays at whatever version it already was, which is exactly
# the silence gh#156 was about.
if [[ ${#held[@]} -gt 0 ]]; then
  echo
  echo "NOT installed — a symlink is already there, and it is not this installer's:" >&2
  for h in "${held[@]}"; do echo "  $h" >&2; done
  echo "Each stays at the version above. To hand one over, remove it and re-run" >&2
  echo "this script:" >&2
  for h in "${held[@]}"; do echo "  rm -f ${h%% (*}" >&2; done
fi
INSTALL
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"

# The payload shipped the engine alone from the first release to v0.3.4 and
# nothing noticed (gh#156), so the tarball is now asked to prove it carries both
# before anyone uploads it. A missing binary is a failed build, not a quiet
# omission — this check is the only thing standing between here and a repeat.
contents="$(tar -tzf "$TARBALL")"
for required in comet comet-board; do
  grep -qx ".*/$required" <<<"$contents" \
    || { echo "packaging bug: $required is missing from $TARBALL" >&2; exit 1; }
done

echo "packaged: $TARBALL"
printf '%s\n' "$contents"
