# The abort was the loop — **done** (gh#557)

The per-user workspace room `ws4/{orgId}/{userId}` poisoned its wasm heap,
aborted, auto-reset, and poisoned again — repeatedly on 2026-08-22, across a
manual reset, an automatic reset and an edge deploy. Every ticket so far had
treated a symptom: gh#527 gave it instrumentation, gh#553 made the manual escape
survivable, gh#554 proposed catching the corruption and evicting.

```
lastAbort: "wasm heap poisoned after 3 strikes: RangeError: Invalid array buffer length"
```

### The hypothesis, and what happened to it

The reading going in was that fragment reassembly was the corrupter. The
workspace doc is ~1.7MB against a 200_000-byte payload budget, so a full-doc
reseed goes up as nine frames; every reset is followed by exactly such a reseed;
and `RangeError: Invalid array buffer length` is what a bad length prefix looks
like when a buffer is reassembled rather than what an oversized-but-valid doc
looks like. `reset -> reseed -> fragmented -> corrupt -> poison -> reset`.

**Fragmentation is exonerated.** `edge/test/workerd/session-fragments.workerd.test.ts`
puts both of the required shapes through the production `SessionRoom` in real
workerd — one update above `FRAGMENT_BYTES`, and two devices reseeding at once
with their fragments interleaved — and both import byte-identically. Both pass
on the unfixed handler.

Two further things rule the shape out rather than merely failing to confirm it.
A Loro update carries a checksum, so a reassembly that *has* gone wrong is
rejected as `Decode error: Checksum mismatch`, not as an allocation the runtime
refuses. And the room's own numbers say the corrupt bytes never existed:
`pushOutcomes` showed 41 ok and 0 rejected from a single pusher, with
`importPenalty` empty. Nothing was being rejected, because nothing was wrong
with what devices sent.

### What was actually happening

`RangeError("Invalid array buffer length")` means one of two things, and they
want opposite cures:

1. the shared loro-wasm linear memory is exhausted — every SessionRoom in the
   isolate shares it, wasm memory only ever grows, and only recycling the
   isolate clears it. This is the 2026-08-04 incident the tripwire was built
   for, where every byte-**exporting** call threw while imports kept working;
2. one wasm call would not be served. Recycling the isolate does nothing for it.

`escalateWasmPoisoning` could not tell them apart, so it read every one as (1).
Three strikes call `ctx.abort()`, which severs every socket in the isolate —
including every co-located room's. The clients redial, the room reseeds, and the
reseed walks straight back into the same call. **The abort is what closes the
loop**, and it closes it whether or not the heap was ever unwell.

Three things made that reachable, and each is fixed here.

#### 1. The tripwire now asks the heap

`wasmHeapUsable()` builds a small `LoroDoc`, commits, exports and re-imports it.
Under the exhaustion the tripwire exists for, that cannot succeed — wasm memory
only grows and the poisoned state is permanent until the isolate dies. If it
does succeed, the heap is not the patient: the failure is recorded and **not**
struck, and nothing aborts. Use-after-free is exempt on purpose (a dangling
wrapper says nothing about the heap, and nothing in the instance recovers it).

The probe costs nothing on the healthy path — only a failure asks.

#### 2. `foldLog` had no catch

The log fold — snapshot re-export, then clear the update log — was the one wasm
export in this room with no guard of its own. The history trim above it has
always caught its own export and fallen back to the fold; the fold *is* that
fallback, and its own fallback was whatever caller happened to be on the stack:
the debounced flush timer, `webSocketClose`, `webSocketError`, the alarm. All
four hand the exception to `escalateWasmPoisoning`.

And a failed fold clears nothing, so the threshold that triggered it is still
crossed on the very next write. One room that could not export its snapshot
therefore aborted the isolate, then did it again, at write rate.

`edge/test/workerd/session-abort-loop.workerd.test.ts` reproduces that verbatim
against the unfixed room:

```
alarm failed room=? consecutiveFailures=1 RangeError: Invalid array buffer length
wasm heap poisoned (3 strikes); aborting isolate for a fresh heap
uncaught exception; jsg.Error: wasm heap poisoned after 3 strikes: RangeError: Invalid array buffer length
{ remote: true, durableObjectReset: true }
```

— the field's `lastAbort` string, from a single unexportable snapshot on a
demonstrably healthy heap.

