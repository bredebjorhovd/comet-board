// vitest 3's bundled types referenced @types/node ambiently; vitest 4 stopped,
// so the node:sqlite module declaration must be pulled in here explicitly.
/// <reference types="node" />
import { DatabaseSync } from "node:sqlite";
import { LoroDoc, VersionVector } from "loro-crdt";
import { CrdtType, JoinErrorCode, MessageType, decode, encode } from "loro-protocol";
import { afterEach, describe, expect, it, vi } from "vitest";
import { AUTH_USER_HEADER, ROOM_KIND_HEADER } from "./env";
import { LOAD_REFUSAL_LIMIT, SessionRoom, isWasmBorrowConflict } from "./session-room";

// gh#607 — the exception one `catch` below every guard we had built.
//
// The `ws4` workspace room evicted every client that joined it, every ~25
// seconds, for five days. gh#527/#553/#554/#557 and PR #602 each caught a
// symptom. The fault itself was this, on a live tail:
//
//   ws message handler failed room=ws4/… type=0
//   Error: attempted to take ownership of Rust value while it was borrowed
//
// That string is wasm-bindgen's `WasmRefCell::take` refusing to release a value
// with an outstanding borrow. Three things made it invisible:
//
//  1. the wrapper's `__wbg_ptr` is still a live pointer, so `isLive()` passes
//     it — the gh#148 dangling-wrapper guard was written for FREED values;
//  2. it is a plain `Error` — not a `RangeError`, not a
//     `WebAssembly.RuntimeError`, and not one of `isWasmUseAfterFree`'s strings
//     — so `escalateWasmPoisoning` returned having counted nothing;
//  3. so it fell to the generic catch and answered `internal error`, which is
//     not a `DocLoadRefused`, so no refusal was counted, so `LOAD_REFUSAL_LIMIT`
//     never tripped and the evict-and-reseed that repairs this never ran.
//
// And `free()` zeroes `__wbg_ptr` and unregisters the finalizer BEFORE calling
// into wasm, so each occurrence abandoned megabytes that neither a later free
// nor GC could ever reclaim — the climb to the isolate's 128MB cap, which reset
// and severed every co-located room's sockets. Every device redialed. Loop.
//
// These are the rules of the third fault class.

// ── a Durable Object, faked just far enough (as in session-alarm.test.ts) ───

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
  scheduled: number | null = null;

  async getAlarm(): Promise<number | null> {
    return this.scheduled;
  }
  async setAlarm(at: number): Promise<void> {
    this.scheduled = at;
  }
  async deleteAlarm(): Promise<void> {
    this.scheduled = null;
  }
  async sync(): Promise<void> {}
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
  readonly aborts: string[] = [];

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
  abort(reason: string): void {
    this.aborts.push(reason);
  }
}

(globalThis as { WebSocketRequestResponsePair?: unknown }).WebSocketRequestResponsePair = class {
  constructor(
    readonly request: string,
    readonly response: string
  ) {}
};

const ROOM = "ws4/org_test/user_test";
const USER = "user_test";

/** The exception, verbatim off the 2026-08-24 22:50 CEST tail. */
const borrowConflict = (): Error =>
  new Error("attempted to take ownership of Rust value while it was borrowed");

interface Stats {
  docLoad: { refusals: number; limit: number };
  borrow: {
    conflicts: number;
    strikes: number;
    limit: number;
    leaked: number;
    isolateLeaked: number;
    lastConflict: unknown;
  };
  updateRows: number;
  snapshotBytes: number;
  wasm: { faults: number };
  lastAbort: unknown;
}

