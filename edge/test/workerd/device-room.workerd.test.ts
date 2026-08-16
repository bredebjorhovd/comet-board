import { env, evictDurableObject, runInDurableObject } from "cloudflare:test";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DeviceRoom, decodeDeviceFrame, encodeDeviceFrame } from "../../src/device-room";

const USER = "user_workerd";
const HANDLER_OUTCOME_TIMEOUT_MS = 1_000;
const encoder = new TextEncoder();
const decoder = new TextDecoder();

const room = (name: string) => env.DEVICE_ROOM.get(env.DEVICE_ROOM.idFromName(name));

const connect = async (
  stub: DurableObjectStub,
  role: "host" | "client",
  connId: string
): Promise<WebSocket> => {
  const response = await stub.fetch(
    new Request(`https://device.test/ws?role=${role}&connId=${connId}`, {
      headers: { Upgrade: "websocket", "x-comet-auth-user": USER }
    })
  );
  expect(response.status).toBe(101);
  const ws = response.webSocket;
  expect(ws).not.toBeNull();
  ws!.accept();
  return ws!;
};

type ClientMessage = ArrayBuffer | Blob | string;

const nextMessage = (ws: WebSocket): Promise<ClientMessage> =>
  Promise.race([
    new Promise((resolve) =>
      ws.addEventListener("message", (event) => resolve(event.data), { once: true })
    ),
    new Promise<never>((_resolve, reject) =>
      setTimeout(
        () => reject(new Error("socket did not receive handler outcome")),
        HANDLER_OUTCOME_TIMEOUT_MS
      )
    )
  ]);

const nextClose = (ws: WebSocket): Promise<{ code: number; reason: string }> =>
  Promise.race([
    new Promise((resolve) =>
      ws.addEventListener(
        "close",
        (event) => resolve({ code: event.code, reason: event.reason }),
        { once: true }
      )
    ),
    new Promise<never>((_resolve, reject) =>
      setTimeout(
        () => reject(new Error("socket did not close after handler event")),
        HANDLER_OUTCOME_TIMEOUT_MS
      )
    )
  ]);

const messageBytes = async (message: ClientMessage): Promise<Uint8Array> => {
  expect(message).not.toEqual(expect.any(String));
  return new Uint8Array(
    message instanceof Blob ? await message.arrayBuffer() : (message as ArrayBuffer)
  );
};

const relayError = async (message: ClientMessage): Promise<string> => {
  const frame = decodeDeviceFrame(await messageBytes(message));
  return (JSON.parse(decoder.decode(frame.payload)) as { error: string }).error;
};

/** A syntactically valid frame envelope whose JSON header is not an object. */
const nullHeaderFrame = new Uint8Array([4, ...encoder.encode("null")]);