Now the fold catches, records, and backs off on the alarm chain's ladder (a
minute, doubling, capped at an hour). The room keeps serving: reads, writes,
joins and relays never touched the fold. It simply cannot compact, and says so.

#### 3. An unimportable push was never answered

`applyUpdates` rethrew any wasm-shaped failure from `doc.import` of
client-supplied bytes, so the sender got no ack at all — it redialled and re-sent
the same payload. That is the reseed half of the loop, driven by the room. A
`RangeError` over a working heap is now a statement about the push: salvage the
rest of the batch, strike the device, answer `InvalidUpdate`.

### What an operator can now read

`/stats` gained two objects, because the old surface let this hide:

- **`fold`** — `consecutiveFailures`, `retryAt`, `lastFailure`. Before this,
  `snapshotBytes: 0` was the only trace a fold had never landed, and it reads
  identically to "nothing to fold yet".
- **`wasm`** — `faults`, `lastFault` (with the **call site** and whether it was
  struck), `abortAfterStrikes`. `lastAbort` carried the exception and no call
  site, which is how three tickets read the same `RangeError` without being able
  to say which of the room's half-dozen export paths produced it. Every
  `escalateWasmPoisoning` caller now passes a site name.

### Reassembly, hardened anyway

The reading turned up real gaps, just not this bug's. `edge/src/session-room.ts`
was the only one of the three reassemblers in the tree without an index bounds
check, a distinct-index count, or a size check — `on_fragment` in
`crates/sync/src/room.rs` and `onFragment` in `apps/ios/Comet/Sync/RoomClient.swift`
have all three. It also preallocated to the header's declared size and `set()`
into it, where both clients concatenate, so a short assembly was silently
zero-padded rather than impossible.

Consequence, today: a repeated fragment could complete a batch that was still
missing one, and the buffer that assembled — short by a fragment, zero-padded at
the tail, everything after the hole shifted 200_000 bytes early — was pushed
through `doc.import` and the failure charged to the device that had sent its
fragments correctly. Now the batch stays open until the fragment that is
actually missing arrives, and a batch whose header and fragments genuinely
disagree is refused at the seam without a wasm call.

### What this does not claim

Which wasm call threw on 2026-08-22 is not settled from here: that needs the
room's `wasm.lastFault.site`, which did not exist until this change. What is
settled is that the room could not tell a sick heap from a call it could not
serve, and that reading it wrong is what made every reset temporary.

### How this was verified

`node` and `npm` are **not on a dispatched agent's PATH** on the box, and
`edge/node_modules` does not survive between sessions. Both suites run fine
once that is supplied — this is a PATH fact, not a "the workerd tier cannot run
here" fact, and the difference matters because the second reading would make
this change CI-verified rather than author-verified:

```
cd edge
PATH=/opt/homebrew/bin:$PATH npm ci
PATH=/opt/homebrew/bin:$PATH npm run typecheck   # clean
PATH=/opt/homebrew/bin:$PATH npm test            # 101 unit + 35 workerd, all green
```

Red-before-green was run three ways, because the claims are separable and a
single crude revert proves none of them.

**Do not revert `session-room.ts` wholesale to check this.** The test file
imports `FRAGMENT_BYTES`, which this change is what exports — so a whole-file
swap makes all nine tests fail on a missing binding, which looks like a
behavioural red and is not one. Revert the specific behaviour instead:

1. **Reassembly only** (restore the arrival count and the `new Uint8Array()`
   placeholder, keep everything else): the two happy-path tests **pass**, the
   three guard tests fail. This is the exoneration — the required shapes import
   byte-identically through the *unfixed* reassembler.
2. **The tripwire's discriminator only** (`wasmHeapUsable()` → `false`, i.e. the
   pre-gh#557 rule, with the fold catch, the backoff, `/stats` and the
   reassembly guards all left in place): the room aborts anyway —
   `wasm heap poisoned (3 strikes); aborting isolate`, `durableObjectReset:
   true`. The backoff test still passes. **That is the load-bearing result**:
   guarding the fold lowers the rate, but the abort policy is what closes the
   loop, and only the discriminator opens it.
3. **Both files against the pre-fix source**: all nine fail, and the two files
   take each other down — the abort-loop tests' `ctx.abort()` kills the isolate
   the fragment tests are using. That is this bug's blast radius reproduced by
   accident, and it is the same mechanism behind the chat rooms churning
   alongside the workspace room.
