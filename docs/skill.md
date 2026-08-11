# The `comet-board` skill

Every Claude session that touches this board should know the same things before
it acts: read first, tickets before commits, `--blocked-is-settled` on `wait`,
whose subscription a dispatch spends, and never dispatch speculatively. That
text is a skill — `SKILL.md` — and Claude Code discovers it automatically.

It ships **inside the binary**. The source is
[`assets/skills/comet-board/SKILL.md`](../assets/skills/comet-board/SKILL.md),
compiled into `comet-board` and stamped with the same version, so the skill an
agent reads always documents the flags that binary actually has. Its "Every
verb" table is generated from the CLI's own `clap` tree by a test — the build
fails if the two disagree.

## On your machine

```sh
comet-board skill install
```

That writes `~/.claude/skills/comet-board/SKILL.md` (or under
`$CLAUDE_CONFIG_DIR`, if you set one). Sessions started after it pick the skill
up; nothing else is needed, and there is no file to copy from anywhere.

This is the one you run on a laptop that drives the board remotely — with
`--device <box>` or `COMET_BOARD_DEVICE` — so that a session dispatching to the
box speaks the same conventions as the agents running on it.

Run it again after upgrading `comet-board`. To check without changing anything:

```sh
comet-board skill status        # --json for a script
comet-board skill show          # print the text this binary ships
```

## On the box, and in the slots

Neither needs a person:

- The box setup wizard (`scripts/box-setup-wizard.sh`) installs it in its
  routes-and-doctor stage.
- Each **agent-account slot** gets its own copy on every dispatch. A slot is a
  config dir of its own — the engine points `CLAUDE_CONFIG_DIR` at it so the run
  bills the right subscription — which means the user-level copy under
  `~/.claude` is invisible to exactly the agents the board dispatches. The
  engine writes the skill beside the credentials when it materializes the slot,
  so a new slot is current the first time it runs and every slot tracks whatever
  binary is running.

## When it is stale

`comet-board doctor` has an **agent skill** line:

```console
ok   agent skill    /home/comet/.claude/skills/comet-board/SKILL.md is v0.3.2 · 3 agent-account slot(s), all current
FAIL agent skill    /home/comet/.claude/skills/comet-board/SKILL.md is v0.3.1, this binary ships v0.3.2 — run `comet-board skill install`
```

Only the user-level copy can fail the check: nothing but `skill install` writes
it, so a stale one stays stale. Slot copies are reported and never failed —
the next dispatch re-stamps them.

An installed copy is a build artifact. Edit
`assets/skills/comet-board/SKILL.md` in this repo instead; a copy edited in
place reads as stale (the doctor line says "not the shipped text") and the next
install overwrites it.

## The other channel

Codex slots get no skill: skills are a Claude Code discovery mechanism and
`CODEX_HOME` has no equivalent. What every runtime does have is an instruction
file it reads on its own — `CLAUDE.md` in the Claude config dir, `AGENTS.md` in
`CODEX_HOME` — and as of §gh#272 a dispatch writes the board's conventions into
it, between `<!-- BEGIN comet-board conventions -->` markers, on the same
schedule the skill is installed on.

It is a *shorter* text than this skill, on purpose: an instruction file is in
context on every turn, so it carries only what has to be true before anything is
invoked, and then points at the skill for the rest. Codex, which can invoke no
skill, gets this text appended to it instead. Only what is between the markers
is managed — your own instruction file is left exactly as it was around it, and
`[defaults] agent_instructions = false` (or the same key on a `[[route]]`) takes
the block back out on the next dispatch. `comet-board doctor`'s **agent
instructions** line reports what is installed where.

The canonical prose behind all of it is
[`docs/agent-conventions.md`](agent-conventions.md) — the skill and the block
are the two short forms, that file is the long one.
