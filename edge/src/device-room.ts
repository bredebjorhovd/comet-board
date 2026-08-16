/**
 * DeviceRoom — one Durable Object per device (design §2, §8): a frame relay
 * for interactive RPC + terminal streams + future HTTP tunnel. The host keeps
 * one outbound wss; clients multiplex over `{streamId, kind, bytes}` frames —
 * a generic byte pipe from day one, transport-agnostic by construction (§8.4:
 * a WebRTC fast path could slot in under the same frames).
 *
 * Frame encoding (binary): uleb128 header-length ‖ UTF-8 JSON header ‖ payload.
 * Header: { s: streamId, k: kind, to?: connId, from?: connId, u?: userId, o?: orgId }.
 * - client → DO: DO stamps `from = connId` plus the verified `u`/`o` and forwards
 *   to the host socket (see `relayedHeader` — gh#161).
 * - host → DO: must carry `to = connId`; DO strips routing keys and delivers.
 *
 * Also holds small "sidecar" JSON slots the host publishes (repos/branches
 * snapshot for instant new-chat pickers §8.1; capability metadata) so pickers
 * render last-known state while the live RPC happens at confirm time.
 */
import { BytesReader, BytesWriter } from "loro-protocol";
import { createBlobStore, getJsonBlob, putJsonBlob, type BlobStore } from "./blobs";
import { AUTH_ORG_HEADER, AUTH_USER_HEADER, type Env } from "./env";
import { createMetaStore, type MetaStore } from "./meta";

export interface DeviceFrameHeader {
  /** Stream id, unique per (connId, logical stream). */
  s: string;
  /** Stream kind: "rpc" | "term" | ... — opaque to the relay. */
  k: string;
  /** Routing: host→client target. */
  to?: string;
  /** Routing: client→host origin (stamped by the relay). */
  from?: string;
  /** The WorkOS user id the Worker verified for the sending socket, stamped by
   * the relay the way `from` is (gh#161). Written HERE, from the socket's own
   * accept-time identity — never copied off the frame a client sent, which is
   * the whole of its value: the host can compare it against its own records,
   * and a frontend willing to lie about who is signed in cannot reach it. */
  u?: string;
  /** The verified org claim that rode with `u`, when the caller's session
   * carried one. Same provenance, same rule. */
  o?: string;
}

export const encodeDeviceFrame = (header: DeviceFrameHeader, payload: Uint8Array): Uint8Array => {
  const writer = new BytesWriter();
  writer.pushVarString(JSON.stringify(header));
  writer.pushBytes(payload);
  return writer.finalize();
};

export const decodeDeviceFrame = (
  bytes: Uint8Array
): { header: DeviceFrameHeader; payload: Uint8Array } => {
  const reader = new BytesReader(bytes);
  const header = JSON.parse(reader.readVarString()) as unknown;
  if (!isDeviceFrameHeader(header)) {
    throw new TypeError("device frame header is not a valid object");
  }
  const payload = reader.readBytes(reader.remaining);
  return { header, payload };
};

const isDeviceFrameHeader = (value: unknown): value is DeviceFrameHeader => {
  if (!isRecord(value) || typeof value.s !== "string" || typeof value.k !== "string") {
    return false;
  }
  return [value.to, value.from, value.u, value.o].every(
    (field) => field === undefined || typeof field === "string"
  );
};

interface SocketState {
  userId: string;
  /** The org claim the Worker verified at accept time, when there was one. */
  orgId?: string;
  role: "host" | "client";
  connId: string;
  /** Accept time — the liveness floor until the socket's first auto-pong. */
  joinedAt?: number;
}

interface SocketIdentity {
  role: "host" | "client";
  connId?: string;
}

