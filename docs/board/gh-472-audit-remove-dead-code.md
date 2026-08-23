# A deletion-first audit: dead code, drifted mirrors, orphaned assets — **done** (gh#472)

Every removal below is backed by a reference check (symbol counted across every
`.rs` file in the workspace — definition-only means nothing calls it), compiler
and clippy output, or repository history. Nothing was removed for looking
unused; where an item is part of a cross-language contract it stayed.

### Dead production code

- **`toggle_switch`** (`crates/ui/src/pickers.rs`) — a `#[allow(dead_code)]`
  port of comet's branch-picker `Toggle` with no caller anywhere. The allow
  was hiding exactly the thing this audit exists for.
- **`set_screen_sample` / `clear_screen_sample` / `set_nudged` / `clear_nudges`**
  (`crates/board/src/db.rs`) — writer methods for screen sampling and nudge
  counting, herdr concepts this engine never drives. No callers; their doc
  comments even link [`crate::screen`] and [`crate::nudge`] modules that do not
  exist in this tree. The stored columns stay (impl spec §3, read by
  `sqlite3 board.db` when debugging); only the unreachable writers went.
- **`set_chat_lineage`** (`crates/engine/src/workspace_host.rs`) +
  **`set_chat_forked_from`** (`crates/doc/src/workspace.rs`) — the gh#425
  write-once lineage helper, superseded the moment forks began writing
  `forkedFrom` inline in the chat row (`engine/src/fork.rs` builds the row
  complete). The wrapper and its wrapped method had no callers left.
- **`route_keys` / `default_keys` / `automation_keys`**
  (`crates/board/src/routes.rs`) — key lists kept "for a picker or a --help";
  neither picker nor help ever arrived, and both `kind_of` and the settings
  page read the underlying tables directly.
- **`COMMANDS`** (`crates/ui/src/commands.rs`) — a one-entry command registry
  nobody iterates; menus and tabs use `NEW_SESSION` directly.
- **`status_for`** (`crates/engine/src/board.rs`) — an ad-hoc-list variant of
  `agent_status` with no caller.
- **`stderr_only`** (`crates/board/src/log.rs`), **`self_exe`**
  (`crates/board/src/git_credentials.rs`), **`watch_presence` /
  `watch_convergence`** (`crates/sync/src/room.rs`), **`is_queue_entry`**
  (`crates/doc/src/queue.rs` — `doc_host.rs` matches the same kinds inline),
  **`kbd_hint`** (`crates/ui/src/popover.rs` — superseded by
  `key_hint_label`), **`space_for_chat`** (`crates/ui/src/state.rs`),
  **`shown_tasks`** (`crates/ui/src/board.rs`), **`find_backspace`**
  (`crates/ui/src/board.rs` — the live find field is a `ComposerInput`, which
  owns backspace itself), **`is_pinned`** (`crates/ui/src/transcript.rs`),
  **`cached_cwd_count`** (`crates/engine/src/diff_sync.rs`),
  **`dot::AWAITING` / `dot::ERRORED`** (`crates/proto/src/view.rs`) — all
  definition-only.

### Drifted mirrors and orphaned dependencies

- **`RETAIN_DAYS` / `COMPACT_LOG_BYTES` / `SOFT_CEILING_BYTES` /
  `DO_FLUSH_MS`** (`crates/doc/src/constants.rs`) deleted. These are Session DO
  tunables whose authoritative copy is `edge/src/session-doc/constants.ts`;
  nothing in the Rust workspace reads them any more, and the mirror had
  already drifted — Rust said `COMPACT_LOG_BYTES = 8 MiB` while the edge moved
  to 2 MiB in the ws4 cold-start fix. A duplicate nobody reads is one that
  documents the wrong contract. The module header now says so.
  `STREAM_COMMIT_MS` stays: the engine's `SegmentWriter` genuinely reads it.
- **`hmac = "0.12"`** (workspace `Cargo.toml`) — orphaned by 299efee9 ("Remove
  repository preparation from dispatch"), which removed the last user and left
  the workspace declaration behind. No member crate names it; no code does.
- **`serde_json` dev-dependency** (`crates/sync/Cargo.toml`) — literal
  duplicate of the same entry already under `[dependencies]`.

### Orphaned assets

- **`scripts/frame_png.py`** deleted. It renders `frame_dump.json` files — a
  format produced only by `crates/tui/tests/frame_dump.rs`, which gh#416
  ("drop the TUI: 20,149 lines nobody runs") deleted along with the rest of
  the TUI. The script survived the cull with no input producer and no
  reference from any file, script, or workflow.

### What was checked and deliberately kept

- Every icon in `crates/ui/assets/icons` is registered in the `icon_assets!`
  macro; fonts, sounds, `dist/` packaging assets and the embedded board skill
  and instructions are all referenced.
- All cargo dependencies across all eleven members resolve to at least one use
  site; the edge's devDependencies serve its smoke/check scripts.
- The motion constants (`COMET_PULSE_MS`, `GRADIENT_SPIN_MS`,
  `crates/proto/src/motion.rs`) have no Rust-side callers but are the
  documented source of truth the iOS app mirrors (`Theme/Motion.swift`).
- The db schema columns for screens/nudges stay with their impl-spec §3 note;
  the `Task` struct's `#[allow(dead_code)]` stays with it.
- `edge/scripts/{compat,fold,reset,whale}-check.mjs` are maintained incident
  tooling tied to documented incidents (gh#146, gh#553, BOARD.md); the
  compat-check pairs with the `gen_fixture` example as the cross-language gate.
- Tests that assert constants against each other (`shell.rs` review-column
  bounds) read as vacuous to clippy but pin design boundaries; kept.

### Found but not fixed here

- `jitter::tests::consecutive_draws_differ` fails on this Mac while passing in
  CI, and the test is the messenger for a real finding: `spread()`
  (`crates/sync/src/jitter.rs`) draws entropy from `subsec_nanos()`, which on
  this box's microsecond-resolution clock is always a multiple of 1000 — so
  `% span_ms` collapses to zero distinct values for a 1000 ms span and the
  herd-breaking window never opens here. Fixing the entropy source changes
  reconnect behavior, which is a product change outside a deletion audit's
  mandate; it needs its own ticket referencing gh#396.
