# What killed the sockets — **done** (gh#527)

On the evening of 2026-08-19 the phone sent work into the box's workspace and
nothing came back. Sends rode the HTTP nudge and landed — the box journal logged
`nudge: chat doc opened` for every one of them — and the replies rode the
session rooms, which were dying 1006 mid-life across the whole fleet. The app
put all 22 workspace rooms on the reconnect ladder, climbed to the 30 s cap, and
rendered a transcript that looked like a conversation nobody had answered yet.

The diagnosis ran on ~450 `wrangler tail` events and a guess. That is the part
worth fixing, because everything else about the incident was already understood:

- **the tail had no exceptions in it.** Not "an error we misread" — nothing. No
  `Exceeded allowed duration`, no cap text, no TypeError. Dozens of `GET /ws`
  invocations ending `canceled`, and close code 1006 on the client;
- **two known producers make exactly that signature**: the Workers free-tier
  duration cap, which kills sessions mid-life rather than refusing them
  (gh#126), and the `ctx.abort()` WASM-poisoning escalation, where a room whose
  stored state kills the instance on every wake "answers the join every single
  time" and then dies (gh#378);
- **nothing in the system could tell them apart after the fact**, and the box's
  own health gauge reported "10 of 10 live" throughout.

The reason the tail was empty is structural, and it is what the fix is built
around: **the deaths that matter take the invocation with them.** A runtime kill
and an `abort()` both tear the instance down where it stands. No
`webSocketClose` runs in it, no `console.error` from it is guaranteed to ship,
and any storage write not already committed is rolled back. A room can lose
every socket it holds and, on its own next wake, have no idea anything happened.

### 1. The room keeps the record the other way round

`edge/src/socket-log.ts`. An accept writes a durable row **before** the socket
can die; a close or error deletes it; the next wake reconciles the survivors
against the sockets the runtime still lists. A row with no live socket and no
close event is a socket that **vanished** — nothing in that instance closed it,
so whatever ended it ended an invocation too. That is the fact the diagnosis was
missing, and it is now a number on `/stats` and a line in the journal naming the
devices and the ages.

`webSocketClose` also, at last, takes the code and reason the runtime has been
handing it and this room was dropping on the floor. Every observed death is
logged with the code, the reason, `wasClean`, the socket's age, the device, and
the room's stored doc size — the three-way discriminator between a peer that
went away, a room we closed on purpose (4410 reset, 4411 reseed, 1011 broadcast
failure) and a room whose own doc is the thing killing it.

And the poisoning escalation now persists **why** before it aborts (`lastAbort`,
made durable with a `sync()` because an abort discards uncommitted writes), so
"we did this" and "the platform did this" stop looking identical from the
outside.

Deaths inside 30 s are counted separately as churn — the same line
`crates/sync/src/room.rs` draws for a session that earns a backoff reset.

### 2. A doc the room cannot load is refused, not attempted

The second producer gets a guard, in `ensureDoc`, before any wasm call:
`MAX_DOC_LOAD_BYTES` (32 MB — the inbound bound this room already enforces, so a
persisted doc above it is not something the fleet can legitimately have made)
and a `try` around the snapshot import. Either failure raises `DocLoadRefused`,
which the join answers as a `JoinError` the client can back off from.

A refusal is strictly better than an attempt: an attempt kills the invocation,
which is a socket that dies 1006 with nothing logged and a client that redials
into the identical death, forever. Three consecutive refusals evict the stored
state and let the fleet reseed it (gh#148/#207) — every engine holds the full
document locally, and `dropLog`'s `postReset` keeps the R2 disaster copy from
being overwritten by the emptied doc meanwhile. Same shape, and the same
reasoning, as `REPLAY_CRASH_LIMIT`.

**This predicate shipped too narrow and never fired on the incident above** —
the room was 1.67 MB against the 32 MB line, and its corruption was routed to
the poisoning tripwire instead. Fixed in gh#554; see
[gh-554-size-is-not-the-predicate.md](gh-554-size-is-not-the-predicate.md) for
where the line between the two escalations actually goes.

`edge/test/workerd/session-load-guard.workerd.test.ts` runs it in real workerd:
a corrupt snapshot is answered three times, then evicted, then the room joins
clean; an oversized room refuses and **keeps answering** — an abort there would
take the test with it, which is the difference being asserted.

### 3. The engine counts the sequence, not the sample

`crates/sync/src/churn.rs`. "10 of 10 live" was not wrong arithmetic. It counts
sockets joined **at the instant it is asked**, and a room in a join-then-die
loop genuinely is joined a fair share of those instants. A sample cannot see a
sequence, so the sequence is counted: every session end that had joined,
recorded against its room with whether it lasted `HEALTHY_SESSION`, reported as
a per-hour rate.

`joined && !healthy` is churn. `!joined` stays what it always was — a down room,
which the live count already reports. The registry is process-global on purpose:
churn belongs to the ROOM, and a sick room is one whose client the supervisor
keeps rebuilding, which would zero anything kept per-handle. It is bounded on
both axes and pruned to the window.

`EdgeHealth` grows a third axis beside `dark()` and `live_but_unconverged()`,
names the worst rooms in its one-line summary, and `comet-board doctor` now
**fails** on it — with the two places to look, because the reading that precedes
it is the one that talked an operator out of looking.

### 3b. …and the same check stopped being blind to stuck content

Review finding on this ticket: the new grade was `!dark() && !churning()` and
still did not consult convergence, so an engine at "18 of 18 live" holding 262
entries that exist on one device — gh#483's own state — printed `ok`. Churn
blindness fixed, convergence blindness not.

It is graded now, on `content_stuck()` rather than on
`live_but_unconverged()`, and the difference is why it could be graded at all:
"unacknowledged" includes the write somebody made half a second ago, and a check
that failed on that would be red all day and read by nobody — a worse outcome
than the blindness it replaces. `content_stuck()` is the fleet-wide form of the
rule gh#483 already wrote per room (`ConvergenceState::needs_attention`:
blocked always, pending only once it has outlasted the alert threshold). The
rule was there; no health surface was asking it. `chat_rooms_stalled` carries it
up through the census, defaulting to zero from an engine too old to report it,
which is the right fallback — it cannot see the state, so it must not be failed
for it.

Still not done, and still gh#483's: the phone renders neither. It has
`SessionStore.convergenceState` and no view reads it, so a stranded transcript
on the phone looks exactly like a converged one. gh#527's strip says why a room
is not connected, not why a connected room's content is not moving.

### 4. The phone says it out loud

`apps/ios/Comet/Sync/RoomHealth.swift` and the strip above the transcript. The
app already knew: it logged "redialing in 30000ms" 22 times. What it showed was
silence, and a person cannot tell silence from "nothing you type will arrive
until this clears".

Degraded means the ladder has climbed three rungs — one dropped socket is a lid,
a tunnel or an edge deploy, and a banner that fires on those is one people learn
to scroll past. The detail line names **which** failure it is, because "the edge
is answering and then dropping" and "the edge is not answering" read identically
from a dead transcript and are fixed in different places.

Copy and Share hang off the strip, since the incident's other half was that
nothing could be got off the phone at all — no screenshot, no copy — and the
diagnosis ran on paraphrases over SSH.

### Which producer it was

Unresolved here, on purpose: the operator was upgrading the Workers plan while
this was written, and that experiment decides it. Both branches are now
instrumented rather than guessed at — a duration-cap kill shows up as `vanished`
sockets with no `lastAbort`, a poisoning escalation shows up as `lastAbort` with
a reason, and an unloadable doc refuses in the open instead of looping. The
free-plan discriminator still holds: the cap resets at UTC midnight, and
upgrading removes the class entirely.

### What is deliberately not here

- **No new alarm, no polling.** Every count is written on a wake something else
  was already paying for (a dial, a close, a join answer, the daily alarm), so
  the hibernation discipline in `session-room.ts` is untouched.
- **No automatic plan change and no automatic room surgery** beyond the bounded
  eviction described above. Destroying state to fix an export problem is worse
  than looping loudly, which is the line `ensureDoc` already draws.
