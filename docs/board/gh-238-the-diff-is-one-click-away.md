# The diff is one click away, and the header says whose turn it is — **done** (gh#238)

The last two pieces of §gh#180's design that nothing had claimed. This ticket
was re-scoped twice and shrank both times: §gh#239 built the verdict bar, so
what is left is not the bar but the strip under it and the pill above it. Both
are in `crates/ui/src/review.rs`, and neither needed a byte of new data.

### The strip that closes the body

The review screen is not a diff viewer. That is the oldest sentence in
§gh#180's write-up and it is not being softened here: a diff viewer on this
screen would be a worse GitHub, built around an activity that stops scaling the
moment a fleet writes more code than one person can read.

But a refusal to draw the diff is only a design if the diff is somewhere. Until
this ticket it was nowhere — the card named files, counted their lines, and
offered no way to see one of them. `crates/ui/src/changes.rs` has drawn diffs
since long before the board existed; it simply could not be reached from here.

So the body closes with a hairline row: `3 files changed`, `+117`, `−33`, and a
`Read the diff` chip. The counts are summed off `review.changed` — the same
list the remainder is subtracted from — so the strip is a restatement of the
denominator and not a second reading of the branch. A screen cannot disagree
with itself about how many files there are if it only counts them once.

**The chip is the whole point, and it does not always draw.** It routes; it
does not render. `changes.rs` reads the *live* checkout of whichever chat is
selected, so the chip needs two things to exist: a chat to select, and a
checkout still on disk. A review whose diff came from the board's recorded
snapshot is one whose worktree `gc` reclaimed (§gh#72) — the numbers are true
and there is nothing left to open — and a review outlives its chat, which is
the reason the claims live on the attempt at all (§gh#183). In both cases the
counts stay and only the chip goes: the numbers are the fact, the chip is the
route to the rest of it.

**It leaves by event.** `ReviewEvent::ReadTheDiff` carries the chat id up to
the shell, which selects that chat, drops back to `Route::Chat` and opens the
right dock — the same shape `BoardEvent::OpenReview` has, for the same reason.
A panel that reached over and set the route would be a dock that owns the
window. The dock open is idempotent on purpose: a session whose diff pane was
already open would otherwise have the chip *close* the thing it was asking for.

The one colour argument worth recording. §gh#180 reserves the ramp's blocked
hue for the unclaimed set — one hue on the screen means "look at this", and a
screen where three things shout has nothing left to shout with. The design
paints `−33` in that hue anyway, and it is right to: there the red belongs to
the minus sign rather than to the screen. Every diff ever printed paints it,
and a grey minus would be the only one in the app that is not. It is written
down as the single carve-out rather than left for somebody to discover as an
inconsistency.

### The pill that says whose turn it is

While a verdict had to be written on GitHub, the review card was a reading and
the answering happened elsewhere. §gh#239 ended that: the verdict is composed,
previewed and sent from this screen. Which means a review waiting for a human
and one that has already been answered are now the same screen exactly — same
verdict strip, same claims, same bar, same everything.

The pill is the difference, and it sits beside the identifier rather than out
by the links, because whose turn it is is a fact about the review and not an
action you can take on it.

Three readings:

- **`Waiting on you`** — the board has parked the task in `review`. The pill
  wears the board's own review hue, which is the colour of the row it was
  opened from.
- **`Blocked on you`** — the agent asked a question. Also your turn, about
  something else, so the same hue and a different word. Notably *not* the
  blocked hue, even though the board paints that row in it: that hue belongs to
  the unclaimed set on this screen, and a header pill wearing it would be the
  loudest thing on the page saying something the remainder block is there to
  say.
- **`Answered`** — this session submitted a verdict. It wins over the board's
  state because it is newer: the state moves when the sync loop next reads the
  pull request, some seconds after the button was pressed and not before.

Everything else gets no pill. An attempt still running and one long since done
are nobody's turn, the facts line beside the pill already says which, and a
badge on every review is not a signal.