const isRecord = (value: unknown): value is Record<string, unknown> =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const isSocketState = (value: unknown): value is SocketState =>
  isRecord(value) &&
  typeof value.userId === "string" &&
  value.userId.length > 0 &&
  (value.orgId === undefined || typeof value.orgId === "string") &&
  (value.role === "host" || value.role === "client") &&
  typeof value.connId === "string" &&
  (value.joinedAt === undefined ||
    (typeof value.joinedAt === "number" && Number.isFinite(value.joinedAt)));

/** The header the relay puts on a client→host frame: the client's own `s`/`k`,
 * and everything else built here (gh#161).
 *
 * Pure, and taking the frame apart key by key rather than spreading it, because
 * that is the security property in one line: a client controls which stream and
 * which kind, and nothing else. `from` was already stamped this way; `u`/`o`
 * join it from the socket's accept-time identity, which the Worker verified
 * before the DO ever saw the request (`AUTH_USER_HEADER`). A client that puts
 * its own `u` in a frame is writing a key that is dropped on the floor. */
export const relayedHeader = (
  frame: DeviceFrameHeader,
  conn: { connId: string; userId: string; orgId?: string }
): DeviceFrameHeader => {
  const header: DeviceFrameHeader = { s: frame.s, k: frame.k, from: conn.connId };
  if (conn.userId) header.u = conn.userId;
  if (conn.orgId) header.o = conn.orgId;
  return header;
};

/** What a caller may do with a device room (gh#66).
 * - `claim`  — nobody owns it yet and the device's own backend is joining;
 * - `owner`  — the claiming user, from any of their devices;
 * - `member` — a different user of the SAME org: relay access, no writes;
 * - `deny`   — everyone else. */
export type DeviceRoomAccess = "claim" | "owner" | "member" | "deny";

/** The relay's authorization rule, pure so it is testable without a DO.
 *
 * Before gh#66 this was claim-on-first-join by user id and nothing else, which
 * made "one box, many users" impossible: a teammate signing in on their own
 * laptop was refused by the edge before a frame ever reached the box. The room
 * now also records the org that claimed it, and any member of that org may use
 * the relay — which is precisely the guarantee `crates/engine/src/rpc.rs`
 * asserts when it forwards board RPCs ("the relay only carries frames between
 * devices of one org").
 *
 * Hosting stays owner-only: the host socket IS the device, and only the
 * identity the device runs under may be it. `org` is unset on rooms claimed
 * before this shipped (and by callers whose session carries no org claim);
 * those keep the old owner-only behavior until the owner's next request
 * backfills it. */
export const deviceRoomAccess = (
  room: { owner?: string; org?: string },
  caller: { userId: string; orgId?: string },
  role: "host" | "client"
): DeviceRoomAccess => {
  if (!room.owner) return role === "host" ? "claim" : "deny";
  if (room.owner === caller.userId) return "owner";
  if (role === "host") return "deny";
  if (room.org && caller.orgId && room.org === caller.orgId) return "member";
  return "deny";
};

const HOST_TAG = "host";
const clientTag = (connId: string) => `client:${connId}`;

/** How long a host socket may go without proving liveness before the relay
 * stops routing to it.
 *
 * A host whose network dies silently (laptop lid, NAT/proxy reaping an idle
 * flow) leaves a socket the runtime still reports as connected: no close
 * event ever fires, so `getWebSockets(HOST_TAG)` keeps returning it and the
 * supersede-on-join `close()` never completes either. Picking `[0]` from that
 * list therefore pinned the room to the OLDEST such corpse — every client
 * frame vanished into it while the live host sat later in the list, and
 * clients hung to their own timeouts because a non-empty host list also
 * suppressed the `host_offline` bounce.
 *
 * Hosts ping every 15s (crates/rpc/src/device_room.rs PING_INTERVAL) and the
 * DO's auto-response stamps a timestamp without waking us, so liveness is free
 * to read. The window is sized for the 30s of older builds still in the fleet
 * — 2.5 of their intervals — so upgrading engines is never a prerequisite. */
const HOST_LIVENESS_MS = 75_000;

