# Board view in `comet-tui` — **done**

Landed as a board pane in `comet-tui` (`crates/tui/src/board.rs` + the board
section of `render.rs`) plus the shared derivations in `crates/proto/src/view/board.rs`:
- `B` swaps the main pane for the board and back; `esc`/`h`/`B` returns.
- Sections in herdr-board's fixed order (blocked → working → ready → review →
  failed → done), `done` folded and bounded to today; headers fold with `enter`.
- Glyph-carried state — `▲ ● ▸ ✓ ✕ ·` — with the herdr-board colour mapping
  (blocked/failed share red, working amber, review the accent) carried on
  `Theme::board_state`, which survives `NO_COLOR` exactly as herdr's did.
- `enter` dispatches a ready row (the operator's dispatch, so no `via`), retries
  a failed one (§gh#68), opens a working/blocked row's chat, and folds section
  headers. `R` retries, replacing a blocked row's live attempt (§gh#68).

- `enter` releases a ready row (the operator's dispatch, so no `via`) — through
  the account picker §gh#73 added, whose first row is the route's own account and
  so is the behaviour this line described before it; opens a working/blocked
  row's chat, and folds section headers.
- The `f` / `/` / `F` filter cycle, with the `/` field replacing the footer and
  the filter's label holding the header corner.
- The derivations live in `comet_proto::view::board` — `Filter`, `sections`,
  `routes_present`/`filter_cycle`, `finished_today`, `row_metadata`, plus the
  state glyphs — so the gpui app can grow the same view later without
  divergence, as the comet architecture rule requires. Two RFC-3339 timestamps
  (`updated_at`, `started_at`) were added to `TaskRow` to feed them; the rest
  of herdr-board's `list --json` contract is unchanged.
