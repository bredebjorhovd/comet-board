# Relay-forward the board RPCs — **done** (gh#55)

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
