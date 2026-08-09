# The release ships the board CLI too — **done** (gh#156)

Found while upgrading the box to v0.3.4: `~/.comet-native/app/0.3.4/` held
`comet`, two icons and a desktop entry, and `~/.local/bin/comet-board` was a
symlink into a source checkout, made by hand on 6 August and untouched since.
The release payload had never carried the board CLI, so `install.sh` upgraded
the engine on every release and stepped over the binary that drives the board.
By then the box was running 17 routes over 8 repos with a CLI three weeks
behind them: `onboard` (§gh#97) and `skill` (§gh#133) did not exist on the machine
whose agents were supposed to use them.

- **Both binaries, one payload.** They come off the same `cargo build`;
  shipping one was the entire bug. `scripts/package-linux.sh` stages
  `comet-board` beside `comet`, `scripts/package-macos.sh` puts it in
  `Comet.app/Contents/MacOS` — signed *before* the bundle, since nested code
  is not covered by signing the wrapper and notarization rejects the
  submission over one unsigned helper. `release.yml` needed no change: it
  uploads whatever the packaging scripts produce.
- **Both packaging scripts now prove it.** Each fails the build if its output
  is missing either binary — the tarball listing is grepped, the bundle's
  `Contents/MacOS` is stat'd. An omission that ships is exactly what happened
  the first time, and it cost nothing to make it impossible to repeat quietly.
- **One lookup was already written for this layout.** `resolve_board_exe`
  (§gh#68's askpass helper) tries `COMET_BOARD_EXECUTABLE`, then *beside the
  running binary* — "how it is installed next to the engine" — then PATH. The
  middle step could never hit, because nothing ever put the two side by side; a
  dispatched agent's `GIT_ASKPASS` resolved through PATH to whatever stale
  binary was there. The comment described the intended layout and the release
  did not ship it. Now it does, and the fallback is a fallback again.
- **Links point at `current`, not at a version.** `~/.local/bin/comet-board →
  ~/.comet-native/app/current/comet-board`, so a later `comet update` flips one
  symlink and both binaries follow it. That is also precisely why an unmanaged
  binary in the way matters: it is the one thing the flip cannot move.
- **A hand-placed binary is not silently replaced.** Both installers take over
  `~/.local/bin/<name>` only when it is missing or already theirs — a symlink
  into the app root for the curl|sh installer, a regular file for the copying
  tarball one. Anything else is a decision a human made, sitting ahead of the
  installer on PATH; overwriting it would destroy a build tree nobody chose to
  throw away. So they name what is there, name the `rm -f` that hands it over,
  and leave it standing. Refusing loudly is a fix; the failure was never the
  stale binary, it was that nothing said so.
- **`doctor` compares the two versions.** The engine reports its own
  `CARGO_PKG_VERSION` in the `LocalDevice` reply, and the `cli version` check
  fails when the CLI's disagrees, naming the path of the binary that answered —
  which copy is talking is most of what you need. An unreachable engine falls
  back to the installed payload's directory name, because a box whose engine is
  down is exactly when someone runs doctor; neither available is "not checked"
  rather than a failure. Against a payload that predates this fix it says so
  instead of offering an installer that would relink nothing. Every other check
  in that report asks about the environment the CLI can see. This one asks
  about the CLI, which is how the whole class of bug stayed invisible.
- **But doctor cannot be the only teller, because it ships inside the stale
  thing.** A CLI old enough to have drifted is old enough not to carry the
  check — so on the one box with the problem, `comet-board doctor` goes on
  reporting a clean board. The check has to also live where the *current* code
  runs, and on that box the current code is the engine. `board_cli::probe`
  inverts it: find the binary (`resolve_board_exe`), run `--version`, compare
  against the engine's own. `comet status` prints a `Board CLI:` line from it,
  and `comet headless` logs one WARN at boot when they disagree — the only
  report in this whole section that fires without somebody first going to look,
  and it fires on the restart the install itself performs. Off the boot path on
  its own thread: the probe executes a binary nobody vouches for, and one that
  hangs instead of answering must not be able to hold the engine down.
- **`comet-board --version`, for everything outside the binary.** doctor never
  needed it — a process knows its own `CARGO_PKG_VERSION` — which is why it
  did not exist, and why `install.sh` could see a binary in its way and not say
  which one. Now the warning names it: `~/.local/bin/comet-board (v0.2.9) ->
  ~/comet-board/target/release/comet-board`. A copy too old to know the flag
  dates itself by failing, since the flag lands with the first release that
  ships this binary at all — reported as "too old to answer `--version`", which
  is a fact and not a guess.
