# The composer points into one checkout and plans the turns after this one — **designed** (gh#424)

Comet already has the two durable paths this design needs. A chat row names the
checkout on the device that hosts the chat, and a session document carries an
append-only command ledger between every viewer and that host. This design
deepens those modules instead of adding a browser-side filesystem or a second
queue.

This is a clean-room Comet design. The Zuse links on gh#424 informed the
product question only. No Zuse source, UI, strings, schema, assets, or tests are
reused here. The contracts below follow Comet's existing workspace rows,
device forwarding, `SessionDoc`, command evaluation, run requests, image
uploads, desktop composer, and iOS session store.

## The decision

An `@` selection is a **live path reference**: a relative path and kind bound
to an opaque identity for the chat's exact checkout. It is not a snapshot, an
attachment, a content hash, or permission to read anything. The host searches
and resolves it against the checkout recorded for the chat. At execution it
must still exist with the selected kind and remain inside that checkout. A
changed file is intentionally the same reference; a missing, kind-changed, or
checkout-mismatched path rejects the instruction with a visible explanation.

A directory reference names where the agent should inspect. It never expands
into a list, uploads a tree, or causes Comet to read its contents into the
workspace or session document.

A follow-up is a durable `queue` command in the existing session command
ledger. Add, edit, move, remove, pause, resume, and run-next are operations on
one host-evaluated queue fold. The ledger remains the source of truth. Clients
may show the deterministic fold including pending operations immediately, but
they do not persist another queue and they roll a rejected operation out of
the fold.

The split at submit is explicit:

| Chat state | User choice | Durable command | Outcome |
| --- | --- | --- | --- |
| idle | Send | `run` | starts an ordinary turn |
| active | Steer this turn | `steer` | enters the live run's steering mailbox |
| active | Do after this turn | `queue.add` | appears at the end of the follow-up queue |
| active or idle | Run next | `queue.runNext` | starts the named queue head when execution is safe |
| active | Stop | `interrupt` | interrupts this run; it does not clear the queue |

There is no heuristic that turns typed text into a steer or a follow-up. While
a run is active, the composer exposes the two named delivery choices. Its
primary up-arrow retains today's **Steer this turn** behavior; **Do after this
turn** is a separate action beside it. When idle, the up-arrow remains Send.

## One checkout, chosen by the host

### Checkout identity

The checkout resolver is one deep module shared by path search and command
execution:

```text
CheckoutContext::for_chat(chat_id) -> CheckoutContext

CheckoutContext
  id                 opaque, stable while this chat names this checkout
  display_name       branch/worktree label safe for clients
  root               host-only absolute canonical path

CheckoutContext::search(query) -> ContextSearch
CheckoutContext::resolve(reference) -> ResolvedContext | ContextError
```

`root` never crosses the checkout module's interface to a remote client and is
never stored in a context reference. Existing workspace chat metadata may
continue to contain `cwd`; this design does not duplicate it into messages or
queue rows.

For an existing chat, `for_chat` uses the same precedence as run execution:
the session document's `hostDeviceId`, then the host's workspace chat row and
its `cwd`. It does not trust a viewer-supplied cwd. Calls naming a chat are
forwarded through the existing device RPC route to that host, just like
`QueueCommand`.

For the new-chat composer, the request names the selected space and ref/worktree
choice. It is routed to that space's device. The returned checkout id is sent
back in `createChat` and the first `run`; creation succeeds only if the resolved
cwd still has that identity. This closes the gap in which device A picks a path
but device B silently creates the chat in a different checkout.

The id is an opaque host-minted digest over a version byte, device id, and the
canonical checkout root. It is equality-only: clients must not parse it. Moving
or replacing a checkout changes the id. Switching a main checkout's branch in
place does not change the id because the reference means “this checkout at
execution time,” not “this commit.” A worktree and the main checkout always
have different ids even when their HEADs match.

`RunRequest.cwd` remains for harness execution and compatibility. The host
still overwrites a guessed cwd from its chat row. `checkoutId` adds the missing
fence: a request carrying references is rejected if its id does not equal the
host-derived identity. A request without references may omit it, preserving
old clients.

### Typed reference contract

The wire type belongs in `comet-proto`, so desktop, engine RPC, command
payloads, and generated/handwritten mobile mirrors share one meaning:

```text
ContextReference
  kind               file | directory
  path               normalized checkout-relative UTF-8 path
  checkout_id        opaque CheckoutContext id
```

