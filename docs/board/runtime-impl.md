# `Runtime` impl against engine internals — **done**

Landed as `crates/engine/src/board_runtime.rs` (`CometRuntime`) plus the real
RPC handlers (see BOARD.md's ported-and-working list). Dispatch/cancel execute
on the board loop's thread through a command channel; `WatchBoard` is a
`watch_stream` fed by the loop after every cycle, status refresh, and command.
Still deliberately open: wiring `chat_alive` into reconcile's
never-started-chat verdict.
