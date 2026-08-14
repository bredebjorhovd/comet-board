# A restart must not eat the attempts — **done** (gh#390)

One box, one afternoon, twelve attempts lost. Six tasks were dispatched inside a
minute across three harnesses (claude-code, codex, opencode); all six entered
`working`; minutes later all six settled `orphaned — its chat is gone`, and the
dispatcher chat received six separate settle notices. The retries died the same
way. No pull request, no pushed commit, nothing salvaged. `comet-board doctor`,
run immediately afterwards, reported every check ok — and a FAIL that had been
present all morning had quietly disappeared, which is what named the culprit:
the engine had been restarted between the dispatch and the sweep.

Three bugs, in the order they bite.

### 1. The premise that is only true inside one process

`reconcile_sessions_with` read an absent session row, on an attempt that had
been seen working, as "the chat is gone". Its own doc comment said why:

> session rows persist as `Idle` after a run ends, so absence after activity
> means the chat itself is gone

That is true, and it is true **only inside one engine process**. The session
mirror is `Inner::statuses` — a `Mutex<HashMap<String, Session>>` built from
live runs and published on a watch channel. It is memory. A graceful shutdown
interrupts every live run (`SessionsEngine::shutdown`), which journals a `Done`,
which means `recover_stale` finds nothing stale at the next boot and writes no
row for those chats. So an engine that restarts with six attempts running comes
back with six chats that are perfectly intact — the transcripts, the briefs, the
branches, the checkouts, all of it — and six session rows that do not exist.

Two ticks later the board declared all six chats gone.

The fix is one question, asked before the verdict: `Runtime::chat_alive`. It was
already on the trait and already used elsewhere for exactly this distinction
(§runtime-impl said "nothing is orphaned on absence-of-evidence alone" about a
chat that had *never* worked; the same rule now covers one that had). The
absence splits in two:

- **the chat is gone** — orphan the attempt, unchanged, on the same
  two-consecutive-ticks rule;
- **the chat is there and its run is not** — an interrupted run. Prompt the chat
  to carry on. Same attempt, same chat, same branch, same checkout;
- **nobody could be asked** — decide nothing. A cycle with no runtime and a
  runtime whose call failed are both this, and neither is evidence about a chat.

Resuming is one `Runtime::prompt`, because there is nothing to re-create: the
chat holds the brief, the transcript and whatever the run had already done, and
the worktree still stands on the branch. That is the whole reason resuming beats
re-dispatching — a retry would have opened a second chat and thrown the first
one's context away, which is precisely what the orphan sweep forced twelve
times.

It is bounded at `runs::MAX_RESUMES = 3`. An engine restart costs one; a second
restart inside one attempt is an ordinary evening on a box somebody is updating;
a third failure inside one attempt is not bad luck. Past the cap the attempt
closes **`failed`, never `orphaned`** — nothing vanished, the chat is on its
shelf with the whole conversation in it, and a red row is what makes somebody
look at the box. `cancelled` would have been worse still: it returns the issue
to `ready` for the board to try again on the same broken box.

The count is written **before** the prompt and whatever the prompt reports, for
`warn_overrun`'s reason: a chat that will not take a prompt is a chat there is no
point re-telling every cycle, and a resume that only counted when it landed
would let an unreachable chat be restarted forever.

One guard rides in front of the resume: `maybe_settle` runs first, with `Idle`
standing for the fact the branch has just established — there is no run any more.
A restart lands on attempts at every stage, including the one that had opened its
pull request thirty seconds earlier, and restarting that agent would spend a turn
undoing the work.

#### What this also fixes

A `blocked (errored)` row promises, in the words of the notice it sends, that
"the chat still holds the whole task, so it is a retry or a cancel, not a lost
attempt". On the afternoon in question two of those rows were swept to
`orphaned` minutes later, which broke the promise while the chat was still
sitting there. It was the same bug and it takes the same fix: the chat is there,
so the attempt is not lost.

### 2. Six notices about six tasks, none about the engine

The mass orphan surfaced only as N independent settle notices to the dispatcher.
Each was true. None of them said the thing that had happened — that the engine
restarted and took every live run on the box with it — and no amount of reading
them says it either, because the event is not about any of those six tasks.

Everything one reconcile pass finds is now collected and announced **once**, as
itself: `N live attempts lost their runs`, with what happened to each, to the
pinned orchestrator and to the operator's webhook (`on_runs_interrupted`, its
own event — a receiver routing on `on_settled` is routing on "a task is over",
and none of these are). Not to the dispatchers: a restarted attempt has not
ended, and an agent waiting on that step has nothing to do until it does.

### 3. `doctor` had nothing to say about a box that could not run anything

Every check was green while twelve consecutive attempts died in minutes, because
every check was about *configuration*, and the configuration was fine. Nothing
asked the question an operator actually has at that point.

`runs` reads the board's own history rather than probing — a run doctor starts
is not a run a dispatch starts, with its account, its sandbox and its worktree —
and the history is free. Over 24 hours: an attempt that ended in under five
minutes having finished nothing produced no evidence at all, and three of those
with nothing finishing between them is a box that cannot start work. It is
deliberately hard to trip: one attempt finishing anywhere in the window clears
it, and a live attempt counts as neither (it is evidence a run started, not yet
evidence about how it ends — counting it would fail the box for every dispatch
made in the last five minutes). An operator who learns to ignore this line has
lost the only check that would have caught this night.

### What is not here

The engine still does not restore session rows for chats whose runs it
interrupted at shutdown, and it should not: a restored `Idle` row would put the
board back on the settle path, where an attempt with no commits stays live and
idle until the two-hour cap closes it — silence for two hours instead of a
restart in sixty seconds. The board is where this decision belongs, because the
board is the only party that knows the chat was in the middle of a dispatched
task.

Nothing tries to distinguish "the engine restarted" from "the harness died on
this one chat". The treatment is identical and the log line names both; what
separates them in practice is the count, which is what `report_interrupted` puts
in front of the reader — one attempt is a bad run, six at once is the box.

- decisions: `crates/board/src/runs.rs`
- the sweep: `SyncEngine::chat_liveness` / `resume` / `give_up` /
  `report_interrupted` in `crates/board/src/sync.rs`
- the notice: `notify::interrupted_message` / `interrupted_payload`
- the check: `doctor::runs_check`
- the column: `attempts.resumes` — not a retry, not a re-open; the same attempt
  picked up after something outside the run killed it