const harness = () => {
  const ctx = new FakeCtx();
  const room = new SessionRoom(
    ctx as unknown as DurableObjectState,
    { BLOBS: { put: vi.fn(async () => {}) } } as unknown as ConstructorParameters<
      typeof SessionRoom
    >[1]
  );
  const authed = (path: string, init?: RequestInit): Request =>
    new Request(`https://room.invalid${path}`, {
      ...init,
      // A workspace room: the Worker enforced org membership and stamps the
      // kind, so every member reads and writes as an owner would. That is the
      // room this ticket is about.
      headers: {
        [AUTH_USER_HEADER]: USER,
        [ROOM_KIND_HEADER]: "workspace",
        ...(init?.headers ?? {})
      }
    });
  const deliver = async (ws: FakeSocket, frame: Uint8Array): Promise<void> => {
    await room.webSocketMessage(
      ws as unknown as WebSocket,
      frame.buffer.slice(frame.byteOffset, frame.byteOffset + frame.byteLength) as ArrayBuffer
    );
  };
  return {
    room,
    ctx,
    /** A workspace-room join, as the engine sends it (empty VV: full snapshot). */
    join: async (socket?: FakeSocket): Promise<FakeSocket> => {
      const ws = socket ?? new FakeSocket();
      if (!socket) {
        ws.serializeAttachment({
          userId: USER,
          rooms: [],
          workspace: true,
          joinedAt: Date.now(),
          deviceId: "mac"
        });
        ctx.sockets.push(ws);
      }
      await deliver(
        ws,
        encode({
          type: MessageType.JoinRequest,
          crdt: CrdtType.Loro,
          roomId: ROOM,
          auth: new Uint8Array(),
          version: new Uint8Array()
        })
      );
      return ws;
    },
    append: async (text: string): Promise<Response> => {
      const doc = new LoroDoc();
      doc.getText("t").insert(0, text);
      doc.commit();
      return room.fetch(
        authed("/append", { method: "POST", body: doc.export({ mode: "update" }) })
      );
    },
    stats: async (): Promise<Stats> =>
      (await room.fetch(authed("/stats"))).json() as Promise<Stats>
  };
};

/** The last frame a socket was sent, decoded. */
const lastFrame = (ws: FakeSocket) => decode(ws.sent[ws.sent.length - 1]!);

afterEach(() => {
  vi.restoreAllMocks();
});

// ── the classifier ─────────────────────────────────────────────────────────

describe("stuck-borrow classification", () => {
  it("recognises the exception that took the ws4 room down", () => {
    expect(isWasmBorrowConflict(borrowConflict())).toBe(true);
  });

  // The `&mut self` half of the same wedge: wasm-bindgen answers a borrow_mut
  // against an outstanding borrow with this instead. Same object, same cure.
  it("recognises the aliasing half of the same fault", () => {
    expect(
      isWasmBorrowConflict(
        new Error("recursive use of an object detected which would lead to unsafe aliasing in rust")
      )
    ).toBe(true);
  });

  // It must stay DISTINCT from heap poisoning, which is cured by aborting the
  // isolate — the loop gh#557 exists to have stopped. A wedged object is one
  // object; the heap around it is fine.
  it("is not the wasm-heap signature, and not use-after-free", () => {
    expect(isWasmBorrowConflict(new RangeError("Invalid array buffer length"))).toBe(false);
    expect(isWasmBorrowConflict(new Error("null pointer passed to rust"))).toBe(false);
    expect(isWasmBorrowConflict(new WebAssembly.RuntimeError("unreachable"))).toBe(false);
    expect(isWasmBorrowConflict(new Error("internal error"))).toBe(false);
    expect(isWasmBorrowConflict(undefined)).toBe(false);
  });
});

// ── the answer on the wire ─────────────────────────────────────────────────

