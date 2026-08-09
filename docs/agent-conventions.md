# Board conventions for coding agents

The canonical text every runtime's agent should have in context. Ported from
herdr-board with the names swapped (herdr-board → comet-board, pane → chat);
the markers are kept so an installer can write it into each runtime's global
instruction file and a correction here reaches all of them.

Nothing below is runtime-specific.

<!-- BEGIN comet-board conventions -->
## The comet-board task board

One global queue across every space: Linear issues and GitHub issues in, comet
chats running coding agents out. The engine puts `comet-board` on the PATH of
every agent it runs — the copy it shipped with — and `gh` is authenticated once
per user; neither is specific to the agent you are. If `comet-board` is
nevertheless not found, say so and stop rather than working on without it: an
agent that quietly skips the board leaves its work with no row, no provenance
and no one able to see it (`comet-board doctor` names the fault as **agent
PATH**).

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

**Work you delegate goes through the board.** A ticket buys a branch, a PR, a
review that reaches the agent that wrote it, a settle, a cap and a bill with a
name on it. Delegating any other way — your own harness's in-chat subagents
being the easy one — buys none of it: agents editing a real repo with no row,
no caps, and no presence in any frontend, so nobody can see that they are
running at all. Subagents are for reading (research, a sweep across files, a
question to answer before deciding); anything that lands a commit is a ticket.

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
specific work, `--state` to replace the settled set entirely. It exits non-zero
on timeout, and refuses when nothing is in flight.

**A child that asks a question settles nothing.** A blocked agent is waiting on
input and will sit there until it gets some, so a plain `wait` on it holds until
its timeout. Add `--blocked-is-settled` and `wait` also returns when a watched
task goes `blocked` — that is how you get called back to answer it, instead of
discovering the question when your own timeout expires:

```bash
comet-board wait --task linear:AGE-14 --blocked-is-settled --timeout 3600
```

It adds to the settled states rather than replacing them, so the same call still
returns on `review`, `failed` and `done`. Wait this way whenever the work you
released can come back with a question — which is most work.

