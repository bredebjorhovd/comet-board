# Board view — **done** (its first surface since removed)

Landed as a board pane in `comet-tui` (`crates/tui/src/board.rs` + the board
section of `render.rs`) plus the shared derivations in `crates/proto/src/view/board.rs`.
The TUI pane — and the TUI — were removed in gh#416 once nobody ran the
terminal viewport; the derivations are the half that outlived it, and they now
draw the desktop panel (`crates/ui/src/board.rs`, §gh#70) and the phone's board
screen (`apps/ios/Comet/Board/`). What the pane established, and what survives
it in the shared layer:

- Sections in herdr-board's fixed order (blocked → working → ready → review →
  failed → done), `done` folded and bounded to today.
- Glyph-carried state — `▲ ● ▸ ✓ ✕ ·` — with the herdr-board colour mapping
  (blocked/failed share red, working amber, review the accent), chosen so the
  board survives colour being stripped (`NO_COLOR` then; monochrome contexts
  still).
- `enter` releases a ready row (the operator's dispatch, so no `via`) — through
  the account picker §gh#73 added, whose first row is the route's own account;
  opens a working/blocked row's chat. `R` retries, replacing a blocked row's
  live attempt (§gh#68).
- The `f` / `/` / `F` filter cycle.
- The derivations live in `comet_proto::view::board` — `Filter`, `sections`,
  `routes_present`/`filter_cycle`, `finished_today`, `row_metadata`, plus the
  state glyphs — which is why growing the next surface (first the gpui panel,
  then the phone) never forked the view logic, as the comet architecture rule
  requires. Two RFC-3339 timestamps (`updated_at`, `started_at`) were added to
  `TaskRow` to feed them; the rest of herdr-board's `list --json` contract is
  unchanged.
