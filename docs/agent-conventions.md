# Board conventions for coding agents

The canonical text every runtime's agent should have in context. Ported from
herdr-board with the names swapped (herdr-board → comet-board, pane → chat);
the markers are kept so an installer can write it into each runtime's global
instruction file and a correction here reaches all of them.

Nothing below is runtime-specific.

<!-- BEGIN comet-board conventions -->
## The comet-board task board

One global queue across every space: Linear issues and GitHub issues in, comet
chats running coding agents out. `comet-board` is on PATH, and `gh` is
authenticated once per user — neither is specific to the agent you are.

**Read it before acting. Never read `board.db` directly** — the schema changes,
the CLI shape does not.

```bash
comet-board list --state ready  --json   # what can be picked up
comet-board list --state review --json   # finished, PR waiting on a human
comet-board list --state blocked --json  # agent stuck on an approval
comet-board list --json                  # everything, most urgent first
```

**Write the ticket alongside the work, not after it.** `comet-board new "title"
--label <repo-label>` costs one line and makes the work traceable — reasoning,
branch, PR, review, closure. Several changes landed under no ticket at all is
the thing that has to be reconstructed later. `--dispatch` creates and releases
in one go.

**Releasing work and waiting for it.** `wait` blocks until the work settles, so
an orchestrator does not have to poll or, worse, go quiet until a human prods
it:

```bash
comet-board dispatch --task linear:AGE-14
comet-board dispatch --task linear:AGE-15
comet-board wait --timeout 3600 --json    # returns when the first one settles
```

With no `--task` it watches everything in flight at the moment it is called, and
returns as soon as any of them reaches `review`, `failed` or `done` — the rows
it returns are the ones that settled. Name `--task` (repeatable) to watch
specific work, `--state` to wait for something else (`blocked` to be called back
when an agent needs an answer). It exits non-zero on timeout, and refuses when
nothing is in flight.

