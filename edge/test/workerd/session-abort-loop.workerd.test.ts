// gh#557 — a room must answer its own bad day, not abort the isolate over it.
//
// The loop this ticket names is `reset -> reseed -> poison -> abort -> reset`,
// and the step that closes it is the abort. `escalateWasmPoisoning` read every
// RangeError as proof the shared loro-wasm heap was exhausted, because that is
// what it looked like on 2026-08-04 — but the two readings of
// `RangeError("Invalid array buffer length")` want opposite cures, and only one
// of them is fixed by recycling an isolate. Guessing wrong costs every
// co-located room its sockets, and the reseed that follows walks the room
// straight back into the same call.
//
// Three rules, all asserted here against the production SessionRoom in real
// workerd:
//
//   1. A wasm-shaped failure raised while the heap demonstrably still works is
//      recorded, not struck, and never aborts.
//   2. `foldLog` catches its own snapshot export. It used to be the one wasm
//      export in this room with no guard at all, so its failure escaped through
//      `flush()` to whichever caller was on the stack — the debounced timer,
//      socket close, socket error, the alarm — and all four hand it to the
//      tripwire. And because a failed fold clears nothing, the very next write
//      crossed the same threshold and did it again.
//
//   3. A push that will not import is ANSWERED. `applyUpdates` rethrew a
//      wasm-shaped failure without acking, so the sender learned nothing,
//      redialled, and re-sent the same payload — the reseed half of the loop,
//      driven by the room itself.

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
import type { SessionRoom } from "../../src/session-room";
import { COMPACT_LOG_ROWS } from "../../src/session-doc";

const ROOM_ID = "ws4/synthetic-org/synthetic-user";
const USER_ID = "synthetic-user";

class CapturingSocket {
  private attachment: unknown = {
    userId: USER_ID,
    rooms: [CrdtType.Loro],
    workspace: true,
    deviceId: "synthetic-mac",
    joinedAt: Date.now(),
    sid: "synthetic-socket"
  };
  readonly sent: Uint8Array[] = [];

  serializeAttachment(value: unknown): void {
    this.attachment = value;
  }

  deserializeAttachment(): unknown {
    return this.attachment;
  }

  send(bytes: Uint8Array): void {
    this.sent.push(bytes.slice());
  }

  close(): void {
    /* the room booting a socket is not what these tests are about */
  }
}

const messageBuffer = (message: ProtocolMessage): ArrayBuffer => {
  const bytes = encode(message);
  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
};

const stats = async (room: SessionRoom): Promise<Record<string, unknown>> => {
  const response = await room.fetch(
    new Request("https://room.invalid/stats", {
      headers: { [AUTH_USER_HEADER]: USER_ID, [ROOM_KIND_HEADER]: "workspace" }
    })
  );
  expect(response.status).toBe(200);
  return (await response.json()) as Record<string, unknown>;
};

/** One small %LOR push over the wire, the way an engine sends a status write. */
const push = async (room: SessionRoom, socket: CapturingSocket, key: string): Promise<void> => {
  const doc = new LoroDoc();
  try {
    doc.setPeerId(BigInt(key.length + 900));
    doc.getMap("workspace").set(key, `${Date.now()}`);
    doc.commit();
    await room.webSocketMessage(
      socket as unknown as WebSocket,
      messageBuffer({
        type: MessageType.DocUpdate,
        crdt: CrdtType.Loro,
        roomId: ROOM_ID,
        updates: [doc.export({ mode: "update" })],
        batchId: "0x0101010101010101"
      })
    );
  } finally {
    doc.free();
  }
};

/** Make this room's materialized doc refuse to export, exactly as a pressed
 * heap does — and leave every other wasm call working, which is the case the
 * tripwire could not previously distinguish. */
const breakSnapshotExport = (instance: object): { exports: number } => {
  const counter = { exports: 0 };
  const doc = (instance as { doc?: LoroDoc }).doc;
  if (!doc) throw new Error("the room has not materialized a doc yet");
  const real = doc.export.bind(doc);
  (doc as unknown as { export: LoroDoc["export"] }).export = ((mode: Parameters<LoroDoc["export"]>[0]) => {
    if ((mode as { mode?: string })?.mode === "snapshot") {
      counter.exports++;
      throw new RangeError("Invalid array buffer length");
    }
    return real(mode);
  }) as LoroDoc["export"];
  return counter;
};

