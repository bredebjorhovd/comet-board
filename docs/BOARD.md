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
  `sync.rs`'s `archive_chats` sweeps it beside the checkouts. `Dispatchers`
  (§gh#354) is the one fact a chat has that a directory does not: a chat whose
  released work has not left the board is nobody's to sweep — `standing` asked
  one edge up, so a dispatcher outlives what it dispatched.
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
  A base that names a branch is also **said in the brief** (gh#284,
  `dispatch::pr_base`): opening the pull request is the agent's job and `gh pr
  create` targets the repo default unasked, so a route based on `release-1.x`
  used to open a request to merge the release branch into `main`. The line is
  appended after interpolation, so a route's own `prompt` gets it too. Only the
  default is silent — `origin/HEAD` is what `gh` would have picked, and `HEAD`
  names a branch this side cannot know. Making it mechanical instead (the `gh`
  shim splicing `--base` into a bare `gh pr create`) is deliberately not done:
  it is the shim growing opinions about argv to cover what the brief states.
- `crates/board/src/review.rs` — review delivery (§review-delivery): herdr-board's
  `review.rs` minus the wake latch and busy-check, delivering over the
  command ledger via `Runtime::prompt`. See §review-delivery for what was dropped
  and why the loop still converges, and §gh#289 for where one-PR-one-chat stops
  being enough — a `changes requested` on a layer of a stack is a fact about every
  layer above it, so `address` resolves any layer to its chat and the layers on
  top get a notice and a row that says they are about to be rebased.
- `crates/board/src/notify.rs` — **new** (gh#71): who gets told what when a
  dispatched attempt blocks or settles, and the wording each of the three
  audiences gets. The effects are in `sync.rs` (`announce`, `wake_dispatcher`,
  `post_webhook`, `note_blocked`); see §gh#70.
- `crates/proto/src/view/board.rs` — **new** (§board-view): the view's shared
  derivations — `BoardState` (moved from `comet-board`, glyphs included),
  `TaskRow` (moved, wire contract), plus `Filter`, `sections`,
  `routes_present`/`filter_cycle`, `finished_today`, `row_metadata` — so the
  gpui app and the phone derive the same rows. `AgentState`/`agent_rows`
  (gh#103) joins those rows to the chats and the session watch;
  `active_rows` (gh#123) merges them with gh#117's `running_rows` into the
  one **Active** group every frontend's sidebar draws.
- The board pane that first consumed `WatchBoard` was the TUI's
  (`crates/tui/src/board.rs`, §board-view) — removed with the TUI in gh#416.
  The surfaces that draw the stream now are the desktop panel
  (`crates/ui/src/board.rs`, §gh#70) and the phone
  (`apps/ios/Comet/Board/`). The same stream also feeds the sidebar's Active
  group (`Shell::render_active_section` in `crates/ui/src/shell/spaces.rs` —
  §gh#97).

RPC surface: `WatchBoard` (stream of `TaskRow`s, current value first),
`DispatchTask {taskId, via?, viaDevice?, viaUser?, runtime?, model?, account?,
bill?, onto?, base?}` →
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
agent actually ran under. `onto`/`base` override where the dispatch *branches
from* — `onto` names another task and resolves to the branch its attempt holds,
`base` names a branch outright — and with it where the pull request is aimed,
which is the unit of stacking (gh#285). `onto` also records the parent attempt
on the child's row (`attempts.stacked_on`), which a branch string cannot:
merging the parent deletes its branch. The base has to be on **origin**, so a
parent that has not pushed refuses the release rather than cutting from trunk. `bill` is the acknowledgement that a run spends
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
  deferred — one host device is correct while one box hosts the board. Nothing
  *enforces* one, though, and two boards polling one repo race the same issue,
  so `doctor` sweeps the account and says who else is hosting one (§gh#195) —
  and since §gh#343 `onboard` and `routes add` run that same sweep *before* they
  write, refusing a repo another board already polls unless `--force` says the
  sharing is intended.
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
  crates/tui, apps/tui, comet tui subcommand, scripts, docs"*. A merge would
  have deleted it here silently, at a time when we depended on it three ways:
  it was the second viewport that justified `comet_proto::view` as pure
  derivations two surfaces share, it carried the only tests asserting those
  derivations against rendered frames, and it was the attach surface for the
  edge-less single-box mode. All three dependencies have since dissolved — the
  iOS app is the second viewport, the derivations are asserted in proto's own
  suite, and the box is driven by `comet-board` and the apps — so gh#416
  **adopted the deletion** deliberately (ours was larger: we had added
  `crates/tui/src/board.rs` after the fork). The point of this bullet stands
  as history: git would have arbitrated this product argument silently in
  2026-08, two months before the answer was actually yes.
- **Upstream reversed gh#124 on the session row.** `ff124f4` (2026-08-03) —
  *"Show owning device in session rows: space@device"* — restores exactly the
  string gh#124 removed as information said twice with different truncation.
  Our side of that argument lives in the derivation that replaced it,
  `view::spaces::device_groups` — the device name once, as a group header,
  instead of riding along on every row (it was also written down in the TUI's
  sources while those existed).
- **Light mode was built twice, independently.** `b6fb19e` (2026-08-03) upstream;
  ours is `efa52ea` (§gh#73), now Settings → Appearance
  (`crates/ui/src/settings/appearance.rs`). Two implementations of one
  feature, and a merge keeps whichever one it wins.

The first is a deletion, the second a reversal, the third a duplicate. Nothing
about them shows up as a conflict marker, which is the point: the failure mode
is a green build and a quiet regression.

### Standing divergences

What we hold against upstream, and why. A cherry-pick that would touch one of
these is declined by default, and the decline belongs in the ledger below.

| Divergence | Ours | Upstream | Why we hold it |
|---|---|---|---|
| The TUI | ~~kept~~ → deleted in gh#416, once iOS was viewport two | deleted (`7b52ce1`) | **resolved** — held while it was the second viewport and the only attach surface; both roles moved (iOS, `comet-board`) and the divergence closed |
| Session rows | space and device said once, device as a group header (gh#124, gh#138) | `space@device` on every row (`ff124f4`) | the old row spent its loudest pixels on its least differentiating fact |
| Light mode | ours (`efa52ea`, Settings → Appearance) | theirs (`b6fb19e`) | same feature, two implementations; ours is the one the shared status anchors were built for |
| The board | `crates/board`, `apps/board-cli`, the board RPCs | none | the fork's reason to exist |

### The SHA ledger — what we have taken

Record declines too — a commit considered and refused is a decision, and
without the row the next person rediscovers it from scratch.

#### Sync 1 — gh#146, 2026-08-09: the edge stability cluster

Upstream spent 2026-08-04/05 hardening the *same edge code we run* and we had
none of it. 24 commits, applied in upstream order onto `board/gh-146-comet-board`.
22 taken, 1 empty upstream, 1 declined; one commit taken in part.

Why us and not just them: we had already chased the client-side shadow of these
bugs twice. gh#116 (a dropped room connection never re-established, box dark for
remote viewers) and gh#126 (presence lying about a box that was up) are what a
room whose joins die silently *looks like* from our side. These are the server
half.

| Upstream SHA | Date | Subject | Taken as | Verdict |
|---|---|---|---|---|
| `013a087` | 08-04 | Edge join-wedge root cause: wasm-heap poisoning made joins die in silence | `53ad687` | **part** — edge + `crates/doc` RETAIN_DAYS 30→3; client log-flock half declined (see below) |
| `13704c7` | 08-04 | Trim history on log fold, not just the daily alarm | `955964a` | taken |
| `0fdea0f` | 08-04 | Trim history on cold materialization too — idle rooms never trimmed | `2319a22` | taken |
| `4ff80ae` | 08-04 | Free loro-wasm objects explicitly — GC finalizers never fire under wasm pressure | `0994232` | taken |
| `5f26832` | 08-04 | Make history trims durable before the join continues | `c93beaa` | taken |
| `c202e91` | 08-04 | Free idle docs — wasm memory outlives DO instances and never shrinks | `75c30cc` | taken |
| `c4a2e46` | 08-04 | Force-trim oversized rooms that have no aged checkpoint | `b5b95bc` | taken |
| `9176b51` | 08-04 | Reset the wasm-poison strike counter on a clean join answer | `96ea5f7` | taken (conflict: kept gh#145 `publishPresence`) |
| `1f2333d` | 08-04 | Relay accepted updates via per-socket fragmentation, not one raw frame | `e993853` | taken |
| `81ebf25` | 08-04 | Gate cutoff trims on lastTrimAt alone — isShallow re-fired them forever | `c201283` | taken |
| `d2a4b57` | 08-04 | Force-trim gate counts log bytes, not just the snapshot | `732aa87` | taken |
| `82ce441` | 08-04 | Automated wedge break boots attached sockets, like /reset-log always did | `1b6d8c9` | taken |
| `1f2152a` | 08-04 | Never serve a freed doc wrapper; count wasm use-after-free as poison | `f5e7858` | taken |
| `b019439` | 08-04 | Workspace rooms force-trim only at aged checkpoints, never the live frontier | `f764a4b` | taken — extended to our registry in `d864d36`, below |
| `c1243c5` | 08-04 | Penalty-box devices whose imports keep failing — stop the doomed-push DOS | `a52091f` | taken |
| `79c8e22` | 08-04 | Expose GET /workspace/:orgId/snapshot for doc repair reads | `d53feaa` | taken (adapted: our `forward` takes the whole `Verified`) |
| `fb6492c` | 08-04 | Retire ws3 workspace rooms — ws4 allocates virgin DO storage | this branch | taken (gh#148, after gh#146 — see *The ws4 break* below) |
| `3a89e68` | 08-04 | Recycle DOs: clear stale in-memory import penalties after fleet doc flatten | — | **empty upstream** — zero file changes; its subject describes `fb6492c`'s fallout. Nothing to carry |
| `02002cf` | 08-05 | Salvage importable updates before striking; small-payload penalty probe; attributed /stats | `7533818` | taken (conflict: gh#66 read gate inside their new try/catch) |
| `469d18a` | 08-05 | Fold the update log when it dwarfs the folded state, not only at fixed budgets | `ee75b07` | taken — then largely reverted by `6f19b76`; kept in order so we land where upstream did |
| `4aacc6d` | 08-05 | Workspace live-frontier-trim guard: match any ws generation, not the literal ws3/ | `3809996` | taken |
| `bf16add` | 08-05 | foldLog: recycle the isolate when the fold export dies on a pressed heap | `1b40962` | taken |
| `6f19b76` | 08-05 | Revert the aggressive fold triggers; route workspace /append for operator repair | `b66fcb2` | taken (adapted: route placement — see below) |
| `6cee4af` | 08-05 | Whale-session sync fix: chunked update-log rows + shallow-aware join backfill | `3f4cd0c` | taken; brought `edge/src/update-log.ts` + its 6 tests |

Plus one commit of ours that is not a cherry-pick: `d864d36`, extending the
live-frontier trim guard to `orgdev1/{orgId}`.

**What did not apply cleanly, and why — the shape to expect next time.**

- **`013a087`, partial.** Its client half patches `open_log_file` in
  `apps/comet/src/main.rs`, a function our tree does not have — it arrived
  upstream in commits outside this cluster. Applying it would have imported
  `sync_cli` and `sweep_stale_pid_logs` wholesale as unreferenced code. It is a
  log-rotation fix, unrelated to edge stability. Its `crates/doc/src/constants.rs`
  half **was** taken: RETAIN_DAYS is a constant the edge and the Rust client both
  read, and letting it drift is how the incident started.
- **Our auth gate conflicts on every read route.** gh#66 replaced upstream's
  inline `if (!workspace) { owner checks }` with `mayRead`/`refuse()` so a shared
  chat admits teammates. Upstream then wrapped several of those same routes in
  try/catch. Every such conflict resolves the same way: **our gate, their body.**
- **Two routes landed in the wrong block.** Upstream's `index.ts` has one
  `parts[2]`-indexed room block; we have two, because gh#66 added the org device
  registry at `parts[3]`. `6f19b76`'s `/append` matched the wrong one and applied
  *without a conflict* — it typechecked as a dead comparison against a `string`
  literal union. `79c8e22`'s `/snapshot` landed right but passed `auth.userId`
  where our `forward` wants the whole `Verified`. **Run `npm run typecheck` in
  `edge/` after every pick**; both of these surfaced there and nowhere else.
- **The guard that only half-covered us.** `b019439` refuses to force-trim at the
  live frontier on docs every device writes at once; `4aacc6d` then made its room
  pattern generation-agnostic, because matching the literal `ws3/` silently
  stopped protecting anything when the room became `ws4`. Upstream has one such
  room. We have two — `orgdev1/{orgId}` is ours (gh#66) and has exactly the
  hazardous shape. Ported verbatim, the guard would have left the registry
  force-trimming at the live frontier. `d864d36` extracts the rule as
  `isConcurrentWriteRoom` next to `livePresence` and `chatRoomAccess`, covers both
  prefixes, and pins it with tests. **This is the general lesson: an upstream
  guard scoped by room name does not know about the rooms only we have.**

Verified at the branch tip: `edge/` typecheck clean, 57 tests green across 7
files (was 46/5 — `update-log.test.ts` came from `6cee4af`,
`session-trim-rooms.test.ts` is ours), `cargo check -p comet-doc` clean.

Not verified: none of this has run against live Durable Objects. The cluster is
upstream's own incident response, so it is load-bearing on their fleet, not on
ours. `edge/scripts/whale-check.mjs` arrived with `6cee4af` and is the tool for
checking a real room.

**Where the next sync starts:** everything through `6cee4af` (2026-08-05) in
`edge/` is now ours. `upstream/main` at `433ff68` (v0.1.26) is still ahead by the
registry-sidebar, iOS, ACP-harness and terminal work — none of it edge.

### The ws4 break (gh#148)

`fb6492c` was held back from the gh#146 cluster on the grounds that it is a
live-data migration rather than a code fix. Both halves of that turned out to
be true, and they point opposite ways: it **is** the only commit in the cluster
that changes which storage the fleet uses, and there is **nothing to migrate**.
Taken here, sequenced after gh#146 so the bump lands on an edge that has stopped
damaging its own storage — a virgin room handed to the old trim/poison bugs
would simply become the next room worth abandoning.

**What actually moves: nothing.** A room name is the Durable Object's identity
(`idFromName`), so `ws3/…` → `ws4/…` allocates an empty object and orphans the
old one. That is survivable because the edge is not authoritative for the
workspace doc — every signed-in device holds a complete local replica
(`WORKSPACE_DOC_ID = "workspace2"` in its own `DocsStore`). The first device to
join the empty room hits the ordinary resubmit-from-version-vector path in
`RoomActor::on_join_ok` against a server whose version vector is empty, uploads
its whole doc, and the rest merge in by CRDT. No script, no cutover window, no
operator step. Pinned by
`abandoning_a_workspace_rooms_storage_is_reseeded_from_the_device`
(`crates/engine/tests/edge_reconnect.rs`), which deletes the room's server-side
doc under a running engine and requires the content back untouched.

The corollary is the rule that matters more than the bump: **a room generation
bump must never bump the local snapshot row id.** They look like the same kind
of break and are not. Do both and there is nothing left to re-seed from, and an
edge-side break that loses nothing becomes real data loss on every device at
once. Recorded on `WORKSPACE_DOC_ID` itself, where somebody bumping the next
generation will actually read it.

**The org device registry does not move.** `orgdev1/{orgId}` is a separate room
with its own lifetime; it never carried ws3's damage, and abandoning it would
blank the one index a teammate needs before they can address the box at all
(gh#66) — the workspace doc cannot stand in for it, being per-user by
construction. Its generation counter is therefore independent, not a mirror of
the workspace doc's.

**A client pinned to ws3 is not an outage.** Worth stating explicitly because
our engines retry a failed join *forever*, so a mismatch that did bite would be
silent — no error, no log line, just a sidebar that never syncs. It does not
bite: clients dial `/workspace/{orgId}/ws`, which names no generation, and the
Worker derives the room from the caller's own auth claim. The room id inside
protocol frames is an echo label neither side routes on — the DO stamps its
`chatId` meta from the Worker's query param at upgrade time, before any
JoinRequest exists, and the Rust client discards the room id on every inbound
frame (`Ack` carries a `BatchId`, not a room). An engine still saying `ws3/…`
therefore lands in the ws4 room and converges. We bumped
`workspace_host.rs`'s derived label anyway, for logs and `EdgeHealth`, and said
in the comment that it is hygiene rather than correctness — the next reader's
obvious question is whether the two must match, and the answer is load-bearing.

**iOS was the one place the bump could actually have destroyed something**, and
it is the general lesson from gh#146 again: *an upstream change scoped by room
name does not know about the clients only we have.* Upstream bumps one string in
one Worker. Our iOS app used the room id for two jobs — the frame label, and the
key for its on-device snapshot (`DocDisk`) — so bumping it naively would have
orphaned the local replica: no instant sidebar render, and any edit made offline
gone. Worse, `DocDisk.prune` protected the workspace snapshot from LRU eviction
by matching the literal prefix `ws3_`; under a `ws4_` name the workspace doc
becomes an ordinary session snapshot and is **deleted** as the 81st-oldest file.
That is the `4aacc6d` failure mode exactly — a guard hard-coding the generation —
except this one fails destructively rather than by force-trimming. So the disk
key is now `workspace2/{org}/{user}`, stable across every room generation and
named to match the engine's `WORKSPACE_DOC_ID`; the prune rule matches that
stable name; and `loadWorkspace` adopts a pre-gh#148 `ws3_…` file once so
nothing is lost on upgrade. The frame label moved to `ws4` alongside.

That iOS session rooms have always joined as the bare `chatId` while the edge
names those objects `s2/{chatId}` is, incidentally, the shipped proof that the
frame label routes nothing.

**`4aacc6d` was a prerequisite, and we already had it.** It exists precisely
because a guard hard-coded the generation: `b019439` refused live-frontier
force-trims on `ws3/` by literal string, and when upstream bumped to `ws4/` the
protection evaporated silently and re-broke the same incident hours later. We
took it in gh#146 as `3809996`, and `d864d36` generalised it to
`isConcurrentWriteRoom` covering `orgdev{n}/` too. Landing `fb6492c` without it
would have shipped exactly upstream's second outage. To stop that recurring by
inspection, room names now come from `edge/src/rooms.ts` — the generation is one
named constant instead of a literal in a route — and `rooms.test.ts` asserts the
generator and the guard still agree, for the current generation and for the next
several nobody has minted yet.

Not verified live: the same caveat as the rest of the cluster. The bump is
observable the moment it deploys — every workspace room in the fleet cold-starts
empty and re-fills from devices — so the thing to watch on the first deploy is
that `GET /workspace/{orgId}/stats` shows a room re-seeding rather than staying
empty, which would mean no device won the race to upload.

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
