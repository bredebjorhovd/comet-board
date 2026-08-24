/**
 * SessionRoom — one Durable Object per doc room, speaking loro-protocol over
 * hibernatable WebSockets (design §2, §3.1). Two doc kinds share this class:
 * chat session docs (room name = chatId, claim-on-first-join ownership, plus
 * the org-shared opt-in below) and org-wide docs (the per-user workspace doc
 * `ws4/{orgId}/{userId}` and the org device registry `orgdev1/{orgId}` —
 * org-membership authz enforced by the Worker, so the DO sees the
 * ROOM_KIND_HEADER stamp and skips ownership entirely).
 *
 * Chat sharing (gh#66): a chat room also records the org that claimed it and a
 * `shared` flag the owner sets through `POST /share`. Shared is how a board
 * dispatch — work the box did on the team's behalf — becomes readable and
 * steerable by every member of the org, without making anyone's private chats
 * org-visible. See [`chatRoomAccess`].
 *
 * Persistence model:
 * - `updates` — append-only incoming update log, buffered in memory during
 *   active streams and flushed every ~DO_FLUSH_MS (a crash losing buffered
 *   ops is healed by normal CRDT resync from the host on reconnect). Updates
 *   above the ~2MB SQL row cap span chunked continuation rows (update-log.ts).
 * - `snapshot` blob — the doc's current snapshot. Two-level compaction:
 *   LOG FOLD (whenever the update log passes COMPACT_LOG_BYTES): re-export a
 *   full snapshot and clear the log — loses nothing. HISTORY TRIM (daily
 *   alarm): once a recorded frontier checkpoint is older than RETAIN_DAYS,
 *   re-export a *shallow* snapshot at that frontier — trimmed op history is
 *   discarded permanently, state is fully preserved (§3.1).
 * - `tail` blob — materialized last-N-messages JSON, recomputed lazily on
 *   GET /tail when dirty (§5 L2).
 * - `diff` blob — latest-only working-tree diff sidecar, overwritten on each
 *   host publish (§6.1).
 * - Ephemeral presence (%EPH room) is memory-only by construction, and is
 *   DERIVED from the socket set rather than written by clients — see
 *   [`livePresence`] and `publishPresence` (gh#145).
 *
 * Hibernation discipline: no wall-clock JS timers except the flush debounce
 * (which only exists while traffic keeps the DO awake anyway); scheduled work
 * (checkpoints, history trim, R2 backup §3.3) rides the durable alarm. That
 * alarm is the only thing here that can call this object with no client behind
 * it, so it is also the only thing that needs a budget of its own: see
 * ALARM_FAILURE_LIMIT and `alarm()` (gh#378).
 *
 * THREE FAULT CLASSES, and they want three different cures. Reading one as
 * another is what every incident in this file's history has actually been.
 * - The stored doc will not materialize → [`DocLoadRefused`]. Refuse the join,
 *   count it, evict and reseed at [`LOAD_REFUSAL_LIMIT`]. Never a wasm strike.
 * - The wasm HEAP is exhausted → poison strikes and, past
 *   `WASM_POISON_ABORT_AFTER`, `ctx.abort()`. Asked of the heap by
 *   [`wasmHeapUsable`], never inferred from the shape of one exception.
 * - ONE wasm object is wedged, the heap around it fine →
 *   [`DocBorrowConflict`] (gh#607). Drop the wrapper, count it, reseed on the
 *   same budget — and never abort, because recycling an isolate over one
 *   object is the loop gh#557 exists to have stopped.
 *
 * That discipline is why presence is derived here. Until gh#145 every engine
 * wrote `presence/{deviceId} → now` into this room's ephemeral store every 15s.
 * A `%EPH` frame is a real message, so it WAKES the object — a room with any
 * device attached therefore never hibernated, and one such object awake around
 * the clock bills 86,400 × 0.125 GB = 10,800 GB-s/day, 83% of the daily free
 * tier before an agent runs. Two of these rooms exist whenever anyone is signed
 * in (the per-user workspace doc and the org device registry), which is exactly
 * the shape of the 2026-08-07/08 overruns. `DeviceRoom` was the control group at
 * 434× less duration for comparable traffic, and the reason is precisely that it
 * derives liveness from `getWebSocketAutoResponseTimestamp` — a stamp the
 * runtime maintains WHILE HIBERNATING, for free. This room now does the same.
 */
import { LoroDoc, EphemeralStore, VersionVector, type PeerID } from "loro-crdt";
import {
  CrdtType,
  JoinErrorCode,
  MAX_MESSAGE_SIZE,
  MessageType,
  RoomErrorCode,
  UpdateStatusCode,
  bytesToHex,
  decode,
  encode,
  type DocUpdate,
  type DocUpdateFragmentHeader,
  type JoinRequest,
  type ProtocolMessage
} from "loro-protocol";
import {
  COMPACT_LOG_BYTES,
  COMPACT_LOG_ROWS,
  DO_FLUSH_MS,
  RETAIN_DAYS,
  materializeTail
} from "./session-doc";
import { AlarmArmer } from "./alarm";
import { createBlobStore, getJsonBlob, putJsonBlob, type BlobStore } from "./blobs";
import { createMetaStore, type MetaStore } from "./meta";
import { appendUpdateRow, ensureUpdateLog, readUpdateRows } from "./update-log";
import {
  YOUNG_SOCKET_MS,
  createSocketLedger,
  type SocketDeath,
  type SocketLedger
} from "./socket-log";
import { AUTH_ORG_HEADER, AUTH_USER_HEADER, ROOM_KIND_HEADER, type Env } from "./env";

const DAY_MS = 24 * 60 * 60 * 1000;
const RETAIN_MS = RETAIN_DAYS * DAY_MS;
/** Consecutive cold-replay deaths (CPU-limit kills mid-`ensureDoc`) before the
 * room concludes it is wedged and drops its own log — see `ensureDoc`. */
const REPLAY_CRASH_LIMIT = 3;
/** Payload bytes per outbound fragment (leaves room for the envelope).
 * Mirrored by every client that fragments into this room — `FRAGMENT_BYTES` in
 * crates/sync/src/room.rs and `fragmentBytes` in apps/ios. */
export const FRAGMENT_BYTES = 200_000;
/** Maximum Loro operations exported by one ordinary join event.
 *
 * `sendUpdates` bounds WebSocket FRAME size, but it runs after Loro has
 * synchronously walked and encoded the requested history. A causally complex
 * workspace can hold years of small operations in a compact snapshot, so one
 * fresh-device `export({ mode: "update" })` crossed the DO CPU limit before
 * the first fragment existed (gh#452). Keep the expensive wasm call bounded
 * by operation count; `RoomError(RejoinSuggested)` asks the client to send its
 * newly advanced VV and collect the next prefix on the same hibernatable
 * socket. */
export const BACKFILL_OP_BUDGET = 512;
/** Reject inbound fragment batches above these at the HEADER, before any
 * reassembly buffer exists — bounds DO memory against a runaway or hostile
 * sender. Comfortably above the 25MB session soft ceiling; the Rust client
 * applies the same discipline inbound (crates/sync MAX_FRAGMENT_COUNT). */
const MAX_REASSEMBLED_BYTES = 32 * 1024 * 1024;
const MAX_FRAGMENT_COUNT = 4096;
/** Keep a rolling ~5 weeks of daily frontier checkpoints. */
const MAX_CHECKPOINTS = 36;
/** Consecutive failed alarms before this room stops rescheduling its own
 * chain (gh#378). The daily alarm was the ONE unbounded call source here:
 * everything else is client-driven and self-limiting (the Rust client caps
 * reconnect backoff at 30s), while a throwing alarm left `backupDirty` set and
 * retried on the runtime's backoff forever — with nobody connected to notice,
 * and on a paid plan billing requests and duration on every attempt.
 *
 * The number is chosen against the failure it must survive: gh#373's edge
 * outage lasted six hours, and a room whose writes are impossible for a day
 * should still heal itself once they are possible again. On the ladder below
 * 24 attempts span ~18h, which clears that with room, and caps a permanently
 * broken room at 24 invocations instead of an open account. */
export const ALARM_FAILURE_LIMIT = 24;
/** Retry ladder after a failed alarm: doubling from a minute, capped at an
 * hour. Fast at first (a transient storage/R2 blip heals in minutes and the
 * backup should not wait a day for it), then slow enough that a long outage
 * is measured in a couple of dozen attempts rather than hundreds. */
const ALARM_RETRY_BASE_MS = 60_000;
const ALARM_RETRY_MAX_MS = 60 * 60 * 1000;
/** Delay before retry number `failures + 1`, given the consecutive failures so
 * far. Pure, so the ladder is testable without a DO (like [`livePresence`]). */
export const alarmRetryDelay = (failures: number): number =>
  Math.min(ALARM_RETRY_BASE_MS * 2 ** Math.max(failures - 1, 0), ALARM_RETRY_MAX_MS);
/** Isolate-wide poisoned-wasm strike counter — MODULE state on purpose: every
 * SessionRoom co-located in this isolate shares ONE loro-wasm linear memory,
 * so heap exhaustion poisons them all at once (2026-08-04: every
 * byte-exporting wasm call threw RangeError("Invalid array buffer length")
 * while imports/relays kept working, silently wedging all joins fleet-wide). */
let wasmPoisonStrikes = 0;
/** Wasm-boundary failures before `ctx.abort()` recycles the isolate. Wasm
 * memory only ever grows, so the poisoned state is permanent until the isolate
 * dies — and Cloudflare's own memory-limit reset arrives only after minutes of
 * thrash. Aborting early turns a silent hours-long wedge into a seconds-long
 * blip (sockets close, clients redial into a fresh isolate). */
const WASM_POISON_ABORT_AFTER = 3;
/** How long the abort waits for its own reason to become durable (gh#527).
 * Short: the abort is the point, the marker is a courtesy to whoever reads the
 * room next. Long enough for one storage sync on a healthy DO. */
const ABORT_SYNC_GRACE_MS = 250;
/** Free a quiet room's materialized doc after this long. Wasm linear memory
 * NEVER shrinks and outlives DO instances (one wasm module per isolate), so
 * docs held resident until instance eviction leak permanently — every
 * reconnect herd rematerialized every co-located room and the heap climbed
 * monotonically to the isolate limit in minutes (2026-08-04 thrash loop).
 * Freeing on idle returns blocks to the wasm allocator for reuse;
 * rematerialization is a 10-50ms cold replay. */
const DOC_IDLE_RELEASE_MS = 60_000;
/** Force-trim threshold: a room with NO trim-eligible checkpoint but a
 * full-history snapshot this large trims at its CURRENT frontier instead of
 * waiting days to age into RETAIN_DAYS eligibility. Behind/concurrent peers
 * take the §3.1 stale-peer full resync (designed-for). Without this, the
 * 2026-08-04 whale rooms (954KB / 1.8MB import chats, checkpoints first
 * recorded today) would have kept re-materializing their full history into
 * the pressed wasm heap for three more days of thrash. */
const TRIM_FORCE_BYTES = 512 * 1024;
/** Import penalty box (see `importPenalty`): consecutive failed %LOR imports
 * before a device's pushes are short-circuited, and for how long. */
const IMPORT_PENALTY_STRIKES = 3;
const IMPORT_PENALTY_MS = 10 * 60 * 1000;
/** While penalized, payloads at or under this size still get one import
 * attempt: a healed (re-flattened) device's first small status/title write
 * clears its box immediately instead of serving out IMPORT_PENALTY_MS in
 * silence — the box exists to stop ~1MB doomed reassemblies, and a bounded
 * small import costs ~nothing even when it fails. */
const PENALTY_PROBE_MAX_BYTES = 4096;
/** Persisted bytes (snapshot + update log) one cold materialization may
 * attempt to import (gh#527).
 *
 * The failure this bounds is the one that cannot be caught: a doc big enough
 * that importing it exceeds the DO's CPU/memory budget takes the whole
 * invocation with it, so `ensureDoc` never returns, no catch runs, and the
 * client sees a socket that answered the join and then died 1006 — the same
 * signature as the duration cap and the `ctx.abort()` escalation, which is why
 * the 2026-08-19 incident could not tell the three apart. A room past this
 * line therefore refuses to load AT ALL, cheaply and before any wasm call, and
 * says so ([`DocLoadRefused`] → `JoinError`): a clean refusal a client backs
 * off from is strictly better than an abort loop nobody can read.
 *
 * The number is the inbound bound this room already enforces
 * (`MAX_REASSEMBLED_BYTES`): no single push above 32MB is accepted, so a
 * persisted doc above it is not something the fleet can legitimately have
 * produced. Real rooms live three orders of magnitude below it — the
 * 2026-08-04 whale imports were ~2MB and TRIM_FORCE_BYTES presses rooms toward
 * 512KB — so this is a backstop, not a policy. */
export const MAX_DOC_LOAD_BYTES = 32 * 1024 * 1024;
/** Consecutive load refusals before the room evicts its own stored state and
 * lets the fleet reseed it (gh#148/#207).
 *
 * Same shape and the same reasoning as `REPLAY_CRASH_LIMIT`: a room whose
 * stored doc cannot be loaded is a room no client can ever repair by dialing
 * it, and every engine holds the full doc locally and re-uploads what the
 * server lacks on its next join. Bounded so a refusal that heals on its own
 * (a corrupt blob replaced by a fold) never costs the room its history. */
export const LOAD_REFUSAL_LIMIT = 3;
/** Alarm attempts that may be answered by a load REFUSAL without spending the
 * give-up budget (gh#554).
 *
 * `ALARM_FAILURE_LIMIT` is sized against an OUTAGE — a day of impossible writes
 * that heals when the platform does (gh#373's six hours). A doc that will not
 * load is the opposite kind of failure: it does not heal by waiting, and the
 * room already owns the escalation that fixes it (`LOAD_REFUSAL_LIMIT` → evict
 * and reseed). The 2026-08-19 room sat at 21 of 24 consecutive failures against
 * exactly that — three attempts from giving up on its trim, snapshot and backup
 * permanently, for a doc the guard could have reseeded in three minutes. So a
 * refusal reschedules at the base delay and leaves the budget alone.
 *
 * Bounded at twice the guard's own limit so this cannot become an unbounded
 * once-a-minute chain: past it the ordinary ladder resumes. Cleared by any
 * completed alarm. */
export const ALARM_REFUSAL_BUDGET = LOAD_REFUSAL_LIMIT * 2;

/** The room refuses to materialize its doc — too large to import, or stored
 * bytes that will not decode. Carried out to the join as a `JoinError` rather
 * than left to kill the invocation, which is the whole point (see
 * [`MAX_DOC_LOAD_BYTES`]). Never a wasm-poisoning strike: nothing about this
 * says the heap is unwell. */
export class DocLoadRefused extends Error {
  constructor(
    readonly detail: string,
    readonly bytes: number
  ) {
    super(`doc load refused (${bytes}B): ${detail}`);
    this.name = "DocLoadRefused";
  }
}

/** THE THIRD FAULT CLASS (gh#607): a wasm-bindgen value stuck BORROWED.
 *
 * Not `DocLoadRefused`'s cousin by accident — it extends it, because the answer
 * is the same one and every guard downstream already knows how to give it: say
 * so on the wire as a `JoinError` the client backs off from, count it toward
 * [`LOAD_REFUSAL_LIMIT`] so the evict-and-reseed runs, spend no alarm budget
 * (see [`ALARM_REFUSAL_BUDGET`]). What it must NOT be is a wasm-poisoning
 * strike: the heap is fine, ONE object in it is unusable, and `ctx.abort()`
 * over that is the isolate-recycle loop gh#557 exists to have stopped.
 *
 * Distinct from `DocLoadRefused` all the same, and named, because the cure has
 * an extra half: the poisoned WRAPPER has to be dropped from the cache too, or
 * the next join reuses the same stuck object and the fault is permanent for the
 * life of the instance (`isLive` cannot see it — see [`isWasmBorrowConflict`]). */
export class DocBorrowConflict extends DocLoadRefused {
  constructor(
    readonly site: string,
    readonly fault: unknown,
    bytes: number
  ) {
    super(`wasm borrow conflict at ${site}: ${String(fault)}`, bytes);
    this.name = "DocBorrowConflict";
  }
}

/** A wasm-bindgen wrapper whose `free()` already ran has `__wbg_ptr === 0`;
 * any method call on it throws `Error("null pointer passed to rust")`. Several
 * flows (trim, fold, alarm, idle release) free-and-replace the cached doc
 * around `await`s, so a stale wrapper can outlive its wasm memory. 2026-08-04
 * evening: one such interleaving left `this.doc` dangling in a live instance —
 * every join/update/tail on the ws3 workspace room threw for 2.5h fleet-wide,
 * and nothing recycled the instance because the error is neither a RangeError
 * nor a RuntimeError (the wasm-poison tripwire ignored it). Check liveness
 * before every reuse; rematerialization is a ~tens-of-ms cold replay.
 *
 * NOT a sufficient precondition, and gh#607 is why: a wrapper can be perfectly
 * live and still unusable, because the borrow flag this asks nothing about
 * lives in wasm linear memory and is unreadable from JS (see
 * [`isWasmBorrowConflict`]). There is no predicate for that one — only a catch,
 * and then `abandonDoc`. */
