# The alarm gets a budget — **done** (gh#378)

`SessionRoom.alarm()` was the only thing in this system that could call a
Durable Object with nobody behind it and no upper bound. Everything else is
client-driven and self-limiting: the Rust client caps reconnect backoff at 30 s
(`crates/sync/src/room.rs`), and gh#373 measured a six-hour total outage
producing 3,000–6,000 calls/hour from a couple of dozen rooms — exactly as
designed. The daily alarm was different. It threw, `backupDirty` stayed set, the
runtime re-ran it on its own backoff, and nothing anywhere counted. The code
said so plainly: *"the flag stays set and the alarm chain keeps trying."*

That unboundedness earned its keep — it is how a wedged room repairs and backs
itself up with zero clients connected. It just never had a ceiling. On the free
plan an alarm that throws forever costs errors; on Workers Paid it bills
requests and duration on every attempt, for as long as the condition lasts, with
nobody watching. It was **not** the cause of gh#373 (that was the rows-written
cap, thrown in the constructor before `alarm()` could run). This is prophylaxis.

### The shape of the bound

`alarm()` is now a wrapper around `runScheduledWork()` — the checkpoint, trim
and R2 backup are untouched — and the wrapper is the budget:

- the attempt is counted in the room's own meta **before** the work starts, and
  `sync()`ed. A CPU-limit kill takes the whole invocation with it and rolls back
  anything not yet committed, so a counter written only in a `catch` would miss
  the failure class that needs it most. `ensureDoc`'s `replayAttempts` learned
  this on 2026-07-30 and it is the same discipline, for the same reason;
- a completed alarm clears the counter. One failure followed by a success costs
  the retry that healed it and nothing else;
- a failed alarm is **swallowed, not rethrown**, and reschedules itself on
  `alarmRetryDelay` — doubling from a minute, capped at an hour. Rethrowing
  would hand scheduling back to the runtime's own retry, which is the chain
  being replaced. The room owns its ladder now;
- past `ALARM_FAILURE_LIMIT` consecutive failures it stops rescheduling and
  writes `alarmGaveUpAt`.

Swallowing has a consequence worth recording, because it is currently a
property of where a line sits rather than a decision anyone wrote down: with the
runtime no longer retrying on our behalf, **our `setAlarm` is the only
reschedule left**, so a reschedule that itself failed would end the chain
silently mid-budget. It is safe because the `setAlarm` call is *not* wrapped in
a `try` of its own — an exception there escapes `alarm()` uncaught and the
runtime's retry fires as the backstop. That is the one place in this handler
where a throw is still the right answer. A later tidy that wraps the catch body,
or moves the reschedule into a helper that logs and continues, would remove the
last backstop while appearing to change nothing.

**N = 24.** Chosen against the failure it has to survive: gh#373's outage lasted
six hours, and a room whose writes are impossible for a day should still heal
itself once they are possible again. Twenty-four attempts on that ladder span
~18 h, which clears both, and caps a permanently broken room at 24 invocations
instead of an open account. Tens, not hundreds.

### Giving up is not giving up forever

This is the part that could have made things worse. The alarm chain matters most
with nobody connected, so a room that stopped retrying and never resumed would
be a room that silently stopped backing up — a bill traded for a data-loss
window. A join, an explicit `/tail` read, or any write clears the give-up and
re-arms the daily chain; `backupDirty` is left set throughout, because the work
is still owed.

Deliberately narrow: a counter **mid-ladder** is not reset. Reviving on every
join would let one flapping client restart the retry budget indefinitely — the
same unboundedness wearing a client's clothes. A room that goes on to give up is
revived by the next join, read or write anyway, which is the case that matters.

And nothing here costs a healthy room a wake it was not already paying: a join
on a room with an alarm already scheduled, or with nothing owed, arms nothing.

### Reading it

The issue said `/status`; on a session room that surface is **`/stats`**, which
already reports `backupDirty`, `postReset`, `lastReplayMs` and the rest. It now
also carries:

```json
"alarm": { "consecutiveFailures": 24, "gaveUp": true, "gaveUpAt": 1786312800000, "limit": 24 }
```

`gaveUpAt` is recorded once, at the first give-up, so "since when" answers the
give-up and not the latest retry against it. A room that has given up on its own
backup is a fact somebody can read, which is the whole point — the give-up
window is precisely the window with nobody connected to notice.

### Claims

`edge/src/session-alarm.test.ts` drives the real `SessionRoom` against a faked
Durable Object — `SqlStorage` over `node:sqlite`, so the room's own SQL (meta,
the chunked update log, the blob store) runs as written — and one alarm
invocation is delivered the way the runtime delivers it: the schedule is cleared
*before* the handler runs, which is what makes "did it reschedule itself?"
answerable at all.

- An alarm that throws N times in a row stops being rescheduled — and a further
  runtime-initiated retry after that does no work and schedules nothing.
- A single failure followed by a success leaves the counter clear and the chain
  armed.
- A room that has given up re-arms when a client joins — also on `/tail`, also
  on a write — and backs up again once whatever broke is fixed.
- `/stats` says a room has given up, and since when.
- The daily backup still runs for every room that has not given up.

Plus the two decisions worth pinning: the ladder spans longer than the six-hour
outage it exists for, and a mid-ladder counter survives a join.

### Not in this

gh#377 (`setMeta` writing unchanged values) and the plan decision, which rests on
[gh-373](gh-373-what-the-edge-was-burning.md).
