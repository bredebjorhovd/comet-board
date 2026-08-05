/**
 * SessionRoom — one Durable Object per doc room, speaking loro-protocol over
 * hibernatable WebSockets (design §2, §3.1). Two doc kinds share this class:
 * chat session docs (room name = chatId, claim-on-first-join ownership, plus
 * the org-shared opt-in below) and org-wide docs (the per-user workspace doc
 * `ws3/{orgId}/{userId}` and the org device registry `orgdev1/{orgId}` —
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
 *   ops is healed by normal CRDT resync from the host on reconnect).
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
 * (checkpoints, history trim, R2 backup §3.3) rides the durable alarm.
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
import { LoroDoc, EphemeralStore, VersionVector } from "loro-crdt";
import {
  CrdtType,
  JoinErrorCode,
  MAX_MESSAGE_SIZE,
  MessageType,
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
import { createBlobStore, getJsonBlob, putJsonBlob, type BlobStore } from "./blobs";
import { AUTH_ORG_HEADER, AUTH_USER_HEADER, ROOM_KIND_HEADER, type Env } from "./env";

const DAY_MS = 24 * 60 * 60 * 1000;
const RETAIN_MS = RETAIN_DAYS * DAY_MS;
/** Consecutive cold-replay deaths (CPU-limit kills mid-`ensureDoc`) before the
 * room concludes it is wedged and drops its own log — see `ensureDoc`. */
const REPLAY_CRASH_LIMIT = 3;
/** Payload bytes per outbound fragment (leaves room for the envelope). */
const FRAGMENT_BYTES = 200_000;
/** Keep a rolling ~5 weeks of daily frontier checkpoints. */
const MAX_CHECKPOINTS = 36;
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
/** A wasm-bindgen wrapper whose `free()` already ran has `__wbg_ptr === 0`;
 * any method call on it throws `Error("null pointer passed to rust")`. Several
 * flows (trim, fold, alarm, idle release) free-and-replace the cached doc
 * around `await`s, so a stale wrapper can outlive its wasm memory. 2026-08-04
 * evening: one such interleaving left `this.doc` dangling in a live instance —
 * every join/update/tail on the ws3 workspace room threw for 2.5h fleet-wide,
 * and nothing recycled the instance because the error is neither a RangeError
 * nor a RuntimeError (the wasm-poison tripwire ignored it). Check liveness
 * before every reuse; rematerialization is a ~tens-of-ms cold replay. */
const isLive = (obj: unknown): boolean =>
  (obj as { __wbg_ptr?: number } | undefined)?.__wbg_ptr !== 0;
/** The use-after-free / detached-buffer signatures of a dangling wasm wrapper
 * — same terminal shape as heap poisoning (nothing in-instance recovers it),
 * so the tripwire must count these too. */
