# Stats, rebuilt around spend — five blocks, not twelve cards — **done** (gh#179)

Step 8 of gh#171, and the UI half of the money work: gh#182 computes the price
and gh#178 is where the rates and plan costs get entered. This is what renders
them.

The page was a column of about a dozen cards, each holding one number and
answering no question in particular — five token tiles, a completion tile, an
hour card, a workspace card, three more tallies — and the reader had to
assemble the question out of them. It is five blocks now, and the order is the
argument: **what it cost**, what it ran, what it spent that on, when and where,
and who.

### The headline is spend, and the two halves are never added

The block leads with three figures: what the window's tokens would have cost at
the meter, what the plans behind them cost over the same days, and the multiple
between the two. That last one is the question people actually open the page
for — *how far does the subscription carry this* — and before gh#182 the page
answered it by showing token counts and letting the reader guess.

The pair is a ratio and never a total, which is the rule
`BoardSpend`'s own doc sets: the list price is the board's arithmetic over
tokens it counted, the subscription is a number a person typed beside a login,
and on a box carrying several teammates' slots (gh#59) adding them would sum
other people's plans into one figure and call it the board's spend.
`subscriptions_in_window` and `subsidy` are the board-wide counterparts to the
per-account ones already on `AccountSpend`, and `human_multiple` phrases the
result — precision growing as the number shrinks, because nobody acts on the
difference between 12.3× and 12× of subsidy and everybody acts on the
difference between 0.9× and 0.4×.

**Comet never sees your bill, and the block says so** rather than implying it
knows. It says which half is a list price off a dated table and which half is
what you entered in Accounts, and an unentered plan reads `not entered` — not
`$0.00/mo`, which would be a claim about somebody's finances. The three ways
there is no price (no rates, rates that matched nothing, no plan to compare
against) each keep their own sentence; `BoardStats::spend_label` owns which one
it is.

### The crossing was the fact both cards were hiding

"When do I release work" was an hour histogram. "Which spaces" was a workspace
tally. Between them a reader can tell that the board runs late and that one
repo takes most of the work, and cannot tell whether those are the same fact —
which is the only thing either card was ever going to be used for. A crossing
is not recoverable from its margins.

So the two are one block: a grid of space × hour, with the workspace totals
down the right edge and the hour histogram along the bottom. Both margins
survive, at the cost of nothing, and the interesting cell is drawn.

That needed one field, not one new thing recorded: every attempt already
carries its start time and its workspace, and `hours_by_workspace` is the same
sweep of the same rows keeping them together instead of apart. It is
`#[serde(default)]` on the wire, so a board older than the field answers
without it and the block degrades to the histogram it replaced rather than
failing to decode. `hour_grid` folds and ranks it with the rule every other
tally here uses — biggest first, ties alphabetical, the tail folded into one
`n others` row that carries the hours it stands for, and the column margin
summed *after* the fold so the bottom of the grid still totals every dispatch.

### What else moved

- **The token tiles are a line.** Five bordered tiles for one total and its four
  buckets is a card per number; they read as one figure and four label-value
  pairs beside it, over the per-model table they qualify.
- **The per-model table has a cost column** when the board could price it, is
  ordered by cost rather than by tokens (this table now sits under a money
  headline, and the row a reader wants is the expensive one), and carries the
  provenance the rate came from when it is news — a family fallback, or an
  override from `routing.toml`. An unpriced model is one honest row at the
  bottom and a dash, never `$0.00`.
- **The day charts stayed, in the blocks they belong to.** Dispatches per day
  under the throughput headline; tokens per day as a strip inside the token
  block, because it qualifies the total above it rather than standing alone.
- **The four glance facts** — where the work landed, friction, agent time, who
  released it — are label-and-value rows beside the day chart. A card per fact
  is what made this page a scroll.
- **The width is the shared one.** gh#178 named two column widths and this page
  takes `dashboard_column` (1160), rather than opting out of the form width
  with a comment attached.

No new data collection, in the sense the ticket meant it: nothing new is
recorded, no new sweep runs, and every figure here is one the board already
had. It is the same numbers asked a better question.
