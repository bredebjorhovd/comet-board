# Review: what changed, not which lines — **done** (gh#180)

The UI half. §gh#183 landed the contract, the storage and the unclaimed-set
computation; this is the screen that renders them, and the reason the two were
split: landing the surface first would have produced a screen showing an empty
contract.

Deliberately **not** a diff viewer. A diff viewer here would be a worse GitHub,
and it would be built around an activity that stops scaling the moment a fleet
generates more code than one person can read. Comet dispatched the brief, so
comet is the only thing that can put what you asked for next to what the agent
says it did next to what actually changed. That is the one question GitHub
cannot answer, and it is the only question this screen asks.

### The layout is inverted, and the inversion is the argument

Everywhere else in the app the conversation is the content and a diff sits in a
dock beside it. Reviewing inverts that, because reviewing inverts what you are
doing: the changes are what you came to read, and the chat is the reference you
consult about them. So `Route::Review` puts the session in a narrow column — on
the **left**, where it says the true thing about the mechanism, that you write
here and it lands over there — and gives the card to the review.

The column is the same `Transcript` and `Composer` entities the chat route
mounts, at their own persisted width (`review_session_width`, 320–620, default
420 — a reference width, deliberately narrower than the 520 the Changes dock
takes as content). It keeps its composer, because the most useful thing a
reviewer can do about an unclaimed change is ask the agent that made it, and it
drops the terminal dock, which at that width is a letterbox.

A review outlives the chat that produced it — that is why §gh#183 records claims
on the *attempt* — so `chat_id: None` is a real state and the column says where
the session went instead of drawing an empty transcript.

### One hue means "look here", and only the remainder gets it

The screen paints in the status ramp (gh#173): the alarm tone is the ramp's
blocked hue, the same red a blocked or failed row wears on the board. An
unclaimed change is not a new kind of bad news needing a new colour; it is the
board's existing "something is wrong here", said about a diff.

Three things wear it and nothing else does — the verdict strip under the header,
the remainder block, and the inline `!` marks that flag a contradiction. The
remainder is the only *bordered, tinted* block on the page and carries the only
figure-size number (gh#174's one off-ramp size), because it is the number the
screen exists to produce. A screen where three things shout has nothing left to
shout with.

The verdict strip is pinned above the scroll on purpose: the sections read in
the order the question is asked — brief, claims, evidence, remainder — and a
long issue body would otherwise push the loudest fact under the fold.

### Every claim carries evidence it did not write

No claim is drawn alone. Under each one sit the changed files its anchors
reached, with git's status letter and line counts — the same row the remainder
draws, so the two read as one table — and where a claim's anchors match nothing,
that is drawn too, in the alarm hue. Work described that did not happen is as
interesting as work nobody described.

Underneath, the run's own evidence: what was executed and how it exited, off the
run journal every harness writes without being asked. Scoped exactly as §gh#183
scoped it — which tests ran, and which call sites moved, remain named follow-ups
rather than half-built here too.

### The reading is shared, not the rendering

Whether a review is alarming, and the sentence that says so, are
`claims::verdict` and `claims::findings` in `comet-board` — read by this card
*and* by `comet-board review`, which now prints the same verdict line above its
sections. Two surfaces that phrased the same attempt for themselves would
eventually disagree about it, and the one place that must never happen is the
surface whose whole job is to be trusted about what a run did.

`Tone` has three values, not two: "nothing is wrong" and "nothing was
established" are the opposite of the same thing. An attempt that never answered
the contract has no findings against it and has also proved nothing, and
painting that green would report an absence of evidence as evidence of absence.
It is the same distinction §gh#183 keeps between `NULL` and `[]`, carried all
the way to a colour.

### Getting there

`r` on any board row that has been attempted, or the "Review changes" chip in
the peek (§gh#132). Not a `RowAction`: the shared action set is the verbs the
*board* offers on a row, and they must mean the same thing wherever they are
drawn — a phone drawing a chip for a screen it does not have would be a promise
the set cannot keep. What *is* shared is the rule about which rows have one,
`view::board::reviewable`: one attempt is enough, and the state is not consulted.
A failed attempt left a branch and a run journal behind it, and "what did it
actually change before it gave up" is arguably the most useful version of this
question.

The board panel reaches the shell by event (`BoardEvent::OpenReview`) rather
than by setting the route itself: opening a review is a route change, and a dock
that owned the window's route would be a dock that owned the window. ⌘⇧B is the
way back — the board is where you came from, so the same key returns to it.
