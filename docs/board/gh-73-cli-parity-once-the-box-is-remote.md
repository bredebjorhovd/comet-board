# CLI parity once the box is remote — **done** (gh#73)

Three gaps that only bite when the board is not on the machine you are typing
on, and the desktop app is not the only frontend. All three are in
`apps/board-cli` (the third also reached the TUI's board pane, while that
existed — removed in gh#416):

- **`--device`.** §gh#55 relay-forwarded the *frontends*; the CLI still hardcoded
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
  row retried from a shell and from the panel takes the same path. (The TUI's
  board pane gained the same half — `R` retried, replacing on blocked — and
  left with it in gh#416.)
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
