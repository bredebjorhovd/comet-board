/**
 * Chunked update-log rows. Durable Object SQL caps individual values at ~2MB
 * (the cap blobs.ts chunks snapshots around) — but a single reassembled client
 * update can exceed it: a bulk session import or a host re-uploading a long
 * session's full missing span arrives as ONE Loro update of arbitrary size.
 * Before chunking, that INSERT threw SQLITE_TOOBIG on every flush forever
 * while the update had already been imported, acked Ok, and relayed — the room
 * stayed live but stopped persisting, and every hibernation silently reverted
 * it to stale state (the 2026-08-05 "newest messages no longer sync" bug).
 *
 * An update wider than CHUNK_BYTES is stored as consecutive rows, the first
 * with cont=0 and the rest with cont=1; replay reassembles by concatenating
 * each cont=0 row with its cont=1 followers. Rows written before the `cont`
 * column existed read back as cont=0 (ALTER's DEFAULT), i.e. one update per
 * row — exactly what they were.
 */
import { CHUNK_BYTES } from "./blobs";
import { FOLD_STEP_BYTES, FOLD_STEP_ROWS } from "./session-doc/constants";

/** Create the log table (or add `cont` to a pre-chunking one). */
export const ensureUpdateLog = (sql: SqlStorage): void => {
  sql.exec(
    "CREATE TABLE IF NOT EXISTS updates (seq INTEGER PRIMARY KEY AUTOINCREMENT, bytes BLOB NOT NULL, received_at INTEGER NOT NULL, cont INTEGER NOT NULL DEFAULT 0)"
  );
  try {
    sql.exec("ALTER TABLE updates ADD COLUMN cont INTEGER NOT NULL DEFAULT 0");
  } catch {
    /* column already exists (fresh table above, or already migrated) */
  }
};

/** Append one logical update as one or more rows, none above the row cap. */
export const appendUpdateRow = (sql: SqlStorage, update: Uint8Array, receivedAt: number): void => {
  for (let off = 0; off < update.byteLength; off += CHUNK_BYTES) {
    const end = Math.min(off + CHUNK_BYTES, update.byteLength);
    sql.exec(
      "INSERT INTO updates (bytes, received_at, cont) VALUES (?, ?, ?)",
      update.buffer.slice(update.byteOffset + off, update.byteOffset + end),
      receivedAt,
      off === 0 ? 0 : 1
    );
  }
};

/** Reassembled logical updates in seq order, one Uint8Array per update. */
export function* readUpdateRows(sql: SqlStorage): Generator<Uint8Array> {
  for (const update of readUpdatesThrough(sql, Number.MAX_SAFE_INTEGER)) yield update.bytes;
}

/** One logged update as replayed: its reassembled bytes, and the `seq` of the
 * LAST row carrying it — the delete cursor a fold step commits against. */
export interface LoggedUpdate {
  lastSeq: number;
  bytes: Uint8Array;
}

/** Reassembled logical updates in seq order through `upToSeq` (inclusive).
 *
 * Same cont-grouping as [`readUpdateRows`], bounded: a fold step reads only
 * the rows it is about to fold rather than the whole log. */
export function* readUpdatesThrough(sql: SqlStorage, upToSeq: number): Generator<LoggedUpdate> {
  let parts: Uint8Array[] = [];
  let lastSeq = 0;
  const join = (group: Uint8Array[]): Uint8Array => {
    if (group.length === 1) return group[0]!;
    const out = new Uint8Array(group.reduce((n, p) => n + p.length, 0));
    let off = 0;
    for (const p of group) {
      out.set(p, off);
      off += p.length;
    }
    return out;
  };
  for (const row of sql.exec("SELECT seq, bytes, cont FROM updates WHERE seq <= ? ORDER BY seq", upToSeq)) {
    const bytes = new Uint8Array(row.bytes as ArrayBuffer);
    const seq = Number(row.seq);
    if (!(row.cont as number) && parts.length > 0) {
      yield { lastSeq, bytes: join(parts) };
      parts = [];
    }
    parts.push(bytes);
    lastSeq = seq;
  }
  if (parts.length > 0) yield { lastSeq, bytes: join(parts) };
}

/** What one fold step may move: the oldest run of COMPLETE logical updates
 * whose storage rows fit both budgets — rows bound many tiny updates, bytes
 * bound one huge one (the same two axes COMPACT_LOG_ROWS/BYTES fold on).
 * Pure over the injected sql, so the batching rule is testable without a DO.
 *
 * A step never splits a chunk group: importing half of a reassembled update
 * is worse than deferring it, so the group waits for the next step. Returns
 * the inclusive `lastSeq` delete cursor and the exact byte total of every row
 * it covers (what the caller subtracts from `updateBytes`), or null when the
 * log is empty. */
export const planFoldStep = (
  sql: SqlStorage,
  maxRows: number = FOLD_STEP_ROWS,
  maxBytes: number = FOLD_STEP_BYTES
): { lastSeq: number; bytes: number } | null => {
  let planRows = 0;
  let planBytes = 0;
  let planEndSeq = 0;
  let groupRows = 0;
  let groupBytes = 0;
  let groupEndSeq = 0;
  /** Fold the finished group into the plan; false when it busts the budget.
   * One exception: a group too big for an EMPTY plan is taken anyway — it
   * gets its own step. Deferring it would be deferring it forever, and a log
   * made of whale updates (a reset room's re-uploaded history) is exactly the
   * log that most needs to fold. */
  const closeGroup = (): boolean => {
    if (groupRows === 0) return true;
    const fits =
      planRows + groupRows <= maxRows && planBytes + groupBytes <= maxBytes;
    if (!fits && planEndSeq > 0) return false;
    planRows += groupRows;
    planBytes += groupBytes;
    planEndSeq = groupEndSeq;
    groupRows = 0;
    groupBytes = 0;
    return true;
  };
  for (const row of sql.exec("SELECT seq, cont, LENGTH(bytes) AS n FROM updates ORDER BY seq")) {
    const n = Number(row.n);
    if (!(row.cont as number)) {
      // A new group begins: the previous one just ended.
      if (!closeGroup()) break;
      groupRows = 1;
      groupBytes = n;
    } else {
      groupRows++;
      groupBytes += n;
    }
    groupEndSeq = Number(row.seq);
  }
  closeGroup(); // the trailing group; overflowing it defers it, loses nothing
  return planEndSeq > 0 ? { lastSeq: planEndSeq, bytes: planBytes } : null;
};
