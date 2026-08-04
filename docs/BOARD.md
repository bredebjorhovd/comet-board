# comet-board — the board fork

This fork adds an autonomous-agent task board to comet, ported from
[herdr-board](https://github.com/bredebjorhovd/herdr-board). Linear issues
(read-write) and GitHub issues/PRs come in; a dispatch releases a task into a
comet chat with a coding agent; session state reconciles back to the board and
to the trackers. Agents can read the board and dispatch from it themselves —
that is the point, not an escape hatch.

This document is the handoff map: what is ported, what maps to what, what was
deliberately left behind, and the remaining work in dependency order.

## Why comet is the better substrate

herdr-board drove a terminal multiplexer, and roughly a third of its code
existed to distrust it. Every one of those fights is a first-class primitive
here:

| herdr-board fought                          | comet gives                            |
|---------------------------------------------|----------------------------------------|
| screen-scraped agent state (manifest rules, spinner regexes) | `SessionStatus` enum in the workspace doc: `Idle / Working / AwaitingInput / Errored` |
| dead agents reading `working` (gh#32 resampling) | client-side staleness gate (`comet_proto::view::SESSION_STALE_MS`) |
| unreliable prompt delivery (nudge-and-verify, wake latch) | durable command ledger (`QueueCommand`) with dedupe/TTL/supersede |
| "did the turn end" (60s settle clock, gh#34) | run journal: runs end as recorded events, crash recovery stamps `aborted` |
| pane identity (`HERDR_PANE_ID`)             | chat id — durable, addressable across devices |
| single machine                              | dispatch from anywhere, execute on the host device, laptop closes, work continues |

## What is ported and working (`crates/board`)

All of it compiles and its 120 tests pass. The tracker-facing core came over
nearly verbatim — it never depended on herdr:

- `model.rs` — tasks, attempts, states, derivation vocabulary. Unchanged.
- `db.rs` — SQLite store (tasks, attempts, writeback queue), migrations.
  Unchanged. **`Attempt.pane_id` now stores a comet chat id** — same column,
  new meaning, documented at the type. Rename only with a migration, and only
  if it ever earns one.
- `sources/linear.rs`, `sources/github.rs` — API clients, state-by-type
  resolution, label filters. Unchanged.
- `config.rs` — `routing.toml`, credentials, per-repo overrides. Changed:
  paths live under `~/.comet-native/board` (`COMET_DATA_DIR` respected);
  runtimes validate against comet harnesses; the herdr pane-layout knobs
  (`split_direction`, `max_panes_per_tab`) and `nudge_stalled` are gone.
  `Route.workspace` now names a comet **space** — the config key keeps its
  spelling so ported routing.toml files load.
- `stats.rs`, `log.rs` — unchanged.
- `runtime.rs` — **new**, the seam. `Runtime` trait (dispatch / prompt /
  cancel / session / chat_alive / chat_cwd), `DispatchSpec`/`DispatchHandle`, the
  `SessionStatus → AgentStatus` mapping with the staleness gate, and
  `harness_for_runtime`. Read this file first; it is the contract every
  remaining task implements against.
- `sync.rs` — the tracker half of herdr-board's sync cycle, ported (H1):
  polling with watermarks and full sweeps, reaping, PR linking with repo
  scoping, merge observation, state derivation with the review/close
  writebacks, the writeback drain, source health. The reconcile half is
  reshaped: `reconcile_sessions` (clocked lifecycle, interval) and
  `refresh_statuses` (event-driven: statuses, plus H4's settle/reopen)
  consume chat-id→status maps built from the session watch through
  `runtime::agent_status`. Orphaning keeps the two-tick rule but only fires
  on a chat that was *seen working* — a chat with no session row yet is
  indistinguishable from a dispatch whose first run has not started, and the
  verdict on those waits for H2's `chat_alive`.
- `crates/board/src/settled.rs` — **new** (H4): the settle decision, pure.
  The evidence hierarchy (PR = the agent's own statement, closes the attempt
  immediately whatever the run's exit said; commits = weaker, close only a
  run that ended cleanly; an `Errored` run stays live so its retry is the
  same attempt) and the reopen rule. The machinery around it is in `sync.rs`
  (`maybe_settle`, `rewatch_settled_attempts`, the targeted PR recheck) and
  keys off run-journal facts via `Runtime::last_run_end` — no debounce
  clock anywhere.
- `adopt.rs`, `doctor.rs`, `init.rs` — the operator-facing trio (H8), walking
  comet spaces where herdr-board walked workspaces. Exposed by
  `apps/board-cli` (binary `comet-board`), which fetches this device's spaces
  over the IPC WebSocket; see §H8 below for what changed in the port.
- `crates/engine/src/board.rs` — **new** (H1): `BoardService`, the engine
  hosting what `syncd` was. One dedicated thread owns `board.db` and the
  blocking source clients; a task forwards `WatchSessions`-shaped snapshots
  into it. Config/credential changes are picked up per cycle. On by default,
  `COMET_BOARD=0` disables (`EngineConfig::board`).

- `crates/board/src/dispatch.rs` — **new** (H2+H3): herdr-board's `dispatch.rs`
  minus panes. Task + route → `DispatchSpec` resolution (branch template,
  brief, space matching) plus the pipeline decisions: `check_capacity`
  (`max_concurrent_per_workspace` counts live attempts per space),
  `dispatcher_for` (the `via` chat id → parent-task/chat provenance verdict),
  `dispatcher_name` for the upstream comment. `{worktree}` in a brief resolves
  late, via `DispatchSpec::prompt_at`, once the executor knows the checkout.
  Every harness run exports `COMET_BOARD_CHAT_ID` (the chat it serves) into
  the child's env — `RunControls::chat_id`, stamped by the engine on every
  dispatch — so `comet-board dispatch` inherits identity the way
  `HERDR_PANE_ID` provided it.
- `crates/board/src/rows.rs` — **new** (H2): `TaskRow` — herdr-board's
  `list --json` contract with the pane→chat renames (`chat_id`,
  `dispatched_by_chat`). What `WatchBoard` streams and H6's `list` prints.
- `crates/engine/src/board_runtime.rs` — **new** (H2): `CometRuntime`, the
  `Runtime` trait against engine internals. Worktrees via `repos.rs`
  (`create_worktree_on` — exact branch names), chats via
  `workspace.create_chat`, briefs/steers/interrupts via the command ledger,
  status off the merged session mirror.
- `crates/board/src/review.rs` — review delivery (H5): herdr-board's
  `review.rs` minus the wake latch and busy-check, delivering over the
  command ledger via `Runtime::prompt`. See §H5 below for what was dropped
  and why the loop still converges.
- `crates/proto/src/view/board.rs` — **new** (H7): the view's shared
  derivations — `BoardState` (moved from `comet-board`, glyphs included),
  `TaskRow` (moved, wire contract), plus `Filter`, `sections`,
  `routes_present`/`filter_cycle`, `finished_today`, `row_metadata` — so the
  TUI and the future gpui app derive the same rows.
- `crates/tui/src/board.rs` + the board section of `crates/tui/src/render.rs` —
  **new** (H7): the board pane (`B`), consuming `WatchBoard`, dispatching over
  `DispatchTask`. See §H7 below.

RPC surface: `WatchBoard` (stream of `TaskRow`s, current value first),
`DispatchTask {taskId, via?, runtime?, model?}` → `{chatId, cwd, attempt}`,
`CancelTask {taskId}` — served in `crates/engine/src/rpc.rs` off the board
service, which executes dispatch/cancel on its loop thread (`board.db` has one
writer). `ListBoardRuntimes` → `[{name, label}]` lists the runtimes a dispatch
can be pointed at (the canonical set `build_spec` validates an override
against) for pickers in the desktop panel and the CLI. `runtime`/`model`
override the route's configured runtime and the harness's default model for
that one dispatch; the attempt row records whatever the agent actually ran
under.

## What was deliberately NOT ported

Do not resurrect these; their reasons to exist are herdr's, not comet's.

- `herdr.rs` — the mux driver. Replaced by `runtime.rs` + engine internals.
- `screen.rs`, `wake.rs`, `nudge.rs` — screen reading, delivery verification,
  stalled-agent nudging. The command ledger and `SessionStatus` make all three
  meaningless here. If a run dies, the engine says `Errored`; the board maps it
  to `blocked` and the operator retries or cancels. No typing into terminals.
- `integration.rs` (agent-detection manifest overrides) and the
  Claude-specific state-detection saga. Nothing to detect: the harness runs
  the agent.
- `ui/` — herdr-board's own ratatui app. The board renders inside comet's
  existing frontends instead (H7).
- `settled.rs`'s screen-resample half and `gc.rs`'s pane logic. Settle keys
  off run-journal events now (H4); worktrees are the engine's (`repos.rs`).
- herdr-board's `adopt.rs` as written — it walked herdr workspaces. Its
  successor landed with H8 as `crates/board/src/adopt.rs`, walking comet
  spaces; the routing.toml writer came over verbatim.

## Remaining work, in dependency order

Sizes: S < half a day of focused agent work, M ≈ a day, L = several.

### H1 — Board service in the engine — **done**
Landed as `crates/board/src/sync.rs` + `crates/engine/src/board.rs` (see the
ported-and-working list above). Store is SQLite at `Paths::db()` — the board
is device-local state, not a CRDT doc (rationale: one writer, no offline
merge problem, and herdr-board's schema/tests came for free). Left with H1
deliberately open: settle decisions (H4) and orphaning a never-started chat
(needs H2's `chat_alive`).

### H2 — `Runtime` impl against engine internals — **done**
Landed as `crates/engine/src/board_runtime.rs` (`CometRuntime`) plus the real
RPC handlers (see the ported-and-working list above). Dispatch/cancel execute
on the board loop's thread through a command channel; `WatchBoard` is a
`watch_stream` fed by the loop after every cycle, status refresh, and command.
Still deliberately open: wiring `chat_alive` into reconcile's
never-started-chat verdict.

### H3 — Dispatch pipeline — **done**
Landed in `crates/board/src/dispatch.rs` + the engine's `handle_dispatch`
(see the ported-and-working list above): concurrency caps as a refusal before
anything is created, `via` resolved into parent-task/chat provenance on the
attempt row and the upstream dispatch comment, `{worktree}` threaded into the
brief at execution time, and `COMET_BOARD_CHAT_ID` exported where the harness
spawns (`RunControls::chat_id` → child env, claude and codex adapters).

### H4 — Settle logic — **done**
Landed as `crates/board/src/settled.rs` (the pure decision) plus the settle
machinery in `sync.rs` (see the ported-and-working list above). The decision
keys off run-journal facts: `Runtime::last_run_end` reads the chat's last
journaled event (`CometRuntime` reads the engine's `RunJournal`), which is
what splits `Errored` (run ended, badly — no settle on commits, PR still
counts) from `AwaitingInput` (run alive — nothing settles) inside the one
`Blocked` status. `Idle` needs no journal read: it is only ever written
after a `Done`, and a chat is fresh per attempt. The 60-second clock is gone;
the event path settles on the status *transition*, the interval reconcile is
the catch-up. Artifact checks kept: recorded PR → commits-since-base → and,
only on the event path (the interval polls seconds earlier), one targeted
GitHub `pulls` recheck before closing on commits, which closes herdr's gh#29
window instead of wording around it. `reopened` semantics kept both ways: an
`Errored`→retried run never left its attempt, and a settled attempt whose
chat works again is re-opened in place (refused when re-dispatched, closed
upstream, or marked done). The dispatcher wake (herdr AGE-25) was
deliberately not ported with this.

### H5 — Review delivery — **done**
Landed as `crates/board/src/review.rs`: `SyncEngine::deliver_reviews`, run by
the board loop after every sync cycle against the pulls that cycle already
polled. Per-PR per-endpoint watermarks (`meta` under `reviews:<task>`), the
`updated_at` gate, the first-sight floor, and the actionability filter came
over verbatim; delivery is `Runtime::prompt` — a durable ledger entry, steer
or send. Dropped as planned: the wake latch and busy-check — the ledger
queues into a busy chat safely and supersede rules handle pileups. The honest
consequence is documented at the top of `review.rs`: with no author to key on
and no latch, an agent's own PR reply is relayed back into its chat once (the
composed message says so), instead of herdr-board's trade of swallowing human
comments that landed inside the wake window; the watermark still makes it a
single bounce. The author check survives as `Runtime::chat_alive` plus a new
`Runtime::chat_cwd` — the chat row's cwd must still be the attempt's
checkout.

### H6 — `comet-board` CLI — **done**
Landed as `apps/board-cli` (the `comet-board` binary H8 started with
`doctor`/`init`/`adopt`), grown into the full surface — `list [--state
--source --json]`, `dispatch --task`, `cancel --task`, `wait`, `new`, `stats`
— speaking the existing typed RPC to the local IPC port exactly as `comet-tui`
attaches. `apps/board-cli/src/ops.rs` is the agent-facing half:
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
- `dispatch` inherits `via` provenance from `COMET_BOARD_CHAT_ID` (H3 exports
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

### H7 — Board view in `comet-tui` — **done**
Landed as a board pane in `comet-tui` (`crates/tui/src/board.rs` + the board
section of `render.rs`) plus the shared derivations in `crates/proto/src/view/board.rs`:
- `B` swaps the main pane for the board and back; `esc`/`h`/`B` returns.
- Sections in herdr-board's fixed order (blocked → working → ready → review →
  failed → done), `done` folded and bounded to today; headers fold with `enter`.
- Glyph-carried state — `▲ ● ▸ ✓ ✕ ·` — with the herdr-board colour mapping
  (blocked/failed share red, working amber, review the accent) carried on
  `Theme::board_state`, which survives `NO_COLOR` exactly as herdr's did.
- `enter` dispatches a ready row (the operator's dispatch, so no `via`), opens
  a working/blocked row's chat, and folds section headers.
- The `f` / `/` / `F` filter cycle, with the `/` field replacing the footer and
  the filter's label holding the header corner.
- The derivations live in `comet_proto::view::board` — `Filter`, `sections`,
  `routes_present`/`filter_cycle`, `finished_today`, `row_metadata`, plus the
  state glyphs — so the gpui app can grow the same view later without
  divergence, as the comet architecture rule requires. Two RFC-3339 timestamps
  (`updated_at`, `started_at`) were added to `TaskRow` to feed them; the rest
  of herdr-board's `list --json` contract is unchanged.

### H8 — Adopt, doctor, `init` — **done**
Landed as `crates/board/src/{adopt,doctor,init}.rs` plus `apps/board-cli`
(binary `comet-board` — H6's binary, started early with the three commands
that need only H1):
- `doctor` — herdr checks replaced with comet ones: per route the *space*
  exists (case-insensitive display-name match), the repo is a git checkout,
  the runtime resolves to a comet harness; plus "engine reachable on the IPC
  port" instead of pidfile-based `syncd` liveness. The herdr-only checks
  (manifest overrides, stall nudge) are gone. An unreachable engine fails its
  own check and leaves route space-checks "not checked" rather than failing
  them all.
- `init` — walks this device's spaces (first `WatchSpaces` snapshot, filtered
  by `LocalDevice`); `git_detected` gates, linked worktrees are skipped as
  attempts' checkouts. Linear team discovery unchanged.
- `adopt` — detection offers git-detected spaces whose repo is missing a
  route and/or a `[github] repos` entry; the validated text-edit writer,
  `.bak` backup, ignore list, and backlog preview came over verbatim. The
  label-picker survives as `--labels`/`--all-issues` on the CLI (H7's screen
  can reuse `preview` + `adopt_with` as-is).

### Cross-cutting notes
- **Trackers stay authoritative.** State is derived on every read from
  upstream + live attempt; nothing here changes that.
- **Writeback discipline**: dispatch/outcome comments and closes are queued in
  `board.db` and drained by H1's loop; per-repo `writeback` decides at
  delivery. Already ported; just needs the loop.
- **Multi-device later.** Everything above is one engine hosting the board.
  The RPC methods are deliberately not relay-forwardable yet; forwarding (or
  moving board rows into the workspace doc) is a decision to make once
  single-device works.
- **Upstream tracking**: `git fetch upstream && git merge upstream/main`
  (upstream = zeronsh/comet). Keep board changes additive — new crate, new
  files, short diffs in `rpc/lib.rs` + `engine/rpc.rs` — so merges stay cheap.

## Reference

The herdr-board repo is the spec for behavior: its README documents every
design decision this port inherits (states, provenance, cancellation
contracts, review delivery loop-closing, settle evidence rules, filters,
adoption). When a remaining-work item says "port X", read X *and its README
section* — the comments carry the why, and the why is the product.
