import { describe, expect, it } from "vitest";
import { decodeDeviceFrame, encodeDeviceFrame, relayedHeader } from "./device-room";

describe("device frame codec", () => {
  it("round-trips header + payload", () => {
    const payload = new Uint8Array([1, 2, 3, 250, 255]);
    const frame = encodeDeviceFrame({ s: "term-42", k: "term", to: "conn-9" }, payload);
    const decoded = decodeDeviceFrame(frame);
    expect(decoded.header).toEqual({ s: "term-42", k: "term", to: "conn-9" });
    expect([...decoded.payload]).toEqual([...payload]);
  });

  it("handles empty payloads and long headers", () => {
    const header = { s: "x".repeat(200), k: "rpc", from: "conn-1" };
    const decoded = decodeDeviceFrame(encodeDeviceFrame(header, new Uint8Array()));
    expect(decoded.header).toEqual(header);
    expect(decoded.payload.length).toBe(0);
  });

  // Key order is the contract with `crates/rpc/src/device_room.rs`, which
  // asserts these exact bytes from the other side.
  it("writes the verified caller after the routing keys", () => {
    const frame = encodeDeviceFrame(
      { s: "rpc", k: "rpc", from: "c1", u: "user_01", o: "org_01" },
      new TextEncoder().encode("{}")
    );
    const json = new TextDecoder().decode(frame.slice(1, 1 + frame[0]!));
    expect(json).toBe('{"s":"rpc","k":"rpc","from":"c1","u":"user_01","o":"org_01"}');
  });
});

/** gh#161: the identity on a relayed frame is the relay's, not the sender's. */
describe("relayed frame headers", () => {
  const conn = { connId: "c1", userId: "user_alice", orgId: "org_1" };

  it("stamps the socket's verified identity beside `from`", () => {
    expect(relayedHeader({ s: "rpc", k: "rpc" }, conn)).toEqual({
      s: "rpc",
      k: "rpc",
      from: "c1",
      u: "user_alice",
      o: "org_1"
    });
  });

  it("drops an identity the client wrote itself", () => {
    // The whole point: a frontend that puts somebody else in the header (or
    // routes the frame at the host's clients) gets its keys thrown away and
    // the truth stamped over them.
    const forged = { s: "rpc", k: "rpc", u: "user_owner", o: "org_2", to: "c9", from: "c9" };
    expect(relayedHeader(forged, conn)).toEqual({
      s: "rpc",
      k: "rpc",
      from: "c1",
      u: "user_alice",
      o: "org_1"
    });
  });

  it("omits an org nobody's session carried", () => {
    const header = relayedHeader({ s: "rpc", k: "rpc" }, { connId: "c1", userId: "user_alice" });
    expect(header).toEqual({ s: "rpc", k: "rpc", from: "c1", u: "user_alice" });
    expect("o" in header).toBe(false);
  });
});
