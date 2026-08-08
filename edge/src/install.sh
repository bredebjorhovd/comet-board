#!/bin/sh
# Comet (native) headless installer.
#
#   curl -fsSL https://edge.comet.offhand.dev/install.sh | sh
#
# Installs the self-contained native binaries (no runtime deps) to
# ~/.comet-native/app, puts `comet` and `comet-board` on PATH, and — once
# you've signed in — runs the engine as a systemd user service that survives
# reboots. Re-running upgrades in place; ~/.comet-native state is preserved.
#
# Both binaries since gh#156. Before that this script linked only `comet`, so a
# box kept whatever `comet-board` it had been given once and every release
# widened the gap in silence — the team box was driving 17 routes with a CLI
# three weeks behind the engine, missing verbs the board assumed it had.
#
# The binary ships with production endpoints baked in: no COMET_EDGE_URL or
# client-id configuration needed. Overrides (if any) go in ~/.comet-native/env.
set -eu

BASE="${COMET_BASE_URL:-https://edge.comet.offhand.dev}"

# --- platform ---------------------------------------------------------------
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) plat=linux ;;
  Darwin)
    echo "comet install: on macOS, download the desktop app instead:" >&2
    echo "  $BASE/releases/latest.txt → $BASE/releases/comet-<version>-macos-arm64.dmg" >&2
    echo >&2
    echo "After dragging Comet.app to Applications, run this once — the build" >&2
    echo "is not Developer ID signed, so macOS blocks it until you clear the" >&2
    echo "download quarantine:" >&2
    echo "  xattr -dr com.apple.quarantine /Applications/Comet.app" >&2
    exit 1
    ;;
  *)
    echo "comet install: unsupported OS '$os' — only Linux for now." >&2
    exit 1
    ;;
esac
case "$arch" in
  x86_64 | amd64) arch=x86_64 ;;
  aarch64 | arm64) arch=aarch64 ;;
  *)
    echo "comet install: unsupported architecture '$arch'." >&2
    exit 1
    ;;
esac

# --- download ----------------------------------------------------------------
ver="$(curl -fsSL "$BASE/releases/latest.txt" | tr -d '[:space:]')"
[ -n "$ver" ] || { echo "comet install: could not resolve latest version" >&2; exit 1; }
file="comet-$ver-$plat-$arch.tar.gz"
data_root="$HOME/.comet-native"
app_root="$data_root/app"
dest="$app_root/$ver"

if [ -x "$dest/comet" ]; then
  echo "comet $ver already downloaded — relinking."
else
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  echo "downloading comet $ver ($plat-$arch)…"
  curl -fSL --progress-bar "$BASE/releases/$file" -o "$tmp/$file"
  mkdir -p "$dest"
  tar -xzf "$tmp/$file" -C "$dest" --strip-components=1
fi

ln -sfn "$dest" "$app_root/current"
mkdir -p "$HOME/.local/bin"

# --- PATH links ---------------------------------------------------------------
# Links point at `current`, not at "$dest": a later `comet update` flips that one
# symlink and both binaries follow it. That is also why an unmanaged binary
# standing in the way matters so much — it is the one thing the flip cannot move.
#
# link_bin <name>: ours to move only when ~/.local/bin/<name> is missing, or is
# already a symlink into ~/.comet-native/app. Anything else — a regular file, or
# a symlink into somebody's source checkout — a human put there, and it sits
# ahead of us on PATH. Replacing it silently is how a hand-built binary vanishes
# with nobody deciding it should, so this reports it and moves on. `comet-board
# doctor` says the same thing from the other end, whenever it is next run.
linked=""    # names this run actually put on PATH
held=""      # "<bin> -> <target>" lines for the ones it would not touch
held_bins="" # the same paths alone, for the rm hints
link_bin() {
  name="$1"
  src="$app_root/current/$name"
  bin="$HOME/.local/bin/$name"
  # A release older than gh#156 shipped the engine alone. Linking to a payload
  # path that does not exist would be worse than shipping nothing.
  [ -e "$src" ] || return 0
  if [ -e "$bin" ] || [ -L "$bin" ]; then
    points_at="$(readlink "$bin" 2>/dev/null || echo '<a file, not a symlink>')"
    case "$points_at" in
      "$app_root"/*) ;; # a previous run of this script — relink below
      *)
        held="$held  $bin -> $points_at
"
        held_bins="$held_bins  rm -f $bin
"
        return 0
        ;;
    esac
  fi
  ln -sfn "$src" "$bin"
  linked="$linked $name"
}

link_bin comet
link_bin comet-board

# --- service -----------------------------------------------------------------
# Auth is decoupled from the daemon: `comet login` persists the session and a
# service-managed `comet headless` loads it (exiting with "run comet login
# first" otherwise) — so the service starts only after first sign-in.
signed_in=no
[ -f "$data_root/session.json" ] && signed_in=yes

service=manual
if command -v systemctl >/dev/null 2>&1 && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  mkdir -p "$HOME/.config/systemd/user"
  cat >"$HOME/.config/systemd/user/comet-native.service" <<'UNIT'
[Unit]
Description=Comet native headless engine
After=network-online.target
StartLimitIntervalSec=60
StartLimitBurst=5

[Service]
ExecStart=%h/.comet-native/app/current/comet headless
Restart=on-failure
RestartSec=5
EnvironmentFile=-%h/.comet-native/env

[Install]
WantedBy=default.target
UNIT
  systemctl --user daemon-reload
  systemctl --user enable comet-native >/dev/null 2>&1 || true
  if [ "$signed_in" = yes ]; then
    systemctl --user restart comet-native
    service=running
  else
    service=ready
  fi
  # Keep the user manager (and the engine) running without an active login.
  loginctl enable-linger "$USER" 2>/dev/null \
    || sudo -n loginctl enable-linger "$USER" 2>/dev/null \
    || echo "warn: could not enable linger — the engine stops when you log out (run: sudo loginctl enable-linger $USER)"
else
  echo "warn: systemd user session not available — run the engine manually with: comet headless"
fi

# --- agent CLIs ---------------------------------------------------------------
command -v claude >/dev/null 2>&1 || \
  echo "note: Claude Code CLI not found — install it with: curl -fsSL https://claude.ai/install.sh | bash"

case ":$PATH:" in
  *":$HOME/.local/bin:"*) path_hint="" ;;
  *) path_hint=' (add ~/.local/bin to your PATH)' ;;
esac

echo ""
# Names what this run actually linked, rather than what the payload contained:
# a binary reported as installed when something else is answering to that name
# on PATH is the same lie in a new place.
[ -n "$linked" ] && echo "✓ $ver installed:$linked$path_hint"
echo ""

# And, on stderr, what it would not touch — because a link left standing is the
# whole failure mode this release exists to stop being silent about: that binary
# keeps whatever version it already was while the engine moves on without it.
if [ -n "$held" ]; then
  {
    echo "warn: left in place — already there, and not this installer's:"
    printf '%s' "$held"
    echo "      each keeps the version it already had, while this box is now on $ver."
    echo "      to hand one over, remove it and re-run this installer:"
    printf '%s' "$held_bins"
    echo ""
  } >&2
fi

case "$service" in
  running)
    echo "the engine restarted with the new version."
    echo "  systemctl --user status comet-native    check the service"
    ;;
  ready)
    echo "next steps:"
    echo "  comet login                              sign in (paste-code) and exit"
    echo "  systemctl --user start comet-native      then start the engine"
    ;;
  manual)
    echo "next: \`comet login\` to sign in, then run the engine with \`comet headless\`."
    ;;
esac
