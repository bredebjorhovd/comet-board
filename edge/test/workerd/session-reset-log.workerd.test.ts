// gh#553 — the wedge break must land on a WEDGED room.
//
// `POST /reset-log` exists for exactly one situation: a room so sick that no
// client can repair it by dialing. On 2026-08-22, recovering the workspace room
// during the gh#527 incident, it answered 500 / Workers 1101 twice before
// succeeding on the third try — a JavaScript exception in the handler, on the
// one room the handler is for. The room had just aborted on `wasm heap poisoned
// after 3 strikes`, so its cached `this.doc` was a dangling wasm-bindgen
// wrapper, and the handler's unguarded `this.doc.free()` threw straight out of
// it. An operator who did not blindly retry would have concluded the wedge break
// was broken and escalated to a generation bump (ws4 -> ws5) instead.
//
// So: dropping the log is the point and every other step in that handler is
// cleanup, allowed to fail without taking the drop with it. Run in real workerd
// against the production SessionRoom, real SQLite and real loro-wasm, because
// what is being asserted is what happens at the wasm/storage boundary.

import { env, runInDurableObject } from "cloudflare:test";
import { LoroDoc } from "loro-crdt";
import { CrdtType, MessageType, decode, encode, type ProtocolMessage } from "loro-protocol";
import { describe, expect, it } from "vitest";
import { AUTH_ORG_HEADER, AUTH_USER_HEADER, ROOM_KIND_HEADER } from "../../src/env";
import type { SessionRoom } from "../../src/session-room";

const ROOM_ID = "reset-log-workspace";
const USER_ID = "synthetic-user";
const ORG_ID = "synthetic-org";

/** Workspace-room headers: the Worker authorizes org membership, so the DO
 * treats every member as an owner — the shape the incident's caller had. */
const headers = (): Record<string, string> => ({
  [AUTH_USER_HEADER]: USER_ID,
  [AUTH_ORG_HEADER]: ORG_ID,
  [ROOM_KIND_HEADER]: "workspace"
});

class CapturingSocket {
  private attachment: unknown = {
    userId: USER_ID,
    orgId: ORG_ID,
    workspace: true,
    rooms: [],
    deviceId: "synthetic-phone",
    joinedAt: Date.now(),
    sid: "synthetic-socket"
  };
  readonly sent: Uint8Array[] = [];
  closedWith: { code?: number; reason?: string } | undefined;

  serializeAttachment(value: unknown): void {
    this.attachment = value;
  }

  deserializeAttachment(): unknown {
    return this.attachment;
  }

  send(bytes: Uint8Array): void {
    this.sent.push(bytes.slice());
  }

  close(code?: number, reason?: string): void {
    this.closedWith = { code, reason };
  }
}

const messageBuffer = (message: ProtocolMessage): ArrayBuffer => {
  const bytes = encode(message);
  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
};

/** One join on a fresh socket; returns every frame the room answered with. */
const join = async (room: SessionRoom): Promise<ProtocolMessage[]> => {
  const socket = new CapturingSocket();
  await room.webSocketMessage(
    socket as unknown as WebSocket,
    messageBuffer({
      type: MessageType.JoinRequest,
      crdt: CrdtType.Loro,
      roomId: ROOM_ID,
      auth: new Uint8Array(),
      version: new Uint8Array()
    })
  );
  return socket.sent.map((frame) => decode(frame));
};

/** Put real state in the log, then force it out of the pending buffer and into
 * SQLite (the debounced flush would otherwise still be pending), leaving the
 * room with a materialized `this.doc` — the state the wedge break meets. */
const seed = async (room: SessionRoom): Promise<void> => {
  const doc = new LoroDoc();
  try {
    doc.getMap("session").set("title", "a room worth resetting");
    doc.commit();
    const update = doc.export({ mode: "snapshot" });
    const appended = await room.fetch(
      new Request("https://room.invalid/append", {
        method: "POST",
        headers: headers(),
        body: update.buffer.slice(update.byteOffset, update.byteOffset + update.byteLength)
      })
    );
    expect(appended.status).toBe(200);
  } finally {
    doc.free();
  }
  // GET /snapshot flushes before it exports, which is what lands the rows.
  const exported = await room.fetch(
    new Request("https://room.invalid/snapshot", { headers: headers() })
  );
  expect(exported.status).toBe(200);
};

