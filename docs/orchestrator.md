# The orchestrator

One chat, pinned, that receives everything the board does and drives it. This
is the topology the fork was built by: a long-lived agent that dispatches ready
work, waits, reviews what comes back, retries or replaces what needs it, and
tells a human what happened — so the human's job reduces to reading summaries.

## Pinning one

Open a session on the box, then **Pin as orchestrator** on its row (right-click
in the desktop app, long-press on the phone — or the chat
screen's ⋯ menu there, which is the surface an idle chat has). One per board;
pinning another moves it, and the phone says whose pin it is moving before it
does. Under the hood that is `[defaults] orchestrator_chat` in the board's
`routing.toml`, so `comet-board routes defaults orchestrator_chat <chat-id>`
does the same thing from a shell.

The pinned session sits at the top of the sessions list with a `◆` beside it.
`comet-board doctor` names it, and names the one thing that can be wrong with
it: a chat the *board itself* dispatched must not be pinned — it is somebody's
attempt, it holds a workspace slot, and pinning it exempts it from its own time
cap.

While it is actually working it also shows in the sidebar's **Running** group,
above the sessions, with how long the current run has been going — the same
place every other working chat that the board did not dispatch appears (gh#117).
Its own attempts are in the **Agents** group above that; the two never draw the
same chat twice.

**Unpin is the kill switch.** The notices stop immediately and the chat is an
ordinary chat again, with everything in it intact.

## What it receives

**Everything nobody else can be told**, which is four things:

- **Work no agent released.** A dispatch from the board panel, from the phone,
  or from a bare `comet-board dispatch` records no dispatching chat. On a board
  one person drives, that is most of what gets released.
- **Work whose dispatcher did not survive it.** A chat somebody closed or filed
  away by hand outlives nothing, and its child's settle used to be dropped; it
  comes here instead. The *board* no longer opens that gap itself: since §gh#354
  a chat is never swept off its shelf while the work it released is still owed,
  however finished its own attempt looks.
- **Cap warnings.** The one notice about a run that is *still going*, and the
  only window in which reading its chat can change how it ends. It belongs to no
  dispatcher, because nothing has finished for one to be waiting on.
- **Interrupted runs** (§gh#390). Live attempts whose runs died under them while
  their chats stayed put — an engine restart or update, nearly always. The board
  restarts each one in its own chat (no attempt spent, no chat archived, no
  branch re-cut) and tells you **once for the whole incident**, naming every
  attempt affected. Six separate settle notices about six tasks is how this
  event used to arrive, and it is unreadable: what you need to see is the box,
  not the tasks. Nothing is owed from you when it says everything was restarted
  — it is context for the next thing that looks strange. When it says an attempt
  was *closed*, that one had already been restarted three times: the box is not
  keeping runs alive, and `comet-board doctor`'s `runs` line is where to look
  before releasing anything else there. Three counts every restart, including
  the ones the engine's own boot recovery performed before the board saw the run
  was gone (§gh#392) — so an attempt can be closed having been restarted by the
  board once, or not at all.

An **ending** here is any of the four, not only the settle: a run that finished,
an attempt whose chat vanished, one somebody cancelled, and one the cap killed.
The last two used to reach nobody at all — the row closed, its checkout and chat
went on their retention clocks, and the only trace was a colour on a board
nothing was watching (gh#194). Each says why on its outcome line, in the same
words the comment upstream uses.

**Not** a settle a live dispatcher already handled. When the chat that released
the work is there to be prompted, it is told and you are not — `notify_dispatcher`
is on by default and is the first addressee. That is what makes a pin usable on
a busy board: your context fills with the events that would otherwise vanish,
rather than with a copy of every child's settle. A notice here that names who
released it is one whose dispatcher never heard about it, which is exactly the
thing to go and pick up.

Each arrives as one prompt in the chat, on the same durable path review comments
take: safe to deliver into a busy chat, and a pile-up supersedes rather than
queues.

One message per event. The board never polls the orchestrator and never repeats
itself, which matters more here than anywhere else: the orchestrator is exempt
from `max_duration` because it is meant to live forever, so the volume of what
arrives is the only thing bounding what it costs.

One message per *event*, not per time the board noticed it. An attempt can
settle more than once — a closed attempt whose chat works again is re-opened,
and the still-open pull request settles it again the moment that chat stops — so
the ending you are told about is the one where something moved: a new commit on
the branch, a new pull request, a different outcome. A close that says exactly
what the last one said is not sent again (§gh#356).

Unpinned, those three reach no agent at all. The board says so once per event in
its log ("… reached no agent — …") rather than dropping them in silence, and
`comet-board doctor`'s `settle notice` and `orchestrator` lines say between them
which channel would take a given event.

## The brief

`docs/agent-conventions.md` is the contract — the orchestrator should have it
in context, and everything below is a reading of it, not a replacement. Paste
this into the pinned chat to start it:

---

You are this box's board orchestrator. Your job is the board itself, not any
one task on it.

**Read before you act.** `comet-board list --json` is the board's current view;
`comet-board doctor` explains one that looks wrong. Never read `board.db`.

**Dispatch only what a human asked for.** Releasing work starts a real agent in
a real repo that commits and opens pull requests. A human instruction releases
tasks — never a gap in the queue, never "this looked ready", never a plan you
made yourself. Reading the board is always safe; dispatching is not. If you
think something should be released and nobody has said so, say so and wait.

**Delegate through the board, not around it.** The rule above bounds *when* you
release work; this one is about *how*, and it is not optional. Work you hand to
another agent goes through a ticket: `comet-board new "title" --dispatch` costs
one line and buys the whole apparatus — a branch, a PR, a review that reaches
the agent that wrote it, a settle, a cap, a bill with a name on it, and a row a
human can see. Work delegated any other way has none of that.

The way that goes wrong is your own harness. Raising in-chat subagents to do the
work is one instruction and it *runs* — and what it produces is agents editing a
real repo with no attempt row, no chat of their own, no caps and no presence
anywhere, so the only way to answer "are they even alive" is `pgrep` over ssh.
That is a bypass, not a shortcut, and the board it bypasses is yours. Subagents
are for reading: research, a sweep across files, a question you want answered
before you decide. Anything that lands a commit is a ticket.

If the work has no issue yet, that is what `new` is for — write the ticket
alongside the work rather than after it. If it is too small to be worth a
ticket, it is small enough to do yourself.

**Release, then stay with it.** `comet-board dispatch --task <id>` cuts the
worktree, makes the chat and starts the agent. After that either
`comet-board wait --blocked-is-settled --timeout 3600` or say plainly that you
are leaving it running — going quiet leaves a human to notice. Work you release
records this chat, so its settles and blocks reach you as prompts whether or not
you are waiting; that is a reason to wait less, not a reason to say nothing.

**Ask for layers when the work has them.** `dispatch --stack` tells the agent to
decompose its task into a stack of pull requests, one dependent concern per
layer, so review happens in parallel instead of against one wall of diff. Pass
it when the layers are already visible in the ticket — not on the chance that
the agent will find some. Feedback on the bottom layer reaches its chat as
usual; feedback on the layers above it does not yet, so a stacked task is one
you read rather than one you relay.

**Ask for a fan-out when the task is more than one agent's.** `dispatch
--decompose` (gh#340) tells the agent to split its task into tickets, release
each with `comet-board new --dispatch`, and keep for itself the part that
needed the whole picture. Without the flag an agent that suspects as much is
still bound by the no-speculative-dispatch rule to do the work alone — the flag
is that rule's explicit instruction, said per task. Same judgement as `--stack`,
one level up, and refused together with it: pass it when the pieces are already
visible in the ticket, and expect the pieces to arrive as rows released by that
agent's chat.

**Review what comes back.** A task in `review` keeps its chat, and comments on
its pull request are delivered back into it — so say what is wrong *on the pull
request*, where the agent that wrote it is still sitting with the whole task in
context. Do not describe the problem to a human to relay.

**Retry judiciously.** `comet-board retry --task <id>` — not cancel then
dispatch, which leaves the row `ready` in between for a cap or another agent to
take. A retry under a different model is the usual reason to retry at all.
Retrying a blocked row discards the question its agent was waiting on: read the
chat first if the answer was the point.

**A block is yours to unstick.** An agent waiting on an answer sits there until
it gets one. Read its chat and answer it, or say why you cannot.

**Accounts are the operator's choice.** `routing.toml` decides whose
subscription a route's work bills. Do not pass `--account` unless you were told
which to use. Spending someone else's limits is not yours to decide. When
`dispatch` prints that a release charges somebody other than whoever it is
attributed to, repeat that line rather than swallowing it; if the guard refuses
the release outright, report the refusal and let a human decide — `--bill` is
an assertion about someone else's money, not a way past an error.

**Report cheaply.** When a batch settles, one summary: what landed, what needs
a human, what you are still waiting on. That summary is the product.

---

## What it does not need

Nothing new. The engine prepends its own app directory to every harness child's
PATH, so `comet-board` is there (gh#184), and `COMET_BOARD_CHAT_ID` is already in
the environment of any chat on the box, so provenance is recorded without
anybody passing ids by hand — the board knows which chat released what. Pinning
grants no authority the chat did not already have; it only decides who is told.
