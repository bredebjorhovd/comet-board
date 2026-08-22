// gh#557 — the reseed must not be what poisons the room.
//
// A workspace doc is the only doc here big enough to fragment: ~1.7MB against a
// 200_000-byte payload budget is nine frames on the wire. Every reset — manual
// `POST /reset-log` or the automated wedge break — is followed by exactly such
// a reseed, so the first question is whether reassembly can produce bytes that
// are not the bytes the device sent:
//
//   reset -> device re-uploads full doc -> fragmented -> reassembly corrupts
//         -> `doc.import` throws -> abort -> reset
//
// The first two tests are that question. They PASS on the unfixed handler: the
// happy path — one update over the budget, and two devices reseeding at once —
// reassembles byte-identically, so fragmentation is not the corrupter. They
// stay as the pin, because "we checked" is worth exactly as much as the test
// that says so.
//
// The rest are the gaps the reading found. This is the only one of the three
// reassemblers in the tree without a bounds check, a distinct-index count, or a
// size check — crates/sync/src/room.rs `on_fragment` and apps/ios `onFragment`
// have all three — and each of them turns a batch the room mishandled into an
// InvalidUpdate charged to the device that sent it correctly.
//
// Real workerd against the production SessionRoom, because the assertion is
// about what crosses the Loro boundary, byte for byte.

import { env, runInDurableObject } from "cloudflare:test";
import { LoroDoc } from "loro-crdt";
import {
  CrdtType,
  MessageType,
  UpdateStatusCode,
  decode,
  encode,
  type ProtocolMessage
} from "loro-protocol";
import { describe, expect, it } from "vitest";
import { AUTH_USER_HEADER, ROOM_KIND_HEADER } from "../../src/env";
import { FRAGMENT_BYTES, type SessionRoom } from "../../src/session-room";

const ROOM_ID = "ws4/synthetic-org/synthetic-user";
const USER_ID = "synthetic-user";

class CapturingSocket {
  private attachment: unknown;
  readonly sent: Uint8Array[] = [];
  closedWith: { code?: number; reason?: string } | undefined;

  constructor(readonly deviceId: string) {
    this.attachment = {
      userId: USER_ID,
      rooms: [CrdtType.Loro],
      workspace: true,
      deviceId,
      joinedAt: Date.now(),
      sid: `sid-${deviceId}`
    };
  }

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

  acks(): Array<{ status: UpdateStatusCode; refId: string }> {
    return this.sent
      .map((frame) => decode(frame))
      .filter(
        (m): m is Extract<ProtocolMessage, { type: typeof MessageType.Ack }> =>
          m.type === MessageType.Ack
      )
      .map((m) => ({ status: m.status, refId: m.refId }));
  }
}

const messageBuffer = (message: ProtocolMessage): ArrayBuffer => {
  const bytes = encode(message);
  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
};

const deliver = async (
  room: SessionRoom,
  socket: CapturingSocket,
  message: ProtocolMessage
): Promise<void> => {
  await room.webSocketMessage(socket as unknown as WebSocket, messageBuffer(message));
};

/** A workspace-sized update: comfortably over one fragment, so the wire shape
 * under test is the reseed shape and not a single frame. */
const bigUpdate = (peer: number, keys: number): { update: Uint8Array; json: unknown } => {
  const doc = new LoroDoc();
  try {
    doc.setPeerId(peer);
    const workspace = doc.getMap("workspace");
    for (let i = 0; i < keys; i++) {
      workspace.set(`peer-${peer}/chat-${i}`, `${"payload".repeat(24)}:${i}`);
    }
    doc.commit();
    return { update: doc.export({ mode: "update" }), json: doc.toJSON() };
  } finally {
    doc.free();
  }
};

const chunk = (update: Uint8Array): Uint8Array[] => {
  const parts: Uint8Array[] = [];
  for (let off = 0; off < update.length; off += FRAGMENT_BYTES) {
    parts.push(update.subarray(off, Math.min(off + FRAGMENT_BYTES, update.length)));
  }
  return parts;
};

const header = (
  batchId: `0x${string}`,
  update: Uint8Array,
  overrides: { fragmentCount?: number; totalSizeBytes?: number } = {}
): ProtocolMessage => ({
  type: MessageType.DocUpdateFragmentHeader,
  crdt: CrdtType.Loro,
  roomId: ROOM_ID,
  batchId,
  fragmentCount: overrides.fragmentCount ?? chunk(update).length,
  totalSizeBytes: overrides.totalSizeBytes ?? update.length
});

const fragment = (batchId: `0x${string}`, index: number, bytes: Uint8Array): ProtocolMessage => ({
  type: MessageType.DocUpdateFragment,
  crdt: CrdtType.Loro,
  roomId: ROOM_ID,
  batchId,
  index,
  fragment: bytes
});

const stats = async (room: SessionRoom): Promise<Record<string, unknown>> => {
  const response = await room.fetch(
    new Request("https://room.invalid/stats", {
      headers: { [AUTH_USER_HEADER]: USER_ID, [ROOM_KIND_HEADER]: "workspace" }
    })
  );
  expect(response.status).toBe(200);
  return (await response.json()) as Record<string, unknown>;
};

/** The doc the room actually holds, read back through the repair export. */
const roomJson = async (room: SessionRoom): Promise<unknown> => {
  const response = await room.fetch(
    new Request("https://room.invalid/snapshot", {
      headers: { [AUTH_USER_HEADER]: USER_ID, [ROOM_KIND_HEADER]: "workspace" }
    })
  );
  expect(response.status).toBe(200);
  const doc = new LoroDoc();
  try {
    doc.import(new Uint8Array(await response.arrayBuffer()));
    return doc.toJSON();
  } finally {
    doc.free();
  }
};

