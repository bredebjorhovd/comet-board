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

CheckoutContext::search(query, cursor?) -> PathSearchPage
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
SearchCheckoutPaths {
  chatId? | { spaceId, refName? },
  query,
  cursor?
}

PathSearchPage {
  checkoutId,
  displayName,
  results: ContextReference[],
  truncated,
  cursor?
}
```

The engine performs a fresh, ignored-aware walk rooted at the checkout. It
does not install a watcher or build an index. The walk follows the repository's
ignore rules and always excludes `.git`, Comet upload storage, and nested
worktree administration files. Untracked non-ignored paths remain searchable.

One request is bounded by all of:

- 20,000 visited directory entries;
- 50 returned matches;
- 150 ms of filesystem work;
- depth 32;
- a 256-byte UTF-8 query and 4 KiB normalized result path.

The first limit reached sets `truncated`. A cursor is an opaque continuation
valid only for the same checkout id and query and for 30 seconds. The engine
may restart the walk when a cursor expires; correctness never depends on a
complete scan. Clients debounce for 120 ms and cancel superseded requests.
Empty `@` returns recent shallow matches only (maximum depth four), not the
whole tree.

Matching is case-insensitive subsequence scoring over the relative path with
bonuses for basename, component starts, and exact prefix. Results sort by
score, then fewer components, then bytewise path. Directories and files share
the list; kind is always explicit. This ranking is part of the module so all
clients show the same answer.

The walk uses `symlink_metadata` and never descends through or returns a
symbolic link. At resolution the engine rejects absolute paths, empty
components, `.` and `..`, platform prefixes, NUL, separators foreign to the
normalized `/` form, symlinks in every component, and a final canonical path
outside the canonical root. It repeats this validation immediately before
dispatch. A repository can of course mutate after a run starts; the harness's
ordinary sandbox remains the authority for everything the agent later reads.

### Live, stale, and deleted

No content hash is captured. A hash would promise snapshot semantics without
carrying the snapshot, would make large-file selection expensive, and would
turn a directory into an undefined hash tree. Image attachments already own
the snapshot/upload use case.

Resolution has four user-visible failures:

```text
checkout_changed    This reference belongs to a different checkout.
missing             “path” no longer exists in this checkout.
kind_changed        “path” is now a file/directory, not the selected kind.
unsafe              “path” no longer resolves safely inside this checkout.
```

All references in an instruction validate as one set before a user message is
appended or a harness side effect starts. One failure rejects the command; the
composer restores the draft and chips for an immediate repair. A queued row
stays visible in an `attention` state with the failure and is not automatically
discarded. Editing it creates a new queue operation and makes it eligible
again.

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
FollowupQueue::fold(commands, now) -> QueueProjection
FollowupQueue::next_action(projection, run_state) -> QueueAction?
```

The engine, desktop, iOS, edge compatibility fixture, and tests use the same
serialized command shapes. Rust owns the normative fold. Swift ports the pure
rules for offline display and checks parity against shared JSON fixtures. If a
web or retained TUI surface exists, it consumes `QueueProjection` from the host
instead of inventing another reducer.

### Command shapes

`SessionCommandKind` gains `Queue`. Queue operations live under one payload
kind so existing steer supersession rules cannot accidentally supersede them:

```text
SessionCommandPayload::Queue { operation }

QueueOperation =
  Add {
    operation_id,
    item_id,
    after_item_id?,
    body: { prompt, context_references, attachment_refs[] },
    expected_checkout_id
  }
  Edit {
    operation_id,
    item_id,
    base_revision,
    body
  }
  Move {
    operation_id,
    item_id,
    after_item_id?,
    base_revision
  }
  Remove {
    operation_id,
    item_id,
    base_revision
  }
  Pause { operation_id }
  Resume { operation_id }
  RunNext { operation_id, item_id?, base_revision? }
```

The outer command id is still the durable processed-ledger id.
`operation_id` is a client-minted idempotency key retained across retries of
the same user gesture. `item_id` is stable for the life of a row. `revision`
is the id of the last applied operation affecting that item, not an integer a
client can increment independently.

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

Commands have a deterministic document order from Loro. The fold considers
all well-formed queue operations in that order and applies these rules:

1. Duplicate `operation_id` values apply once; the earliest document position
   wins and later duplicates project as `superseded`.
2. `Add` creates `item_id` once. A duplicate add is an idempotent success only
   when its body and anchor are identical; otherwise it is rejected.