`RunRequest` gains default-empty `contextReferences`. `Steer` gains the same
field. A queued follow-up stores the same typed values. The reference never
contains bytes, a preview, an absolute path, size, modification time, hash,
git object id, or a recursively enumerated directory.

The prompt delivered to a harness gets one engine-authored trailer after the
user's text and after today's image attachment trailer:

```text
Context references (paths relative to this checkout):
- [file] crates/doc/src/commands.rs
- [directory] crates/ui/src
```

This is a harness adapter detail, not the durable representation. The command
and transcript retain typed references, so another frontend never has to parse
decorated prompt text to recover a chip. Harnesses that later gain native
resource parts can change their adapter without changing the session contract.

User transcript entries gain a `context` message part containing one
`ContextReference`. The text part remains exactly what the user typed. The
host creates the typed parts only when it accepts the command, using the same
client-minted message id that deduplicates today's optimistic echo. Old readers
degrade unknown part kinds to an inert empty text part under the existing
compatibility policy; upgraded readers render the chip.

### Search without an index

The host adds one forwardable unary RPC:

```text
SearchContextFiles {
  chatId? | spaceId?,
  query,
  limit?
}

ContextSearch {
  checkoutId?,
  matches: ContextMatch[],
  truncated
}
```

The engine performs a fresh walk rooted at the validated checkout. It does not
install a watcher or build an index. A fixed heavy-directory list excludes
`.git`, dependency/build caches, and similar trees; it does not parse gitignore.

One request is bounded by all of:

- 20,000 visited directory entries;
- 20 returned matches by default (caller limit capped at 100);
- depth 12.

The entry or result limit sets `truncated`. There is no cursor or index. The
desktop debounces for 90 ms and discards replies whose chat/query key is stale.

Matching is case-insensitive subsequence scoring over the relative path with
bonuses for basename, component starts, and exact prefix. Results sort by
score, then fewer components, then bytewise path. Directories and files share
the list; kind is always explicit. This ranking is part of the module so all
clients show the same answer.

The walk uses `DirEntry::file_type`, so it does not descend into directory
symlinks; a symlink leaf may appear as a file result. Resolution rejects
absolute and parent-traversing paths, canonicalizes every existing ancestor,
and refuses any ancestor or final target outside the canonical checkout root.
A symlink whose canonical target remains inside the checkout is accepted. The
implementation does not promise file-vs-directory kind stability. It repeats
checkout and containment validation immediately before dispatch.

### Live, stale, and deleted

No content hash is captured. A hash would promise snapshot semantics without
carrying the snapshot, would make large-file selection expensive, and would
turn a directory into an undefined hash tree. Image attachments already own
the snapshot/upload use case.

Checkout mismatch and containment escape reject the command with the engine's
error text. A missing leaf is not fatal: the engine appends a visible note to
the prompt and records the missing count in the command resolution. This also
supports instructions that intentionally name a file the agent should create.

A file whose bytes changed is not stale. The chip continues to point at its
path and the agent sees the execution-time contents. That is the useful
default for long-running development and the only behavior consistent across
files and directories.

## The follow-up queue is a ledger fold

### Why it is not a list beside the ledger

A mutable Loro list written directly by every renderer would introduce a
second command plane, unclear ownership, and different conflict behavior on
desktop and iPhone. A host-only mutable list would make offline edits invisible
until reconnection. Instead, queue intent is expressed as immutable operations
in `commands`, and one pure fold derives the visible queue from those entries.

The module interface is deliberately small:

```text
queue::project(commands) -> QueueView
commands::evaluate_command(command, context) -> CommandDisposition
```

The engine and desktop use the Rust command shapes. Rust owns the normative
fold. Swift mirrors those wire fields and a small projection for offline
display; this slice has source-contract coverage for durable-write failure but
no cross-language shared queue fixture or parity runner. A future web or TUI
surface should consume the host projection rather than invent another reducer.

### Command shapes

`SessionCommandKind` gains `Queue` and `QueueControl`. They are immutable
session-ledger entries, separate from steer supersession:

```text
Queue { prompt, message_id, attachments[] }
QueueControl = Edit { target, prompt, context[] }
             | Move { target, after? }
             | Remove { target }
             | Clear | Pause | Resume | RunNext { target }
```

The command id is both the durable processed-ledger id and the queued row id
for `Queue`. Queue-aware RPC clients mint it before sending and retain it if
the transport retries that same gesture. The host refuses to append a second
entry with the same id. A later explicit click is a new gesture and therefore
a new command id.

