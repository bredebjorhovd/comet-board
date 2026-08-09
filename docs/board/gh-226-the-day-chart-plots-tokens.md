# The day chart plots tokens, not dispatches — **done** (gh#226)

Step 8's chart, corrected. gh#179 rebuilt the stats page around spend and kept
the day chart it inherited: dispatches per day, two tones in one bar for the
share that ended `done`. It is now token volume per day, with the dispatch
count in the caption under it — `1.31M` over `Mon 3 · 2`.

### Dispatch count is nearly flat, and nearly meaningless

Two dispatches can differ by twenty times in what they cost. A week of
five-a-day is five bars of the same height whether it was a week of one-line
label fixes or a week of migrations, so the chart's shape carried almost no
information, and the number that *does* vary between days — tokens — was drawn
in another block as an unlabelled forty-pixel strip nobody could read a figure
off.

Tokens is also what the spend block directly above is computed from. Plotting
it here means the two blocks argue one case: this is what the days cost, this
is what that came to at list price. Before, they were two charts of two
different quantities that happened to share a calendar.

The completion share the bar gave up did not go missing — it is a sentence in
the headline above the chart (`82% ended in done`), which is where a proportion
belongs. A reader who wants "how much of a day's work landed" was never going
to recover it from two tones at 92 pixels anyway.

### A quiet day is a hairline under a dash, never a gap

The failure this is drawn against: a seven-day window on a board that worked
one day rendered as a single lonely bar with nothing beside it, which reads as
six days the board failed to record rather than as six days nobody dispatched.

`day_columns` returns a column for every day in the window's calendar, and a
day with no tokens on it draws a one-pixel rule in the border's tone with `—`
above it and its real dispatch count below. The em dash is the same rule the
totals follow: a day that spent nothing has no figure to show, and `0` printed
over a hairline is a number nobody needs to read.

`DayColumn::is_quiet` deliberately does not distinguish "nothing ran" from
"what ran reported nothing" — the caption's dispatch count says the first, and
the coverage line at the top of the block says the second, once, rather than
per bar.

The board-side test that promised this (`a_window_draws_a_bar_for_every_day_
including_the_quiet_ones`) only ever checked that a *bucket* existed. It now
checks what the chart makes of the buckets: seven columns, six of them at zero
height with a dash, and the one that spent something the only one with a bar.

### The arithmetic is in proto, like every other rule on this page

`day_columns`, `token_fraction`, `short_day` and `day_captions_fit` are in
`comet_proto::view::stats` beside `ranked_top`, `hour_grid` and `bar_fraction`
— a stats page is mostly arithmetic on the way to a layout, and arithmetic done
twice is arithmetic done differently. The columns zip the two series **by
date**, not by index, so a board answering with series of different lengths
draws short rather than putting a spike under the wrong day.

Two rendering rules worth naming:

- **`short_day` is `Mon 3`.** Weekday and day-of-month, not the ISO date: under
  a bar, `2026-08-03` is ten characters of which two are news, and the weekday
  is the half a reader actually pattern-matches on.
- **`day_captions_fit` caps captions at ten columns.** A week is captioned; a
  month is not — at eleven pixels a caption is about fifty wide, and thirty of
  those want a chart no window is. Past the cap the columns go bare and the
  axis carries the range, which is the shape a month is read for anyway. The
  peak annotation reads in tokens (`peak 1.31M/day`) either way, and says so in
  words when no day in the window reported any.

### The token strip in block three is gone

It was the same series, drawn twice on one page — once captioned with a peak,
once as an unlabelled strip — which is two answers to one question. The tokens
block keeps its totals, its four buckets and the per-model table it exists for:
what the spend was made of, not when it happened.

### Not touched: the phone

`apps/ios/Comet/Views/StatsView.swift` still draws dispatches per day, and
`peak_dispatches` stays for it. iOS is gh#181, after the desktop system
settles; when it catches up, `day_columns` is the arithmetic it adopts and the
cross-language fixture is where the case goes — the same note `hour_grid`
carries for the same reason.
