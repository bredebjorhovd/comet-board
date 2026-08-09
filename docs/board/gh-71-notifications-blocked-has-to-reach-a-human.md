# Notifications: blocked has to reach a human — **done** (gh#71)

Landed as `crates/board/src/notify.rs` (who is told what, and the wording)
plus the effects in `sync.rs`. Before it, `notify`/`notify_dispatcher` were
parsed, documented and reported by `doctor` — and read nowhere. Worse, the
one state that most needs a signal produced none: a `blocked` attempt settles
nothing and closes nothing (correctly — the chat holds the context and the
call is the operator's), so no outcome writeback fired, and an agent that
asked a question at 02:00 was discoverable only by looking at the board.

Three audiences, three channels, and the point of the design is that they are
not the same person:

- **The issue.** Entering `blocked` queues a `blocked` writeback — one comment
  saying whether the agent is waiting on an answer or its run died, and what
  to do about each. Keyed `<task>:blocked:<attempt>:<block>` off the new
  `attempts.blocked_count` column, bumped on the *transition* into blocked:
  once per block, so a block that lasts three hours is one comment and a
  question answered at 09:00 followed by another at 11:00 is two. Delivered
  by the existing queue, so GitHub's per-repo `writeback` decides at delivery
  exactly as it does for dispatch and outcome comments. An attempt that blocks
  and settles in the same pass (an errored run whose PR is already open) gets
  the outcome comment only — two comments contradicting each other is worse
  than one.
- **The agent that released it.** herdr-board's AGE-25 dispatcher wake, now
  ported: `notify_dispatcher = true` prompts the dispatching chat when its
  released work settles (or orphans), over the same `Runtime::prompt` review
  delivery uses. The provenance was already on the attempt row
  (`dispatched_by_pane`). Off by default here, on the grounds that an
  orchestrator woken by every child it released cannot hold a train of thought
  — §gh#165 inverts that and says why the sentence was about the other channel.
  Operator-released work has no dispatcher chat, so the switch is silent for it
  by construction, which is why it stays separate from the operator's own.
- **The operator, out of band.** `notify` is now real: it switches one webhook
  URL (`notify_webhook`), POSTed `{"event": "on_blocked" | "on_settled", …}`
  with a `text` line for endpoints that render nothing else. One URL, no
  per-service clients — Slack, ntfy, a pager and a two-line relay all already
  accept a POST, and a board holding three credentials it never reads would be
  three more things to be wrong. Five-second timeout, no retry: the writeback
  queue retries because a comment is worth the same tomorrow, and a
  notification is not — one delivered forty minutes late reads as current.
  A dead endpoint logs and is dropped; it never holds a settle open.

`doctor` now matches reality, which was half the bug. Its settle-notice line
no longer claims "only you are notified when released work settles" — nothing
notified you. There is a `blocked notice` line that names the read-only repos
where a block really does show nowhere but the board. And an `operator notice`
line reads the two keys together and is true in each state: *not configured*
(no webhook — a preference, so `ok`, but worded so nobody reads it as a notice
that fires), *on*, *muted* (`notify = false` over a configured URL), and the
one genuine fault — an address that cannot be posted to, where the operator
asked for the notice and every one is being dropped into a log line. Only that
last state fails; a `doctor` that exits 1 over a preference stops meaning
anything.
