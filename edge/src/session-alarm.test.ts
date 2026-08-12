import { DatabaseSync } from "node:sqlite";
import { LoroDoc } from "loro-crdt";
import { CrdtType, MessageType, encode } from "loro-protocol";
import { describe, expect, it, vi } from "vitest";
import { AUTH_USER_HEADER } from "./env";
import { ALARM_FAILURE_LIMIT, SessionRoom, alarmRetryDelay } from "./session-room";

// gh#378 — the alarm chain is the only unbounded call source in this system.
//
// Every other path into a SessionRoom is client-driven and self-limiting: the
// Rust client caps reconnect backoff at 30s, and gh#373 measured a six-hour
// total outage producing 3,000–6,000 calls/hour from a couple of dozen rooms,
// exactly as designed. The daily alarm was different. It threw, `backupDirty`
// stayed set, the runtime re-ran it on its own backoff, and nothing anywhere
// counted — with zero clients connected to notice, and on a paid plan billing
// requests and duration on every attempt.
//
// It is bounded now, and these are the rules of the bound. The three that
// matter most are the last three: giving up must be READABLE, must not cost a
// room its backup forever, and must not touch a room whose alarm works.

// ── a Durable Object, faked just far enough ────────────────────────────────
// SqlStorage over node:sqlite, so the room's real SQL (meta, the chunked
// update log, the blob store) runs as written rather than against a matcher
// that agrees with whatever the test expects.

class FakeSql {
  private readonly db = new DatabaseSync(":memory:");

  exec(query: string, ...params: unknown[]): Iterable<Record<string, unknown>> {
    const bound = params.map((p) => (p instanceof ArrayBuffer ? new Uint8Array(p) : p));
    const stmt = this.db.prepare(query);
    if (/^\s*SELECT/i.test(query)) {
      return stmt.all(...(bound as never[])) as unknown as Record<string, unknown>[];
    }
    stmt.run(...(bound as never[]));
    return [];
  }
}

class FakeStorage {
  readonly sql = new FakeSql();
  /** When the next alarm fires; null means nothing is scheduled — the whole
   * question this test asks. */
  scheduled: number | null = null;
  syncs = 0;

  async getAlarm(): Promise<number | null> {
    return this.scheduled;
  }
  async setAlarm(at: number): Promise<void> {
    this.scheduled = at;
  }
  async deleteAlarm(): Promise<void> {
    this.scheduled = null;
  }
  async sync(): Promise<void> {
    this.syncs++;
  }
}

class FakeSocket {
  attachment: unknown = null;
  readonly sent: Uint8Array[] = [];
  closed: number | undefined;

  serializeAttachment(value: unknown): void {
    this.attachment = value;
  }
  deserializeAttachment(): unknown {
    return this.attachment;
  }
  send(bytes: Uint8Array): void {
    this.sent.push(bytes);
  }
  close(code?: number): void {
    this.closed = code;
  }
}

class FakeCtx {
  readonly storage = new FakeStorage();
  readonly sockets: FakeSocket[] = [];

  acceptWebSocket(ws: FakeSocket): void {
    this.sockets.push(ws);
  }
  getWebSockets(): FakeSocket[] {
    return this.sockets;
  }
  setWebSocketAutoResponse(): void {}
  getWebSocketAutoResponseTimestamp(): Date | null {
    return null;
  }
  abort(): void {}
}

// The constructor builds one of these for the hibernation ping/pong pair.
(globalThis as { WebSocketRequestResponsePair?: unknown }).WebSocketRequestResponsePair = class {
  constructor(
    readonly request: string,
    readonly response: string
  ) {}
};

const CHAT_ID = "chat_alarm";
const USER = "user_owner";

