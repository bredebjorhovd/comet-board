# The ws4 fold never lands — **done** (gh#611)

The 2026-08-25 ~00:35 CEST reading of `/workspace/{org}/stats`: the room is
serving again after the wedge and the `reset-log`, and every number that made
the wedge possible is still true — `snapshotBytes: 0`, `lastTrimAt: ""`,
`checkpoints: 1`, ~2.09MB of log, and
`alarm.consecutiveFailures: 8` of 24 with nothing beside the count saying why.
At 24 the alarm sets `gaveUp` and compaction is dead permanently (gh#378's
terminus), and the only tools left are `reset-log` (which throws the snapshot
away too) or a `ws5` generation break.

Three things were done about it, all while the room is diagnosable LIVE rather
than after the next incident.

### 1. The count has an error beside it now

`alarm.consecutiveFailures` was the same observability gap gh#557 closed for
socket deaths: a number with no cause. Two durable markers close it:

- **`lastAlarmError`** — written by the alarm's catch (`noteAlarmFailure`),
  cleared on success and on revival, carried into give-up so the state an
  operator finally reads includes what broke. `/stats` exposes it as
  `alarm.lastError`.
- **`lastAlarmAttempt`** — stamped BEFORE the work runs and synced with the
  counter, exposed as `alarm.lastAttempt`. This is the marker for the failure
  class that has no catch block at all: a CPU/duration kill mid-replay or
  mid-export takes the invocation and every uncommitted write with it. What
  survives is exactly a spent counter plus an attempt stamp with NO error —
  which now reads as "the work died where nothing could see it" instead of
  climbing toward give-up undecipherably.

The two together discriminate every class the chain has died of: R2 outage
(lastError names it), wasm fault (lastError + `wasm.lastFault` name the call),
load refusal (counted separately in `refusals`, spends no budget), and the
kill-no-catch case (attempt stamp without an error).

### 2. The fold is incremental, not all-or-nothing

One correction to the incident narrative first: the daily alarm never folds
the log — the fold rides flushes (`maybeFoldLog`). The ALARM was failing on
its own chain (checkpoint → trim → backup), and whatever killed it eight times
was invisible until (1). The fold had its own problem: it was all-or-nothing.
Replay everything, export one full snapshot, delete everything — and on a room
whose stored state only exists BECAUSE it is too big to compact cheaply, any
part of that crossing the CPU/memory line meant nothing landed and the
identical attempt rode every later flush forever (gh#557's backoff turned that
from an abort loop into a stall, but not into progress).

`foldLog` now moves the log in bounded STEPS (`planFoldStep`,
`FOLD_STEP_ROWS`/`FOLD_STEP_BYTES`): import the oldest batch into the live
doc, export, put, delete exactly those seqs, decrement `updateBytes`,
**sync**. The sync is the point — each landed step shrinks the room even if a
later one dies mid-invocation, and the next attempt resumes at the first
unfolded row instead of repeating the whole doomed pass. One invocation runs
at most `FOLD_MAX_STEPS_PER_CALL` steps; a backlog walks down across events.
An oversized single update gets its own step rather than being deferred
forever — a reset room's re-uploaded whale rows are exactly the log that most
needs to fold.

A batch that will not IMPORT stops the fold AT itself: nothing past it is
deleted (deleting forward would silently drop ops from the snapshot — the
gh#554 hole argument), everything behind it has already landed, and
`fold.lastFailure` carries `{site: "fold-import", blockedAtSeq}` naming the
exact row. Import/export/put failures are recorded under distinct sites
(`fold-import` / `fold-export` / `fold-put`), extending gh#557's call-site
discipline.

### 3. Young sockets can be read against the answers we gave

`diedYoungLastHour: 47` on a room reporting healthy was unreadable: nothing
said whether those sockets had been ANSWERED. A JoinError the room sent (our
fault, named) and a 1005/1006 that never got one (transport or instance death)
are different incidents wearing the same census row. Per-device join outcomes
(`joinOutcomes`, in-memory like `pushOutcomes`) count ok / refused / failed
per device and land on `/stats`; read them beside `sockets.diedYoungLastHour`
— refusals climbing with churn means the room is ending its own sessions,
clean oks beside churn point at the wire or the runtime.

### What this does not claim

WHY the alarm failed eight times on 2026-08-25 is not settled from here —
that needs `alarm.lastError`/`lastAttempt`, which did not exist until this
change (the same honesty gh#557's writeup paid). What is settled: the room
could not say, the budget was counting down to a permanent give-up while the
fold could not have rescued the room either way, and both are fixed. The
`foldRetryAt` gate, the give-up ladder, and the refusal budget are unchanged.

### Verification

```
cd edge && npm ci
npm run typecheck        # clean
npm run test:unit        # 122 passed (incl. new alarm-attribution cases)
npm run test:workerd     # 43 passed (incl. session-fold-steps.workerd.test.ts)
```

Red-before-green: revert `foldLog` to the all-or-nothing body and
`session-fold-steps` fails on `exports >= 2` (one pass, no steps) and on the
poison-row stop (log emptied past unimportable bytes); strip the
`lastAlarmAttempt` stamp and the mid-work unit case fails with no marker
beside the spent counter.