const isLive = (obj: unknown): boolean =>
  (obj as { __wbg_ptr?: number } | undefined)?.__wbg_ptr !== 0;
/** A wasm-bindgen value whose `WasmRefCell` still has an outstanding borrow
 * (gh#607). `free()` — and any by-value method — calls `WasmRefCell::take`,
 * which refuses with `attempted to take ownership of Rust value while it was
 * borrowed`; a `&mut self` method on the same value answers `recursive use of
 * an object detected …`. Both mean one object is wedged, and neither is
 * anything the tripwires above can see.
 *
 * HOW A BORROW OUTLIVES ITS CALL, since the answer is not "our code kept one".
 * wasm-bindgen scopes each borrow to a single exported call with a Rust guard,
 * and JS is single-threaded, so no `await` of ours can interleave one. What CAN
 * end a wasm frame without dropping its guard is an exception that unwinds
 * THROUGH it: wasm has no destructor unwinding for a JS throw, and every
 * borrowing loro call re-enters JS while borrowed (`toJSON` builds a JS Map;
 * `export`/`import` read their options object back through serde). So on a
 * pressed isolate — this room's whole history — a `RangeError: Invalid array
 * buffer length` raised in one of those JS callbacks leaves the borrow set
 * FOREVER on that object. Same for a CPU-limit kill landing mid-call, which is
 * the failure `MAX_DOC_LOAD_BYTES` documents and this room keeps taking.
 *
 * Two things make it invisible to everything built so far. `__wbg_ptr` is still
 * a live pointer, so [`isLive`] passes it. And the message is a plain `Error` —
 * not a `RangeError`, not a `WebAssembly.RuntimeError`, not one of
 * [`isWasmUseAfterFree`]'s strings — so `escalateWasmPoisoning` returns having
 * counted nothing and the generic catch answers `internal error`.
 *
 * It is also, quietly, a permanent leak: `free()` zeroes `__wbg_ptr` and
 * unregisters the FinalizationRegistry entry BEFORE calling into wasm, so a
 * throwing free abandons the Rust value with nothing left to reclaim it — not
 * a later free, not GC. Every occurrence is megabytes the isolate keeps until
 * the runtime resets it at the 128MB cap, severing every co-located room.
 *
 * Our own classified error carries the signature in its message, so exclude it:
 * re-classifying a `DocBorrowConflict` would double-count the refusal. */
export const isWasmBorrowConflict = (e: unknown): boolean =>
  e instanceof Error &&
  !(e instanceof DocLoadRefused) &&
  /attempted to take ownership of Rust value while it was borrowed|recursive use of an object detected/i.test(
    e.message
  );
/** Isolate-wide count of wasm values this module could not free — MODULE state
 * for the same reason as `wasmPoisonStrikes`: the leak is in the one linear
 * memory every co-located SessionRoom shares, so the number that matters is the
 * isolate's, not the room's. Surfaced on `/stats` beside the per-room count. */
let wasmBorrowLeaks = 0;
/** What a [`freeWasm`] attempt did. `leaked` is the one worth reading: the
 * value is gone from JS and still resident in wasm, permanently. */
type FreeOutcome = "freed" | "dangling" | "leaked" | "failed";
/** Release a wasm-bindgen value, and NEVER throw at the caller (gh#607).
 *
 * Every `free()` in this file lives in a `finally` or a cleanup step, which is
 * precisely where a throw does the most damage: it replaces the in-flight
 * exception with its own — so the `RangeError` the guards were built to read
 * arrives at the catch as `attempted to take ownership…` instead, matching
 * nothing — and it skips every free after it, which is how one fault turns into
 * several megabytes. Both halves of the 2026-08-24 `ws4` loop are that.
 *
 * A free that cannot happen is still worth saying out loud, so it is logged and
 * counted rather than swallowed. */
const freeWasm = (value: { free(): void } | undefined, site: string): FreeOutcome => {
  if (value === undefined || !isLive(value)) return "dangling";
  try {
    value.free();
    return "freed";
  } catch (e) {
    if (isWasmBorrowConflict(e)) {
      wasmBorrowLeaks++;
      console.error(
        "wasm value could not be freed (stuck borrow); its memory is leaked for this isolate",
        `site=${site}`,
        `isolateLeaks=${wasmBorrowLeaks}`,
        String(e)
      );
      return "leaked";
    }
    console.error("wasm free failed", `site=${site}`, String(e));
    return "failed";
  }
};
/** The use-after-free / detached-buffer signatures of a dangling wasm wrapper
 * — same terminal shape as heap poisoning (nothing in-instance recovers it),
 * so the tripwire must count these too. */
const isWasmUseAfterFree = (e: unknown): boolean =>
  e instanceof Error && /null pointer passed to rust|detached ArrayBuffer/i.test(e.message);
/** The error shapes that come off the loro-wasm boundary rather than out of our
 * own code — the set `escalateWasmPoisoning` and `handleDocUpdate` each spell
 * inline, named here because `ensureDoc` now asks the same question (gh#554). */
const isWasmShaped = (e: unknown): boolean =>
  e instanceof RangeError || e instanceof WebAssembly.RuntimeError || isWasmUseAfterFree(e);
/** Does loro-wasm still work? Asked of the heap, not of the document (gh#557).
 *
 * `RangeError("Invalid array buffer length")` means one of two things and they
 * want OPPOSITE cures: an exhausted linear memory, which only an isolate
 * recycle clears, or one call the runtime would not serve, which a recycle
 * does nothing for. The tripwire below could not tell them apart, so it read
 * every one as the first — and on 2026-08-22 the `ws4` workspace room aborted
 * its isolate over and over on that reading, severing every co-located room's
 * sockets each time, while the reseed that followed walked it straight back
 * (gh#527/#378/#553/#554 all chased a symptom of that loop).
 *
 * A fresh round trip through the allocator settles it. Under the exhaustion
 * this tripwire exists for, wasm memory only ever grows and the poisoned state
 * is permanent until the isolate dies — so even these few hundred bytes cannot
 * be allocated, committed, exported and re-imported. If they can, the heap is
 * not the patient. Costs nothing on the healthy path: only a failure asks. */
const wasmHeapUsable = (): boolean => {
  let probe: LoroDoc | undefined;
  let echo: LoroDoc | undefined;
  try {
    probe = new LoroDoc();
    probe.getMap("probe").set("k", "v");
    probe.commit();
    // Export is the call that fails first on a pressed heap (2026-08-04: every
    // byte-EXPORTING call threw while imports kept working), so the probe has
    // to cross the boundary in both directions to mean anything.
    const bytes = probe.export({ mode: "snapshot" });
    echo = new LoroDoc();
    echo.import(bytes);
    return true;
  } catch {
    return false;
  } finally {
    // freeWasm, not a bare try/catch: a probe that cannot be freed is a LEAK
    // worth counting, not merely an error worth ignoring — and this function
    // runs on exactly the failure paths where that is most likely.
    freeWasm(probe, "heap-probe");
    freeWasm(echo, "heap-probe-echo");
  }
};

interface SocketState {
  userId: string;
  /** The caller's verified WorkOS org claim, when their session carries one —
   * what admits a teammate to a chat the owner shared into the org (gh#66).
   * Sockets attached before this shipped deserialize without it and fall back
   * to the owner-only rule. */
  orgId?: string;
  /** Joined sub-rooms by crdt magic ("%LOR", "%EPH"). */
  rooms: string[];
  /** True for sockets on a workspace-doc room — org membership was enforced
   * by the Worker, so the per-chat ownership discipline does not apply. */
  workspace?: boolean;
  /** Which device this socket belongs to (`?deviceId=`), so presence can be
   * DERIVED from the socket set instead of beaten into the room (gh#145).
   * Absent on sockets from engines older than that, and on browser clients —
   * those simply contribute no presence. */
  deviceId?: string;
  /** Accept time — the liveness floor until the socket's first auto-pong,
   * mirroring `DeviceRoom`'s `SocketState.joinedAt`. */
  joinedAt?: number;
  /** This socket's ledger id (gh#527). The attachment is the ONLY place it can
   * live: it survives hibernation and instance eviction with the socket, which
   * is what lets the next wake tell a socket that closed from one that
   * vanished when the instance was killed under it (see socket-log.ts).
   * Absent on sockets attached by a deploy older than gh#527 — those simply
   * contribute nothing to the census. */
  sid?: string;
}

/** Ephemeral key prefix for device presence (`presence/{deviceId}` → ms).
 * MUST match `comet_doc::presence_key` — the wire shape is deliberately
 * unchanged across gh#145 so an engine that predates it still reads correct
 * presence off a room that now derives it. */
const PRESENCE_PREFIX = "presence/";

/** How long a socket may go without proving liveness before it stops counting
 * as present. Identical in value and reasoning to `DeviceRoom`'s
 * `HOST_LIVENESS_MS`: clients text-ping every 15s
 * (crates/sync/src/room.rs PING_INTERVAL), the runtime auto-answers and stamps
 * a timestamp without waking us, and the window is sized for 2.5 intervals of
 * the slower 30s cadence still possible in the fleet. A socket whose uplink
 * died silently (laptop lid, NAT reaping an idle flow) is never closed by the
 * runtime, so this staleness check is the ONLY thing that stops a corpse from
 * reading online forever. */
const PRESENCE_LIVENESS_MS = 75_000;

/** Derive `{deviceId: lastSeenAt}` from a room's sockets. Pure, so the rule is
 * testable without a DO (mirrors `pickLiveHost`).
 *
 * A device may hold several sockets at once — a redial that overlaps its
 * predecessor, or genuinely two processes — so the FRESHEST wins; a corpse
 * listed alongside a live socket must not drag the device offline. */
export const livePresence = (
  sockets: ReadonlyArray<{ deviceId?: string; lastSeenAt: number }>,
  now: number
): Record<string, number> => {
  const live: Record<string, number> = {};
  for (const socket of sockets) {
    if (!socket.deviceId) continue;
    if (now - socket.lastSeenAt > PRESENCE_LIVENESS_MS) continue;
    const best = live[socket.deviceId];
    if (best === undefined || socket.lastSeenAt > best) live[socket.deviceId] = socket.lastSeenAt;
  }
  return live;
};

/** True for rooms whose doc EVERY device writes concurrently, which is what
 * makes a live-frontier force-trim unsafe: a shallow start at the live
 * frontier orphans any peer whose next ops depend on the history just
 * discarded (see the force-trim guard in `maybeTrimHistory`). Pure so the rule
 * is testable without a DO, like [`livePresence`] and [`chatRoomAccess`].
 *
 * Two shapes qualify here, and the pattern must survive a generation bump —
 * upstream matched the literal `ws3/`, and when the room name moved to `ws4`
 * the protection silently evaporated and a live-frontier trim stranded
 * in-flight peers (upstream 4aacc6d). So: any `ws{n}/` per-user workspace doc,
 * and any `orgdev{n}/` org device registry. The registry is OURS (gh#66 —
 * upstream has no such room), and it has exactly the hazardous shape: every
 * device in the org writes its own row into one shared doc, continuously.
 *
 * Chat rooms are single-owner and named as bare ids, so they never match and
 * keep the immediate live-frontier trim the whale-import incident needed. */
export const isConcurrentWriteRoom = (chatId: string | undefined): boolean =>
  /^(ws|orgdev)\d+\//.test(chatId ?? "");

interface BackfillChunk {
  bytes: Uint8Array;
  more: boolean;
}

/** Export a causally closed prefix of the operations `from` lacks.
 *
 * Slicing Loro's peer spans by length is not sufficient: device B's next
 * change can depend on a change from device A that appears later in the span
 * list. Importing that slice advances nothing and every continuation repeats
 * it forever. Instead, inspect only each peer's first missing change, select
 * it when all dependencies are already in the client's advancing VV, and
 * repeat until the fixed operation budget is full. A change may be sliced
 * inside its contiguous op range; the next request resumes at that counter.
 *
 * Work is independent of total history length: at most
 * `BACKFILL_OP_BUDGET` operations enter the wasm export. The peer/dependency
 * factor is the number of devices represented in the workspace VV, normally
 * single digits and independent of its accumulated session rows. */
export const exportBackfillChunk = (
  doc: LoroDoc,
  from: VersionVector,
  maxOps = BACKFILL_OP_BUDGET
): BackfillChunk => {
  if (!Number.isSafeInteger(maxOps) || maxOps <= 0) throw new Error("invalid backfill budget");
  const target = doc.version();
  try {
    const have = new Map(from.toJSON());
    const end = target.toJSON();
    const peers = [...end.keys()].sort((a, b) => {
      const left = BigInt(a);
      const right = BigInt(b);
      return left < right ? -1 : left > right ? 1 : 0;
    });
    const spans: { id: { peer: PeerID; counter: number }; len: number }[] = [];
    let selected = 0;

    while (selected < maxOps) {
      let advanced = false;
      for (const peer of peers) {
        const counter = have.get(peer) ?? 0;
        const peerEnd = end.get(peer) ?? 0;
        if (counter >= peerEnd) continue;

        const change = doc.getChangeAt({ peer, counter });
        if (!change.deps.every((dep) => (have.get(dep.peer) ?? 0) > dep.counter)) continue;

        const changeEnd = Math.min(change.counter + change.length, peerEnd);
        const len = Math.min(changeEnd - counter, maxOps - selected);
        if (len <= 0) continue;
        spans.push({ id: { peer, counter }, len });
        have.set(peer, counter + len);
        selected += len;
        advanced = true;
        if (selected >= maxOps) break;
      }
      if (!advanced) break;
    }

    const more = peers.some((peer) => (have.get(peer) ?? 0) < (end.get(peer) ?? 0));
    if (more && spans.length === 0) {
      // A decoded VV that is a real subset of this oplog always has at least
      // one causally-ready next change. Do not fall back to the unbounded
      // export this function exists to remove if that invariant is violated.
      throw new Error("client version has no causally ready backfill prefix");
    }
    return {
      bytes:
        spans.length === 0
          ? new Uint8Array()
          : doc.export({ mode: "updates-in-range", spans }),
      more
    };
  } finally {
    // `target.toJSON()` above builds a JS Map from inside a borrow of this
    // vector, so a throw out of that callback (an OOM on a pressed isolate)
    // leaves it stuck borrowed. A bare `free()` here would then throw the
    // borrow conflict IN PLACE OF whatever actually went wrong — the 2026-08-24
    // `ws4` join, exactly (gh#607).
    freeWasm(target, "backfill-target-vv");
  }
};

/** What a caller may do with a chat room (gh#66).
 * - `claim`  — unclaimed; the first joiner becomes its owner;
 * - `owner`  — the claiming user, from any of their devices;
 * - `member` — a different user of the same org, on a chat the owner SHARED
 *              into that org (a board dispatch — see `POST /share`);
 * - `deny`   — everyone else. */
export type ChatRoomAccess = "claim" | "owner" | "member" | "deny";

/** The chat room's authorization rule, pure so it is testable without a DO.
 *
 * Chat rooms were owner-only forever, which is right for the private chats a
 * per-user workspace doc indexes — and wrong for the one case the product has
 * to support: a task the box dispatched on behalf of the team, whose transcript
 * every teammate must be able to open and steer. Sharing is therefore explicit
 * and per-room (never "everyone in the org sees every chat"): the owner marks
 * the room shared, and only then does org membership admit anyone else. */
export const chatRoomAccess = (
  room: { owner?: string; org?: string; shared?: boolean },
  caller: { userId: string; orgId?: string }
): ChatRoomAccess => {
  if (!room.owner) return "claim";
  if (room.owner === caller.userId) return "owner";
  if (room.shared && room.org && caller.orgId && room.org === caller.orgId) return "member";
  return "deny";
};

interface FragmentBatch {
  /** One slot per `fragmentCount`; `undefined` until that index arrives. */
  parts: (Uint8Array | undefined)[];
  /** Distinct indices seen — a repeat does not advance it (see `handleFragment`). */
  received: number;
  totalSize: number;
  header: DocUpdateFragmentHeader;
}

interface FrontierCheckpoint {
  at: number;
  frontiers: { peer: string; counter: number }[];
}