interface Harness {
  room: SessionRoom;
  ctx: FakeCtx;
  put: ReturnType<typeof vi.fn>;
  /** R2 outage / storage refusal: what the failure budget is spent on. */
  failBackups: (fail: boolean) => void;
  /** One alarm invocation as the runtime delivers it: the schedule is cleared
   * BEFORE the handler runs, so "did it reschedule itself?" is answerable. */
  fire: () => Promise<void>;
  join: () => Promise<FakeSocket>;
  append: (text: string) => Promise<Response>;
  tail: () => Promise<Response>;
  stats: () => Promise<{
    backupDirty: boolean;
    alarm: { consecutiveFailures: number; gaveUp: boolean; gaveUpAt: number | null; limit: number };
  }>;
  /** Let markActivity's getAlarm().then(...) arming land. */
  settle: () => Promise<void>;
}

const harness = (): Harness => {
  const ctx = new FakeCtx();
  let broken = false;
  const put = vi.fn(async () => {
    if (broken) throw new Error("R2 unavailable");
  });
  const room = new SessionRoom(
    ctx as unknown as DurableObjectState,
    { BLOBS: { put } } as unknown as ConstructorParameters<typeof SessionRoom>[1]
  );
  const settle = async (): Promise<void> => {
    await new Promise((resolve) => setTimeout(resolve, 0));
  };
  const authed = (path: string, init?: RequestInit): Request =>
    new Request(`https://room.invalid${path}`, {
      ...init,
      headers: { [AUTH_USER_HEADER]: USER, ...(init?.headers ?? {}) }
    });
  return {
    room,
    ctx,
    put,
    failBackups: (fail) => {
      broken = fail;
    },
    fire: async () => {
      ctx.storage.scheduled = null;
      await room.alarm();
      await settle();
    },
    join: async () => {
      const ws = new FakeSocket();
      ws.serializeAttachment({ userId: USER, rooms: [], joinedAt: Date.now(), deviceId: "box" });
      ctx.sockets.push(ws);
      const frame = encode({
        type: MessageType.JoinRequest,
        crdt: CrdtType.Loro,
        roomId: CHAT_ID,
        auth: new Uint8Array(),
        version: new Uint8Array()
      });
      await room.webSocketMessage(
        ws as unknown as WebSocket,
        frame.buffer.slice(frame.byteOffset, frame.byteOffset + frame.byteLength) as ArrayBuffer
      );
      await settle();
      return ws;
    },
    append: async (text) => {
      const doc = new LoroDoc();
      doc.getText("t").insert(0, text);
      doc.commit();
      const response = await room.fetch(
        authed("/append", { method: "POST", body: doc.export({ mode: "update" }) })
      );
      await settle();
      return response;
    },
    tail: async () => {
      const response = await room.fetch(authed("/tail"));
      await settle();
      return response;
    },
    stats: async () => (await room.fetch(authed("/stats"))).json() as never,
    settle
  };
};

/** A claimed room holding unbacked-up state — the state every case starts in. */
const roomWithWork = async (): Promise<Harness> => {
  const h = harness();
  await h.join();
  expect((await h.append("hello")).status).toBe(200);
  expect(h.ctx.storage.scheduled).not.toBeNull(); // a write arms the daily alarm
  return h;
};

// ── the ladder ────────────────────────────────────────────────────────────

describe("alarm retry ladder", () => {
  it("starts at a minute and doubles", () => {
    expect(alarmRetryDelay(1)).toBe(60_000);
    expect(alarmRetryDelay(2)).toBe(120_000);
    expect(alarmRetryDelay(3)).toBe(240_000);
  });

  it("caps at an hour, so a long outage is a couple of dozen calls, not hundreds", () => {
    expect(alarmRetryDelay(10)).toBe(60 * 60 * 1000);
    expect(alarmRetryDelay(ALARM_FAILURE_LIMIT)).toBe(60 * 60 * 1000);
  });

  // The number N is chosen against a real failure: gh#373's edge outage lasted
  // six hours, and a room must still self-heal after one. Too small a budget
  // trades an unbounded cost for rooms that stop backing up — the worse bug.
  it("spans longer than the six-hour outage it must survive", () => {
    const span = Array.from({ length: ALARM_FAILURE_LIMIT - 1 }, (_, i) =>
      alarmRetryDelay(i + 1)
    ).reduce((a, b) => a + b, 0);
    expect(span).toBeGreaterThan(6 * 60 * 60 * 1000);
    // ...and still ends within a day or so. "Tens, not hundreds."
    expect(span).toBeLessThan(36 * 60 * 60 * 1000);
    expect(ALARM_FAILURE_LIMIT).toBeLessThan(100);
  });
});

