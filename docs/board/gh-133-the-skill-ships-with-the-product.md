# The skill ships with the product — **done** (gh#133)

herdr-board had a `board` skill any Claude session discovered on its own.
comet's equivalent was written by hand and lived in three copied places — the
operator's Mac, the box user, and each agent-account slot — which is three
things to remember and one silent failure: a copy documenting flags the binary
no longer has. Now the text is an asset compiled into the binary, and every
place an agent reads from is written by something that already runs.

- **One source, versioned with the CLI it documents.**
  `assets/skills/comet-board/SKILL.md` is `include_str!`'d by
  `comet_board::skill`, and `rendered()` stamps `CARGO_PKG_VERSION` into a
  trailing marker on the way out — so the repo file carries no version to bump
  and every installed copy says which binary wrote it. `status_of` compares
  bytes, not versions: a copy edited in place is stale too, because an
  installed skill is a build artifact.
- **The verb table cannot drift, because it is not written by hand.**
  `apps/board-cli/src/skill_doc.rs` renders the "Every verb" block from this
  binary's own `clap::Command` — verbs, positionals, flags, and each `about`
  line, hidden commands excluded — and a test fails the build when the
  committed file and the parser disagree (`UPDATE_SKILL=1 cargo test -p
  comet-board-bin` rewrites it). The prose above the block is authored; only
  the reference is generated, which is the half that rots.
- **Three install paths, no copying.** `comet-board skill install` writes
  `<config dir>/skills/comet-board/SKILL.md` — the wizard's routes stage runs
  it on a fresh box, and a teammate runs it once on a laptop that dispatches
  with `--device`. The third is the one that had to be automatic:
  `AgentAccounts::materialize` writes it into the slot dir beside the
  credentials, on every dispatch, because a slot *is* the run's
  `CLAUDE_CONFIG_DIR` and the user-level copy is invisible from inside one. The
  write is byte-compared first (a re-materialize that changes nothing writes
  nothing) and never fatal — a dispatch that can run beats a file that could
  not be written.
- **Doctor knows the difference between "stale" and "self-healing".** The
  `agent skill` check fails on the user-level copy, which nothing but `skill
  install` writes, and only reports on the slots, which the next dispatch
  re-stamps — otherwise every version bump would turn doctor red over files
  that fix themselves.
- **Claude Code only.** Skills are its discovery mechanism; `CODEX_HOME` has no
  equivalent, so a Codex slot is left alone and still learns the board from
  `docs/agent-conventions.md`. [`docs/skill.md`](skill.md) is the operator-facing
  version of all of the above.
