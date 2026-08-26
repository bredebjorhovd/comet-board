import { env, runInDurableObject } from "cloudflare:test";
import { LoroDoc, VersionVector } from "loro-crdt";
import {
  CrdtType,
  MessageType,
  RoomErrorCode,
  decode,
  encode,
  type ProtocolMessage
} from "loro-protocol";
import { describe, expect, it } from "vitest";
import { AUTH_USER_HEADER, ROOM_KIND_HEADER } from "../../src/env";
import { BACKFILL_OP_BUDGET, type SessionRoom } from "../../src/session-room";

const ROOM_ID = "ws4/synthetic-org/synthetic-user";
const USER_ID = "synthetic-user";

class CapturingSocket {
  private attachment: unknown = {
    userId: USER_ID,
    rooms: [],
    workspace: true,
    deviceId: "synthetic-phone",
    joinedAt: Date.now()
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

/** A workspace-shaped history with several devices taking turns after
 * observing one another. Each round therefore depends on the previous
 * devices' changes; a naive peer-span slice produces pending, non-convergent
 * updates on the second event. */
const causallyComplexWorkspace = (): {
  snapshot: Uint8Array;
  json: unknown;
  peers: number;
} => {
  const devices = Array.from({ length: 6 }, (_, index) => {
    const doc = new LoroDoc();
    doc.setPeerId(index + 1);
    return doc;
  });
  try {
    for (let round = 0; round < 12; round++) {
      for (let device = 0; device < devices.length; device++) {
        const writer = devices[device]!;
        const workspace = writer.getMap("workspace");
        for (let row = 0; row < 64; row++) {
          workspace.set(
            `device-${device}/session-${round}-${row}`,
            `${round}:${device}:${row}:${"state".repeat(8)}`
          );
        }
        writer.commit();
        const update = writer.export({ mode: "update" });
        for (const reader of devices) {
          if (reader !== writer) reader.import(update);
        }
      }
    }
    const source = devices[0]!;
    const version = source.version();
    try {
      return {
        snapshot: source.export({ mode: "snapshot" }),
        json: source.toJSON(),
        peers: version.length()
      };
    } finally {
      version.free();
    }
  } finally {
    for (const doc of devices) doc.free();
  }
};

const messageBuffer = (message: ProtocolMessage): ArrayBuffer => {
  const bytes = encode(message);
  return bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + bytes.byteLength) as ArrayBuffer;
};

const applyBackfill = (
  client: LoroDoc,
  frames: Uint8Array[]
): { serverVersion: Uint8Array; continuation: boolean } => {
  let serverVersion: Uint8Array | undefined;
  let continuation = false;
  const fragments = new Map<
    string,
    { parts: Array<Uint8Array | undefined>; totalSize: number }
  >();
  for (const frame of frames) {
    const message = decode(frame);
    if (message.type === MessageType.JoinResponseOk) {
      serverVersion = message.version;
    } else if (message.type === MessageType.DocUpdate) {
      for (const update of message.updates) client.import(update);
    } else if (message.type === MessageType.DocUpdateFragmentHeader) {
      fragments.set(message.batchId, {
        parts: Array.from({ length: message.fragmentCount }),
        totalSize: message.totalSizeBytes
      });
    } else if (message.type === MessageType.DocUpdateFragment) {
      const batch = fragments.get(message.batchId);
      if (!batch) throw new Error(`fragment without header: ${message.batchId}`);
      batch.parts[message.index] = message.fragment;
      if (batch.parts.every((part) => part !== undefined)) {
        const update = new Uint8Array(batch.totalSize);
        let offset = 0;
        for (const part of batch.parts as Uint8Array[]) {
          update.set(part, offset);
          offset += part.length;
        }
        client.import(update);
        fragments.delete(message.batchId);
      }
    } else if (
      message.type === MessageType.RoomError &&
      message.code === RoomErrorCode.RejoinSuggested
    ) {
      continuation = true;
    }
  }
  expect(fragments.size).toBe(0);
  if (!serverVersion) throw new Error("join did not return a server version");
  return { serverVersion, continuation };
};

describe("SessionRoom backfill on real workerd", () => {
  // Six Loro docs in causal rounds against real workerd: ~4s of test time on
  // an idle box, past vitest's 5s default the moment anything else runs (the
  // wide pass, gh#386, watched it flake under a parallel cargo build). The
  // margin was the bug; the budget below is room for a loaded machine.
  it("bounds one causally complex workspace join and converges over continuations", { timeout: 30_000 }, async () => {
    const source = causallyComplexWorkspace();
    expect(source.peers).toBe(6);
    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("complex-backfill"));

    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const append = await room.fetch(
        new Request("https://room.invalid/append", {
          method: "POST",
          headers: {
            [AUTH_USER_HEADER]: USER_ID,
            [ROOM_KIND_HEADER]: "workspace"
          },
          body: source.snapshot.buffer.slice(
            source.snapshot.byteOffset,
            source.snapshot.byteOffset + source.snapshot.byteLength
          )
        })
      );
      expect(append.status).toBe(200);

      const client = new LoroDoc();
      try {
        let turns = 0;
        let converged = false;
        while (turns < 32 && !converged) {
          turns++;
          const version = client.version();
          let versionBytes: Uint8Array;
          let beforeOps: number;
          try {
            versionBytes = version.encode();
            beforeOps = [...version.toJSON().values()].reduce((sum, counter) => sum + counter, 0);
          } finally {
            version.free();
          }
          const socket = new CapturingSocket();
          await room.webSocketMessage(
            socket as unknown as WebSocket,
            messageBuffer({
              type: MessageType.JoinRequest,
              crdt: CrdtType.Loro,
              roomId: ROOM_ID,
              auth: new Uint8Array(),
              version: versionBytes
            })
          );
          const { serverVersion, continuation } = applyBackfill(client, socket.sent);
          const clientVersion = client.version();
          const decodedServer = VersionVector.decode(serverVersion);
          try {
            const afterOps = [...clientVersion.toJSON().values()].reduce(
              (sum, counter) => sum + counter,
              0
            );
            expect(afterOps - beforeOps).toBeLessThanOrEqual(BACKFILL_OP_BUDGET);
            converged = clientVersion.compare(decodedServer) === 0;
          } finally {
            clientVersion.free();
            decodedServer.free();
          }

          if (!converged) {
            expect(continuation).toBe(true);
            expect(socket.closedWith).toBeUndefined();
          } else {
            expect(continuation).toBe(false);
          }
          if (turns === 1) {
            // The unfixed handler exports and sends the complete document in
            // this first event, so this is the red assertion.
            expect(converged).toBe(false);
          }
        }
        expect(converged).toBe(true);
        expect(turns).toBeGreaterThan(1);
        expect(client.toJSON()).toEqual(source.json);
      } finally {
        client.free();
      }
    });
  });

  it("keeps an ordinary workspace join to one event", async () => {
    const source = new LoroDoc();
    source.setPeerId(77);
    source.getMap("workspace").set("chat/synthetic", "ready");
    source.commit();
    const expected = source.toJSON();
    const snapshot = source.export({ mode: "snapshot" });
    source.free();

    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("small-backfill"));
    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const append = await room.fetch(
        new Request("https://room.invalid/append", {
          method: "POST",
          headers: {
            [AUTH_USER_HEADER]: USER_ID,
            [ROOM_KIND_HEADER]: "workspace"
          },
          body: snapshot.buffer.slice(snapshot.byteOffset, snapshot.byteOffset + snapshot.byteLength)
        })
      );
      expect(append.status).toBe(200);

      const client = new LoroDoc();
      const empty = client.version();
      const socket = new CapturingSocket();
      try {
        await room.webSocketMessage(
          socket as unknown as WebSocket,
          messageBuffer({
            type: MessageType.JoinRequest,
            crdt: CrdtType.Loro,
            roomId: ROOM_ID,
            auth: new Uint8Array(),
            version: empty.encode()
          })
        );
        const { serverVersion, continuation } = applyBackfill(client, socket.sent);
        const server = VersionVector.decode(serverVersion);
        const current = client.version();
        try {
          expect(current.compare(server)).toBe(0);
        } finally {
          current.free();
          server.free();
        }
        expect(continuation).toBe(false);
        expect(socket.closedWith).toBeUndefined();
        expect(client.toJSON()).toEqual(expected);
      } finally {
        empty.free();
        client.free();
      }
    });
  });

  it("keeps a stale shallow-peer recovery atomic", async () => {
    const source = new LoroDoc();
    source.setPeerId(88);
    const staleVersion = source.version();
    let staleVersionBytes: Uint8Array;
    try {
      staleVersionBytes = staleVersion.encode();
    } finally {
      staleVersion.free();
    }
    const workspace = source.getMap("workspace");
    workspace.set("chat/old", "kept");
    source.commit();
    workspace.set("chat/new", "also kept");
    source.commit();
    const expected = source.toJSON();
    const shallow = source.export({ mode: "shallow-snapshot", frontiers: source.frontiers() });
    source.free();

    const stub = env.TEST_SESSION.get(env.TEST_SESSION.idFromName("shallow-recovery"));
    await runInDurableObject(stub, async (instance) => {
      const room = instance as unknown as SessionRoom;
      const append = await room.fetch(
        new Request("https://room.invalid/append", {
          method: "POST",
          headers: {
            [AUTH_USER_HEADER]: USER_ID,
            [ROOM_KIND_HEADER]: "workspace"
          },
          body: shallow.buffer.slice(shallow.byteOffset, shallow.byteOffset + shallow.byteLength)
        })
      );
      expect(append.status).toBe(200);

      const replacement = new LoroDoc();
      const socket = new CapturingSocket();
      try {
        await room.webSocketMessage(
          socket as unknown as WebSocket,
          messageBuffer({
            type: MessageType.JoinRequest,
            crdt: CrdtType.Loro,
            roomId: ROOM_ID,
            auth: new Uint8Array(),
            version: staleVersionBytes
          })
        );
        const { serverVersion, continuation } = applyBackfill(replacement, socket.sent);
        const server = VersionVector.decode(serverVersion);
        const current = replacement.version();
        try {
          expect(current.compare(server)).toBe(0);
        } finally {
          current.free();
          server.free();
        }
        expect(replacement.isShallow()).toBe(true);
        expect(replacement.toJSON()).toEqual(expected);
        expect(continuation).toBe(false);
        expect(socket.closedWith).toBeUndefined();
      } finally {
        replacement.free();
      }
    });
  });
});
