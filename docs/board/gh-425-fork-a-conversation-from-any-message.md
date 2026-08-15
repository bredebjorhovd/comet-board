# Fork a conversation from any message — **done** (gh#425)

A comet chat has one linear history. Asking a second agent what it makes of the
same work, trying a competing approach, or continuing from a decision three
turns back all meant the same thing: open a new chat and carry the context by
hand, or steer the original and lose the branch point.

Landed as one host-side verb — `ForkChat` (`crates/engine/src/fork.rs`) — plus
the lineage it writes onto the chat row (`comet_proto::ChatLineage`), the fork
menu in the gpui shell (`crates/ui/src/shell/fork.rs`), and the strip that says
"this is a fork" on desktop and iPhone.

### Two destinations, never one with a checkbox

- **In this checkout** (`ForkCheckout::Shared`) — another conversation over the
  *same working directory*. Nothing is created on disk and nothing is reclaimed;
  a reviewer here watches the original agent's edits appear underneath it in
  explicit read-only mode.
- **In a new worktree** (`ForkCheckout::Isolated`) — its own checkout on its own
  `comet/…` branch, cut from the commit the source chat is standing on
  (`Repos::head_sha`, so it lands where the source *is* rather than where a
  branch name has since moved). Two agents, two trees, one repo.

They get separate rows, separate sentences and separate tints, and the dialog
opens with **neither** selected: a default would make one of them the thing that
happens when you are not paying attention, and the difference between them is
exactly what somebody not paying attention needs to notice. The words are in
`comet_proto::view::fork` so the desktop and phone cannot invent two
vocabularies for one distinction.

### An independent copy, said out loud

A provider session can only be picked up where it was left, by the harness it
belongs to, in the directory it was created under. Those four facts —
`ForkPoint { at_tail, same_harness, same_checkout, session_id }` — go through
one function, `comet_proto::decide_context`, which the host runs and the fork
menu *also* runs to preview the answer. Two implementations would be how a
preview comes to promise memory the fork does not have.

- **Visible transcript copy**: `comet_doc::build_handoff` renders the messages
  up to the fork point — text, one line per tool call, errors — bounded at 24
  KiB, newest kept, and never silently: the text says how many older messages
  were dropped. It lands in *two* places, deliberately: as a `system` message in
  the fork's own doc (so the reader can check what the agent was given) and in
  a host-local handoff file (`{data}/orgs/…/handoff/{chat}.md`) that prefixes
  the fork's **first** prompt. It remains claimed until `SessionStarted` is
  durably journaled and is only then acknowledged, so restart cannot lose it
  before an accepted spawn or repeat it afterward. The combined first payload
  is capped at 32 KiB; an over-limit prompt is refused before any user-message,
  run, or status side effect, so copied context is never silently discarded and
  then reported as carried. Their transcript message stays unchanged.
- Every copy records **why** (`CopyReason`), and every reason is a fact about
  the source chat a reader could check for themselves.

Even at the tail, Comet copies: a provider resume id owns one mutable session,
not a clone operation, and two chats must never continue that same session.

The legacy `NativeResume` wire value remains decode-only for old synced rows;
`decide_context` never constructs it.

### Durable creation that a restart can finish

Before `git worktree add`, the host persists a staged intent naming the exact
chat, repository, path, and branch it owns. Before the chat row is observable,
it also persists an immutable execution-policy capsule binding the host, exact
registered checkout, config, and lineage — not a replayable copy of mutable
title/archive/branch/cache fields. Every turn proves the checkout is still in
the host registry before comparing its identity. A staged or
fork-shaped row without committed authority fails closed; the optional first
Run remains pending until commit. It acknowledges creation only after the
workspace row/config/lineage, session snapshot, handoff, and optional first
command are durable. One owner gate makes publication and reseed indivisible,
and the policy fields are rechecked at the point of commit. Startup reclaims
every uncommitted intent, finishes committed cleanup idempotently, and discards
orphan authority rather than recreating a deleted chat. Every
git, filesystem, registration, snapshot, and handoff cleanup result is checked;
a rollback failure is returned alongside the original error rather than logged
as success, and one failed request never runs the global recovery sweep over a
concurrent fork.

A rejected destination resume writes a journal-owned sequence tombstone before
the fresh-session retry. On restart it dominates every older provider id while
a later successful destination `SessionStarted` remains resumable.

### What a fork does not inherit

The complete board authority tuple is cleared: `push_repo`, `push_contract`,
`git_author`, `turn_limits`, and route MCP servers. Shared-checkout forks are
read-only, so clearing board authority cannot fall through to ambient writes.

And it is excluded from `CometRuntime::review_candidates`. A shared-checkout
fork has the same branch and the same repo as its source, and a transcript copy
puts the source's pull-request URL into the fork's own journal — so on every
signal the adoption sweep uses it looks exactly like the chat that opened the
pull request, and being newer it would win `explicit.last()` and take the
attempt's chat link with it. Review has to reach the conversation that did the
work.

### Where it shows

- **Desktop**: "Fork from here" appears in the per-message hover strip (the same
  reserved lane the timestamp uses, so revealing it never shifts the
  virtualizer), and only on completed user or assistant messages. System notices
  never advertise an action the host refuses. The menu offers the two
  destinations, a harness and a
  model override (asked of the *host* device: "available" is a fact about the
  box the fork will run on), and the predicted context mode. On success the
  prediction is replaced by what actually happened and the new chat is selected.
- **Phone**: parsed off the synced chat row (`forkedFrom`) and rendered as a
  strip above the transcript, tappable for the sentence behind the tags.

### Deliberately not here

- No DAG editor, and no merging two branches of a conversation back together.
- No fork of a chat this device does not host: the transcript, the checkout and
  the provider session are all over there. `ForkChat` is forwardable, so the
  laptop's menu drives the box's fork — it is refused, with the host named, if
  it lands somewhere else.
- No provider-session cloning claim. Every new fork is an independent visible
  transcript copy, including same-harness forks taken at the tail.

Closes #425