describe("a join that hits a stuck borrow", () => {
  it("is REFUSED, not answered `internal error`", async () => {
    const h = harness();
    await h.append("state the room holds");
    vi.spyOn(LoroDoc.prototype, "export").mockImplementationOnce(() => {
      throw borrowConflict();
    });

    const ws = await h.join();

    const answer = lastFrame(ws);
    expect(answer.type).toBe(MessageType.JoinError);
    // Still AppError on the wire — the protocol has no code for this, and the
    // client's long-backoff rejoin is the right behaviour either way. What
    // changed is that the room now SAYS what happened instead of "internal
    // error", which is what five days of tails could not distinguish.
    expect((answer as { code: JoinErrorCode }).code).toBe(JoinErrorCode.AppError);
    expect((answer as { message: string }).message).toContain("wasm borrow conflict");
  });

  // (4) in the diagnosis: no refusal counted, so LOAD_REFUSAL_LIMIT never
  // tripped, so the evict-and-reseed that repairs this never ran. PR #602
  // widened the right guard for a fault that never reached it.
  it("counts toward the reseed budget, on a counter a reload cannot clear", async () => {
    const h = harness();
    await h.append("state the room holds");
    vi.spyOn(LoroDoc.prototype, "export").mockImplementationOnce(() => {
      throw borrowConflict();
    });

    await h.join();

    const stats = await h.stats();
    expect(stats.borrow.conflicts).toBe(1);
    expect(stats.borrow.strikes).toBe(1);
    // Same limit as the load guard, its own counter: `loadRefusals` is cleared
    // by a successful cold replay, and a borrow conflict ALWAYS follows one —
    // the wedge shows up on a doc that loaded. Booked there it could never
    // reach the limit, which is the diagnosis's (4) in a different hat.
    expect(stats.borrow.limit).toBe(LOAD_REFUSAL_LIMIT);
    expect(stats.docLoad.refusals).toBe(0);
    expect(stats.borrow.lastConflict).toMatchObject({ site: `ws-${MessageType.JoinRequest}` });
  });

  // The cure that usually ends it before the budget matters: the wedged wrapper
  // is dropped, so the next join materializes a fresh doc off the same stored
  // bytes and the room serves again. `isLive()` cannot see the wedge, so
  // nothing else would have dropped it — the room would have handed the same
  // unusable object to every join for the life of the instance.
  it("drops the wedged doc, and the very next join succeeds", async () => {
    const h = harness();
    await h.append("state the room holds");
    vi.spyOn(LoroDoc.prototype, "export").mockImplementationOnce(() => {
      throw borrowConflict();
    });

    await h.join();
    const second = await h.join();

    const answered = second.sent.map((f) => decode(f).type);
    expect(answered).toContain(MessageType.JoinResponseOk);
    expect(answered).not.toContain(MessageType.JoinError);
    // A join that went in and out of the doc and came back with bytes is the
    // only evidence that exists that the object is usable — so it, and nothing
    // else, clears the strike.
    expect((await h.stats()).borrow.strikes).toBe(0);
  });

  // The line this ticket exists to not cross: `ctx.abort()` recycles the
  // isolate, which sheds every co-located room's sockets and invites the
  // reseed that reproduces the fault. A wedged object is not a poisoned heap.
  it("never strikes the wasm-poison tripwire", async () => {
    const h = harness();
    await h.append("state the room holds");
    vi.spyOn(LoroDoc.prototype, "export").mockImplementation(() => {
      throw borrowConflict();
    });

    for (let i = 0; i < 5; i++) await h.join();

    expect(h.ctx.aborts).toEqual([]);
    expect((await h.stats()).lastAbort).toBeNull();
  });

  it("evicts and reseeds after LOAD_REFUSAL_LIMIT consecutive conflicts", async () => {
    const h = harness();
    await h.append("state the room holds");
    expect((await h.stats()).updateRows).toBeGreaterThan(0);
    vi.spyOn(LoroDoc.prototype, "export").mockImplementation(() => {
      throw borrowConflict();
    });

    for (let i = 0; i < LOAD_REFUSAL_LIMIT; i++) await h.join();

    const stats = await h.stats();
    expect(stats.updateRows).toBe(0); // stored state evicted; the fleet reseeds it
    expect(stats.snapshotBytes).toBe(0);
    expect(stats.borrow.strikes).toBe(0); // budget reset by the eviction
    // Attached sockets are booted so a redial with an empty VV re-uploads full
    // state, rather than a live session pushing deps the emptied doc lacks.
    expect(h.ctx.sockets.some((s) => s.closed === 4411)).toBe(true);
  });
});

