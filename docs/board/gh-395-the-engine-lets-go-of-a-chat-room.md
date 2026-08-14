# The engine lets go of a chat room — **done** (gh#395)

`DocHost` kept its handles in an insert-only map. Grep the file as it stood:
`get`, `insert`, `values`, `keys` — no `remove`, and no eviction anywhere. Every
chat the engine had ever opened kept its handle, and therefore its `RoomClient`
websocket to the edge, for the life of the process. The comment at
`doc_host.rs:384` promised that re-opening hands back the same handle, which is
the intended caching. Nothing ever ended the cache entry.

### What it cost, measured 2026-08-14

A Mac engine restarted at 19:10 had **20 chat rooms** by 19:20. Ten minutes.
`comet-board doctor` on that engine, with the edge down:

```
FAIL edge connections   0 of 23 live — device room, workspace room,
                        org registry down, 20 of 20 chat room(s) down
```

45 s of `wrangler tail comet-native-edge`, 74 requests, by path shape:

```
  52  session/ws      <- per-chat rooms: 70% of all edge traffic
  13  device/ws
   4  org/ws
   3  device/status
   2  workspace/ws
```

Per-chat rooms were the dominant load on the edge, and the count only grew with
uptime — a box left running for a day carried a day's chats. That is the
multiplier behind the recurring Durable Objects free-tier trips (gh#126,
gh#373): every one of those rooms pings, probes and redials on its own 30 s
backoff, forever, for chats nobody has looked at since morning.

gh#373's arithmetic is the reason this is worth doing rather than merely tidy.
Duration is the only billed dimension within an order of magnitude of a real
charge (76% of the Workers Paid allowance at the time), and duration is
per-socket work: imports, replays, and the wake-ups a standing connection
implies. Rooms nobody is using are the cheapest possible thing to stop paying
for.

### The seam that makes releasing safe

`DocHost::open` already reconstructs a handle from the SQLite snapshot on
demand. A released chat is therefore not a closed chat — it is a chat whose
handle will be rebuilt the moment anyone asks, and the caller cannot tell.

The other half was already designed for this, three years of comments deep:
a command for a chat this device hosts arrives by **nudge** (`POST
/device/{host}/nudge`), and the host's relay opens the doc, which drains the
queue. `open()`'s own comment says it — *"the command executes with no standing
per-chat socket."* The cold path was always the real delivery path. The standing
socket was never what made it work.

### The policy

Two rules, in `rooms_to_release` — a pure function over `(chat_id, last_used,
pinned)`, so the decision is testable without a clock, a socket or a doc:

- anything unpinned and unused for **5 minutes** is released;
- if more than **32** chats would still be open, the least recently used
  unpinned ones go until the cache is back under the bound.

The bound sits above the boot warm-open's cap of 30 on purpose: warm-open is a
deliberate burst that exists to drain commands queued while the box was down,
and trimming it the moment it lands would defeat it. The idle sweep is what
gives those chats back, once they have done what they were opened for.

`last_used` moves on an `open()` — hit or miss — and on any doc change, local
commit or import from the room. A chat that is quietly syncing is a chat in use.

### What "in use" means, and why each one is asked

A chat is **pinned** — never released, at any age — when any of these holds:

- **someone is watching it.** A `WatchDocMessages` stream is fed by the handle's
  `messages_tx`, and `watch_stream` ends when that sender drops. Releasing a
  watched chat would cut a live viewer's transcript off mid-scroll. The sweep
  asks `receiver_count()` rather than letting the UI discover it.
- **a run is live on it.** `SessionsEngine::chat_is_busy` — the `runs` map first
  (a dispatch is live there before its status transition lands), then `Working`
  or `AwaitingInput`. This one cannot be derived from the cache: a run streams
  through an `Arc<SessionDoc>` of its own, not through the handle, so the handle
  looks untouched while a turn is pouring through it.
- **a caller is holding the handle right now.** `Arc::strong_count > 1`; the
  map's own reference is the 1.

The sweep asks the first and third under the handles lock and the second with it
released — a dispatch walks sessions → doc host, and this lock must never be
held facing back. The decision is then re-checked under the lock before anything
is removed: an `open()` between the passes either handed the handle out or moved
the clock, and either way the chat is in use again.

Two orderings in the release itself are load-bearing:

- the handle is dropped **before** the snapshot is exported. Dropping it is what
  ends the room supervision (`spawn_room_join` holds only a `Weak` to the room
  slot) and the per-chat task, so by export time nothing can still be importing
  changes behind the writer.
- `is_host` now reads the host stamp off the handle the drain already holds
  instead of looking the chat up in the cache. The old form would have answered
  "unclaimed" for a released chat, and "unclaimed" reads as claimable — which on
  a teammate's laptop is how a chat shared into the org gets executed twice
  (gh#66). The drain always has the handle; nothing needed the lookup.

`room_census` needed no change and stays honest: a released chat is in neither
number, which is the true answer — it has no socket because it is not meant to
have one. The gap between open and live keeps meaning what it meant, rooms that
are down.

### Verification

The policy has direct unit tests (`crates/engine/src/doc_host.rs`): the pure
decision over idle/pinned/over-bound candidates, and the cache itself — a chat
released and re-opened with its transcript intact, a watched chat kept, a held
chat kept, idleness measured from the last use rather than the first, and the
bound taking the oldest. `chat_is_busy` has its own table in `sessions.rs`,
because reading a settled chat's lingering `Idle` row as busy would have made
every chat this engine ever ran permanently un-releasable.

**Not verified against a live edge.** The edge is currently failing 100% of
requests on the free-tier duration cap, so the thing this change is actually for
— the per-chat socket count on `wrangler tail` falling away as chats go idle —
could not be observed end to end. What is outstanding is exactly one measurement:
re-run the 45 s tail and the `doctor` census on a box that has been up for an
hour, and check that `session/ws` is no longer 70% of the traffic and that the
chat-room count tracks the chats in use rather than the chats ever opened.
