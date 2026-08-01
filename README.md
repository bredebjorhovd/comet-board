# comet-board

A fork of [zeronsh/comet](https://github.com/zeronsh/comet) adding an
autonomous-agent task board, ported from
[herdr-board](https://github.com/bredebjorhovd/herdr-board): Linear and GitHub
issues come in, a dispatch releases a task into a comet chat with a coding
agent, and session state reconciles back to the board and the trackers.
Agents read the board and dispatch from it themselves.

Board code lives in `crates/board`; the port's status, design mapping, and
remaining work are in [docs/BOARD.md](docs/BOARD.md). Upstream comet is the
`upstream` remote; everything below this section is its README.

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
curl -fsSL https://comet.zeron.sh/install.sh | sh
comet login                          # sign in (paste a code, done)
systemctl --user start comet-native
```

No configuration needed. Day-to-day:

```bash
comet status      # signed in? engine running?
comet update      # update to the latest release
comet tui         # terminal UI, attaches to the daemon
comet daemon start|stop|restart|status
```

On macOS: build `comet` from source, then `comet daemon install` (launchd).

---

Developing or curious how it works? See [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
