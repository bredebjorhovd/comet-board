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
//
// gh#554 — and the predicate must be about LOADING, not about SIZE. The guard
// above shipped size-only, so during the very incident it was written for it
// recorded ZERO refusals: the room was 1.67MB against a 32MB threshold, its
// stored state was corrupt rather than large, and the corruption was routed to
// the wasm-poisoning tripwire, which `ctx.abort()`ed the instance on every wake
// and healed nothing (the poison is in the persisted log; the next cold start
// replays it). The cases below are that gap — corruption small enough to sail
// past the size line, in the log as well as the snapshot, and a `RangeError`
// out of the import that must be read as sick STORED STATE rather than as a
// sick heap.

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
import {
  ALARM_FAILURE_LIMIT,
  ALARM_REFUSAL_BUDGET,
  LOAD_REFUSAL_LIMIT,
  MAX_DOC_LOAD_BYTES,
  type SessionRoom
} from "../../src/session-room";

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

/** The proportions of the 2026-08-19 room: 1.67MB stored, ~5% of the guard.
 * Written as the recorded log size because that is what the size predicate
 * reads — the point of these cases is that it reads FAR below the line and the
 * room is unloadable anyway. */
const INCIDENT_STORED_BYTES = 1_749_030;

/** An update row that is not a Loro update. The shape a truncated write, a
 * half-flushed chunk, or an incompatible encoding leaves in the log. */
const corruptBytes = (fill: number): Uint8Array => new Uint8Array(512).fill(fill);

const insertUpdateRow = (state: DurableObjectState, bytes: Uint8Array): void => {
  state.storage.sql.exec(
    "INSERT INTO updates (bytes, received_at, cont) VALUES (?, ?, 0)",
    bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength),
    Date.now()
  );
};

