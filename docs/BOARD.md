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
  cancel / session / chat_alive), `DispatchSpec`/`DispatchHandle`, the
  `SessionStatus → AgentStatus` mapping with the staleness gate, and
  `harness_for_runtime`. Read this file first; it is the contract every
  remaining task implements against.
- `sync.rs` — the tracker half of herdr-board's sync cycle, ported (H1):
  polling with watermarks and full sweeps, reaping, PR linking with repo
  scoping, merge observation, state derivation with the review/close
  writebacks, the writeback drain, source health. The reconcile half is
  reshaped: `reconcile_sessions` (lifecycle, interval-clocked) and
  `refresh_statuses` (status-only, event-driven) consume chat-id→status maps
  built from the session watch through `runtime::agent_status`. Orphaning
  keeps the two-tick rule but only fires on a chat that was *seen working* —
  a chat with no session row yet is indistinguishable from a dispatch whose
  first run has not started, and the verdict on those waits for H2's
  `chat_alive`.
- `crates/engine/src/board.rs` — **new** (H1): `BoardService`, the engine
  hosting what `syncd` was. One dedicated thread owns `board.db` and the
  blocking source clients; a task forwards `WatchSessions`-shaped snapshots
  into it. Config/credential changes are picked up per cycle. On by default,
  `COMET_BOARD=0` disables (`EngineConfig::board`).

RPC surface claimed (stubs): `WatchBoard`, `DispatchTask`, `CancelTask` in
`crates/rpc/src/lib.rs::methods`, answered in `crates/engine/src/rpc.rs` with
a "not wired yet" error pointing here.

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
- `adopt.rs` for now — it walked herdr workspaces. Its successor walks comet
  spaces (H8); the config it writes is already supported.

## Remaining work, in dependency order

Sizes: S < half a day of focused agent work, M ≈ a day, L = several.

### H1 — Board service in the engine — **done**
Landed as `crates/board/src/sync.rs` + `crates/engine/src/board.rs` (see the
ported-and-working list above). Store is SQLite at `Paths::db()` — the board
is device-local state, not a CRDT doc (rationale: one writer, no offline
merge problem, and herdr-board's schema/tests came for free). Left with H1
deliberately open: settle decisions (H4) and orphaning a never-started chat
(needs H2's `chat_alive`).

### H2 — `Runtime` impl against engine internals (M, needs H1's skeleton)
`CometRuntime` inside the engine implementing `crates/board`'s trait:
- `dispatch`: worktree via `repos.rs` (`CreateWorktree` path), chat via
  `workspace.create_chat` (cwd/branch/checkout/space/config from
  `DispatchSpec`), brief via `doc_host.queue_command` send. Return the chat id.
- `prompt`: `queue_command` steer-or-send.
- `cancel`: `queue_command` interrupt, then archive the chat.
- `session`/`chat_alive`: read the workspace mirror.
Then replace the three RPC stubs in `engine/src/rpc.rs` with real handlers
(`WatchBoard` as a `watch_stream`, same pattern as `WATCH_CHATS`).

### H3 — Dispatch pipeline (M, needs H2)
Port `dispatch.rs` minus panes: route resolution (exists in `config.rs`),
branch templating (exists), brief building from task + route `prompt`
template (exists as `interpolate`), concurrency caps
(`max_concurrent_per_workspace` counts live attempts per space), attempt
row lifecycle. Provenance: accept `via` (dispatching chat id) on
`DispatchTask`; export `COMET_BOARD_CHAT_ID` into the harness process env
(one-line change where the harness spawns) so `comet-board dispatch` inherits
identity the way `HERDR_PANE_ID` provided it.

### H4 — Settle logic (M, needs H2)
Port `settled.rs`'s *decision* (PR = the agent's own statement of done,
closes the attempt immediately; commits alone = weaker evidence) onto run
events instead of idle-sampling: a run ending is a journal fact, so "the
turn ended, now check the checkout for a PR/commits" replaces the 60-second
clock entirely. Keep the artifact checks (branch pushed? PR open? — GitHub
client already ported). Keep `reopened` semantics: an `Errored`→retried run
is the same attempt, not a new one.

### H5 — Review delivery (M, needs H2)
Port `review.rs`: per-PR per-endpoint watermarks (schema already in `db.rs`),
`updated_at`-gated polling, deliver via `Runtime::prompt`. Drop the wake
latch and busy-check — the ledger queues into a busy chat safely, and
supersede rules handle pileups. Keep "verify the chat still exists and its
cwd is still the attempt's checkout" (`chat_alive` + chat row cwd).

### H6 — `comet-board` CLI (M, needs H2; agents' entry point)
A thin binary (new `apps/board-cli`, name the binary `comet-board`) speaking
the existing typed RPC to the local IPC port, exactly as `comet-tui` attaches:
`list [--state --json]`, `dispatch --task`, `cancel --task`, `wait`, `new`,
`stats`, `doctor`. JSON shapes: keep herdr-board's `list --json` contract
verbatim (documented in herdr-board's README §"Driving the board from an
agent") — the agent conventions text depends on it. `wait` becomes a
`WatchBoard` subscription rather than a poll loop. Port
`agent-conventions.md` with names swapped (herdr-board → comet-board,
pane → chat).

### H7 — Board view in `comet-tui` (M–L, needs H2)
A board pane in the TUI: sections in fixed order (blocked / working / ready /
review / failed / done), glyph-carried state, `enter` to dispatch, the
filter cycle. Row derivations belong in `comet_proto::view` so the gpui app
can grow the same view later without divergence — that split is a comet
architecture rule, not a suggestion. herdr-board's `ui/render.rs` and
`ui/state.rs` are the reference for what rows say; its README documents every
interaction's rationale.

### H8 — Adopt, doctor, `init` (S–M each, needs H1)
`doctor`: port, replacing herdr checks with comet ones (space exists, repo is
a git repo, harness resolves, IPC reachable). `init`: walk spaces instead of
workspaces. `adopt`: offer git-detected spaces with no route (the workspace
doc already stamps `git_detected`); the label-picker screen ports as-is
conceptually.

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