The first release supports context references in queued rows. Image
attachments keep today's upload behavior and are represented by their existing
durable staged paths only after upload succeeds. The queue body calls those
`attachment_refs` to distinguish them from checkout context; it does not
change attachment storage or transcript formatting. Removing a queue row does
not immediately delete a content-addressed upload because another draft,
message, or row may still name it; existing upload retention remains owner.

Queue operations use the ordinary command TTL only until the host first
evaluates them. An applied `Add` creates a durable row by virtue of the ledger
fold and does not expire a day later. The item's age is not the expiry of the
operation that created it.

### Deterministic fold and convergence

Commands have a deterministic document order from Loro. Every client folds
that same ordered list. `Queue` appends a row. Edit is last-writer-wins for a
row still present. Remove and Clear affect rows already present at their fold
position. Move removes the target and inserts it after a live anchor; `None`
means the head, a self-anchor is a no-op, and an anchor concurrently removed
falls back to the tail. Pause/Resume are last-writer-wins. RunNext names one
currently live row and is cleared by projection if that row disappears.

This is deliberately not a CAS/conflict-reporting protocol. Concurrent edits
or moves converge by document order; they do not surface a rejected revision.
Idempotency is at the command boundary: a retry carrying the same client-owned
command id cannot append after an intervening operation and undo it. There are
no stored rank values, renderer-local mutations, or queue-specific tombstone
objects.

The projection returned by `WatchFollowupQueue` contains no absolute context
paths or file bytes. Existing image attachment paths retain today's transport
contract and are not treated as checkout context:

```text
QueueProjection
  paused
  run_next?
  rows[]

QueueRow
  id
  prompt
  context[]
  attachments[]
  message_id
  issued_at
  issued_by
  edited
```

Position is the row's index in the projection, not stored rank data. Clients
reorder by row ids and neighbour anchors.

### Starting the next turn

Queue operations and ordinary delivery remain distinct. An applied `Add` does
not itself call the harness. The host queue scheduler asks `next_action` after:

- a queue command changes;
- a run reaches a terminal or parked-between-turns state;
- the host opens or restores a session document;
- resume or run-next is applied.

Automatic delivery is allowed when no run is live, the queue is unpaused, and
the row is the projected head. `RunNext` may name one live row while paused;
while that target exists every non-target row defers. It still waits for the
current run to end and never steers or interrupts it.

The queued command itself uses the recoverable Run path. It remains Pending
and is claimed in-process while `SessionsEngine::dispatch` accepts the turn;
only then is its processed bit and Applied outcome written. A crash before
dispatch therefore leaves the same durable Queue command available after
restart. A dispatch rejection records Rejected and removes the non-pending row
from the projection; its reason remains in the command ledger.

The derived Run rebuilds model, reasoning, sandbox, and resume from the chat's
current row exactly as today's dead-steer fallback does. A queued follow-up
does not freeze model configuration. It does freeze the user's prompt,
references, and uploaded image identities.

Stopping a run does not consume or clear rows. If rows exist, the Interrupt
entry projects the queue as stopped/paused until an explicit Resume. With no
rows it leaves no latent pause behind.

### RPC surface

Clients append queue operations through the existing `QueueCommand` RPC. They
do not need list/add/update/delete/reorder RPCs that bypass the document.

One stream is added for callers that do not host a Loro reducer:

```text
WatchFollowupQueue { chatId } -> QueueProjection
```

It is a derived watch over the session document and turn-state watch, not new
storage. Desktop may fold its already-open document locally for immediate
optimism; iOS already has the document and does the same. Both compare their
result to the host projection in compatibility tests. A future thin client can
use only the stream.

The existing cold-host nudge applies to every queue command. A command written
on an offline viewer survives in its local session document, syncs when the
room reconnects, nudges the owning device, and is then evaluated. An offline
viewer can inspect every row already present in its local document. “Survive
an offline viewer” does not mean that a device which has never synced the chat
can invent its contents.

## Composer and queue tray

### Desktop

Typing `@` after whitespace or at the start of the draft opens the path picker.
The token from `@` through the caret is the query. Arrow keys move, Return
selects, Escape closes, and typing whitespace without a selection leaves the
literal text alone. `/` discovery retains precedence when the active token
starts with `/`; neither picker rewrites the other's tokens.

A selection removes only its trigger token and inserts a non-text chip at that
caret position in the composer's draft model. Backspace adjacent to a chip
selects then removes it; clicking its close removes it. Copying a draft emits
plain `@path` for human use, but paste is text and does not manufacture a typed
reference. A user must select from the host result to create authority-bound
metadata.

