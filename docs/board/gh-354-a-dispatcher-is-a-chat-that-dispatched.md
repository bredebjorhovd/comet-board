# A dispatcher is a chat that dispatched — **done** (gh#354)

Reported on 2026-08-11: *"I dispatched two agents through another pane; when
the PRs were merged, the thread that I was working from disappeared."*

That pane was itself a board attempt. Its own task settled, its window passed,
and §gh#139's shelf sweep archived it — correctly, by every rule the sweep had.

Landed as `gc::Dispatchers` and one more guard in `gc::chat_standing`.

### What the sweep could see

`SyncEngine::archive_chats` archives the chat of every attempt nobody is coming
back to. The list of what it will not touch is careful: a live or blocked
attempt, a task in review, a chat with no board attempt ("those are theirs"),
and

> **The pinned orchestrator**, whatever attempt it was dispatched as: it is told
> about every settle on the board, so it is never finished.

That protection was attached to the pin and to nothing else. A chat that is
*acting* as a dispatcher — one somebody is working in, that has released work
still owed — was, as far as the sweep was concerned, a finished attempt.

Which is the cost of the framing gh#348 argues against, in one concrete bug:
**the protections were attached to the declaration, not to the behaviour.** A
chat with two agents out is waiting on exactly what the orchestrator is waiting
on, at a smaller scale. Nobody pinned it, and nobody should have to: it became a
dispatcher by dispatching.

### The edge was already there

`COMET_BOARD_CHAT_ID` rides every dispatch (§dispatch-pipeline), so
`dispatched_by_pane` on an attempt row is the chat that released it. The board
already reads that edge *downward*, to find an address for a settle notice
(`wake_dispatcher`, §gh#165). `gc::Dispatchers` reads the same edge **upward** —
chat → what it is still waiting on — built once per sweep from the whole board,
for the reason `rebased::Dependents` is: no amount of looking at one attempt
finds the work it released, which lives under another task entirely.

A chat holding work the board is not finished with reads `Live` to
`chat_standing`, for the same reason a blocked attempt does: somebody is coming
back to it. `Live` rather than the pin's `Held`, because unlike a pin it says
something about right now — somebody is in there waiting for an answer.

### Until when, exactly

The first cut of this held the chat while its children were *open* —
`outcome.is_none()` — and that is the wrong moment, in the direction that leaves
the reported bug in place. An attempt's outcome is set when it **settles**: the
agent finishes and the pull request opens. Everything a person does *with*
released work happens after that. The settle notice is delivered into this very
chat (§gh#165), the diff is read from it, and the merge, the retry or the
`changes requested` is decided in it.

`archive_chats` defaults to `on-settle`, which is a window of `0`: `Mark` on one
sweep, `Collect` on the next, with no grace to absorb the gap. So a hold that
lifted at settle would cover the interval when nobody needs the chat and let go
at the start of the one when they do — and Brede's thread would still vanish,
now slightly *before* the merge he described rather than at it.

The measure is the child's `standing`, the module's one ownership question asked
one edge further up: **a dispatcher outlives the work it released.** A chat is
spent when the last thing it dispatched is spent — merged, closed upstream, or
marked done. Not a second rule; the same one, which is also why `Dispatchers::of`
takes the `Dependents` it needs to ask it.

**Still narrower than "never archive anything that ever dispatched."** A child
that lands releases its dispatcher, and a chat whose work has all landed goes to
the shelf on the ordinary terms. The family finishes together: merge the two
pull requests and the next sweep marks the children's chats and the thread that
released them alike. The shelf still clears, which is the entire point of
§gh#139.

Nothing about having dispatched holds the *checkout*. The directory and the
branch are a separate question with their own answer (`gc::standing`), and this
deliberately does not reach for them.

`note_held_as_dispatcher` says so once per attempt, the counterpart to §gh#286's
`note_held_by_dependents`: `keeping chat <id> on the shelf — it released attempt
14, which the board is not finished with`. Otherwise "why is this settled
attempt's chat still here" is a silence the operator has to reconstruct from the
dispatch edge by hand. It fires only where this hold is the operative one — a
pinned orchestrator that also dispatches was held by its pin either way, and a
line claiming otherwise would send the reader to the wrong fact, which is the
failure §gh#194 was about. Asked by re-running the same decision without the
hold, so there is no second copy of the rule to drift.

### The reversibility argument, and where it stops

§gh#139 justified no grace and no confirmation on the ground that archiving is
reversible: the transcript is untouched, Settings → Archived puts it back, and a
wrongly-settled attempt un-archives its own chat. That argument is about a spent
*agent's* chat, and it holds.

It holds much less for the window somebody had open. One click restores a
transcript; it does not restore the place you were working from — and a person
whose thread vanished mid-dispatch has no reason to guess that "archived" is the
word for what happened to it.

The answer taken here is to keep that window out of the sweep's reach rather
than to soften the sweep with a confirmation. A confirmation would land on every
archive, which is overwhelmingly the case the argument was right about, and it
would leave the actual defect — that a working thread read as finished — in
place behind a prompt. Everything still archived is an ended attempt's chat.

Related: gh#348, §gh#139 (where the sweep came from), §gh#144 (which made the
window `on-settle`, so this bit the same day it could).
