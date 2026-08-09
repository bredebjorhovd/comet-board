# A settle nobody hears still leaves a line — **done** (gh#194)

Filed off a box on v0.3.5: an attempt with `notify_dispatcher = true` and
`dispatched_by_pane` populated on every row settled, was collected, had its
chat archived — and the log carried **no notice line of any kind** against the
task. Not "queued into chat", not "chat is gone", not "could not notify". The
report's own diagnosis was right about where to look: `wake_dispatcher` logged
on every failure branch it had except the early `let chat = …?`.

### Re-tested first

§gh#165 reworked this exact path and merged after the observation, so the
starting question was which of its three outcomes applied. Against the current
build, with the box's configuration:

- **The reported symptom does not reproduce.** A settle on an attempt whose
  `dispatched_by_pane` is set is queued into that chat and says so
  (`a_settle_with_a_dispatcher_recorded_reaches_it_and_says_so`). §gh#165's
  `Told` hop turned two of the silent exits into a hop to the orchestrator, and
  `note_unheard` covers the case where neither address answers.
- **The silence did not go away, it got narrower.** `note_unheard` only fires
  when the orchestrator *also* comes up empty. A dispatcher skipped on a board
  with a pin behind it still left nothing in the log — the trail said the
  orchestrator was told and never said why the dispatcher was not.
- **And one close still announced to nobody at all**, which is the reported
  shape exactly.

### What changed

**Every exit from `wake_dispatcher` explains itself.** Three did not: the
switch, an attempt nobody released, and a `--via` naming the attempt's own
chat. The switch and the `--via` are `warn` — a chat is recorded, somebody
expected a notice, and the line names the knob to turn. An attempt nobody
released is `info`, because most of a solo operator's board is that, but it is
said rather than assumed: a settle whose log says nothing about a dispatcher is
one an operator cannot tell from a settle whose notice was dropped, which is
what made a real misconfiguration a full-log read rather than a `grep`.

The address is now read **before** the switch, so the reason names the right
thing. `notify_dispatcher` off on an attempt nobody released is not a routing
decision that lost a notice; it is an empty address, and saying "the switch is
off" about it sends an operator to a knob that changes nothing. `Told` grew
`Itself` for the `--via` case, which `NoOne` used to absorb and describe wrong.

**`chat_can_be_told` names which channel was asking.** It is shared by both, so
a task whose dispatcher and pin have both been archived used to leave two
identical lines with no way to tell them apart.

**Two closes that announced to nobody now do.**

- **The duration cap.** `cancel_overrun` closed the attempt `failed`, queued the
  comment upstream, logged, and told no agent anything. The cap's *warning* goes
  straight to the orchestrator because a run that is still going is nothing for
  a dispatcher to act on — but its ending is the opposite, and a close the board
  decided on its own is the most owed notice, not the least.
- **An operator's cancel.** Decided in the engine's board service, which owns
  the interrupt, and so ended an attempt on every channel a settle uses minus
  all of them. The operator pressed the key and knows; the agent that released
  the work is elsewhere, waiting on a step that is now never finishing.
  `SyncEngine::announce_ended` is the seam.

Not announced, deliberately: a **retry**'s replaced attempt (the dispatcher
hears about the attempt that replaces it, and two events for one continuation is
noise) and the panel's **verdict flip** on an already-closed `failed` row
(whoever was owed a notice got one when it closed).

**`Signal::Settled` carries a `note`.** `failed` on its own reads as a dispatch
that never produced an agent, and `cancelled` does not say whether the board
gave up or a person did. The note is the clause the outcome comment already
carried upstream, so the chat and the issue describe one close in one wording —
`attempt 2 · failed — timed out after 3h (cap 2h)`. It rides the webhook body
too, present-and-null on every settle so a receiver need not tell "the board
said nothing about why" from "this build has no such field".

Unchanged: the upstream comment, which was never the thing that went missing.
