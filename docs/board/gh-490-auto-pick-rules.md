# gh#490 — auto-pick rules: dispatch eligible labeled tasks automatically

Tasks a human had already approved for autonomous execution still waited for
that human to notice them and press Dispatch. This adds the opt-in rule that
presses it: a saved, enableable **automation** that periodically and reactively
finds ready, dispatchable board tasks carrying a configured label and releases
them under explicit policy. Deterministic throughout — the coding model is the
worker, and a label match needs no model call.

### The rule is policy, so it lives in `routing.toml`

A `[[automation]]` block (`config.rs::Automation`) says what a route says, one
level up: what it matches (source, required labels — all of them —, excluded
labels, optionally one route), what a dispatch runs as (runtime, model, and an
**explicit** `account` — an unattended dispatch has nobody at a confirm dialog,
so consent to spend is written down once, here), how much it may do
(`max_per_eval`, `max_concurrent`, `daily_budget`, `cooldown`), and **whose
automation it is** (`owner`). Everything that edits policy already existed for
routes and is reused whole: the validating writer (`routes.rs` gained
`Edit::Automation` / `AutomationAdd` / `AutomationRemove`, addressed by *name*
— an index is the one address a concurrent delete silently re-points), the
`ReadBoardConfig`/`WriteBoardConfig` RPCs, `.bak` discipline, `$EDITOR` over
ssh.

Validation splits on `enabled`, deliberately. A disabled rule may be
half-written — that is what the settings page creates and fills in key by key
— but *enabling* requires an owner and at least one required label, and the
writer refuses the enable with the validator's own sentence. Enabling is the
explicit human authorization for every dispatch the rule later makes; a
duplicate never starts enabled, because authorization is given to a rule, not
inherited by its copies.

### Evaluation is pure, execution is the pipeline that already exists

`autopick.rs::plan` is the planner: one rule, the board's tasks in board order
(the same section sort `board_rows` publishes — "first eligible row" is the row
an operator sees at the top of Ready), and a snapshot of facts in; one decision
per considered task out — `Dispatch`, `Skip` (excluded label, already running,
not ready, unrouted), `Defer` (capacity, cooldown, budget, per-eval), or
`Refuse` (no billing account). Pure, so matching, ordering, limits and cooldown
are table-tested without an engine.

The engine's board loop executes (`board.rs::run_auto_pick`), in two seats the
issue asked for: after every sync cycle's reconciliation (the periodic half —
what catches up after a restart or a missed event) and after a session refresh
that changed a row (the reactive half — a settle frees capacity, a failed run
returns a task to ready). Each planned dispatch goes through the **same**
`handle_dispatch` a keypress does, so one-live-attempt (the partial unique
index), route resolution, workspace capacity, the billing guard (`bill`
acknowledges the rule's own account), the credential preflight and harness
availability all keep ruling. Idempotency is inherited, not invented:
evaluation writes nothing but log rows, everything is measured from `board.db`,
and the loop thread is the store's one writer — overlapping ticks, replayed
events and restarts converge.

A refusal does not hot-loop. `note_automation_refusal` records the pipeline's
own sentence, and the planner's cooldown reads that row (and failed attempts'
`ended_at`), so the task is deferred until the rule's `cooldown` lapses —
across restarts, because nothing about it lives in memory.

### Provenance and the notice path

Every autonomous attempt records `automation` + `automation_owner` with the
insert (`NewAttempt`, gh#285's `stacked_on` discipline: known before anything
is created, so a crash cannot leave an autonomous attempt looking like
somebody's keypress). It rides `TaskRow` (`automation_line`: *dispatched by
"Approved maintenance" · owned by Brede* — one derivation, because a rule name
alone is provenance nobody answers for), the review header (`AttemptReview`),
and the upstream dispatch comment names the rule and its owner instead of a
dispatcher. Blocked/failed/settled outcomes take the gh#71 path unchanged —
an automation-dispatched attempt has no dispatcher chat, so the orchestrator
and the webhook are the addressees, which is exactly that channel's stated
job. No silent orphans.

### The history is a table, because operators list it

`automation_log` in `board.db`: one row per (rule, task, decision, reason)
**streak** — an unchanged answer bumps `last_at`/`count` instead of appending,
so thirty seconds of "at capacity" is one legible line; a changed answer
appends, because history is the point. Pruned after 14 days; the attempts a
rule released are the durable record. Never credentials or webhook material —
reasons are the board's own sentences, and the account is a slot id the config
already names.

### Surfaces

- **Settings → Automations** (`ui/settings/automations.rs`, after Routing,
  before Stats — B6 updated): rules with enabled/paused/unhealthy state, key-
  by-key editing through the writer, pause/resume/duplicate/delete, the enable
  confirmation naming what is authorized and who answers for it, and the
  recent history. Board-hosted via the same sweep Routing uses (gh#434's
  furniture rule included).
- **Board header** (gh#490's indicator): a quiet wand chip, tinted when a rule
  is unhealthy; its popover shows active/unhealthy counts, the next
  reconciliation, the latest action or refusal, per-rule pause/resume through
  the same validating writer, and *Manage automations…* deep-linking to
  Settings. The full editor deliberately does not fit in the dock.
- **Task peek and review header** carry the provenance line.

`ReadBoardAutomations` (relay-forwardable, refused by a device hosting no
board) answers the derived view: per-rule state, live and daily meters,
problems, last event, recent history, and the loop's own answer for when the
next periodic evaluation runs.

### Out of scope, still

Model-assisted readiness or priority, arbitrary scripts, workflow graphs,
auto-merge, unbounded retry. The rule/run model here is the substrate those
would build on once deterministic auto-pick has earned trust.