export class SessionRoom implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly env: Env;
  private readonly blobs: BlobStore;
  private readonly meta: MetaStore;
  /** Socket lifecycle census (gh#527) — the record of WHY sockets died, kept
   * durably because the deaths that matter take the invocation with them. */
  private readonly sockets: SocketLedger;
  private readonly dailyAlarm: AlarmArmer;
  /** Lazily materialized doc — the log is authoritative; this is a cache. */
  private doc: LoroDoc | undefined;
  private eph: EphemeralStore | undefined;
  private pending: Uint8Array[] = [];
  private pendingBytes = 0;
  private flushTimer: ReturnType<typeof setTimeout> | undefined;
  /** In-memory fragment reassembly. Lost on hibernation → the sender gets a
   * FragmentTimeout ack for the unknown batch and resends — self-healing. */
  private readonly fragments = new Map<WebSocket, Map<string, FragmentBatch>>();
  /** Per-device import penalty box. A peer whose %LOR pushes repeatedly fail
   * to import (a stale peer behind a shallow trim, a device on a diverged
   * timeline) redials and re-pushes its ENTIRE unacceptable diff forever —
   * 2026-08-04 ~23:44Z: home-laptop's ~1MB doomed re-uploads, reassembled and
   * import-attempted every retry cycle, pressed the shared wasm heap into
   * RangeErrors and the poison tripwire recycled the isolate over and over,
   * dropping every device's sockets. After IMPORT_PENALTY_STRIKES consecutive
   * failures a device's pushes are rejected WITHOUT reassembly or import for
   * IMPORT_PENALTY_MS — zero wasm cost, bounded retry, and the room stays up
   * for everyone else. In-memory: an instance recycle grants a fresh 3 tries. */
  private readonly importPenalty = new Map<string, { strikes: number; until: number }>();
  /** Per-device %LOR push outcomes (in-memory, like `importPenalty`). Workers
   * Logs cannot see hibernatable webSocketMessage handlers, so /stats is the
   * only live per-device attribution surface an operator has mid-incident. */
  private readonly pushOutcomes = new Map<
    string,
    { ok: number; rejected: number; lastOkAt: number; lastRejectAt: number }
  >();
  /** Idle-doc release bookkeeping (see DOC_IDLE_RELEASE_MS / touchDoc). */
  private docIdleTimer: ReturnType<typeof setTimeout> | undefined;
  private lastDocUse = 0;

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    this.env = env;
    this.dailyAlarm = new AlarmArmer(ctx.storage);
    ensureUpdateLog(ctx.storage.sql);
    this.meta = createMetaStore(ctx.storage.sql);
    this.sockets = createSocketLedger(ctx.storage.sql, this.meta);
    this.blobs = createBlobStore(ctx.storage.sql);
    // Protocol-designed hibernation keepalive: ping → pong without waking us.
    // NOTE (2026-07-30 incident): precisely BECAUSE the runtime answers these
    // itself, a pong is NOT evidence this DO can still run — a wedged room
    // kept auto-ponging for hours while never processing a join. Clients judge
    // room liveness from protocol frames plus a join-response deadline
    // (crates/sync/src/room.rs), never from these pongs. Do not "upgrade" this
    // to an app-level handler: waking on every ping would abolish hibernation.
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  // ── meta helpers ──────────────────────────────────────────────────────────

  private getMeta(key: string): string | undefined {
    return this.meta.get(key);
  }

  /** Storing a value that is already stored is not a write — see meta.ts. */
  private setMeta(key: string, value: string): void {
    this.meta.set(key, value);
  }

  // ── HTTP surface (only reachable through the authed Worker) ──────────────

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return new Response("unauthenticated", { status: 401 });
    const orgId = request.headers.get(AUTH_ORG_HEADER) ?? undefined;
    // Workspace rooms: the Worker already checked org membership; every
    // member may read/write, so the owner gates below are bypassed.
    const workspace = request.headers.get(ROOM_KIND_HEADER) === "workspace";

    if (url.pathname === "/ws") {
      const chatId = url.searchParams.get("chatId") ?? "";
      if (chatId && !this.getMeta("chatId")) this.setMeta("chatId", chatId);
      const deviceId = url.searchParams.get("deviceId") ?? "";
      // A dial is a wake, and a wake is the moment to ask what happened to the
      // sockets we thought we had. A room killed mid-life (duration cap,
      // eviction, ctx.abort) ran no close handler for any of them, so this is
      // the only place their deaths are ever recorded (gh#527).
      this.reconcileSockets();
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1]);
      const now = Date.now();
      const sid = this.newSocketId();
      const state: SocketState = {
        userId,
        rooms: [],
        joinedAt: now,
        sid,
        ...(orgId ? { orgId } : {}),
        ...(workspace ? { workspace } : {}),
        ...(deviceId ? { deviceId } : {})
      };
      pair[1].serializeAttachment(state);
      // Written AFTER the accept but before anything that can die: an accept
      // row that outlives its socket is the signal, so a missing row would
      // silently un-count exactly the deaths this exists to catch.
      this.sockets.opened(sid, deviceId || undefined, now);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    const owner = this.getMeta("owner");
    // Chat rooms authorize per [`chatRoomAccess`]; workspace rooms were already
    // authorized by the Worker, so every member acts as an owner would.
    const access: ChatRoomAccess = workspace
      ? "owner"
      : chatRoomAccess(
          { owner, org: this.getMeta("org"), shared: this.getMeta("shared") === "1" },
          { userId, orgId }
        );
    // Unclaimed reads keep answering `not_found` — "nothing here yet" is a
    // different fact from "not yours", and clients branch on it.
    const refuse = (): Response =>
      access === "claim" ? json({ error: "not_found" }, 404) : json({ error: "forbidden" }, 403);
    const mayRead = access === "owner" || access === "member";

    if (url.pathname === "/share") {
      // gh#66: the owner marks this chat visible to their org (the board does
      // it for every task it dispatches). Nothing else in the room changes —
      // the flag only widens who [`chatRoomAccess`] admits.
      if (workspace) return json({ error: "not_applicable" }, 400);
      if (request.method === "POST") {
        if (access === "member" || access === "deny") return json({ error: "forbidden" }, 403);
        // Without an org claim there is nobody to share WITH, and stamping an
        // empty org would open the room to every other org-less caller.
        if (!orgId) return json({ error: "no_org" }, 400);
        if (access === "claim") this.setMeta("owner", userId);
        this.setMeta("org", orgId);
        this.setMeta("shared", "1");
        return json({ ok: true, shared: true, org: orgId });
      }
      if (request.method === "GET") {
        if (!mayRead) return refuse();
        return json({ shared: this.getMeta("shared") === "1", org: this.getMeta("org") ?? null });
      }
    }
    if (url.pathname === "/stats" && request.method === "GET") {
      // Observability: what this room holds and who's on it. Owner-gated like
      // every other read (org-membership-gated for workspace and shared rooms).
      if (!mayRead) return refuse();
      try {
        // Attribute vanished sockets before reporting: /stats is often the
        // FIRST thing anyone asks a room after an incident, and a census that
        // still counted the dead as open would be the same lie the engine's
        // "N of N live" gauge told all evening (gh#527).
        this.reconcileSockets();
        await this.flush();
        const updateRows = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]
          ?.n as number;
        const snapshot = this.blobs.get("snapshot");
        return json({
          chatId: this.getMeta("chatId") ?? null,
          connectedSockets: this.ctx.getWebSockets().length,
          // Derived presence (gh#145) — how many devices this room would report
          // present if asked right now.
          presentDevices: Object.keys(this.socketPresence()).length,
          updateRows,
          updateLogBytes: Number(this.getMeta("updateBytes") ?? "0"),
          snapshotBytes: snapshot?.length ?? 0,
          // Cold-start cost of the LAST materialization — the wedge-risk gauge
          // (2026-07-30: this creeping toward the CPU limit was invisible).
          lastReplayMs: Number(this.getMeta("lastReplayMs") ?? "0"),
          lastReplayRows: Number(this.getMeta("lastReplayRows") ?? "0"),
          // True between a wedge-break log drop and the first re-uploaded state
          // (the nightly backup is paused in that window).
          postReset: this.getMeta("postReset") === "1",
          tailCached: this.getMeta("tailDirty") !== "1" && this.blobs.get("tail") !== undefined,
          diffPublished: this.blobs.get("diff") !== undefined,
          checkpoints: (JSON.parse(this.getMeta("checkpoints") ?? "[]") as unknown[]).length,
          lastTrimAt: this.getMeta("lastTrimAt") ?? null,
          backupDirty: this.getMeta("backupDirty") === "1",
          // The daily chain's health (gh#378). A room that has given up on its
          // own checkpoint/trim/backup is a fact somebody must be able to
          // READ — the alternative is silence, and the give-up window is
          // precisely the window with nobody connected to notice. `gaveUpAt`
          // is when it FIRST gave up; `limit` is the budget it exhausted.
          alarm: {
            consecutiveFailures: Number(this.getMeta("alarmFailures") ?? "0"),
            gaveUp: Boolean(this.getMeta("alarmGaveUpAt")),
            gaveUpAt: Number(this.getMeta("alarmGaveUpAt") || "0") || null,
            limit: ALARM_FAILURE_LIMIT,
            // Alarms answered by the load guard rather than by a failure
            // (gh#554) — these do NOT spend the budget above, because the guard
            // is reseeding the room the alarm would otherwise give up on.
            refusals: Number(this.getMeta("alarmRefusals") ?? "0"),
            refusalBudget: ALARM_REFUSAL_BUDGET
          },
          // Non-zero while a cold replay is in flight or has been dying — the
          // wedge signature ensureDoc's automated reset watches for.
          replayAttempts: Number(this.getMeta("replayAttempts") ?? "0"),
          // Per-device %LOR attribution (in-memory: since this instance woke).
          importPenalty: [...this.importPenalty].map(([device, e]) => ({ device, ...e })),
          pushOutcomes: [...this.pushOutcomes].map(([device, e]) => ({ device, ...e })),
          // WHY SOCKETS DIED (gh#527). Durable, unlike everything above it that
          // is scoped to this instance — which is the point: the deaths worth
          // reading about are the ones that took an instance with them.
          // `vanished` is the killer that logs nothing; `diedYoungLastHour` is
          // the churn rate the engine's live-connection gauge cannot see.
          sockets: this.sockets.census(Date.now()),
          // The load guard and the last time this room refused to materialize
          // (see MAX_DOC_LOAD_BYTES).
          docLoad: {
            guardBytes: MAX_DOC_LOAD_BYTES,
            storedBytes: this.storedDocBytes(),
            refusals: Number(this.getMeta("loadRefusals") ?? "0"),
            limit: LOAD_REFUSAL_LIMIT,
            lastRefusal: this.readJsonMeta("lastLoadRefusal")
          },
          // Stuck wasm borrows (gh#607). `conflicts` is how often this room has
          // ever found its doc wedged and `strikes` how many in a row it is on
          // now — the budget that ends in the same evict-and-reseed the load
          // guard uses, which is why it carries the same limit. `leaked` counts
          // the ones whose memory could not be handed back at all, which is the
          // number that explains an isolate walking into its memory limit with
          // every room reporting health; `isolateLeaked` is that count across
          // all co-located rooms, because the leak is in the linear memory they
          // share.
          borrow: {
            conflicts: Number(this.getMeta("borrowConflicts") ?? "0"),
            strikes: Number(this.getMeta("borrowStrikes") ?? "0"),
            limit: LOAD_REFUSAL_LIMIT,
            leaked: Number(this.getMeta("borrowLeaks") ?? "0"),
            isolateLeaked: wasmBorrowLeaks,
            lastConflict: this.readJsonMeta("lastBorrowConflict")
          },
          // Whether this room can still COMPACT (gh#557). `snapshotBytes: 0`
          // on a megabyte room was the only trace a fold had never landed, and
          // it reads identically to "nothing to fold yet" — so a room whose
          // snapshot export kept failing looked healthy on every surface while
          // it aborted its isolate over it. Say it outright.
          fold: {
            consecutiveFailures: Number(this.getMeta("foldFailures") ?? "0"),
            retryAt: Number(this.getMeta("foldRetryAt") ?? "0") || null,
            lastFailure: this.readJsonMeta("lastFoldFailure")
          },
          // Every wasm-shaped failure, struck or not, and WHICH call made it.
          // `lastAbort` carried the exception and no call site, which is how
          // three tickets read the same RangeError without being able to say
          // where it came from.
          wasm: {
            faults: Number(this.getMeta("wasmFaults") ?? "0"),
            lastFault: this.readJsonMeta("lastWasmFault"),
            abortAfterStrikes: WASM_POISON_ABORT_AFTER
          },
          // The last time THIS room aborted its own instance, and why — the
          // half of a 1006 storm that is our doing rather than the platform's.
          lastAbort: this.readJsonMeta("lastAbort")
        });
      } catch (e) {
        // The one observability surface must never die as a bare 1101/500 —
        // /stats is how an operator sees a room mid-incident (see /tail).
        console.error("stats failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
        this.escalateWasmPoisoning(e, "stats");
        return json({ error: "stats_failed", message: String(e) }, 500);
      }
    }
    if (url.pathname === "/presence" && request.method === "GET") {
      // THE ASK (gh#145). Presence is no longer pushed on a timer, so this is
      // how a caller gets a fresh answer on demand — computed from the socket
      // set at the moment of asking, which is the only reading that is true
      // about a room that has been asleep. Cheap by construction: no doc
      // materialization, no flush, nothing that outlives the request.
      if (!mayRead) return refuse();
      return json({ at: Date.now(), devices: this.socketPresence() });
    }
    if (url.pathname === "/tail" && request.method === "GET") {
      if (!mayRead) return refuse();
      try {
        return json(await this.currentTail());
      } catch (e) {
        // Surface the real error to the operator: a bare 1101 here cost the
        // 2026-08-05 incident an hour of blind guessing (Workers Logs can't
        // be queried without an observability-scoped token).
        console.error("tail materialization failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
        this.escalateWasmPoisoning(e, "tail");
        return json({ error: "tail_failed", message: String(e) }, 500);
      }
    }
    if (url.pathname === "/diff" && request.method === "GET") {
      if (!mayRead) return refuse();
      const diff = getJsonBlob<unknown>(this.blobs, "diff");
      return diff === undefined ? json({ error: "not_found" }, 404) : json(diff);
    }
    if (url.pathname === "/diff" && request.method === "POST") {
      // Publishing is the hosting device's job — a teammate reads the sidecar,
      // never writes it. The host may publish before any room join has claimed
      // the doc, so an unclaimed room claims here.
      if (access === "member" || access === "deny") return json({ error: "forbidden" }, 403);
      if (access === "claim") this.setMeta("owner", userId);
      putJsonBlob(this.blobs, "diff", await request.json());
      return json({ ok: true });
    }
    if (url.pathname === "/snapshot" && request.method === "GET") {
      // Repair/inspection read: the doc's full current snapshot bytes.
      if (!mayRead) return refuse();
      try {
        await this.flush();
        const doc = await this.ensureDoc();
        const bytes = doc.export({ mode: "snapshot" });
        return new Response(bytes as unknown as BodyInit, {
          headers: { "content-type": "application/octet-stream" }
        });
      } catch (e) {
        // The repair-read must never fail as a bare 1101 — see /tail above.
        console.error("snapshot export failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
        this.escalateWasmPoisoning(e, "snapshot-export");
        return json({ error: "snapshot_failed", message: String(e) }, 500);
      }
    }
    if (url.pathname === "/append" && request.method === "POST") {
      // MERGE-safe repair write: import a Loro update (never replaces the
      // doc). Same durability bookkeeping as a WS DocUpdate. Repair is an
      // owner/operator path — a shared chat's members write through the room.
      if (access !== "owner") return refuse();
      const body = new Uint8Array(await request.arrayBuffer());
      const doc = await this.ensureDoc();
      try {
        if (body.length > 0) doc.import(body);
      } catch {
        return json({ error: "invalid_update" }, 400);
      }
      await this.recordLoroUpdates([body]);
      // Converge live peers: relay the update to connected %LOR sockets.
      const roomId = this.getMeta("chatId") ?? "";
      for (const ws of this.ctx.getWebSockets()) {
        const state = ws.deserializeAttachment() as SocketState | null;
        if (!state?.rooms.includes(CrdtType.Loro)) continue;
        this.sendUpdates(ws, CrdtType.Loro, roomId, [body]);
      }
      return json({ ok: true });
    }
    if (url.pathname === "/reset-log" && request.method === "POST") {
      // WEDGE BREAK: drop the persisted update log + snapshot so the NEXT cold
      // `ensureDoc` starts from empty instead of replaying a log so large it
      // exceeds the DO CPU limit and resets before any client can join (which
      // also blocks the compaction that would have shrunk it — a permanent
      // wedge). Deliberately does NOT call `ensureDoc`, so it stays cheap
      // enough to land on an already-wedged DO. State is not lost: every engine
      // holds the full workspace doc locally and re-uploads it on the next join
      // (CRDT merge), exactly like the `ws3` fresh-namespace recovery. Presence
      // is ephemeral and simply re-published. Owner/chatId meta are preserved.
      //
      // EVERY STEP BUT THE DROP IS CLEANUP, and is written to fail without
      // taking the drop with it (gh#553). The room this lands on is by
      // definition sick: on 2026-08-22 the workspace room had just aborted on
      // `wasm heap poisoned after 3 strikes`, and the unguarded `this.doc.free()`
      // below threw out of the handler as a bare 1101 — twice — so the one tool
      // that exists for a wedged room appeared not to work on the exact room it
      // exists for. An operator who did not blindly retry would have escalated
      // to a generation bump instead.
      if (access !== "owner") return refuse();
      const room = this.getMeta("chatId") ?? "?";
      const problems: string[] = [];
      /** Cleanup step: log it, remember it, never let it reach the caller. */
      const cleanup = (what: string, run: () => void): void => {
        try {
          run();
        } catch (e) {
          problems.push(`${what}: ${String(e)}`);
          console.error(
            "reset-log cleanup step failed (continuing)",
            `room=${room}`,
            what,
            String(e)
          );
        }
      };
      let before: number | undefined;
      cleanup("count updates", () => {
        before = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n as
          | number
          | undefined;
      });
      try {
        this.dropLog();
      } catch (e) {
        // The drop IS the request. If it cannot happen, say which step died
        // rather than dying as an anonymous 1101 (same reasoning as /snapshot).
        console.error("reset-log could not drop the log", `room=${room}`, String(e));
        return json({ error: "reset_failed", message: String(e) }, 500);
      }
      // Commit the drop BEFORE touching wasm: a free on a poisoned heap can take
      // the whole invocation down rather than merely throw, and a killed
      // invocation rolls back this event's uncommitted storage writes — the
      // wedge break would be undone by its own cleanup (same hazard, same fix,
      // as the trim's sync in `maybeTrim`).
      await this.ctx.storage.sync();
      cleanup("free doc", () => {
        // Release the wasm memory rather than wait on GC finalizers — but only
        // if the wrapper is still live. A room that just aborted on a poisoned
        // heap is precisely a room whose cached `this.doc` is a dangling
        // wrapper, and calling into a freed one throws (see `isLive`). A room
        // that got here from a stuck borrow (gh#607) is one whose doc cannot be
        // released at all; `abandonDoc` clears the field either way, which is
        // what "force a fresh materialization next join" actually requires.
        //
        // It swallows the failure, and this step must not: the ask of gh#553 is
        // failure-TOLERANT, not failure-blind — the drop happens regardless and
        // the operator is still told which cleanup limped. Re-raise so
        // `cleanup` records it in `problems`.
        const outcome = this.abandonDoc("reset-log");
        if (outcome === "leaked" || outcome === "failed") {
          throw new Error(`cached doc could not be released (${outcome})`);
        }
      });
      // Boot any currently-attached %LOR/%EPH sockets so their hung/half-cold
      // sessions bail and reconnect into the now-empty doc.
      cleanup("boot sockets", () => {
        for (const sock of this.ctx.getWebSockets()) {
          try {
            sock.close(4410, "room reset");
          } catch {
            /* already gone */
          }
        }
      });
      return json({
        ok: true,
        clearedUpdateRows: before ?? 0,
        // Present only when a cleanup step failed: the log is dropped either
        // way, and the operator should still see what limped.
        ...(problems.length > 0 ? { problems } : {})
      });
    }
    return new Response("not found", { status: 404 });
  }

  // ── WebSocket protocol ────────────────────────────────────────────────────

  async webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): Promise<void> {
    if (typeof message === "string") return; // ping/pong handled by auto-response
    let decoded: ProtocolMessage;
    try {
      decoded = decode(new Uint8Array(message));
    } catch {
      ws.close(1002, "Protocol error");
      return;
    }
    const state = ws.deserializeAttachment() as SocketState;
    try {
      switch (decoded.type) {
        case MessageType.JoinRequest:
          await this.handleJoin(ws, state, decoded);
          break;
        case MessageType.DocUpdate:
          await this.handleDocUpdate(ws, state, decoded);
          break;
        case MessageType.DocUpdateFragmentHeader:
          this.handleFragmentHeader(ws, state, decoded);
          break;
        case MessageType.DocUpdateFragment:
          await this.handleFragment(ws, state, decoded);
          break;
        case MessageType.Leave:
          state.rooms = state.rooms.filter((r) => r !== decoded.crdt);
          ws.serializeAttachment(state);
          break;
        case MessageType.Ack:
        case MessageType.RoomError:
          break;
        default:
          ws.close(1002, "Unsupported message");
      }
    } catch (raw) {
      // Classify BEFORE anything else looks at it (gh#607). A stuck wasm borrow
      // is neither a heap fault nor a bad-bytes fault, and read as either it
      // gets the wrong cure — so it becomes a `DocBorrowConflict` here, which is
      // a `DocLoadRefused`, and falls into the refusal branch below with the
      // poisoned wrapper already dropped and the reseed already counting.
      const e = this.reclassifyBorrow(raw, `ws-${decoded.type}`);
      if (e instanceof DocLoadRefused) {
        // A refusal is an ANSWER (gh#527), not a failure: the room has already
        // logged why it will not load and counted it toward the reseed. Say so
        // on the wire so the client parks on its long backoff instead of
        // hot-dialing a room that cannot serve it, and never strike the wasm
        // tripwire — the heap is fine, the stored doc is not.
        console.warn(
          e instanceof DocBorrowConflict
            ? "refusing a join: this room's doc is wedged in wasm"
            : "refusing a join: this room will not load",
          `room=${this.getMeta("chatId") ?? "?"}`,
          `device=${state?.deviceId ?? "unattributed"}`,
          e.message
        );
        if (decoded.type === MessageType.JoinRequest) {
          this.send(ws, {
            type: MessageType.JoinError,
            crdt: decoded.crdt,
            roomId: decoded.roomId,
            code: JoinErrorCode.AppError,
            message: e.message
          });
        }
        return;
      }
      // A handler that dies pre-answer used to fail in SILENCE: the client
      // waits out its 15s join deadline, redials, and dies the same way —
      // the 2026-08-04 fleet-wide join wedge. Log attributed, answer an
      // outstanding join so clients fail fast and VISIBLY (JoinError →
      // long-backoff rejoin instead of a hot 15s dial loop), then escalate
      // suspected wasm-heap poisoning to an isolate recycle.
      console.error(
        "ws message handler failed",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${state?.deviceId ?? "unattributed"}`,
        `type=${decoded.type}`,
        String(e)
      );
      if (decoded.type === MessageType.JoinRequest) {
        this.send(ws, {
          type: MessageType.JoinError,
          crdt: decoded.crdt,
          roomId: decoded.roomId,
          code: JoinErrorCode.AppError,
          message: "internal error"
        });
      }
      this.escalateWasmPoisoning(e, "ws-message");
    }
  }

  async webSocketClose(
    ws: WebSocket,
    code?: number,
    reason?: string,
    wasClean?: boolean
  ): Promise<void> {
    // THE ASK of gh#527: the edge must say why a socket died. The runtime hands
    // us the code and reason and this handler used to drop both on the floor,
    // so even the deaths we DO observe were unattributed — a tail full of
    // `canceled` invocations and nothing to say whether the peer went away, we
    // closed it (4410 room reset, 1011 broadcast failure), or the transport
    // broke.
    this.noteSocketDeath(ws, "close", code, reason, wasClean);
    this.fragments.delete(ws);
    // A socket leaving IS the presence event — and it already woke us, so
    // announcing it costs nothing beyond the wake we were paying anyway.
    // `ws` is excluded because the runtime still lists a socket while its
    // close is being handled (same trap `pickLiveHost` documents).
    this.publishPresence(ws);
    try {
      await this.flush();
    } catch (e) {
      // Flush can fold the log (a wasm snapshot export); an uncaught throw
      // here is invisible in a close handler. Same discipline as above.
      console.error("flush on socket close failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
      this.escalateWasmPoisoning(e, "flush-on-close");
    }
  }

  async webSocketError(ws: WebSocket, error?: unknown): Promise<void> {
    this.noteSocketDeath(ws, "error", undefined, error === undefined ? undefined : String(error));
    this.fragments.delete(ws);
    this.publishPresence(ws);
    try {
      await this.flush();
    } catch (e) {
      console.error("flush on socket error failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
      this.escalateWasmPoisoning(e, "flush-on-error");
    }
  }

  /** Record an OBSERVED socket death and say so in the journal (gh#527).
   *
   * The log line carries what the 2026-08-19 diagnosis had to guess at: the
   * close code and reason, how long the socket lived, whose device it was, and
   * how big this room's stored doc is — the three-way discriminator between a
   * peer that went away, a room we closed on purpose, and a room whose own doc
   * is the thing killing it. Never throws: a close handler that dies is
   * invisible, and this is the surface that exists to end invisibility. */
  private noteSocketDeath(
    ws: WebSocket,
    kind: "close" | "error",
    code?: number,
    reason?: string,
    wasClean?: boolean
  ): void {
    try {
      const state = ws.deserializeAttachment() as SocketState | null;
      const death = this.sockets.died(state?.sid, kind, Date.now(), code, reason);
      const young = death !== undefined && death.ageMs >= 0 && death.ageMs < YOUNG_SOCKET_MS;
      const line = [
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${state?.deviceId ?? "unattributed"}`,
        `kind=${kind}`,
        `code=${code ?? "-"}`,
        `reason=${reason ?? "-"}`,
        `wasClean=${wasClean ?? "-"}`,
        `ageMs=${death?.ageMs ?? "unknown"}`,
        `docBytes=${this.storedDocBytes()}`
      ];
      // A young death is the incident shape (a room that answers the join and
      // dies), so it is a warning; an ordinary hang-up is a log line.
      if (young) console.warn("session socket died young", ...line);
      else console.log("session socket closed", ...line);
    } catch (e) {
      console.error("socket death bookkeeping failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
    }
  }

  /** Attribute the sockets that died with NO handler — the killer that does
   * not log (see socket-log.ts). Runs on wakes, never throws. */
  private reconcileSockets(): void {
    try {
      const liveIds: string[] = [];
      for (const ws of this.ctx.getWebSockets()) {
        const state = ws.deserializeAttachment() as SocketState | null;
        if (state?.sid) liveIds.push(state.sid);
      }
      const deaths = this.sockets.reconcile(liveIds, Date.now());
      if (deaths.length === 0) return;
      // The line the tail did not have. "Vanished" is a claim about the ROOM,
      // not the client: nothing in this instance closed these sockets, so
      // whatever ended them ended an invocation too.
      console.error(
        "sockets vanished with no close event (the instance was killed under them)",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `count=${deaths.length}`,
        `devices=${[...new Set(deaths.map((d) => d.deviceId ?? "unattributed"))].join(",")}`,
        `ages=${deaths.map((d: SocketDeath) => d.ageMs).join(",")}`,
        `docBytes=${this.storedDocBytes()}`,
        `lastAbort=${this.getMeta("lastAbort") || "none"}`
      );
    } catch (e) {
      console.error("socket reconcile failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
    }
  }

  /** Persisted doc size (snapshot + update log) without materializing either —
   * the cheap number every death line carries, because "how big is this room"
   * is the first question asked of a room that keeps dying. */
  private storedDocBytes(): number {
    try {
      return this.blobs.size("snapshot") + Number(this.getMeta("updateBytes") ?? "0");
    } catch {
      return -1;
    }
  }

  /** A JSON meta value, or null. Never throws: these are diagnostics, and
   * /stats is the surface that must answer when everything else has stopped
   * (see the catch below it). */
  private readJsonMeta(key: string): unknown {
    const raw = this.getMeta(key);
    if (!raw) return null;
    try {
      return JSON.parse(raw) as unknown;
    } catch {
      return { unparseable: raw.slice(0, 200) };
    }
  }

  private newSocketId(): string {
    const bytes = new Uint8Array(8);
    crypto.getRandomValues(bytes);
    return bytesToHex(bytes);
  }

  /** RangeError("Invalid array buffer length") / wasm RuntimeError are the
   * signature of an exhausted loro-wasm heap (see `wasmPoisonStrikes`).
   * Strike out and `ctx.abort()` so clients redial into a fresh isolate
   * within seconds instead of hot-looping against a deaf room until
   * Cloudflare's memory-limit reset finally fires.
   *
   * `site` names the wasm call that threw, and it is not decoration (gh#557):
   * `lastAbort` recorded the exception and nothing else, so three tickets read
   * `RangeError: Invalid array buffer length` without being able to say which
   * of the room's half-dozen export paths produced it. Every caller passes one.
   *
   * A wasm-shaped throw off a heap that [`wasmHeapUsable`] proves still works
   * is NOT a strike. Aborting on it recycles an isolate that was never sick,
   * sheds every socket in it, and invites the reseed that reproduces the
   * fault — the loop this ticket is about. It is still recorded. */
  private escalateWasmPoisoning(e: unknown, site: string): void {
    // A stuck borrow is the THIRD class (gh#607) and it must not reach the
    // strike counter: aborting the isolate over one wedged object sheds every
    // co-located room's sockets and invites the reseed that reproduces the
    // fault. It is not merely declined, though — the cure still has to run, and
    // this is the only hook the /tail, /stats, /snapshot, flush and alarm paths
    // share. `reclassifyBorrow` drops the poisoned wrapper and counts the
    // refusal; the fault it returns is thrown away because these callers have
    // already answered theirs.
    if (isWasmBorrowConflict(e)) {
      this.reclassifyBorrow(e, site);
      return;
    }
    if (!(e instanceof RangeError || e instanceof WebAssembly.RuntimeError || isWasmUseAfterFree(e))) {
      return;
    }
    // Use-after-free is exempt from the probe on purpose: a dangling wrapper
    // says nothing about the heap (a probe would always pass) and nothing in
    // the instance recovers the flows still holding it — see `isLive`.
    if (!isWasmUseAfterFree(e) && wasmHeapUsable()) {
      this.noteWasmFault(site, e, false);
      console.error(
        "wasm-shaped failure on a WORKING heap; not a poison strike",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `site=${site}`,
        `storedBytes=${this.storedDocBytes()}`,
        String(e)
      );
      return;
    }
    this.noteWasmFault(site, e, true);
    wasmPoisonStrikes++;
    if (wasmPoisonStrikes < WASM_POISON_ABORT_AFTER) return;
    console.error(`wasm heap poisoned (${wasmPoisonStrikes} strikes); aborting isolate for a fresh heap`);
    // Best effort: if abort recycles only the DO instance (not the whole
    // isolate), at least this room's doc goes back to the wasm allocator.
    this.abandonDoc("poison-abort");
    // gh#527: leave the reason DURABLY behind before the instance dies. An
    // abort severs every socket — the clients read 1006 — and takes the
    // invocation with it, so the console line above is the only account and
    // may never ship. This marker is what lets the next wake's socket
    // reconcile say "we did this, and here is why" instead of leaving
    // `vanished` sockets to be blamed on the duration cap. `sync()` is what
    // makes it survive: an abort discards uncommitted writes.
    this.recordAbort(
      `wasm heap poisoned after ${wasmPoisonStrikes} strikes at ${site}: ${String(e)}`
    );
  }

  /** Leave every wasm-shaped failure behind, struck or not (gh#557).
   *
   * The non-striking ones are the point: a room that keeps failing one export
   * on a healthy heap now says so on /stats instead of being silently absorbed,
   * which is the reading `snapshotBytes: 0` was quietly making all along.
   * Never throws — this is diagnostics on an already-bad path. */
  private noteWasmFault(site: string, e: unknown, struck: boolean): void {
    try {
      const faults = Number(this.getMeta("wasmFaults") ?? "0") + 1;
      this.setMeta("wasmFaults", String(faults));
      this.setMeta(
        "lastWasmFault",
        JSON.stringify({ at: Date.now(), site, struck, error: String(e) }).slice(0, 512)
      );
    } catch {
      /* storage refused the marker; the classification above still stands */
    }
  }

  /** Persist why this instance is about to die, then die (gh#527).
   *
   * Ordered on purpose: mark → sync → abort, with a bounded fallback so a
   * storage sync that never settles cannot leave a poisoned instance running.
   * Aborting twice is harmless; not aborting at all is the wedge this
   * escalation exists to break. */
  private recordAbort(reason: string): void {
    const abortNow = (): void => {
      try {
        this.ctx.abort(reason);
      } catch {
        /* already aborted / already gone */
      }
    };
    try {
      this.setMeta("lastAbort", JSON.stringify({ at: Date.now(), reason }).slice(0, 512));
    } catch {
      /* storage refused the marker; the abort still has to happen */
    }
    setTimeout(abortNow, ABORT_SYNC_GRACE_MS);
    this.ctx.storage.sync().then(abortNow, abortNow);
  }

  private async handleJoin(ws: WebSocket, state: SocketState, message: JoinRequest): Promise<void> {
    if (!state.workspace) {
      // Chat rooms: claim-on-first-join ownership, then the owner — plus, once
      // the owner has shared the chat into their org (gh#66), that org's
      // members, who join with the same write permission so a teammate can
      // steer a board-dispatched run.
      const access = chatRoomAccess(
        {
          owner: this.getMeta("owner"),
          org: this.getMeta("org"),
          shared: this.getMeta("shared") === "1"
        },
        { userId: state.userId, orgId: state.orgId }
      );
      if (access === "claim") {
        this.setMeta("owner", state.userId);
        if (state.orgId) this.setMeta("org", state.orgId);
      } else if (access === "deny") {
        this.send(ws, {
          type: MessageType.JoinError,
          crdt: message.crdt,
          roomId: message.roomId,
          code: JoinErrorCode.AuthFailed,
          message: "not the room owner"
        });
        return;
      }
    }
    if (!this.getMeta("chatId") && message.roomId) this.setMeta("chatId", message.roomId);
    // A client is back on a room that had given up on its own maintenance
    // (gh#378) — that is the signal to try again, whatever broke last time.
    await this.reviveAlarm();

    if (message.crdt === CrdtType.Loro) {
      const doc = await this.ensureDoc();
      if (!state.rooms.includes(message.crdt)) state.rooms.push(message.crdt);
      ws.serializeAttachment(state);
      // Wasm-bindgen objects (VersionVector here and below) free their wasm
      // memory only via GC finalizers — and V8 has no reason to collect when
      // the pressure is in WASM linear memory, not the JS heap. Under join
      // storms these leaked per-answer until the isolate hit its memory
      // limit (2026-08-04 exhaustion). Free explicitly.
      const vv = doc.version();
      try {
        this.send(ws, {
          type: MessageType.JoinResponseOk,
          crdt: message.crdt,
          roomId: message.roomId,
          permission: "write",
          version: vv.encode()
        });
      } finally {
        freeWasm(vv, "join-version-vv");
      }
      let backfill: Uint8Array<ArrayBufferLike> = new Uint8Array();
      let continueBackfill = false;
      if (message.version.length > 0) {
        let from: VersionVector | undefined;
        try {
          from = VersionVector.decode(message.version);
        } catch {
          // Unknown/garbled client version — fall back to the recovery
          // snapshot expected by existing clients.
          backfill = doc.export({ mode: "snapshot" });
        }
        if (from) {
          try {
            // A shallow doc cannot diff across its trimmed root, and loro does
            // NOT throw for a `from` behind the shallow start — it silently
            // exports only the post-root ops (~90 bytes of nothing for a fresh
            // reader), which import client-side as forever-pending deps: zero
            // messages, zero errors anywhere (found 2026-08-05 — every fresh
            // device joining a force-trimmed whale room got an empty
            // transcript). An encoded EMPTY version vector is 1 byte, so
            // `version.length > 0` does not mean "has state" — detect
            // behind-the-root explicitly and serve the §3.1 stale-peer full
            // snapshot instead of trusting the export.
            let stale = false;
            if (doc.isShallow()) {
              const since = doc.shallowSinceVV();
              try {
                const cmp = from.compare(since);
                stale = cmp === undefined || cmp < 0;
              } finally {
                freeWasm(since, "join-shallow-since-vv");
              }
            }
            if (stale) {
              // Snapshot reseeding is intentionally atomic: the Rust client's
              // recovery path validates one snapshot against the advertised VV
              // before swapping documents. A shallow snapshot has already
              // discarded old history, so it does not have the unbounded oplog
              // shape this continuation path is for.
              backfill = doc.export({ mode: "snapshot" });
            } else {
              const chunk = exportBackfillChunk(doc, from);
              backfill = chunk.bytes;
              continueBackfill = chunk.more;
            }
          } finally {
            freeWasm(from, "join-client-vv");
          }
        }
      } else {
        backfill = doc.export({ mode: "snapshot" });
      }
      if (backfill.length > 0) {
        this.sendUpdates(ws, message.crdt, message.roomId, [backfill]);
      }
      if (continueBackfill) {
        // Ordered after the update frames. Existing phone clients handle
        // RejoinSuggested by immediately sending another JoinRequest with
        // their now-advanced VV on THIS socket, so each message event gets a
        // fresh CPU budget without reconnect/backoff or server-side timers.
        this.send(ws, {
          type: MessageType.RoomError,
          crdt: message.crdt,
          roomId: message.roomId,
          code: RoomErrorCode.RejoinSuggested,
          message: "continue backfill"
        });
      }
      // A doc-only joiner still carries a device id, so its arrival changes the
      // answer for everyone already watching presence.
      this.publishPresence();
      // This join chunk exported cleanly — the wasm heap is healthy.
      // Without this reset, occasional transient RangeErrors accumulated
      // over an isolate's lifetime and the tripwire aborted HEALTHY
      // isolates, each abort causing a reconnect herd that produced more
      // transient errors (observed 2026-08-04: ~1 abort/min with all rooms
      // already trimmed small). Poisoning is CONSECUTIVE failures.
      wasmPoisonStrikes = 0;
      // And so is a wedged doc (gh#607): this join went in and out of the
      // cached `LoroDoc` and came back with bytes, which is the only evidence
      // that exists that the object is usable again. Same discipline, one room
      // narrower — the borrow is per-object, so the counter is per-room.
      if (this.getMeta("borrowStrikes")) this.setMeta("borrowStrikes", "0");
      return;
    }

    if (message.crdt === CrdtType.LoroEphemeralStore) {
      this.ensureEph();
      if (!state.rooms.includes(message.crdt)) state.rooms.push(message.crdt);
      ws.serializeAttachment(state);
      this.send(ws, {
        type: MessageType.JoinResponseOk,
        crdt: message.crdt,
        roomId: message.roomId,
        permission: "write",
        version: new Uint8Array()
      });
      // Recompute BEFORE answering rather than replaying whatever the store
      // happens to still hold: the ephemeral store's 30s TTL has forgotten
      // every entry if the room has been quiet, which is now the normal state.
      // This both backfills the joiner and tells everyone else it arrived.
      this.publishPresence();
      return;
    }

    this.send(ws, {
      type: MessageType.JoinError,
      crdt: message.crdt,
      roomId: message.roomId,
      code: JoinErrorCode.Unknown,
      message: "unsupported crdt"
    });
  }

  private async handleDocUpdate(ws: WebSocket, state: SocketState, message: DocUpdate): Promise<void> {
    if (message.updates.some((u) => u.length > MAX_MESSAGE_SIZE)) {
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge);
      return;
    }
    if (!state.rooms.includes(message.crdt)) {
      this.ack(ws, message, UpdateStatusCode.PermissionDenied);
      return;
    }
    await this.applyUpdates(ws, state, message.crdt, message.roomId, message.batchId, message.updates);
  }

  /** Shared apply path for whole and reassembled updates. */
  /** True (and refreshed) when the device is in the import penalty box —
   * callers must reject its %LOR payloads without reassembly or import. */
  private importPenalized(deviceId: string | undefined, now: number): boolean {
    if (!deviceId) return false;
    const entry = this.importPenalty.get(deviceId);
    return !!entry && entry.strikes >= IMPORT_PENALTY_STRIKES && entry.until > now;
  }

  /** Per-device push-outcome bookkeeping for /stats (see `pushOutcomes`). */
  private notePush(deviceId: string | undefined, ok: boolean, now: number): void {
    if (!deviceId) return;
    const entry =
      this.pushOutcomes.get(deviceId) ?? { ok: 0, rejected: 0, lastOkAt: 0, lastRejectAt: 0 };
    if (ok) {
      entry.ok++;
      entry.lastOkAt = now;
    } else {
      entry.rejected++;
      entry.lastRejectAt = now;
    }
    this.pushOutcomes.set(deviceId, entry);
  }

  private async applyUpdates(
    ws: WebSocket,
    state: SocketState,
    crdt: CrdtType,
    roomId: string,
    batchId: `0x${string}`,
    updates: Uint8Array[]
  ): Promise<void> {
    if (crdt === CrdtType.Loro) {
      const now = Date.now();
      const totalBytes = updates.reduce((n, u) => n + u.length, 0);
      if (this.importPenalized(state.deviceId, now) && totalBytes > PENALTY_PROBE_MAX_BYTES) {
        this.notePush(state.deviceId, false, now);
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      const doc = await this.ensureDoc();
      const imported: Uint8Array[] = [];
      let failed = false;
      try {
        for (const update of updates)
          if (update.length > 0) {
            doc.import(update);
            imported.push(update);
          }
      } catch (e) {
        // Wasm-heap poison is terminal for this instance — no salvage
        // retries against a dying heap; the outer handler's tripwire counts
        // it and recycles the isolate.
        //
        // But only if the heap is the patient (gh#557). These bytes came off
        // the wire, and a wasm-shaped throw over a working heap is a statement
        // about the PUSH, not the isolate — rethrowing it left the sender with
        // no ack at all, so it redialled and re-sent the same payload, which is
        // the reseed half of the loop. Salvage below answers it instead.
        // A stuck borrow (gh#607) is neither, and salvage is actively wrong for
        // it: every retry below re-enters the same wedged object, fails, and
        // ends with the SENDER in the penalty box for a fault that is the
        // room's. Rethrow so the handler's classifier drops the doc and refuses
        // the push honestly; the next materialization is clean.
        if (isWasmBorrowConflict(e)) throw e;
        const wasmShaped =
          e instanceof RangeError || e instanceof WebAssembly.RuntimeError || isWasmUseAfterFree(e);
        // Only the RangeError arm is ambiguous enough to be worth asking about
        // — a RuntimeError or a dangling wrapper says nothing about the bytes.
        const blameTheBytes = e instanceof RangeError && wasmHeapUsable();
        if (wasmShaped && !blameTheBytes) throw e;
        // Includes imports concurrent to a shallow-snapshot start (§3.1 stale
        // peer) — the client resyncs fresh and re-submits at the app layer.
        // Salvage the rest of the batch individually first: one unimportable
        // update (a stale peer's bundled old history) must not void the
        // batch's good writes — session status/title are exactly the small
        // updates that ride along. Re-importing an already-applied update is
        // idempotent, so restarting the loop from the top is safe.
        failed = true;
        imported.length = 0;
        for (const update of updates) {
          if (update.length === 0) continue;
          try {
            doc.import(update);
            imported.push(update);
          } catch {
            /* this update is the poison; strike below */
          }
        }
      }
      if (failed) {
        // Strike the device: past IMPORT_PENALTY_STRIKES its (large) pushes
        // are rejected without import for IMPORT_PENALTY_MS (importPenalty).
        if (state.deviceId) {
          const entry = this.importPenalty.get(state.deviceId) ?? { strikes: 0, until: 0 };
          entry.strikes++;
          entry.until = now + IMPORT_PENALTY_MS;
          this.importPenalty.set(state.deviceId, entry);
          if (entry.strikes === IMPORT_PENALTY_STRIKES) {
            console.warn(
              "device entered import penalty box",
              `room=${this.getMeta("chatId") ?? "?"}`,
              `device=${state.deviceId}`
            );
          }
        }
        this.notePush(state.deviceId, false, now);
        if (imported.length > 0) {
          await this.recordLoroUpdates(imported);
          this.relay(ws, crdt, roomId, imported);
        }
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      if (state.deviceId) this.importPenalty.delete(state.deviceId);
      this.notePush(state.deviceId, true, now);
      await this.recordLoroUpdates(updates);
      this.ack(ws, { crdt, roomId }, UpdateStatusCode.Ok, batchId);
      this.relay(ws, crdt, roomId, updates);
      return;
    }
    if (crdt === CrdtType.LoroEphemeralStore) {
      const eph = this.ensureEph();
      try {
        for (const update of updates) if (update.length > 0) eph.apply(update);
      } catch {
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      this.ack(ws, { crdt, roomId }, UpdateStatusCode.Ok, batchId);
      this.relay(ws, crdt, roomId, updates);
      return;
    }
    this.ack(ws, { crdt, roomId }, UpdateStatusCode.Unknown, batchId);
  }

  /** Durability bookkeeping for accepted %LOR updates: buffer for the flush
   * batch, dirty the tail/backup caches, keep the daily alarm armed. */
  private async recordLoroUpdates(updates: Uint8Array[]): Promise<void> {
    let real = false;
    for (const update of updates) {
      if (update.length === 0) continue;
      real = true;
      this.pending.push(update);
      this.pendingBytes += update.length;
    }
    // A batch of only zero-length updates (empty POST /append body, empty
    // DocUpdate frame) recorded nothing: it must not dirty caches, arm the
    // alarm, or — critically — clear postReset, which would re-expose the
    // disaster backup to an empty-doc overwrite (round-2 review finding).
    if (!real) return;
    // These three run per batch, not per flush — but setMeta writes only on a
    // change (meta.ts), so a burst costs three rows for the whole burst rather
    // than three per batch (gh#377). Do not re-add a guard here.
    this.setMeta("tailDirty", "1");
    this.setMeta("backupDirty", "1");
    // Real state landed — the backup may advance past a wedge-break drop
    // (the monotonic VV gate in alarm() still has the final say).
    this.setMeta("postReset", "0");
    this.scheduleFlush();
    await this.markActivity();
  }

  private handleFragmentHeader(
    ws: WebSocket,
    state: SocketState,
    message: DocUpdateFragmentHeader
  ): void {
    if (!state.rooms.includes(message.crdt)) {
      this.ack(ws, message, UpdateStatusCode.PermissionDenied, message.batchId);
      return;
    }
    if (
      message.totalSizeBytes > MAX_REASSEMBLED_BYTES ||
      message.fragmentCount > MAX_FRAGMENT_COUNT
    ) {
      console.warn(
        "rejecting oversized fragment batch",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `device=${state.deviceId ?? "unattributed"}`,
        `totalSizeBytes=${message.totalSizeBytes}`,
        `fragmentCount=${message.fragmentCount}`
      );
      this.ack(ws, message, UpdateStatusCode.PayloadTooLarge, message.batchId);
      return;
    }
    // Penalized devices get rejected at the HEADER — before any reassembly
    // buffers exist. Their doomed multi-megabyte re-uploads are what pressed
    // the wasm heap into the 2026-08-04 abort loop. Small totals fall through
    // to the applyUpdates probe (PENALTY_PROBE_MAX_BYTES).
    if (
      message.crdt === CrdtType.Loro &&
      message.totalSizeBytes > PENALTY_PROBE_MAX_BYTES &&
      this.importPenalized(state.deviceId, Date.now())
    ) {
      this.notePush(state.deviceId, false, Date.now());
      this.ack(ws, message, UpdateStatusCode.InvalidUpdate, message.batchId);
      return;
    }
    let batches = this.fragments.get(ws);
    if (!batches) {
      batches = new Map();
      this.fragments.set(ws, batches);
    }
    batches.set(message.batchId, {
      // `undefined`, not an empty Uint8Array (gh#557): the placeholder has to
      // be distinguishable from a fragment that legitimately arrived, or a
      // part that never came reassembles as a zero-length one and every later
      // part lands 200_000 bytes early. Both clients that fragment into this
      // room hold the same discipline — `Option` in crates/sync/src/room.rs,
      // `nil` in apps/ios — and this was the one reassembler without it.
      parts: Array.from({ length: message.fragmentCount }, () => undefined),
      received: 0,
      totalSize: message.totalSizeBytes,
      header: message
    });
  }

  private async handleFragment(
    ws: WebSocket,
    state: SocketState,
    message: { crdt: CrdtType; roomId: string; batchId: `0x${string}`; index: number; fragment: Uint8Array }
  ): Promise<void> {
    const batch = this.fragments.get(ws)?.get(message.batchId);
    if (!batch) {
      if (message.crdt === CrdtType.Loro && this.importPenalized(state.deviceId, Date.now())) {
        // Fragments of a batch whose header we rejected (penalty box): drop
        // silently — a FragmentTimeout here would just solicit a resend of
        // the same doomed megabytes.
        return;
      }
      // Unknown batch (e.g. header lost to hibernation) — tell the sender to
      // retry the whole batch.
      this.ack(ws, message, UpdateStatusCode.FragmentTimeout, message.batchId);
      return;
    }
    // An index past the header's `fragmentCount` would extend the parts array
    // with holes, so the batch could never complete and its buffer would leak
    // until the socket died. The batch is internally inconsistent; drop it.
    if (message.index >= batch.parts.length) {
      this.refuseBatch(
        ws,
        state,
        message,
        `fragment index ${message.index} past fragmentCount ${batch.parts.length}`
      );
      return;
    }
    // Count DISTINCT indices. Counting arrivals let a repeated fragment
    // complete a batch that was still missing one — and the buffer that
    // assembles from is not the update the sender sent: short by a fragment,
    // zero-padded at the tail, everything after the hole shifted left.
    if (batch.parts[message.index] === undefined) batch.received++;
    batch.parts[message.index] = message.fragment;
    if (batch.received < batch.parts.length) return;
    this.fragments.get(ws)?.delete(message.batchId);
    const total = new Uint8Array(batch.totalSize);
    let off = 0;
    for (const part of batch.parts) {
      if (part === undefined || off + part.length > total.length) {
        // Unreachable via the two guards above unless the sender's own header
        // disagrees with its fragments. Refuse rather than import bytes that
        // are not what any device holds — a Loro update whose interior lengths
        // have shifted is exactly the shape that makes the runtime try to
        // allocate an absurd buffer.
        this.refuseBatch(ws, state, message, "fragments do not fill totalSizeBytes");
        return;
      }
      total.set(part, off);
      off += part.length;
    }
    if (off !== total.length) {
      this.refuseBatch(
        ws,
        state,
        message,
        `assembled ${off}B against a header claiming ${total.length}B`
      );
      return;
    }
    await this.applyUpdates(ws, state, message.crdt, message.roomId, message.batchId, [total]);
  }

  /** A fragment batch that cannot be assembled as its own header describes it.
   *
   * Answered `InvalidUpdate` rather than `FragmentTimeout`: a timeout asks for
   * the same batch again, and a sender whose header and fragments disagree
   * would send the same one forever. `InvalidUpdate` is bounded on both ends
   * (the client's rejoin cap, this room's penalty box) and true — nothing was
   * imported. Nothing is charged to the wasm heap: no wasm call was made. */
  private refuseBatch(
    ws: WebSocket,
    state: SocketState,
    message: { crdt: CrdtType; roomId: string; batchId: `0x${string}` },
    why: string
  ): void {
    this.fragments.get(ws)?.delete(message.batchId);
    console.warn(
      "refusing an unassemblable fragment batch",
      `room=${this.getMeta("chatId") ?? "?"}`,
      `device=${state.deviceId ?? "unattributed"}`,
      `batch=${message.batchId}`,
      why
    );
    this.notePush(state.deviceId, false, Date.now());
    this.ack(ws, message, UpdateStatusCode.InvalidUpdate, message.batchId);
  }

  // ── doc/ephemeral materialization ────────────────────────────────────────

  private async ensureDoc(): Promise<LoroDoc> {
    this.touchDoc();
    if (this.doc) {
      if (isLive(this.doc)) return this.doc;
      // Dangling wrapper (freed by a concurrent trim/release while another
      // flow still held the instance): drop it and rematerialize instead of
      // handing every caller a guaranteed throw (the 2026-08-04 ws3 wedge).
      console.error(
        "cached doc was freed (dangling wrapper); rematerializing",
        `room=${this.getMeta("chatId") ?? "?"}`
      );
      this.doc = undefined;
    }
    // AUTOMATED WEDGE BREAK: a cold replay that exceeds the DO CPU limit kills
    // the invocation before `replayAttempts` is cleared below — and every
    // reconnecting client cold-starts the room into the same death, forever
    // (the manual escape is POST /reset-log). Count consecutive replay deaths;
    // past the limit, drop the log+snapshot exactly like /reset-log does.
    // Recovery is by design lossless-enough: every engine holds the full doc
    // locally and re-uploads whatever the server lacks on its next join.
    const attempts = Number(this.getMeta("replayAttempts") ?? "0");
    if (attempts >= REPLAY_CRASH_LIMIT) {
      this.dropLog();
      // Boot every attached socket, exactly like POST /reset-log. The
      // automated wedge break used to swap the doc out from UNDER live
      // sessions: their next writes carried deps the emptied doc lacks,
      // imports failed, clients burned their capped invalid-rejoin resyncs
      // and then sat LATCHED — rows frozen on a healthy-looking socket
      // (2026-08-04: work-metal's workspace status never updated again
      // after the 20:16Z wedge-break while its chat rooms streamed fine).
      // A close → redial → empty-VV join re-uploads full state instead.
      for (const sock of this.ctx.getWebSockets()) {
        try {
          sock.close(4410, "room reset");
        } catch {
          /* already gone */
        }
      }
    }
    this.setMeta("replayAttempts", String(attempts + 1));
    // INCIDENT (2026-07-30): a CPU-limit kill ROLLS BACK the event's
    // uncommitted storage writes — so the increment above died with every
    // crash, the count never reached the limit, and the wedge break never
    // fired on the exact failure it was built for. The ws3 workspace room
    // died 7 times in two minutes and then sat wedged for 3+ hours until a
    // manual engine restart. sync() makes the count durable BEFORE the risky
    // replay below, so consecutive deaths are actually counted; clients
    // redialing on their join deadline (crates/sync/src/room.rs) supply the
    // attempts, and the room self-heals within REPLAY_CRASH_LIMIT dials.
    await this.ctx.storage.sync();
    // LOAD GUARD (gh#527), before any wasm call: a doc whose stored bytes are
    // past MAX_DOC_LOAD_BYTES is refused rather than attempted. Sized off the
    // chunk rows, so asking the question costs nothing like answering it.
    const storedBytes = this.blobs.size("snapshot") + Number(this.getMeta("updateBytes") ?? "0");
    if (storedBytes > MAX_DOC_LOAD_BYTES) {
      this.refuseLoad(`stored doc exceeds the load guard (${MAX_DOC_LOAD_BYTES}B)`, storedBytes);
    }
    // FREE THE DOC ON THE THROW PATH (gh#607). Everything below builds a
    // document that is not cached in `this.doc` until the very last step, and
    // every throw in between — a refusal, a storage sync that fails, a wasm
    // fault mid-replay — used to walk out of here leaving the whole
    // materialization resident. On a room re-materializing ~2MB of history on
    // every one of the 25-second dial cycles this ticket is about, that is the
    // climb to the 128MB isolate cap by itself. The replay proper is a sibling
    // method purely so this guard can own the only reference to the doc.
    const doc = new LoroDoc();
    try {
      return await this.replayInto(doc, storedBytes, attempts);
    } catch (e) {
      // `this.doc !== doc` is the whole question: a doc that reached the cache
      // is the room's now and lives or dies with the instance, and one that did
      // not is nobody's — freeing it here is the only chance it will ever get.
      if (this.doc !== doc) freeWasm(doc, "cold-replay-abort");
      throw e;
    }
  }

  /** The cold replay proper: snapshot + log + buffered updates into `doc`, then
   * cache it. Split out of [`ensureDoc`] so the doc has exactly one owner while
   * it is uncached — see the guard there. Takes the numbers `ensureDoc` already
   * computed rather than re-reading them, so the two cannot disagree. */
  private async replayInto(doc: LoroDoc, storedBytes: number, attempts: number): Promise<LoroDoc> {
    const started = Date.now();
    const snapshot = this.blobs.get("snapshot");
    let snapshotLoaded = false;
    if (snapshot && snapshot.length > 0) {
      try {
        doc.import(snapshot);
        snapshotLoaded = true;
      } catch (e) {
        // Heap poisoning is a different fault with its own escalation — never
        // let it be mistaken for a corrupt blob, or a pressed isolate would
        // talk healthy rooms into evicting their history.
        //
        // gh#554: ask the heap, the way gh#557 taught `escalateWasmPoisoning`
        // and `handleDocUpdate` to. These bytes came off THIS room's storage,
        // and a wasm-shaped throw over a working heap is a statement about the
        // stored snapshot, not about the isolate. Rethrowing it on shape alone
        // is why the guard below never fired on the incident it was written
        // for: the refusal path existed and the corruption never reached it.
        // A use-after-free stays exempt — it says nothing about the bytes.
        const blameTheBytes = e instanceof RangeError && wasmHeapUsable();
        if (isWasmShaped(e) && !blameTheBytes) {
          freeWasm(doc, "cold-replay");
          throw e;
        }
        // Stored bytes that will not decode. Every cold start dies here
        // forever otherwise — the room answers each join and then throws,
        // which is the 1006 loop from the client's side and, when the throw
        // happens to be a RangeError, an abort loop from ours. Refuse
        // cleanly instead; LOAD_REFUSAL_LIMIT then evicts and reseeds.
        freeWasm(doc, "cold-replay");
        this.refuseLoad(`snapshot will not import: ${String(e)}`, storedBytes);
      }
    }
    let rows = 0;
    let rowsFailed = 0;
    let lastRowError: unknown;
    for (const update of readUpdateRows(this.ctx.storage.sql)) {
      rows++;
      try {
        doc.import(update);
      } catch (e) {
        // A poisoned update cannot be applied; skip it rather than brick the
        // room — one bad row among many heals on the next fold. But COUNT it
        // (gh#554): this loop swallowed every row in turn, so a log that would
        // not replay AT ALL produced a hollow doc, silently, on every wake
        // forever. That is the same wedge a refusal names, minus the evidence
        // and minus the reseed.
        rowsFailed++;
        lastRowError = e;
      }
    }
    if (rowsFailed > 0 && !snapshotLoaded) {
      // The replay started from NOTHING and lost ops on the way: no snapshot,
      // and at least one row of the log would not import. That is not a
      // poisoned row, it is stored state this instance cannot materialize —
      // and it is the gh#554 shape exactly, where the corruption is small
      // enough to sail past the size guard (the 2026-08-19 room was 1.67MB
      // with `snapshotBytes: 0`, 5% of the threshold, and recorded zero
      // refusals all evening).
      //
      // It used to demand `rowsFailed === rows`, and that is why the same room
      // was still looping five days later at 1.94MB with `failed=5/71`. A
      // PARTIAL failure with no snapshot is not a milder version of this
      // fault, it is the same one: the doc begins empty, so a dropped row is a
      // hole no later row fills, every client update depends on ops the room
      // no longer has, and each one throws `Invalid array buffer length` out
      // of the message handler until the socket dies. Serving that doc is
      // strictly worse than refusing it — the refusal is what reaches
      // LOAD_REFUSAL_LIMIT and reseeds from the replicas every engine holds.
      //
      // Deliberately narrow. A log that fails BEHIND a snapshot that loaded
      // keeps the skip above: the doc still holds the room's history, the next
      // fold rewrites the log out of it, and evicting there would spend a good
      // snapshot on a bad tail.
      freeWasm(doc, "cold-replay");
      const blameTheBytes = lastRowError instanceof RangeError && wasmHeapUsable();
      if (isWasmShaped(lastRowError) && !blameTheBytes) throw lastRowError;
      this.refuseLoad(
        `stored log will not import (${rows} rows): ${String(lastRowError)}`,
        storedBytes
      );
    }
    if (rowsFailed > 0) {
      // Skipped rows are silent data loss — say so. A room whose tail never
      // replays reads to its users as "my last messages vanished", and read to
      // this code, until now, as a perfectly ordinary cold start.
      console.error(
        "stored update rows would not import; replaying without them",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `failed=${rowsFailed}/${rows}`,
        String(lastRowError)
      );
    }
    for (const update of this.pending) {
      try {
        doc.import(update);
      } catch {
        /* same */
      }
    }
    this.setMeta("replayAttempts", "0");
    // Scope the crash budget to the replay ALONE: without this second sync, a
    // CPU kill later in the same event (a backfill export for a fresh client,
    // the alarm's shallow trim) would roll back this reset while the synced
    // increment above survives — three such deaths would wedge-break a room
    // whose replay is perfectly healthy (adversarial-review finding). One
    // extra sync, cold path only. Deliberate consequence: a deterministic
    // POST-replay death (a doc so big its snapshot export blows the CPU
    // limit) gets no automatic wedge-break — destroying state over an export
    // problem is worse than looping loudly. That class is watched via
    // lastReplayMs creep and escaped manually with POST /reset-log.
    await this.ctx.storage.sync();
    // Cold-start telemetry (Workers Logs + /stats): the replay cost is the
    // wedge risk — watch lastReplayMs trend toward the CPU limit to catch the
    // next 2026-07-30 while it is still a statistic, not an incident.
    const replayMs = Date.now() - started;
    this.setMeta("lastReplayMs", String(replayMs));
    this.setMeta("lastReplayRows", String(rows));
    // The doc loaded: whatever the guard was refusing is gone (a fold replaced
    // the bad snapshot, a trim shrank the room, an eviction reseeded it).
    // Consecutive, like every other budget here.
    this.setMeta("loadRefusals", "0");
    console.log(
      `cold replay: ${replayMs}ms, ${rows} rows, snapshot ${snapshot?.length ?? 0}B, attempt ${attempts + 1}`,
      `room=${this.getMeta("chatId") ?? "?"}`
    );
    this.doc = doc;
    // Record a frontier checkpoint on cold start too: the alarm only records
    // while WRITES keep it armed, so an idle room never aged into trim
    // eligibility — it could never shrink, ever. One checkpoint a day max.
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]") as FrontierCheckpoint[];
    const newest = checkpoints[checkpoints.length - 1];
    if (!newest || Date.now() - newest.at >= DAY_MS) {
      checkpoints.push({
        at: Date.now(),
        frontiers: doc.frontiers().map((f) => ({ peer: String(f.peer), counter: f.counter }))
      });
      while (checkpoints.length > MAX_CHECKPOINTS) checkpoints.shift();
      this.setMeta("checkpoints", JSON.stringify(checkpoints));
    }
    // Trim on cold materialization too: fold and alarm both ride WRITES, so
    // an idle-but-watched room NEVER trimmed — yet every isolate restart
    // re-materializes its full history into the shared wasm heap (the
    // 2026-08-04 exhaustion recurred post-fix on exactly those rooms). The
    // one-off export cost here permanently shrinks the room.
    if (await this.trimHistoryIfDue(doc, Date.now())) {
      console.log(`history trimmed on cold start room=${this.getMeta("chatId") ?? "?"}`);
    }
    return this.doc;
  }

  /** Refuse to materialize, and count it (gh#527).
   *
   * The refusal itself is the fix: an unloadable doc that is ATTEMPTED kills
   * the invocation, and a killed invocation is a socket that died 1006 with
   * nothing logged and a client that redials into the identical death. A
   * refusal is a `JoinError` the client can back off from, a line in the
   * journal, and a number on `/stats`.
   *
   * Past LOAD_REFUSAL_LIMIT consecutive refusals the room evicts its own
   * stored state (gh#148/#207): a doc no instance can load is a doc no client
   * can repair by dialing, and every engine holds the full document locally
   * and re-uploads what the server lacks on its next join. `dropLog` sets
   * `postReset`, so the R2 disaster copy is NOT overwritten by the emptied
   * doc — the eviction is recoverable in both directions.
   *
   * Never returns. */
  private refuseLoad(detail: string, bytes: number): never {
    const refusals = Number(this.getMeta("loadRefusals") ?? "0") + 1;
    this.setMeta("loadRefusals", String(refusals));
    this.setMeta(
      "lastLoadRefusal",
      JSON.stringify({ at: Date.now(), detail, bytes }).slice(0, 512)
    );
    console.error(
      "refusing to load this room's doc",
      `room=${this.getMeta("chatId") ?? "?"}`,
      `bytes=${bytes}`,
      `guard=${MAX_DOC_LOAD_BYTES}`,
      `refusals=${refusals}/${LOAD_REFUSAL_LIMIT}`,
      detail
    );
    if (refusals >= LOAD_REFUSAL_LIMIT) {
      this.setMeta("loadRefusals", "0");
      this.evictAndReseed(bytes, detail);
    }
    throw new DocLoadRefused(detail, bytes);
  }

  /** Throw away this room's stored doc and let the fleet rebuild it
   * (gh#148/#207) — the escalation both consecutive-fault budgets end in.
   *
   * Extracted for gh#607, which needs the same ending from a counter of its
   * own. `dropLog` sets `postReset`, so the R2 disaster copy is not overwritten
   * by the emptied doc; booting the sockets is what makes the redial arrive
   * with an EMPTY version vector and re-upload full state, where a live
   * session's next writes would carry deps the emptied doc lacks. */
  private evictAndReseed(bytes: number, why: string): void {
    console.error(
      "evicting this room's stored state; the fleet reseeds it on rejoin",
      `room=${this.getMeta("chatId") ?? "?"}`,
      `bytes=${bytes}`,
      why
    );
    this.dropLog();
    for (const sock of this.ctx.getWebSockets()) {
      try {
        sock.close(4411, "room reseed");
      } catch {
        /* already gone */
      }
    }
  }

  /** Drop the cached doc, whatever state it is in (gh#607).
   *
   * The order is load-bearing: the field is cleared FIRST, so a `free()` that
   * throws can never leave the poisoned wrapper cached for the next join to
   * pick up. That is the bug `releaseIdleDoc` had — its free threw the borrow
   * conflict out of a bare timer and the line that cleared `this.doc` never
   * ran, so the room went on serving the one object in the isolate that could
   * not be used or released.
   *
   * A leaked free is recorded, not swallowed: a room that has abandoned
   * megabytes into the shared wasm heap is the thing an operator needs to see
   * on `/stats` when the isolate starts resetting on its memory limit. */
  private abandonDoc(site: string): FreeOutcome {
    const doc = this.doc;
    this.doc = undefined;
    const outcome = freeWasm(doc, site);
    if (outcome === "leaked") {
      const leaks = Number(this.getMeta("borrowLeaks") ?? "0") + 1;
      try {
        this.setMeta("borrowLeaks", String(leaks));
      } catch {
        /* diagnostics on an already-bad path */
      }
    }
    return outcome;
  }

  /** Reclassify a caught fault before any other guard reads it (gh#607).
   *
   * Returns the fault to handle: `e` untouched unless it is a stuck wasm
   * borrow, in which case the poisoned wrapper is dropped, the conflict is
   * counted toward the reseed, and a [`DocBorrowConflict`] comes back — which
   * IS a `DocLoadRefused`, so every catch downstream already answers it
   * correctly and none of them strikes the poison tripwire.
   *
   * Dropping the wrapper is the half that usually ends it: the next
   * materialization is a fresh `LoroDoc` off the same stored bytes, a ~tens of
   * ms cold replay, and the room serves again. The counter is the backstop for
   * when it does not — [`LOAD_REFUSAL_LIMIT`] in a row and the stored state
   * that keeps producing the fault is evicted and reseeded from the replicas
   * every engine holds.
   *
   * Its OWN counter, deliberately, spending the same limit. `loadRefusals` says
   * "this doc will not materialize", and a successful cold replay clears it —
   * which a borrow conflict always follows, because the wedge shows up on the
   * doc AFTER it loaded. Booked there, the count would be reset by the very
   * rematerialization that precedes the next conflict and the eviction could
   * never arrive, which is (4) in the diagnosis wearing a different hat.
   * Consecutive like every other budget here: cleared by a join that completes.
   */
  private reclassifyBorrow(e: unknown, site: string): unknown {
    if (!isWasmBorrowConflict(e)) return e;
    const bytes = this.storedDocBytes();
    const outcome = this.abandonDoc(site);
    const strikes = Number(this.getMeta("borrowStrikes") ?? "0") + 1;
    try {
      this.setMeta("borrowStrikes", String(strikes));
      this.setMeta("borrowConflicts", String(Number(this.getMeta("borrowConflicts") ?? "0") + 1));
      this.setMeta(
        "lastBorrowConflict",
        JSON.stringify({ at: Date.now(), site, outcome, error: String(e) }).slice(0, 512)
      );
    } catch {
      /* storage refused the marker; the classification still stands */
    }
    console.error(
      "wasm borrow conflict: one object is wedged, the heap is not (gh#607)",
      `room=${this.getMeta("chatId") ?? "?"}`,
      `site=${site}`,
      `cachedDoc=${outcome}`,
      `strikes=${strikes}/${LOAD_REFUSAL_LIMIT}`,
      String(e)
    );
    if (strikes >= LOAD_REFUSAL_LIMIT) {
      this.setMeta("borrowStrikes", "0");
      this.evictAndReseed(bytes, `wasm borrow conflict at ${site}: ${String(e)}`);
    }
    return new DocBorrowConflict(site, e, bytes);
  }

  /** Idle-doc release (see DOC_IDLE_RELEASE_MS): a debounced timer frees the
   * materialized doc after a quiet minute. Timer only exists while traffic
   * keeps the DO awake — same hibernation discipline as the flush debounce.
   * Buffered `pending` updates survive a release: cold replay re-imports
   * them (see ensureDoc). */
  private touchDoc(): void {
    this.lastDocUse = Date.now();
    if (this.docIdleTimer) return;
    this.docIdleTimer = setTimeout(() => this.releaseIdleDoc(), DOC_IDLE_RELEASE_MS + 500);
  }

  private releaseIdleDoc(): void {
    this.docIdleTimer = undefined;
    if (!this.doc) return;
    const idle = Date.now() - this.lastDocUse;
    if (idle < DOC_IDLE_RELEASE_MS) {
      this.docIdleTimer = setTimeout(
        () => this.releaseIdleDoc(),
        Math.max(DOC_IDLE_RELEASE_MS - idle, 1_000) + 500
      );
      return;
    }
    // Same liveness rule as everywhere else (see `isLive`): the cached wrapper
    // can already have been freed by a flow that did not clear the field, and
    // `free()` on a zeroed pointer is a double free, not a no-op. This one fires
    // from a bare timer, where the throw has no caller to answer to — which is
    // why it goes through `abandonDoc`: that clears the field FIRST, so a doc
    // stuck borrowed (gh#607) cannot survive its own failed release and be
    // handed to the next join.
    this.abandonDoc("idle-release");
  }

  /** Drop the persisted update log + snapshot (the /reset-log storage clear):
   * the next materialization starts empty and engines re-upload state on
   * rejoin. Preserves owner/chatId meta. */
  private dropLog(): void {
    this.ctx.storage.sql.exec("DELETE FROM updates");
    this.blobs.delete("snapshot");
    this.setMeta("updateBytes", "0");
    this.setMeta("checkpoints", "[]");
    this.setMeta("lastTrimAt", "");
    // An emptied log has nothing to fold, so the fold backoff must not outlive
    // it: the reseed that follows is exactly when the room needs to be able to
    // compact again, and carrying an hour-long ladder across the drop would
    // hold it open through the whole re-upload (gh#557).
    this.setMeta("foldFailures", "0");
    this.setMeta("foldRetryAt", "0");
    this.setMeta("lastFoldFailure", "");
    this.pending = [];
    this.pendingBytes = 0;
    // Until an engine re-uploads real state, anything materialized from here
    // is empty — postReset gates the nightly R2 put so the DISASTER backup
    // cannot be overwritten by the emptied doc. Without it, the durable crash
    // counter let alarm auto-retries complete a wedge break with ZERO clients
    // connected and then back up the empty doc in the same invocation,
    // destroying the one copy that exists for the engine-never-returns case
    // (adversarial-review finding). Cleared by recordLoroUpdates.
    this.setMeta("postReset", "1");
  }

  private ensureEph(): EphemeralStore {
    if (!this.eph) this.eph = new EphemeralStore(30_000);
    return this.eph;
  }

  // ── derived presence (gh#145) ────────────────────────────────────────────

  /** `{deviceId: lastSeenAt}` for every device with a live socket on this room.
   *
   * `exclude` drops the socket a close is being handled for — the runtime still
   * lists it there, and counting it would report a device as present in the
   * very answer its departure triggered. */
  private socketPresence(exclude?: WebSocket): Record<string, number> {
    return livePresence(
      this.ctx.getWebSockets().map((ws) => {
        if (ws === exclude) return { lastSeenAt: 0 };
        const state = ws.deserializeAttachment() as SocketState | null;
        return {
          deviceId: state?.deviceId,
          // Auto-pongs are stamped even while hibernating — this is the whole
          // trick; `joinedAt` covers the window before a fresh socket's first
          // ping. A socket attached by an older deploy has neither and simply
          // contributes nothing.
          lastSeenAt: Math.max(
            this.ctx.getWebSocketAutoResponseTimestamp(ws)?.getTime() ?? 0,
            state?.joinedAt ?? 0
          )
        };
      }),
      Date.now()
    );
  }

  /** Recompute derived presence into the `%EPH` store and push it to every
   * joined presence socket.
   *
   * Only called from events that ALREADY woke this object — a join, a close —
   * so the whole presence channel costs no wake of its own. That is the gh#145
   * fix in one line: the room answers when something happens, and when it is
   * asked (`GET /presence`), and otherwise sleeps.
   *
   * Devices whose sockets are gone have their key DELETED rather than left to
   * age out, so a clean departure is instant instead of a TTL later. (A
   * transitional cost: an engine older than gh#145 still beats its own key in,
   * and a delete here races that beat — it re-appears within its 15s cadence,
   * so the flicker is bounded and self-healing.) */
  private publishPresence(exclude?: WebSocket): void {
    const live = this.socketPresence(exclude);
    const eph = this.ensureEph();
    for (const key of Object.keys(eph.getAllStates())) {
      if (!key.startsWith(PRESENCE_PREFIX)) continue;
      if (live[key.slice(PRESENCE_PREFIX.length)] === undefined) eph.delete(key);
    }
    for (const [deviceId, at] of Object.entries(live)) eph.set(`${PRESENCE_PREFIX}${deviceId}`, at);
    const all = eph.encodeAll();
    if (all.length === 0) return;
    const roomId = this.getMeta("chatId") ?? "";
    for (const ws of this.ctx.getWebSockets()) {
      if (ws === exclude) continue;
      const state = ws.deserializeAttachment() as SocketState | null;
      if (!state?.rooms.includes(CrdtType.LoroEphemeralStore)) continue;
      this.sendUpdates(ws, CrdtType.LoroEphemeralStore, roomId, [all]);
    }
  }

  // ── durability: flush, compaction, backups ───────────────────────────────

  private scheduleFlush(): void {
    if (this.flushTimer) return;
    this.flushTimer = setTimeout(() => {
      this.flushTimer = undefined;
      // Not `void`: this is the ONLY flush call site with no handler above it,
      // so a throw here (a fold export dying, a storage error) used to vanish
      // as an unhandled rejection — the 2026-08-05 whale rooms failed every
      // debounced flush for days with zero log lines. Same discipline as the
      // socket-close flush.
      this.flush().catch((e) => {
        console.error("debounced flush failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
        this.escalateWasmPoisoning(e, "flush-debounced");
      });
    }, DO_FLUSH_MS);
  }

  private async flush(): Promise<void> {
    if (this.flushTimer) {
      clearTimeout(this.flushTimer);
      this.flushTimer = undefined;
    }
    if (this.pending.length === 0) return;
    const now = Date.now();
    for (const update of this.pending) {
      // Chunked rows (update-log.ts): a single update above the ~2MB SQL row
      // cap — a bulk import, a whale session's full re-upload span — used to
      // throw SQLITE_TOOBIG here on every flush forever, freezing the room's
      // persistence while acks kept reading Ok (2026-08-05).
      appendUpdateRow(this.ctx.storage.sql, update, now);
    }
    const logBytes = Number(this.getMeta("updateBytes") ?? "0") + this.pendingBytes;
    this.setMeta("updateBytes", String(logBytes));
    this.pending = [];
    this.pendingBytes = 0;
    // A fold that just failed is not retried on the next write (gh#557). The
    // fold trigger is a THRESHOLD, and a failed fold clears nothing — so a room
    // whose snapshot export will not complete crosses it again on every single
    // flush, burning a wasm export and (before this ticket) a poison strike
    // each time. Back off instead; see `noteFoldFailure`. Rows are already
    // appended above: this defers compaction, never durability.
    if (now < Number(this.getMeta("foldRetryAt") ?? "0")) return;
    // Fold on EITHER budget: bytes bounds one huge update, rows bounds many
    // tiny ones — a cold `ensureDoc` replay pays per-import overhead per row,
    // so a high row count is as expensive as a high byte count (see
    // COMPACT_LOG_ROWS). COUNT(*) is a cheap indexed read, once per flush.
    if (logBytes > COMPACT_LOG_BYTES) {
      await this.foldLog();
      return;
    }
    const rows = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n as
      | number
      | undefined;
    if ((rows ?? 0) > COMPACT_LOG_ROWS) await this.foldLog();
  }

  /** LOG FOLD: snapshot re-export + clear the update log. Prefers a shallow
   * trim when one is due — waiting for the DAILY alarm meant a heap-pressed
   * colo (2026-08-04 wasm exhaustion) kept thrash-cycling for up to a day
   * after the retention fix deployed; a high-churn room folds every ~400
   * rows, so trimming here converges in minutes instead. Falls back to the
   * lossless full snapshot when no trim is due (or the trim export fails).
   *
   * Never throws (gh#557). The trim above has always caught its own export —
   * "best-effort, leave the room to the caller's lossless fold" — but the fold
   * IS that caller, and it had no catch of its own, so the fallback's fallback
   * was whatever the flush's caller did with the exception: on the debounced
   * timer, on socket close, on socket error and in the alarm, all four hand it
   * to `escalateWasmPoisoning`. One room that could not export its snapshot
   * therefore aborted the isolate — severing every co-located room's sockets —
   * and did it again on the next write, because a failed fold leaves the log
   * exactly as large as the threshold that triggered it. */
  private async foldLog(): Promise<void> {
    const doc = await this.ensureDoc();
    if (await this.trimHistoryIfDue(doc, Date.now())) {
      this.noteFoldSuccess();
      return;
    }
    // Re-resolve after the await above: a concurrent trim may have replaced
    // and FREED the wrapper captured in `doc` (guarded ensureDoc returns the
    // live cached doc, or cheaply rematerializes).
    const live = await this.ensureDoc();
    try {
      // Export AND put together: a put that dies partway leaves chunk rows
      // that do not add up to a snapshot, and the log below is the only other
      // copy of that state. Dropping the partial blob keeps the log
      // authoritative instead of handing the next cold start a snapshot that
      // will not import.
      this.blobs.put("snapshot", live.export({ mode: "snapshot" }));
    } catch (e) {
      try {
        this.blobs.delete("snapshot");
      } catch {
        /* nothing to undo, or storage is refusing everything */
      }
      this.noteFoldFailure(e);
      return;
    }
    this.ctx.storage.sql.exec("DELETE FROM updates");
    this.setMeta("updateBytes", "0");
    this.noteFoldSuccess();
  }

  /** The log folded: clear the backoff ladder and the failure it recorded. */
  private noteFoldSuccess(): void {
    if (this.getMeta("foldFailures")) {
      this.setMeta("foldFailures", "0");
      this.setMeta("foldRetryAt", "0");
      this.setMeta("lastFoldFailure", "");
    }
  }

  /** The log did NOT fold: back off, say so durably, and classify the fault.
   *
   * The room keeps working — reads, writes, joins and relays never touched the
   * fold — it simply cannot compact, so its log goes on growing until either a
   * later attempt succeeds or a checkpoint ages into trim eligibility (a
   * `ws4` room reset yesterday becomes trimmable a day later, `dropLog` having
   * cleared its checkpoints). `MAX_DOC_LOAD_BYTES` is the backstop under all
   * of that. What must NOT happen is the room taking the isolate down over it,
   * which is what `escalateWasmPoisoning` is now able to decline. */
  private noteFoldFailure(e: unknown): void {
    const failures = Number(this.getMeta("foldFailures") ?? "0") + 1;
    this.setMeta("foldFailures", String(failures));
    // Same ladder the alarm chain uses: a minute, doubling, capped at an hour.
    this.setMeta("foldRetryAt", String(Date.now() + alarmRetryDelay(failures)));
    this.setMeta(
      "lastFoldFailure",
      JSON.stringify({ at: Date.now(), failures, error: String(e) }).slice(0, 512)
    );
    console.error(
      "log fold failed; backing off (the room still serves, it just cannot compact)",
      `room=${this.getMeta("chatId") ?? "?"}`,
      `consecutiveFailures=${failures}`,
      `updateBytes=${this.getMeta("updateBytes") ?? "0"}`,
      String(e)
    );
    this.escalateWasmPoisoning(e, "fold-export");
  }

  /** HISTORY TRIM (§3.1): shallow snapshot at the newest recorded frontier
   * checkpoint older than RETAIN_DAYS — history before it is discarded
   * permanently, state fully preserved. Returns whether a trim landed (the
   * snapshot + log + materialized doc were all replaced — the passed `doc`
   * is CONSUMED: its wasm memory is freed, callers must switch to
   * `this.doc`). Best-effort: any export failure leaves the room to the
   * caller's lossless fold. */
  private async trimHistoryIfDue(doc: LoroDoc, now: number): Promise<boolean> {
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]") as FrontierCheckpoint[];
    const cutoff = checkpoints.filter((c) => now - c.at >= RETAIN_MS).pop();
    let frontiers: { peer: `${number}`; counter: number }[];
    // lastTrimAt alone gates the cutoff trim: the trim is durable (sync()
    // below), and requiring doc.isShallow() re-fired it on EVERY cold start
    // once a log fold re-exported the once-shallow doc as a regular
    // snapshot (isShallow reads false after rematerializing from it) —
    // observed as the same rooms "trimming" every few minutes all evening.
    if (cutoff && this.getMeta("lastTrimAt") !== String(cutoff.at)) {
      frontiers = cutoff.frontiers.map((f) => ({ peer: f.peer as `${number}`, counter: f.counter }));
    } else if (
      (this.blobs.get("snapshot")?.length ?? 0) + Number(this.getMeta("updateBytes") ?? "0") >
      TRIM_FORCE_BYTES
    ) {
      // Snapshot AND log bytes: after a wedge-break reset the re-uploaded
      // full histories live as LOG ROWS against an empty snapshot (observed
      // live: 0B snapshot + 119 rows replaying for 7 SECONDS), so a
      // snapshot-only gate never fired while every cold start ballooned the
      // heap with the same megabytes.
      // No aged checkpoint but the full history is already a heap hazard:
      // trim at the current frontier (see TRIM_FORCE_BYTES).
      // Which rooms every device writes at once — see [`isConcurrentWriteRoom`]
      // for why the pattern is generation-agnostic and why our org device
      // registry has to be in it too.
      if (isConcurrentWriteRoom(this.getMeta("chatId"))) {
        // Never force-trim these at the LIVE frontier: a live-frontier shallow
        // start orphans any peer whose next ops depend on history just
        // discarded — their pushes InvalidUpdate forever and, worse, a
        // post-wedge-break trim can shallow-lock the room before all
        // engines finish re-uploading (2026-08-04: the 20:34Z force-trim
        // froze on a partial rebuild; the whole fleet needed manual doc
        // surgery to converge). Trim only at a checkpoint ≥1 day old —
        // every recently-active device has passed it, and dropLog clears
        // checkpoints, so a freshly reset room gets a full day of grace to
        // re-form before any trim. Chat rooms (single-owner, the 2026-08-04
        // whale imports) keep the immediate live-frontier trim.
        const aged = checkpoints.filter((c) => now - c.at >= DAY_MS).pop();
        if (!aged) return false;
        frontiers = aged.frontiers.map((f) => ({ peer: f.peer as `${number}`, counter: f.counter }));
      } else {
        frontiers = doc.frontiers().map((f) => ({ peer: String(f.peer) as `${number}`, counter: f.counter }));
      }
    } else {
      return false;
    }
    try {
      const shallow = doc.export({
        mode: "shallow-snapshot",
        frontiers
      });
      this.blobs.put("snapshot", shallow);
      this.ctx.storage.sql.exec("DELETE FROM updates");
      this.setMeta("updateBytes", "0");
      this.setMeta("lastTrimAt", String(cutoff?.at ?? now));
      // Make the trim durable NOW: a later kill in the same event (a join's
      // backfill export on a pressed isolate — observed live 2026-08-04)
      // rolls back uncommitted storage writes, silently resurrecting the
      // full-history snapshot the trim just replaced.
      await this.ctx.storage.sync();
      const fresh = new LoroDoc();
      fresh.import(shallow);
      const old = this.doc;
      this.doc = fresh;
      // Free the replaced cached doc AND the caller's (possibly distinct,
      // stale) doc exactly once each — waiting on GC finalizers leaks them
      // into the shared wasm heap exactly when trimming was supposed to
      // relieve it (see handleJoin). The isLive guards keep a concurrent
      // interleaved trim from double-freeing what a sibling already returned
      // to the allocator. `freeWasm` carries that guard and adds the gh#607
      // one: a doc stuck borrowed must not throw out of here and undo the trim
      // that already landed — it is leaked, said so, and the room moves on.
      if (old !== fresh) freeWasm(old, "trim-replaced-doc");
      if (doc !== fresh && doc !== old) freeWasm(doc, "trim-caller-doc");
      return true;
    } catch {
      return false;
    }
  }

  /** Daily alarm: frontier checkpoint, history trim, R2 backup — bounded by a
   * consecutive-failure counter (gh#378).
   *
   * The work itself is in [`runScheduledWork`]; this wrapper is the budget.
   * Every attempt is counted BEFORE the work starts and cleared when it
   * completes, so the counter survives the failure class that has no catch
   * block — a CPU-limit kill takes the whole invocation and rolls back
   * anything not yet committed (the same reasoning as `replayAttempts` in
   * `ensureDoc`, and the same `sync()`).
   *
   * A failure is SWALLOWED, not rethrown: rethrowing hands scheduling back to
   * the runtime's own retry, which is exactly the chain being replaced. This
   * room owns its ladder now — `alarmRetryDelay` until the budget runs out,
   * then nothing, with `alarmGaveUpAt` left behind to say so on /stats.
   *
   * Giving up is not permanent, and must not be: the alarm chain is how a
   * wedged room repairs and backs itself up with zero clients connected. Any
   * join, /tail read, or write revives it — see [`reviveAlarm`]. `backupDirty`
   * is deliberately left set through all of this: the work is still owed. */
  async alarm(): Promise<void> {
    // The one wake with no client behind it — and therefore the only chance a
    // room that lost every socket at 03:00 has to record that it happened
    // before the next dial (gh#527).
    this.reconcileSockets();
    const failures = Number(this.getMeta("alarmFailures") ?? "0");
    if (failures >= ALARM_FAILURE_LIMIT) {
      // Reached only when the runtime retries an invocation that died where
      // no catch could see it. Same terminus as the catch below.
      this.noteAlarmGaveUp(failures);
      return;
    }
    this.setMeta("alarmFailures", String(failures + 1));
    await this.ctx.storage.sync();
    const attempts = failures + 1;
    try {
      await this.runScheduledWork();
    } catch (raw) {
      // Same reclassification the message handler does (gh#607), and for the
      // same reason the refusal budget exists (gh#554): a wedged doc does not
      // heal by waiting, the room already owns the escalation that repairs it,
      // and spending the give-up budget on it would strand the trim, snapshot
      // and backup of a room that is three attempts from reseeding itself.
      const e = this.reclassifyBorrow(raw, "alarm");
      const refusals = Number(this.getMeta("alarmRefusals") ?? "0");
      if (e instanceof DocLoadRefused && refusals < ALARM_REFUSAL_BUDGET) {
        // gh#554: a refusal is not an alarm failure. The guard has already
        // counted it and will evict and reseed within LOAD_REFUSAL_LIMIT
        // attempts — the alarm's job is to keep ARRIVING until it does, not to
        // count down to a permanent give-up on a room that is about to heal.
        // Restore the pre-spent failure and come back on the base delay, so the
        // strikes land in minutes rather than across the ladder's hours.
        this.setMeta("alarmFailures", String(failures));
        this.setMeta("alarmRefusals", String(refusals + 1));
        console.warn(
          "alarm refused by the load guard; retrying while it reseeds",
          `room=${this.getMeta("chatId") ?? "?"}`,
          `alarmRefusals=${refusals + 1}/${ALARM_REFUSAL_BUDGET}`,
          e.message
        );
        await this.ctx.storage.setAlarm(Date.now() + ALARM_RETRY_BASE_MS);
        return;
      }
      console.error(
        "alarm failed",
        `room=${this.getMeta("chatId") ?? "?"}`,
        `consecutiveFailures=${attempts}`,
        String(e)
      );
      if (attempts >= ALARM_FAILURE_LIMIT) {
        this.noteAlarmGaveUp(attempts);
      } else {
        // LOAD-BEARING that this is NOT itself wrapped in a try: swallowing
        // the work's failure means our setAlarm is the only reschedule left,
        // so if the reschedule ITSELF fails the chain would end silently
        // mid-budget. Uncaught, it escapes alarm() and the runtime's own
        // retry fires as the backstop — the one place a throw is still the
        // right answer. A tidy that catches here removes that; don't.
        await this.ctx.storage.setAlarm(Date.now() + alarmRetryDelay(attempts));
      }
      // Escalate LAST: a wasm-poisoning strike-out calls ctx.abort(), which
      // tears this instance down where it stands — anything after it may never
      // run, and the ladder is what must survive.
      this.escalateWasmPoisoning(e, "alarm");
      return;
    }
    // Completed — the chain is clean. One failure followed by a success costs
    // nothing but the retry that healed it.
    this.setMeta("alarmFailures", "0");
    if (this.getMeta("alarmRefusals")) this.setMeta("alarmRefusals", "0");
    if (this.getMeta("alarmGaveUpAt")) this.setMeta("alarmGaveUpAt", "");
  }

  /** Stop rescheduling and leave the state that says so. Recorded once, so
   * "since when" answers the first give-up rather than the latest retry. */
  private noteAlarmGaveUp(failures: number): void {
    if (!this.getMeta("alarmGaveUpAt")) this.setMeta("alarmGaveUpAt", String(Date.now()));
    console.error(
      "alarm gave up; no further retries until a client returns",
      `room=${this.getMeta("chatId") ?? "?"}`,
      `consecutiveFailures=${failures}`
    );
  }

  /** A client came back: undo a give-up and re-arm the daily chain (gh#378).
   *
   * Returns whether it revived anything, so `markActivity` can skip its own
   * arming when this already did it.
   *
   * Deliberately narrow: a counter mid-ladder is NOT reset. Resetting on every
   * join would let one flapping client restart the retry budget indefinitely —
   * the same unboundedness in a client's clothes. A room that goes on to give
   * up is revived by the next join, /tail read or write anyway, which is the
   * case that matters: a room that has stopped retrying must never be a room
   * that has stopped backing up. */
  private async reviveAlarm(): Promise<boolean> {
    if (!this.getMeta("alarmGaveUpAt")) return false;
    // Arm first: if storage refuses, the give-up marker remains truthful and
    // the initiating event fails instead of claiming this room was revived.
    await this.armDailyAlarm();
    this.setMeta("alarmGaveUpAt", "");
    this.setMeta("alarmFailures", "0");
    this.setMeta("alarmRefusals", "0");
    console.log("alarm re-armed after give-up", `room=${this.getMeta("chatId") ?? "?"}`);
    return true;
  }

  /** The scheduled work proper: frontier checkpoint, history trim, R2 backup. */
  private async runScheduledWork(): Promise<void> {
    await this.flush();
    if (this.getMeta("backupDirty") !== "1") return; // idle: stop the chain
    const doc = await this.ensureDoc();
    const now = Date.now();

    // 1. Record today's frontier checkpoint.
    const checkpoints = JSON.parse(this.getMeta("checkpoints") ?? "[]") as FrontierCheckpoint[];
    checkpoints.push({
      at: now,
      frontiers: doc.frontiers().map((f) => ({ peer: String(f.peer), counter: f.counter }))
    });
    while (checkpoints.length > MAX_CHECKPOINTS) checkpoints.shift();

    // 2. HISTORY TRIM — must see today's checkpoint list, so persist first.
    //    (Also fires from foldLog, which is what usually gets there first on
    //    a high-churn room.)
    this.setMeta("checkpoints", JSON.stringify(checkpoints));
    await this.trimHistoryIfDue(doc, now);

    // 3. Nightly R2 backup (§3.3) — full current snapshot, disaster hatch.
    // Two guards (round-2 review): postReset pauses the put between a
    // wedge-break drop and the first re-uploaded state, and the put is
    // MONOTONIC — the new snapshot must version-include the previously
    // backed-up one, so even a post-drop doc that took a few fresh writes
    // (clearing postReset) can never replace the last good copy with a
    // hollow one. CRDT merge guarantees a genuinely recovered doc includes
    // the old VV, at which point the put resumes; until then backupDirty
    // stays set and the alarm chain keeps trying — for as long as its budget
    // lasts, and then until a client returns (gh#378). Note this branch does
    // not THROW: a paused or non-advancing put is a completed alarm, not a
    // failed one, so it never spends the failure budget.
    const chatId = this.getMeta("chatId");
    if (chatId && this.getMeta("postReset") !== "1") {
      // Guarded re-resolve, not `this.doc ?? doc`: the trim above may have
      // freed either reference, and `doc` was captured before several awaits.
      const current = await this.ensureDoc();
      const prevVV = this.getMeta("backupVV");
      let advances = true;
      if (prevVV) {
        let prev: VersionVector | undefined;
        let cur: VersionVector | undefined;
        try {
          prev = VersionVector.decode(Uint8Array.from(atob(prevVV), (c) => c.charCodeAt(0)));
          cur = current.version();
          const cmp = cur.compare(prev);
          advances = cmp !== undefined && cmp >= 0;
        } catch {
          /* unreadable meta: allow the put and rewrite it below */
        } finally {
          // Explicit frees: see handleJoin — GC finalizers don't run under
          // wasm-side memory pressure.
          freeWasm(prev, "backup-prev-vv");
          freeWasm(cur, "backup-current-vv");
        }
      }
      if (advances) {
        // Read everything off the doc BEFORE the R2 await — a concurrent
        // trim/release during the put can free `current` under us.
        const snapshot = current.export({ mode: "snapshot" });
        const vv = current.version();
        let vvB64: string;
        try {
          vvB64 = btoa(String.fromCharCode(...vv.encode()));
        } finally {
          freeWasm(vv, "backup-vv");
        }
        await this.env.BLOBS.put(`backup/${chatId}/latest.loro`, snapshot);
        this.setMeta("backupVV", vvB64);
        this.setMeta("backupDirty", "0");
      }
    }
    // Re-arm only while there is a reason to wake again; markActivity re-arms
    // on the next write otherwise.
  }

  /** Arm the daily alarm if none is scheduled (called on every write). A write
   * is also a client returning, so it revives a room that gave up. */
  private async markActivity(): Promise<void> {
    if (await this.reviveAlarm()) return; // already re-armed
    await this.armDailyAlarm();
  }

  private async armDailyAlarm(): Promise<void> {
    await this.dailyAlarm.armAfter(DAY_MS);
  }

  private async currentTail(): Promise<unknown> {
    // An explicit read counts as a client returning too (gh#378): the L2 tail
    // is what an instant-open does before any socket exists, so on a room
    // nobody has joined yet it is the earliest evidence anyone is watching.
    await this.reviveAlarm();
    await this.flush();
    if (this.getMeta("tailDirty") !== "1") {
      const cached = getJsonBlob<unknown>(this.blobs, "tail");
      if (cached !== undefined) return cached;
    }
    const doc = await this.ensureDoc();
    const tail = materializeTail(doc, Date.now());
    putJsonBlob(this.blobs, "tail", tail);
    this.setMeta("tailDirty", "0");
    return tail;
  }

  // ── wire helpers ─────────────────────────────────────────────────────────

  /** Returns false when the frame could not be delivered (socket gone /
   * runtime refused the send). Encode failures throw out instead — they are
   * OUR bug, never the peer's, and must not be mistaken for a deaf socket. */
  private send(ws: WebSocket, message: ProtocolMessage): boolean {
    const bytes = encode(message);
    try {
      ws.send(bytes);
      return true;
    } catch {
      /* socket already gone; hibernation API cleans it up */
      return false;
    }
  }

  /** Send updates, fragmenting any single update above FRAGMENT_BYTES and
   * chunking small ones so no encoded frame approaches the loro-protocol
   * 256KB message cap (envelope overhead included). Returns false if any
   * frame failed to deliver. */
  private sendUpdates(ws: WebSocket, crdt: CrdtType, roomId: string, updates: Uint8Array[]): boolean {
    let ok = true;
    let batch: Uint8Array[] = [];
    let batchBytes = 0;
    const flushBatch = () => {
      if (batch.length === 0) return;
      ok =
        this.send(ws, {
          type: MessageType.DocUpdate,
          crdt,
          roomId,
          updates: batch,
          batchId: this.newBatchId()
        }) && ok;
      batch = [];
      batchBytes = 0;
    };
    for (const u of updates) {
      if (u.length > FRAGMENT_BYTES) continue;
      if (batchBytes + u.length > FRAGMENT_BYTES) flushBatch();
      batch.push(u);
      batchBytes += u.length;
    }
    flushBatch();
    for (const update of updates) {
      if (update.length <= FRAGMENT_BYTES) continue;
      const batchId = this.newBatchId();
      const fragmentCount = Math.ceil(update.length / FRAGMENT_BYTES);
      ok =
        this.send(ws, {
          type: MessageType.DocUpdateFragmentHeader,
          crdt,
          roomId,
          batchId,
          fragmentCount,
          totalSizeBytes: update.length
        }) && ok;
      for (let i = 0; i < fragmentCount; i++) {
        ok =
          this.send(ws, {
            type: MessageType.DocUpdateFragment,
            crdt,
            roomId,
            batchId,
            index: i,
            fragment: update.subarray(
              i * FRAGMENT_BYTES,
              Math.min((i + 1) * FRAGMENT_BYTES, update.length)
            )
          }) && ok;
      }
    }
    return ok;
  }

  /** Relay accepted updates to every other member socket via sendUpdates —
   * NOT a single pre-encoded frame. broadcast() used to encode the batch
   * once, so a reassembled >256KB client push (a device re-uploading its
   * full workspace history after a server reset) blew the loro-protocol
   * message cap and NEVER reached peers live; they only converged via a
   * later rejoin backfill (2026-08-04, the last silent-staleness path). */
  private relay(from: WebSocket, crdt: CrdtType, roomId: string, updates: Uint8Array[]): void {
    for (const ws of this.ctx.getWebSockets()) {
      if (ws === from) continue;
      const state = ws.deserializeAttachment() as SocketState | null;
      if (!state?.rooms.includes(crdt)) continue;
      if (!this.sendUpdates(ws, crdt, roomId, updates)) {
        // A member socket we cannot send to is a DEAF PEER, not a skippable
        // one: swallowing the failure left it looking alive (runtime
        // auto-pongs, accepted writes) while it silently missed every
        // broadcast until an app restart (2026-08-04 incident). Close it so
        // the client's session ends and its redial + VV backfill heal the
        // gap within seconds.
        console.warn(
          "relay send failed; closing socket",
          `room=${this.getMeta("chatId") ?? "?"}`,
          `device=${state.deviceId ?? "unattributed"}`
        );
        try {
          ws.close(1011, "broadcast delivery failed");
        } catch {
          /* already gone */
        }
      }
    }
  }

  private ack(
    ws: WebSocket,
    message: { crdt: CrdtType; roomId: string; batchId?: `0x${string}` },
    status: UpdateStatusCode,
    refId?: `0x${string}`
  ): void {
    this.send(ws, {
      type: MessageType.Ack,
      crdt: message.crdt,
      roomId: message.roomId,
      refId: refId ?? message.batchId ?? "0x0000000000000000",
      status
    });
  }

  private newBatchId(): `0x${string}` {
    const bytes = new Uint8Array(8);
    crypto.getRandomValues(bytes);
    return bytesToHex(bytes);
  }
}

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });
