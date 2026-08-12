/**
 * The `meta` key/value table both rooms keep beside their update log.
 *
 * The one thing worth knowing about it: **storing a value that is already
 * stored is not a write.** A bare `INSERT … ON CONFLICT DO UPDATE` performs
 * the update whether or not the value changed, and Cloudflare bills every
 * updated row as a row written — so the dirty flags `recordLoroUpdates` sets
 * on every inbound batch (`tailDirty`, `backupDirty`, `postReset`) charged
 * three rows per batch while writing the same three values the whole time.
 * The update-log rows themselves are debounced by DO_FLUSH_MS; these were not,
 * so on any room busier than one update per five seconds they dominated the
 * count (gh#377, and the multiplier behind the 100,000-row day of gh#373).
 *
 * `set` therefore reads before it writes and returns early when the value is
 * unchanged. A row read costs 1/50th of a row write's daily allowance on the
 * free plan and 1/1000th of its price on paid, so the trade is strictly better
 * on both. It belongs here rather than at the hot call sites: the property is
 * true of every caller, and a guard at one call site is a guard the next
 * caller will not know about.
 */

export interface MetaStore {
  get(key: string): string | undefined;
  /** Writes only if `value` differs from what is stored. */
  set(key: string, value: string): void;
}

export const createMetaStore = (sql: SqlStorage): MetaStore => {
  sql.exec("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)");
  const get = (key: string): string | undefined => {
    const rows = [...sql.exec("SELECT value FROM meta WHERE key = ?", key)];
    return rows[0]?.value as string | undefined;
  };
  return {
    get,
    set(key, value) {
      // Read-before-write: the early return is the whole point of this module.
      // `undefined` (no row yet) never equals a string, so a first write always
      // lands, and every real transition still lands — only the no-op repeats
      // are dropped, which is exactly what /tail, the daily alarm and a
      // wedge-break reset are indifferent to.
      if (get(key) === value) return;
      sql.exec(
        "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        key,
        value
      );
    }
  };
};