describe("SessionRoom abort loop on real workerd", () => {
  it("keeps serving a room whose snapshot export will not complete", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("fold-failure"));

    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const socket = new CapturingSocket();

      // Materialize and get past the row threshold, so the next flush folds.
      await push(room, socket, "warmup");
      const broken = breakSnapshotExport(instance);
      for (let i = 0; i < COMPACT_LOG_ROWS + 2; i++) await push(room, socket, `row-${i}`);
      // `flush` is debounced; the alarm's scheduled work runs it inline.
      await room.alarm();

      expect(broken.exports).toBeGreaterThan(0);
      const reported = await stats(room);

      // THE RULE: no abort. Unfixed, this threw out of `flush()` into the
      // alarm's catch and struck the tripwire; three of those recycle the
      // isolate and sever every socket in it.
      expect(reported.lastAbort).toBeNull();
      const wasm = reported.wasm as { faults: number; lastFault: { site: string; struck: boolean } };
      expect(wasm.faults).toBeGreaterThan(0);
      expect(wasm.lastFault.site).toBe("fold-export");
      // The heap is fine — this test's own LoroDocs prove it — so the fault is
      // recorded against the call, not charged to the isolate.
      expect(wasm.lastFault.struck).toBe(false);

      // The failure is legible instead of showing up as `snapshotBytes: 0`.
      const fold = reported.fold as { consecutiveFailures: number; retryAt: number | null };
      expect(fold.consecutiveFailures).toBeGreaterThan(0);
      expect(fold.retryAt).toBeGreaterThan(Date.now());

      // Nothing was lost: the log still holds everything the fold could not
      // move into a snapshot, and the room still reads and writes.
      expect(reported.snapshotBytes).toBe(0);
      expect(reported.updateRows as number).toBeGreaterThan(COMPACT_LOG_ROWS);
      await push(room, socket, "after-the-failure");
      expect((await stats(room)).lastAbort).toBeNull();
    });
  });

  it("does not re-attempt a failed fold on every write", async () => {
    // A fold that fails clears nothing, so the threshold that triggered it is
    // still crossed on the next flush — an unbounded retry that spends a full
    // snapshot export of CPU per write, on the room least able to afford it.
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("fold-backoff"));

    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const socket = new CapturingSocket();

      await push(room, socket, "warmup");
      const broken = breakSnapshotExport(instance);
      for (let i = 0; i < COMPACT_LOG_ROWS + 2; i++) await push(room, socket, `row-${i}`);
      await room.alarm();
      const afterFirst = broken.exports;
      expect(afterFirst).toBeGreaterThan(0);

      for (let i = 0; i < 20; i++) {
        await push(room, socket, `more-${i}`);
        await room.alarm();
      }
      expect(broken.exports).toBe(afterFirst);
      expect((await stats(room)).lastAbort).toBeNull();
    });
  });

  it("folds normally once the export works again, clearing the backoff", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("fold-recovery"));

    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const socket = new CapturingSocket();

      await push(room, socket, "warmup");
      const doc = (instance as { doc?: LoroDoc }).doc!;
      const real = doc.export.bind(doc);
      let failing = true;
      (doc as unknown as { export: LoroDoc["export"] }).export = ((
        mode: Parameters<LoroDoc["export"]>[0]
      ) => {
        if (failing && (mode as { mode?: string })?.mode === "snapshot") {
          throw new RangeError("Invalid array buffer length");
        }
        return real(mode);
      }) as LoroDoc["export"];

      for (let i = 0; i < COMPACT_LOG_ROWS + 2; i++) await push(room, socket, `row-${i}`);
      await room.alarm();
      expect((await stats(room)).fold).toMatchObject({ consecutiveFailures: 1 });

      // Whatever the export could not have, it can now — and the backoff must
      // not outlive it. Clear the retry gate the way a later wake would.
      failing = false;
      await instance.ctx.storage.sql.exec("UPDATE meta SET value = '0' WHERE key = 'foldRetryAt'");
      await push(room, socket, "post-recovery");
      await room.alarm();

      const reported = await stats(room);
      expect(reported.fold).toMatchObject({ consecutiveFailures: 0, retryAt: null });
      expect(reported.snapshotBytes as number).toBeGreaterThan(0);
      expect(reported.updateRows).toBe(0);
      expect(reported.lastAbort).toBeNull();
    });
  });

  it("answers a push it cannot import instead of dying unacked", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("unimportable-push"));

    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const socket = new CapturingSocket();
      await push(room, socket, "warmup");

      const doc = (instance as { doc?: LoroDoc }).doc!;
      const real = doc.import.bind(doc);
      let failing = true;
      (doc as unknown as { import: LoroDoc["import"] }).import = ((bytes: Uint8Array) => {
        if (failing) throw new RangeError("Invalid array buffer length");
        return real(bytes);
      }) as LoroDoc["import"];

      socket.sent.length = 0;
      await push(room, socket, "will-not-import");

      // The whole point: an ANSWER. Unfixed this threw out of `applyUpdates`,
      // past the handler, and into the tripwire — the sender got no ack, and
      // three of those recycled the isolate.
      const acks = socket.sent
        .map((frame) => decode(frame))
        .filter((m) => m.type === MessageType.Ack);
      expect(acks).toHaveLength(1);
      expect(acks[0]).toMatchObject({ status: UpdateStatusCode.InvalidUpdate });

      const reported = await stats(room);
      expect(reported.lastAbort).toBeNull();
      // Attributed to the device that sent it, which is what the penalty box
      // is for — and what an empty `importPenalty` beside a dying room hid.
      expect(reported.importPenalty).toMatchObject([{ device: "synthetic-mac", strikes: 1 }]);
      expect((reported.wasm as { lastFault: { site: string; struck: boolean } }).lastFault).toBeNull();

      // And the room is not damaged: the next push lands normally.
      failing = false;
      await push(room, socket, "after-the-bad-push");
      expect((await stats(room)).pushOutcomes).toMatchObject([{ device: "synthetic-mac", ok: 2 }]);
    });
  });
});
