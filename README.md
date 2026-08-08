# comet-board

A fork of [zeronsh/comet](https://github.com/zeronsh/comet) adding an
autonomous-agent task board, ported from
[herdr-board](https://github.com/bredebjorhovd/herdr-board): Linear and GitHub
issues come in, a dispatch releases a task into a comet chat with a coding
agent, and session state reconciles back to the board and the trackers.
Agents read the board and dispatch from it themselves.

Pin one chat as the board's **orchestrator** and it receives every settle,
block, orphan and cap warning on the board — so one long-lived agent can
dispatch, review, retry and report while your job reduces to reading its
summaries. [docs/orchestrator.md](docs/orchestrator.md) is the brief to open
that chat with.

Sessions on your own machine learn the board's conventions from a skill that
ships inside the binary — `comet-board skill install` writes it into
`~/.claude/skills/`, and dispatched agents get their own copy automatically
([docs/skill.md](docs/skill.md)).

Board code lives in `crates/board`; the port's status, design mapping, and
remaining work are in [docs/BOARD.md](docs/BOARD.md). Upstream comet is the
`upstream` remote; everything below this section is its README.

## Local (edge-less) mode — the fork's primary deployment

This fork is built to run on a single box: the daemon and the TUI talk over
localhost IPC, and you reach the box over mosh or Tailscale. No edge service,
no account, no sync — the box is the whole system.

Set `COMET_EDGE_URL=off` to make that a stated configuration instead of an
accident:

```bash
COMET_EDGE_URL=off comet headless        # or: COMET_EDGE_URL=off comet daemon install
comet tui                                # attaches over localhost IPC
```

With the edge off, the engine skips every edge transport — no session-room
joins (and none of the per-chat join warnings), no presence, no device room,
no release polling — and auth runs in dev mode with no sign-in.
`COMET_EDGE_URL=off comet status` reports `Edge: disabled (local mode)`.
`comet daemon install` captures the variable into the service unit, so an
installed daemon stays local across restarts.

Multi-device sync needs an edge: leave `COMET_EDGE_URL` unset for the
production edge, or point it at a self-hosted one. `comet update` also fetches
releases from the edge, so local-mode installs update from source instead.

---

# Comet

Control your coding agents (Claude Code, Codex) from any of your devices.

![Comet running a Claude Code session](docs/screenshot.png)

Every device runs a small engine that keeps your sessions in sync: start an
agent on one machine, follow and drive it from another. Install the engine as
a daemon on an always-on machine (a VPS, a spare box) and your agents keep
working after you close your laptop.

## Install the daemon (Linux)

```bash
curl -fsSL https://edge.comet.offhand.dev/install.sh | sh
comet login                          # sign in (paste a code, done)
systemctl --user start comet-native
```

That installs two binaries from the same release: `comet`, the engine, and
`comet-board`, the CLI that drives the board. They are upgraded together and
`comet-board doctor` fails if they ever fall out of step. If something you put
in `~/.local/bin` yourself is already in the way, the installer says so and
leaves it alone rather than overwriting it.

No configuration needed. Day-to-day:

```bash
comet status      # signed in? engine running? edge rooms actually connected?
comet update      # update to the latest release
comet tui         # terminal UI, attaches to the daemon
comet daemon start|stop|restart|status
```

## macOS

Download the `.dmg` from [the latest release](https://github.com/bredebjorhovd/comet-board/releases),
drag `Comet.app` to Applications, then — because the build is not yet signed
with an Apple Developer ID — clear the download quarantine once:

```bash
xattr -dr com.apple.quarantine /Applications/Comet.app
```

Without that step macOS refuses to open the app and says nothing useful about
why. [docs/macos-install.md](docs/macos-install.md) explains what is going on
and what removes the step for good.

To run the engine as a background service instead, build `comet` from source
and `comet daemon install` (launchd).

---

Developing or curious how it works? See [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
