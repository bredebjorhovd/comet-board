// gh#527 — a room whose stored doc cannot be loaded must REFUSE, not loop.
//
// The failure this pins is the second of the two producers of the 2026-08-19
// signature (the first being the free-tier duration cap): a room whose stored
// state kills the instance on every wake "answers the join every single time"
// and then dies 1006, with the client's ladder climbing to its 30s cap and the
// tail showing nothing but `canceled` invocations. Every dial re-enters the
// same death, so the room can never heal and nobody can see why.
//
// So: the load is bounded and checked BEFORE any wasm call, an unloadable doc
// is answered with a JoinError rather than attempted, and after
// LOAD_REFUSAL_LIMIT consecutive refusals the room evicts its stored state and
// lets the fleet reseed it. Run in real workerd, against the production
// SessionRoom and real SQLite, because the thing being asserted is what
// happens at the Loro/storage boundary.

import { env, runInDurableObject } from "cloudflare:test";
import { LoroDoc } from "loro-crdt";
import {
  CrdtType,
  JoinErrorCode,
  MessageType,
  decode,
  encode,
  type ProtocolMessage
} from "loro-protocol";
import { describe, expect, it } from "vitest";
import { AUTH_USER_HEADER } from "../../src/env";
import { LOAD_REFUSAL_LIMIT, MAX_DOC_LOAD_BYTES, type SessionRoom } from "../../src/session-room";

const ROOM_ID = "load-guard-chat";
const USER_ID = "synthetic-user";

class CapturingSocket {
  private attachment: unknown = {
    userId: USER_ID,
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

const stats = async (room: SessionRoom): Promise<Record<string, unknown>> => {
  const response = await room.fetch(
    new Request("https://room.invalid/stats", { headers: { [AUTH_USER_HEADER]: USER_ID } })
  );
  expect(response.status).toBe(200);
  return (await response.json()) as Record<string, unknown>;
};

describe("SessionRoom load guard on real workerd", () => {
  it("refuses a corrupt snapshot cleanly, then evicts it so the fleet can reseed", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("corrupt-snapshot"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      // Stored bytes that are not a Loro snapshot. This is the shape a
      // truncated write or an incompatible encoding leaves behind, and before
      // the guard it threw out of every cold `ensureDoc` forever.
      const corrupt = new Uint8Array(4096).fill(0x7f);
      state.storage.sql.exec(
        "INSERT INTO blobs (name, idx, bytes) VALUES ('snapshot', 0, ?)",
        corrupt.buffer.slice(0, corrupt.byteLength)
      );

      for (let attempt = 1; attempt <= LOAD_REFUSAL_LIMIT; attempt++) {
        const frames = await join(room);
        // The whole point: an ANSWER. Not a dead socket, not an abort — a
        // JoinError the client's backoff can act on.
        expect(frames).toHaveLength(1);
        const error = frames[0]!;
        expect(error.type).toBe(MessageType.JoinError);
        if (error.type === MessageType.JoinError) {
          expect(error.code).toBe(JoinErrorCode.AppError);
          expect(error.message).toContain("doc load refused");
        }
      }

      // And the room says so where an operator can read it, on a surface that
      // still works precisely because the room refused instead of dying.
      const refusing = await stats(room);
      const docLoad = refusing.docLoad as Record<string, unknown>;
      expect(docLoad.guardBytes).toBe(MAX_DOC_LOAD_BYTES);
      expect((docLoad.lastRefusal as { detail: string }).detail).toContain("will not import");

      // Past the limit the unloadable state is EVICTED (gh#148/#207): every
      // engine holds the doc locally and re-uploads it on the next join, and
      // `postReset` keeps the R2 disaster copy from being overwritten by the
      // emptied doc in the meantime.
      expect(refusing.postReset).toBe(true);
      const remaining = [
        ...state.storage.sql.exec("SELECT COUNT(*) AS n FROM blobs WHERE name = 'snapshot'")
      ][0]?.n;
      expect(Number(remaining)).toBe(0);

      // The room is healed: the next dial joins an empty doc rather than
      // re-entering the same death.
      const healed = await join(room);
      expect(healed.some((frame) => frame.type === MessageType.JoinResponseOk)).toBe(true);
      expect(healed.some((frame) => frame.type === MessageType.JoinError)).toBe(false);
    });
  });

  it("refuses a doc whose stored bytes are past the guard, and keeps answering", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("oversized-doc"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      // The recorded log size is what the guard reads, so an oversized room is
      // reproducible without writing 32MB into the test's SQLite.
      state.storage.sql.exec("INSERT INTO meta (key, value) VALUES ('updateBytes', ?)", String(
        MAX_DOC_LOAD_BYTES + 1
      ));

      const frames = await join(room);
      expect(frames).toHaveLength(1);
      const error = frames[0]!;
      expect(error.type).toBe(MessageType.JoinError);
      if (error.type === MessageType.JoinError) {
        expect(error.message).toContain("load guard");
      }

      // The instance is alive — a refusal costs a join, not the room. (An
      // abort here would take the whole test with it, which is the difference
      // this test exists to hold.)
      const reported = await stats(room);
      expect((reported.docLoad as { refusals: number }).refusals).toBe(1);
    });
  });

  it("leaves an ordinary room alone", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("healthy-doc"));

    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      // Claim the room first: a chat room is owned by its first joiner, and an
      // unclaimed one answers repair writes with `not_found`.
      expect((await join(room)).some((frame) => frame.type === MessageType.JoinResponseOk)).toBe(
        true
      );
      const doc = new LoroDoc();
      try {
        doc.getMap("session").set("title", "an ordinary chat");
        doc.commit();
        const update = doc.export({ mode: "snapshot" });
        const appended = await room.fetch(
          new Request("https://room.invalid/append", {
            method: "POST",
            headers: { [AUTH_USER_HEADER]: USER_ID },
            body: update.buffer.slice(update.byteOffset, update.byteOffset + update.byteLength)
          })
        );
        expect(appended.status).toBe(200);
      } finally {
        doc.free();
      }

      const frames = await join(room);
      expect(frames.some((frame) => frame.type === MessageType.JoinResponseOk)).toBe(true);
      expect(frames.some((frame) => frame.type === MessageType.JoinError)).toBe(false);
      const reported = await stats(room);
      expect((reported.docLoad as { refusals: number }).refusals).toBe(0);
    });
  });
});
