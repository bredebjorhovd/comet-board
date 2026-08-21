/**
 * Why a socket died — the room's own account of it (gh#527).
 *
 * THE INCIDENT: 2026-08-19/20, dozens of SessionRoom joins across 22 rooms
 * ended `canceled` with close code 1006 (`wasClean: false`) on the client, and
 * `wrangler tail` over the whole repro window held **no exception, no cap
 * text, nothing**. Plain HTTP was 2xx throughout. Diagnosis was 450 tail
 * events and a guess between two known producers of exactly that signature —
 * the Workers duration cap killing sessions mid-life (gh#126), and the
 * `ctx.abort()` WASM-poisoning escalation (gh#378) — with nothing in the
 * system able to tell them apart after the fact.
 *
 * The reason nothing was logged is structural, and it is the thing this module
 * is built around: **the deaths that matter are the ones that take the
 * invocation with them.** A runtime kill (CPU/duration cap) and `ctx.abort()`
 * both tear the instance down where it stands — no `webSocketClose` runs in
 * that instance, no `console.error` from it is guaranteed to ship, and any
 * storage write not already committed is rolled back. A room can therefore
 * lose every socket it holds and, on its own next wake, have no idea anything
 * happened.
 *
 * So the record is kept the other way round: an accept writes a durable row
 * BEFORE the socket can die, a close/error DELETES it, and the next wake
 * reconciles the surviving rows against the sockets the runtime still lists
 * ([`SocketLedger.reconcile`]). A row with no live socket and no close event
 * is a socket that VANISHED — the exact 1006-with-no-exception shape — and it
 * is counted, attributed to its device, and left on `/stats` for whoever comes
 * looking. Sockets that died before proving a working session are counted
 * separately ([`YOUNG_SOCKET_MS`]): that is churn, and churn is what the
 * engine's "N of N live" gauge could not see.
 *
 * Everything here is pure over an injected sql/meta pair so the rules are
 * testable without a Durable Object (same discipline as `livePresence`).
 */

import type { MetaStore } from "./meta";

/** A socket that died before this long is churn, not a session: it never
 * proved the room works. Matches `HEALTHY_SESSION` in crates/sync/src/room.rs
 * and `ReconnectBackoff.healthySessionNs` on the phone — the same 30s line the
 * clients use to decide whether a session earned a backoff reset, so "died
 * young" here and "did not reset the ladder" there are the same fact. */
export const YOUNG_SOCKET_MS = 30_000;

/** Rolling window the churn rate is reported over. An hour because that is the
 * unit an operator reasons in mid-incident ("is it still happening?"), and
 * because the free-tier duration cap resets on a UTC day — a per-hour rate is
 * what distinguishes a cap that has just been hit from a room that has been
 * dying all day. */
export const CHURN_WINDOW_MS = 60 * 60 * 1000;

/** Deaths retained for the churn rate. 240/hour is far past any real room
 * (the client ladder caps at one dial per 30s per room) and bounds the meta
 * row this rides in. */
const MAX_DEATH_STAMPS = 240;
/** Deaths retained in full detail for `/stats`. Enough to see the shape of a
 * churn burst — who, how old, and what code — without carrying a log. */
const MAX_RECENT_DEATHS = 8;

export type SocketDeathKind =
  /** `webSocketClose` ran: the client or the room closed it, and we have a code. */
  | "close"
  /** `webSocketError` ran: the transport failed under us. */
  | "error"
  /** No handler ran at all — the socket is simply gone from the runtime's list
   * on our next wake. The instance was killed (duration/CPU cap, eviction,
   * `ctx.abort()`), which is what the client sees as 1006. */
  | "vanished";

export interface SocketDeath {
  at: number;
  /** How long the socket lived. `-1` when the accept row predates this deploy
   * and the age is genuinely unknown, so a fake zero cannot be read as churn. */
  ageMs: number;
  kind: SocketDeathKind;
  code?: number;
  reason?: string;
  deviceId?: string;
}

/** The room's socket-lifecycle census, as `/stats` reports it. */
export interface SocketCensus {
  /** Sockets accepted since this room first ran a build with the ledger.
   * DERIVED (deaths + still open) rather than counted: an accept would
   * otherwise cost a meta row of its own on every dial, and a room in a
   * dial/die loop is exactly where extra writes are least affordable
   * (gh#373/gh#377). Every socket ends up in one bucket or the other. */
  accepted: number;
  closed: number;
  errored: number;
  /** Deaths with no handler — the killer that does not log. */
  vanished: number;
  /** Deaths inside [`YOUNG_SOCKET_MS`]. The churn number. */
  diedYoung: number;
  /** Young deaths in the last [`CHURN_WINDOW_MS`] — the rate, not the total. */
  diedYoungLastHour: number;
  /** Accept rows with no close event yet (≈ sockets the room believes it holds). */
  open: number;
  recent: SocketDeath[];
}

interface CensusRow {
  closed: number;
  errored: number;
  vanished: number;
  diedYoung: number;
  /** Timestamps of young deaths, newest last, capped at MAX_DEATH_STAMPS. */
  young: number[];
  recent: SocketDeath[];
}

const EMPTY: CensusRow = {
  closed: 0,
  errored: 0,
  vanished: 0,
  diedYoung: 0,
  young: [],
  recent: []
};

/** Young deaths inside the window ending at `now`. Pure, so the rate the
 * engine and doctor act on can be asserted without a DO. */
export const churnRate = (stamps: readonly number[], now: number): number =>
  stamps.filter((at) => now - at < CHURN_WINDOW_MS).length;

