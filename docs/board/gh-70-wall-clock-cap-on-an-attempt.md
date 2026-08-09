# Wall-clock cap on an attempt — **done** (gh#70)

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
`saw_working`, deliberately left alone by §runtime-impl) is closed by the same clock.

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