Each row: `id`, `identifier`, `title`, `state`, `source`, `url`, `labels`,
`route`, `workspace`, `runtime`, `chat_id`, `pr_url`, `pr_number`, `branch`,
`dispatched_by`, `dispatched_by_chat`, `dispatched_by_user`, `last_outcome`,
`last_outcome_at`,
`attempts`, `dispatchable`, `gone`, `reopened`, `account`, `billed_to`.
`workspace` names a
comet *space* — the field keeps herdr-board's spelling so ported tooling reads
it. `account` is the agent login whose subscription the row's attempt spends
(the route's default before anything has run); null is the device's own CLI
login, which is every row on a single-account box. `billed_to` is that account
resolved to an email — whose subscription it actually is — recorded when the
attempt was released, and null on a row nothing has run on. Read it against
`dispatched_by_user`: different values mean the run is charged to somebody other
than whoever released it.

`attempts`, `dispatchable`, `gone`, `reopened`, `account`,
`max_duration_secs`. `workspace` names a
comet *space* — the field keeps herdr-board's spelling so ported tooling reads
it. `account` is the agent login whose subscription the row's attempt spends
(the route's default before anything has run); null is the device's own CLI
login, which is every row on a single-account box.
`max_duration_secs` is the wall-clock cap one attempt on this row gets — the
route's `max_duration` resolved against `[defaults]`, null when the route is
uncapped. Read against `started_at` it is how long a running agent has left;
it is on the row because the routing config lives on the board's host, and a
caller reading a relayed board has never seen it.
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
   `dispatch` prints a line on stderr when a release charges somebody other
   than whoever it is attributed to — repeat it, do not swallow it. Under
   `[defaults] billing_guard = "require-own"` such a release is *refused*
   instead; `--bill <slot-or-email>` is the acknowledgement that overrides the
   refusal, and passing it is a decision about someone else's money. Report the
   refusal and let a human make it.
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
7. **Retry** with `comet-board retry --task <id>`, not cancel-then-dispatch. On
   a `blocked` row it ends the live attempt and releases a fresh one in the same
   call; done as two commands the row is `ready` in between and a concurrency
   cap or another agent can take the slot. On a `failed` or `ready` row nothing
   is live and it is an ordinary dispatch. It takes the same `--runtime`,
   `--model` and `--account` overrides as `dispatch` — a retry under a different
   model is the usual reason to retry at all. Retrying a blocked row **discards
   the question its agent was waiting on**: read the chat first if the answer
   was the point.
8. **Freshness.** `list` prints the engine's current rows: `WatchBoard` pushes
   after every sync cycle, status refresh and dispatch, so there is no sync
   command to run first. `wait` holds the same subscription open, so it answers
   as soon as the answer is true.
9. **After releasing work, do not fall silent about it.** That leaves the human
   to notice the agent finished and to prompt you. Either `wait` for it, or say
   plainly that you are leaving it running. A board configured with
   `notify_dispatcher` will prompt you in this chat when work you released
   settles, and a board that has pinned this chat as its **orchestrator** will
   prompt you about every settle, block, orphan and cap warning on the board —
   including work you did not release. Both are off by default and you cannot
   tell which is on from here, so never promise that you will be woken. Note also that `wait` does **not** return on
   `blocked` by default: an agent that stops to ask a question holds its
   attempt open, and a plain `wait` on it hangs until somebody answers. Pass
   `--blocked-is-settled` to be called back on the question too; either way the
   blocked agent comments on its own issue, which is the human's signal, not
   yours.
10. **Never dispatch speculatively.** Releasing work starts a real agent in a
    real repo that commits and opens PRs. A human keypress — or an explicit
    instruction — releases tasks. Reading the board is always safe; dispatching
    is not.

**One chat may be pinned as the board's orchestrator.** If this one is, you
receive a `comet-board:` prompt for every settle, block, orphan and cap warning
on the board — not only for work you released — and it is one message per
event, never a stream. Everything else about you is unchanged: you hold no
workspace slot, everything you release counts against the caps like anyone's,
and you bill whatever account your chat names. You are exempt from
`max_duration` and from `archive_chats` alike, because you are meant to outlive
every attempt — the shelf sweep never files the pinned chat away. That makes
restraint your responsibility rather than the clock's: never poll the board in
a loop, and never dispatch because a queue looked empty. Being told about work
is not being told to release any. `docs/orchestrator.md` is the brief.

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

**If you were dispatched by the board, commit your work and push it.** The
board has no callback: it decides an attempt is finished by seeing your run end
with either an open PR or commits *on origin* for the attempt branch. Work left
uncommitted, or committed and never pushed, reads as an agent that is still
going — the row sits in `working` until the clock cap takes it or a human
notices. Commit and push even when you are not opening a PR.

Pushing is the part that is easy to skip and the part that matters: a commit in
your worktree is on one box, and a row that said `review` about it would send
somebody to read a branch that is not there. The board log names the branch when
it finds an attempt in that state. If `git push` fails, say so in the chat
rather than stopping quietly — a push nobody can make is a board problem, not
yours (`comet-board doctor` reports the credential).

The two artifacts are not weighed the same. A pull request is your own
statement that you are finished, so it settles the attempt promptly. Commits
are not — you were told to make them mid-flight — so they settle it only once
the run has genuinely ended. Open the PR when you want the row to move promptly.

**Screenshots go in the repo, not in a `raw.githubusercontent.com` link.** A
ticket that asks for screenshots in the PR description is asking for something
that keeps working, and the URL an agent reaches for first does not:
`https://raw.githubusercontent.com/<owner>/<repo>/<branch>/shot.png` is
unreadable without a token on a **private** repo — so it renders broken for
everyone, the author included — and it names a **branch**, which is deleted the
moment the PR merges. Both failures are silent: the markdown is well-formed and
the PR looks right until somebody opens it.

Commit the images on your branch and reference them with a **relative** path
from a markdown file that is also in the repo (`prototypes/v1/DESIGN-NOTES.md`
→ `![Variant A](screenshots/01.png)`). Relative paths resolve against whatever
ref the reader is on, private or not, and survive the branch. The PR body then
links to that file, and the images you added also render as image diffs in the
PR's own Files-changed tab. Inline images that live in the PR body itself
(`user-attachments`) come from a drag-and-drop upload — a human can make one,
you cannot, so do not write markup that pretends otherwise.

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

**Your chat is filed away once the work is over, not deleted.** As soon as the
task leaves the board — merged, closed upstream, or marked done — the board
archives the chat it dispatched you into; the checkout you worked in is
reclaimed on its own, week-long clock (`archive_chats` / `retain_worktrees`,
both per route). Never while
your attempt is live or blocked, and never while a pull request is still in
review: a chat in review is how the board delivers comments back to you, so it
outlives everything else. The transcript survives archiving, Settings →
Archived puts a chat back, and an attempt re-opened by the rule above brings its
own chat back with it. Nothing here needs anything from you; it is why a space's
sidebar shows what is current rather than everything that ever ran.

**The board may not be on this machine.** It lives on exactly one device —
usually an always-on box — and every verb reaches it over the relay. If a
command says this device's board is disabled or not running, name the host with
`--device <name-or-id>` (or set `COMET_BOARD_DEVICE` once for the shell) and the
same commands work unchanged: the dispatch cuts its worktree, makes its chat and
runs its agent on that device, not this one. Nothing else about the contract
changes, and `--device` is unnecessary wherever the board is local.

`comet-board doctor` explains a board that looks wrong: missing keys,
unreachable repos, routes pointing at spaces that do not exist, an engine that
is not listening. Prefer it to guessing.

Source lives in this repo (`apps/board-cli`, `crates/board`); `docs/BOARD.md`
maps the port, and herdr-board's README documents the inherited behavior. The
short form of this text is the `comet-board` skill, which ships inside the
binary and is what a Claude session discovers on its own — `comet-board skill
install` puts it where sessions on this machine will find it (`docs/skill.md`).
<!-- END comet-board conventions -->