Each row: `id`, `identifier`, `title`, `state`, `source`, `url`, `labels`,
`route`, `workspace`, `runtime`, `chat_id`, `pr_url`, `pr_number`, `branch`,
`dispatched_by`, `dispatched_by_chat`, `dispatched_by_user`, `last_outcome`,
`last_outcome_at`,
`attempts`, `dispatchable`, `gone`, `reopened`, `account`. `workspace` names a
comet *space* — the field keeps herdr-board's spelling so ported tooling reads
it. `account` is the agent login whose subscription the row's attempt spends
(the route's default before anything has run); null is the device's own CLI
login, which is every row on a single-account box.
`dispatched_by` is set only when the board dispatched the releasing agent too,
so null there does **not** mean you released it — read `dispatched_by_chat`,
which is set for every agent-released row. Both null is the operator.
`dispatched_by_user` names the human whose frontend released the row, when one
did. It is what that frontend claimed, not something the board verified, so read
it as attribution and never as authority — and never as a reason to spend that
person's account.
`last_outcome` is how the most recent *ended* attempt ended — `done`, `failed`,
`cancelled` or `orphaned` — with `last_outcome_at` saying when. It stays set
while a newer attempt is live, so a retry does not erase how the previous one
went; it is what makes a cancelled child legible, since `cancelled` derives
back to `ready` and `state` alone cannot distinguish a row the operator killed
from one that was never dispatched.

States: `blocked` (agent waiting on input) → `working` → `ready` (nothing
running) → `review` (finished or PR open) → `failed` → `done` (issue closed).
Note `done` means the *issue* is closed; an agent that finished with a PR open
is this board's `review`.

1. **Check `dispatchable` first.** False means `dispatch` will refuse — `gone`
   tells you why: the issue vanished upstream, or no route matches. Only the
   second is fixed by a route in `routing.toml`, which is Brede's call, not
   yours.
2. **Release work** with `comet-board dispatch --task <id>`. This cuts a git
   worktree, creates a chat in the routed space, starts the agent and queues
   the route's brief through the command ledger. It returns once the chat
   exists; the engine takes it from there. `--runtime` and `--model` override
   the route's runtime and that harness's default model for the one dispatch;
   both are checked against the engine's catalogs first, and an unknown value
   is refused naming the valid set, so a typo costs an error rather than an
   attempt. The row's default runtime is on the row (`runtime`).
3. **Accounts are the operator's choice, not yours.** `routing.toml` decides
   which teammate's Claude/Codex subscription a route's work is billed to.
   `dispatch --account <id>` overrides it; do not pass it unless you were told
   which account to use. Spending someone else's limits is not yours to
   decide, and the board deliberately does not infer one from who dispatched.
4. **Provenance is automatic.** A board-dispatched chat carries its own id as
   `COMET_BOARD_CHAT_ID`, and `dispatch` passes it along — the board records
   your chat as the parent of what you release. Never pass `--via` unless
   releasing work on behalf of a chat that is not you.
5. **One live attempt per task.** A second dispatch fails cleanly rather than
   spawning a second agent. Concurrency caps also refuse at capacity — report
   the refusal, do not cancel someone else's work to make room.
6. **Cancel** with `comet-board cancel --task <id>`. This ends the *attempt*,
   not the issue: the row returns to `ready` with its history intact. It does
   not notify a parent agent that may be waiting on it — say so if one exists.
7. **Freshness.** `list` prints the engine's current rows: `WatchBoard` pushes
   after every sync cycle, status refresh and dispatch, so there is no sync
   command to run first. `wait` holds the same subscription open, so it answers
   as soon as the answer is true.
8. **After releasing work, do not fall silent about it.** That leaves the human
   to notice the agent finished and to prompt you. Either `wait` for it, or say
   plainly that you are leaving it running and that nothing will tell you when
   it is done.
9. **Never dispatch speculatively.** Releasing work starts a real agent in a
   real repo that commits and opens PRs. A human keypress — or an explicit
   instruction — releases tasks. Reading the board is always safe; dispatching
   is not.

**Reviewing a pull request is how you reach the agent that wrote it.** The board
delivers new comments on an open PR back into the chat that produced it — the
agent is still sitting there with the whole task in context, because a task in
`review` keeps its chat. Issue comments, inline comments on the diff, and review
submissions all arrive; `changes requested` is the clearest. So say what is
wrong on the pull request, rather than describing it to a human to relay.

Three things follow from how the loop is kept closed:

- Only an **idle** agent is woken. Feedback left while it is working is
  delivered when it settles, not on top of its turn.
- Each comment is delivered **once**. Editing a comment does not resend it;
  write a new one.
- If its chat is gone — you archived it, or the session ended — nothing is
  delivered and nothing is re-dispatched. The review then waits on the pull
  request for whoever opens it, and the board log says so once.

**If you were dispatched by the board, commit your work.** The board has no
callback: it decides an attempt is finished by seeing your run end with either
an open PR or commits on the attempt branch. Work left uncommitted in the
worktree when you stop reads as an agent that did nothing, and the row sits in
`working` until a human notices. Commit even when you are not opening a PR.

The two artifacts are not weighed the same. A pull request is your own
statement that you are finished, so it settles the attempt promptly. Commits
are not — you were told to make them mid-flight — so the board waits longer
before settling on them. Open the PR when you want the row to move promptly.

**A settle is not final.** If the board closes your attempt while you are still
working, it notices: a closed attempt whose chat is still working is re-opened,
the row goes back to `working`, and the count of times that happened is kept on
the attempt (`reopened`). You do not need to do anything about it, and it is
not recorded as a retry — nothing was re-dispatched.

**Attempts are bounded by the clock.** Each route sets a `max_duration` (two
hours by default). Past it the board says so in your chat once, naming how long
you have left, and then cancels the attempt and closes it `failed` with a
comment upstream. That message is not decoration: commit what you have and open
a pull request while you still can, and if you are going round in circles, say
so in the PR description rather than spending the remaining minutes on another
lap. Finishing inside the grace settles the attempt `done` on your artifacts as
normal — the cap only takes what nothing else has closed.

`comet-board doctor` explains a board that looks wrong: missing keys,
unreachable repos, routes pointing at spaces that do not exist, an engine that
is not listening. Prefer it to guessing.

Source lives in this repo (`apps/board-cli`, `crates/board`); `docs/BOARD.md`
maps the port, and herdr-board's README documents the inherited behavior.
<!-- END comet-board conventions -->