const setMeta = (state: DurableObjectState, key: string, value: string): void => {
  state.storage.sql.exec("INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)", key, value);
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

  // ── gh#554: the corruption that is SMALL ─────────────────────────────────

  it("refuses a stored log that will not import, however small it is", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("corrupt-small-log"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      // No snapshot, a log that will not replay, and a size the guard is happy
      // with — the incident's exact shape. Before gh#554 every row failed
      // SILENTLY (the replay loop swallowed each one), the room served a hollow
      // doc, and nothing anywhere counted a refusal.
      insertUpdateRow(state, corruptBytes(0x7f));
      insertUpdateRow(state, corruptBytes(0x3c));
      setMeta(state, "updateBytes", String(INCIDENT_STORED_BYTES));

      for (let attempt = 1; attempt <= LOAD_REFUSAL_LIMIT; attempt++) {
        const frames = await join(room);
        expect(frames).toHaveLength(1);
        const error = frames[0]!;
        expect(error.type).toBe(MessageType.JoinError);
        if (error.type === MessageType.JoinError) {
          expect(error.code).toBe(JoinErrorCode.AppError);
          expect(error.message).toContain("stored log will not import");
        }
      }

      const refusing = await stats(room);
      const docLoad = refusing.docLoad as Record<string, unknown>;
      // THE READING THE INCIDENT NEVER GOT: a refusal recorded against a doc
      // two orders of magnitude below the size threshold.
      expect((docLoad.lastRefusal as { detail: string }).detail).toContain(
        "stored log will not import"
      );
      expect(docLoad.storedBytes as number).toBeLessThan(MAX_DOC_LOAD_BYTES);
      // And no abort: the isolate was never the problem, so it was never the
      // remedy. (An abort here would take the test process with it.)
      expect(refusing.lastAbort).toBeNull();

      // Evicted and reseedable, exactly like the oversized case.
      expect(refusing.postReset).toBe(true);
      const remaining = [...state.storage.sql.exec("SELECT COUNT(*) AS n FROM updates")][0]?.n;
      expect(Number(remaining)).toBe(0);

      const healed = await join(room);
      expect(healed.some((frame) => frame.type === MessageType.JoinResponseOk)).toBe(true);
      expect(healed.some((frame) => frame.type === MessageType.JoinError)).toBe(false);
    });
  });

  it("keeps skipping a bad row behind a snapshot that loaded", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("bad-row-good-snapshot"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      const doc = new LoroDoc();
      let snapshot: Uint8Array;
      try {
        doc.getMap("session").set("title", "history worth keeping");
        doc.commit();
        snapshot = doc.export({ mode: "snapshot" });
      } finally {
        doc.free();
      }
      state.storage.sql.exec(
        "INSERT INTO blobs (name, idx, bytes) VALUES ('snapshot', 0, ?)",
        snapshot.buffer.slice(snapshot.byteOffset, snapshot.byteOffset + snapshot.byteLength)
      );
      insertUpdateRow(state, corruptBytes(0x7f));

      // The counterweight to the case above, and the reason it is narrow: the
      // doc still holds the room's history, the next fold rewrites the log out
      // of it, and evicting here would spend a good snapshot on a bad tail.
      const frames = await join(room);
      expect(frames.some((frame) => frame.type === MessageType.JoinResponseOk)).toBe(true);
      expect(frames.some((frame) => frame.type === MessageType.JoinError)).toBe(false);
      const reported = await stats(room);
      expect((reported.docLoad as { refusals: number }).refusals).toBe(0);
      expect(reported.snapshotBytes).toBe(snapshot.byteLength);
    });
  });

  it("reads a RangeError out of the stored snapshot as sick state, not a sick heap", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("rangeerror-snapshot"));
    // The literal error the 2026-08-19 room died on. Its SHAPE is the wasm
    // poisoning signature and its CAUSE was a snapshot that would not decode;
    // the guard must tell them apart by asking the heap, not by reading the
    // error. Injected because a heap genuinely out of memory cannot be staged
    // in a test — and because the shape is the whole point.
    const poisonFor = corruptBytes(0x5a);
    const realImport = LoroDoc.prototype.import;
    LoroDoc.prototype.import = function patched(this: LoroDoc, bytes: Uint8Array) {
      if (bytes.length === poisonFor.length && bytes.every((b, i) => b === poisonFor[i])) {
        throw new RangeError("Invalid array buffer length");
      }
      return realImport.call(this, bytes);
    } as typeof LoroDoc.prototype.import;

    try {
      await runInDurableObject(stub, async (instance, state) => {
        const room = instance as unknown as SessionRoom;
        state.storage.sql.exec(
          "INSERT INTO blobs (name, idx, bytes) VALUES ('snapshot', 0, ?)",
          poisonFor.buffer.slice(0, poisonFor.byteLength)
        );

        for (let attempt = 1; attempt <= LOAD_REFUSAL_LIMIT; attempt++) {
          const frames = await join(room);
          expect(frames).toHaveLength(1);
          const error = frames[0]!;
          expect(error.type).toBe(MessageType.JoinError);
          if (error.type === MessageType.JoinError) {
            expect(error.message).toContain("snapshot will not import");
          }
        }

        const refusing = await stats(room);
        // The split, asserted from both sides: the load guard owns it...
        expect((refusing.docLoad as { lastRefusal: unknown }).lastRefusal).not.toBeNull();
        // ...and the poisoning tripwire does not. Three RangeErrors is exactly
        // WASM_POISON_ABORT_AFTER; before gh#554 this room had aborted itself
        // by now and every attached socket would have read 1006.
        expect(refusing.lastAbort).toBeNull();
        expect(refusing.heapSpared).toBe(0);
        // Evicted, so the fleet reseeds instead of replaying the poison.
        expect(refusing.postReset).toBe(true);
        expect(refusing.snapshotBytes).toBe(0);
      });
    } finally {
      LoroDoc.prototype.import = realImport;
    }
  });

  it("does not let a load refusal spend the alarm's give-up budget", async () => {
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("alarm-refusal"));

    await runInDurableObject(stub, async (instance, state) => {
      const room = instance as unknown as SessionRoom;
      insertUpdateRow(state, corruptBytes(0x7f));
      setMeta(state, "updateBytes", String(INCIDENT_STORED_BYTES));
      setMeta(state, "backupDirty", "1");
      // No join in this case — the alarm is the wake with no client behind it,
      // which is exactly the window the give-up would have happened in — so the
      // owner is stamped directly to keep /stats readable.
      setMeta(state, "owner", USER_ID);
      // Where the incident's room actually sat: three attempts from giving up
      // on its own trim, snapshot and backup FOREVER, against a doc the guard
      // could have reseeded in three minutes.
      const brink = ALARM_FAILURE_LIMIT - 1;
      setMeta(state, "alarmFailures", String(brink));

      await room.alarm();
      const held = await stats(room);
      const alarm = held.alarm as Record<string, unknown>;
      expect(alarm.consecutiveFailures).toBe(brink); // not spent
      expect(alarm.gaveUp).toBe(false);
      expect(alarm.refusals).toBe(1);
      expect(alarm.refusalBudget).toBe(ALARM_REFUSAL_BUDGET);

      // Keep arriving until the guard evicts — which is the point of not
      // spending the budget — and then the chain completes and clears itself.
      for (let attempt = 2; attempt <= LOAD_REFUSAL_LIMIT + 1; attempt++) await room.alarm();
      const healed = await stats(room);
      const healedAlarm = healed.alarm as Record<string, unknown>;
      expect(healed.postReset).toBe(true);
      expect(healedAlarm.gaveUp).toBe(false);
      expect(healedAlarm.consecutiveFailures).toBe(0);
      expect(healedAlarm.refusals).toBe(0);
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
