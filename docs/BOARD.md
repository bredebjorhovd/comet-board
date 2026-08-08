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
  `collect_worktrees`, on the same interval clock. `chat_standing` (H30,
  gh#139) asks the same question about the attempt's *chat*, and
  `sync.rs`'s `archive_chats` sweeps it beside the checkouts.
- `crates/board/src/settled.rs` — **new** (H4): the settle decision, pure.
  The evidence hierarchy (PR = the agent's own statement, closes the attempt
  immediately whatever the run's exit said; commits = weaker, close only a
  run that ended cleanly *and* only once they are on origin, gh#69; an
  `Errored` run stays live so its retry is the same attempt) and the reopen
  rule. The machinery around it is in `sync.rs`
  (`maybe_settle`, `rewatch_settled_attempts`, the targeted PR recheck,
  `commits_are_on_origin`) and
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
- `crates/board/src/notify.rs` — **new** (gh#71): who gets told what when a
  dispatched attempt blocks or settles, and the wording each of the three
  audiences gets. The effects are in `sync.rs` (`announce`, `wake_dispatcher`,
  `post_webhook`, `note_blocked`); see §H10 below.
- `crates/proto/src/view/board.rs` — **new** (H7): the view's shared
  derivations — `BoardState` (moved from `comet-board`, glyphs included),
  `TaskRow` (moved, wire contract), plus `Filter`, `sections`,
  `routes_present`/`filter_cycle`, `finished_today`, `row_metadata` — so the
  TUI and the gpui app derive the same rows. `AgentState`/`agent_rows`
  (gh#103) joins those rows to the chats and the session watch;
  `active_rows` (gh#123) merges them with gh#117's `running_rows` into the
  one **Active** group every frontend's sidebar draws.
- `crates/tui/src/board.rs` + the board section of `crates/tui/src/render.rs` —
  **new** (H7): the board pane (`B`), consuming `WatchBoard`, dispatching over
  `DispatchTask`. See §H7 below. The same stream also feeds the sidebar's
  Active group in both frontends (`Row::Agent` here,
  `Shell::render_active_section` in `crates/ui/src/shell/spaces.rs` — §H17).

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
`ListBoardRuntimes` → `[{name, label, harness}]` lists the runtimes a dispatch
can be pointed at (the canonical set `build_spec` validates an override against)
for pickers in the desktop panel, the TUI and the CLI; `harness` is what the
name resolves to, so a picker can tell which agent accounts a runtime could
spend without re-implementing `harness_for_runtime`. `runtime`/`model`/`account`
override the route's configured runtime, the harness's default model, and the
route's `account` for that one dispatch; the attempt row records whatever the
agent actually ran under. `bill` is the acknowledgement that a run spends
somebody else's subscription, which `billing_guard = "require-own"` wants
instead of a refusal — see §H17. `via`/`viaDevice`/`viaUser` are provenance,
never authority — see §H12.

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

One consequence the composer's `/` picker (gh#134) now makes visible: a slot
IS the run's `CLAUDE_CONFIG_DIR`, so what a dispatched agent can invoke is what
`{data_dir}/accounts/{slotId}/skills/` holds — the board's own skill that
`materialize` stamps there (gh#133) and nothing else — plus whatever its
checkout ships in `.claude/`. The user-level `~/.claude/skills` the operator
sees in their own sessions is invisible from inside a slot.

The picker reports that rather than offering the box user's list: offering a
list the run cannot invoke is worse than a short one. So a skill an agent is
*meant* to have belongs in the repo, or is installed the way the board's own is
— written into every slot on every dispatch, byte-compared, never fatal.

Deliberately not in v1, and still not: inferring an account from the WorkOS user
who dispatched. §H12 now records who released the work by name as well as by
chat, and that changes nothing here — guessing a login from either is the kind
of clever that bills the wrong person, and the identity it would guess from is
unverified. `comet-board doctor`
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
window instead of wording around it.

**Commits must be on origin** (gh#69). `attempt_has_commits` is a local
`rev-list` count, so an agent that committed and could not open a pull request
— guaranteed on a headless box with no `gh` credential, which is what gh#68
went on to fix — ended `Completed`, settled on `Evidence::Commits`, and put the
row in `review` while the work sat in one worktree on one box. The crash path
had the same shape: recovery stamps an aborted run `Interrupted`, not
`Errored`, so the errored-runs-never-settle guard never covered it.
`settled::decide` now takes a three-way `Commits::{None, Unpushed, Pushed}`,
and `Unpushed` is a `StayLive` with its own `Why` — logged once per attempt,
naming the branch, and left for the H10 clock to close if nobody acts. The
push check is `SyncEngine::commits_are_on_origin`: a remote-tracking ref that
*contains* HEAD (free, offline, true of any ordinary `git push`, and the only
tier a non-GitHub remote gets), then — event path only, for the same reason the
`pulls` recheck is — one `GET /repos/{repo}/branches/{branch}`, for a push made
straight to a URL, which updates no tracking ref. Containment rather than
existence, because a retry reuses its predecessor's branch. Unproven reads as
unpushed: an attempt that stays live is visible and bounded, a row that says
`review` about work nobody can fetch is the bug. A pull request short-circuits
all of it — GitHub will not open one for a branch it does not have.

`reopened` semantics kept both ways: an
`Errored`→retried run never left its attempt, and a settled attempt whose
chat works again is re-opened in place (refused when re-dispatched, closed
upstream, or marked done). The dispatcher wake (herdr AGE-25) was not ported
with this; it landed later as part of §H10.

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
--source --json]`, `dispatch --task`, `retry --task` (H11), `cancel --task`,
`wait`, `new`, `stats` — speaking the existing typed RPC to the local IPC port
exactly as `comet-tui` attaches, at the board host named by `--device` (H11).
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
- `enter` dispatches a ready row (the operator's dispatch, so no `via`), retries
  a failed one (H11), opens a working/blocked row's chat, and folds section
  headers. `R` retries, replacing a blocked row's live attempt (H11).

- `enter` releases a ready row (the operator's dispatch, so no `via`) — through
  the account picker H12 added, whose first row is the route's own account and
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
  label-picker survives as `--labels`/`--all-issues` on the CLI. H13 took that
  reuse: `WriteBoardConfig {op: adopt}` calls `adopt_with` unchanged, and the
  settings page's Add is that call. H16 took it again, from the other end: what
  `adopt` offers is repos with a checkout *already on the box*, and `onboard`
  (gh#97) is the same writer reached from a repo the box has never seen.

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
that one exists. Since gh#69 it closes one more: an attempt whose commits never
reached origin (`StayLive(Unpushed)`) stays live by design, and this is what
eventually calls it `failed` rather than leaving it `working` forever. A dispatch whose brief never reached a chat (no session, no
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

### H12 — CLI parity once the box is remote — **done** (gh#73)
Three gaps that only bite when the board is not on the machine you are typing
on, and the desktop app is not the only frontend. All three are in
`apps/board-cli` (the third also in `crates/tui`):

- **`--device`.** H9 relay-forwarded the *frontends*; the CLI still hardcoded
  `ws://127.0.0.1:{port}` with no passthrough, so a laptop's `comet-board list`
  could only ever say "this device's board is disabled". It now dials the same
  localhost port — the transport was never the problem, the local engine
  forwards — and carries `targetDeviceId` on every board call, `ListModels`
  included (the run executes on the host, so the catalog a dispatch is checked
  against has to be the host's). `ops::Board` owns the host, so a call that
  forgets it does not typecheck into existence. The flag takes a device *name*
  or id, resolved against `WatchDevices` before anything is sent: a typo costs
  an error naming the fleet, not a call forwarded into nothing, and an
  ambiguous name asks for the id rather than picking a device the operator did
  not choose. `COMET_BOARD_DEVICE` carries it for a whole shell, which is what
  an orchestrator wants — the alternative is threading a flag through every
  call it makes. Deliberately no auto-sweep: the viewports hold a connection
  open and can afford to probe candidates, a one-shot command would pay for it
  on every invocation, and a laptop with no board is a *configuration*, said
  once. The setup commands (doctor, init, adopt) still read this device's own
  config — a route's `repo =` is a local path, which is #66's problem.
- **`retry --task`.** The verbs were list/dispatch/cancel/wait/new/stats/doctor
  and `ops::dispatch` never sent `replace`, so retrying a blocked row from a
  shell meant cancel-then-dispatch — and between the two the row is `ready`,
  where a concurrency cap or another agent can take the slot the retry was
  trying to keep. `retry` reads the row and decides: `blocked` replaces (the
  engine ends the live attempt and releases in one call — `handle_dispatch`'s
  deliberate breach of the one-live-attempt rule), `failed` and `ready` are
  ordinary dispatches, and anything else is left to the engine's own refusal,
  which names the chat. Reading the row is not optional: sending `replace`
  unconditionally would let `retry` end a *working* agent nobody asked to
  interrupt. Same rule as the desktop panel (`crates/ui/src/board.rs`), so a
  row retried from a shell and from the panel takes the same path.
  `crates/tui` gained the pane's half: `R` retries (replacing on blocked), and
  `enter` now retries a `failed` row as the panel's does.
- **`wait --blocked-is-settled`.** `wait`'s default settle set is
  review/failed/done, which is right — an agent pausing for an approval is not
  a result. But a child that asks a question and is never answered reaches none
  of those, so an orchestrator waited until its timeout or forever. `--state
  blocked` was already accepted and always had been; what it could not do is
  *add* blocked, since naming any state replaces the default trio, and
  respelling the whole set to say "call me back on a question OR a finish" is
  the kind of thing nobody does twice. The flag tops up whichever set is in
  play. Not in the default set: `wait` returning on every permission prompt
  would break the contract `docs/agent-conventions.md` teaches.

### H13 — Notifications: blocked has to reach a human — **done** (gh#71)
Landed as `crates/board/src/notify.rs` (who is told what, and the wording)
plus the effects in `sync.rs`. Before it, `notify`/`notify_dispatcher` were
parsed, documented and reported by `doctor` — and read nowhere. Worse, the
one state that most needs a signal produced none: a `blocked` attempt settles
nothing and closes nothing (correctly — the chat holds the context and the
call is the operator's), so no outcome writeback fired, and an agent that
asked a question at 02:00 was discoverable only by looking at the board.

Three audiences, three channels, and the point of the design is that they are
not the same person:

- **The issue.** Entering `blocked` queues a `blocked` writeback — one comment
  saying whether the agent is waiting on an answer or its run died, and what
  to do about each. Keyed `<task>:blocked:<attempt>:<block>` off the new
  `attempts.blocked_count` column, bumped on the *transition* into blocked:
  once per block, so a block that lasts three hours is one comment and a
  question answered at 09:00 followed by another at 11:00 is two. Delivered
  by the existing queue, so GitHub's per-repo `writeback` decides at delivery
  exactly as it does for dispatch and outcome comments. An attempt that blocks
  and settles in the same pass (an errored run whose PR is already open) gets
  the outcome comment only — two comments contradicting each other is worse
  than one.
- **The agent that released it.** herdr-board's AGE-25 dispatcher wake, now
  ported: `notify_dispatcher = true` prompts the dispatching chat when its
  released work settles (or orphans), over the same `Runtime::prompt` review
  delivery uses. The provenance was already on the attempt row
  (`dispatched_by_pane`). Still off by default, and that is the design rather
  than caution — an orchestrator woken by every child it released cannot hold
  a train of thought. Operator-released work has no dispatcher chat, so the
  switch is silent for it by construction, which is why it stays separate from
  the operator's own.
- **The operator, out of band.** `notify` is now real: it switches one webhook
  URL (`notify_webhook`), POSTed `{"event": "on_blocked" | "on_settled", …}`
  with a `text` line for endpoints that render nothing else. One URL, no
  per-service clients — Slack, ntfy, a pager and a two-line relay all already
  accept a POST, and a board holding three credentials it never reads would be
  three more things to be wrong. Five-second timeout, no retry: the writeback
  queue retries because a comment is worth the same tomorrow, and a
  notification is not — one delivered forty minutes late reads as current.
  A dead endpoint logs and is dropped; it never holds a settle open.

`doctor` now matches reality, which was half the bug. Its settle-notice line
no longer claims "only you are notified when released work settles" — nothing
notified you. There is a `blocked notice` line that names the read-only repos
where a block really does show nowhere but the board. And an `operator notice`
line reads the two keys together and is true in each state: *not configured*
(no webhook — a preference, so `ok`, but worded so nobody reads it as a notice
that fires), *on*, *muted* (`notify = false` over a configured URL), and the
one genuine fault — an address that cannot be posted to, where the operator
asked for the notice and every one is being dropped into a log line. Only that
last state fails; a `doctor` that exits 1 over a preference stops meaning
anything.

### H14 — Worktree gc — **done** (gh#72)
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

### H15 — The frontends send an account, and say who dispatched — **done** (gh#74)
`DispatchTask` has taken an `account` since gh#59 and no frontend sent one, so
a dispatch from the panel spent whatever the route said — which on a shared box
means the owner's subscription, whoever pressed enter. Nothing recorded who that
was either: `Dispatcher::Operator` is anonymous by construction.

**The account picker.** The desktop panel's dispatch picker grew a third strip
between runtime and model; the TUI, which had no picker at all, opens one on
`enter` over a ready row. Both are fed by `ListAgentAccounts` **on the board's
host** — the run executes there, and a slot id means nothing on the device that
did not save it, the same reason `ListModels` is fetched with the host
passthrough. Both filter the slots to the harness the row's runtime resolves to,
which is why `ListBoardRuntimes` now carries `harness`: a Claude slot cannot pay
for a codex run (`CLAUDE_CONFIG_DIR` and `CODEX_HOME` are not interchangeable),
and offering one would be offering a dispatch that refuses itself.

Row 0 in both is **the route's own account**, and sends no override — so
enter-enter is exactly what enter did before, and the strip costs a keystroke
rather than a decision. It names the route's account where the row knows one, so
the default is a fact rather than a shrug. Picking a slot is one click and never
itself the release: whose limits a run burns is too consequential to happen by
accident, so the model row (or enter) still does the releasing.

**Attribution, at the strength the transport allows.** Every dispatch from
either frontend now carries `viaDevice` (this device's id) and `viaUser` (the
signed-in email), recorded on the attempt as `dispatched_by_device` /
`dispatched_by_user`. `dispatched_by_user` joins the `TaskRow` contract, so
`list --json` and both viewports can say who released a row; the device id
deliberately stays off the wire, since it names a laptop, not a person. With no
agent in the chain the upstream dispatch comment names the human — "dispatched
by ana@example.com" — where it previously said nothing at all.

These are **claims, not credentials**, and `DispatchOrigin` says so where the
code is: relayed board calls arrive as the device room's owner (§H9), so the box
has no per-call identity to check them against. #66 established that a teammate
may reach the box at all; establishing *which* teammate is the next step, and
these two columns are where a verified identity will land. Until then nothing is
authorized on them, and in particular no account is inferred from them — which
subscription a run spends stays the explicit `account` (gh#59).

Deliberately not here: a per-user default account (that is a preference, and
preferences want a home and a settings surface), and any UI for reading the
attribution back beyond the row field — the panel's dispatch notice names the
account it spent, and the issue comment names the human, which is where people
were already looking.

### H19 — Remote routing surface — **done** (gh#75)
`routing.toml` is a hand-edited file on the box, documented as "not managed
config", and no RPC touched it. Adding a repo, pointing a route at a different
agent account, or lifting a cap was an ssh-and-edit job — fine for whoever set
the box up, a dead end for the teammate #66 just let onto the board.

An RPC pair, `ReadBoardConfig` / `WriteBoardConfig`, both forwardable for the
same reason the board four are: the file lives on the host.

- **`crates/board/src/routes.rs`** is the file half. `read` answers with the
  text, its parse, and *everything* wrong with it; `edit` applies one change.
  Every write goes through `adopt::apply` — the writer H8 already proved out —
  so the discipline is one implementation and not two: it has to parse, it has
  to validate, and the previous contents land in `routing.toml.bak` first. An
  edit that would break the config is refused naming what it would have broken,
  and the file is untouched. `RoutingConfig` gained `Serialize` so a reader can
  be handed the parse; nothing writes TOML from it, because every edit is a
  *text* edit for the reason `add_to_array` gives — the file is full of
  comments explaining choices, and re-serializing throws all of them away.
- **`RoutingConfig::problems`** collects what `validate` refuses on instead of
  stopping at the first. Same checks, same strings; `validate` is now "the first
  problem, if any". An editor that shows one at a time turns fixing three into
  three round trips, and the reader of a remote box's config cannot see the file
  to spot the rest.
- **The edits are a closed list.** `text` (whole file, with an optional `base`
  precondition), `route` and `default` (one typed key each), plus `adopt` and
  `ignore`, which live on the engine because they need the space list and a git
  probe. An unknown key is refused *by name*: a misspelt key in a TOML file is
  not an error — it parses, it is ignored, and the route goes on behaving the
  way it did while somebody believes they changed it. Multi-line-string tracking
  is load-bearing in the key finder, exactly as it is in `header_lines`: a
  route's `prompt = """…"""` containing a line that reads `base = …` is prose,
  and editing it would rewrite the agent's brief.
- **`comet-board routes`** — `list` (routes, problems, what is unadopted),
  `show`, `add`/`ignore`, `set <n> <key> <value>`, `defaults <key> <value>`, and
  `edit` for `$EDITOR`. All over the RPC, so `--device` reaches the box; `adopt`
  stays the local-files command it was. `routes edit` carries the text it
  started from, so a hand-edit on the box in the meantime is refused rather than
  overwritten — this is still a file people edit by hand.
- **Settings → Board routing** (`crates/ui/src/settings/routing.rs`) is the
  desktop half: the routes, the problems as warning strips, per-route Runtime /
  Account / Cap, and the unadopted list with Add and Ignore. It finds the host
  by the same contract the board panel sweeps on — the engine refuses a board
  method outright when it hosts no board, so a candidate that errors has
  answered "not me" — which keeps it independent of whether the board panel has
  ever been opened.
- **Not touched: `.env`.** Credentials are the other hand-edited file, and
  moving secrets over the wire is a different decision from moving routes.
  `doctor` still says which keys are missing.

### H16 — A GitHub-only board is not a broken one — **done** (gh#96)
Inherited straight from herdr-board, where Linear always existed: `doctor`
reported `FAIL LINEAR_API_KEY missing — add it to …/.env` on a board that polls
GitHub only. Everything worked; the report said otherwise, which on a fresh box
is the first thing anyone sees.

The credential now reads the way `operator notice` does — three states, and only
the ones with a consequence fail:

- **Absent, and no route matches on `linear_team`** — `ok`, *not configured —
  the board polls GitHub only*. A supported configuration, worded so nobody
  reads it as half-installed.
- **Absent, but a route matches on `linear_team`** — `FAIL`, naming the teams.
  The same shape the GitHub credential already had (`ok` until repos are
  configured): without the key those tickets never arrive and the route can
  never fire, silently, which is what `doctor` is for.
- **Present** — probed against the API (`Linear::viewer`, injected into the
  check so it is testable off the wire). Accepted names who the board polls as;
  rejected fails with Linear's own reason. An API that could not be *reached* —
  a `reqwest` error, or a 429 — is `not checked`, never a rejection: failing a
  laptop on a train is the same false alarm from the other direction.

`linear review state` is not printed at all on a board with neither a key nor a
Linear route, on the rule the per-route `account` check already follows (gh#59):
a board is not told about a feature it is not using. `init` stops listing
`LINEAR_API_KEY` beside the GitHub credential as something to "add" and offers
it instead.

Empty reads as absent everywhere, which `config::credential` already did for
both the shell and the file — now with a test that says why: the box wizard
writes `LINEAR_API_KEY=` when the stage is skipped with Enter, and a skipped
stage has to look exactly like a board nobody configured. Same for the App pair,
where an empty `GITHUB_APP_ID` would otherwise read as "half configured".

### H17 — `onboard`: clone, space, adopt in one verb — **done** (gh#97)
Putting a repo on the board took three mechanisms that knew nothing about each
other: a clone somebody made on the box by hand, a `createSpace` from the
desktop (or, the night before this landed, a hand-built RPC seeder), and
`comet-board adopt`. The App side had already stopped needing a human —
all-repos installations — and #75 made routing remote. The clone and the space
were the last thing that still wanted a shell on the box.

`comet-board onboard <owner/repo> [--dir <path>] [--labels a,b | --all-issues]`,
and every step of it happens **on the device that hosts the board**:

1. **Resolve** against GitHub under the board's own credential. Not a
   formality — the laptop running the command usually has no GitHub credential
   at all, so resolving locally would answer about the wrong world; and a repo
   the App cannot see is one that would clone, get a space, get a route, and
   then poll nothing forever. A refusal names whoever can fix it, which under an
   App is the *installer* and not the operator: nothing on the box can grant an
   installation, so sending them to `.env` would be sending them nowhere.
2. **Clone**, with the askpass minting #68 built (`clone_env` →
   `git_credentials::agent_env`). The board's App is the credential that will
   push this repo's branches; it should be the one that fetched it. The URL that
   authenticates carries `x-access-token@`, so the remote is rewritten to the
   canonical one afterwards — a checkout outlives the clone that made it, and
   the human who opens that folder should find the remote they would have typed.
   `adopt::github_slug` learned to read the userinfo form anyway, because an
   interrupted onboard must not leave a checkout that detection cannot see.
3. **`createSpace`** — the same op the desktop picker sends, with
   `git_detected` stated rather than guessed (we just cloned it).
4. **Adopt** — `adopt_with` unchanged, through the same validating writer.
   Deliberately *not* via `WriteBoardConfig {op: adopt}`: that path detects
   through the spaces **watch**, which has not necessarily observed the row
   created three lines earlier, and an onboard that raced its own space would
   report "not on the unadopted list" about a repo it had just cloned. The
   polled/routed decision itself is shared — `adopt::missing_for`, factored out
   of `detect` — so the two surfaces cannot disagree about what "on the board"
   means.

**Idempotent at every step**, because the failure it exists to remove is a
*half*-onboarded repo: a clone with no space, a space with no route, a route
for a repo nothing polls. Re-running has to be the repair, not a second mess.
An existing checkout of the same repo is reused, an existing space for that path
is reused (`create_space` dedupes on `(device, path)` anyway, but silently —
reading the row back is what keeps the reply's `spaceId` honest), and a repo
already both polled and routed says so and writes nothing. What is *not* reused:
a directory holding something else. A checkout of a different repo is the
dangerous case — every step downstream would succeed, and the board would
dispatch this repo's issues into another repo's code — so it is refused.

- **`crates/board/src/onboard.rs`** holds the decisions and the report; the
  engine holds the effects, because the clone is `repos.rs`'s and the space is
  the workspace doc's. `Repos` gained `clone_to` (exact path, credential
  environment, canonical remote, and a failed clone cleaned up so a retry is a
  clean retry) and `origin_url`.
- **`OnboardRepo` / `ListAppRepos`**, both forwardable for the reason the
  config pair is: all of it belongs to the host. Two blocking phases inside the
  handler rather than one — the GitHub clients hold `Rc`s and cannot cross the
  await the clone needs — which costs one extra installation-token mint per
  onboard and is the right price for not holding a `!Send` value across a
  `git clone`.
- **`ListAppRepos` is the App's grant**, not the operator's repos: exactly the
  set the box can clone and the loop can poll, gathered across every
  installation under installation tokens (`/installation/repositories` is the
  one endpoint that answers *about* an installation and so names no repo to
  derive its credential from — hence `AppAuth::token_for_installation`). Repos
  already on the board stay in the list rather than being filtered out; "is this
  one already set up?" is half of why anybody opens the picker.
- **Settings → Board routing** grew the "Onboard a repo…" panel: the App's list
  with a button per row, plus a free-text field, which is not a fallback for a
  broken list — a board on `GITHUB_TOKEN` has no installations to enumerate at
  all, and the picker would otherwise be empty for it forever. A `--dir` field
  beside it, expanded against the *box's* home.
- **Writeback is reported, never set.** It is off by default on purpose —
  writing to somebody's issues is not a thing to start doing because a repo was
  pointed at the board — so onboarding says where it stands and leaves the
  decision where it was. Same for an archived repo and one with issues disabled:
  both would otherwise be discovered as a board that stays empty.

### H18 — Never spend someone's subscription silently — **done** (gh#101)
gh#59 made *which* account a dispatch spends explicit and gh#74 made every
frontend send one, and between them they left the case they had just made
visible unaddressed: a dispatch that names no account runs on the box's own CLI
login. On a shared box that is the owner's subscription, whoever pressed enter.
The teammate did not know they were spending it; the owner found out on their
usage page.

**Who a run bills** is resolved at dispatch: the named slot's
`AgentAccount.email`, or — when no slot is named — the box's own login, which is
the *active* account for that harness and is displayed as the operator's. It is
recorded on the attempt (`attempts.billed_to`) and joins the `TaskRow` contract
as `billed_to`, rather than being looked up from `account` on demand: a slot id
means nothing to a reader who has not saved that login, and the box's own login
can be switched under a run that is still going. A run is **cross-billed** when
that email differs from the dispatcher's `viaUser` claim; two unknowns read as
"not cross-billed", because an unattributed dispatch names nobody to have
wronged.

**`[defaults] billing_guard = "warn" | "require-own" | "off"`**, per-route
override, parsed like `max_duration` — an unrecognised value is refused by
`validate` rather than falling back silently, since a typo would read exactly
like the default and un-arm a route somebody deliberately set to `require-own`.

`warn` (the default) says so everywhere and releases anyway:
- both pickers mark a selection that cross-bills with a warning treatment and
  the text *bills brede@tally.no* — **including row 0**, which is exactly the
  chip an enter-enter release lands on without anybody having chosen it. Row 0's
  effective slot is the route's account, resolved against the host's own
  `ListAgentAccounts`, because `Route default · 8f2c1d0a` answers nothing;
- `comet-board dispatch` / `retry` print one line before releasing — *this run
  bills brede@tally.no's Claude — pass --account <your slot>*. The CLI resolves
  it itself rather than reading it back off the reply: by the time `DispatchTask`
  answers, the worktree is cut and the agent is running on that account. This is
  also why the CLI now sends `viaUser` (from the local `AuthStatus`) — it is a
  frontend like the other two, and without it the guard has nothing to compare;
- the upstream dispatch comment appends *· on brede@tally.no's subscription*, so
  the record is public to both parties instead of living on one usage page;
- `row_metadata` appends *· bills brede@tally.no* for the attempt's whole life —
  outside the per-state arms, because a fact that survives the row changing
  section does not belong inside the match on which section it is in.

`require-own` refuses instead, in `handle_dispatch` **beside the concurrency
cap** — before any attempt row exists, because a refusal that left a `failed`
attempt behind would cost the operator exactly the cleanup this mode exists to
avoid. The override has to *name* the payer: `--bill <slot>` (which also selects
the account) or `--bill <email>` (the only spelling available when the login is
the box's own and has no slot id). In the panel the confirm is reactive — the
mode lives in the host's `routing.toml`, which the panel does not read, so the
only honest way to ask "do you mean it" is to ask after the box has said it
minds. The refusal carries `view::board::REQUIRE_OWN_REFUSAL` so the panel can
tell it from every other dispatch failure without parsing prose.

**This is a seatbelt, not a lock**, and every surface says so in the words that
stay true afterwards. The match is claim-vs-slot-email: a frontend willing to
misreport its signed-in user walks straight through `require-own`, because
relayed board calls arrive as the device room's owner (§H9) and #66's verified
identity is what will change that. It is worth having anyway — the failure it
exists for is nobody noticing, not somebody attacking. `doctor` reports the mode
the way it reports the notices, never failing, and worded so `off` reads as the
choice it is on a box where one person's plan pays for everything.

Deliberately not here: token or cost caps (§H10's note still stands — those need
per-run accounting the harnesses do not expose), and inferring an account from
the WorkOS user who dispatched. The guard *compares* the claim; it still never
authorizes on it, and which subscription a run spends stays the explicit
`account`.
### H21 — The pinned orchestrator — **done** (gh#104)
Landed as `[defaults] orchestrator_chat` plus `notify::orchestrator_message` /
`SyncEngine::wake_orchestrator`, a `WatchBoardOrchestrator` stream, and a "Pin
as orchestrator" item on the session context menu of both viewports.
`docs/orchestrator.md` is the brief to open the pinned chat with.

This is herdr-board's AGE-24 topology, made a product concept instead of
something a human wires by hand every time. It is also how this fork was
built: one long-lived agent that dispatches board work, is woken on settles,
reviews, merges and backfills.

H13's `notify_dispatcher` was the closest thing and it is the wrong shape for
this. It wakes *whoever released each task*, which is right for a chat waiting
on the one thing it released and useless for an agent whose job is the board:
work an operator releases from the panel has no dispatcher chat at all, and
work a sibling releases wakes the sibling. So the pin is a **superset target**,
not a second switch on the same channel — every settle, block, orphan and cap
warning is prompted into it, over the same `Runtime::prompt` review delivery
and the dispatcher wake already use. When the orchestrator *is* the dispatcher
it is told once, in the dispatcher's words: the more specific truth wins.

Wording is shared with the settle notice (`notify::settled_block`) rather than
written twice, so the one description in `docs/agent-conventions.md` stays the
contract for both audiences. The orchestrator's copy differs in exactly two
ways, both because it did not release the work: the lead line does not claim it
did, and it names who did.

The cap warning is the one event that is *not* on `Signal`, and deliberately:
`Signal` means "an attempt is over or stuck", which is what the webhook and the
issue comment are about. A cap warning is about a run that is still going, and
the only window in which reading its chat can still change how it ends — so it
goes to the orchestrator and not to the operator's pager.

Guardrails, all of them stated rather than incidental:
- **No workspace slot.** The orchestrator is a chat somebody opened, not an
  attempt, so it holds nothing — while everything it releases counts against
  the caps exactly as anyone's does.
- **It bills its own chat's `account`.** Nothing special, and nothing new:
  §H18's billing guard reads its dispatches exactly as it reads anyone's, so an
  orchestrator releasing work on somebody else's subscription is warned about —
  or refused — on the same terms a human at the panel is.
- **Exempt from `max_duration`.** It is supposed to outlive every attempt on
  the board, so the clock that stops a looping agent must not stop it. Stated
  in `enforce_duration_cap` and stamped so the log says it once. The exemption
  is on the chat, which makes pinning a *board-dispatched* chat the one real
  misconfiguration — so `doctor` fails on it by name rather than letting a
  child run forever.
- **Notice volume is the budget.** One prompt per event, no polling, no retry.
  An agent that lives forever has no other bound on what it costs.
- **Unpin is the kill switch.** The notices stop; the chat is an ordinary chat.

Delivery to the frontends is its own stream rather than a field on `WatchBoard`
or a read of `ReadBoardConfig`. The pin marks a row in the session list, which
is on screen before any board panel is opened, and `ReadBoardConfig` shells out
to git once per space — the wrong price for a glyph. `WriteBoardConfig`
republishes it as the write lands, so pinning from the app is visible when the
click returns rather than on the board's next reread; the loop's reread still
covers an `$EDITOR` over ssh.

### H20 — Live agents in the sidebar — **done** (gh#103)
*(Since gh#123 — §H26 — this group and §H24's draw as one **Active** section;
every rule below is unchanged, minus the header.)*
In herdr every working agent was a pane, so the pane list *was* the presence
list and presence cost nothing. Here a dispatched agent is a chat among chats:
three of them are three rows somewhere in a recency-sorted list, indistinguishable
from the session you opened yesterday, and tracking them meant the board pane or
nothing.

Both sidebars grew an **Agents** section between Spaces and the sessions. One
row per live attempt: the issue identifier as the title, the branch underneath,
and elapsed against the route's cap on the right. Pure presentation — everything
it draws was already streamed, and nothing here dispatches, settles or decides.

- **`comet_proto::view::board::agent_rows`** is the whole derivation, shared as
  the architecture rule requires: `WatchBoard` rows joined to the chat rows and
  the session watch. Membership is "`working` or `blocked` **and** has a chat
  id", which is why a row leaves on its own — settle, cancel and orphan all end
  the attempt, clearing `chat_id` and moving the row out of both states in the
  same frame. The chat stays findable under its space, as it always was.
- **The state is the session watch's, not the row's** (`AgentState`). The board
  is a sync cycle behind, and it calls a dead run and an agent asking a question
  both `blocked` — correctly, since both hold a chat and a slot, but they want
  different things from a person. The sidebar splits them: a spinner, a blocked
  badge, an errored glyph. Staleness-gated through `effective_indicator`, so a
  crashed backend cannot leave an eternal spinner; the row's own state is the
  fallback for a chat whose session mirror does not exist yet.
- **Blocked floats, with a count on the header** — the board's section-order
  rationale, and the same ranking `attention_rank` gives chat rows. Under it,
  longest-running first, which is stable because that order is start order.
- **`TaskRow.max_duration_secs`** is new on the wire. An elapsed counter says
  half of what it knows without the cap beside it ("1h50m" means one thing under
  two hours and another under six), and the routing config lives on the board's
  host — a laptop reading a relayed board has never seen it. Past the cap the
  counter turns and bolds: gh#70's clock is about to end that attempt, and the
  number is the reason.
- **The desktop's board subscription is now standing.** It was lazy — no RPC
  until the dock was first opened — and a presence list that only works after
  you have visited the board is not presence. `BoardPanel` is built with the
  shell and observed by it; the host sweep is unchanged and bounded, and it is
  what `comet-tui` has always done (its board stream has been standing since
  H7).
- **The TUI pays one wake-up a second** while a live agent row is on screen
  (`App::counting`), which `animating` does not cover: a *blocked* agent
  animates nothing, and its age would otherwise sit at whatever the last frame
  happened to catch. The row carries the start instant, not the age, so the
  draw reads the clock and nothing rebuilds.

Deliberately not here: acting on a row. Enter/click opens the chat and that is
all — retry, cancel and dispatch stay in the board pane, which is the deep view
and has the confirmations. A glance that can kill an agent is a glance nobody
trusts.

### H22 — Commits carry the dispatcher, not whatever the box improvises — **done** (gh#107)
The box had no `git config user.*`. Git does not stop at that: the first
dispatched agent invented an author, the commits went up under an address
belonging to no GitHub account, and Vercel's contributor check refused to deploy
the push. Everything downstream of the commit — attribution, the deploy gate,
`git log` — was reading an identity nobody set.

Three parts, in the order somebody meets them:

- **`doctor` reports the box's identity** (`git identity`). Read with `git -C
  <config dir> config --get user.name/user.email`, deliberately from outside any
  checkout: a repo-local override answers about that repo, and the question is
  what the *next* worktree the board cuts inherits. No name or no email is the
  one state that FAILs — an anonymous box is not a preference, it is a box
  nobody finished setting up — and the failure names the command to fix it. A
  `<id>+<login>@users.noreply.github.com` address passes naming the account it
  attributes to. Anything else passes **with guidance**, never a failure: whether
  an address is on an account's verified list is `GET /user/emails`, a
  user-scoped call the board's App may not make, and failing every operator who
  uses their real work address would be the gh#96 false alarm from the other
  side. The box wizard (`scripts/box-setup-wizard.sh`, tracked here as of this
  change) gained the matching stage, so a fresh box is pinned before it ever
  dispatches.
- **Per-dispatch authorship.** The attempt already recorded `dispatched_by_user`
  (gh#74); `[users]` in `routing.toml` maps that identity to a git author
  (`"ana@example.com" = "22494697+ana@users.noreply.github.com"`, or the
  `Name <email>` form git itself prints), and `build_spec` resolves it at
  dispatch time — the agent doing the committing knows nothing about who
  released it. From there it rides exactly where the push credential rides
  (gh#68): onto the chat config, so the fix for a review comment next week is by
  the same person as the first commit, then onto the harness child as
  `GIT_AUTHOR_NAME`/`GIT_AUTHOR_EMAIL`. **Author only** — the committer stays
  the box's pinned identity, which is what actually happened and what GitHub
  renders as *authored by them*. A file rather than a directory service because
  a two-person box does not need one, and because nothing on the box can answer
  "which GitHub account is this person?" on its own.
- **The two halves are independent.** An author with no App credential still
  authors (it just pushes as the box user); a credential with no `[users]` entry
  pushes as the App and commits as the box, which is what every board did before
  this. `doctor`'s `dispatch authorship` line prints in both states for the
  reason the duration cap does: "everything lands as the box" and "the map is
  working" look identical on GitHub until somebody reads the commit list.

**Why this reaches deploys at all.** Vercel attributes a deployment to the
commit's *author*, and on a team plan that attribution is a gate: an author
address that resolves to no GitHub account — or to an account that is not a team
member — can have its deploy refused rather than queued, and the failure appears
on the deployment, not on the push. Two settings decide the rest, both on
Vercel's side and neither of them ours to set: whether deploys created by a Git
*bot* (an App-authored commit, which is what a board pushing under its App can
produce) are built at all, and who counts as a contributor. What the board owes
them is a truthful, linkable author on every commit, which is what this section
is. A commit authored by a mapped teammate satisfies the gate *for that
teammate* — which is the point: their work deploys under their name, not the
box's.

Nothing here is authority. A commit author is a claim anybody can write by hand,
`dispatched_by_user` is unverified provenance (§H15), and neither decides what a
run may spend or push — that stays the explicit `account` (gh#59) and the board's
own App credential (gh#58).

### H23 — The board in your pocket — **done** (gh#114)
herdr's answer to "on the phone" was Tailscale plus mosh into a terminal. comet
already had a native iOS app signed in against the edge and syncing chats; the
board was the missing screen, and with it the only surface you can reach from a
parking lot.

**The load-bearing unknown was the transport, and it was real.** `WatchBoard` is
a *stream*, and `DeviceRelayClient` — the phone's ControlRpc client over the
device-room relay — only understood unary `{ok}`/`{err}` replies. An `{item}`
frame fell through to "unexpected reply" and a subscription hung until its
deadline, so no engine subscription had ever been reachable from iOS. The client
grew the other half of `comet-rpc`'s client: `subscribe` returns an
`AsyncThrowingStream`, `{item}` yields, `{done}` finishes, `{err}` throws, and
dropping the consumer sends `{id, cancel: true}` so the host stops producing
into a socket nobody reads. `E2ERunner.probeBoardStream` is the regression: on a
device that hosts no board the engine *refusing* is a pass — what it is testing
is that a ServerFrame comes back at all, since a hang is what the old client
looked like.

- **Finding the host.** The desktop asks its own engine to forward with
  `targetDeviceId`; the phone has no engine, so it dials each candidate's device
  room directly and calls the same relay-forwardable method there. Same sweep,
  same rule for ruling a candidate out (a stream that ends without ever
  delivering a frame said "not me"), minus the `None`-is-this-device entry that
  a viewport with no local board cannot use — `boardHostCandidates`.
- **The subscription is standing**, not opened with the board screen. gh#103
  made that correction on the desktop for the same reason: the Agents section on
  Home is presence, and presence that only works after you have visited the
  board is not presence.
- **The view derivations are ports, not approximations** (`BoardModels.swift`
  cites each one): section order and glyphs, `finished_today`'s local-midnight
  bound on `done`, `format_elapsed`/`format_cap`, the `billed_email` /
  `cross_billed` / `bills_label` vocabulary, and `agent_rows` whole. What is
  deliberately not ported is terminal layout — `row_metadata`'s fixed-width
  column block pads to a monospace grid that does not exist here, so the
  *content* decisions inside `state_metadata` come across as `BoardRowDetail`
  and the row view lays them out.
- **The dispatch sheet asks two questions**, runtime and account, and the
  account one matters more here than anywhere else: on a desktop the picker is a
  popover you arrow through, on a phone enter-enter is a thumb tap, and the row
  a tap lands on without anybody choosing it is the route's default — exactly
  the release that can quietly spend a teammate's subscription. So the chips
  resolve to emails rather than slot ids, and the sheet opens at the large
  detent because a detent that hides the account row is a picker that always
  lands on the default. A `require-own` refusal (gh#101) comes back as the
  confirm the CLI spells `--bill`, never as a dead end.
- **Deliberately not on the phone**: the model picker (a dispatch with no
  override runs the harness default, which is what the route already meant, and
  nobody changes models at a bus stop) and the `f`/`/` filter cycle (a keyboard
  affordance). Cancel is behind a long-press on the board's own rows, on the
  desktop's rule that a glance which can kill an agent is a glance nobody
  trusts.

Verified end to end from the simulator against a real headless box over a
`wrangler dev` edge (`-e2e-board <repo>`): board attached over the relay, rows
arrived by stream, a ready row dispatched and moved to `working` with its branch
cut and `started_at` set, the agent row derived, and `replace` ended that
attempt and released a second one (`attempts=2`).

**Distribution, for the operator.** Personal-device sideload via Xcode free
provisioning works today and re-signs every 7 days. TestFlight needs the $99
Apple Developer account — the same one gh#100's signing tier wants, so it is one
purchase for both.

### H24 — Unmanaged runs are visible, and delegation goes through the board — **done** (gh#117)
*(Since gh#123 — §H26 — this group and §H20's draw as one **Active** section;
every rule below is unchanged, minus the header.)*
The first real orchestrator session asked for two agents in a space, and the
orchestrator raised two of its harness's *own* in-chat subagents inside its run
instead of dispatching. Work was genuinely running on the box — editing a repo,
holding checkouts — with zero presence anywhere: no attempt rows, so §H20's
Agents section drew nothing; no caps, no billing chips, no settle tracking. The
operator's question was "are they even alive", and the only answer was `pgrep`
over ssh.

Two halves, because the hole was two holes.

**Presence for every working chat.** All three sidebars grew a second group,
**Running**, under the Agents: any chat the session watch calls `Working` or
`AwaitingInput` that is *not* a live board attempt — the pinned orchestrator, an
ad-hoc agent chat, anything somebody started by hand. It is the same join §H20
does, minus the board row, which is why it costs nothing: the session watch
already streams a status for every chat.

- **`comet_proto::view::board::running_rows`** is the derivation, shared like
  `agent_rows` beside it (and hand-ported to Swift in `BoardModels.swift`, as
  the iOS half of §H23 is). Membership is the live indicator and nothing else,
  staleness-gated through `effective_indicator`, so the group fills within one
  watch frame of a run starting and empties within one of it stopping. **No
  board is required** — a box hosting none subtracts nothing and shows its whole
  live list, which is the case the group matters most in.
- **The two groups partition the box's load.** A chat claimed by a
  `working`/`blocked` row belongs to Agents, which knows its issue, branch, cap
  and bill; drawing it in both would double-count what is running. The
  subtraction reads the board rows directly rather than `agent_rows`'s output,
  so a claimed chat stays out even in the case that drops it from the other
  list.
- **The row says only what is knowable**: the chat's own title (there is no
  issue behind it), elapsed since the *run* started off the session mirror —
  not since the chat was created, which for a long-lived orchestrator is days —
  and a blocked badge in words, since no identifier is there to recognise it by.
  One line, not two: an agent row's sub-line carries its branch, and this has
  none. No cap, because nothing bounds a run the board never released.
- **Staleness has to expire the row, and staleness arrives as no frame at all.**
  A backend that died mid-run sends nothing ever again. The TUI rebuilds its
  rows on the counter tick rather than only on updates, and the desktop's board
  ticker redraws once more after the last live thing goes quiet — without that
  the frame the row is gone from is never painted.
- **Not counted: subagents.** The brief asked for "· 2 subagents" on a row if
  the harness stream exposes it. It does not: the Claude normalizer
  (`crates/harness/src/claude/normalize.rs`) drops every frame carrying a
  `parent_tool_use_id` before it leaves, deliberately (a background Task runs concurrently with
  the parent's text stream and folding it in would split a contiguous text
  block). Nothing about a subagent reaches the session mirror, so counting them
  would mean a new streamed field through harness → engine → doc → three
  frontends. Left out rather than faked.

**The brief teaches tickets-first.** `docs/orchestrator.md` said never dispatch
speculatively and never said the inverse, so an orchestrator obeying it to the
letter could still bypass the board entirely. It now names the rule (work you
delegate goes through a ticket; `comet-board new "title" --dispatch` is one
line) and the anti-pattern explicitly: in-chat subagents are for reading, and
anything that lands a commit is a ticket. The same paragraph is in
`docs/agent-conventions.md`, which is the canonical text every runtime gets —
the anti-pattern belongs to every dispatching agent, not only the pinned one.

Deliberately not here, on §H20's rule: acting on a running row. Opening it opens
the chat, which is where answering it happens. A glance that can kill an agent
is a glance nobody trusts — and these rows have no attempt to cancel anyway.

### H25 — The space selector asks "which repo?" — **done** (gh#118)
The space picker was machine-first: it offered *this device's* folders, and
reaching the box meant already knowing that hosts and spaces exist. Upstream that
is right — comet is a mesh of your own machines. For this fork it is backwards:
work lives in GitHub repos and the box is where they run, so the front door is a
repo list and everything else follows from picking one.

- **One list, repo-shaped.** The union of every space you have (named by its
  repo where the box could resolve one, box first) and every repo the board's
  App can see that has no space yet, marked "not connected yet". Search matches
  the repo name people actually say, not the owner they rarely do — `comet`
  finds `bredebjorhovd/comet-board`.
- **Picking a connected repo opens its space**, on whatever device it lives on.
  There is no host step because there is nothing to ask: a space knows its
  device. This is the part that makes the phone usable — it hosts no folders at
  all, so a folder browser could never have been its front door.
- **Picking an unconnected repo runs `OnboardRepo` inline** (gh#97's verb:
  clone on the box, `createSpace`, adopt), with its progress and its refusals in
  the picker rather than a bounce to Settings. On success the space row is echoed
  optimistically and you are standing in it, with its issues on the board.
- **The box is the default target**, and the sweep is what knows. The picker
  asks every device `ListRepoSpaces` and keeps *all* the answers rather than
  stopping at the first: a device hosting no board refuses before it does any git
  or GitHub work, so the sweep costs one cheap round trip per non-host and
  answers "how many boards are there?" for free. One → clone there silently.
  Two → the one question the picker asks.

**The new RPC exists because of a gap in the doc.** A `Space` knows its device
and its folder but not its *repo*, and the folder cannot supply one — `~/src/comet`
is a name, not an owner. Only the device holding the checkout can ask git. So
`ListRepoSpaces` (relay-forwardable, board-hosts-only) answers with both halves
the frontends cannot compute: this host's `space → owner/repo` links, and the
App's grant (`ListAppRepos`, reused). The grant is best-effort — a board on a
`GITHUB_TOKEN` has no installations to enumerate (gh#96), which is a supported
board, not a broken one — so it degrades to a note beside a list of spaces that
is otherwise complete.

- **`crates/proto/src/view/repos.rs`** is the merge, the order and the search,
  pure and tested, for the same reason the rest of `view` is: three frontends, and
  a picker that ordered its rows differently on each would be three products.
  `RepoRows.swift` ports it rule for rule (the Rust tests are the spec), as
  `BoardModels.swift` ports `agent_rows`.
- **`comet_board::onboard::space_links`** is the git half, injected-probe
  testable exactly as `adopt::detect` is, and deliberately *not* filtered by the
  routing config: "what repo is this space?" is true whether or not the board
  watches it. Whether it is adopted stays `adopt::missing_for`'s single answer.
- **Local folders stay reachable.** The folder browser is the second door, in
  the same card on the desktop (→ / the rail's "This device") and one tap down on
  the phone. A scratch directory that is nobody's repo is a real place to work,
  and the repo list cannot offer it.
- **Not done: the TUI.** It has no add-space surface at all today — spaces get
  made from the desktop, the phone, or `comet-board onboard` — so "the TUI's
  picker follows if cheap" was not cheap; it is a new screen, not a re-shaped one.

### H26 — Presence tells the truth, or says it cannot know — **done** (gh#126)

Eight space rows read "@Tokenmaxxer9000 · offline" (amber) while the box's
orchestrator had run for eight hours. Live diagnosis found the actual outage
one layer down: the Cloudflare account is on the Workers **free** plan, and the
15s presence beats keep the workspace/org Durable Objects permanently awake
(they can never hibernate), so the free tier's daily DO *duration* allowance
burns out mid-afternoon — from then until the daily reset the edge answers
every DO request with a 500 (`Exceeded allowed duration in Durable Objects
free tier`, caught live via `wrangler tail`). All rooms on all devices die at
once: the box goes genuinely unreachable, the Mac goes deaf, and the sidebar's
loudest signal accuses the box. The plan upgrade is the operational cure; the
code changes make the *label* stop lying whatever the outage:

- **The read is three-state now** (`comet_proto::view::host_presence`, pure +
  tested): a lapsed heartbeat is `Offline` (amber) only while THIS viewer's
  engine can hear — at least one sync room live. Deaf (both rooms down, signed
  out, local mode) renders `SyncDown` — "@ box · sync down", muted — indicting
  the pipe, which is the thing the viewer actually knows is broken. The gpui
  app polls its own engine's `EdgeHealth` every 15s to know which it is. One
  presence window (70s) is now shared by every surface via
  `view::PRESENCE_STALE_MS` — the TUI's was 45s, a real cross-surface
  disagreement.
- **The beat lost its silent failure mode** (`crates/sync/src/room.rs`): the
  `%EPH` presence sub-join was fire-and-forget — sent once per session; a
  `JoinError` only warned, an unanswered join left `joined_eph` false forever,
  and every outbound heartbeat was then dropped while doc sync stayed
  perfectly healthy. The session now re-sends the join every 15s until it
  lands (and on liveness-probe answers). Room test: swallow the join twice,
  presence must still come up.
- **The census names presence** (gh#116's `EdgeHealth`): new
  `workspacePresence`/`orgPresence` fields, read from the room clients' new
  `%EPH`-joined flag. `summary()` calls out "presence dead on … (doc sync is
  up; this device will read offline elsewhere)" — the wedge that used to be
  indistinguishable from 4-of-4-live. `comet status` and doctor inherit it.
- **The exit criterion is a test**: the gh#116 fake edge now broadcasts room
  frames between members, and a two-engine test (box + viewer, same user)
  asserts the box's heartbeat lands in the viewer's device row, survives a
  full edge redeploy unattended, and carries the presence census the whole
  way (`edge_reconnect.rs`).

The per-row device-suffix rendering itself still moves with gh#124; this issue
fixed what the suffix is allowed to claim.

### H27 — Readable at 124 rows — **done** (gh#125)
An operator report with screenshots: at 124 rows the panel was "messy and hard
to read", the host chip whispered "This device ●" while showing the Mac's own
stale test board instead of the box's, and an undispatchable `no route` row was
the top selected row of the whole panel. Five fixes, derived once in
`comet_proto::view::board` and drawn by all three viewports:

- **Groups within sections** (`grouped_sections`): rows group by route — the
  same field `f` and `Filter::Route` partition on — biggest group first, ties
  alphabetical, each group collapsible. One routed group draws no header
  (`group_headers_shown`); a flat three-row WORKING section stays flat.
- **`no route` is a trailing, folded group** (`group_starts_collapsed`):
  visibility-only rows get a headline and a count, never pole position. The
  default fold applies only on the unfiltered board — `f` to the `no route`
  position or `/` matching an unrouted title must show what it asked for, so
  under any active filter the group opens.
- **Honest hosts.** The host moved from a corner chip into the panel's title —
  "Board on Tokenmaxxer9000 · 124" — on desktop (clicking it opens the old
  pin menu); the TUI header gained the count beside its host note; iOS already
  titled itself this way. And the automatic sweep stopped settling on the first
  frame: a frame proves a board *exists*, not that it is the org's board.
  `board_dispatched` (any row with an attempt on record — not `chat_id`, which
  only rides the live attempt) is the evidence the sweep settles on; a board
  without it is held as a *fallback* while the remaining candidates are asked,
  and settled on only when nobody with evidence answers. A lone device and a
  fresh install still see their own board; a laptop's stale test board loses to
  the box. All three sweeps (desktop watch loop, TUI link loop, iOS
  `BoardStore`) carry the same hold-and-return shape.
- **The leading token is repo-qualified** (`TaskRow::display_identifier`):
  `tally #507`, the CLI's form humanized, because `gh#507` vs `gh#44` are
  different repos distinguishable only by a muted sub-line. `gh_repo` /
  `gh_repo_name` moved from `comet_board::model` into proto (re-exported) so
  the viewports and the board crate parse one id format once. Linear ids show
  unchanged. `/` matches the rendered form. The TUI's id column stretches to
  the widest token on screen (capped), and the Ready sub-line dropped the
  route it used to name — the group header and leading token now say it — 
  keeping only a workspace that differs from the route's name plus the
  `[enter to dispatch]` / `no route` affordances.
- **Two-line titles under the cursor** *(reverted by §H30 — gh#132: a row that
  grows under the pointer reflows the list below it, which is what the operator
  then reported as jank)*: the desktop's selected/hovered row
  wraps its title to two lines (`line_clamp(2)`, row height min not fixed);
  iOS already wrapped. The TUI stays one terminal row per line — a grid where
  one row is sometimes two makes the scroll arithmetic lie — but its title
  column got wider with the metadata cuts above. Desktop section headers also
  took on the weight of what they manage: bold, always-visible counts, a
  chevron instead of a 10 px text button.

### H28 — One Active group — **done** (gh#123)
§H20 gave the sidebar an **Agents** group and §H24 added **Running** under it —
a split by how a run started: the board released it, or somebody (or some
orchestrator) just started it. That is a mechanism distinction, and the
reader's question does not contain it: "what is working, and which of it wants
me" has one answer. Now it gets one group — **Active**, needs-you first, then
working, blind to origin in the order — on all three frontends.

- **`comet_proto::view::board::active_rows` is the merge, and it is small.**
  Membership already partitioned (`running_rows` subtracts every chat a live
  attempt claims — §H24), so the union never draws a chat twice and the merge
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
  counter debt: all exactly as §H20 and §H24 state them, per half.

### H29 — The skill ships with the product — **done** (gh#133)
herdr-board had a `board` skill any Claude session discovered on its own.
comet's equivalent was written by hand and lived in three copied places — the
operator's Mac, the box user, and each agent-account slot — which is three
things to remember and one silent failure: a copy documenting flags the binary
no longer has. Now the text is an asset compiled into the binary, and every
place an agent reads from is written by something that already runs.

- **One source, versioned with the CLI it documents.**
  `assets/skills/comet-board/SKILL.md` is `include_str!`'d by
  `comet_board::skill`, and `rendered()` stamps `CARGO_PKG_VERSION` into a
  trailing marker on the way out — so the repo file carries no version to bump
  and every installed copy says which binary wrote it. `status_of` compares
  bytes, not versions: a copy edited in place is stale too, because an
  installed skill is a build artifact.
- **The verb table cannot drift, because it is not written by hand.**
  `apps/board-cli/src/skill_doc.rs` renders the "Every verb" block from this
  binary's own `clap::Command` — verbs, positionals, flags, and each `about`
  line, hidden commands excluded — and a test fails the build when the
  committed file and the parser disagree (`UPDATE_SKILL=1 cargo test -p
  comet-board-bin` rewrites it). The prose above the block is authored; only
  the reference is generated, which is the half that rots.
- **Three install paths, no copying.** `comet-board skill install` writes
  `<config dir>/skills/comet-board/SKILL.md` — the wizard's routes stage runs
  it on a fresh box, and a teammate runs it once on a laptop that dispatches
  with `--device`. The third is the one that had to be automatic:
  `AgentAccounts::materialize` writes it into the slot dir beside the
  credentials, on every dispatch, because a slot *is* the run's
  `CLAUDE_CONFIG_DIR` and the user-level copy is invisible from inside one. The
  write is byte-compared first (a re-materialize that changes nothing writes
  nothing) and never fatal — a dispatch that can run beats a file that could
  not be written.
- **Doctor knows the difference between "stale" and "self-healing".** The
  `agent skill` check fails on the user-level copy, which nothing but `skill
  install` writes, and only reports on the slots, which the next dispatch
  re-stamps — otherwise every version bump would turn doctor red over files
  that fix themselves.
- **Claude Code only.** Skills are its discovery mechanism; `CODEX_HOME` has no
  equivalent, so a Codex slot is left alone and still learns the board from
  `docs/agent-conventions.md`. [`docs/skill.md`](skill.md) is the operator-facing
  version of all of the above.

### H30 — A row is a door, not a tooltip — **done** (gh#132)
An operator report with a screenshot: "the animation feels a bit laggy or
jagged … that it shows more text but isn't openable either as a modal or
something doesn't sit right." Two faults with one root — §H27 answered "which
Signicat issue?" by making the row *bigger*, which is both the jank and a
promise the row could not keep.

- **Hover never changes layout again.** §H27's `line_clamp(2)` + `min_h` meant
  the row under the pointer grew and every row below it moved; the chips
  appearing on hover added a few pixels more. The desktop row is now a
  constant `ROW_H`, its two lines are constants too (`ROW_LINE_H` is the chip's
  height, so a row with chips is exactly as tall as one without), and the title
  is `truncate()` in every state. Of the issue's three options this is (c) —
  and (c) is not a consolation prize once (2) exists: with the full title one
  keypress away, a selected-row expansion would be the same sentence said
  twice, and arrowing down the list would reflow it on every step.
- **The row opens.** Desktop: a peek panel between the list and the footer —
  `space` toggles it, a click on a row opens it, escape shuts it before it
  shuts the board, and it follows the cursor once open. TUI: the help screen's
  full-screen shape, because a 24-row terminal has no beside; it owns the
  keyboard while up (`j`/`k` scroll the body) with **one deliberate exception**
  — `enter` still dispatches from inside it. iOS: a sheet, which is what a tap
  on a row now does. All three carry the whole title, the issue body as
  markdown, the labels, where the work sits, what has been tried on it, and the
  links.
- **Reading is never on the way to releasing.** `enter` still dispatches from
  the list on every surface, the phone row keeps its own Dispatch/Retry chip,
  and a release started from a detail surface goes through the same account
  picker — the detail must not become the one place on the board that skips the
  question of whose subscription a run spends (§H17).
- **The body is a call, not a field.** `ReadBoardTask {taskId}` → `{id, body}`,
  forwardable to the board's host like every other board verb, served off the
  loop thread that owns `board.db`. `WatchBoard` republishes every row on every
  sync cycle; a hundred issue bodies riding along would make each frame two
  orders of magnitude larger, relayed to a phone, to draw one truncated line.
- **The actions are one rule.** `row_actions` (a row's own affordances) and
  `detail_actions` (those plus the links a list has no room for) live in
  `comet_proto::view::board`, ported to `BoardModels.swift`. The desktop's
  per-state chip logic — hard-coded since §H12 — now reads from it, so the
  three surfaces cannot drift into offering a Retry one of them does not.
  `history_line` and `placement_line` join `row_metadata` as the shared
  formatting. `history_line` names `billed_to` *unconditionally*, unlike the
  row's own sub-line (`billing_note`, which speaks up only when somebody else
  is paying): the detail is where you go to ask, and an answer that appears
  only when there is a problem cannot be trusted to mean anything.

### H31 — The shelf is not a landfill — **done** (gh#139)
"Do all complete sessions just accumulate under the folder?" They did. gh#72
reclaimed the *checkout* an attempt leaves behind and nothing reclaimed the
other half: a board-dispatched chat was archived only by a hand, so at agent
throughput a space's shelf silted up in days and the six chats somebody was
actually working in were somewhere in it.

`[defaults] archive_chats` (per route, `off` honored), swept by
`SyncEngine::archive_chats` beside `collect_worktrees` on the same interval.
Shipped at the checkout's `7d`; §H33 made it `on-settle` after a morning spent
reading last night's finished rows.

- **The same rule, not a second one.** `gc::chat_standing` is `gc::standing`
  with two additions, and `gc::decide` ages both windows: a chat and a checkout
  are one attempt's leavings, and a box that reclaimed the work while keeping
  every conversation about it forever would have tidied half the mess. The
  clock starts when the task leaves the board — merged, closed upstream, marked
  done — and is stamped on `attempts.chat_archivable_at`.
- **What it will not touch.** A live *or blocked* attempt (both are open
  attempts, and the agent that stopped to ask at 02:00 is the worst chat to
  file away). A task in review — review delivery asks `chat_alive` about
  exactly this chat, so archiving one would break its own delivery loop,
  silently, for the tasks a human is still working on. The pinned
  orchestrator, which hears about every settle and is therefore never
  finished. And a chat with no board attempt: the sweep walks attempts, so a
  hand-made chat is never a candidate — those are the human's.
- **Archiving is not deleting.** The mutation is the same `set_chat_archived`
  the sidebar's own Archive writes, through a new `Runtime` verb, so every
  surface updates off the workspace-doc watch with nothing told. The
  transcript is intact, Settings → Archived unarchives, and the board
  un-archives a chat itself when a wrongly-settled attempt goes back to work
  (`rewatch_settled_attempts` — but only one it archived; a chat an operator
  filed away is theirs).
- **Per route**, unlike `retain_worktrees`: a shelf belongs to a space, routes
  are how work is pointed at spaces, and the route running a hundred throwaway
  fixes a week into a scratch space is not the route whose finished chats
  somebody re-reads.
- **`doctor` says what it costs.** A `chats` check reports how many board chats
  are still on their shelves, how many are on the clock, the window, and how
  many routes answer differently. Never red: keeping everything is a choice,
  and `off` is worded as one.

### H32 — A chat lives in exactly one list — **done** (gh#138)
§H28's **Active** and gh#124's spaces tree answer different questions — "what
is alive" and "what lives here" — and both answered with a full session row.
Three agents working in one space therefore rendered twice inside one screen
height: full rows in Active, the same rows again under the expanded space. The
duplication was deliberate (status vs navigation) and overshoots exactly when
activity concentrates in one space, which is the common case.

- **Active owns a chat while its session is live; the space's shelf shows it
  when idle.** `comet_proto::view::spaces::space_shelf` is the split, over
  `active_placements` — the `(chat id, space id)` join Active's rows need,
  since an `AgentRow` names an issue and not a folder. Membership is
  §H28's list verbatim: nothing new decides who is alive, and the tree simply
  stops re-listing what Active already said.
- **Two seams keep the surfaces tied.** The space row keeps its aggregate dot
  (how urgent) and gains `running_label` — `· 3 running` (how many, and where
  they went); a space whose sessions are all up in Active discloses
  `shelf_note` — "3 running above · no idle sessions" — instead of a gap that
  would read as a bug. The count comes from the placements, not the tab order,
  so an archived-but-working chat is still admitted.
- **A repo slug is not a folder.** The same screenshot showed
  `bredebjorhovd/attn` twice in the local group: `~/dev/attn` and the board
  worktree at `~/.comet-native/worktrees/attn/board-gh-10-attn`, two real
  spaces on one machine that gh#118's repo-first naming calls the same thing.
  Not a `device_groups` sort bug — the grouping is total and both rows were
  correctly in the header-less local group. `space_titles` now makes names
  unique *within* a group by appending the shortest path tail that separates
  them (`· attn` / `· board-gh-10-attn`); across groups the device header
  already tells them apart, so gh#124's "named once" stands.
- **All three surfaces, one derivation.** Desktop derives Active once per
  sidebar frame and hands it to both sections; the TUI subtracts the same
  placements when it builds its nested `Row::Chat`s and carries `running` on
  `Row::Space`; the phone applies it to the home screen's Sessions list, where
  Active sits directly above (`SpaceRows.swift`, the Swift port). `SpaceView`
  is a different screen with no Active on it and stays complete.

### H33 — What the morning after §H31/§H32 showed — **done** (gh#144)
The operator looked at the same sidebar the next morning (2026-08-08) and the
two fixes read as no fix at all. Neither was wrong; both stopped one step short
of the screen.

- **A disambiguator at the end of a name the sidebar elides from the right is
  the first thing cut.** §H32's `space_titles` correctly named the two attn
  checkouts `· attn` and `· board-gh-10-attn`, and both rows still drew
  `bredebjorhovd/attn…`: the pane is narrower than the slug alone. The tail is
  now a field, not a suffix — `SpaceTitle { base, qualifier }`, with
  `line()` for the surfaces that have room (the TUI, the drag ghost). The
  desktop row gives the qualifier its own `flex_none` width (capped at
  `SPACE_QUALIFIER_MAX`, so the chevron stays put) and lets the *base* shrink
  first. The half that differs is the half that survives.
- **A week is the checkout's clock, not the chat's.** §H31 gave both the same
  window on the theory that they are one attempt's leavings. They are not read
  the same way: a checkout is evidence you might go back for, a chat is a row
  you are *shown*, and thirteen finished rows from one night's work buried the
  live ones — "having the issues alive and not collected is kinda worthless
  really". `[defaults] archive_chats` is **`on-settle`**: no window. The guards
  are what protect an unfinished chat, and they are all in `chat_standing`
  already — live attempt, blocked attempt, open pull request, issue still open,
  the pinned orchestrator, a chat nobody dispatched. When none of them hold, the
  task has merged or closed and the row has nothing left to say. A duration
  still works for a space that wants a grace period; a bare `0` is now an error,
  because it reads as "no window" here and "keep forever" for a checkout, and
  guessing between opposites is worse than asking.
- **A row you have to look up is not a row.** A dispatched chat was named for
  its identifier alone, so a shelf read `gh#10 gh#25 gh#26 gh#11 gh#13`.
  `DispatchSpec` now carries the task's `title` and `chat_title()` composes
  `gh#25 · D1 Prototype v1: the Today window (static)` — identifier first,
  because it is short, it is what the board rows and the branch sub-line say,
  and it is therefore the half that has to survive a narrow pane. The title is
  clipped at 60 chars on a word boundary; an empty one leaves the bare
  identifier rather than a dangling separator.
- **The kill switch was behind a row that does not exist.** §H27 put unpinning
  on the pinned chat's own row — "whoever wants the notices to stop reaches for
  the session they pinned" — and gh#122's slot is not that row. Exit the
  orchestrator's session and its chat leaves Active; if its space shelf never
  listed it, the slot above Spaces is the only row it has, and that row had
  `on_click` and nothing else. The operator could reopen the thread and not
  unpin it from either app. The slot now carries the same context menu a chat
  row does, on both surfaces (`render_orchestrator_slot`, and `Row::Orchestrator`
  in the TUI's `open_context_menu`, which fell through to `_ => return`). The
  CLI escape hatch was always there and nobody should need it:
  `comet-board routes defaults orchestrator_chat --unset`.
- **Screenshots in a PR body die twice.** Not a board bug, but the board's
  agents keep writing it: an attempt asked for screenshots in its PR
  description and reached for
  `raw.githubusercontent.com/<owner>/<repo>/<branch>/…`, which is unreadable
  without a token on a private repo *and* names a branch that merge deletes.
  Both failures are silent. `docs/agent-conventions.md` (and the shipped skill,
  rule 9) now says: commit the images and reference them with a relative path
  from a markdown file in the repo.

### H34 — The board reports on itself — **done** (gh#143)
`comet-board stats` has answered "is delegating actually working" since the
port, on one text screen nobody opens: a shell verb is where you go when you
already suspect something. The numbers belong where the board is looked at.

- **Settings → Board stats.** A section beside Board routing, and for the same
  reason it is beside it: `board.db` lives on whichever device hosts the board,
  so the page sweeps `host_candidates` for the one that answers and a laptop
  reads the box's throughput without an ssh account on it. Headline tiles
  (dispatches, tasks, completion, median, live), then dispatches per day with
  the share that ended `done` filled in, where the work *landed*, how long runs
  take, friction, hour-of-day, and the tallies — space, runtime, tracker, whose
  subscription.
- **`BoardStats`, a call and not a stream.** Like §H30's `ReadBoardTask`: these
  are read when a page opens and stale by a poll interval at worst, and
  streaming a full aggregate on every board tick would cost every connected
  viewport a recompute nobody is looking at. Served on the board loop's own
  thread, which owns `board.db`.
- **One gatherer, one shape.** `comet_board::stats::gather` still produces it
  and the CLI still prints it; the *type* moved to
  `comet_proto::view::stats::BoardStats` so a viewport can deserialize the reply
  without linking a SQLite store — the `RuntimeOption` split. The renderer's
  arithmetic (ranking a tally, scaling a bar, phrasing a duration, folding a
  long tail into `n others`) lives there too, so the CLI, this page and whatever
  comes next cannot disagree about it.
- **What the record already knew.** Nothing new is stored. Landing comes from
  the task's `pr_merged`/`pr_open`/`pr_number` and is counted per *task* — three
  goes at one issue produce one pull request, and counting attempts would report
  the same merge three times. Friction is `reopened` + `blocked_count` +
  `overrun_warned_at`, which the board was already writing and nothing was
  reading. Whose subscription is gh#101's `billed_to`, with the dispatches that
  named no slot said out loud as the box's own login rather than hidden as
  unattributed.
- **Honest empties.** A completion rate is `None` until something has ended and
  renders as `—`: a `0%` on a board whose first agent is still running is a lie
  about the board. Day buckets are emitted for quiet days too — a gap that is
  simply absent reads as data the board failed to record.

Not on the TUI or the phone yet. The derivation is shared, so both are a
rendering away.

### H35 — Tokens: the number the engine was throwing away — **done** (gh#151)
§H34's page could say how long the work took and never what it spent, because
nothing persisted a token. `AgentEvent::Usage` was emitted by both harnesses
and dropped on the floor in `doc/parts.rs`, on purpose — token display is
excluded from *docs* by design, a poor fit for CRDTs. That exclusion was read
as "the board does not count tokens", which does not follow: the board has its
own SQLite store and its own history.

Two things had to be settled before anything could be added up, and both are
pinned by tests:

- **The cache fields were never parsed.** Claude's result frame carries
  `cache_creation_input_tokens` and `cache_read_input_tokens` beside
  `input_tokens`; the wire struct read the first two fields only. On any
  session past its first turn the cached half is most of what was read, so the
  old two numbers under-reported a run by an order of magnitude.
- **Codex reports a snapshot, and counts cache *inside* input.**
  `thread/tokenUsage/updated` re-fires through a turn with that turn's running
  total; the session loop already held it in `pending_usage` and flushed once
  at `turn/completed`, so what reaches the journal is one figure per turn —
  the event is per-turn for both harnesses, and summing over a journal is a
  sum over turns. Its `inputTokens` *includes* `cachedInputTokens`, the
  reverse of the Anthropic shape, so the normalizer subtracts. `TokenUsage`'s
  four buckets are disjoint by construction, which is what makes `total()` a
  plain sum rather than an argument.

- **Journal → attempt row, copied while the evidence exists.** The engine's run
  journal is the source (the settle authority already reads it, so a crash
  mid-attempt loses nothing) and the attempt row is the record, because the two
  have different lifetimes: §H33 archives a chat once nobody is coming back to
  it. `Runtime::run_tokens` sums a chat's journal — filtering lines by tag
  before parsing, since a long run's journal is mostly text deltas — and the
  reconcile copies it onto the row **before** any branch that can close the
  attempt, so an orphaned or capped run keeps what it had spent. Cancel and
  retry-replace read it themselves; they never pass through reconcile.
- **The model, because nothing else states it.** `DispatchSpec::model` is
  `None` on most attempts — the route named no override and the harness default
  ran — so a per-model breakdown keyed on it would be almost entirely
  "unknown". What the harness announces in its `SessionStarted` is the model
  that actually ran, and it is recorded beside the tokens.
- **Blank, never zero.** Five nullable columns with no default, written as a
  set. NULL means "this attempt reported nothing" — every row from before this
  existed, and any harness that meters nothing — and the page counts those out
  of its coverage rather than adding a zero to a total. The same rule §H34's
  `completion_rate` follows, and for the same reason: a zero reads as free
  work. Backfill is not possible and would not be honest if it were.
- **Coverage said out loud.** Every token card carries "62% of attempts
  reported usage (8 of 13)" as its aside. A total read without it is a total
  read wrong. `token_coverage` is `None` when nothing ran and `Some(0.0)` when
  attempts ran and none reported — those are different facts.
- **Counts only; pricing is a separate ticket.** What a token costs depends on
  a seat, a plan and a price table the board does not have, and a number that
  looks like money is read as money. When that ticket comes: work absorbed by a
  Claude Max seat did not cost a per-token figure, so whatever is shown must be
  labelled a list-price estimate of the same usage on the API, not a bill.

### H36 — The release ships the board CLI too — **done** (gh#156)
Found while upgrading the box to v0.3.4: `~/.comet-native/app/0.3.4/` held
`comet`, two icons and a desktop entry, and `~/.local/bin/comet-board` was a
symlink into a source checkout, made by hand on 6 August and untouched since.
The release payload had never carried the board CLI, so `install.sh` upgraded
the engine on every release and stepped over the binary that drives the board.
By then the box was running 17 routes over 8 repos with a CLI three weeks
behind them: `onboard` (§H17) and `skill` (§H29) did not exist on the machine
whose agents were supposed to use them.

- **Both binaries, one payload.** They come off the same `cargo build`;
  shipping one was the entire bug. `scripts/package-linux.sh` stages
  `comet-board` beside `comet`, `scripts/package-macos.sh` puts it in
  `Comet.app/Contents/MacOS` — signed *before* the bundle, since nested code
  is not covered by signing the wrapper and notarization rejects the
  submission over one unsigned helper. `release.yml` needed no change: it
  uploads whatever the packaging scripts produce.
- **Both packaging scripts now prove it.** Each fails the build if its output
  is missing either binary — the tarball listing is grepped, the bundle's
  `Contents/MacOS` is stat'd. An omission that ships is exactly what happened
  the first time, and it cost nothing to make it impossible to repeat quietly.
- **One lookup was already written for this layout.** `resolve_board_exe`
  (§H11's askpass helper) tries `COMET_BOARD_EXECUTABLE`, then *beside the
  running binary* — "how it is installed next to the engine" — then PATH. The
  middle step could never hit, because nothing ever put the two side by side; a
  dispatched agent's `GIT_ASKPASS` resolved through PATH to whatever stale
  binary was there. The comment described the intended layout and the release
  did not ship it. Now it does, and the fallback is a fallback again.
- **Links point at `current`, not at a version.** `~/.local/bin/comet-board →
  ~/.comet-native/app/current/comet-board`, so a later `comet update` flips one
  symlink and both binaries follow it. That is also precisely why an unmanaged
  binary in the way matters: it is the one thing the flip cannot move.
- **A hand-placed binary is not silently replaced.** Both installers take over
  `~/.local/bin/<name>` only when it is missing or already theirs — a symlink
  into the app root for the curl|sh installer, a regular file for the copying
  tarball one. Anything else is a decision a human made, sitting ahead of the
  installer on PATH; overwriting it would destroy a build tree nobody chose to
  throw away. So they name what is there, name the `rm -f` that hands it over,
  and leave it standing. Refusing loudly is a fix; the failure was never the
  stale binary, it was that nothing said so.
- **`doctor` compares the two versions.** The engine reports its own
  `CARGO_PKG_VERSION` in the `LocalDevice` reply, and the `cli version` check
  fails when the CLI's disagrees, naming the path of the binary that answered —
  which copy is talking is most of what you need. An unreachable engine falls
  back to the installed payload's directory name, because a box whose engine is
  down is exactly when someone runs doctor; neither available is "not checked"
  rather than a failure. Against a payload that predates this fix it says so
  instead of offering an installer that would relink nothing. Every other check
  in that report asks about the environment the CLI can see. This one asks
  about the CLI, which is how the whole class of bug stayed invisible.
- **But doctor cannot be the only teller, because it ships inside the stale
  thing.** A CLI old enough to have drifted is old enough not to carry the
  check — so on the one box with the problem, `comet-board doctor` goes on
  reporting a clean board. The check has to also live where the *current* code
  runs, and on that box the current code is the engine. `board_cli::probe`
  inverts it: find the binary (`resolve_board_exe`), run `--version`, compare
  against the engine's own. `comet status` prints a `Board CLI:` line from it,
  and `comet headless` logs one WARN at boot when they disagree — the only
  report in this whole section that fires without somebody first going to look,
  and it fires on the restart the install itself performs. Off the boot path on
  its own thread: the probe executes a binary nobody vouches for, and one that
  hangs instead of answering must not be able to hold the engine down.
- **`comet-board --version`, for everything outside the binary.** doctor never
  needed it — a process knows its own `CARGO_PKG_VERSION` — which is why it
  did not exist, and why `install.sh` could see a binary in its way and not say
  which one. Now the warning names it: `~/.local/bin/comet-board (v0.2.9) ->
  ~/comet-board/target/release/comet-board`. A copy too old to know the flag
  dates itself by failing, since the flag lands with the first release that
  ships this binary at all — reported as "too old to answer `--version`", which
  is a fact and not a guess.

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
