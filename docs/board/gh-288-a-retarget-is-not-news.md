# A retarget is not news — **done** (gh#288)

Stacks 7/9, on top of gh#282's stored `base_ref`. Landed as `Delivered::base_ref`
and `Decision::Retargeted` in `crates/board/src/review.rs` — a few lines in
`plan_delivery`, and the argument for why they are safe, which is the rest of
this file.

### The cost this removes

Review delivery has one poll-cost gate, and it is the pull request's
`updated_at`: the list the sync cycle already fetches reports it, so "did
anything happen since the watermark?" is answered without asking any comment
endpoint. Only when it says yes does the board run `Github::pr_feedback` —
issue comments, inline comments, reviews, three calls.

A stack breaks the question. When a lower layer merges, GitHub retargets every
pull request above it, and every retarget bumps `updated_at` with nothing on any
of the three endpoints to show for it. Merge the bottom of a five-PR stack and
that is four pull requests × three calls, per merge, every one of them returning
`NothingNew` off the per-endpoint watermarks (§review-delivery). Correctness was
never at stake — the watermarks were already doing their job. It is cost, and
what §gh#283 makes ordinary is a stack, so the rare edge became the common case.

### The gate, with the base folded in

`Delivered` records the base branch those watermarks were computed against,
beside the timestamp. A tick where `updated_at` moved *and* the base moved is a
retarget: the base change accounts for the timestamp change, both halves are
written, and no endpoint is asked. `plan_delivery` answers `Decision::Retargeted`
— an outcome distinct from `Unchanged` because it is a different fact about a
pull request that did move, and the caller says so once in the log, so an
operator watching a stack land can see why a moved pull request asked GitHub
nothing.

Two exclusions, both because an unknown base differs from every real one:

- **First sight is never a retarget.** The first-sight floor — never deliver a
  pull request's back catalogue — applies only on the first look. A skipped
  first look would leave the floor unapplied *and* leave `first_sight()` false,
  so the next time anything moved, the whole back catalogue would arrive above a
  watermark of zero. That is not an optimisation misfiring, it is the agent's own
  PR-opening chatter pasted into its chat.
- **State written before this field existed is never a retarget.** An empty
  `base_ref` means unknown, not "no base". The first tick after an upgrade
  fetches exactly as it always did and records the base it compared nothing
  against; the gate is armed from there on.

### The delay, said honestly

The gate is approximate, and the approximation is directional: a retarget and a
comment can share a poll window, and then the comment is not fetched.

It is not lost. It was never consumed either — nothing was asked, so no
watermark moved — so it is still above its watermark and will be delivered
whole. But **it is not delivered on the next tick.** `state.updated_at` is
advanced to the retarget's own timestamp, which is the point: leaving it behind
would mean fetching one tick later and paying the same 4×3 calls this issue set
out to remove. With it advanced, the top gate answers `Unchanged` on every
following tick until **some other event moves the pull request** — another
comment, a review, a push, the next layer landing. The first tick after that,
with the base standing still, fetches and delivers everything above the
watermark, the waiting comment included.

`a_comment_that_shares_a_window_with_a_retarget_is_delivered_late_not_lost`
pins exactly this, and steps the clock to a later event rather than to the next
tick, because the next tick is not what recovers it.

### The residual risk

A reviewer who leaves exactly one comment inside the same poll window as a stack
landing, and then waits, sees nothing delivered into the agent's chat until
anything else touches that pull request. If nobody touches it, nobody is told.

That is real, and it is the price. It is not paid down by fetching one tick
later — that is the same three calls per layer per merge, which is the entire
cost this issue exists to remove — so the alternative buys nothing. The window
is one sync tick wide and needs a comment to land inside it; the recovery is
anything at all happening on the pull request afterwards, including the agent's
own next push. Rare, cheap to escape, and written down here so the person who
hits it finds an accepted trade rather than rediscovering it as a bug in
delivery.

### What was considered instead

Comparing per-endpoint `latest`-style timestamps, so "PR metadata moved" and
"feedback moved" could be told apart outright. The pulls list payload does not
carry them: a pull request item has `updated_at` and nothing that distinguishes
which of its many faces moved. Asking for them is the fetch this gate exists to
avoid.

### Not in this issue

Delivering across a stack — what an agent on layer three should be told when the
review lands on layer one — is 8/9. This issue only stops the board asking about
layers nothing was said on.
