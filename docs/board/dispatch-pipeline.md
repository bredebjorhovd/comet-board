# Dispatch pipeline — **done**

Landed in `crates/board/src/dispatch.rs` + the engine's `handle_dispatch`
(see BOARD.md's ported-and-working list): concurrency caps as a refusal before
anything is created, `via` resolved into parent-task/chat provenance on the
attempt row and the upstream dispatch comment, `{worktree}` threaded into the
brief at execution time, and `COMET_BOARD_CHAT_ID` exported where the harness
spawns (`RunControls::chat_id` → child env, claude and codex adapters).