const isWasmUseAfterFree = (e: unknown): boolean =>
  e instanceof Error && /null pointer passed to rust|detached ArrayBuffer/i.test(e.message);

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
  parts: Uint8Array[];
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
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS updates (seq INTEGER PRIMARY KEY AUTOINCREMENT, bytes BLOB NOT NULL, received_at INTEGER NOT NULL)"
    );
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)"
    );
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
    const rows = [...this.ctx.storage.sql.exec("SELECT value FROM meta WHERE key = ?", key)];
    return rows[0]?.value as string | undefined;
  }

  private setMeta(key: string, value: string): void {
    this.ctx.storage.sql.exec(
      "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
      key,
      value
    );
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
      const pair = new WebSocketPair();
      this.ctx.acceptWebSocket(pair[1]);
      const state: SocketState = {
        userId,
        rooms: [],
        joinedAt: Date.now(),
        ...(orgId ? { orgId } : {}),
        ...(workspace ? { workspace } : {}),
        ...(deviceId ? { deviceId } : {})
      };
      pair[1].serializeAttachment(state);
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
        // Non-zero while a cold replay is in flight or has been dying — the
        // wedge signature ensureDoc's automated reset watches for.
        replayAttempts: Number(this.getMeta("replayAttempts") ?? "0"),
        // Per-device %LOR attribution (in-memory: since this instance woke).
        importPenalty: [...this.importPenalty].map(([device, e]) => ({ device, ...e })),
        pushOutcomes: [...this.pushOutcomes].map(([device, e]) => ({ device, ...e }))
      });
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
        this.escalateWasmPoisoning(e);
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
        this.escalateWasmPoisoning(e);
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
      this.recordLoroUpdates([body]);
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
      if (access !== "owner") return refuse();
      const before = [...this.ctx.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n as
        | number
        | undefined;
      this.dropLog();
      this.doc?.free(); // release the wasm memory, don't wait on GC finalizers
      this.doc = undefined; // force a fresh (empty) materialization next join
      // Boot any currently-attached %LOR/%EPH sockets so their hung/half-cold
      // sessions bail and reconnect into the now-empty doc.
      for (const sock of this.ctx.getWebSockets()) {
        try {
          sock.close(4410, "room reset");
        } catch {
          /* already gone */
        }
      }
      return json({ ok: true, clearedUpdateRows: before ?? 0 });
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
    } catch (e) {
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
      this.escalateWasmPoisoning(e);
    }
  }

  async webSocketClose(ws: WebSocket): Promise<void> {
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
      this.escalateWasmPoisoning(e);
    }
  }

  async webSocketError(ws: WebSocket): Promise<void> {
    this.fragments.delete(ws);
    this.publishPresence(ws);
    try {
      await this.flush();
    } catch (e) {
      console.error("flush on socket error failed", `room=${this.getMeta("chatId") ?? "?"}`, String(e));
      this.escalateWasmPoisoning(e);
    }
  }

  /** RangeError("Invalid array buffer length") / wasm RuntimeError are the
   * signature of an exhausted loro-wasm heap (see `wasmPoisonStrikes`).
   * Strike out and `ctx.abort()` so clients redial into a fresh isolate
   * within seconds instead of hot-looping against a deaf room until
   * Cloudflare's memory-limit reset finally fires. */
  private escalateWasmPoisoning(e: unknown): void {
    if (!(e instanceof RangeError || e instanceof WebAssembly.RuntimeError || isWasmUseAfterFree(e))) {
      return;
    }
    wasmPoisonStrikes++;
    if (wasmPoisonStrikes < WASM_POISON_ABORT_AFTER) return;
    console.error(`wasm heap poisoned (${wasmPoisonStrikes} strikes); aborting isolate for a fresh heap`);
    // Best effort: if abort recycles only the DO instance (not the whole
    // isolate), at least this room's doc goes back to the wasm allocator.
    try {
      this.doc?.free();
    } catch {
      /* already poisoned beyond freeing */
    }
    this.doc = undefined;
    this.ctx.abort("loro-wasm heap exhausted; recycling isolate");
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
        vv.free();
      }
      let backfill: Uint8Array;
      if (message.version.length > 0) {
        let from: VersionVector | undefined;
        try {
          from = VersionVector.decode(message.version);
          backfill = doc.export({ mode: "update", from });
        } catch {
          // Unknown/garbled client version — fall back to a full snapshot.
          backfill = doc.export({ mode: "snapshot" });
        } finally {
          from?.free();
        }
      } else {
        backfill = doc.export({ mode: "snapshot" });
      }
      if (backfill.length > 0) {
        this.sendUpdates(ws, message.crdt, message.roomId, [backfill]);
      }
      // A doc-only joiner still carries a device id, so its arrival changes the
      // answer for everyone already watching presence.
      this.publishPresence();
      // A full join answer just exported cleanly — the wasm heap is healthy.
      // Without this reset, occasional transient RangeErrors accumulated
      // over an isolate's lifetime and the tripwire aborted HEALTHY
      // isolates, each abort causing a reconnect herd that produced more
      // transient errors (observed 2026-08-04: ~1 abort/min with all rooms
      // already trimmed small). Poisoning is CONSECUTIVE failures.
      wasmPoisonStrikes = 0;
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
        if (e instanceof RangeError || e instanceof WebAssembly.RuntimeError || isWasmUseAfterFree(e)) {
          throw e;
        }
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
          this.recordLoroUpdates(imported);
          this.relay(ws, crdt, roomId, imported);
        }
        this.ack(ws, { crdt, roomId }, UpdateStatusCode.InvalidUpdate, batchId);
        return;
      }
      if (state.deviceId) this.importPenalty.delete(state.deviceId);
      this.notePush(state.deviceId, true, now);
      this.recordLoroUpdates(updates);
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
  private recordLoroUpdates(updates: Uint8Array[]): void {
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
    this.setMeta("tailDirty", "1");
    this.setMeta("backupDirty", "1");
    // Real state landed — the backup may advance past a wedge-break drop
    // (the monotonic VV gate in alarm() still has the final say).
    this.setMeta("postReset", "0");
    this.scheduleFlush();
    this.markActivity();
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
      parts: Array.from({ length: message.fragmentCount }, () => new Uint8Array()),
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
    batch.parts[message.index] = message.fragment;
    batch.received++;
    if (batch.received < batch.parts.length) return;
    this.fragments.get(ws)?.delete(message.batchId);
    const total = new Uint8Array(batch.totalSize);
    let off = 0;
    for (const part of batch.parts) {
      total.set(part, off);
      off += part.length;
    }
    await this.applyUpdates(ws, state, message.crdt, message.roomId, message.batchId, [total]);
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
    const started = Date.now();
    const doc = new LoroDoc();
    const snapshot = this.blobs.get("snapshot");
    if (snapshot && snapshot.length > 0) doc.import(snapshot);
    let rows = 0;
    for (const row of this.ctx.storage.sql.exec("SELECT bytes FROM updates ORDER BY seq")) {
      rows++;
      try {
        doc.import(new Uint8Array(row.bytes as ArrayBuffer));
      } catch {
        // A poisoned row cannot be applied; skip it rather than brick the room.
      }
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
    this.doc.free();
    this.doc = undefined;
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
      void this.flush();
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
      this.ctx.storage.sql.exec(
        "INSERT INTO updates (bytes, received_at) VALUES (?, ?)",
        update.buffer.slice(update.byteOffset, update.byteOffset + update.byteLength),
        now
      );
    }
    const logBytes = Number(this.getMeta("updateBytes") ?? "0") + this.pendingBytes;
    this.setMeta("updateBytes", String(logBytes));
    this.pending = [];
    this.pendingBytes = 0;
    // Fold on EITHER budget: bytes bounds one huge update, rows bounds many
    // tiny ones — a cold `ensureDoc` replay pays per-import overhead per row,
    // so a high row count is as expensive as a high byte count (see
    // COMPACT_LOG_ROWS). COUNT(*) is a cheap indexed read, once per flush.
    if (logBytes > COMPACT_LOG_BYTES) {
      await this.foldLog();
      return;
    }
    // Redundancy budget: a re-seeded room (wedge break → every device re-pushes
    // its whole doc) sits far under COMPACT_LOG_BYTES yet replays seconds of
    // near-identical full-doc bundles on every cold start (2026-08-05: 1.1MB
    // log, ZERO snapshot, 5.1s replays, heap-pressure aborts, reconnect herds).
    // Fold whenever the log dwarfs the folded state it would collapse into.
    const snapshotBytes = this.blobs.get("snapshot")?.length ?? 0;
    if (logBytes > 4 * Math.max(snapshotBytes, 32 * 1024)) {
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
   * lossless full snapshot when no trim is due (or the trim export fails). */
  private async foldLog(): Promise<void> {
    const doc = await this.ensureDoc();
    if (await this.trimHistoryIfDue(doc, Date.now())) return;
    // Re-resolve after the await above: a concurrent trim may have replaced
    // and FREED the wrapper captured in `doc` (guarded ensureDoc returns the
    // live cached doc, or cheaply rematerializes).
    const live = await this.ensureDoc();
    this.blobs.put("snapshot", live.export({ mode: "snapshot" }));
    this.ctx.storage.sql.exec("DELETE FROM updates");
    this.setMeta("updateBytes", "0");
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
      // Any workspace-room generation (ws3/, ws4/, …) — matching the literal
      // "ws3/" silently dropped this protection when fb6492c bumped the room
      // name to ws4: the ws4 room force-trimmed at the LIVE frontier on
      // 2026-08-05 02:37Z (and likely 00:55Z), stranding in-flight peers —
      // the exact incident b019439 added this guard for. Chat rooms are bare
      // UUIDs (hex — never a "ws" prefix), so the pattern cannot collide.
      if (/^ws\d+\//.test(this.getMeta("chatId") ?? "")) {
        // WORKSPACE rooms: never force-trim at the LIVE frontier. Every
        // device writes this doc concurrently, so a live-frontier shallow
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
      // to the allocator.
      if (old && old !== fresh && isLive(old)) old.free();
      if (doc !== fresh && doc !== old && isLive(doc)) doc.free();
      return true;
    } catch {
      return false;
    }
  }

  /** Daily alarm: frontier checkpoint, history trim, R2 backup. */
  async alarm(): Promise<void> {
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
    // stays set and the alarm chain keeps trying.
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
          prev?.free();
          cur?.free();
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
          vv.free();
        }
        await this.env.BLOBS.put(`backup/${chatId}/latest.loro`, snapshot);
        this.setMeta("backupVV", vvB64);
        this.setMeta("backupDirty", "0");
      }
    }
    // Re-arm only while there is a reason to wake again; markActivity re-arms
    // on the next write otherwise.
  }

  /** Arm the daily alarm if none is scheduled (called on every write). */
  private markActivity(): void {
    void this.ctx.storage.getAlarm().then((existing) => {
      if (existing === null) void this.ctx.storage.setAlarm(Date.now() + DAY_MS);
    });
  }

  private async currentTail(): Promise<unknown> {
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
