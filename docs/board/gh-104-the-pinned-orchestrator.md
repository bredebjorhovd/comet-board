# The pinned orchestrator — **done** (gh#104)

Landed as `[defaults] orchestrator_chat` plus `notify::orchestrator_message` /
`SyncEngine::wake_orchestrator`, a `WatchBoardOrchestrator` stream, and a "Pin
as orchestrator" item on the session context menu of both viewports.
`docs/fallback-chat.md` is what this became: §gh#348 kept the mechanism,
renamed the key to `fallback_chat`, and retired the role.

This is herdr-board's AGE-24 topology, made a product concept instead of
something a human wires by hand every time. It is also how this fork was
built: one long-lived agent that dispatches board work, is woken on settles,
reviews, merges and backfills.

§gh#71's `notify_dispatcher` was the closest thing and it is the wrong shape for
this. It wakes *whoever released each task*, which is right for a chat waiting
on the one thing it released and useless for an agent whose job is the board:
work an operator releases from the panel has no dispatcher chat at all, and
work a sibling releases wakes the sibling. So the pin is a **superset target**,
not a second switch on the same channel — every settle, block, orphan and cap
warning is prompted into it, over the same `Runtime::prompt` review delivery
and the dispatcher wake already use. When the orchestrator *is* the dispatcher
it is told once, in the dispatcher's words: the more specific truth wins.
(Superset until §gh#165, which cut it back to the events no dispatcher could be
told about — the tie-break above generalised into the whole rule.)

Wording is shared with the settle notice (`notify::settled_block`) rather than
written twice, so the one description in `docs/agent-conventions.md` stays the
contract for both audiences. The orchestrator's copy differs in exactly two
ways, both because it did not release the work: the lead line does not claim it
did, and it names who did.

The cap warning is the one event that is *not* on `Signal`, and deliberately:
`Signal` means "an attempt is over or stuck", which is what the webhook and the
issue comment are about. A cap warning is about a run that is still going, and
the only window in which reading its chat can still change how it ends — so it
goes to the orchestrator and not to the operator's pager.

Guardrails, all of them stated rather than incidental:
- **No workspace slot.** The orchestrator is a chat somebody opened, not an
  attempt, so it holds nothing — while everything it releases counts against
  the caps exactly as anyone's does.
- **It bills its own chat's `account`.** Nothing special, and nothing new:
  §gh#101's billing guard reads its dispatches exactly as it reads anyone's, so an
  orchestrator releasing work on somebody else's subscription is warned about —
  or refused — on the same terms a human at the panel is.
- **Exempt from `max_duration`.** It is supposed to outlive every attempt on
  the board, so the clock that stops a looping agent must not stop it. Stated
  in `enforce_duration_cap` and stamped so the log says it once. The exemption
  is on the chat, which makes pinning a *board-dispatched* chat the one real
  misconfiguration — so `doctor` fails on it by name rather than letting a
  child run forever.
- **Notice volume is the budget.** One prompt per event, no polling, no retry.
  An agent that lives forever has no other bound on what it costs.
- **Unpin is the kill switch.** The notices stop; the chat is an ordinary chat.

Delivery to the frontends is its own stream rather than a field on `WatchBoard`
or a read of `ReadBoardConfig`. The pin marks a row in the session list, which
is on screen before any board panel is opened, and `ReadBoardConfig` shells out
to git once per space — the wrong price for a glyph. `WriteBoardConfig`
republishes it as the write lands, so pinning from the app is visible when the
click returns rather than on the board's next reread; the loop's reread still
covers an `$EDITOR` over ssh.
