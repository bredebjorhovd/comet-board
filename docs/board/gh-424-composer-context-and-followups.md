# Composer checkout context and durable follow-ups — implemented (gh#424)

This record describes the protocol that ships in gh#424. It is intentionally
narrower than the earlier proposal: where implementation chose document-order
last-writer-wins behavior, contained symlinks, or missing-path reporting, this
document says so rather than describing an unimplemented CAS protocol.

This is a clean-room Comet implementation. The Zuse links in the issue informed
product and architecture research only. No Zuse source, UI code, text, assets,
schemas, or tests were copied.

### The decision

An `@` selection is a typed, checkout-relative live path. It carries file or
directory display kind plus an opaque identity minted by the host for the exact
registered checkout where it was picked. It contains no file bytes, preview,
hash, absolute path, or recursively enumerated directory.

The path is resolved at execution time. A changed file remains the same
reference. A missing leaf is allowed and reported in the prompt and command
resolution, which supports asking the agent to create it. File/directory kind
stability is not promised. Lexical escape, canonical escape, and checkout
identity mismatch reject the command. A symlink target contained by the same
checkout is allowed; an escaping symlink ancestor or leaf is rejected.

A follow-up is a durable `Queue` command in the existing session-document
command ledger. `QueueControl` entries edit, move, remove, clear, pause, resume,
or select run-next. `comet_doc::queue::project` derives the visible ordered plan
from that immutable converged log. There is no second renderer-local queue.

The submit outcomes remain distinct:

| State | Choice | Command | Outcome |
| --- | --- | --- | --- |
| idle | Send | `Run` | starts an ordinary turn |
| active | Steer | `Steer` | addresses the current turn |
| active | Do after this turn | `Queue` | appends a durable follow-up |
| active | Stop | `Interrupt` | interrupts and holds an existing plan |
| idle/paused | Run next | `QueueControl::RunNext` | selects exactly one queued row |

Existing `/` skill discovery and ordinary image sending retain their behavior.
Queued images are not silently discarded: desktop refuses that queue action and
keeps both draft and staged images.

### Checkout authority and identity

`SearchContextFiles` is relay-forwardable and executes on the chat host. Its
request names `chatId` or `spaceId`, a query, and an optional result limit. It
never accepts a filesystem root.

Mutable workspace `cwd` strings are not filesystem authority. For an existing
chat, the host resolves the chat's owning `space_id`; a supplied different
`spaceId` is refused. It then permits only:

- the owning space when it is a locally registered repository checkout; or
- a linked worktree currently registered by that same repository.

A forwarded create-chat or set-chat-cwd mutation pointing at another host path
therefore yields an empty context search. ChatId-only requests work for both a
registered main checkout and its registered worktrees.

The returned `ContextSearch.checkout_id` is a SHA-256 equality token over a
version marker, host device, declared checkout identity, and canonical root.
Desktop stores that token with each completed reference in per-draft state;
closing the picker, its trailing completion space, async upload work, or chat
navigation cannot replace it with the current checkout. Provenance is bound to
each exact picker-inserted token occurrence, not its path. Editor replacement
deltas shift unaffected occurrences and revoke every intersected occurrence,
including an indistinguishable same-bytes replacement; a separately typed
duplicate never inherits the selected occurrence's stamp. Exact-token checks
require start/whitespace before and end/whitespace after, and an edit touching
either endpoint revokes the occurrence. A successful Send,
Steer, Queue, or edit draft clear removes the consumed provenance. Retyping or
pasting the same path is therefore still plain text. Queue editing replaces
normal-draft provenance and rehydrates only paths with exactly one durable ref
and one prompt occurrence; ambiguous duplicates require picker reselection.
Wizard advance/finish and edit cancel/success clear provenance with their shared
input. Programmatic `@` and `/` completions apply the same range deltas as editor
events. iPhone mirrors the same
`checkoutId` field. Immediately before Run, Steer, or queued execution, the host
re-resolves the current chat root through the same registered repository and
worktree authority as search, recomputes the identity, and refuses an unstamped
reference, a mismatch, or an unregistered mutable cwd. Hand-typed or pasted
`@path` syntax remains ordinary prompt text; only a picker selection creates a
typed `ContextRef`.

