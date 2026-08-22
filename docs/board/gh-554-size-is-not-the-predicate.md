# Size is not the predicate — **done** (gh#554)

The load guard from gh#527 (#550) did not fire during the incident it was
written for. Here is the workspace room's `/stats` at the moment both engines
and the phone were unable to hold a join:

```
docLoad: { guardBytes: 33554432, storedBytes: 1749030, refusals: 0, limit: 3 }
lastAbort: { reason: "wasm heap poisoned after 3 strikes: RangeError: Invalid array buffer length" }
```

Zero refusals, because the doc was 5% of the size threshold. The guard is
size-only; the failure was corruption. So the escalation that actually ran, all
evening, was the wasm-poisoning tripwire — `ctx.abort()`, every attached socket
1006 with no close frame, and a cold start that replays the same persisted bytes
into the same throw. The self-healing path existed and was never reachable.
`diedYoung: 505`, `vanished: 368`, `alarm.consecutiveFailures: 21` of 24,
`lastTrimAt: ""`, `snapshotBytes: 0`. It ended with a manual `POST /reset-log`.

#550's premise is right — a room that cannot be loaded should be refused, and
after `LOAD_REFUSAL_LIMIT` strikes evicted and reseeded. This is the predicate
that decides whether it ever runs.

### The two escalations answer the same error

`RangeError("Invalid array buffer length")` is the signature of an exhausted
loro-wasm heap. It is also what a truncated snapshot throws on import. The
remedies are opposite:

- **poisoning-strikes → `ctx.abort()`** recycles a sick heap and **keeps** the
  stored state;
- **load-refusal → evict-and-reseed** keeps the isolate and **drops** the stored
  state.

Answer a corrupt log with an abort and the next cold start replays it — that is
the loop above. Answer a pressed heap with a reseed and a healthy room loses its
history because a co-located whale exhausted the isolate. The error's shape
cannot tell you which you have, and `ensureDoc` was branching on exactly that.

### Ask the heap instead of reading the error

`wasmHeapHealthy()` allocates and exports 256 KB on a throwaway doc. Under the
poisoning this tripwire was built for, every byte-exporting call throws, so the
probe throws too; on a healthy heap it costs a couple of milliseconds and proves
the fault belongs to the caller's bytes. It runs only on a path that is already
failing.

That single question is `isHeapFault`, and it now sits in front of every place
the old shape-test lived:

- **`ensureDoc`'s snapshot import** refuses instead of rethrowing, unless the
  heap agrees it is the heap;
- **`ensureDoc`'s update-log replay** counts the rows that fail instead of
  swallowing them one at a time. When the replay produces *nothing* — no
  snapshot, and not one row of the log would import — that is not a poisoned row
  but stored state this instance cannot materialize, rebuilt empty on every wake
  forever. It refuses. That is the gh#554 shape exactly: the incident room was
  1.67 MB with `snapshotBytes: 0`. Deliberately narrow — a log that fails
  *behind* a snapshot that loaded keeps the old skip-and-carry-on, because the
  doc still holds the room's history and the next fold rewrites the log out of
  it; evicting there would spend a good snapshot on a bad tail. Either way the
  skipped rows are now logged, since a tail that never replays reads to its users
  as "my last messages vanished" and read to this code as an ordinary cold start;
- **`escalateWasmPoisoning`** spares the isolate when the heap answers, and says
  so durably (`heapSpared`, `lastHeapSpare` on `/stats`) — a non-zero count next
  to a zero-refusal `docLoad` is the reading the incident never got. It points at
  the room, where `lastAbort` pointed at an isolate that was fine. A dangling
  wasm wrapper keeps its unconditional strike: the probe cannot see memory this
  instance already freed, and the recycle is the only recovery;
- **`handleDocUpdate`** stops aborting the isolate over a device that pushed
  bytes loro chokes on. That is an ordinary bad update — the salvage pass and the
  import penalty box already exist for it.

### The alarm stops counting down to giving up

The alarm sat at 21 of 24 consecutive failures for the whole incident. At 24 it
gives up permanently — no trim, no snapshot, no backup, until a client returns.
Three more attempts against a room the guard could have reseeded in three
minutes.

A `DocLoadRefused` out of the alarm is not an alarm failure. The guard has
already counted it and will evict within `LOAD_REFUSAL_LIMIT` attempts; the
alarm's job is to keep **arriving** until it does. So a refusal restores the
pre-spent failure, reschedules at the base delay so the strikes land in minutes
rather than across the retry ladder's hours, and is counted separately
(`alarm.refusals`). Bounded at `ALARM_REFUSAL_BUDGET` — twice the guard's own
limit — so it cannot become an unbounded once-a-minute chain; past it the
ordinary ladder resumes. The give-up budget itself is unchanged, because it is
sized against a platform outage (gh#373's six hours), and an outage is exactly
the failure that must **not** cost a room its history.

### What is deliberately not here

- **An export fault still loops loudly.** A doc that materializes and then fails
  to export is not a failed materialization, and destroying a room's history over
  an export problem is worse than looping — the line `ensureDoc` already draws.
  What changes is that it no longer aborts the isolate and no longer reads as a
  heap emergency: it lands in `heapSpared` with the room named.
- **No new threshold.** `MAX_DOC_LOAD_BYTES` stays where it is and keeps its
  backstop job. It is now one input to the guard rather than its definition.

### Held by

`edge/test/workerd/session-load-guard.workerd.test.ts`, in real workerd against
the production `SessionRoom` and real SQLite: a small corrupt log refuses, evicts
and reseeds with `lastAbort` still null; a bad row *behind* a snapshot that
loaded is still skipped rather than evicted; an injected `RangeError` out of the
stored snapshot is answered `LOAD_REFUSAL_LIMIT` times — exactly
`WASM_POISON_ABORT_AFTER` — without a single poisoning strike; and an alarm on
the brink of its give-up budget spends none of it on a refusal, then completes
and clears itself once the guard has reseeded the room.
