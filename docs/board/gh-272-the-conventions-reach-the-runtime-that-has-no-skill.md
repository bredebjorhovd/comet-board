# The conventions reach the runtime that has no skill — **done** (gh#272)

Landed as `crates/board/src/conventions.rs` (the marker-managed writer, plus the
compiled-in text in `assets/instructions/comet-board.md`), called from
`CometRuntime::dispatch` (`crates/engine/src/board_runtime.rs`) with
`agent_instructions` on `[defaults]` and per `[[route]]`.

§gh#133 gave the board's contract a shipping channel and closed it for exactly
one harness. Claude Code discovers `<config dir>/skills/comet-board/SKILL.md`,
and `AgentAccounts::materialize` writes that into every account slot on every
dispatch — so a dispatched Claude agent can find out what the board is. Codex
has no skill mechanism at all, and the comment in `agent_accounts.rs` conceded
as much: a Codex slot "learns the board from `docs/agent-conventions.md` the way
it always has", which is to say by nobody, mechanically. `docs/agent-conventions.md`
had carried a marker pair since the port, described as being there "so an
installer can write it into each runtime's global instruction file". Nothing was
that installer.

Both CLIs read one file out of the same config dir the engine already relocates
per run — `CLAUDE.md` under `CLAUDE_CONFIG_DIR`, `AGENTS.md` under `CODEX_HOME`
— so the write is mechanically small and sits two lines from the skill install
it mirrors. What took thinking is what goes in it, whose file it is, and what
happens on the second dispatch.

### What goes in it, and why the skill did not shrink

An instruction file is in context on **every turn**; a skill is read when the
agent judges it relevant. They are not two spellings of one thing, so the block
is not a copy of the skill:

- **The block carries what must be true before anything is invoked** — you may
  have been dispatched, `comet-board` is on your PATH, commit *and push*, the
  push credential is the board's and a failure is a stop, claim what you
  changed, delegate through the board, never dispatch speculatively. About fifty
  lines.
- **Claude then gets a pointer** at the skill sitting in the same config dir,
  which is the deep reference: every verb, every flag, the claims contract.
- **Codex gets the skill's own text appended**, frontmatter stripped, because
  for Codex there is nothing to point *at*. It pays for a longer block than
  Claude does, and that is the correct asymmetry: it has no other channel.

So the skill stayed exactly as it was. Shrinking it to invocation-only would
have traded a real regression — every Claude session on the box that is *not* a
board dispatch reads the skill and nothing else — for a duplication that only
looks like one.

`docs/agent-conventions.md` stays the long canonical text, and the two short
forms answer to it. Three texts is one more than is comfortable; the alternative
was compiling the 390-line canonical document into every dispatched agent's
context window on every turn.

### Whose file it is

The block is managed, the file is not. Everything between
`<!-- BEGIN comet-board conventions -->` and its `END` belongs to the binary and
is rewritten whole on every dispatch; everything outside is somebody's own
instruction file and is never read for meaning, reordered, or dropped. Tested in
both directions: install, update, and removal all leave the surrounding text
byte-identical, and a file that was only ever ours is deleted rather than left
behind as an empty `CLAUDE.md`.

That matters because of where a dispatch naming **no account** lands. It reads
the CLI's own config dir — the box user's `~/.claude` or `~/.codex` — which on a
single-login box is every dispatch there is. Restricting the write to
materialized account slots would have been the safe-looking choice and would
have shipped a feature that reaches almost no board. So it writes there, in the
one form that can be undone.

Two guards on the writer, both because it is a guest:

- **A half-marked file is refused, not spliced.** One marker without the other,
  or `END` before `BEGIN`, and nothing is written — guessing where a block ends
  is how a splice eats a paragraph. `doctor` fails on exactly this state,
  because it is the only one that does not repair itself.
- **The write is atomic and byte-compared first.** Every dispatch calls it, so a
  no-op has to be a genuine no-op: nothing rewritten under a CLI that may be
  reading it, and never half a document on disk.

### The second dispatch, and the one that opted out

`agent_instructions` is on by default. The flag exists for the two boxes where
the write is unwelcome — an operator who wants nothing in their own
`~/.claude/CLAUDE.md`, and an account slot shared with work that is not the
board's — rather than because the choice is interesting. A Codex agent that has
never heard of the board is not a configuration anybody wants.

Turning it off **removes** rather than merely stopping. Account dirs are reused
across dispatches and routes; a route that opted out would otherwise keep
serving whatever the last dispatch on that slot left behind, which is the exact
staleness the marker pair exists to prevent. Same shape as the turn guardrails
(§gh#270): resolved from the route in `build_spec`, carried on `DispatchSpec`,
actuated in the engine — because the executor writes the file beside the config
dir it just materialized, and has no `routing.toml` anywhere near it.

Unlike the turn limits it is *not* stamped on the chat. The instruction file is
a property of the account dir rather than of one attempt: a later run in the
same chat reads whatever the most recent dispatch through that dir wrote, and
pretending otherwise would mean tracking per-chat what is a per-directory fact.

### What it never touches

The checkout. A dispatched worktree ships its repo's own `AGENTS.md` /
`CLAUDE.md`, and those are the authority on how to write code in it. The block
says so in as many words — *where they appear to disagree with this, the repo
wins* — and the writer only ever opens paths under a config dir.

### Where it shows

`comet-board doctor` gained an **agent instructions** line beside the existing
**agent skill** one: how many instruction files carry this binary's block, how
many are behind (the next dispatch rewrites them), how many have none, and how
many routes ask for one. It fails on nothing but a file it refuses to touch —
for §gh#133's reason, a version bump must not turn doctor red over a file that
repairs itself.

Which harness a materialized slot dir belongs to is read off the credential the
CLI left in it (`.credentials.json` → Claude, `auth.json` → Codex), because a
slot dir is named by its id and nothing else.

### Not done here

- **No `comet-board conventions` verb group.** The write happens on dispatch and
  `doctor` reports it; `routes defaults agent_instructions false` is the whole
  of the opt-out. A verb to install by hand would be a fourth way to produce the
  same file.
- **No user-editable source.** The text is compiled in, like the skill, for the
  drift reason §gh#133 gives: a copy nobody re-copies goes stale silently
  against a CLI whose flags it documents. The escape hatch is better than an
  editable source anyway — write your own instructions *outside* the markers and
  they survive every dispatch.
- **A live read-back probe.** That the Claude arm lands in the file the CLI
  reads rests on the same "`CLAUDE_CONFIG_DIR` relocates the dir wholesale"
  property the skill install has relied on in production since §gh#133;
  `crates/harness/tests/acp_probe.rs` already pins the relocation itself. A
  probe that proves the *model* saw the text needs an authenticated slot and
  spends somebody's subscription to assert it, which is a decision for the
  operator rather than for CI.