// ── the bound ─────────────────────────────────────────────────────────────

describe("bounded alarm chain", () => {
  it("runs the daily backup for a room that has not given up", async () => {
    const h = await roomWithWork();
    await h.fire();

    expect(h.put).toHaveBeenCalledTimes(1);
    expect(h.put.mock.calls[0]?.[0]).toBe(`backup/${CHAT_ID}/latest.loro`);
    const stats = await h.stats();
    expect(stats.backupDirty).toBe(false);
    expect(stats.alarm).toMatchObject({ consecutiveFailures: 0, gaveUp: false, gaveUpAt: null });
  });

  it("stops rescheduling after N consecutive failures", async () => {
    const h = await roomWithWork();
    h.failBackups(true);

    for (let attempt = 1; attempt < ALARM_FAILURE_LIMIT; attempt++) {
      await h.fire();
      // Still inside the budget: the room rearms itself on the ladder.
      expect(h.ctx.storage.scheduled).not.toBeNull();
      expect((await h.stats()).alarm).toMatchObject({
        consecutiveFailures: attempt,
        gaveUp: false
      });
    }

    await h.fire(); // the Nth
    expect(h.ctx.storage.scheduled).toBeNull();
    expect((await h.stats()).alarm).toMatchObject({
      consecutiveFailures: ALARM_FAILURE_LIMIT,
      gaveUp: true
    });
    expect(h.put).toHaveBeenCalledTimes(ALARM_FAILURE_LIMIT);
    // The work is still OWED — giving up is not "this room is backed up".
    expect((await h.stats()).backupDirty).toBe(true);
  });

  // A failure is swallowed rather than rethrown, precisely so the runtime does
  // not layer its own retry on top of the ladder. But an invocation can also
  // die where no catch can see it (a CPU-limit kill), and the runtime retries
  // THAT — which is why the attempt is counted before the work starts, and
  // synced, and why a room past its budget refuses at the door.
  it("counts the attempt before the work, and refuses once the budget is spent", async () => {
    const h = await roomWithWork();
    h.failBackups(true);
    const syncsBefore = h.ctx.storage.syncs;
    await h.fire();
    expect(h.ctx.storage.syncs).toBeGreaterThan(syncsBefore);

    for (let attempt = 2; attempt <= ALARM_FAILURE_LIMIT; attempt++) await h.fire();
    const spent = h.put.mock.calls.length;

    // A runtime-initiated retry after the give-up: no work, no reschedule.
    await expect(h.fire()).resolves.toBeUndefined();
    expect(h.put).toHaveBeenCalledTimes(spent);
    expect(h.ctx.storage.scheduled).toBeNull();
  });

  it("clears the counter on a success after a failure, and stays armed", async () => {
    const h = await roomWithWork();
    h.failBackups(true);
    await h.fire();
    expect((await h.stats()).alarm).toMatchObject({ consecutiveFailures: 1, gaveUp: false });
    const retryAt = h.ctx.storage.scheduled;
    expect(retryAt).not.toBeNull();
    expect(retryAt! - Date.now()).toBeLessThanOrEqual(alarmRetryDelay(1) + 1_000);

    h.failBackups(false);
    await h.fire();
    expect(h.put).toHaveBeenCalledTimes(2);
    const stats = await h.stats();
    expect(stats.alarm).toMatchObject({ consecutiveFailures: 0, gaveUp: false, gaveUpAt: null });
    expect(stats.backupDirty).toBe(false);
    // ...and the chain is still live: the next write arms it again.
    await h.append("more");
    expect(h.ctx.storage.scheduled).not.toBeNull();
  });

  it("says on /stats that a room gave up, and since when", async () => {
    const h = await roomWithWork();
    h.failBackups(true);
    const before = Date.now();
    for (let attempt = 1; attempt <= ALARM_FAILURE_LIMIT; attempt++) await h.fire();
    const after = Date.now();

    const { alarm } = await h.stats();
    expect(alarm.gaveUp).toBe(true);
    expect(alarm.gaveUpAt).toBeGreaterThanOrEqual(before);
    expect(alarm.gaveUpAt).toBeLessThanOrEqual(after);
    expect(alarm.limit).toBe(ALARM_FAILURE_LIMIT);

    // "Since when" means the first give-up, not the latest retry against it.
    const first = alarm.gaveUpAt;
    await h.fire();
    expect((await h.stats()).alarm.gaveUpAt).toBe(first);
  });
});

