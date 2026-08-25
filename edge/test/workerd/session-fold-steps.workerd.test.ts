// gh#611 — the fold must be able to LAND on a room whose log is the reason it
// needs to fold.
//
// The 2026-08-25 ws4 room was the wedge shape, live: `snapshotBytes: 0`,
// `lastTrimAt: ""`, ~2.1MB of log and `alarm.consecutiveFailures: 8` of 24 —
// a room one budget-exceeding fold away from giving up on compaction
// permanently (gh#378's terminus), where the only remaining tools are
// reset-log (which throws the snapshot away too) or a generation break.
//
// Three properties are pinned here against the production SessionRoom in real
// workerd:
//
//   1. The stepped fold runs MORE THAN ONE bounded, committed step over a
//      backlog — incremental, not one all-or-nothing export.
//
//   2. A row that will not import stops the fold AT itself: everything behind
//      it has already landed, nothing past it is silently dropped (the
//      gh#554 hole argument), and the blocking seq is named on /stats.
//
//   3. Join answers are attributed per device, so a young-socket churn can be
//      read against the answers the room actually gave.

import { env, runInDurableObject } from "cloudflare:test";
import { LoroDoc } from "loro-crdt";
import {
  CrdtType,
  JoinErrorCode,
  MessageType,
  UpdateStatusCode,
  decode,
  encode,
  type ProtocolMessage
} from "loro-protocol";
import { describe, expect, it } from "vitest";
import { AUTH_USER_HEADER, ROOM_KIND_HEADER } from "../../src/env";
import type { SessionRoom } from "../../src/session-room";
import { COMPACT_LOG_BYTES, FOLD_STEP_ROWS } from "../../src/session-doc";

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

let peerCounter = 900;

/** One small %LOR push over the wire, the way an engine sends a status write.
 * Every push is its own peer, so rows import standalone — a later row never
 * depends on an earlier one except through the log's order. */