3. `Edit`, `Move`, and `Remove` apply only when `base_revision` equals the
   item's current revision at that fold position. A stale operation is
   rejected with the current row in its resolution.
4. A missing `after_item_id` means the front. An unknown, removed, or
   self-referential anchor rejects the move; it never guesses another place.
5. Each successful add or move removes the item from its prior position and
   inserts it after the anchor in the current folded order. Concurrent moves
   therefore have one order: the command document order.
6. Remove is a tombstone. Later operations cannot resurrect that item id.
7. Pause and Resume are last-operation-wins in document order and are
   idempotent when already in the requested state.
8. `RunNext` names the item it intends to consume. An omitted id means the head
   at that fold position. Replaying it cannot consume a second item.

Every operation produces `applied`, `rejected`, or `superseded` through the
existing host-only command outcome fields. The host's processed-id store still
guards side effects. Pure folding uses the outcome when present; while an
operation is pending it tentatively includes a locally valid result. Thus two
offline viewers may briefly show their own tentative order, but after their
documents merge they compute the same Loro order, and after host evaluation
they show the same accepted order and rejection messages.

Optimistic edit is not silent last-write-wins. If two clients edit revision R,
the earlier merged operation advances the row to E1 and the later operation is
rejected as based on R. Both clients converge on E1 and the second editor sees
their still-recoverable text in the rejected operation. Reorder and delete use
the same rule. This makes conflict visible without a queue-specific merge UI.

The projection returned by `WatchFollowupQueue` contains no absolute context
paths or file bytes. Existing image attachment paths retain today's transport
contract and are not treated as checkout context:

```text
QueueProjection
  chat_id
  paused
  revision             last folded queue operation id
  items[]
  pending_operations[] only this client's recoverable failures when known

QueueItem
  id
  revision
  position
  body
  state                ready | starting | attention
  resolution?
  created_at
  created_by
```

`position` is a projection, not stored rank data. Clients reorder by item ids,
not floating-point or fractional keys. This avoids rank exhaustion and makes
the ledger's existing total order the only conflict arbiter.

### Starting the next turn

Queue operations and ordinary delivery remain distinct. An applied `Add` does
not itself call the harness. The host queue scheduler asks `next_action` after:

- a queue command changes;
- a run reaches a terminal or parked-between-turns state;
- the host opens or restores a session document;
- resume or run-next is applied.

`run_state` must distinguish an actively producing/awaiting-input turn from a
steerable harness parked between turns. Today's `chat_is_busy` is too coarse
for that decision. `SessionsEngine` therefore exposes `TurnState` as
`active`, `awaitingInput`, `parked`, or `absent`, and a watch notification when
it changes. The queue scheduler is the only caller that translates `parked`
or `absent` plus a ready item into a new turn.

Automatic delivery is allowed only when the queue is not paused, no queue item
is already `starting`, and state is `parked` or `absent`. `awaitingInput` is
still the current turn and does not drain the queue. `active` never drains it.
Pause affects automatic delivery only. `RunNext` is explicit and may select
the head while paused, but still waits for active/awaiting-input work to become
safe; it never steers or interrupts that work.

To consume an item without a crash gap, the host uses the existing recoverable
Run protocol:

1. for automatic delivery, append/ensure a host-issued `RunNext` queue
   operation whose id is deterministically derived from the item id and
   revision; explicit delivery already has that operation;
2. deterministically derive a child run command id from the item id and the
   consuming `RunNext` operation;
3. validate its checkout and context references;
4. append/ensure that ordinary `Run` command under that stable id;
5. leave the item projected as `starting` while the Run is pending;
6. let the existing Run executor persist its snapshot, claim it in-process,
   dispatch, and mark it processed;
7. project the item consumed only when the child Run is `applied`.

A crash before step 3 leaves a ready queue row. A crash after step 3 finds the
same stable Run id and does not append another. A crash during dispatch uses
today's pending-Run recovery. A rejected Run leaves the item in `attention`
with its body intact. The next item cannot pass it automatically; the user may
edit, remove, or explicitly run a different item.

The derived Run rebuilds model, reasoning, sandbox, and resume from the chat's
current row exactly as today's dead-steer fallback does. A queued follow-up
does not freeze model configuration. It does freeze the user's prompt,
references, and uploaded image identities.

