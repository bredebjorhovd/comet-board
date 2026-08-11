# Turn-level guardrails on a spinning run — **done** (gh#270)

Landed as `crates/board/src/spin.rs` (the pure decision) plus the actuation in
`drive_run` (`crates/engine/src/sessions.rs`), with `max_tool_failures` (10) and
`max_tool_calls` (2000) on `[defaults]` and per `[[route]]`.

The cap that existed caught the *slow* run: `overrun.rs` bounds an attempt by
wall clock, and `dispatch.rs` bounds how many run at once. Nothing caught the
fast one. An agent retrying the same failing command, or making tool calls
without end, burns tokens at full speed until the wall clock finally trips — and
is charged for the whole two hours on the way. For an unattended board that was
the biggest unguarded failure mode left.

comet cannot enforce this inside the agent's loop (that loop belongs to Claude
Code, Codex or opencode) and does not need to. It already watches the loop from
outside: every harness normalizes to `ToolCall` / `ToolResult`, `is_error` is
normalized too (claude's `is_error` block field, codex's `failed`/non-zero exit,
opencode's status — no normalize-layer work was needed), and the run loop owns
both the steering mailbox and the interrupt token.

The shape:
- **Steer, then stop.** At the cap, one message into the live run's mailbox
  naming what is looping. At twice the cap, the run ends. Steering first because
  most loops are recoverable and an interrupt throws away a worktree's worth of
  context — the same "never silent" rule the duration cap follows.
- **Counted from zero, not from the warning**, and the steer does not reset
  anything. The ladder has to hold for a run whose warning was never *delivered*
  — a harness with no steering, a mailbox torn down mid-turn — and a rung armed
  only on delivery would leave exactly those runs unbounded. Nothing is lost:
  one tool call that lands clears the failure counters outright, so an agent
  that takes the advice is out of the ladder entirely.
- **Same call vs. any call.** `max_tool_failures` is the count for the *same*
  call failing (whole target, not tool name — three broken files is not a loop);
  assorted calls failing get twice the rope, because flailing is the looser
  signal.
- **A stop is a block, not a lost attempt.** The hard rung ends the run
  `Errored`, which is the path that already existed: the board's reconcile reads
  it off the journal, `settled::decide` says `StayLive(Errored)`, `note_blocked`
  tells the dispatcher and the orchestrator, and the chat keeps its full context
  for a retry. Nothing new had to be invented for anybody to hear about it.
- **Per turn.** `Done` is the boundary and resets everything, warning included:
  the next turn is new work.
- **Resolved at dispatch, enforced in the engine.** Unlike the duration cap —
  decided by the board loop on its own interval, config in hand — this is
  decided from the events going past, where there is no board to ask. So
  `build_spec` resolves the route's limits and they ride the chat
  (`ChatConfig::turn_limits`) beside `push_repo` and `git_author`. A full-config
  replace preserves them (`WorkspaceHost::set_chat_config`), so changing model
  mid-session cannot disarm a dispatched chat.
- **Off for every chat nobody dispatched.** `TurnLimits::default()` is
  unbounded, and that is what a composer-created chat carries: a guardrail is
  something a board arms for work nobody is watching, not something to impose on
  a person sitting at their own session.

Deliberately not here: a separate `Stopped` variant for the block notice. The
board's block signal already says "the run stopped with an error — the chat
still holds the whole task", which is exactly true, and the *reason* is in the
chat, the transcript's error part, the journal's `Done` and the engine log.
Splitting the enum would have meant threading an error string through
`Runtime::last_run_end` and re-rendering four notification surfaces to say
something the reader can already see.

Also not here: a token or cost cap per turn. `Usage` arrives once per turn, at
the end, so there is nothing to count against mid-flight — the same accounting
gap gh#70 named.