describe("DeviceRoom hibernation handlers", () => {
  afterEach(() => vi.restoreAllMocks());

  it("closes a reloaded socket with a missing attachment without throwing", async () => {
    const warning = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const stub = room("missing-attachment");
    await connect(stub, "host", "host");
    const client = await connect(stub, "client", "client-missing-state");
    await runInDurableObject(stub, (_instance, state) => {
      state.getWebSockets("client:client-missing-state")[0]!.serializeAttachment(null);
    });
    await evictDurableObject(stub);

    const closed = nextClose(client);
    client.send(encodeDeviceFrame({ s: "rpc", k: "rpc" }, new Uint8Array()));

    await expect(closed).resolves.toEqual({ code: 1011, reason: "Socket state error" });
    expect(warning).toHaveBeenCalledWith(
      "DeviceRoom WebSocket event rejected",
      "event=message",
      "reason=invalid_socket_state",
      "cause=none"
    );
  });

  it("rejects a non-object frame header after reload as a protocol error", async () => {
    const warning = vi.spyOn(console, "warn").mockImplementation(() => undefined);
    const stub = room("null-frame-header");
    await connect(stub, "host", "host");
    const client = await connect(stub, "client", "client-null-header");
    await evictDurableObject(stub);

    const closed = nextClose(client);
    client.send(nullHeaderFrame);

    await expect(closed).resolves.toEqual({ code: 1002, reason: "Frame error" });
    expect(warning).toHaveBeenCalledWith(
      "DeviceRoom WebSocket event rejected",
      "event=message",
      "reason=invalid_frame",
      "cause=TypeError"
    );
  });

  it("keeps the original pre-joinedAt attachment shape compatible after reload", async () => {
    const stub = room("legacy-client-attachment");
    const host = await connect(stub, "host", "host");
    const client = await connect(stub, "client", "legacy-client");
    const pong = nextMessage(host);
    host.send("ping");
    await expect(pong).resolves.toBe("pong");
    await runInDurableObject(stub, (_instance, state) => {
      state.getWebSockets("host")[0]!.serializeAttachment({
        userId: USER,
        role: "host",
        connId: "host"
      });
      state.getWebSockets("client:legacy-client")[0]!.serializeAttachment({
        userId: USER,
        role: "client",
        connId: "legacy-client"
      });
    });
    await evictDurableObject(stub);

    const atHost = nextMessage(host);
    client.send(encodeDeviceFrame({ s: "legacy", k: "rpc" }, encoder.encode("request")));
    const frame = decodeDeviceFrame(await messageBytes(await atHost));
    expect(frame.header).toMatchObject({ s: "legacy", k: "rpc", from: "legacy-client" });
    expect(decoder.decode(frame.payload)).toBe("request");
  });

  it("uses socket tags to announce a host close when its attachment is missing", async () => {
    const stub = room("missing-host-close-state");
    const host = await connect(stub, "host", "host");
    const client = await connect(stub, "client", "client-watching-host");
    await runInDurableObject(stub, (_instance, state) => {
      state.getWebSockets("host")[0]!.serializeAttachment(null);
    });
    await evictDurableObject(stub);

    const outcome = nextMessage(client);
    host.close(1000, "test close without attachment");

    expect(await relayError(await outcome)).toBe("host_closed");
  });

  it("does not announce host_closed twice when error and close re-enter", async () => {
    const stub = room("host-error-close-reentry");
    await connect(stub, "host", "host");
    const client = await connect(stub, "client", "client-one-close");
    await evictDurableObject(stub);

    const messages: ClientMessage[] = [];
    const record = (event: MessageEvent) => {
      messages.push(event.data as ClientMessage);
    };
    client.addEventListener("message", record);
    await runInDurableObject(stub, (instance, state) => {
      const host = state.getWebSockets("host")[0]!;
      const deviceRoom = instance as DeviceRoom;
      deviceRoom.webSocketError(host, new Error("synthetic socket failure"));
      deviceRoom.webSocketClose(host);
    });
    await new Promise((resolve) => setTimeout(resolve, 50));
    client.removeEventListener("message", record);

    expect(messages).toHaveLength(1);
    expect(await relayError(messages[0]!)).toBe("host_closed");
  });

  it("still routes relay frames and reports an offline host after reload", async () => {
    const stub = room("routing-after-reload");
    const host = await connect(stub, "host", "host");
    const client = await connect(stub, "client", "client-live");
    await evictDurableObject(stub);

    const atHost = nextMessage(host);
    client.send(encodeDeviceFrame({ s: "rpc-1", k: "rpc" }, encoder.encode("request")));
    const request = decodeDeviceFrame(await messageBytes(await atHost));
    expect(request.header).toMatchObject({ s: "rpc-1", k: "rpc", from: "client-live" });
    expect(decoder.decode(request.payload)).toBe("request");

    const atClient = nextMessage(client);
    host.send(
      encodeDeviceFrame(
        { s: "rpc-1", k: "rpc", to: "client-live" },
        encoder.encode("response")
      )
    );
    const response = decodeDeviceFrame(await messageBytes(await atClient));
    expect(response.header).toEqual({ s: "rpc-1", k: "rpc" });
    expect(decoder.decode(response.payload)).toBe("response");

    host.close(1000, "test host offline");
    expect(await relayError(await nextMessage(client))).toBe("host_closed");

    const offline = nextMessage(client);
    client.send(encodeDeviceFrame({ s: "rpc-2", k: "rpc" }, new Uint8Array()));
    expect(await relayError(await offline)).toBe("host_offline");
  });
});
