---
name: comet-board
description: Read and drive the comet board — the task board fed by GitHub/Linear that dispatches coding agents into comet chats on the box. Use when asked what work is queued/ready/blocked, to pick up or release a task, to check on a running agent, or when an issue should become an agent. Dispatching starts real agents that commit and open PRs.
---

The board is one global queue: GitHub (and optionally Linear) issues in, comet
chats running coding agents out. The board lives on ONE host device (the box);
`comet-board` speaks to it over the local IPC, or from any other machine with
`--device <box-name>`.

The engine puts `comet-board` on your PATH — the copy it shipped with. If it is
not there, say so and stop; do not carry on without the board. Work done off the
board has no row, no provenance and no one who can see it (`comet-board doctor`
reports this as **agent PATH**).

**Read before acting. Never read board.db directly.**

```bash
comet-board list --state ready  --json     # what can be picked up
comet-board list --state review --json     # finished, PR waiting on a human
comet-board list --state blocked --json    # agent stuck or asking
comet-board list --json                    # everything, most urgent first
comet-board --device <box> list ...        # from a machine that isn't the box
```

**Work you delegate goes THROUGH the board.** `comet-board new "title"
--dispatch` costs one line and makes the work traceable — branch, PR, review,
settle, billing. In-chat subagents are for reading and research; anything that
lands a commit is a ticket. Bypassing the board leaves rows dispatchable under
you and your agents invisible to every surface.

**Close the issue from the work**: `Closes #N` in a commit on the branch. A
merged PR settles the row itself.

## Finishing: claim what you did

Before you say you are done, tell the board what you changed — in **claims**,
one per line, each anchored to the files it is about:

```bash
comet-board claim --task gh:owner/repo#183 <<'EOF'
Claims are stored against the attempt :: crates/board/src/db.rs crates/board/src/model.rs
The remainder is computed from the branch diff :: crates/board/src/claims.rs
EOF
```

`<sentence> :: <path> [<path>…]`. Paths are repo-relative; a directory
(`crates/board/src/`) accounts for everything under it. A line with no `::`,
or nothing after it, is **refused** — without file anchors a claim cannot be
checked, and an unanchored summary is the thing this replaces.

The reply is the part worth reading: the board diffs your branch and prints
**every changed file no claim accounts for**. That set is computed from git, not
from what you wrote, so it is where the dependency you bumped and the function
you edited in passing turn up. Read it and either claim those changes or go and
look at them — they are the ones a reviewer would have caught.

Claims live on the attempt, so they outlive this chat; a retry makes its own.
Submitting again replaces the set. `comet-board review --task <id> [--json]`
prints the whole thing back: brief, claims, the commands the run ran, and the
remainder.

**Releasing and waiting:**

```bash
comet-board dispatch --task gh:owner/repo#14 [--account <slot>] [--runtime ..] [--model ..]
comet-board retry    --task gh:owner/repo#14   # blocked → replace; failed/ready → dispatch
comet-board wait --timeout 3600 --json [--blocked-is-settled]
```

`wait` with no --task watches everything in flight when called; it does NOT
return on blocked unless you pass `--blocked-is-settled` — an orchestrator that
skips that flag hangs on a child's question forever.

Rules (canonical text: docs/agent-conventions.md in the comet-board repo):

- Check `dispatchable` first — false means no route; fixing routes is the
  operator's call (`comet-board routes`, or Settings → Board routing).
- Provenance is automatic: your chat id rides COMET_BOARD_CHAT_ID; never
  fabricate `--via`.
- One live attempt per task; a second dispatch fails cleanly. Caps refuse at
  max_concurrent — report, don't cancel someone else's work.
- **Billing**: a dispatch naming no account runs on the box owner's Claude
  login. Pass `--account <slot>` for whoever should pay; the picker rows and
  the CLI warn on cross-billing (billing_guard).
- Cancel ends the attempt, not the issue; the row returns to ready.
- After releasing work, wait for it or say plainly you're leaving it running.
  Your chat is prompted when it settles or blocks (`notify_dispatcher`, on by
  default) — and is the first addressee, so what reaches you does not also
  reach the board's orchestrator. Never promise you'll be woken: the setting
  is invisible from here and an archived chat is told nothing.
- Never dispatch speculatively — a human keypress or explicit instruction
  releases tasks. Reading is always safe.