describe("SessionRoom fragment reassembly on real workerd", () => {
  it("imports a single update above FRAGMENT_BYTES byte-identically", async () => {
    const source = bigUpdate(1, 3000);
    expect(source.update.length).toBeGreaterThan(FRAGMENT_BYTES);
    const parts = chunk(source.update);
    expect(parts.length).toBeGreaterThan(1);

    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("fragment-single"));
    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const socket = new CapturingSocket("device-a");
      const batchId = "0x1111111111111111" as const;

      await deliver(room, socket, header(batchId, source.update));
      for (const [index, bytes] of parts.entries()) {
        await deliver(room, socket, fragment(batchId, index, bytes));
      }

      expect(socket.acks()).toEqual([{ status: UpdateStatusCode.Ok, refId: batchId }]);
      expect(await roomJson(room)).toEqual(source.json);
      expect((await stats(room)).lastAbort).toBeNull();
    });
  });

  it("keeps two devices' concurrent reseeds separate", async () => {
    // The shape the ticket names specifically: both engines re-upload their
    // full replica after a reset, at once, into one room.
    const a = bigUpdate(11, 3000);
    const b = bigUpdate(22, 3000);
    const partsA = chunk(a.update);
    const partsB = chunk(b.update);

    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("fragment-concurrent"));
    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const socketA = new CapturingSocket("device-a");
      const socketB = new CapturingSocket("device-b");
      const batchA = "0xaaaaaaaaaaaaaaaa" as const;
      const batchB = "0xbbbbbbbbbbbbbbbb" as const;

      await deliver(room, socketA, header(batchA, a.update));
      await deliver(room, socketB, header(batchB, b.update));
      for (let i = 0; i < Math.max(partsA.length, partsB.length); i++) {
        if (i < partsA.length) await deliver(room, socketA, fragment(batchA, i, partsA[i]!));
        if (i < partsB.length) await deliver(room, socketB, fragment(batchB, i, partsB[i]!));
      }

      expect(socketA.acks()).toEqual([{ status: UpdateStatusCode.Ok, refId: batchA }]);
      expect(socketB.acks()).toEqual([{ status: UpdateStatusCode.Ok, refId: batchB }]);
      expect(await roomJson(room)).toEqual({
        workspace: {
          ...(a.json as { workspace: Record<string, unknown> }).workspace,
          ...(b.json as { workspace: Record<string, unknown> }).workspace
        }
      });
      expect((await stats(room)).lastAbort).toBeNull();
    });
  });

  it("does not let a repeated fragment complete a batch that is still short", async () => {
    // Counting ARRIVALS rather than distinct indices let a resent fragment
    // finish a batch with a hole in it. What assembles then is short by one
    // fragment, zero-padded at the tail, with everything after the hole
    // shifted 200_000 bytes early — bytes no device ever held, handed to
    // `doc.import`, and blamed on the sender.
    const source = bigUpdate(33, 3000);
    const parts = chunk(source.update);
    expect(parts.length).toBeGreaterThan(2);

    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("fragment-duplicate"));
    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const socket = new CapturingSocket("device-a");
      const batchId = "0xcccccccccccccccc" as const;

      await deliver(room, socket, header(batchId, source.update));
      // Everything but the middle fragment, then fragment 0 a second time: the
      // arrival count reaches fragmentCount while index 1 has never come.
      for (const [index, bytes] of parts.entries()) {
        if (index === 1) continue;
        await deliver(room, socket, fragment(batchId, index, bytes));
      }
      await deliver(room, socket, fragment(batchId, 0, parts[0]!));

      // Nothing assembled, so nothing was answered and nothing was imported.
      expect(socket.acks()).toEqual([]);
      expect(await roomJson(room)).toEqual({});

      // And the batch is still open: the fragment that was actually missing
      // completes it, correctly, whenever it turns up.
      await deliver(room, socket, fragment(batchId, 1, parts[1]!));
      expect(socket.acks()).toEqual([{ status: UpdateStatusCode.Ok, refId: batchId }]);
      expect(await roomJson(room)).toEqual(source.json);
      expect((await stats(room)).lastAbort).toBeNull();
    });
  });

  it("refuses a fragment index past the header's fragmentCount", async () => {
    const source = bigUpdate(44, 1200);
    const parts = chunk(source.update);

    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("fragment-out-of-range"));
    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const socket = new CapturingSocket("device-a");
      const batchId = "0xdddddddddddddddd" as const;

      await deliver(room, socket, header(batchId, source.update));
      await deliver(room, socket, fragment(batchId, parts.length, parts[0]!));

      // Unfixed, this extended the parts array with a hole: the batch could
      // never complete, its buffer sat there until the socket died, and the
      // sender was never told.
      expect(socket.acks()).toEqual([{ status: UpdateStatusCode.InvalidUpdate, refId: batchId }]);
      expect(await roomJson(room)).toEqual({});
      expect((await stats(room)).lastAbort).toBeNull();
    });
  });

  it("refuses a batch whose fragments do not fill its declared size", async () => {
    const source = bigUpdate(55, 1200);
    const parts = chunk(source.update);

    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("fragment-size-mismatch"));
    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const socket = new CapturingSocket("device-a");
      const batchId = "0xeeeeeeeeeeeeeeee" as const;

      // A header that claims more bytes than its fragments carry. The tail of
      // the assembled buffer would be zeros — a valid-looking length prefix
      // over nothing, which is the shape that makes a decoder reach for an
      // allocation it cannot have.
      await deliver(
        room,
        socket,
        header(batchId, source.update, { totalSizeBytes: source.update.length + 4096 })
      );
      for (const [index, bytes] of parts.entries()) {
        await deliver(room, socket, fragment(batchId, index, bytes));
      }

      expect(socket.acks()).toEqual([{ status: UpdateStatusCode.InvalidUpdate, refId: batchId }]);
      expect(await roomJson(room)).toEqual({});
      const reported = await stats(room);
      expect(reported.lastAbort).toBeNull();
      // The ack was InvalidUpdate before this fix too — but only after the
      // zero-padded buffer had been pushed through `doc.import`, which struck
      // the device toward the penalty box for a batch the room mis-assembled.
      // Recognising it at the seam charges nobody and costs no wasm call.
      expect(reported.importPenalty).toEqual([]);
    });
  });
});