The chip shows a file/folder glyph and basename; duplicate basenames add the
shortest distinguishing parent. Hover/focus reveals the full relative path and
checkout label. The accessible name is “File reference path” or “Directory
reference path.” No preview or file editor opens in the composer.

The queue tray appears immediately above the composer only when it has rows or
is paused. Its collapsed line says the count and either “after this turn” or
“paused.” Expanded rows show a drag handle, ordinal, prompt preview, context
and image chips, edit, run-next, and remove. Keyboard move buttons are exposed
to accessibility even when pointer drag is available. Drag commits one Move
operation on drop, never one per pointer frame.

Editing a row reuses the composer editor in an explicit queue-edit mode. Send
becomes Save; Escape cancels without a command. Failed optimistic mutations
keep the attempted body in the editor and show the host's conflict reason.

During a run the action area says **Steer** on the primary button and offers
**Queue** beside it. Tooltips spell out “affects the current turn” and “runs
after the current turn.” A user preference may remember which secondary menu
was last open, but never changes the primary semantics.

### iPhone

The same typed draft model sits behind the SwiftUI text surface. Because a
plain `TextField` cannot embed robust interactive chips, selected references
render in the existing horizontal chip row above the action button, in draft
order; their insertion markers remain in the model rather than in the string.
The picker is a sheet with the checkout label pinned above host results.

During a run, tapping the arrow continues to steer. A small adjacent clock-plus
control queues the draft and carries the accessibility label “Do after this
turn.” The queue tray is a collapsed count row above the composer; tapping it
opens a bottom sheet with edit, move up/down, remove, run-next, pause, and
resume. Drag reorder may be added, but move up/down is the required behavior
and emits the same Move operation as desktop.

File and directory references use the same relative-path labels as desktop.

### TUI, if retained

The TUI does not need an inline rich-text editor. Picker results insert a
numbered reference pill in a line above the text input and a literal display
marker in the editable line. Its queue pane lists numbered rows and exposes
`e`, `J/K`, `d`, `r`, and `p` for edit, move, delete, run-next, and
pause/resume. It sends and observes the identical typed commands. If the TUI
is removed before implementation, no compatibility shim is required; the
wire types remain frontend-neutral.

## Compatibility and migrations

All new request vectors are `serde(default)` and omitted when empty. Old
desktop and iOS clients continue to send Run, Steer, Interrupt, RespondInput,
skills, and image attachments exactly as today. New engines accept them.

Old engines reject unknown `queue` commands and do not advertise
`contextReferences` or `SearchCheckoutPaths`. Clients gate both features on
capabilities returned by the existing handshake; they do not fall back to
embedding an untyped path and pretending it is safe. Ordinary text containing
`@foo` remains ordinary text on every version.

`SESSION_SCHEMA_VERSION` advances because message parts gain `context`, but
the top-level containers remain `meta`, `messages`, and `commands`. There is no
`queue` Loro container. Edge render-part policy preserves only reference kind,
relative path, and opaque checkout id; it never resolves or enriches them.

The first implementation does not compact queue operations independently.
Every snapshot and shallow/backfill path that preserves the command ledger
must preserve all queue operations too; dropping an old Add while retaining a
later Edit would corrupt the fold. A future general command-ledger compactor
must first design a causally complete checkpoint understood by old and new
clients. That is not part of this slice and cannot be introduced as a
renderer-only optimization.

## Security and privacy

- Search executes only on the chat host and only below its derived checkout.
- A caller cannot provide an absolute root for an existing chat.
- Results and references contain paths, which are workspace metadata, but
  never file contents or content-derived previews.
- Search does not descend into directory symlinks. Resolution accepts only
  symlink targets canonically contained by the checkout.
- Every execution revalidates checkout id, lexical path, and canonical
  containment before appending the user message.
- Directory references do not enumerate contents into a document or prompt.
- Reference tokens remain relative paths in the user's prompt; the harness
  receives the same checkout as cwd. Only missing-path diagnostics are added.
- Queue commands follow existing session-room authorization and host-only
  execution. Queue control does not grant a viewer more filesystem access than
  ordinary Send already grants.
- Limits apply before fuzzy scoring and serialization, so a remote caller
  cannot turn path search into an unbounded walk or response.

## Failure and race table