Stopping a run does not consume, flush, pause, or resume the queue. When the
interrupted run reaches terminal state, normal automatic-drain rules apply.
The UI offers **Stop and pause queue** as a two-command convenience only if a
later design needs it; it is not an overloaded interrupt command here.

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

Transcript chips wrap under the user text. File and directory references use
the same labels as desktop. A stale queue row uses inline attention styling,
not a modal alert.

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
- Symlinks are neither searched nor referenceable.
- Every execution revalidates checkout id, lexical path, component kinds, and
  canonical containment before appending the user message.
- Directory references do not enumerate contents into a document or prompt.
- The engine-authored harness trailer contains relative paths only. The
  harness already receives the checkout as cwd.
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
| Picked file is deleted | Command rejects `missing`; queued row remains repairable. |
| Picked file changes bytes | Command runs against the new bytes. |
| Path becomes a symlink | Command rejects `unsafe`. |
| Two clients edit one row | First in document order applies; stale base revision rejects visibly. |
| Edit retry after response loss | Same operation id is an idempotent success. |
| Remove races run-next | Document order decides; exactly one effect applies. |
| Reorder races reorder | Document order plus revision check yields one order and one visible conflict. |
| Host restarts with three rows | Fold restores all three; scheduler resumes only if not paused. |
| Viewer is offline | Its operations remain in its session doc and converge on reconnect. |
| Host crashes while starting queued row | Stable child Run id recovers through existing pending-Run protocol. |
| User steers with rows queued | Steer affects current turn; queue order is unchanged. |
| User stops with rows queued | Current run interrupts; queue is neither cleared nor implicitly paused. |
| Run awaits input | Queue does not drain until the input turn settles. |

## Verification contract

The implementation is complete only with tests at the two module interfaces,
not renderer-only snapshots of their internals.

Checkout-context tests use temporary main checkouts and linked worktrees and
prove:

- the same chat searched from another device produces references bound to the
  host's worktree, not the viewer's similarly named repository;
- lexical escape, absolute paths, symlink files, symlink directories, kind
  changes, checkout replacement, and deletion reject;
- changed bytes remain valid;
- ignored files and `.git` never appear;
- entry, result, depth, time, query, and path bounds set truncation without
  blocking later RPCs;
- a directory selection causes no recursive metadata or content write;
- serialized commands, messages, and edge snapshots contain no file bytes or
  absolute reference paths.

Follow-up queue fixture tests fold the same JSON in Rust and Swift and prove:

- several adds survive snapshot export/import and app restart in order;
- add/edit/move/remove/pause/resume/run-next retries are idempotent;
- every pairwise concurrent edit, move, and remove ordering converges;
- rejected optimistic operations restore the attempted body;
- pause survives restart and blocks automatic drain;
- explicit run-next while paused consumes only its named item;
- active and awaiting-input states never drain, while parked and absent do;
- stop, steer, queued delivery, and idle send produce the four separate
  outcomes in the decision table;
- crash injection before child Run append, after append, before dispatch, and
  after dispatch never loses or duplicates a turn;
- an attention row blocks automatic pass-through to later rows;
- shallow snapshots and causal backfill preserve every live row and tombstone
  across an offline client's later merge.

Desktop and iOS interaction tests additionally prove `@` and `/` discovery do
not steal each other's input, image-only sends still work, reference chips
round-trip without parsing prompt text, drag/move emits one operation, and the
two active-run choices carry distinct accessible names.

## Implementation map

The design deliberately concentrates policy rather than spreading it through
renderers:

- `crates/proto`: `ContextReference`, request fields, search/projection wire
  types, and capability flags;
- `crates/doc`: queue operation types, context message parts, deterministic
  queue fold, checkpoint rules, and shared fixtures;
- `crates/engine`: checkout-context search/resolution, forwardable RPCs,
  command validation, queue scheduler, stable child Run handoff, and turn-state
  watch;
- `edge`: compatibility preservation and causally complete ledger checkpoint
  retention only;
- `crates/ui`: typed draft spans, host picker, delivery split, transcript
  chips, and queue tray;
- `apps/ios`: the same typed draft/command mirror and mobile queue sheet;
- retained TUI: an adapter over the same interfaces, never separate semantics.

The checkout module earns depth by hiding routing, root choice, bounds,
normalization, containment, and stale detection behind `search` and `resolve`.
The queue module earns depth by hiding CRDT order, optimistic projection,
idempotency, conflict rejection, scheduling, and crash recovery behind `fold`
and `next_action`. Deleting either module would spread those rules across every
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
