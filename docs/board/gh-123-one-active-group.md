# One Active group — **done** (gh#123)

§gh#103 gave the sidebar an **Agents** group and §gh#117 added **Running** under it —
a split by how a run started: the board released it, or somebody (or some
orchestrator) just started it. That is a mechanism distinction, and the
reader's question does not contain it: "what is working, and which of it wants
me" has one answer. Now it gets one group — **Active**, needs-you first, then
working, blind to origin in the order — on all three frontends.

- **`comet_proto::view::board::active_rows` is the merge, and it is small.**
  Membership already partitioned (`running_rows` subtracts every chat a live
  attempt claims — §gh#117), so the union never draws a chat twice and the merge
  is only an order: the key both halves already sorted by (urgency rank, then
  longest-running, then the chat id), applied once across the union. Without
  that one sort, concatenated halves would put a working attempt above a
  blocked hand-started run — the exact order the merge exists to end.
  `ActiveRow` is the two-variant row (`Agent`/`Unmanaged`);
  `active_needing_attention` feeds the one header badge, replacing the two
  per-group counters. Ported whole to Swift in `BoardModels.swift`, as the
  halves were.
- **All three frontends dropped a header, not a row shape.** The TUI's
  `Row::Agent`/`Row::Running`, the desktop's
  `render_agent_row`/`render_running_row` and the phone's
  `AgentRowView`/`RunningRowView` all survive; only the section build changed
  (one "Active" header, one blocked count). The desktop's
  `BoardPanel::active()` replaced `agents()`/`running()`, and the sessions
  list's `render_active_rows` was renamed `render_session_rows` so "active"
  means exactly one thing in that file.
- **The chip is the origin telling.** In the split world, which header a row
  sat under said "board" or "not"; merged, that job moves onto the row. The
  issue identifier draws as a chip — element/wash fill, the "thing you act on"
  level, never an accent tint (the accent stays on the state rail) — and an
  unmanaged run deliberately wears none: its bare title *is* the other half of
  the telling. The board rows keep their branch/cap sub-line unchanged.
- **Everything else is inherited.** Membership, staleness expiry, the
  no-empty-header rule, open-only click behavior, the second-by-second
  counter debt: all exactly as §gh#103 and §gh#117 state them, per half.
