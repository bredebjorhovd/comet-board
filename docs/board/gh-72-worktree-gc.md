# Worktree gc — **done** (gh#72)

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
