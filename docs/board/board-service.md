# Board service in the engine — **done**

Landed as `crates/board/src/sync.rs` + `crates/engine/src/board.rs` (see
BOARD.md's ported-and-working list). Store is SQLite at `Paths::db()` — the
board is device-local state, not a CRDT doc (rationale: one writer, no offline
merge problem, and herdr-board's schema/tests came for free). Left deliberately
open: settle decisions (§settle-logic) and orphaning a never-started chat
(needs §runtime-impl's `chat_alive`).
