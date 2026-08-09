# A test's board is the directory it made — **done** (gh#190)

The box's `syncd.log` recorded **31 `board loop up` lines in one day**, in bursts
three seconds apart, interleaved with the real board's work. The engine restarted
twice; the rest line up with the windows in which dispatched agents were running
`cargo test`. §gh#162 found this failure and fixed the two files it was looking at.
The bursts are the proof that a convention does not hold across a suite this
size — and the resolution that decides where `syncd.log` goes decides where
`board.db` goes, so those runs were reading the live queue on the way past.

- **The variables are an answer, not an instruction.** `COMET_BOARD_CONFIG_DIR` /
  `COMET_BOARD_STATE_DIR` exist for one process: a `comet-board` that git spawns
  as `GIT_ASKPASS` with none of its parent's arguments (§gh#68). The engine exports
  its *already-resolved* pair into every dispatched agent's environment so that
  helper attaches to the board that dispatched it. They answer "which board is
  this shell's". They were never a way to relocate a board — and every process an
  agent starts inherits them, `cargo test` included.
- **So `Paths::under` no longer reads them.** It is pure: the data dir the caller
  names is the answer. `Paths::discover` — the resolution for a `comet-board`
  handed no data dir — is now the only reader in the workspace, which is what the
  pair was introduced for. This also fixes a case nobody had named: a dev engine
  started under `COMET_DATA_DIR=/some/scratch` *inside* an agent took the live
  board too, which is the likeliest source of the bursts three seconds apart.
  An operator relocating an install moves `COMET_DATA_DIR`, which the derivation
  follows; the two-variable escape hatch for the engine is gone, and nothing on
  disk used it.
- **Every hand-built `Paths` went back through the constructor.** The four
  helpers §gh#162 left behind (`device_routing.rs`, `board_members.rs`,
  `push_credentials.rs`, the board-service unit tests) were struct literals
  written to dodge the environment. They call `Paths::under` again, because the
  guarantee is the constructor's now instead of each caller's discipline — which
  is the actual fix, since the next test to be written will not have read §gh#162.
- **Two guards, at both altitudes.** `crates/engine/tests/board_env_isolation.rs`
  sets both variables to a poisoned directory, as a dispatched agent's
  environment sets them, starts a **real board loop** on a tempdir, waits for its
  own `board loop up`, and fails if a single byte lands in the poisoned one — it
  names `syncd.log` and `board.db` when it fires, which is the exit criterion
  read back verbatim. `crates/board/tests/env_isolation.rs` holds the same
  guarantee one layer down, pins that `discover` *does* still honour the pair (so
  the fix cannot become "read them nowhere" and strand the askpass helper), and
  scans the workspace: outside the files that own the pair, no `.rs` file may
  name it, and nothing but the CLI's entry point may call `Paths::discover`. The
  scan is what catches the *next* test, at the line that did it, rather than the
  symptom a month later in somebody's log.
- **Verified the way the ticket asked.** With both variables pointed at a
  directory standing in for the live board, the whole workspace run — 1714 tests,
  42 binaries — wrote nothing there, and the box's own `syncd.log` gained no
  `board loop up` line. Reintroducing the old resolution fails both guards.
- **Noted, not fixed:** `Repos::new` resolves its worktree root from
  `COMET_WORKTREES_DIR`, else `$HOME`, so an engine test that cuts a checkout
  through `EngineCore::assemble` alone would write under the box's real worktrees
  root. Every such test avoids it today — some pass `with_worktrees_root`, others
  `set_var` the override themselves — and the live root was untouched by the run
  above. But that is a convention again, of exactly the kind this section exists
  about. Not folded in here: it is a different directory with a different owner
  (§gh#186's), and the fix is a seam in `Repos`, not in the board's paths.