| Race or failure | Required result |
| --- | --- |
| Device A searches a chat hosted on B | Search forwards to B; returned refs carry B's checkout id. |
| Chat checkout differs by send time | Whole command rejects `checkout_changed`; no harness starts. |
| Picked file is deleted | Command reports the missing path to the agent and command ledger. |
| Picked file changes bytes | Command runs against the new bytes. |
| Path becomes an escaping symlink | Command rejects the containment escape. |
| Path becomes an internal symlink | Command follows it within the same checkout. |
| Two clients edit one row | Document order decides; both clients project the same last edit. |
| Edit retry after response loss | Same client-owned command id appends once. |
| Remove races run-next | Document order decides; exactly one effect applies. |
| Reorder races reorder | Document order plus the missing-anchor-to-tail rule yields one order. |
| Host restarts with three rows | Fold restores all three; scheduler resumes only if not paused. |
| Viewer is offline | Its operations remain in its session doc and converge on reconnect. |
| Host crashes while starting queued row | Pending Queue command recovers through the Run dispatch protocol. |
| User steers with rows queued | Steer affects current turn; queue order is unchanged. |
| User stops with rows queued | Current run interrupts; rows remain and project paused until Resume. |
| Run awaits input | Queue does not drain until the input turn settles. |

## Verification contract

The implementation is complete only with tests at the two module interfaces,
not renderer-only snapshots of their internals.

Checkout-context tests use temporary main checkouts and linked worktrees and
prove:

- the same chat searched from another device produces references bound to the
  host's worktree, not the viewer's similarly named repository;
- lexical escape, absolute paths, escaping symlink ancestors/leaves, and
  checkout replacement reject; contained symlinks work and deletion is reported;
- changed bytes remain valid;
- `.git` and the fixed heavy-directory list never appear;
- entry, result, and depth bounds set truncation;
- a directory selection causes no recursive metadata or content write;
- serialized commands, messages, and edge snapshots contain no file bytes or
  absolute reference paths.

Rust tests in `crates/doc/src/queue.rs` and `crates/doc/src/commands.rs` prove:

- several adds survive snapshot export/import and app restart in order;
- transport retries carrying the same command id append once;
- concurrent edit, move, and remove logs project identically on both clients;
- failed durable appends retain the draft or edit for retry;
- pause survives restart and blocks automatic drain;
- explicit run-next while paused consumes only its named item;
- a live chat defers delivery and an idle chat permits the selected row;
- stop, steer, queued delivery, and idle send produce the four separate
  outcomes in the decision table;
- a pending turn stays outside the processed ledger until dispatch owns it;
  `doc_host::a_crash_before_run_dispatch_leaves_the_durable_turn_for_restart`
  injects the pre-dispatch crash in the shared recoverable-turn branch. This
  slice does not claim a separate Queue-specific process-crash fixture;
- shallow snapshots and causal backfill preserve the command log and every
  row still live under its deterministic fold.

Desktop unit tests cover token completion, checkout-stamp retention, and
chat-scoped add retry keys alongside the existing image/send decision tests.
The changed Swift sources parse with `swiftc`; `crates/sync/tests/ios_room.rs`
source-checks that failed durable appends retain queue drafts and edits. There
is no simulator interaction suite claimed by this slice.

## Implementation map

The design deliberately concentrates policy rather than spreading it through
renderers:

- `crates/proto`: `ContextReference`, request fields, search/projection wire
  types, and capability flags;
- `crates/doc`: queue command types, deterministic projection, and evaluator
  tests;
- `crates/engine`: checkout-context search/resolution, forwardable RPCs,
  command validation, queue scheduling, and recoverable Queue dispatch;
- `edge`: compatibility preservation and causally complete ledger checkpoint
  retention only;
- `crates/ui`: typed draft spans, host picker, delivery split, transcript
  chips, and queue tray;
- `apps/ios`: the same typed draft/command mirror and mobile queue sheet;
- retained TUI: an adapter over the same interfaces, never separate semantics.

The checkout module earns depth by hiding routing, root choice, bounds,
normalization, containment, and stale detection behind `search` and `resolve`.
The queue module earns depth by hiding CRDT order, command-id deduplication,
projection, scheduling, and crash recovery behind `project` and command
evaluation. Deleting either module would spread those rules across every
composer and executor, which is precisely the duplication this design avoids.

## Explicit non-goals

- browsing or editing file contents in the composer;
- semantic/code-index search, grep, symbol search, or file previews;
- snapshotting files or directory trees through `@`;
- allowing a typed `@path` pasted as text to become a trusted reference;
- freezing model or sandbox configuration when a follow-up is queued;
- steering automatically when the user asked to queue;
- treating Interrupt as queue cancellation;
- a renderer-local queue database, renderer-authored rank values, or a second
  edge queue endpoint;
- changing `/` skill discovery or image attachment storage and delivery.

Those can be designed independently. None is required to point at the exact
checkout or make the next instructions visible and durable.
