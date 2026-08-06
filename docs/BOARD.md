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
- `crates/board/src/overrun.rs` — **new** (H10, gh#70): the wall-clock cap on
  a live attempt, pure. Within / warn / cancel, plus the grace the warning
  buys. The machinery is `sync.rs`'s `enforce_duration_cap`, on the interval
  reconcile — the same clock orphaning rides.
- `crates/board/src/gc.rs` — **new** (H12, gh#72): whose an attempt's checkout
  is (live / held / spent) and when it may go, pure — plus the worktree-root
  measurement `doctor` reports. The machinery is `sync.rs`'s
  `collect_worktrees`, on the same interval clock.
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
  A fresh branch is cut from the route's `base` **fetched from origin**
  (`defaults.base`, `origin/HEAD` by default — gh#67), never from the space
  folder's HEAD: an always-on box's folder sits on whatever ran there last, and
  branching from it hands every agent a stale main. A fetch that fails refuses
  the dispatch rather than falling back; `base = "HEAD"` is the explicit opt-out
  for a repo with no remote, and `doctor` fails a route needing an `origin` the
  repo does not have. A retry neither fetches nor moves anything — an existing
  branch is re-opened on its own commits (`git worktree prune` first, so a
  hand-deleted checkout is a retry rather than a failure).
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
`DispatchTask {taskId, via?, runtime?, model?, account?}` →
`{chatId, cwd, attempt}`, `CancelTask {taskId}` — served in
`crates/engine/src/rpc.rs` off the board service, which executes
dispatch/cancel on its loop thread (`board.db` has one writer).
`ListBoardRuntimes` → `[{name, label}]` lists the runtimes a dispatch can be
pointed at (the canonical set `build_spec` validates an override against) for
pickers in the desktop panel and the CLI. `runtime`/`model`/`account` override
the route's configured runtime, the harness's default model, and the route's
`account` for that one dispatch; the attempt row records whatever the agent
actually ran under.

### Per-run agent accounts (gh#59)

Whose Claude/Codex subscription a dispatch spends is a per-run choice, not an
engine-wide mode. Each teammate attaches their own login under Agent accounts;
a route's `account` (or `DispatchTask {account}` / `comet-board dispatch
--account`) names the slot, and that dispatch burns its owner's limits.

The mechanism is env, not files. `crates/engine/src/agent_accounts.rs`
materializes a slot into a config dir of its own — `{data_dir}/accounts/{slotId}/`,
holding `.credentials.json` + `.claude.json` for Claude and `auth.json` for
Codex — and the run stamps `CLAUDE_CONFIG_DIR` / `CODEX_HOME` at it in the
harness child's env, exactly as `RunControls::chat_id` becomes
`COMET_BOARD_CHAT_ID`. The alternative it replaces (`activate`, which overwrites
`~/.claude/.credentials.json`) is engine-wide and mutates what a *live* run is
reading — a footgun even for one user. `activate` remains, for choosing the
device's own CLI login; a run naming an account never touches it.

The dir is the live copy from then on: refresh writebacks the CLI makes land
there, `read_slots` absorbs them back into the slot file, and usage probes read
the result. A run holds a lease on its slot for its lifetime, which keeps the
usage refresher from rotating a refresh token the CLI is still holding — the
same rule that already applied to the active login.

The account rides `ChatConfig`, not `RunRequest`: a login belongs to the agent,
so every later turn in the chat (steers, review deliveries, an operator typing
into the same session) keeps spending it, and a steer arriving mid-turn cannot
change it. An account that will not resolve **refuses** the dispatch before the
chat exists, and refuses a later run rather than falling back — a silent
fallback bills whoever the device's own login belongs to.

Deliberately not in v1: inferring an account from the WorkOS user who
dispatched. `via` already records who released the work; guessing a login from
it is the kind of clever that bills the wrong person. `comet-board doctor`
checks each route's `account` against the device's saved logins, including the
CLI it belongs to — a Claude slot on a codex route is not lendable, since the
two config-dir variables are not interchangeable.

All four are relay-forwardable (H9): `targetDeviceId` = the box, and a
teammate's laptop reads and drives the box's board without hosting one.

### A teammate's view of the board (gh#66)

A second person in the org gets there through three org gates, all at the edge:

1. **They see the box.** Device rows are published to an org-wide registry
   (`orgdev1/{orgId}`) alongside the per-user workspace doc, so `WatchDevices`
   on a teammate's laptop lists the box — which is what the pane's host sweep
   walks to find a board at all.
2. **They may relay to it.** A device room admits any member of the org that
   claimed it (as a client; only the box's own backend may host it), so the
   forwarded board RPCs reach it.
3. **They may open its chats.** A dispatch marks its chat shared with the org
   (`POST /share/{chatId}`), which is what lets a teammate open the transcript
   and steer the agent. Chats nobody shared stay private to their owner, board
   or not — being in the org does not make someone's own sessions readable.

The chat still RUNS on the box: its session doc names the hosting device, so a
teammate's engine syncs and writes the doc (a steer is a command entry in it)
without ever executing the work itself.

### GitHub App auth (gh#58)

The board takes **either** a personal access token or a GitHub App, and prefers
the App when both are configured. `.env`:

```
GITHUB_TOKEN=ghp_…                        # a personal access token
GITHUB_APP_ID=123456                      # or a GitHub App
GITHUB_APP_PRIVATE_KEY_PATH=/…/app.pem    # (chmod 600)
```

`GITHUB_TOKEN` keeps working exactly as it did, and is still the right answer
for a board watching your own repos. It stops being the right answer the moment
somebody else wants the board on *their* repos: a PAT belongs to an account and
carries that account's whole reach, and a fine-grained one is scoped to a single
resource owner — so two owners would need a classic PAT's blanket `repo` scope.
An App is installed by the repo owner, on the repos they pick, with no credential
changing hands. Rate limits become per installation, offboarding is an uninstall
rather than a rotation, and writes land as `[bot]`, which is honestly not a
human.

**The seam** is `sources/github_app.rs`: `TokenProvider` — `Anonymous` /
`Static(pat)` / `App` — replaces `HttpRest`'s `Option<String>` token. Under an
App the credential depends on where the request is going: a `/repos/{owner}/{repo}/…`
path gets that repo's installation token, and the App's own endpoints get a
freshly signed JWT (RS256, `iat` backdated 60s for clock skew, `exp` 9 minutes
out — inside GitHub's ten-minute ceiling). Two caches, each keyed on what the
fact belongs to: repo → installation, installation → token. Keying the token by
**installation** rather than by repo is what makes six repos behind one
installation cost one mint instead of six, and is what will let one board process
serve several installations later. Tokens refresh five minutes before their
stated expiry.

A **401 under an App re-mints once and retries, then gives up**. An installation
token lives an hour and the world can change inside one, so a stale token is
worth exactly one retry; a genuinely revoked installation answers 401 to the
fresh token too, and a board that kept re-minting would spin against GitHub
forever rather than fail and be fixed. A PAT never takes that path — there is
nothing to invalidate, so its 401 is final, as it always was.

`jsonwebtoken` is the crate's first crypto dependency, pinned to 9.x because
that line signs through `ring`, which rustls already builds into this workspace;
10+ dropped it for `aws-lc-rs` (cmake in the release pipeline) or `rust_crypto`
(the `rsa` crate, and RUSTSEC-2023-0071).

`comet-board doctor` reports which mode is live, the App's slug and every
installation with its account, the private key's permissions, and each repo's
installation and token expiry.

**Pushing** (`git_credentials.rs`) needs a token too, and the token must not be
written into `.git/config` — it expires in an hour and the checkout does not.
`push_url` carries only the username (`x-access-token`); `push_env` points
`GIT_ASKPASS` at `comet-board git-askpass`, which mints at push time and writes
the token to the pipe git is holding. Nothing lands in argv, in `.git/config`,
or in the environment — all three are readable by other processes on a box that,
since #55, several people drive. The box's own credential helper is switched off
for the push, so an hourly token cannot end up cached in the keychain. Who runs
that push is H10 below.

Operator work, not the agent's: register the App, set **Issues: RW, Pull
requests: RW, Contents: RW, Metadata: R** (Contents write is what `merge_pr`'s
`PUT /pulls/{n}/merge` needs), generate the key, make the App public so others
can install it, and drop the PEM on the box.

Deliberately out of scope, each its own ticket: webhooks replacing polling (a
separate delivery path with its own endpoint and secret), and repo
auto-discovery via `GET /installation/repositories` replacing the manual
`[github] repos` list (it changes what "polled" means).

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
- `settled.rs`'s screen-resample half and `gc.rs`'s **pane** logic. Settle keys
  off run-journal events now (H4); there are no panes to collect. Its
  *worktree* half was the half that mattered and its absence was a leak —
  ported at last as H12 below (gh#72), keyed on board state rather than on
  herdr's pane listing.
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

### H9 — Relay-forward the board RPCs — **done** (gh#55)
The four board methods joined `forwardable` in `crates/engine/src/rpc.rs`
(`WatchBoard` also joined `is_stream_method`, so its stream is proxied
item-by-item). Nothing else on the engine changed: the handlers were already
transport-agnostic, and authorization falls out of org membership plus relay
auth exactly as it does for terminals or agent accounts. `crates/engine/tests/
device_routing.rs` covers it end to end — a box hosting the board, a laptop
hosting none, and the laptop reading the box's rows and being refused by the
box's own dispatch guard.

Finding the host needs no configuration. The engine refuses `WatchBoard`
outright when it hosts no board, so a device whose stream ends without ever
delivering a frame has said "not me"; both viewports sweep the candidates from
`comet_proto::view::board::host_candidates` (this device first, then every
registered device in registration order) until one answers. Pinning is there
for when the guess is wrong or two boxes both host a board: the desktop panel's
header chip (with "Automatic" to hand the sweep back) and the TUI's `d`, which
cycles automatic → this device → each device → automatic. Every board call
carries the host, `ListModels` included — the run executes on the host, so the
model catalog a dispatch picks from has to be the host's.

Still one host device by design: moving board rows into the workspace doc is a
different decision, and one host is correct while one box hosts the board.

### H10 — Wall-clock cap on an attempt — **done** (gh#70)
Landed as `crates/board/src/overrun.rs` (the pure decision) plus
`SyncEngine::enforce_duration_cap` in `sync.rs`, with `max_duration` on
`[defaults]` (2h) and per `[[route]]`.

Nothing bounded a *running* attempt before this. No run-duration, token or
cost cap existed anywhere; `attempts.started_at` was stored and read by
nobody. The engine's stall watchdog (`sessions.rs`) hard-stops only a run that
emits nothing — its silence-after-output tier is advisory by design — so an
agent looping and talking ran until somebody looked. The same clock closes the
stranded-`working` row at the other end: an engine crash past its revival
budget settles the chat `Idle`, and with no commits `settled::decide` returns
`StayLive(NoArtifacts)`; orphaning fires only on a *missing* session row, and
that one exists. A dispatch whose brief never reached a chat (no session, no
`saw_working`, deliberately left alone by H2) is closed by the same clock.

The shape:
- **Warn, then cancel.** Past the cap, one prompt into the chat naming the age,
  the cap and the deadline, plus a log line — the stamp goes on the attempt
  whether or not delivery succeeded, so a dead chat cannot buy an eternal
  reprieve. When the grace expires, the chat is interrupted and archived and
  the attempt closes `failed` with an upstream comment naming the timeout
  (`enqueue_outcome_note` — `failed` alone reads as a dispatch that never
  produced an agent). `failed`, not `cancelled`: nobody chose this, and
  `cancelled` would derive the issue back to `ready` as if nothing had run.
- **Grace** is a sixth of the cap, capped at ten minutes and floored at two
  sync intervals — long enough to commit and open a PR, and never shorter than
  the interval that has to notice it.
- **Settle beats cap.** The check runs after `maybe_settle`, so an agent that
  takes the warning and finishes inside its grace closes `done` on its
  artifacts.
- **Wall time.** On the interval reconcile only, exactly as orphaning is: a
  burst of watch events must not age an attempt faster than the clock.
- **Every live attempt, whatever its status.** `blocked` holds a chat and a
  concurrency slot as surely as `working`. The cap bounds the attempt; which
  way it got stuck is the log line's business.

Deliberately not here: token and cost caps. Those need per-run accounting the
board does not have (the engine knows; the board sees sessions), and wall time
is the bound that was actually missing.

### H11 — Dispatched agents push with the board's credential — **done** (gh#68)
#58 built the askpass machinery and left it with no caller, so a dispatched
agent pushed with whatever git credentials the box user had: fine on a Mac
somebody set up by hand, nothing at all on a clean headless box. This is the
caller, threaded through the same harness-env seam `COMET_BOARD_CHAT_ID` and
`CLAUDE_CONFIG_DIR` already use (`RunControls.push` →
`comet_harness::PushCredentials::apply`).

The repo is resolved at dispatch (`DispatchSpec.push_repo`: the task id for a
GitHub ticket, the checkout's `origin` remote for anything else) and stored on
the chat as `ChatConfig.push_repo` — on the chat rather than the run for the
same reason `account` is, since the fix for a review comment next week is a new
run in the same chat and has to reach the same branch. `crates/engine/
push_credentials.rs` turns that repo into an environment, per run.

**Late minting, twice.** `git push` goes through askpass, which mints inside the
push. `gh` has no askpass — it reads `GH_TOKEN` once, at startup — and an
installation token lives an hour while a run does not, so exporting one at spawn
would hand a three-hour run an expired credential exactly when it goes to open
its pull request. Instead a generated `gh` wrapper goes on the front of the
child's PATH and mints per invocation (`comet-board gh-token`, the `gh` twin of
`git-askpass`). The token reaches that one `gh` process's environment, which is
gh's only interface; it is never in the agent's own.

**Scoping.** One repo per run, and it is the attempt's. Because the helper now
answers for every `git` the agent runs rather than for one push the board
issued, `askpass` refuses any prompt naming a host other than github.com — an
installation token answered to a `git fetch https://gitlab.com/…` would be a
credential handed to a stranger. The wrapper defers to a `GH_TOKEN`/
`GITHUB_TOKEN` the operator set and to a non-github.com `GH_HOST`, for the same
reason.

**Every part is optional and fails back to what happened before.** No board
credential, no `comet-board` binary, no `gh`, no repo on the chat: the child is
spawned untouched and the agent pushes as the box user. The PAT path is
unchanged — `token_for_push` hands back the static token, which is what a
self-hosted board on a PAT already pushes with. `comet-board doctor` answers the
question directly with a `dispatched pushes` check.

### H12 — Worktree gc — **done** (gh#72)
Landed as `crates/board/src/gc.rs` (the pure decision + the disk measurement)
plus `SyncEngine::collect_worktrees` in `sync.rs`, with `retain_worktrees` on
`[defaults]` (7d, `off` to disable).

Nothing deleted a worktree before this. `Repos::delete_worktree` was reachable
only from the `DeleteWorktree` RPC: settle, orphan, cancel and retry-replace all
close the attempt row and walk away, so every attempt leaked a full checkout
plus a local branch, forever. And the branch leaked even from the RPC — that
function deleted a branch only when it was named `comet/…`, while the board's
come from `branch_template` and are `board/…`.

The shape:
- **Whose is it.** `gc::standing` reads three states off the task: *live* (any
  live attempt on the task — retries reuse the branch, so a closed attempt's
  directory is usually the live one's), *held* (a pull request still open, or an
  issue still owed — a retry lands on the previous attempt's commits and must
  find them), *spent* (closed upstream, deleted upstream, or marked done, with
  no open PR). Only spent is collectable, and it is read off upstream facts
  rather than off the rendered `BoardState`, so the sweep does not depend on
  having re-derived first.
- **The clock starts when it is freed**, not when the attempt ended: a PR open
  for a fortnight would otherwise be collected the instant it merged. The mark
  is `attempts.collectable_at`; coming back to life clears it, so the next
  window is whole.
- **Wall time, on the interval**, like the cap and orphaning.
- **Never silent.** The mark and the collection are both log lines naming the
  path, a week apart.
- **The branch too.** `delete_worktree` now takes the branch its creator
  vouches for and deletes it when the checkout is still on it (or gone). An
  operator's own branch checked out in there is still off limits, which is what
  the `comet/` test was standing in for.
- **`doctor` says what it costs.** A `worktrees` check reports the checkout
  count, the disk under the root (time-boxed walk; `≥` when it ran out), how
  many the board still tracks, and the retention window in force — the warning
  that makes the leak visible before the disk is full.

Deliberately not here: collecting checkouts the board has no row for (comet's
own `comet/…` worktrees, attempts whose task was reaped). `doctor` counts them,
because the disk does; deleting a directory nothing claims is a bigger decision
than this one.

### Cross-cutting notes
- **Trackers stay authoritative.** State is derived on every read from
  upstream + live attempt; nothing here changes that.
- **Writeback discipline**: dispatch/outcome comments and closes are queued in
  `board.db` and drained by H1's loop; per-repo `writeback` decides at
  delivery. Already ported; just needs the loop.
- **Multi-device.** One engine hosts the board; every other device drives it
  over the relay (H9 above). Moving board rows into the workspace doc stays
  deferred — one host device is correct while one box hosts the board.
- **Upstream tracking**: `git fetch upstream && git merge upstream/main`
  (upstream = zeronsh/comet). Keep board changes additive — new crate, new
  files, short diffs in `rpc/lib.rs` + `engine/rpc.rs` — so merges stay cheap.

## Reference

The herdr-board repo is the spec for behavior: its README documents every
design decision this port inherits (states, provenance, cancellation
contracts, review delivery loop-closing, settle evidence rules, filters,
adoption). When a remaining-work item says "port X", read X *and its README
section* — the comments carry the why, and the why is the product.