// ── giving up is not giving up forever ────────────────────────────────────
//
// The wedge-break path this chain completes matters most with NOBODY
// connected. A room that stopped retrying and never resumed would be a room
// that silently stopped backing up, which is worse than the bill.

describe("a room that gave up comes back", () => {
  const givenUp = async (): Promise<Harness> => {
    const h = await roomWithWork();
    h.failBackups(true);
    for (let attempt = 1; attempt <= ALARM_FAILURE_LIMIT; attempt++) await h.fire();
    expect((await h.stats()).alarm.gaveUp).toBe(true);
    expect(h.ctx.storage.scheduled).toBeNull();
    return h;
  };

  it("re-arms when a client joins", async () => {
    const h = await givenUp();
    await h.join();

    expect(h.ctx.storage.scheduled).not.toBeNull();
    expect((await h.stats()).alarm).toMatchObject({
      consecutiveFailures: 0,
      gaveUp: false,
      gaveUpAt: null
    });
  });

  it("backs up again once whatever broke is fixed", async () => {
    const h = await givenUp();
    const spent = h.put.mock.calls.length;
    await h.join();
    h.failBackups(false);
    await h.fire();

    expect(h.put).toHaveBeenCalledTimes(spent + 1);
    expect((await h.stats()).backupDirty).toBe(false);
  });

  // An instant-open reads the L2 tail before any socket exists, so on a room
  // nobody has joined yet it is the earliest evidence anyone is watching.
  it("re-arms on an explicit /tail read", async () => {
    const h = await givenUp();
    expect((await h.tail()).status).toBe(200);

    expect(h.ctx.storage.scheduled).not.toBeNull();
    expect((await h.stats()).alarm.gaveUp).toBe(false);
  });

  it("re-arms on a write", async () => {
    const h = await givenUp();
    await h.append("a client is back");

    expect(h.ctx.storage.scheduled).not.toBeNull();
    expect((await h.stats()).alarm.gaveUp).toBe(false);
  });

  // The deliberate narrowing: reviving is for rooms that have GIVEN UP. A join
  // that reset a mid-ladder counter would let one flapping client restart the
  // budget forever — the unboundedness this issue removes, wearing a client's
  // clothes. The room finishes deciding, and the next join revives it.
  it("does not reset a counter mid-ladder", async () => {
    const h = await roomWithWork();
    h.failBackups(true);
    await h.fire();
    await h.fire();
    await h.join();
    await h.tail();

    expect((await h.stats()).alarm).toMatchObject({ consecutiveFailures: 2, gaveUp: false });
  });

  // Nothing above may cost a healthy room an extra wake: a join on a room with
  // no alarm scheduled and nothing owed must leave it asleep.
  it("leaves a healthy idle room unarmed", async () => {
    const h = await roomWithWork();
    await h.fire(); // backs up, clears backupDirty, schedules nothing
    expect(h.ctx.storage.scheduled).toBeNull();

    await h.join();
    await h.tail();
    expect(h.ctx.storage.scheduled).toBeNull();
  });
});