### Bounded path search and resolution

The host performs a fresh breadth-first walk—no index and no watcher—with these
bounds:

- at most 20,000 directory entries inspected;
- depth at most 12;
- 20 results by default, with caller requests capped at 100.

A fixed list skips `.git`, dependency trees, build outputs, and common caches.
This is not a gitignore parser. Matches are deterministically ranked by the
shared context view helper; the response reports `truncated` when the entry or
result budget is hit. Desktop debounces by 90 ms, keys replies by checkout and
query, drops stale replies, and clears search results when the token closes.

Resolution normalizes checkout-relative paths and canonicalizes every existing
ancestor. This catches an escaping directory symlink even when the selected
leaf does not yet exist. Existing final targets are canonicalized again. A
missing target produces an engine-authored note; the reference itself remains
the relative token in the user's prompt. Directory references tell the agent
where to inspect and do not enumerate content into metadata.

### Durable queue projection

The wire shapes are:

```text
Queue { prompt, message_id, attachments[] }

QueueControl = Edit { target, prompt, context[] }
             | Move { target, after? }
             | Remove { target }
             | Clear | Pause | Resume | RunNext { target }

QueueView { rows[], paused?, run_next? }
```

The Queue command id is also its row id. Fold behavior is deterministic in
Loro document order:

- Queue appends a pending row.
- Edit is last-writer-wins while the target row is live.
- Move places the target after a live anchor; `None` means head, self is a
  no-op, and an anchor removed concurrently falls back to tail.
- Remove and Clear affect rows already present at that point in the fold.
- Pause/Resume are last-writer-wins. Interrupt projects `Stopped` only when a
  plan exists, so stopping an empty queue leaves no latent pause.
- RunNext is retained only while its target remains live.

This is not a revision-CAS or durable-tombstone protocol. Concurrent clients
converge because they fold the same document order. Transport retry idempotency
is provided at the command boundary instead.

Desktop queue/control gestures send a client-owned `commandId`. A pending add
retains both command id and transcript message id under a semantic key made from
chat id, prompt, and typed context. A lost response followed by an intervening
operation and retry therefore reuses the original pair; a materially changed
gesture gets a new pair. Generic controls likewise retain a chat-scoped id by
serialized intent. The host acknowledges an existing id only when payload and
context are identical and rejects an id collision otherwise.

### Delivery and crash behavior

Command evaluation introduces `Defer`. A Queue command executes automatically
only when it is the unpaused head and no chat run is live. If a valid RunNext
target exists, every non-target row—including the ordinary head—defers. The
target may execute while paused but still waits for the current turn to finish.

Queue and QueueControl RPCs force the session-document snapshot to disk before
returning success. A crash immediately after acknowledgement therefore reopens
the acknowledged plan mutation.

Queue uses the same recoverable dispatch branch as Run. The command remains
Pending and outside the processed ledger while it is claimed in-process and
`SessionsEngine::dispatch` accepts the turn. The host then writes the terminal
Applied/Rejected outcome and persists that snapshot before marking the command
processed. A process death before dispatch leaves the durable Pending command
for restart; a death after the outcome snapshot reopens a visibly terminal row
that evaluation will not dispatch again. A dispatch rejection records Rejected
and the row is no longer part of the pending projection; the ledger retains the
reason. Status mutation, snapshot export, and saving those exact bytes share the
current-document read guard; a concurrent room reseed waits and then carries a
terminal locally issued command over a stale Pending copy or a snapshot that
predates the command entirely.

Queued execution uses the row's current projected prompt/context and its own
attachments, never the preceding request's images. It rebuilds run configuration
from the live request or durable chat row and clears harness resume so the
follow-up is a new turn.

### Desktop and iPhone surfaces

Desktop provides the host-backed `@` picker and a queue tray with edit, move,
remove, run-next, pause, and resume. “Steer this turn” and “Do after this turn”
are separate actions. Queue adds and edits clear only after the durable RPC
acknowledges success; failures retain draft/edit state and show an error.

