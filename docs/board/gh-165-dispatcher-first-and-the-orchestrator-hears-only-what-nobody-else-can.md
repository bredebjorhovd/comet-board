# Dispatcher-first, and the orchestrator hears only what nobody else can — **done** (gh#165)

`notify.rs` had four audiences and wired the two that are agents as independent
switches: the chat that released the work (`notify_dispatcher`, off) and the
pinned orchestrator (`orchestrator_chat`, unset). Both fired when both were set,
and the orchestrator heard about *everything* — including every settle a live
dispatcher had already handled. That is backwards, and it is why the dispatcher
wake shipped off: §gh#71's config comment said "an orchestrator woken by every
child it released cannot hold a train of thought", which is true and is a
description of the **other** channel. The orchestrator channel delivers the
whole board into one chat. The dispatcher channel prompts the one agent whose
plan that task was a step in. The caution belonged to the second one and was
attached to the first.

So the two are now **one channel with a fallback hop** (`SyncEngine::announce`),
and the switches were the wrong shape rather than wrongly set:

- **Dispatcher-first, on by default.** A settle *or a block* goes to
  `attempt.dispatched_by_pane` when there is one and it can still be told.
  Blocks are new here: a block settles nothing and closes nothing, so nothing at
  all happens until somebody acts, and the party who was waiting on that step is
  the one who can act soonest. `dispatcher_message` takes a `Signal` now and
  shares `blocked_block` + `unsticks` with the orchestrator's copy, for
  `settled_block`'s reason — two audiences describing one state two ways is two
  contracts for it.
- **The orchestrator is the addressee of last resort**, and those three cases
  are the whole of its job. Work no agent released — the panel, the phone, a
  bare `comet-board dispatch` — which is most of a solo operator's dispatches
  and reached nobody at all before. Work whose dispatcher did not survive it:
  attempts cap at 2h and chats archive as their task settles (§gh#139), so
  `chat_can_be_told` saying no is ordinary rather than exceptional, and the
  notice was *dropped* there, silently, documented as best-effort. It is a hop
  now, not a drop — the event still matters, it just needs a different reader.
  And the events that belong to no attempt, which is the duration cap's warning:
  the attempt is still running, so no dispatcher is waiting on a step that
  finished, and it goes straight to the pin as it always did.
- **No double delivery.** When a dispatcher was told, the orchestrator is not.
  That is what makes a pinned orchestrator survivable on a board with
  dispatching siblings on it: its context fills with the things that would
  otherwise vanish, not with a copy of every child's settle. §gh#104's tie-break —
  "when the orchestrator *is* the dispatcher it is told once" — was this rule
  seen through a keyhole.
- **Nothing is dropped in silence.** An event that reaches neither agent now
  logs which half of the address was empty ("no chat released it, and no
  orchestrator is pinned" / "the chat that released it is gone, and …"), because
  "the orchestrator never told me" and "the orchestrator had nothing to say"
  were indistinguishable. That is what `Told` exists for: three answers rather
  than a bool, since both the hop and the log line need to know *why* a channel
  came up empty.
- **`doctor` says which channel takes an event**, rather than reporting two
  switches. `settle notice` reads the pin as well as the switch — off with a pin
  is a routing choice and off without one is silence, and one boolean cannot
  tell those apart — and the `orchestrator` line says "addressee of last resort"
  or, with the wake off, that it is taking the whole board again.

Unchanged, and said out loud: the operator webhook (audience 4) and the upstream
comment (audience 1). Neither is addressed to an agent, and the comment is the
durable trail either way. Whether a *human* dispatcher gets anything is still
out of scope — they get the webhook and the row.