export interface SocketLedger {
  /** Record an accepted socket. Called BEFORE the socket can die — that
   * ordering is the whole design (see the module comment). */
  opened(socketId: string, deviceId: string | undefined, at: number): void;
  /** Record a death for which a handler ran. Returns the death as recorded
   * (with the socket's real age) so the caller can log it, or `undefined`
   * when the socket has no accept row (an older deploy's socket). */
  died(
    socketId: string | undefined,
    kind: Exclude<SocketDeathKind, "vanished">,
    at: number,
    code?: number,
    reason?: string
  ): SocketDeath | undefined;
  /** Reconcile the accept rows against the sockets the runtime still lists.
   * Every row with no live socket died without a handler: the instance was
   * killed under it. Returns those deaths, newest first. */
  reconcile(liveSocketIds: readonly string[], now: number): SocketDeath[];
  census(now: number): SocketCensus;
}

export const createSocketLedger = (sql: SqlStorage, meta: MetaStore): SocketLedger => {
  sql.exec(
    "CREATE TABLE IF NOT EXISTS open_sockets (id TEXT PRIMARY KEY, at INTEGER NOT NULL, device TEXT)"
  );

  const read = (): CensusRow => {
    const raw = meta.get("socketCensus");
    if (!raw) return { ...EMPTY, young: [], recent: [] };
    try {
      const parsed = JSON.parse(raw) as Partial<CensusRow>;
      return {
        closed: parsed.closed ?? 0,
        errored: parsed.errored ?? 0,
        vanished: parsed.vanished ?? 0,
        diedYoung: parsed.diedYoung ?? 0,
        young: Array.isArray(parsed.young) ? parsed.young : [],
        recent: Array.isArray(parsed.recent) ? parsed.recent : []
      };
    } catch {
      // A census that cannot be parsed is a diagnostic, not state anything
      // depends on: start a fresh one rather than throwing inside a close
      // handler, which is precisely where an unhandled throw is invisible.
      return { ...EMPTY, young: [], recent: [] };
    }
  };

  // One meta key for the whole census, deliberately: `meta.set` writes only on
  // a change, and a death changes every counter at once — six keys would bill
  // six rows per death for one fact (the gh#377 shape).
  const write = (row: CensusRow): void => meta.set("socketCensus", JSON.stringify(row));

  const note = (row: CensusRow, death: SocketDeath): CensusRow => {
    if (death.kind === "close") row.closed++;
    else if (death.kind === "error") row.errored++;
    else row.vanished++;
    // An unknown age (-1) is never counted as churn: a socket from before this
    // deploy would otherwise read as a 0ms death and invent a burst.
    if (death.ageMs >= 0 && death.ageMs < YOUNG_SOCKET_MS) {
      row.diedYoung++;
      row.young.push(death.at);
      if (row.young.length > MAX_DEATH_STAMPS) row.young.splice(0, row.young.length - MAX_DEATH_STAMPS);
    }
    row.recent.unshift(death);
    if (row.recent.length > MAX_RECENT_DEATHS) row.recent.length = MAX_RECENT_DEATHS;
    return row;
  };

  return {
    opened(socketId, deviceId, at) {
      sql.exec(
        "INSERT INTO open_sockets (id, at, device) VALUES (?, ?, ?) ON CONFLICT(id) DO UPDATE SET at = excluded.at, device = excluded.device",
        socketId,
        at,
        deviceId ?? null
      );
    },

    died(socketId, kind, at, code, reason) {
      if (!socketId) return undefined;
      const found = [...sql.exec("SELECT at, device FROM open_sockets WHERE id = ?", socketId)][0];
      if (!found) return undefined;
      sql.exec("DELETE FROM open_sockets WHERE id = ?", socketId);
      const openedAt = Number(found.at ?? 0);
      const death: SocketDeath = {
        at,
        ageMs: openedAt > 0 ? Math.max(at - openedAt, 0) : -1,
        kind,
        ...(code === undefined ? {} : { code }),
        ...(reason ? { reason: reason.slice(0, 120) } : {}),
        ...(found.device ? { deviceId: String(found.device) } : {})
      };
      write(note(read(), death));
      return death;
    },

    reconcile(liveSocketIds, now) {
      const live = new Set(liveSocketIds);
      const stale = [...sql.exec("SELECT id, at, device FROM open_sockets")].filter(
        (row) => !live.has(String(row.id))
      );
      if (stale.length === 0) return [];
      let row = read();
      const deaths: SocketDeath[] = [];
      for (const entry of stale) {
        sql.exec("DELETE FROM open_sockets WHERE id = ?", String(entry.id));
        const openedAt = Number(entry.at ?? 0);
        const death: SocketDeath = {
          // The death itself was not observed, so `at` is when we NOTICED. The
          // age is measured to the same instant, which makes it an upper bound
          // on the socket's real life — deliberately conservative: it can only
          // ever under-report churn, never invent it.
          at: now,
          ageMs: openedAt > 0 ? Math.max(now - openedAt, 0) : -1,
          kind: "vanished",
          ...(entry.device ? { deviceId: String(entry.device) } : {})
        };
        row = note(row, death);
        deaths.push(death);
      }
      write(row);
      return deaths;
    },

    census(now) {
      const row = read();
      const open = Number(
        [...sql.exec("SELECT COUNT(*) AS n FROM open_sockets")][0]?.n ?? 0
      );
      return {
        accepted: row.closed + row.errored + row.vanished + open,
        closed: row.closed,
        errored: row.errored,
        vanished: row.vanished,
        diedYoung: row.diedYoung,
        diedYoungLastHour: churnRate(row.young, now),
        open,
        recent: row.recent
      };
    }
  };
};