// ── a push that hits one ───────────────────────────────────────────────────

describe("a %LOR push that hits a stuck borrow", () => {
  // Salvage re-imports each update individually against the same doc. Against a
  // WEDGED doc every retry fails, and the batch ends with the SENDER struck
  // toward the import penalty box for a fault that is the room's.
  it("does not put the sending device in the penalty box", async () => {
    const h = harness();
    const ws = await h.join();
    const update = (() => {
      const doc = new LoroDoc();
      doc.getText("t").insert(0, "a write from the mac");
      doc.commit();
      return doc.export({ mode: "update" });
    })();
    vi.spyOn(LoroDoc.prototype, "import").mockImplementationOnce(() => {
      throw borrowConflict();
    });

    await h.room.webSocketMessage(
      ws as unknown as WebSocket,
      (() => {
        const f = encode({
          type: MessageType.DocUpdate,
          crdt: CrdtType.Loro,
          roomId: ROOM,
          batchId: "0x0000000000000001",
          updates: [update]
        });
        return f.buffer.slice(f.byteOffset, f.byteOffset + f.byteLength) as ArrayBuffer;
      })()
    );

    const stats = (await h.stats()) as Stats & { importPenalty: unknown[] };
    expect(stats.importPenalty).toEqual([]);
    expect(stats.borrow.conflicts).toBe(1);
  });
});

// ── the leak, and the masking ──────────────────────────────────────────────

describe("a free that cannot happen", () => {
  // `free()` zeroes `__wbg_ptr` and unregisters the finalizer BEFORE calling
  // into wasm, so a throwing free abandons the Rust value with nothing left to
  // reclaim it. The room must at least SAY so: an isolate walking into its
  // memory limit with every room reporting health is the five-day diagnosis.
  it("is counted as the leak it is", async () => {
    const h = harness();
    await h.append("state the room holds");
    await h.join();
    vi.spyOn(LoroDoc.prototype, "free").mockImplementationOnce(() => {
      throw borrowConflict();
    });

    // Any path that abandons the cached doc will do; /reset-log is the one an
    // operator reaches for, and gh#553 already made its cleanup non-fatal.
    const reset = await h.room.fetch(
      new Request("https://room.invalid/reset-log", {
        method: "POST",
        headers: { [AUTH_USER_HEADER]: USER, [ROOM_KIND_HEADER]: "workspace" }
      })
    );

    expect(reset.status).toBe(200); // the drop still happened (gh#553)
    const stats = await h.stats();
    expect(stats.borrow.leaked).toBe(1);
    expect(stats.borrow.isolateLeaked).toBeGreaterThan(0);
  });

  // The masking half, and the reason the borrow signature appears NOWHERE in
  // edge/src: every free here lives in a `finally`, so a throwing free replaces
  // the in-flight exception with its own. The RangeError the guards were built
  // to read arrived at the catch as `attempted to take ownership…`, matching
  // nothing. It must not be able to do that.
  it("never replaces the exception that was already in flight", async () => {
    const h = harness();
    await h.append("state the room holds");
    // The join's own version vector is freed in a `finally` around the send.
    vi.spyOn(VersionVector.prototype, "free").mockImplementation(() => {
      throw borrowConflict();
    });
    vi.spyOn(LoroDoc.prototype, "export").mockImplementationOnce(() => {
      throw new RangeError("Invalid array buffer length");
    });

    const ws = await h.join();

    // The RangeError reached the guards — as a wasm fault, on a working heap,
    // which is gh#557's non-striking classification — rather than being
    // rewritten into a borrow conflict by the cleanup underneath it.
    const stats = await h.stats();
    expect(stats.wasm.faults).toBeGreaterThan(0);
    expect(stats.borrow.conflicts).toBe(0);
    expect(lastFrame(ws).type).toBe(MessageType.JoinError);
  });
});