iPhone mirrors `ContextRef`, `ContextSearch`, and the queue fields, searches on
the host, renders reference chips and a queue tray, and exposes the same queue
controls. Its direct Loro append returns Bool; it nudges the host and clears the
draft or closes the edit sheet only after commit succeeds. Add, edit, move,
remove, run-next, pause, and resume all surface a failed durable write. Failed
add/edit writes retain the user's text. The phone folds queue commands for
offline display; this slice does not claim a shared Rust/Swift parity fixture.

The former TUI is removed, so no TUI surface or compatibility shim is included.

### Compatibility and privacy

New vectors/options use serde defaults, and old entries without context,
attachments, checkout ids, or client command ids remain readable. An old typed
context reference without a host stamp is deliberately not executable. Queue
and QueueControl entries carry no TTL because they were deliberately written to
wait.

Context metadata contains relative paths and opaque checkout ids only. Search
runs on the authoritative host inside registered checkouts. References never
smuggle contents into the workspace/session document. Existing attachment
storage and `/` skill discovery are unchanged.

### Verification evidence

The repository tests cover:

- context normalization/ranking, bounded search, missing reporting, checkout
  change refusal, and escaping symlink ancestor/leaf refusal;
- real QueueCommand RPC searches for chatId-only registered root/worktree plus
  mismatched space and poisoned create/set-cwd rows returning no host files;
- execution refusing forged unstamped context and a stamped ref after mutable
  SetChatCwd points outside registered roots;
- picker completion retaining the original checkout stamp and hand-typed
  syntax remaining plain text, including select → Send → retype, select →
  delete → retype, selected plus typed duplicate, and identical paste-over
  editor-event regressions;
- command Loro JSON and snapshot reopen retaining non-empty typed context, and
  the engine passing restored refs into execution-time validation;
- queue-edit provenance replacement, ambiguous duplicate fail-closed hydration,
  exact token-boundary/prefix refusal, endpoint/delimiter revocation,
  edit/wizard clear revocation, and programmatic completion range shifting;
- Queue fold edit/move/remove/pause/run-next and two-client deterministic fold;
- exclusive unpaused/paused RunNext evaluation and removed-target cleanup;
- client-owned id deduplication, collision refusal, and add → treated-as-lost
  response → intervening edit → retry through QueueCommand;
- `multiple_followups_and_controls_survive_session_snapshot_reopen`, which
  exports/imports a SessionDoc snapshot and proves three rows plus edit, move,
  and pause survive reopen;
- acknowledged Queue snapshot reopening without a debounce flush;
- the recoverable dispatch branch's pre-dispatch crash window and the
  Queue-specific terminal-outcome-before-processed crash window, plus a
  deterministic terminal-outcome/reseed interleavings where the replacement
  contains stale Pending or omits the command entirely, both reopening terminal;
- a forwarded WatchDocQueue initial frame and changed pause frame;
- iOS source contract that failed durable appends retain add/edit UI state and
  every queue control surfaces failure.

Changed Swift sources parse with `swiftc -frontend -parse`; this slice does not
claim a simulator interaction suite or Rust/Swift shared projection fixture.

### Implementation map

- `crates/proto/src/context.rs`, `crates/proto/src/view/context.rs`: typed wire
  and pure token/ranking helpers.
- `crates/engine/src/context_files.rs`, `crates/engine/src/rpc.rs`: registered
  checkout authority, host search, identity, containment, and forwarding.
- `crates/doc/src/commands.rs`, `crates/doc/src/queue.rs`: evaluator and durable
  document-order projection.
- `crates/engine/src/doc_host.rs`, `crates/engine/src/sessions.rs`: durable
  append, id collision checks, recoverable queue dispatch, and settle kick.
- `crates/ui/src/composer.rs`: desktop picker, retained reference provenance,
  chat-scoped retry ids, and queue tray/actions.
- `apps/ios/Comet`: iPhone typed context and durable queue surface.
