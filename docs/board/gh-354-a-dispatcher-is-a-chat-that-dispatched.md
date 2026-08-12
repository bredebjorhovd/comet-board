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
still running — was, as far as the sweep was concerned, a finished attempt.

Which is the cost of the framing §gh#348 argues against, in one concrete bug:
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

A chat with an unsettled attempt against it reads `Live` to `chat_standing`, for
the same reason a blocked attempt does: somebody is coming back to it. `Live`
rather than the pin's `Held`, because unlike a pin it says something about right
now — somebody is in there waiting for an answer.

**Narrower than "never archive anything that dispatched."** The hold is on work
in flight. `outcome` unset is the whole population — a running child and a
blocked one both — and once its children have ended, a dispatcher is finished
like anything else and its chat goes to the shelf on the usual terms. The shelf
still gets cleared, which is the entire point of §gh#139.

Nothing about having dispatched holds the *checkout*. The directory and the
branch are a separate question with their own answer (`gc::standing`), and this
deliberately does not reach for them.

`note_held_as_dispatcher` says so once per attempt, the counterpart to §gh#286's
`note_held_by_dependents`: `keeping chat <id> on the shelf — it released attempt
14, which has not come back`. Otherwise "why is this settled attempt's chat still
here" is a silence the operator has to reconstruct from the dispatch edge by
hand.

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

Related: §gh#348, §gh#139 (where the sweep came from), §gh#144 (which made the
window `on-settle`, so this bit the same day it could).