/** Control frames the relay itself emits (kind " relay"). */
// MUST byte-match packages/rpc device-frames.ts RELAY_KIND — clients compare
// with ===; a mismatch makes host_offline/host_closed invisible to them.
const RELAY_KIND = " relay";

/** Nudge frames (§7 cold-chat command delivery): payload `{chatId}` tells the
 * host "this chat's doc has pending commands — open it and drain". Durable:
 * queued in the DO while the host is offline, replayed on its next join, so a
 * command sent to a chat the host hasn't warm-opened is never stranded. */
export const NUDGE_KIND = "nudge";
const NUDGE_MAX_PENDING = 256;
const CHAT_ID_RE = /^[A-Za-z0-9_-]{1,64}$/;

export class DeviceRoom implements DurableObject {
  private readonly ctx: DurableObjectState;
  private readonly blobs: BlobStore;
  private readonly meta: MetaStore;
  /** Error is commonly followed by close for the same socket. Both callbacks
   * are allowed to arrive, but their relay-level teardown is one event. */
  private readonly terminatedSockets = new WeakSet<WebSocket>();

  constructor(ctx: DurableObjectState, env: Env) {
    this.ctx = ctx;
    void env;
    this.meta = createMetaStore(ctx.storage.sql);
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS pending_nudges (chat_id TEXT PRIMARY KEY, queued_at INTEGER NOT NULL)"
    );
    this.blobs = createBlobStore(ctx.storage.sql);
    ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair("ping", "pong"));
  }

  private getMeta(key: string): string | undefined {
    return this.meta.get(key);
  }

  /** Storing a value that is already stored is not a write — see meta.ts. */
  private setMeta(key: string, value: string): void {
    this.meta.set(key, value);
  }

  /** The host socket to route to: the freshest one that has proven itself
   * alive within [`HOST_LIVENESS_MS`]. `undefined` = no live host, which is
   * what makes clients see `host_offline` instead of hanging on a corpse.
   *
   * `exclude` drops the socket a close is being handled for — the runtime
   * still lists it during `webSocketClose`, and counting it as live would
   * suppress the very `host_closed` that close is supposed to announce. */
  private liveHost(exclude?: WebSocket): WebSocket | undefined {
    const hosts: Array<{ ws: WebSocket; lastSeenAt: number }> = [];
    for (const ws of this.ctx.getWebSockets(HOST_TAG)) {
      // In close/error callbacks workerd still lists the departing socket. Do
      // not ask runtime APIs about it merely to discard it in pickLiveHost.
      if (ws === exclude) continue;
      const state = this.socketState(ws, "live_host");
      if (!state || state.role !== "host") continue;
      let autoResponseAt = 0;
      try {
        autoResponseAt = this.ctx.getWebSocketAutoResponseTimestamp(ws)?.getTime() ?? 0;
      } catch (error) {
        this.logRejectedEvent("live_host", "auto_response_timestamp_failed", error);
      }
      // Auto-pongs are stamped even while hibernating; `joinedAt` covers the
      // window before a fresh socket's first ping. Sockets attached by an
      // older deploy have neither and read as ancient — correct: they are.
      hosts.push({ ws, lastSeenAt: Math.max(autoResponseAt, state.joinedAt ?? 0) });
    }
    return pickLiveHost(hosts, Date.now());
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const userId = request.headers.get(AUTH_USER_HEADER);
    if (!userId) return new Response("unauthenticated", { status: 401 });
    const orgId = request.headers.get(AUTH_ORG_HEADER) ?? undefined;
    const owner = this.getMeta("owner");
    const caller = { userId, orgId };
    // Backfill the room's org from the owner's own requests: rooms claimed
    // before gh#66 have an owner and no org, and would stay invisible to the
    // rest of the org forever. The owner is the only identity trusted to say
    // which org their device belongs to, and re-stamping keeps a device that
    // moved orgs from being reachable by the old one.
    if (owner === userId && orgId && this.getMeta("org") !== orgId) {
      this.setMeta("org", orgId);
    }
    const room = { owner, org: this.getMeta("org") };

    if (url.pathname === "/ws") {
      const role = url.searchParams.get("role") === "host" ? "host" : "client";
      const access = deviceRoomAccess(room, caller, role);
      if (access === "deny") return new Response("forbidden", { status: 403 });
      if (access === "claim") {
        // The device's own backend claims the room; the claim is the identity
        // anchor every later join is checked against, and its org is what
        // opens the relay to the rest of the team.
        this.setMeta("owner", userId);
        if (orgId) this.setMeta("org", orgId);
      }
      const connId = url.searchParams.get("connId") ?? crypto.randomUUID();
      const pair = new WebSocketPair();
      if (role === "host") {
        // One live host socket: close any predecessor (backend restart).
        for (const stale of this.ctx.getWebSockets(HOST_TAG)) {
          try {
            stale.close(4409, "superseded by new host connection");
          } catch {
            /* already gone */
          }
        }
        this.ctx.acceptWebSocket(pair[1], [HOST_TAG]);
        this.replayNudges(pair[1]);
      } else {
        this.ctx.acceptWebSocket(pair[1], [clientTag(connId)]);
      }
      const state: SocketState = { userId, orgId, role, connId, joinedAt: Date.now() };
      pair[1].serializeAttachment(state);
      return new Response(null, { status: 101, webSocket: pair[0] });
    }

    // Everything below is a client-side use of the room (reads and nudges),
    // so it is gated as a client join would be: the owner, or a teammate in
    // the room's org. An unclaimed room denies everyone here (only a host
    // claims) and keeps answering `not_found` — clients branch on that.
    const access = deviceRoomAccess(room, caller, "client");
    const refuse = () => json({ error: "forbidden" }, owner ? 403 : 404);

    // Sidecar slots (host-published JSON, e.g. repos snapshot §8.1).
    const sidecar = url.pathname.match(/^\/sidecar\/([a-z0-9-]{1,64})$/);
    if (sidecar) {
      const name = sidecar[1]!;
      if (access === "deny") return refuse();
      if (request.method === "GET") {
        const value = getJsonBlob<unknown>(this.blobs, `sidecar:${name}`);
        return value === undefined ? json({ error: "not_found" }, 404) : json(value);
      }
      if (request.method === "POST") {
        // Publishing is the host's job — a teammate reads these slots, never
        // writes them.
        if (access !== "owner") return json({ error: "forbidden" }, 403);
        putJsonBlob(this.blobs, `sidecar:${name}`, await request.json());
        return json({ ok: true });
      }
    }

    if (url.pathname === "/status" && request.method === "GET") {
      if (access === "deny") return refuse();
      // `hostSockets` counts corpses too — the gap between it and
      // `hostConnected` is the only externally visible signal that a device's
      // room is accumulating silently-dead host sockets.
      return json({
        hostConnected: this.liveHost() !== undefined,
        hostSockets: this.ctx.getWebSockets(HOST_TAG).length
      });
    }

    // Durable command nudge (§7). Any authenticated device of the org may
    // nudge; the payload is only a chat id — the host validates against its
    // own doc before executing anything (and a teammate steering a chat the
    // box hosts needs exactly this to wake a cold host).
    if (url.pathname === "/nudge" && request.method === "POST") {
      if (access === "deny") return refuse();
      const body = (await request.json().catch(() => null)) as { chatId?: string } | null;
      const chatId = body?.chatId;
      if (!chatId || !CHAT_ID_RE.test(chatId)) return json({ error: "bad_chat_id" }, 400);
      const host = this.liveHost();
      if (host) {
        this.deliver(host, { s: chatId, k: NUDGE_KIND }, new TextEncoder().encode(JSON.stringify({ chatId })));
        return json({ delivered: true });
      }
      // Host offline: queue durably (dedup by chat — one open covers any
      // number of pending commands), bounded so a runaway sender can't grow
      // the DO forever. Overflow drops the OLDEST: recency wins.
      this.ctx.storage.sql.exec(
        "INSERT INTO pending_nudges (chat_id, queued_at) VALUES (?, ?) ON CONFLICT(chat_id) DO UPDATE SET queued_at = excluded.queued_at",
        chatId,
        Date.now()
      );
      this.ctx.storage.sql.exec(
        "DELETE FROM pending_nudges WHERE chat_id NOT IN (SELECT chat_id FROM pending_nudges ORDER BY queued_at DESC LIMIT ?)",
        NUDGE_MAX_PENDING
      );
      return json({ delivered: false, queued: true });
    }

    return new Response("not found", { status: 404 });
  }

  private replayNudges(host: WebSocket): void {
    const rows = [
      ...this.ctx.storage.sql.exec("SELECT chat_id FROM pending_nudges ORDER BY queued_at ASC")
    ] as Array<{ chat_id: string }>;
    if (rows.length === 0) return;
    for (const row of rows) {
      this.deliver(
        host,
        { s: row.chat_id, k: NUDGE_KIND },
        new TextEncoder().encode(JSON.stringify({ chatId: row.chat_id }))
      );
    }
    this.ctx.storage.sql.exec("DELETE FROM pending_nudges");
  }

  webSocketMessage(ws: WebSocket, message: ArrayBuffer | string): void {
    if (typeof message === "string") return; // ping/pong auto-response
    const state = this.socketState(ws, "message");
    if (!state) {
      this.closeSocket(ws, 1011, "Socket state error", "message");
      return;
    }
    let frame: { header: DeviceFrameHeader; payload: Uint8Array };
    try {
      frame = decodeDeviceFrame(new Uint8Array(message));
    } catch (error) {
      this.logRejectedEvent("message", "invalid_frame", error);
      this.closeSocket(ws, 1002, "Frame error", "message");
      return;
    }
    if (state.role === "client") {
      const host = this.liveHost();
      if (!host) {
        // Host offline: bounce a relay-level error so the client can surface
        // "device is asleep" instead of hanging.
        this.deliver(ws, { s: frame.header.s, k: RELAY_KIND }, encodeRelayError("host_offline"));
        return;
      }
      this.deliver(host, relayedHeader(frame.header, state), frame.payload);
      return;
    }
    // Host frame: route by `to`.
    const to = frame.header.to;
    if (!to) return;
    const target = this.ctx.getWebSockets(clientTag(to))[0];
    if (!target) {
      this.deliver(ws, { s: frame.header.s, k: RELAY_KIND, to }, encodeRelayError("client_gone"));
      return;
    }
    this.deliver(target, { s: frame.header.s, k: frame.header.k }, frame.payload);
  }

  webSocketClose(ws: WebSocket): void {
    this.handleSocketTermination(ws, "close");
  }

  webSocketError(ws: WebSocket, error?: unknown): void {
    this.logRejectedEvent("error", "socket_error", error);
    this.handleSocketTermination(ws, "error");
  }

  private handleSocketTermination(ws: WebSocket, event: "close" | "error"): void {
    if (this.terminatedSockets.has(ws)) return;
    this.terminatedSockets.add(ws);

    const identity = this.socketIdentity(ws, event);
    const state = this.socketState(ws, event, identity);
    const role = state?.role ?? identity?.role;
    if (role === "client") {
      // Tell the host so it can tear down any per-client streams (ptys etc.).
      const host = this.liveHost();
      if (host) {
        const connId = state?.connId ?? identity?.connId;
        if (connId) {
          this.deliver(host, { s: "", k: RELAY_KIND, from: connId }, encodeRelayError("client_closed"));
        }
      }
      return;
    }
    if (role !== "host") return;
    // A host socket went away. Only tear the clients' links down when NO live
    // host is left: a superseded predecessor closing (or a corpse the runtime
    // finally reaps) must not knock clients off the successor that already
    // replaced it.
    if (this.liveHost(ws)) return;
    // Host went away: notify every client.
    for (const client of this.ctx.getWebSockets()) {
      if (client === ws) continue;
      const cs = this.socketState(client, event);
      if (cs?.role !== "client") continue;
      this.deliver(client, { s: "", k: RELAY_KIND }, encodeRelayError("host_closed"));
    }
  }

  private socketIdentity(
    ws: WebSocket,
    event: "message" | "close" | "error" | "live_host"
  ): SocketIdentity | undefined {
    let tags: string[];
    try {
      tags = this.ctx.getTags(ws);
    } catch (error) {
      this.logRejectedEvent(event, "socket_tags_unavailable", error);
      return undefined;
    }
    if (tags.includes(HOST_TAG)) return { role: "host" };
    const tag = tags.find((candidate) => candidate.startsWith("client:"));
    if (tag) return { role: "client", connId: tag.slice("client:".length) };
    this.logRejectedEvent(event, "socket_tags_invalid");
    return undefined;
  }

  private socketState(
    ws: WebSocket,
    event: "message" | "close" | "error" | "live_host",
    knownIdentity?: SocketIdentity
  ): SocketState | undefined {
    // Attachments outlive the object instance and may outlive a deployment;
    // the runtime contract is `any | null`, not this revision's interface.
    let value: unknown;
    try {
      value = ws.deserializeAttachment();
    } catch (error) {
      this.logRejectedEvent(event, "attachment_deserialize_failed", error);
      return undefined;
    }
    const identity = knownIdentity ?? this.socketIdentity(ws, event);
    if (
      !isSocketState(value) ||
      !identity ||
      value.role !== identity.role ||
      (value.role === "client" && value.connId !== identity.connId)
    ) {
      this.logRejectedEvent(event, "invalid_socket_state");
      return undefined;
    }
    return value;
  }

  private closeSocket(
    ws: WebSocket,
    code: number,
    reason: string,
    event: "message" | "close" | "error"
  ): void {
    try {
      ws.close(code, reason);
    } catch (error) {
      this.logRejectedEvent(event, "socket_close_failed", error);
    }
  }

  /** Permanent diagnostics for rejected hibernation inputs. Values are fixed
   * classifications only: never socket attachments, ids, or frame contents. */
  private logRejectedEvent(event: string, reason: string, error?: unknown): void {
    const cause = error instanceof Error ? error.name : error === undefined ? "none" : typeof error;
    console.warn(
      "DeviceRoom WebSocket event rejected",
      `event=${event}`,
      `reason=${reason}`,
      `cause=${cause}`
    );
  }

  private deliver(ws: WebSocket, header: DeviceFrameHeader, payload: Uint8Array): void {
    try {
      ws.send(encodeDeviceFrame(header, payload));
    } catch {
      /* stale socket */
    }
  }
}

/** Freshest host socket that has proven itself alive inside the liveness
 * window, or `undefined` when every candidate is stale (or there are none).
 * `exclude` skips a socket whose close is being handled — the runtime still
 * lists it there, and counting it as live would suppress the `host_closed`
 * that close exists to announce. Pure so the routing rule is testable without
 * a DO. */
export const pickLiveHost = <T>(
  hosts: ReadonlyArray<{ ws: T; lastSeenAt: number }>,
  now: number,
  exclude?: T
): T | undefined => {
  let best: { ws: T; lastSeenAt: number } | undefined;
  for (const host of hosts) {
    if (host.ws === exclude) continue;
    if (!best || host.lastSeenAt > best.lastSeenAt) best = host;
  }
  return best && now - best.lastSeenAt <= HOST_LIVENESS_MS ? best.ws : undefined;
};

const encodeRelayError = (code: string): Uint8Array =>
  new TextEncoder().encode(JSON.stringify({ error: code }));

const json = (value: unknown, status = 200): Response =>
  new Response(JSON.stringify(value), {
    status,
    headers: { "content-type": "application/json" }
  });
