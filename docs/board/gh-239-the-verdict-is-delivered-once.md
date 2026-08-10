# The verdict is delivered once, into the checkout the agent is still in — **done** (gh#239)

The outbound half of review delivery. §gh#180 is the inbound one: a review
written on GitHub, noticed by the sync loop, relayed into the chat that wrote
the pull request. This is what happens when the review is written *here* —
`crates/board/src/verdict.rs`, the `SubmitVerdict` RPC, and the verdict bar at
the bottom of the review card.

Three things make it different from typing the same sentence into GitHub.

### The unclaimed changes ride along

The remainder §gh#183 derives — diff minus claim anchors — is recomputed at
submit time on the board's host and attached to **both** copies: the review
posted on the pull request and the prompt queued into the chat. It is not a
parameter of the call. A reviewer cannot be asked to retype the one part of the
screen they did not write, and a caller that could supply it could also get it
wrong.

The chat's copy spells them `[unclaimed] Cargo.toml — added, +1 −0`; the pull
request's copy lists the same lines under `2 changes no claim accounts for`.
File granularity, because that is the granularity the remainder has — a symbol
anchor is how a *claim* is matched (§gh#235), not a way of cutting a changed
file into pieces.

### It goes into the checkout the agent is already in

Delivery is `Runtime::prompt` into the authoring attempt's chat, guarded by the
same author check the inbound path makes: `chat_alive`, plus
`review::still_the_authors_checkout` on the chat row's cwd. A chat re-pointed at
another checkout is somebody else's session, and a review pasted into it is
delivered to the wrong author. The review is still posted — the pull request is
where it belongs regardless — and the receipt says nobody was told, which is a
real outcome rather than an error.

An approval with nothing written in it is posted and delivered to nobody, which
is §gh#180's actionability rule kept on the way out: it says the agent has
nothing left to do, and interrupting it to hear so is the opposite of useful.

### Once, on the watermarks that already exist

A verdict delivered twice is worse than one delivered late, and there are two
ways to send it twice.

**Submitting twice.** Each submission is recorded in `Delivered` — the same
`meta` row §gh#180 keeps its watermarks in — under a fingerprint of
`{attempt, verdict, prose}`. A double click or a retried call finds it and
finishes whichever half failed instead of posting a second review. A different
sentence is a different review and does go out.

**The poll finding our own review.** The id GitHub assigns the posted review is
written straight into the inbound `review` watermark, so `deliver_reviews` can
never hand it back: an id at or below the watermark is consumed, forever. That
is the reason `Github::post_review` exists rather than a `comment` — it is the
call that answers with an id. When GitHub answers without one, the body's
`POSTED_MARK` trailer is what `review::is_the_boards_own` recognises instead.
The watermark is the mechanism; the mark is the backstop.

Order is post → record → deliver, which is the recoverable one. A crash between
the post and the record costs one duplicate review, a window of milliseconds;
recording first would remember a failed post as a success and the verdict would
never reach GitHub at all.

### What it refuses

A closed pull request — the payload's promise that the branch is still live
would not be true — and a `comment` or `changes requested` with nothing written
in it, which GitHub refuses itself and which tells an agent to change something
unnamed. Both come back as the call's error, decided where the pull request and
the diff are rather than in whichever client sent it.

What it deliberately does **not** refuse is a board with `[github] writeback`
off. That flag is off by default because it governs comments the board
volunteers — a dispatch line on an issue nobody asked it to narrate. A verdict
is the opposite kind of write: a human wrote the sentence and pressed the
button, in the same second, about a pull request they are looking at.

### The bar, and the preview above it

`crates/ui/src/review.rs` grew the part of the design this ticket needs: a
comment box, the three verdicts as a picker, and a Submit button, pinned under
the scroll for the same reason the verdict strip is pinned above it — the thing
you came to do must not be reachable only by scrolling past a long issue body.
Enter submits and shift-Enter is the newline, as in every other box in this app.
(The rest of gh#238's list — the `Read the diff` strip into `changes.rs` and the
`Waiting on you` pill — is still gh#238's; this is the button and what it does.)

Above them is the dashed `WILL BE DELIVERED ON SUBMIT` card: mono 11/17,
`border-dashed` in `border_strong`, faded out from 72% of its height. It renders
`verdict::compose` — *the same function the board sends with*, called rather
than reimplemented, because the one promise the card makes is that it is showing
what will be sent. The unclaimed lines are drawn in the alarm hue there, since
they are the part of the payload nobody typed.

Under the buttons, the contract in a sentence: *"Delivered into board/gh-138
once, with both unclaimed changes attached."* It follows the armed verdict — a
bare approval says it is going nowhere — and after submit the receipt line
replaces the promise with what actually happened.

The board's own `comet-board verdict --task … --request-changes --comment -`
does the same thing from a terminal, for the orchestrator that reviews without a
window open.
