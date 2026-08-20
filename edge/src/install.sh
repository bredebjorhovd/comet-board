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
case "$ver" in
  *[!0-9A-Za-z._-]* | .*)
    echo "comet install: release service returned an invalid version '$ver'" >&2
    exit 1
    ;;
esac
file="comet-$ver-$plat-$arch.tar.gz"
data_root="$HOME/.comet-native"
app_root="$data_root/app"
dest="$app_root/$ver"
mkdir -p "$app_root"

# A managed release is one indivisible pair. Validate the files themselves —
# not merely the tar listing — before reusing a stage or moving `current`.
validate_release() {
  release_dir="$1"
  expected="$2"
  [ -d "$release_dir" ] && [ ! -L "$release_dir" ] || {
    echo "comet install: $release_dir must be a real release directory" >&2
    return 1
  }
  for name in comet comet-board; do
    binary="$release_dir/$name"
    [ -f "$binary" ] && [ ! -L "$binary" ] && [ -x "$binary" ] || {
      echo "comet install: incomplete release: $binary must be a regular executable" >&2
      return 1
    }
    if command -v timeout >/dev/null 2>&1; then
      line="$(timeout 5 "$binary" --version 2>/dev/null | sed -n '1p')" || line=""
    else
      line="$("$binary" --version 2>/dev/null | sed -n '1p')" || line=""
    fi
    [ "$line" = "$name $expected" ] || {
      echo "comet install: $binary reported '${line:-nothing}', expected '$name $expected'" >&2
      return 1
    }
  done
}

if [ -d "$dest" ] && validate_release "$dest" "$ver"; then
  echo "comet $ver already downloaded — relinking."
else
  if [ -e "$dest" ] || [ -L "$dest" ]; then
    active="$(readlink -f "$app_root/current" 2>/dev/null || true)"
    existing="$(readlink -f "$dest" 2>/dev/null || true)"
    if [ -n "$active" ] && [ "$active" = "$existing" ]; then
      echo "comet install: the active release is incomplete; refusing to remove it in place." >&2
      echo "Restore the previous current symlink, then re-run this installer." >&2
      exit 1
    fi
    rm -rf "$dest"
  fi
  tmp="$(mktemp -d "$app_root/.stage-$ver.XXXXXX")"
  trap 'rm -rf "$tmp"' EXIT
  echo "downloading comet $ver ($plat-$arch)…"
  curl -fSL --progress-bar "$BASE/releases/$file" -o "$tmp/$file"
  mkdir -p "$tmp/unpacked"
  tar -xzf "$tmp/$file" -C "$tmp/unpacked" --strip-components=1
  validate_release "$tmp/unpacked" "$ver" || {
    echo "comet install: refusing to activate an incomplete release; current was not changed" >&2
    exit 1
  }
  mv "$tmp/unpacked" "$dest"
fi

# Same-filesystem rename: there is never a window without `current`, and every
# failure above leaves its previous target untouched.
next="$app_root/.current-$$"
rm -f "$next"
ln -s "$dest" "$next"
mv -Tf "$next" "$app_root/current"
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
# with nobody deciding it should, so this reports it and moves on. `comet status`
# says the same thing afterwards, from a binary this script does upgrade.
linked=""    # names this run actually put on PATH
held=""      # "<bin> (<version>) -> <target>" lines for the ones it would not touch
held_bins="" # the same paths alone, for the rm hints

# What a binary says it is, so the warning below can name the version it is
# leaving in place instead of gesturing at it. A copy too old to know
# `--version` dates itself by failing: the flag ships with the first release
# that carries comet-board at all.
#
# Under `timeout` where there is one (coreutils; this script is Linux-only), and
# five seconds is generous for a flag that prints and exits. The binary being
# run is by definition one nobody vouches for — the whole reason we are asking
# is that we do not know what it is — and a copy that hangs instead of answering
# must not be able to wedge an installer somebody is watching over a curl pipe.
version_of() {
  if command -v timeout >/dev/null 2>&1; then
    v="$(timeout 5 "$1" --version 2>/dev/null | head -1 | awk '{print $NF}')" || v=""
  else
    v="$("$1" --version 2>/dev/null | head -1 | awk '{print $NF}')" || v=""
  fi
  case "$v" in
    [0-9]*) echo "v$v" ;;
    *) echo "version unknown — too old to answer \`--version\`" ;;
  esac
}

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
        held="$held  $bin ($(version_of "$bin")) -> $points_at
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
# gh#529: the default OOMPolicy=stop turns one OOM-killed agent into a dead
# engine. Throttle the cgroup before the kernel acts; kill inside it after.
# The third copy of these three lines, and the only one that cannot share the
# constant — this file is shell served off the edge. The other two are
# `comet_update::service::RESOURCE_GOVERNANCE` (rendered by `comet daemon
# install`, and shipped as a drop-in on every engine update, gh#533); change
# them together, and `comet-board doctor`'s `engine unit` line is what catches
# it if they drift.
OOMPolicy=continue
MemoryHigh=75%
MemoryMax=90%

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
    echo "      each stays at the version above, while this box is now on $ver."
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
