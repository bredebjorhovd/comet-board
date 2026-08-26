# The wide pass — **walked** (gh#386)

The narrow list (gh#382) asks whether last night's fixes landed. This one asks
whether the whole thing still works. Walked on 2026-08-26 from the dispatched
chat it released — which is itself one of the checks: an attempt that cuts a
worktree, names a branch after its task and settles through the claims contract
is §3 running, not a description of it.

Verdicts: **live** (exercised against the box or the real board today),
**suite** (held up by the workspace test run today — cargo `--workspace
--no-fail-fast`, plus the edge vitest suites), **human** (needs eyes or hands
this session does not have). Everything claimed here names where to look.

### What the walk caught

Four things, three of them fixed in this branch:

1. **The suite was red on its own box.** Three test binaries failed the moment
   the suite ran *from a dispatched chat* — which is where suites get run
   (§gh#561): `askpass_git`, engine `clone_credential`, harness `codex`'s
   push-attestation test. Not because the credential path broke, but because
   PATH's first `git` inside a chat is the board's own guard shim (gh#488),
   whose job is to re-stamp the chat's credential at its exec boundary no
   matter what the caller's environment said. The tests spawned plain `git`
   (or resolved via `resolve_git(None)`), got the wrapper, and measured the
   shell they were raised in: the live helper answered, its host guard refused
   the listener's `127.0.0.1` — correctly — and red tests said nothing about
   the code under test. Fix: [`resolve_git`]/
   [`resolve_gh`] now skip everything under `{state_dir}/bin` — those
   directories hold only wrappers this code wrote, so treating one as "the
   real tool" was a guard wrapping a guard (`crates/board/src/git_credentials.rs`,
   `scan`) — and the engine's two network-capable spawn sites clone and fetch
   through that resolution instead of PATH's first hit
   (`crates/engine/src/repos.rs::the_git`, `crates/engine/src/diff_sync.rs::capture_git`).
   A dev engine started from inside a chat had the same latent borrow of the
   chat's identity; the fix is for it too.

2. **Reconnect jitter silently collapsed on macOS.** `spread()` drew entropy
   from the wall clock's sub-second nanosecond field alone, modulo the span.
   On a clock that ticks in whole microseconds — this Mac, as the test
   discovered mid-walk — that field is always a multiple of 1000, so any span
   dividing 1000 returned one fixed delay forever: zero jitter at the 250 ms
   backoff base, permanently, on the box least able to survive a synchronised
   herd. The failing test (`jitter::tests::consecutive_draws_differ`, saw
   "1 distinct delay") was the canary; the fix mixes a process-local atomic
   draw counter with the clock field under odd 64-bit multipliers
   (`crates/sync/src/jitter.rs`). Still not a random number generator, still
   dependency-free — and now actually herd-breaking everywhere it runs.

3. **The edge is failing again, live.** `comet-board doctor` today:
   14 of 15 connections live, the workspace room down, two rooms CHURNING —
   48 session joins died inside 30 s in the last hour — and one chat room
   STALLED with 13 local entries unacknowledged past the alert threshold on a
   live socket. That is the gh#373 / gh#527 class recurring after both were
   closed: rooms answer a join and die mid-session, so replies do not arrive.
   Not fixed here — it needs the edge's own account (`/stats` per room,
   including whether a room aborted itself) and is someone's next ticket.
   What the walk *can* say: the client side held up its end. Bounded backoff
   capped at 30 s (`crates/sync/src/room.rs`), the alarm budget from gh#378
   passing its suite, and the churn did not take the board down.

4. **A test had no margin** (`edge/test/workerd/session-backfill.workerd.test.ts`):
   ~4 s of CRDT merge work against real workerd under vitest's default 5 s
   timeout, red when anything else runs on the box, green in isolation. Gave
   it an explicit budget with the reason in a comment beside it.

Plus one documentation gap closed while walking §8: the 2026-08-12 lesson —
quitting Comet before replacing `/Applications/Comet.app`, because deleting
the bundle under the running headless engine kills it — lived only in the
issue text. It is now in `docs/macos-install.md` under *Updating by hand*,
which is the page an operator has open while doing the thing it warns about.

### 1. The app, cold

- Launches from `/Applications` with no engine running, comes up signed in —
  **human**. Adjacent facts from today: the engine *is* running from the app
  bundle, CLI and engine agree on v0.14.2, auth tokens present for all three
  accounts (doctor lines `engine`, `cli version`, `agent account ×3`).
- Sidebar spaces list with branch + running count — **suite**
  (`crates/ui/src/shell/spaces.rs`: branch claim width, `space_branch` →
  `detached`, `n_label`; count only for spaces with a checkout). Pixels:
  **human**.
- Titlebar buttons; back/forward `--faint` with nowhere to go — **suite** +
  design claims (`docs/design/window.md` B2); pixels: **human**.
- `cmd-shift-b` opens the board — **suite** for the binding and action
  (`crates/ui/src/shell.rs` wires keymap default `mod-shift-b` →
  `ToggleBoard` → `toggle_board_from_route`). The synthetic-click caveat in
  the checklist stands; nobody re-tests that but a person.
- Light and dark render; nothing hard-codes a grey; the two deliberate
  deviations — **suite** for the values (`crates/ui/src/theme.rs` token
  tests; `docs/design/tokens.md` records exactly two deviations and says
  everything else is a bug). Composition is the known-manual residue.
- Composer attach / send / interrupt / resume — **suite** (composer state
  machine, attachment pipeline, durable QueueCommand run/steer/interrupt);
  feel: **human**.
- Transcript folds tool calls; long output scrolls inside its box —
  **suite** (tool-group folding heights are analytic;
  gh#611's fold-never regression landed before this walk); pixels: **human**.

### 2. Chats and agents

- Start a chat, small task, runs to completion — **live**: doctor counts 9
  attempts ended in the last 24 h, none within 5 minutes of starting (the
  smoke-dispatch signal is healthy).
- Interrupt stops and the transcript says so — **suite**
  (`crates/engine/src/sessions.rs`: interrupt queues through the doc command
  queue; terminal pairs finalize like every other ending).
- Steer taken at the next step boundary — **suite** (same queue; guardrail
  steering rides the same boundary).
- Guardrails stop a spinning turn and say which — **suite** and **live** in
  config: defaults 10 consecutive failures / 2000 tool calls
  (`crates/board/src/config.rs` `default_max_tool_failures/_calls`), enforced
  in the run loop with a visible transcript entry naming the reason
  (`crates/board/src/spin.rs` `stop_text`; asserted verbatim in its tests),
  printed per route by doctor ("turn guardrails 10 failures in a row, 2000
  tool calls per turn").
- Skill-spun work produces board rows, not silent side work — **live**: this
  attempt is the sample; the skill ships with the binary
  (`assets/skills/comet-board/SKILL.md`) even though the box's installed copy
  is behind (§9 below).

### 3. The board loop, end to end

- `comet-board list` on the box and in the app agree — **live** for the CLI
  (rows carry state, branch, PR, landing); app side reads the same row view
  (`comet_proto::view::board`) over the engine RPC. Side-by-side eyeballing:
  **human**.
- Dispatch → `working`, worktree cut, branch named after the task —
  **live**, by construction: the attempt walking this pass sits in
  `…/worktrees/comet-board/board-gh-386-wide-pass-whole` on
  `board/gh-386-wide-pass-whole`, row state `working`.
- Settle → `review`, PR open, dispatching chat prompted once — **suite**
  (`Signal::settle_print` keeps the last announced settle on the attempt under
  `settled:<id>`; repeat announcements are suppressed, blocks keep their own
  counter — `crates/board/src/notify.rs`). Live row evidence exists on the
  board (gh#348: PR open, `landing: not-clean`, attempts history intact).
- Review screen: brief, effects chips, claims, evidence, remainder,
  read-the-diff — **suite** (`crates/ui/src/review.rs` carries all six, with
  the effects-before-claims order asserted); pixels: **human**.
- Verdict posts and the receipt says what happened — **suite**
  (`VerdictReceipt` round-trip into the review window; "o/r#87 merged" style
  receipts; approval-as-comment fallback named when GitHub would refuse).
- Merge settles the row; issue closes from `Closes #N` — **suite**
  (`closing.rs` reads `closingIssuesReferences`, so a merge that closes
  nothing is visible as such; stack-aware finish in `crates/board/src/sync.rs`
  `merge_pull_request`).
- Retry: same branch, previous attempt's commits — **suite**
  (`a_retry_lands_on_the_branch_the_task_already_holds`; worktree retention
  refuses to collect a branch a retry will reuse).
- Cancel ends the attempt, issue stays open, row returns to `ready` —
  **suite** (cancel paths in `crates/board/src/sync.rs`, including the
  cancelled-run-commits isolation).
- Caps refuse cleanly — **suite** (`check_capacity` bails with "space `x` is
  at N of M working — cancel one first"; autopick meters the same cap).
- Billing warning when no account is named — **live**: doctor reports
  `billing guard: warn`, names the two subscriptions the unaccounted dispatch
  would spend, and the picker/CLI/row/comment all carry it
  (`crates/board/src/billing.rs`, `check_billing`).
- Archive: spent chat shelves; a chat waiting on released work does not —
  **suite** (`crates/board/src/gc.rs` reads upward from chat to what it is
  still waiting on; the orchestrator exemption is pinned) and **live**
  (6 chats shelved, 0 on the archive clock).

### 4. Stacks

All five mechanical lines — `--onto` cutting from the sibling's branch, the
row grammar (`clean against … · waiting on PR #11 · in PR #12`), parent-merge
retargeting arriving as fact rather than news, `changes requested` pulling the
layers above out of review, and a merge endpoint that merges a stack instead
of refusing it — **suite**: `crates/board/src/stacks.rs` pins the row strings
verbatim, `rebased.rs` owns surviving the parent's merge (gh#286), the
retarget-not-news rule lives in `crates/board/src/review.rs` (gh#288), verdict
adoption propagates downward-only (gh#289), and the finish gate demands the
whole stack merged (gh#290).

What the suite cannot be: **a real GitHub stack on `board-scratch` with a
lower-layer change and an upper one.** That remains gh#337's dispatch, by the
issue's own note. Not attempted from here — creating scratch repos and
stacked PRs on the live account is exactly the manual-with-side-effects class
the wide pass flags.

### 5. Permissions and credentials

- The review names the sandbox the run actually got, including "full access to
  the box" — **suite** (`SandboxReport` recorded per attempt
  (`run_sandbox`, `crates/board/src/db.rs`), surfaced through
  `sandbox_note` in `crates/board/src/claims.rs`).
- Codex commits and pushes under `workspace-write`, no silent escalation —
  **suite**: sandbox levels map straight onto Codex's own policy names
  (`crates/harness/src/codex/catalog.rs`), dispatched runs get
  `WorkspaceWrite` by default, and the attestation test walks the real app
  server with an adversarial lower policy and asserts the push is bound back
  to the dispatch contract — a test that was itself red on this box until
  finding 1 above was fixed.
- Pushes use the board's askpass credential; doctor says so; another-way pushes
  reported — **live** (doctor: helper answers and mints per push, `gh` wrapped
  per call) and **suite** (`tests/askpass_git.rs` drives real git against a
  401 listener; the host guard refusing non-github prompts is asserted, which
  is precisely what fired during finding 1).
- `review identity` names opener and reviewers, FAILs on collision — **live**
  line correct on this box (the App opens; @bredebjorhovd reviews as the board
  until a member token exists), **suite** for both collision shapes
  (`crates/board/src/doctor.rs`: member token identical to the board token;
  opener == reviewer).
- Teammate authorship survives the review-token work — **live**: doctor maps
  each account to its noreply commit identity (`dispatch authorship`), and the
  mapping code (`git_identity.rs`, gh#162) is untouched by the verdict-token
  path.

### 6. Sync, devices, and the edge

- Two devices see the same board — wire-level convergence **suite**
  (`crates/sync` convergence/restart tests, including shallow-reseed
  preservation); two-real-devices: **human**.
- Phone sees board, review, stats — **human** (the iOS spec scripts exist to
  make it a half-hour: `scripts/ios-{review,stats,sync}-spec.sh`).
- Engine killed mid-session restarts intact — **suite** (journal replay,
  resume budgets, crash-revive counting in `crates/engine/src/sessions.rs`;
  convergence-across-restart tests in `crates/sync/tests`); the dramatic
  hands-on version: **human**.
- Edge away → bounded backoff, recovers unattended — **suite**
  (`BACKOFF_CAP` 30 s with the ladder-reset discipline gh#396 demanded) plus
  gh#373's six-hour soak already on record — and today's live churn (finding
  3) exercised the client side again without losing the board. Strengthened
  this walk by fixing the jitter collapse that would have undone the smear
  on macOS (finding 2).
- Daily R2 backup lands; `/stats` shows `alarm.consecutiveFailures` at 0 —
  **suite** (`edge/src/session-alarm.test.ts`: budget, give-up, revival,
  stats shape). Reading prod `/stats` today: **human**.

### 7. Settings and stats

- Every settings section opens; values persist across restart — **suite**
  for persistence (`ui-settings.json` backed stores); opening every section
  and looking at it: **human**.
- Routing edits land in `routing.toml` and take effect — **suite**
  (`crates/board/src/routes.rs` typed keys, validation refusing mistyped
  limits rather than reading them as unbounded).
- Stats: five cards, header naming the board, unpriced models keep their row
  and say so — **suite** (`crates/proto/src/view/stats.rs`,
  `rates.rs` reporting "N model rate(s) … a model with no rate is reported
  unpriced rather than free", gh#359/gh#254 regressions pinned).
- Codex 5.6 sol/terra and DeepSeek v4 flash price — **suite**:
  `codex_and_deepseek_models_are_in_the_shipped_table` holds
  (`crates/proto/src/view/rates.rs`: sol 5.0/30.0, terra 2.0/12.0,
  deepseek-v4-flash with explicit cache rates, provider prefix normalized).

### 8. Install, update, release

- `install.sh` installs and restarts the engine on Linux — **code-read yes**
  (`edge/src/install.sh`: systemd user unit, `Restart=on-failure`,
  daemon-reload + restart, binary linked onto PATH); a live Linux curl-pipe:
  **human**.
- macOS dmg installs; quarantine xattr still needed — **live** for the
  artifact (v0.14.3 built and published yesterday; ad-hoc signing unchanged
  per the workflow's no-certificate gate); the drag and the dialog: **human**.
- Stop the engine before replacing the bundle — the lesson now lives where
  the action happens (`docs/macos-install.md`, new *Updating by hand*
  section). Nothing runtime to verify.
- Skill install after update; doctor reports the mismatch if forgotten —
  **live, both halves**: doctor FAILs `agent skill` on this box right now
  ("not installed · 1 of 1 agent-account slots behind"), which is the report
  working as designed; the operator step (`comet-board skill install`)
  remains done-not-yet on this Mac.
- Tag push builds four artifacts and publishes `releases/latest.txt` —
  **live**: v0.14.3 published 2026-08-25T22:00Z;
  `edge.comet.offhand.dev/releases/latest.txt` returns `0.14.3`. One lag
  worth naming: doctor's cached release line still said v0.14.2 three hours
  later ("last checked 3h ago") — the engine polls slowly enough that an
  operator reading doctor sees yesterday's version after a release lands.
  Observation, not a defect filed; worth remembering the next time an update
  seems missing.

### 9. Doctor, as its own pass

Ran live on the box today, top to bottom. Every line either ok with its
caveat spelled out or FAIL understood:

- **FAIL `edge connections`** — real (finding 3): workspace room down, two
  churning rooms, one stalled room holding unacknowledged entries. Needs the
  edge's side of the story; ticket it.
- **FAIL `agent skill`** — expected report of the forgotten post-update step
  (§8 above). Run `comet-board skill install`.
- Route checks pass live: space exists, repo fetches, base resolves off
  `origin/HEAD`, runtime installed, duration cap and turn guardrails printed
  from `[defaults]`.
- `agent PATH`, `agent instructions`, `gh stack`, `dispatched pushes`,
  `git identity` (ok with the non-noreply caveat spelled out),
  `review identity`, `billing guard`, `default account`, worktree/build-output
  retention — all answering with numbers rather than gestures.

### Still human-only

Unchanged from the issue's own notes: anything asserting *a dispatched agent
on the box did X* stays a smoke dispatch (this attempt excepted — see §3),
and canvas parity stays a person comparing pixels; the token tests cover the
values, never the composition. To today's list add one: reading the edge's
own account of the churning rooms, which needs the dashboard or a prod
`/stats` pull and a decision about what to do next.