- New repo: `comet-board onboard <owner/repo>` (clone on box + space + adopt,
  one verb). New *person*: `comet-board member add <their-sign-in-email>
  --github <login>`, or their dispatches commit under the box owner's name —
  `comet-board member list` shows who is mapped and who has no agent account
  (docs/teammate.md). `comet-board doctor` explains a board that looks wrong.
- **Screenshots in a PR description**: commit them and link them with a
  relative path from a markdown file in the repo. A
  `raw.githubusercontent.com/<owner>/<repo>/<branch>/…` URL is broken on a
  private repo and dies with the branch on merge — silently, both times.

The pinned orchestrator's fuller brief: docs/orchestrator.md.

## Every verb

<!-- BEGIN generated verbs — rendered from clap by `cargo test -p comet-board-bin`; rewrite with UPDATE_SKILL=1 -->
Global flags, on every verb: `--port`, `--data-dir`, `--device`.

| verb | flags | what it is for |
| --- | --- | --- |
| `list` | `--state`, `--source`, `--json` | List what is on the board. `--json` for orchestrating agents |
| `dispatch` | `--task`, `--via`, `--runtime`, `--model`, `--account`, `--bill` | Release a task into a coding-agent chat |
| `retry` | `--task`, `--via`, `--runtime`, `--model`, `--account`, `--bill` | Release a task again — the desktop panel's Retry, from a shell |
| `cancel` | `--task` | Cancel a task's live attempt. The issue stays open |
| `wait` | `--task`, `--state`, `--blocked-is-settled`, `--timeout`, `--json` | Block until watched work settles. The counterpart to `dispatch` |
| `claim` | `--task`, `--claim`, `--json` | Say what your attempt did, in claims a reviewer can check |
| `review` | `--task`, `--attempt`, `--json` | What an attempt was asked to do, what it says it did, and what it did not account for |
| `new <title>` | `--body`, `--team`, `--label`, `--source`, `--repo`, `--dispatch` | Write a ticket. Cheaper than not writing one |
| `stats` | `--since-days`, `--json` | What the board knows about its own throughput |
| `doctor` | — | Check the environment: keys, engine, routes, repos. Exits non-zero on any failing check |
| `init` | `--force` | Generate a starter routing.toml from the spaces on this device |
| `routes` | — | Read and change the board's `routing.toml` — over the RPC, so `--device` reaches the box that hosts the board (gh#75) |
| `routes list` | `--json` | The routes in force, what is wrong with the config, and what is not routed yet |
| `routes show` | — | Print `routing.toml` verbatim. Comments and all: this is the file |
| `routes add <slug>` | `--labels`, `--all-issues` | Route a repo that has a space on the board's device but nothing watching it — the `[[route]]` and `[github] repos` halves, written together |
| `routes ignore <slug>` | — | Stop offering a repo — you are only reading it |
| `routes set <route> <key> [value]` | `--unset` | Set one key on one route: `routes set 2 account brede-personal` |
| `routes defaults <key> [value]` | `--unset` | Set one key under `[defaults]`: `routes defaults max_duration 4h` |
| `routes edit` | — | Open `routing.toml` in `$EDITOR` and write it back, validated |
| `member` | — | Who else drives this board — the `[users]` map that decides whose name a teammate's dispatched commits carry (gh#162) |
| `member add <email>` | `--github`, `--name` | Map a teammate's sign-in email to their GitHub identity, so their dispatches commit as them |
| `member list` | `--json` | The map, the box's agent-account slots, and who has one without the other |
| `member remove <email>` | — | Take somebody out of the map — offboarding, or an entry for the wrong account |
| `onboard [slug]` | `--dir`, `--labels`, `--all-issues`, `--json` | Put a repo the board has never seen on the board: clone it, give it a space, and route it — one verb (gh#97) |
| `adopt [slug]` | `--labels`, `--all-issues`, `--ignore` | Offer git-detected spaces the board is not watching; adopt one by slug |
| `skill` | — | Install this skill — the one you are reading — where agents on this machine will find it |
| `skill install` | `--dir` | Write it into a Claude config dir (default `$CLAUDE_CONFIG_DIR`, else `~/.claude`) |
| `skill status` | `--json` | Where the skill is installed, and whether it matches this binary |
| `skill show` | — | Print the skill this binary ships, stamped with its version |
<!-- END generated verbs -->

<!-- comet-board skill {{VERSION}} — shipped with the binary. The source is
     assets/skills/comet-board/SKILL.md in the comet-board repo; installed
     copies are overwritten by `comet-board skill install`, so edit the repo. -->
