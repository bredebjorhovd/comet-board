# A settle announced twice is one settle — **done** (gh#356)

Seen on the box on 2026-08-12, board v0.4.0, dispatching and reviewing two
orion-productmapping tickets — gh#32 on Codex and gh#7 on opencode. The
"work you released has finished" notice arrived in the dispatching chat
repeatedly for the *same* settled attempt: `attempts` still `1` on both rows,
`git ls-remote` returning the identical head on two consecutive notices
(`31d8fc5…` and `1fbcfb4…`), no new pull request event, no state transition.
`last_outcome_at` advanced each time and nothing else did.

Landed as `Signal::settle_print` in `crates/board/src/notify.rs` and
`SyncEngine::is_news` in front of `announce`.

### Why an attempt settles more than once

The settle path only ever runs on live attempts, so a repeat needs the attempt
to have gone live again in between. It had: §settle-logic's inverse re-opens a
closed attempt whose chat starts working, *rather than* re-dispatching it, and
that is why `attempts` stayed at `1` while `ended_at` — which the row publishes
as `last_outcome_at` — moved. The row was not lying. It was reporting churn.

The churn is cheap to start and free to repeat. Both tasks sat in `review` with
an open pull request, and a pull request short-circuits the whole evidence
hierarchy: no commits are consulted, no push is checked, the attempt is finished
because the agent said so. So *anything* that makes the chat work again —
a review comment the board delivers into it, an operator's follow-up, the agent
answering that it already handled the point — re-opens the attempt and settles
it again on the spot, on the same pull request it settled on the first time.

Some of those repeats are the feature working exactly as designed, and the same
two dispatches showed both faces of it: a review was posted, delivered into the
authoring chat, the agent pushed a fix, and the attempt settled again with a
real new commit behind it. Twice. That notice has to arrive.

### What the notice is keyed on now

Not the event, and not the attempt: **what the notice asserts, plus the branch
head it asserts it about.** `Signal::settle_print` joins the outcome, the
evidence, the pull request, the note, and the attempt checkout's `HEAD`; the
last one announced is kept on the attempt under `settled:<id>`, beside the
`rewritten:<id>` mark §gh#286 keeps for the same reason. An equal print means a
woken dispatcher would be read the same sentence about the same commit, so it is
not sent — on any channel.

That is the issue's own list of what may re-fire a notice, made concrete: a new
commit moves the head, a new pull request moves the URL, a state transition moves
the outcome. Anything else was the board noticing the same close twice.

**Local `HEAD`, not origin's, and never a fetch.** The question is whether the
agent did anything since the last notice, and the checkout answers it offline: a
settle path that reached across the network to decide whether to *speak* would be
paying a poll for a notice. Local `HEAD` also moves in a superset of the cases
origin's does — work has to exist locally before it can be pushed — and that is
the direction to be wrong in. A commit that never left the box costs one repeat
notice; reading a stale tracking ref would cost a suppressed real one. An
unreadable checkout prints as `-`, which is a value like any other and differs
from every real head, so it fails towards sending too.

**Blocks keep their own counter.** `settle_print` is `None` for a block:
`Attempt::blocked_count` already tells a block once, and it counts a state rather
than an event. A second mark for the same thing is only a second way for the two
to disagree.

**The mark records what was announced, not what was delivered.** A dispatcher
whose chat was archived does not get its notice re-sent on the next reopen. The
attempt is closed, the comment upstream is the durable trail, and `wake_dispatcher`
already says out loud that retrying a notice about a thing that already happened
is how a dispatcher gets told twice.

### The webhook is suppressed with everything else

`notify_webhook` is beside the agent channels and unconditional on either, but it
has the same complaint: a POST that arrives reads as something having happened.
So the guard sits in front of `announce` rather than inside one of its channels,
and a repeat is an announcement that does not happen. The suppression leaves one
`info` line naming the branch, so an operator reading `syncd.log` can tell a
suppressed repeat from a settle path that never ran — the discipline §gh#194
established for every other silent exit on this path.

### `last_outcome_at`, and why it is left alone

The issue asks whether the timestamp moving on an unchanged attempt is the same
bug. It is the same churn, but it is not a lie: the attempt really did close
again at that moment, and `ended_at` is what `gc` prunes by and what `stats`
measures durations with. Pinning it to the first close would make an attempt that
re-opened, worked and settled again report a duration that ends before its own
last commit.

What the row was missing was never the timestamp — it is `reopened`, which the
row already carries and the history line already renders as `reopened 2×`. The
dispatcher checking "has anything changed?" was reading `attempts`, which by
design does not move for a reopen. With this issue the question stops being
asked at all for the case that prompted it, because the wake-up that prompted it
does not arrive.

### The residual

An agent that commits locally without pushing, under a still-open pull request,
moves the print and gets one repeat notice for work no reviewer can fetch. It is
the acknowledged direction of the trade above, it is one wake-up rather than a
stream of them, and closing it would mean asking origin what its head is on a
path whose whole job is to say something.

### Adjacent

gh#339 — on the same two dispatches the Codex run submitted no claims at all, so
the review contract went unanswered while this handshake over-fired. Both are the
settle/review path misbehaving; the causes are separate and so are the fixes.
