# Review window — the canvas, as checkable claims

Source: `canvas/comet-review-window.dc.html`. Tokens: `tokens.md`. Shell claims
(titlebar, sidebar) are `window.md` §A–§C and are not restated here. Issue:
gh#276.

Every claim is a thing you can look at one of and say yes or no. Written from
the canvas markup before the app was opened, so the list is not a description of
what we already do.

Numbers are canvas pixels at 1320×880. `--x` names a token; a claim that says a
token means the field in `Theme`, never a literal at the call site.

## Looking at it

A review is assembled from the attempt's **checkout** — the diff, the tests
either side of it, the manifests, the symbols the claims anchor — so a board row
alone draws an empty card, and an empty card is the one state that cannot show
whether the composition is right. The fixture builds the checkout too:

```sh
scripts/review-demo.sh          # dark
scripts/review-demo.sh light
```

It seeds `crates/board/examples/seed_review.rs` (the canvas's own task: gh#138,
PR #212, three claims, two unclaimed changes), stands a headless board up on it,
creates the chat the attempt names, and opens the window with
`COMET_OPEN_REVIEW=1` — the knob that opens the first reviewable row's review on
the first board frame, because `r` on a row is the keypress a capture script
cannot rely on.

Captures: `screenshots/review-{dark,light}-{before,after}.png`, plus
`-after-scrolled` for the unclaimed block and the diff strip. The tab and its
exits (gh#311) are `screenshots/gh311-review-tab-{dark,light}.png` and
`gh311-review-closed-dark.png` — the same window one click of the tab's ✕ later.

## A. The route's frame

- **A1** The review is the CARD and the authoring session is the narrow column
  beside it — the inversion the route exists for.
- **A2** The session column is on the **right**, not the left.
- **A3** They are two cards, not one split pane: each is `--card` bed, 1px
  `--line`, radius 14, `--cardshadow`.
- **A4** The review card's gutters are 8 left, 8 bottom, 0 top, and **4** to the
  session column — the seam between two cards is half a gutter, as it is between
  the conversation and the changes pane.
- **A5** The session column is 380 wide including its 8px right gutter and 8px
  bottom gutter.
- **A6** The review card is three bands in a column: a header (flex none), the
  body (flex 1, scrolls), and the verdict bar (flex none). Nothing else scrolls.
- **A7** **A review is a tab.** The strip is drawn on this route, and the review
  leads it: 150×28 at x=172, radius 10, `--sel` under the `--sellift` ring, a
  5px dot in `--review`, `Review · <identifier>` in 12px `--text`, and the 18px
  `--subtle` close — `window.md` B3–B7 exactly, and B10. The space's chats
  follow it, drawn unselected.
- **A8** **You can leave.** Three ways out, none of them a shortcut you have to
  know first: the tab's ✕, clicking another tab, and `esc`. Each drops the card
  and lands on the chat route. (`mod-shift-b` also leaves, back to the board the
  review was opened from — that one is a shortcut, and it is not the only way.)

## B. Review header

- **B1** Padding 12/16/10, a 1px `--line` bottom border, two rows 6px apart.
- **B2** Row one, gap 9, items centred: the identifier in mono 11 `--subtle`,
  then the title, then the turn pill, then a 24px icon button in `--subtle`.
- **B3** The title is 14px/600, letter-spacing −0.01em, `--text`, flex-1 and
  truncating — it is the widest thing in the row.
- **B4** The turn pill is padding 1/8, radius 6, 11px, its own hue on a 14% tint
  of that hue. Never `--blocked`.
- **B5** Row two is the facts line: 12px `--subtle`, gap 10, wrapping, each fact
  led by a 12px icon and separated by a `·` in `--faint`.
- **B6** The pull-request fact wears the GitHub mark; the branch fact wears the
  branch icon and reads `<branch> → <base>`; the agent fact wears the Claude
  mark at 11px in `--claude` and reads `<model> · <elapsed>`.
- **B7** The header carries no uppercase section label. "REVIEW" as a heading is
  not on the canvas.

## C. Asked for

- **C1** A full-bleed row on the `--raised` bed, padding 11/16, 1px `--line`
  bottom border.
- **C2** It is label + body, gap 10, aligned to the top: the label "Asked for"
  at 11px/600 `--subtle` in a fixed **62px** column, nudged 2px down.
- **C3** The body is 13/20 `--muted`.
- **C4** The label is sentence case, not `THE BRIEF` in caps.

## D. Effects

- **D1** The same label-and-body row as C, at padding 11/16 with a 1px `--line`
  bottom border — but on the card's own bed, not `--raised`. The two rows are
  told apart by tone, and that is the only difference.
- **D2** The label is "Effects", 11px/600 `--subtle`, 62px column, nudged 3px.
- **D3** The chips wrap with a 6px row gap and an 8px column gap.
- **D4** A chip is 22 high, 8 of side padding, radius 6, gap 6: a 5px dot, then
  12px copy.
- **D5** A neutral chip is the `--chip` wash with `--muted` copy and a
  `--settled` dot. A chip that carries a status is a 12% tint of its own hue
  with its hue as the copy colour.

## E. Claims

- **E1** A heading row at padding 11/16/4, gap 10, items centred: **"What it
  says it did"** at 12px/600 `--text`, the count ("3 claims") at 12px
  `--subtle`, a spacer, and **"evidence gathered by the board, not the agent"**
  at 11px `--faint` on the right.
- **E2** The claims are a column of cards 8px apart, inset 16px from the card's
  sides, with no hairline above or below the group.
- **E3** A claim is its own bordered card: 1px `--line`, radius 10, padding
  10/12, gap 10, aligned to the top.
- **E4** Its glyph column is 14 wide, centred, 11px, nudged 2px down: `✓` in
  `--settled`, `·` in `--subtle`, `!` in `--blocked`.
- **E5** The claim's sentence is 13/19; `--text` when something stands behind
  it, `--muted` when nothing does.
- **E6** Under the sentence, 7px below it, the claim's chips wrap with a 5px row
  gap and a 7px column gap.
- **E7** A claim chip is 20 high, 7 of side padding, radius 6, 11px — one step
  smaller than an effects chip, and with no dot.
- **E8** An evidence chip ("4 new tests pass") is a 12% tint of its hue with the
  hue as its copy. An anchor chip (`shell/spaces.rs`) is the `--chip` wash, mono
  11, `--subtle`.

## F. Unclaimed

- **F1** The block is inset 16px with 14px of air above it, radius 10, clipped,
  with a 1px border in `--blocked` at 32%.
- **F2** Its header is padding 9/12, gap 8, on `--blocked` at 9%, with a 1px
  bottom border in `--blocked` at 22%.
- **F3** The header is a 14px warning glyph in `--blocked`, then **"N changes no
  claim accounts for"** at 12px/600 `--blocked`, a spacer, then **"this is where
  drift hides"** at 11px `--blocked` at 75% opacity.
- **F4** A row is padding 9/12, gap 10, 12px: the path in mono `--muted`,
  flex-1 and truncating; then what happened to it in `--subtle`; then a chip.
- **F5** Rows after the first carry a 1px `--line` top border. The block itself
  carries no fill behind them.
- **F6** The count is said once, in the header. There is no separate figure.
- **F7** No uppercase `UNCLAIMED` label above the block: the header is the
  label.

## G. Diff strip

- **G1** Inset 16px with 12px of air above it: padding 9/12, radius 10, 1px
  `--line`, gap 10, 12px.
- **G2** It reads `N files changed` in `--subtle`, then `+117` in mono
  `--settled` and `−33` in mono `--blocked`.
- **G3** `Read the diff` is a chip on the right: 22 high, 8 of side padding,
  radius 6, `--chip` bed, `--text` copy.

## H. Verdict bar

- **H1** The bar is the card's bottom band: a 1px `--line` top border, padding
  12/16, gap 10, on the `--raised` bed.
- **H2** The comment box is radius 10 with a 1px `--line2` border on the
  `--card` bed — darker than the bar it sits on, not lighter — padding 9/12,
  13/20 `--text`, at least 42 tall.
- **H3** Under it, one row, gap 10: the contract sentence on the left at 12/17
  `--subtle` with the branch named in `--muted`, then the verdicts on the right.
- **H4** A verdict is 28 tall, radius 6, 12px: `Comment` in `--muted`,
  `Approve` in `--settled`, both bare — no bed, no border.
- **H5** `Request changes` is the one filled control on the screen: padding
  0/12, 12px/500, `--text` bed with `--card` copy.

## I. Authoring session column

- **I1** Its header is 40 tall, padding 0/14, gap 8, with a 1px `--line` bottom
  border: a 5px status dot (`--faint` when the session is idle), the branch at
  13px/600 `--text`, a spacer, then `idle · 41m` at 12px `--subtle`.
- **I2** The transcript sits at the BOTTOM of the column when it underfills, and
  fades out at its top edge over 44px.
- **I3** The transcript column is padded 0/14 — no centred max-width column, no
  message rail, at this width.
- **I4** The delivery preview is the last thing in the column, above whatever
  input the column ends with: margin 0/12/12, radius 10, clipped, with a
  **dashed** 1px `--line2` border.
- **I5** Its header is padding 8/12 on `--raised` with a 1px `--line` bottom
  border: a 13px info glyph in `--subtle`, then "Will be delivered on submit" at
  12px/500 `--muted`.
- **I6** Its body is mono 11/17 `--subtle`, padding 10/12, capped at 196 tall
  and masked from 72% of that height to transparent.
- **I7** The `[unclaimed]` lines in the payload are the one thing in it the
  reviewer did not type, and they are `--blocked`.

## J. Both themes

- **J1** Every claim above holds in light with the light value of each token.
- **J2** In dark `--cardshadow` is `none`; the two cards are told apart from the
  shell by tone and hairline alone. In light they carry the shadow.

## Deliberate deviations

Each one is a claim above that this app does not satisfy, with the reason. Every
other difference from the list is a bug.

- **B6 says model and elapsed; the app says attempt and outcome.**
  `AttemptReview` carries neither a model nor a duration — the wire shape is
  `crates/board/src/claims.rs`, and the review is read from a board that may be
  on another device. The third fact is `attempt 1 · done`, which is what the
  board does know about the run. The icons, separators and the `--claude` mark
  on the agent fact are exactly as B5/B6 give them.

- **B2's icon button is a link out, and there are up to two.** The canvas draws
  one 24px globe. The app draws the same 24px control twice — the GitHub mark
  opens the pull request, the document glyph opens the issue — because the
  review is read on a device that may not hold the checkout, so the way out to
  the upstream is the header's job and dropping one of the two would lose a
  route. Two identical globes would say the two go to the same place.

- **The verdict strip stays, above the body.** The canvas has no line under the
  header saying what the review amounts to; the app has one, and it is the
  reading `comet-board review` prints too (`claims::Verdict`). Removing it here
  would leave the desktop surface saying less about an attempt than the terminal
  does about the same attempt, and would make the two disagree — which is the
  one thing `claims.rs` exists to prevent. It is drawn as one more band (C/D's
  shape, glyph column instead of a label) and it does **not** shout: no tint, no
  fill, the hue carried by the glyph and the copy. The screen's one loud block
  is F, and two blocks shouting the same number leaves it with nothing to shout
  with.

- **The evidence section stays, has no canvas row of its own, and sits with
  Effects.** What the run executed and how it exited is the second half of
  "prose alone marks its own homework" (gh#183); the canvas composes a review
  that has claims and effects and stops. It is drawn as one more label-and-body
  band in C/D's shape, with "Evidence" in the 62px label column — and it goes
  *above* the claims rather than below them, for the reason D exists at all: the
  numbers a reader meets first are what the story then has to agree with.

- **F4's per-row chip is absent.** The canvas ends each unclaimed row with a
  `Show` control. There is no per-file diff in this app to show — the way to the
  diff is G3, once, for the whole branch — and a chip that opened the same thing
  from six rows would be six copies of one affordance.

- **H4/H5 pick a verdict; they do not send it.** On the canvas the three
  controls are the submit. Here a verdict is armed and then submitted, because
  the preview (I4) promises "this is what will be sent" and a promise made about
  a payload nobody has looked at yet is not one. So the armed verdict wears
  H5's filled treatment and a `Submit` control sits at the end of the row.
  gh#276 is a fidelity pass and does not change the verdict flow.

- **The session column keeps its composer.** The canvas ends the column with the
  delivery preview. The most useful thing a reviewer can do about an unclaimed
  change is ask the agent that made it, and that affordance is not this pass's
  to remove — the preview sits directly above the composer instead.

- **I2/I3's transcript is the main window's transcript.** `window.md` §D governs
  bubbles, tool lines, prose, the gutters and where the content sits; at this
  width they keep their own measures rather than being restyled per column. The
  rail is off (`rail_visible` gates on the column's width, not the window's).
  What this pass takes from I is the column's card, its header, and the delivery
  preview at its foot.

- **The host line stays, under the diff strip.** One 11px `--faint` sentence
  naming the device the review was read from. The canvas has none because a
  canvas has one device; a review that swept three of them and does not say
  which answered is a number with no provenance.