const resetLog = (room: SessionRoom): Promise<Response> =>
  room.fetch(new Request("https://room.invalid/reset-log", { method: "POST", headers: headers() }));

/** The room's cached doc, as the handler sees it. */
const cachedDoc = (room: SessionRoom): { __wbg_ptr: number; free(): void } =>
  (room as unknown as { doc: { __wbg_ptr: number; free(): void } }).doc;

const updateRows = (state: DurableObjectState): number =>
  Number([...state.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n ?? -1);

describe("POST /reset-log on real workerd", () => {
  it("still drops the log when the cached doc wrapper has already been freed", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("reset-freed-wrapper"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      await seed(room);
      expect(updateRows(state)).toBeGreaterThan(0);

      // The fixture: free the cached wrapper WITHOUT clearing the field, which
      // is what a poisoned-heap abort and the free-and-replace flows (trim,
      // fold, alarm, idle release) leave behind. Calling into it now throws
      // `null pointer passed to rust`; freeing it again is a double free.
      const dangling = cachedDoc(room);
      expect(dangling).toBeDefined();
      dangling.free();
      expect(dangling.__wbg_ptr).toBe(0);

      const response = await resetLog(room);
      // Not a 1101. The one tool for a wedged room works on a wedged room.
      expect(response.status).toBe(200);
      const body = (await response.json()) as {
        ok: boolean;
        clearedUpdateRows: number;
        problems?: string[];
      };
      expect(body.ok).toBe(true);
      expect(body.clearedUpdateRows).toBeGreaterThan(0);
      // Nothing limped: the freed wrapper is skipped, not caught.
      expect(body.problems).toBeUndefined();

      // And the log is actually gone — the point of the request.
      expect(updateRows(state)).toBe(0);
      expect(
        Number(
          [...state.storage.sql.exec("SELECT COUNT(*) AS n FROM blobs WHERE name = 'snapshot'")][0]
            ?.n
        )
      ).toBe(0);

      // The room is usable again: the next dial materializes an empty doc
      // rather than re-entering the dangling wrapper.
      const healed = await join(room);
      expect(healed.some((frame) => frame.type === MessageType.JoinResponseOk)).toBe(true);
      expect(healed.some((frame) => frame.type === MessageType.JoinError)).toBe(false);
    });
  });

  it("drops the log even when releasing the doc throws, and reports what limped", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("reset-poisoned-free"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      await seed(room);
      expect(updateRows(state)).toBeGreaterThan(0);

      // A wrapper that still looks live but whose free() dies inside wasm —
      // the exhausted-heap shape (`RangeError: Invalid array buffer length`)
      // the incident room carried. `isLive` cannot see this one coming, so the
      // handler has to survive it rather than predict it.
      const real = cachedDoc(room);
      (room as unknown as { doc: unknown }).doc = {
        __wbg_ptr: 1,
        free() {
          throw new RangeError("Invalid array buffer length");
        }
      };

      const response = await resetLog(room);
      expect(response.status).toBe(200);
      const body = (await response.json()) as {
        ok: boolean;
        clearedUpdateRows: number;
        problems?: string[];
      };
      expect(body.ok).toBe(true);
      expect(body.clearedUpdateRows).toBeGreaterThan(0);
      // Failure-tolerant, not failure-blind: the drop happened AND the operator
      // is told which cleanup step died.
      expect(body.problems).toEqual([expect.stringContaining("free doc")]);
      expect(updateRows(state)).toBe(0);

      const healed = await join(room);
      expect(healed.some((frame) => frame.type === MessageType.JoinResponseOk)).toBe(true);
      real.free(); // the doc the fake displaced; nothing else will release it
    });
  });

  it("boots attached sockets and leaves an ordinary reset unchanged", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("reset-healthy-room"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      await seed(room);

      const response = await resetLog(room);
      expect(response.status).toBe(200);
      const body = (await response.json()) as { ok: boolean; clearedUpdateRows: number };
      expect(body.ok).toBe(true);
      expect(body.clearedUpdateRows).toBeGreaterThan(0);
      expect(updateRows(state)).toBe(0);
      // dropLog's guard on the nightly R2 put: until an engine re-uploads, the
      // materialized doc is empty and must not overwrite the disaster copy.
      const reported = (await (
        await room.fetch(new Request("https://room.invalid/stats", { headers: headers() }))
      ).json()) as Record<string, unknown>;
      expect(reported.postReset).toBe(true);
    });
  });
});
