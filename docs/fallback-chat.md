# Where stray notices go

Most of what the board has to say has an obvious addressee: the chat that
released the work. `notify_dispatcher` is on by default and is the **first**
addressee, so a settle or a block goes back to the agent whose plan that task
was a step in.

Some of it has none. That is what this is for — one chat, named by `[defaults]
fallback_chat`, that takes the notices nobody else can be told about.

**It is an address, not a role.** Naming a chat here does not appoint it to
anything. It is not the board's driver, it is owed no brief, it is exempt from
nothing, and clearing it takes nothing away from the chat but the notices.
Driving the board is a different thing entirely, and nothing configures it:
see [Driving the board](#driving-the-board) below.

## Naming one

Open a session on the box, then **Send board notices here** on its row
(right-click in the desktop app, long-press on the phone — or the chat screen's
⋯ menu there, which is the surface an idle chat has). One per board; naming
another moves it, and the phone says whose it is moving before it does. Under
the hood that is `[defaults] fallback_chat` in the board's `routing.toml`, so
`comet-board routes defaults fallback_chat <chat-id>` does the same thing from
a shell.

The chat sits at the top of the sessions list with a `◆` beside it.
`comet-board doctor`'s `fallback` line names it, and names the one thing that
can be wrong with it: a chat the *board itself* dispatched should not be it —
that chat is somebody's attempt, with a task of its own and a context spent on
it, and the board's news about the rest of the board is not something it has
room for.

**Clearing it is the kill switch.** The notices stop immediately and everything
in the chat is intact.

> The key was called `orchestrator_chat` before §gh#348 and is still read under
> that name, so an existing `routing.toml` keeps working. Any write from any
> surface converts it: the board writes the current spelling and clears the old
> line in the same edit, so a file never says two things about one setting. The
> RPC method behind the `◆` still has its old name on the wire, deliberately —
> a method name is an identifier, and renaming it would blank the mark on every
> phone installed before the rename.

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
the work is there to be prompted, it is told and this chat is not — that is what
keeps the address survivable on a busy board: the context fills with the events
that would otherwise vanish, rather than with a copy of every child's settle. A
notice here that names who released it is one whose dispatcher never heard about
it, which is exactly the thing to go and pick up.

Each arrives as one prompt in the chat, on the same durable path review comments
take: safe to deliver into a busy chat, and a pile-up supersedes rather than
queues.

One message per event. The board never polls this chat and never repeats itself.
One message per *event*, too, not per time the board noticed it: an attempt can
settle more than once — a closed attempt whose chat works again is re-opened,
and the still-open pull request settles it again the moment that chat stops — so
the ending you are told about is the one where something moved: a new commit on
the branch, a new pull request, a different outcome. A close that says exactly
what the last one said is not sent again (§gh#356).

Nothing here is exempt from the clock. Before §gh#348 an attempt running in this
chat was exempt from `max_duration`, on the theory that an orchestrator is meant
to live forever; all that exemption could ever do was keep a real attempt alive
past every cap in the one configuration `doctor` refuses. An ordinary chat you
opened yourself has no attempt and no cap to be exempt from.

With nothing named, those notices reach no agent at all. The board says so once
per event in its log ("… reached no agent — …") rather than dropping them in
silence, and `comet-board doctor`'s `settle notice` and `fallback` lines say
between them which channel would take a given event.

## Driving the board

There is no such thing as being appointed to drive the board, and there never
needed to be. **A chat that dispatches is already a dispatcher**: the dispatch
records `COMET_BOARD_CHAT_ID`, settles and blocks for that work come back to it
as prompts, the shelf sweep will not archive it while the work it released is
still owed (§gh#354), and it holds its own state in its own conversation. That
is the whole topology — a parent that spins out children and hears from them —
and it arrives with the first `comet-board dispatch`, with nothing set anywhere.

The evidence is how this repo has been built: sessions that dispatched twenty-odd
tasks, reviewed and merged every one, filed issues and cut releases, with no
declaration of any kind. gh#348 is the ticket that noticed the role was
describing something that happens anyway.

So the guidance for driving lives where a driving agent will actually read it —
in the `comet-board` skill (`assets/skills/comet-board/SKILL.md`, installed by
`comet-board skill install`) and in `docs/agent-conventions.md`, which is the
contract. In short, and in full there:

- **Read before you act.** `comet-board list --json` is the board's current
  view; `comet-board doctor` explains one that looks wrong. Never read
  `board.db`.
- **The board is the state, so re-read rather than remember.** Everything a
  driver needs — what is ready, working, blocked, in review — is one command
  away, and it is true after a compaction, a restart, or somebody else's
  dispatch. A driver that re-reads has no context problem to solve.
- **Never dispatch speculatively.** A human keypress or an explicit instruction
  releases work. A gap in the queue is not one.
- **Delegate through the board, not around it.** `comet-board new "title"
  --dispatch` buys a branch, a pull request, a review that reaches the agent
  that wrote it, a cap and a bill with a name on it. In-chat subagents buy none
  of that: they are for reading.
- **Release, then stay with it** — `comet-board wait`, or say plainly that you
  are leaving it running. **Review on the pull request**, where the agent that
  wrote it is still sitting with the whole task in context. **A block is yours
  to unstick.** **Report cheaply**: one summary when a batch settles.

## What none of it needs

Nothing new. The engine prepends its own app directory to every harness child's
PATH, so `comet-board` is there (gh#184), and `COMET_BOARD_CHAT_ID` is already
in the environment of any chat on the box, so provenance is recorded without
anybody passing ids by hand — the board knows which chat released what. Naming
a fallback chat grants no authority the chat did not already have; it only
decides who is told.
