# `comet-board` CLI — **done**

Landed as `apps/board-cli` (the `comet-board` binary §adopt-doctor-init started with
`doctor`/`init`/`adopt`), grown into the full surface — `list [--state
--source --json]`, `dispatch --task`, `retry --task` (§gh#68), `cancel --task`,
`wait`, `new`, `stats` — speaking the existing typed RPC to the local IPC port
exactly as any viewport attaches, at the board host named by `--device` (§gh#68).
`apps/board-cli/src/ops.rs` is the agent-facing half:
- `list --json` prints herdr-board's contract verbatim, modulo the two renames
  the port dictates (`pane_id` → `chat_id`, `dispatched_by_pane` →
  `dispatched_by_chat`). The shape lives in `comet_proto::view::board::TaskRow`
  and is re-exported by `crates/board/src/rows.rs`, which owns it; the CLI only
  serializes what `WatchBoard` streamed. `docs/agent-conventions.md` teaches it,
  so it is not the CLI's to bend.
- `wait` is a `WatchBoard` subscription, not a poll loop: it answers as soon as
  a watched row reaches a settled state, and with no `--task` watches whatever
  was in flight when it was called (resolved once — work dispatched later is
  not what that call is waiting for). Unknown filters and states are refused
  rather than answered with `[]`, which a caller cannot tell from "nothing is
  ready".
- `dispatch` inherits `via` provenance from `COMET_BOARD_CHAT_ID` (§dispatch-pipeline exports
  it into the harness child env), so a dispatch from inside a board-dispatched
  chat records its parent without being told. `--via` is for releasing on
  behalf of a chat that is not you; the operator's dispatch has neither.
- `--runtime` / `--model` override the route's runtime and the harness's
  default model for one dispatch, checked first against `ListBoardRuntimes` /
  `ListModels {harness}` — the same two calls the desktop picker fills its rows
  from, so the CLI refuses what the picker would not have offered. The engine
  validates the runtime on its own, but an unknown *model* is only the
  harness's business, and by the time the harness sees it the dispatch has cut
  a worktree, made a chat and started an agent. A catalog that cannot be read
  proves nothing: it degrades to a note on stderr and lets the dispatch
  through, as the panel degrades to the route's runtime.
- `new` is the one command that does not ask the engine — it writes to the
  trackers, which sit upstream of it, with the same clients the sync loop uses.
  `--dispatch` then waits for the engine's next poll to put the row on the
  board before releasing it, rather than failing on the race.
