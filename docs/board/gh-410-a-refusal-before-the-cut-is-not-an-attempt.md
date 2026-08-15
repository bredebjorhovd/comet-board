# A refusal before the cut is not an attempt — **done** (gh#410)

Found by the gh#337 rig. Two `--onto` releases against a parent that had not
pushed were both refused — correctly, with the message gh#337's case 21 pins:
nothing was cut, no chat was made, no agent ran. But each refusal left a
`failed` attempt row behind, so after one real run the row read `attempts = 3`
and the successful attempt was numbered 5.

Same class as gh#390: the count is what a reader uses to judge whether a task
is going badly, and a pre-flight refusal is not a failed attempt. It also makes
the `--onto` refusal expensive to retry, which is the wrong shape for a refusal
whose whole point is "push the parent and come back".

### The rule

An attempt is recorded when a branch is cut, not when a release is asked for.

The insert itself cannot move: it lands before the runtime runs because the
partial unique index on live attempts is the duplicate-dispatch guard, and a
second concurrent release has to lose on that index *before* a worktree or chat
exists. So the row opens optimistically, and what changes is what happens to it
when the dispatch does not.

The failure splits where the checkout does:

- **refused before anything was cut** — `CometRuntime::dispatch` fails inside
  `create_worktree_on`: the base could not be fetched (gh#67's stale-checkout
  refusal, which is where `--onto` an unpushed parent lands), origin could not
  name its default branch, the worktree could not be opened. No branch, no
  chat; the world is exactly as it was before the release was asked for. The
  error is typed `RefusedBeforeCut`, and the board **deletes** the row it
  opened instead of closing it `failed`.
- **failed after the cut** — the account would not materialize, the chat could
  not be created, the brief could not be queued. By then the branch exists, so
  the attempt happened; the row closes `failed` and stands, as before.

`Db::delete_attempt` is guarded to rows nothing ever ran under — no pane, still
open. A row a chat ever held is history, and history closes through
`close_attempt`; the guard means a bug that mislabels a later failure as a
refusal cannot eat a real attempt's record.

### What is not here

Refusals raised before the insert — the space cap, a missing space, a harness
the box cannot start (gh#187), a billing refusal (gh#101) — never opened a row
and are untouched. Nothing retries the refused release on its own either:
dispatch stays operator-driven, so a refusal that leaves no trace cannot become
a hammer on origin.

- the split: the error arm of `handle_dispatch` in `crates/engine/src/board.rs`
- the type: `RefusedBeforeCut` in `crates/board/src/runtime.rs`
- the tag: `CometRuntime::dispatch`'s checkout phase in
  `crates/engine/src/board_runtime.rs`
- the delete: `Db::delete_attempt`, guarded to pane-less open rows