const push = async (room: SessionRoom, socket: CapturingSocket, key: string): Promise<void> => {
  const doc = new LoroDoc();
  try {
    doc.setPeerId(BigInt(peerCounter++));
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

/** A real Loro update written straight into the log at an exact seq — for
 * placing damage (and rows after it) where the test needs it. */
const standaloneUpdate = (): Uint8Array => {
  const doc = new LoroDoc();
  try {
    doc.setPeerId(BigInt(peerCounter++));
    doc.getMap("workspace").set(`seeded-${peerCounter}`, "1");
    doc.commit();
    return doc.export({ mode: "update" });
  } finally {
    doc.free();
  }
};

const insertUpdateRow = (sql: SqlStorage, bytes: Uint8Array): void => {
  sql.exec(
    "INSERT INTO updates (bytes, received_at, cont) VALUES (?, ?, 0)",
    bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength),
    Date.now()
  );
};

describe("stepped log fold on real workerd", () => {
  /** Join the room the way a device does — this is what stamps `chatId`, so
   * the room reads as a concurrent-write workspace doc and the fold (not a
   * live-frontier trim) is what answers a crossed compaction threshold. */
  const join = async (room: SessionRoom): Promise<void> => {
    await room.webSocketMessage(
      new CapturingSocket() as unknown as WebSocket,
      messageBuffer({
        type: MessageType.JoinRequest,
        crdt: CrdtType.Loro,
        roomId: ROOM_ID,
        auth: new Uint8Array(),
        version: new Uint8Array()
      })
    );
  };

  it("folds a backlog as several committed steps, not one all-or-nothing export", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("fold-steps"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      const socket = new CapturingSocket();
      let pushes = 0;

      await join(room);
      // More real rows than one step's budget holds, flushed into the log
      // (a /stats read runs the room's own flush).
      for (let i = 0; i < FOLD_STEP_ROWS + 10; i++) {
        await push(room, socket, `row-${i}`);
        pushes++;
      }
      await stats(room);

      // Force the BYTE trigger rather than pushing 400 rows for it: what is
      // under test is the step machinery, not the threshold arithmetic.
      state.storage.sql.exec(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('updateBytes', ?)",
        String(COMPACT_LOG_BYTES + 1024)
      );
      // One more push leaves the flush non-empty, so the alarm's flush
      // crosses the trigger and folds the whole staged backlog.
      await push(room, socket, "the-trigger");
      pushes++;

      // Count snapshot exports off the live doc: one per fold step, plus the
      // R2 backup export at the end of the scheduled work.
      const doc = (instance as unknown as { doc?: LoroDoc }).doc;
      if (!doc) throw new Error("the room has not materialized a doc yet");
      const realExport = doc.export.bind(doc);
      let exports = 0;
      (doc as unknown as { export: LoroDoc["export"] }).export = ((
        mode: Parameters<LoroDoc["export"]>[0]
      ) => {
        if ((mode as { mode?: string })?.mode === "snapshot") exports++;
        return realExport(mode);
      }) as LoroDoc["export"];

      await room.alarm();

      // More than one step ran — the fold is incremental, not one pass.
      expect(exports).toBeGreaterThanOrEqual(2);

      const reported = await stats(room);
      // ...and the steps compose into a full fold anyway.
      expect(reported.updateRows).toBe(0);
      expect(reported.snapshotBytes as number).toBeGreaterThan(0);
      expect(reported.fold).toMatchObject({ consecutiveFailures: 0 });
      expect(reported.lastAbort).toBeNull();

      // The room keeps serving beside its new snapshot.
      await push(room, socket, "post-fold");
      pushes++;
      expect(((await stats(room)).pushOutcomes as { ok: number }[])[0]!.ok).toBe(pushes);
    });
  });

  it("stops at a row that will not import, with everything behind it landed", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("fold-poison"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      const socket = new CapturingSocket();
      const sql = instance.ctx.storage.sql;

      await join(room);
      // A good prefix beyond one step's budget, flushed into the log.
      let pushes = 0;
      for (let i = 0; i < FOLD_STEP_ROWS + 5; i++) {
        await push(room, socket, `row-${i}`);
        pushes++;
      }
      await stats(room);

      // Stored damage at a known seq, healthy rows AFTER it, and finally one
      // more pushed update — the shape the gh#554 hole argument is about.
      const before = Number([...sql.exec("SELECT MAX(seq) AS n FROM updates")][0]?.n ?? 0);
      insertUpdateRow(sql, new Uint8Array(512).fill(0x7f)); // seq before+1: the poison
      insertUpdateRow(sql, standaloneUpdate());
      insertUpdateRow(sql, standaloneUpdate());

      state.storage.sql.exec(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('updateBytes', ?)",
        String(COMPACT_LOG_BYTES + 1024)
      );
      await push(room, socket, "the-trigger"); // seq before+4; makes the flush fold
      pushes++;

      await room.alarm();

      const reported = await stats(room);
      // Step one committed everything before the poison...
      expect(reported.snapshotBytes as number).toBeGreaterThan(0);
      // ...and the fold STOPPED at it: the poison row and every row after it
      // are still in the log — named, not silently folded around.
      const totalRows = before + 4;
      expect(reported.updateRows).toBe(totalRows - FOLD_STEP_ROWS);
      const fold = reported.fold as {
        consecutiveFailures: number;
        retryAt: number | null;
        lastFailure: { site: string; blockedAtSeq: number } | null;
      };
      expect(fold.consecutiveFailures).toBe(1);
      expect(fold.retryAt!).toBeGreaterThan(Date.now());
      expect(fold.lastFailure!.site).toBe("fold-import");
      expect(fold.lastFailure!.blockedAtSeq).toBe(before + 1);
      expect(reported.lastAbort).toBeNull();

      // The room still serves beside a fold that cannot finish: pushes land,
      // the failure is legible instead of fatal. Every push so far acked Ok.
      await push(room, socket, "after-the-stop");
      pushes++;
      expect(((await stats(room)).pushOutcomes as { ok: number }[])[0]!.ok).toBe(pushes);
    });
  });
});

describe("join outcome attribution on real workerd", () => {
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

  it("counts the answers a room gave joins, so young deaths read against them", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("join-outcomes"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      // Stored bytes no Loro version will decode: every join is REFUSED until
      // LOAD_REFUSAL_LIMIT trips the evict-and-reseed, after which a join
      // answers OK on an empty doc.
      const corrupt = new Uint8Array(4096).fill(0x7f);
      state.storage.sql.exec(
        "INSERT INTO blobs (name, idx, bytes) VALUES ('snapshot', 0, ?)",
        corrupt.buffer.slice(0, corrupt.byteLength)
      );

      for (let attempt = 0; attempt < 3; attempt++) {
        const frames = await join(room);
        expect(frames).toHaveLength(1);
        const error = frames[0]!;
        expect(error.type).toBe(MessageType.JoinError);
        if (error.type === MessageType.JoinError) {
          expect(error.code).toBe(JoinErrorCode.AppError);
        }
      }
      const healed = await join(room); // reseeded; the empty doc answers
      expect(healed.some((f) => f.type === MessageType.JoinResponseOk)).toBe(true);

      const outcomes = (await stats(room)).joinOutcomes as {
        device: string;
        ok: number;
        refused: number;
        failed: number;
      }[];
      expect(outcomes).toHaveLength(1);
      expect(outcomes[0]).toMatchObject({
        device: "synthetic-mac",
        refused: 3,
        ok: 1,
        failed: 0
      });

      // An ordinary push after the reseed still reads as before, unchanged.
      const socket = new CapturingSocket();
      await push(room, socket, "after-reseed");
      const ack = socket.sent.map((f) => decode(f)).find((m) => m.type === MessageType.Ack);
      expect(ack).toMatchObject({ status: UpdateStatusCode.Ok });
    });
  });
});
