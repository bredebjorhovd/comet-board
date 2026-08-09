# comet-board — the board fork

This fork adds an autonomous-agent task board to comet, ported from
[herdr-board](https://github.com/bredebjorhovd/herdr-board). Linear issues
(read-write) and GitHub issues/PRs come in; a dispatch releases a task into a
comet chat with a coding agent; session state reconciles back to the board and
to the trackers. Agents can read the board and dispatch from it themselves —
that is the point, not an escape hatch.

This document is the handoff map: what is ported, what maps to what, and what
was deliberately left behind. The work itself — one write-up per item, done or
open — is one file each in [`docs/board/`](board/); see **The work** below for
how they are named and referred to.

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
  resolution, label filters. Linear unchanged; `github.rs` keeps its endpoints
  and gained an auth seam (`sources/github_app.rs`, gh#58 below).
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
- `sync.rs` — the tracker half of herdr-board's sync cycle, ported (§board-service):
  polling with watermarks and full sweeps, reaping, PR linking with repo
  scoping, merge observation, state derivation with the review/close
  writebacks, the writeback drain, source health. The reconcile half is
  reshaped: `reconcile_sessions` (clocked lifecycle, interval) and
  `refresh_statuses` (event-driven: statuses, plus §settle-logic's settle/reopen)
  consume chat-id→status maps built from the session watch through
  `runtime::agent_status`. Orphaning keeps the two-tick rule but only fires
  on a chat that was *seen working* — a chat with no session row yet is
  indistinguishable from a dispatch whose first run has not started, and the
  verdict on those waits for §runtime-impl's `chat_alive`.
- `crates/board/src/overrun.rs` — **new** (§gh#70): the wall-clock cap on
  a live attempt, pure. Within / warn / cancel, plus the grace the warning
  buys. The machinery is `sync.rs`'s `enforce_duration_cap`, on the interval
  reconcile — the same clock orphaning rides.
- `crates/board/src/gc.rs` — **new** (§gh#72): whose an attempt's checkout
  is (live / held / spent) and when it may go, pure — plus the worktree-root
  measurement `doctor` reports. The machinery is `sync.rs`'s
  `collect_worktrees`, on the same interval clock. `chat_standing` (§gh#132,
  gh#139) asks the same question about the attempt's *chat*, and
  `sync.rs`'s `archive_chats` sweeps it beside the checkouts.
  `cache_standing` + `sweep_build_output` (§gh#186) ask it of the build
  output *inside* the checkout — the one leaving that is a cache rather than
  evidence, and the only one whose clock does not wait for the task to leave
  the board.
- `crates/board/src/settled.rs` — **new** (§settle-logic): the settle decision, pure.
  The evidence hierarchy (PR = the agent's own statement, closes the attempt
  immediately whatever the run's exit said; commits = weaker, close only a
  run that ended cleanly *and* only once they are on origin, gh#69; an
  `Errored` run stays live so its retry is the same attempt) and the reopen
  rule. The machinery around it is in `sync.rs`
  (`maybe_settle`, `rewatch_settled_attempts`, the targeted PR recheck,
  `commits_are_on_origin`) and
  keys off run-journal facts via `Runtime::last_run_end` — no debounce
  clock anywhere.
- `adopt.rs`, `doctor.rs`, `init.rs` — the operator-facing trio (§adopt-doctor-init), walking
  comet spaces where herdr-board walked workspaces. Exposed by
  `apps/board-cli` (binary `comet-board`), which fetches this device's spaces
  over the IPC WebSocket; see §adopt-doctor-init for what changed in the port.
- `crates/engine/src/board.rs` — **new** (§board-service): `BoardService`, the engine
  hosting what `syncd` was. One dedicated thread owns `board.db` and the
  blocking source clients; a task forwards `WatchSessions`-shaped snapshots
  into it. Config/credential changes are picked up per cycle. On by default,
  `COMET_BOARD=0` disables (`EngineConfig::board`).

- `crates/board/src/dispatch.rs` — **new** (§runtime-impl+§dispatch-pipeline): herdr-board's `dispatch.rs`
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
- `crates/board/src/rows.rs` — **new** (§runtime-impl): `TaskRow` — herdr-board's
  `list --json` contract with the pane→chat renames (`chat_id`,
  `dispatched_by_chat`). What `WatchBoard` streams and §board-cli's `list` prints.
- `crates/engine/src/board_runtime.rs` — **new** (§runtime-impl): `CometRuntime`, the
  `Runtime` trait against engine internals. Worktrees via `repos.rs`
  (`create_worktree_on` — exact branch names), chats via
  `workspace.create_chat`, briefs/steers/interrupts via the command ledger,
  status off the merged session mirror.
  A fresh branch is cut from the route's `base` **fetched from origin**
  (`defaults.base`, `origin/HEAD` by default — gh#67), never from the space
  folder's HEAD: an always-on box's folder sits on whatever ran there last, and
  branching from it hands every agent a stale main. A fetch that fails refuses
  the dispatch rather than falling back; `base = "HEAD"` is the explicit opt-out
  for a repo with no remote, and `doctor` fails a route needing an `origin` the
  repo does not have. A retry neither fetches nor moves anything — an existing
  branch is re-opened on its own commits (`git worktree prune` first, so a
  hand-deleted checkout is a retry rather than a failure).
- `crates/board/src/review.rs` — review delivery (§review-delivery): herdr-board's
  `review.rs` minus the wake latch and busy-check, delivering over the
  command ledger via `Runtime::prompt`. See §review-delivery for what was dropped
  and why the loop still converges.
- `crates/board/src/notify.rs` — **new** (gh#71): who gets told what when a
  dispatched attempt blocks or settles, and the wording each of the three
  audiences gets. The effects are in `sync.rs` (`announce`, `wake_dispatcher`,
  `post_webhook`, `note_blocked`); see §gh#70.
- `crates/proto/src/view/board.rs` — **new** (§board-view): the view's shared
  derivations — `BoardState` (moved from `comet-board`, glyphs included),
  `TaskRow` (moved, wire contract), plus `Filter`, `sections`,
  `routes_present`/`filter_cycle`, `finished_today`, `row_metadata` — so the
  TUI and the gpui app derive the same rows. `AgentState`/`agent_rows`
  (gh#103) joins those rows to the chats and the session watch;
  `active_rows` (gh#123) merges them with gh#117's `running_rows` into the
  one **Active** group every frontend's sidebar draws.
- `crates/tui/src/board.rs` + the board section of `crates/tui/src/render.rs` —
  **new** (§board-view): the board pane (`B`), consuming `WatchBoard`, dispatching over
  `DispatchTask`. See §board-view. The same stream also feeds the sidebar's
  Active group in both frontends (`Row::Agent` here,
  `Shell::render_active_section` in `crates/ui/src/shell/spaces.rs` — §gh#97).

RPC surface: `WatchBoard` (stream of `TaskRow`s, current value first),
`DispatchTask {taskId, via?, viaDevice?, viaUser?, runtime?, model?, account?,
bill?}` →
`{chatId, cwd, attempt}`, `CancelTask {taskId}` — served in
`crates/engine/src/rpc.rs` off the board service, which executes
dispatch/cancel on its loop thread (`board.db` has one writer).
`ReadBoardTask {taskId}` → `{id, body}` reads one row's issue text for the
detail surfaces (gh#132). A call rather than a field on the streamed row,
deliberately: `WatchBoard` republishes all hundred-odd rows on every sync cycle,
and a hundred issue bodies riding along would make each frame two orders of
magnitude larger — relayed to a phone — to draw one truncated line. It is read
when somebody opens a row, and only that row's.
`ListBoardRuntimes` → `[{name, label, harness, unavailable?}]` lists the runtimes
a dispatch can be pointed at (the canonical set `build_spec` validates an
override against) for pickers in the desktop panel, the TUI and the CLI;
`harness` is what the name resolves to, so a picker can tell which agent
accounts a runtime could spend without re-implementing `harness_for_runtime`.
`unavailable` is why that runtime could not start **on the device the call was
answered by** — see §gh#165. `runtime`/`model`/`account`
override the route's configured runtime, the harness's default model, and the
route's `account` for that one dispatch; the attempt row records whatever the
agent actually ran under. `bill` is the acknowledgement that a run spends
somebody else's subscription, which `billing_guard = "require-own"` wants
instead of a refusal — see §gh#97. `via`/`viaDevice`/`viaUser` are provenance,
never authority — see §gh#73.

The write-ups for the three that needed one — per-run agent accounts (gh#59),
a teammate's view of the board (gh#66), GitHub App auth (gh#58) — are in
[`docs/board/`](board/) with everything else, under **The work** below.

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
  existing frontends instead (§board-view).
- `settled.rs`'s screen-resample half and `gc.rs`'s **pane** logic. Settle keys
  off run-journal events now (§settle-logic); there are no panes to collect.
  Its *worktree* half was the half that mattered and its absence was a leak —
  ported at last as §gh#72, keyed on board state rather than on herdr's pane
  listing.
- herdr-board's `adopt.rs` as written — it walked herdr workspaces. Its
  successor landed with §adopt-doctor-init as `crates/board/src/adopt.rs`,
  walking comet spaces; the routing.toml writer came over verbatim.

## The work, one file per item

Every item — done or open — is its own file under [`docs/board/`](board/).
There is no list of them here, and that is deliberate: a list is a shared line,
a shared line is a merge conflict, and a conflicted PR gets no CI run. Two
branches writing up two items touch two different files and merge silently
(gh#203).

The directory listing *is* the index. A filename carries the ticket and the
headline, so `ls docs/board` reads as a table of contents:

```console
$ ls docs/board
gh-187-the-picker-tells-the-truth-about-what-a-box-can-run.md
gh-190-a-tests-board-is-the-directory-it-made.md
settle-logic.md
...
```

**Naming.** `gh-<ticket>-<headline>.md`. The ticket is the key; it is assigned
by the tracker, never by the writer, so two agents working at once cannot claim
the same one. The eight files with no `gh-` prefix are the original port plan,
written before this repo had a tracker: `board-service`, `runtime-impl`,
`dispatch-pipeline`, `settle-logic`, `review-delivery`, `board-cli`,
`board-view`, `adopt-doctor-init`.

**Referring to one**, from prose or a code comment: `§gh#187`, or
`§settle-logic` for one of the eight. The key is the filename with the headline
dropped. `apps/board-cli`'s `board_docs` test resolves every `§` reference in
the repo against `docs/board/` and fails the build on one that names no file —
so a rename is caught rather than discovered.

**Adding one.** Write the file. Nothing else — no index entry, no line in this
document. If the item wants a size: S < half a day of focused agent work,
M ≈ a day, L = several.

The `H1`…`H47` numbering these sections used until gh#203 is gone. It was
assigned when an agent wrote, not when work was scheduled, so two open PRs had
both claimed `H43`; three cross-references in this file pointed at the wrong
section; and it ordered nothing anybody read in that order. Git history and the
ticket numbers carry what order there was.

## Cross-cutting notes
- **Trackers stay authoritative.** State is derived on every read from
  upstream + live attempt; nothing here changes that.
- **Writeback discipline**: dispatch/outcome comments and closes are queued in
  `board.db` and drained by §board-service's loop; per-repo `writeback` decides at
  delivery. Already ported; just needs the loop.
- **Multi-device.** One engine hosts the board; every other device drives it
  over the relay (§gh#55). Moving board rows into the workspace doc stays
  deferred — one host device is correct while one box hosts the board.
- **Upstream tracking**: cherry-pick, never merge. The additive-diff advice this
  line used to carry stopped being true in August 2026 — see **Upstream**
  below for the merge base, the standing divergences, and the SHA ledger.

## Upstream: cherry-pick, not merge

`upstream` is [zeronsh/comet](https://github.com/zeronsh/comet). Until August
2026 the note here read *"`git fetch upstream && git merge upstream/main`; keep
board changes additive so merges stay cheap"*, and at the merge base that was
true — the board was a new crate plus short diffs in `rpc/lib.rs` and
`engine/rpc.rs`, and upstream had not yet touched anything we had an opinion
about. Following it today would quietly undo work. This section is what
replaces it.

**The merge base is `fb22e26`** — upstream v0.1.6, 2026-08-01. As of 2026-08-09
`upstream/main` is `433ff68` (v0.1.26), **89 commits ahead** of that base. Every
one of those commits is a candidate; none of them is automatically ours.

### Why a merge is the wrong verb now

Three upstream commits are not conflicts. Each one compiles on both sides, so
git resolves it silently — and each resolution is git picking a side of a
product argument nobody asked it to arbitrate.

- **Upstream deleted the TUI.** `7b52ce1` (2026-08-04) — *"Remove the TUI:
  crates/tui, apps/tui, comet tui subcommand, scripts, docs"*. A merge deletes
  it here. We depend on it three ways: `ARCHITECTURE.md` calls it *"a peer of
  the gpui app rather than a subset of it"*, which is the whole reason
  `comet_proto::view` exists as pure derivations two viewports share (see the
  module docs in `crates/proto/src/view/spaces.rs` — "so both viewports agree");
  it carries 87 render tests in `crates/tui/tests/render.rs` (226 in the crate),
  which is the only place the derivations are asserted against rendered frames;
  and it is the attach surface for the fork's **primary deployment** — the
  edge-less single-box mode README.md documents as `COMET_EDGE_URL=off comet
  headless` + `comet tui`. `docs/orchestrator.md` and `docs/teammate.md` both
  give operator steps in terms of it.
- **Upstream reversed gh#124 on the session row.** `ff124f4` (2026-08-03) —
  *"Show owning device in session rows: space@device"* — restores exactly the
  string gh#124 removed as information said twice with different truncation.
  Our side of that argument is written down where the code is
  (`crates/tui/src/app.rs:211`, `:271`, `crates/tui/src/render.rs:737`) and the
  derivation that replaced it is `view::spaces::device_groups` — the device name
  once, as a group header, instead of riding along on every row.
- **Light mode was built twice, independently.** `b6fb19e` (2026-08-03) upstream;
  ours is `efa52ea` (§gh#73), now Settings → Appearance
  (`crates/ui/src/settings/appearance.rs`) and `crates/tui/src/theme.rs`. Two
  implementations of one feature, and a merge keeps whichever one it wins.

The first is a deletion, the second a reversal, the third a duplicate. Nothing
about them shows up as a conflict marker, which is the point: the failure mode
is a green build and a quiet regression.

### Standing divergences

What we hold against upstream, and why. A cherry-pick that would touch one of
these is declined by default, and the decline belongs in the ledger below.

| Divergence | Ours | Upstream | Why we hold it |
|---|---|---|---|
| The TUI | kept: `crates/tui`, `apps/tui`, `comet tui` | deleted (`7b52ce1`) | peer frontend, 87 render tests, the edge-less box's only viewport |
| Session rows | space and device said once, device as a group header (gh#124, gh#138) | `space@device` on every row (`ff124f4`) | the old row spent its loudest pixels on its least differentiating fact |
| Light mode | ours (`efa52ea`, Settings → Appearance) | theirs (`b6fb19e`) | same feature, two implementations; ours is the one the TUI shares |
| The board | `crates/board`, `apps/board-cli`, the board RPCs | none | the fork's reason to exist |

### The SHA ledger — what we have taken

Empty by design until the first deliberate sync lands. gh#146 (the edge
wasm-poisoning / doc-freeing / history-trim cluster) will produce the first
rows; its exit condition is a note here recording which upstream SHAs we carry,
*so the next sync knows where it started*. Record declines too — a commit
considered and refused is a decision, and without the row the next person
rediscovers it from scratch.

| Upstream SHA | Date | Subject | Taken as | Verdict |
|---|---|---|---|---|
| — | — | *(none yet; gh#146 fills this)* | — | — |

### What we owe upstream

`upstream/main:crates/engine/src/workspace_host.rs:45` still reads
`PRESENCE_INTERVAL_MS = 15_000`, with `presence_tick` beating a `%EPH` frame
into the workspace-doc room on that interval. That is gh#145: a `%EPH` frame is
a real message, not a text ping, so a room woken every 15s never hibernates —
one idle user burned 83% of the Durable Objects free tier before an agent ran.
We deleted the beat in `51241bc` (2026-08-08) and derive liveness from the
socket the way `DeviceRoom` always did (`crates/engine/src/presence.rs`).
Upstream carries the bug into one room; we carried it into two, because the
org device registry (gh#66) is ours. When the fix has proven out on the box it
is worth sending back.

There is **no PR path**. `bredebjorhovd/comet-board` is not a GitHub fork of
`zeronsh/comet` — `isFork: false`, `parent: null` — so the GitHub UI offers no
compare-across-forks and no upstream PR button. Contributing back means a
branch on a real fork, or a patch. Reading from them means the same thing it
means today: a plain `upstream` remote and `git cherry-pick`.

### The mechanics

```bash
git fetch upstream
git log --oneline fb22e26..upstream/main -- edge/     # scope to what you mean to take
git cherry-pick -x <sha>                              # -x records the origin in the message
```

`-x` is not optional: it stamps *"cherry picked from commit …"* into the commit
message, which makes the ledger above recoverable from git even when somebody
forgets to write the row. Scope the `git log` to a path — the 89-commit range is
mostly `edge/`, and the commits that touch `crates/` are the ones most likely to
land on a standing divergence.

Keeping board changes additive is still good practice, for the plain reason that
short diffs cherry-pick cleanly in both directions. It is no longer a strategy.

## Reference

The herdr-board repo is the spec for behavior: its README documents every
design decision this port inherits (states, provenance, cancellation
contracts, review delivery loop-closing, settle evidence rules, filters,
adoption). When a remaining-work item says "port X", read X *and its README
section* — the comments carry the why, and the why is the product.
