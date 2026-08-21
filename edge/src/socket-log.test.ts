// gh#527 — the edge must be able to say why a socket died.
//
// The incident these rules are written against: 22 rooms' worth of sockets
// ending 1006 mid-life with NOTHING in the tail. The reason nothing was in the
// tail is that the deaths took their invocations with them, so the only record
// that can survive is one written before the death and reconciled after it.
// These tests pin that: an accept row is written first, an observed death
// deletes it and is attributed, and a row that outlives its socket with no
// close event is counted as VANISHED — which is the fact the whole diagnosis
// was missing.

/// <reference types="node" />
import { DatabaseSync } from "node:sqlite";
import { describe, expect, it } from "vitest";
import { createMetaStore } from "./meta";
import {
  CHURN_WINDOW_MS,
  YOUNG_SOCKET_MS,
  churnRate,
  createSocketLedger,
  type SocketLedger
} from "./socket-log";

class FakeSql {
  private readonly db = new DatabaseSync(":memory:");

  exec(query: string, ...params: unknown[]): Iterable<Record<string, unknown>> {
    const bound = params.map((p) => (p instanceof ArrayBuffer ? new Uint8Array(p) : p));
    const stmt = this.db.prepare(query);
    if (/^\s*SELECT/i.test(query)) {
      return stmt.all(...(bound as never[])) as unknown as Record<string, unknown>[];
    }
    stmt.run(...(bound as never[]));
    return [];
  }
}

const ledger = (): SocketLedger => {
  const sql = new FakeSql() as unknown as SqlStorage;
  return createSocketLedger(sql, createMetaStore(sql));
};

describe("socket ledger", () => {
  it("attributes an observed close with its code, reason, age and device", () => {
    const log = ledger();
    const t0 = 1_700_000_000_000;
    log.opened("sock-a", "phone", t0);

    const death = log.died("sock-a", "close", t0 + 4_000, 1011, "broadcast delivery failed");

    expect(death).toMatchObject({
      kind: "close",
      code: 1011,
      reason: "broadcast delivery failed",
      deviceId: "phone",
      ageMs: 4_000
    });
    const census = log.census(t0 + 5_000);
    expect(census).toMatchObject({ accepted: 1, closed: 1, vanished: 0, open: 0, diedYoung: 1 });
  });

  it("counts a socket that outlived its instance as VANISHED — the killer that logs nothing", () => {
    const log = ledger();
    const t0 = 1_700_000_000_000;
    log.opened("sock-a", "phone", t0);
    log.opened("sock-b", "work-metal", t0 + 1_000);

    // The instance was killed: no close handler ran for either socket, and on
    // the next wake the runtime lists neither.
    const deaths = log.reconcile([], t0 + 9_000);

    expect(deaths).toHaveLength(2);
    expect(deaths.map((d) => d.kind)).toEqual(["vanished", "vanished"]);
    expect(new Set(deaths.map((d) => d.deviceId))).toEqual(new Set(["phone", "work-metal"]));
    const census = log.census(t0 + 9_000);
    expect(census).toMatchObject({ accepted: 2, closed: 0, vanished: 2, open: 0, diedYoung: 2 });
    // And the census does not double-count: a second reconcile has nothing
    // left to find, or a room that woke twice would report a phantom storm.
    expect(log.reconcile([], t0 + 12_000)).toHaveLength(0);
    expect(log.census(t0 + 12_000).vanished).toBe(2);
  });

  it("leaves sockets the runtime still lists alone", () => {
    const log = ledger();
    const t0 = 1_700_000_000_000;
    log.opened("sock-live", "phone", t0);
    log.opened("sock-dead", "phone", t0);

    const deaths = log.reconcile(["sock-live"], t0 + 60_000);

    expect(deaths).toHaveLength(1);
    expect(log.census(t0 + 60_000)).toMatchObject({ open: 1, vanished: 1 });
  });

  it("separates churn from an ordinary long-lived hang-up", () => {
    const log = ledger();
    const t0 = 1_700_000_000_000;
    log.opened("long", "phone", t0);
    log.died("long", "close", t0 + 10 * 60_000, 1000, "going away");
    log.opened("short", "phone", t0);
    log.died("short", "close", t0 + YOUNG_SOCKET_MS - 1, 1006);

    const census = log.census(t0 + 10 * 60_000);
    expect(census.closed).toBe(2);
    // Only the one that never proved a working session is churn.
    expect(census.diedYoung).toBe(1);
    expect(census.diedYoungLastHour).toBe(1);
  });

  it("reports churn as a RATE, so a room that healed hours ago reads healthy", () => {
    const now = 1_700_000_000_000;
    const stamps = [now - CHURN_WINDOW_MS - 1, now - CHURN_WINDOW_MS - 5_000, now - 60_000, now];
    expect(churnRate(stamps, now)).toBe(2);
    expect(churnRate([], now)).toBe(0);
  });

  it("does not invent churn for a socket whose accept predates the ledger", () => {
    const log = ledger();
    // No accept row: a socket attached by an older deploy. It contributes
    // nothing rather than reading as a 0ms death.
    expect(log.died("unknown-socket", "close", 1_700_000_000_000, 1006)).toBeUndefined();
    expect(log.census(1_700_000_000_000)).toMatchObject({ closed: 0, diedYoung: 0, accepted: 0 });
  });

  it("bounds what it retains: the detail list is a shape, not a log", () => {
    const log = ledger();
    const t0 = 1_700_000_000_000;
    for (let i = 0; i < 40; i++) {
      log.opened(`sock-${i}`, "phone", t0 + i);
      log.died(`sock-${i}`, "close", t0 + i + 10, 1006);
    }
    const census = log.census(t0 + 1_000);
    expect(census.closed).toBe(40);
    expect(census.diedYoung).toBe(40);
    expect(census.recent.length).toBeLessThanOrEqual(8);
    // Newest first — a mid-incident reader wants the last death, not the first.
    expect(census.recent[0]?.at).toBe(t0 + 39 + 10);
  });
});
