# The orchestrator was a role we did not need — **done** (gh#348)

Brede, 2026-08-11, after a day of driving the board from one long-lived chat:

> *"in terms of the orchestrator stuff we spoke about earlier we could take
> inspiration from /herdr — if you look at the space we are in, you are the main
> space, you spin out all the other agents, but you haven't been pinned as an
> orchestrator or had any other instruction that it is your role to do so. Maybe
> I've been overcomplicating it."*

The evidence was the session itself: ~25 tasks dispatched, reviewed and merged,
a dozen issues filed, a release cut and two machines updated — with no pin, no
orchestrator brief and no orchestrator machinery. The role was never declared.
It emerged from dispatching.

`docs/orchestrator.md` conflated two things, and this splits them:

1. **A mechanism** — the fallback addressee for the notices that have no
   dispatcher: work released from the panel, the phone or a bare CLI call; work
   whose dispatching chat did not survive it; cap warnings; §gh#390's incident
   line. Small, necessary, kept.
2. **A role** — "one chat, pinned, that receives everything the board does and
   drives it". Retired. Not deprecated in favour of something else: deleted,
   because the thing it described happens on its own.

### What the board says now

`[defaults] orchestrator_chat` → **`[defaults] fallback_chat`**, and everything
downstream of it reads in the new vocabulary: `Defaults::fallback`,
`notify::fallback_message`, `SyncEngine::wake_fallback` / `tell_fallback`,
`gc::chat_standing`'s hold, `doctor`'s **`fallback`** line, the `settle notice`
line beside it, `routing.example.toml`, the file `init` writes, and every word a
person reads on either viewport ("Send board notices here" / "Stop sending board
notices here"; the sidebar slot is **Board notices**).

**Nothing on disk breaks.** The old key is still read — its own field rather
than a `serde(alias)`, because a file carrying *both* spellings is a duplicate
field to an alias and a parse error for the whole config, and a stale key being
ignored is a far better failure than a board that will not start. `fallback()`
prefers the current spelling. On the write side `routes::LEGACY_KEYS` maps the
old key to the new one and clears the old line in the same edit, so a write from
any surface — including a phone installed before this landed, which still sends
`orchestrator_chat` — lands under the current spelling and leaves the file
saying one thing about one setting.

**The wire name is frozen.** `WatchBoardOrchestrator` stays as the method
string; only the Rust constant is renamed (`methods::WATCH_BOARD_FALLBACK`). A
method name is an identifier, not a description: an installed phone subscribes
by that exact string, and renaming it would take the ◆ off that phone's session
list until somebody shipped it a new build — a real cost, for no gain to any
reader. The rename is about what the board *says*, and it says nothing in an RPC
method name.

### The exemption the role was paying for

A pinned orchestrator was exempt from `max_duration`, "because it is meant to
live forever". Follow that through: the chat this key names is one you opened
yourself, so it has no attempt, so the duration cap has nothing to say about it
anyway. The exemption could only ever fire in the one configuration `doctor`
refuses — a board-dispatched chat named here — where its whole effect was to
keep a real attempt alive past every cap on the strength of a config key.

It is gone. An address is not a role, and it is not a licence either. The
`doctor` fault that names the same misconfiguration stays, with its reason
rewritten: the cost is not a cap being dodged, it is the board's own news landing
in a chat that is in the middle of a task of its own.

`gc`'s hold stays exactly as it was — that chat is never archived, because more
notices are always coming to it. That is a property of the address, not of a
role, and §gh#354 already attached the *behavioural* half of the same protection
to any chat holding work the board is not finished with.

### Where the brief went

The brief was the role. Its content was not: it is how to drive a board, and it
now lives where a driving agent will actually read it — a **Driving the board**
section in `assets/skills/comet-board/SKILL.md` (the skill that ships in the
binary and is installed into every dispatched agent's config dir) and a matching
paragraph in `docs/agent-conventions.md`. Not "paste this into the pinned chat":
guidance for anyone who dispatches, which is the population it was always about.

One thing in it is new, and it is the answer to the sharp edge the ticket named.
The old doc's own volume argument conceded that on a one-person board the
fallback is nearly everything, and a chat exempt from `max_duration` that
accumulates every event forever fills its context window (gh#271 is the meter
that makes it measurable). The durable answer is not a bigger window:

> **The board is the state — re-read, don't remember.** `comet-board list
> --json` knows what is ready, working, blocked and in review, and it is still
> true after a compaction, a restart, or somebody else's dispatch. A driver that
> re-reads has no context problem to solve.

That is a smaller change than a role, and it is the one that survives a
compaction.

### What did not change

Delivery, membership, ordering, the one-message-per-event rule, the
dispatcher-first hop (§gh#165), the not-repeated rule that keeps the address
survivable on a busy board, the `◆`, the slot above Spaces, and both viewports'
menus in every place they already were. This is a rename, one deletion, and a
document that stops describing a job title.

Related: §gh#104 (where the pin came from), §gh#165 (dispatcher first), §gh#354
(the protections attached to the declaration rather than the behaviour — the
same argument, found as a bug), §gh#122 (the slot), §gh#390 (the incident line),
gh#271 (the context meter), gh#340 (an agent not delegating — the same question
from the other end).
