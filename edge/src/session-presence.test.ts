import { describe, expect, it } from "vitest";
import { livePresence } from "./session-room";

// gh#145. Presence used to be a 15s `%EPH` write from every engine into both
// the workspace room and the org registry. A `%EPH` frame is a real message, so
// it wakes the Durable Object: those two rooms never hibernated, and one object
// awake around the clock is 10,800 GB-s/day — 83% of the daily free tier with
// zero agents running. The cure is the one `DeviceRoom` already used: derive
// liveness from the socket set, where the runtime stamps auto-pong timestamps
// while hibernating, for free.
//
// These are the rules that replaced the beat.
describe("session-room derived presence", () => {
  const NOW = 1_000_000_000_000;
  const fresh = NOW - 10_000; // pinged 10s ago
  const corpse = NOW - 10 * 60_000; // silent for 10 minutes

  it("reports the device behind a live socket", () => {
    expect(livePresence([{ deviceId: "box", lastSeenAt: fresh }], NOW)).toEqual({
      box: fresh
    });
  });

  it("reports nothing for a room with no sockets", () => {
    expect(livePresence([], NOW)).toEqual({});
  });

  // The failure this replaces: a laptop lid closing leaves a socket the runtime
  // never closes. Without the staleness check the box would read online forever
  // — and nobody is awake to notice, because nothing beats any more.
  it("drops a socket whose uplink died silently", () => {
    expect(livePresence([{ deviceId: "box", lastSeenAt: corpse }], NOW)).toEqual({});
  });

  it("keeps the window at 75s — 2.5 of the slowest ping cadence in the fleet", () => {
    expect(livePresence([{ deviceId: "box", lastSeenAt: NOW - 75_000 }], NOW)).toEqual({
      box: NOW - 75_000
    });
    expect(livePresence([{ deviceId: "box", lastSeenAt: NOW - 76_000 }], NOW)).toEqual({});
  });

  // A redial that overlaps its predecessor leaves two sockets for one device.
  // Reading the corpse would report the device offline while it is right there.
  it("takes the freshest socket when a device holds several", () => {
    expect(
      livePresence(
        [
          { deviceId: "box", lastSeenAt: corpse },
          { deviceId: "box", lastSeenAt: fresh }
        ],
        NOW
      )
    ).toEqual({ box: fresh });
  });

  it("keeps a socket that just joined and has not pinged yet", () => {
    expect(livePresence([{ deviceId: "box", lastSeenAt: NOW }], NOW)).toEqual({ box: NOW });
  });

  // Engines older than gh#145 (and browser clients) attach without a device id.
  // They contribute no presence rather than being guessed at — they still beat
  // their own key in over `%EPH`, which the room relays untouched.
  it("ignores a socket that names no device", () => {
    expect(livePresence([{ lastSeenAt: fresh }], NOW)).toEqual({});
  });

  it("reports every device in a multi-device room", () => {
    expect(
      livePresence(
        [
          { deviceId: "box", lastSeenAt: fresh },
          { deviceId: "laptop", lastSeenAt: NOW - 1_000 },
          { deviceId: "old-phone", lastSeenAt: corpse }
        ],
        NOW
      )
    ).toEqual({ box: fresh, laptop: NOW - 1_000 });
  });
});
