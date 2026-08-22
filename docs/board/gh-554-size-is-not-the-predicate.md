# Size is not the predicate — **done** (gh#554)

The gh#527 load guard (#550) recorded **zero refusals** during the incident it
was written for:

```
docLoad: { guardBytes: 33554432, storedBytes: 1749030, refusals: 0, limit: 3 }
lastAbort: { reason: "wasm heap poisoned after 3 strikes: RangeError: Invalid array buffer length" }
```

5% of the size threshold. The guard was size-only; the failure was corruption.

### What gh#557 already fixed

Most of the remedy landed there, and better than this ticket proposed it:
`wasmHeapUsable()` settles whether a `RangeError` is an exhausted heap or one
call that would not be served, and `escalateWasmPoisoning` declines to strike
when the heap still works. `handleDocUpdate` got the same question for bytes off
the wire. See
[gh-557-the-abort-was-the-loop.md](gh-557-the-abort-was-the-loop.md) — that is
where the abort loop itself was broken.

What it did **not** do is make the load guard reachable. Three gaps were left,
and they are what is here.

### 1. A snapshot that will not import is stored state

`ensureDoc`'s snapshot import still rethrew every wasm-shaped error on shape
alone, so a `RangeError` off this room's own storage never reached `refuseLoad`.
After gh#557 that no longer aborts the isolate — but it does not refuse, count,
evict or reseed either. It throws out of `ensureDoc` on every cold start, the
join answers a generic `internal error`, and the room stays wedged forever with
`docLoad.refusals: 0`.

It now asks the same question `handleDocUpdate` does: a `RangeError` over a
working heap is a statement about the stored snapshot. Use-after-free stays
exempt — it says nothing about the bytes.

### 2. A log that will not replay AT ALL is stored state

The replay loop swallowed each failing row individually, so a log that would not
import at all produced a **hollow doc** — silently, on every wake, forever. Same
wedge a refusal names, minus the evidence and minus the reseed. That is the
2026-08-19 shape exactly: `snapshotBytes: 0`, 1.67 MB of update log.

The rows are now counted. When the replay produced *nothing* — no snapshot, and
not one row importable — the room refuses. Deliberately narrow: a log that fails
*behind* a snapshot that loaded keeps the old skip, because the doc still holds
the room's history and the next fold rewrites the log out of it; evicting there
would spend a good snapshot on a bad tail. Either way the skipped rows are now
logged, since a tail that never replays reads to its users as "my last messages
vanished" and read to this code as an ordinary cold start.

### 3. The alarm stops counting down to giving up

It sat at 21 of 24 consecutive failures for the whole incident. At 24 it gives up
permanently — no trim, no snapshot, no backup, until a client returns.

A `DocLoadRefused` is not an alarm failure. The guard has already counted it and
will evict within `LOAD_REFUSAL_LIMIT` attempts; the alarm's job is to keep
**arriving** until it does. So a refusal restores the pre-spent failure,
reschedules at the base delay so the strikes land in minutes rather than across
the ladder's hours, and is counted separately (`alarm.refusals`). Bounded at
`ALARM_REFUSAL_BUDGET` — twice the guard's own limit — so it cannot become an
unbounded once-a-minute chain. The give-up budget itself is unchanged: it is
sized against a platform outage (gh#373's six hours), and an outage is exactly
the failure that must **not** cost a room its history.

### What is deliberately not here

- **An export fault still loops loudly.** A doc that materializes and then fails
  to export is not a failed materialization, and destroying a room's history over
  an export problem is worse than looping — the line `ensureDoc` already draws.
  gh#557's `wasm.faults` is where that one is now read.
- **No new threshold.** `MAX_DOC_LOAD_BYTES` keeps its backstop job. It is now
  one input to the guard rather than its definition.

### Held by

`edge/test/workerd/session-load-guard.workerd.test.ts`, in real workerd against
the production `SessionRoom` and real SQLite: a small corrupt log refuses, evicts
and reseeds with `lastAbort` still null; a bad row *behind* a snapshot that
loaded is still skipped rather than evicted; an injected `RangeError` out of the
stored snapshot is answered `LOAD_REFUSAL_LIMIT` times — exactly
`WASM_POISON_ABORT_AFTER` — without reaching the tripwire at all
(`wasm.faults: 0`); and an alarm on the brink of its give-up budget spends none
of it on a refusal, then completes and clears itself once the guard has reseeded
the room.
