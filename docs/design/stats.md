# Settings → Board stats — the canvas, as checkable claims

Source: `canvas/comet-stats-window.dc.html`. Tokens: `tokens.md`. Issue: gh#278.

Written from the canvas markup, not from the running app — the same shape as
`window.md`, and for the same reason: gh#258 judged this page "close" in prose,
and prose is what let it stay half-done through four pull requests.

Numbers are canvas pixels at 1320×880. `--x` names a token; a claim that names
a token means the field in `Theme`, never a literal at the call site.

## A. The page frame

- **A1** The page is the settings panel card: `--card` bed, 1px `--line`,
  radius 14, `--cardshadow`, margin 0/8/8/8 — it floats off the shell with a
  gutter on three sides and butts the titlebar on the fourth.
- **A2** Its interior is padded 26 top, 24 each side, 0 bottom, and the column
  inside is a flex column with a 12px gap between the header and every card.
- **A3** At 1320 the cards fill that interior — the column's max width does not
  bite, and no card is narrower than the panel it sits in.
- **A4** Five cards in four rows: spend, the day series, the crossing, then
  Breakdown beside Where the work landed.

## B. Header

- **B1** One row, `align-items:flex-end`, gap 16: the title block at `flex:1`
  and the window picker `flex:none` on the right.
- **B2** The title block is a column with a 3px gap.
- **B3** "Board stats" is 20px/600, letter-spacing −.01em, `--text`. **This page
  is the one settings page whose header is a headline rather than a section
  title** — the accounts canvas heads its page at 15.
- **B4** The subtitle is 13px `--subtle`: what the board on `<device>` did with
  the work it was given.
- **B5** The window picker is a track: gap 2, padding 2, radius 10, 1px `--line`
  border, `--chip` bed.
- **B6** A window segment is padding 4×10, radius 6, 12px, `--muted`.
- **B7** The selected window segment is 12px/500 `--text` on **`--card`** with
  `--lift` — the page's own ground, punched through the chip wash. It steps DOWN
  in dark and UP in light, because `--card` is `#070707` and `#ffffff`.
- **B8** The four windows are `24h`, `7 days`, `30 days`, `All time`, and
  `7 days` is where the page opens.

## C. Card 1 — spend

- **C1** `--raised` bed, 1px `--line`, radius 14, clipped, and **no title**: the
  figures are the page's headline.
- **C2** The band is one `items-stretch` row — a 238px cell, a 1px `--line`
  column, a second 238px cell, another 1px column, then the split at `flex:1`.
- **C3** A fixed cell is padded 18/20 and stacks with a 3px gap.
- **C4** The figure is 34px on a 38px line, 600, letter-spacing −.02em, `--text`.
- **C5** Under it, a 12px `--subtle` caption.
- **C6** Under that, at `margin-top:8`, a 13px `--muted` note.
- **C7** Cell one is the list price, "list price for this work", and the tokens
  over the window.
- **C8** Cell two is the multiple, "subsidised by your subscription", and the
  two amounts it was divided from.
- **C9** The split cell is padded 18/20 and stacks with a 10px gap under a 12px
  `--subtle` "Where the list price goes".
- **C10** Its bar is 8px tall, radius 2, four spans 2px apart, sized by share.
- **C11** The four class tones are `--text` at 62 / 44 / 28 / 16 %, biggest
  class first.
- **C12** The legend wraps at gap 6 down / 18 across. A legend item is a 7px
  swatch at radius 2 in the class tone, then 12px `--muted` "output $93", then
  the token count in `--faint`.
- **C13** The footer is a 1px `--line` top border, padding 9/20, gap 8: a 13px
  info circle in `--faint` and 12px `--subtle` copy, with "Settings → Agents"
  lifted one tone to `--muted`.

## D. Card 2 — Tokens and dispatches per day

- **D1** `--raised`, 1px `--line`, radius 14, padding 16 top / 20 sides / 14
  bottom, and the card's own stack gap is **14**.
- **D2** Head is a baseline row, gap 10: "Tokens and dispatches per day" 13/600
  `--text`, then 12px `--subtle` "bars are tokens · peak 11.4M".
- **D3** The band is 96px tall; columns are `flex:1`, 8px apart, grown from the
  baseline, radius 2.
- **D4** A column is `--text` at 40%, the peak day at 55%, and a day that spent
  nothing is a **2px rule at 12%** rather than a gap.
- **D5** Under the band, one caption column per day at the same 8px gap, each a
  2px-gap stack: the token figure at 12px over the day at 11px.
- **D6** The figure is `--muted`, `--text` on the peak day, `--faint` on a quiet
  one. The day line is `--faint`, and `--subtle` on the peak day.

## E. Card 3 — When you release work, and where

- **E1** Card as D1, with a 12px gap.
- **E2** Head is a baseline row, gap 10: "When you release work, and where"
  13/600, then 12px `--subtle` "dispatches by local hour · 7 days".
- **E3** Rows are 3px apart. A row is `items-center` gap 12: a 92px `flex:none`
  label at 12px `--muted`, truncated; the 24-hour grid at `flex:1`; then a 34px
  right-aligned total at 12px `--subtle`.
- **E4** The grid is 24 equal columns 2px apart; a cell is 20px tall, radius 2,
  `--text` at 4% when the hour is empty and 12%→68% by heat.
- **E5** Under the rows, an axis of the same three-part shape: a 92px spacer,
  `00` `06` `12` `18` each spanning six columns at 11px `--faint`, and a 34px
  spacer — so the labels line up with the grid, not with the card.
- **E6** The grid folds past five spaces; the canvas draws four.

## F. Card 4 — Breakdown

- **F1** Card as D1 with a 12px gap, at `flex:3` and `min-width:0`.
- **F2** Head is `items-center` gap 10: "Breakdown" 13/600 `--text`, a spacer,
  then the axis toggle hard right.
- **F3** That toggle is a track: gap 2, padding 2, radius **8**, `--chip` bed,
  and **no border** — one step smaller than the window picker in every way.
- **F4** An axis segment is padding 3×8, radius 6, 11px `--muted`; the selected
  one is 11px/500 `--text` on `--card` with `--lift`.
- **F5** The axes are Model, Runtime, Space, Tracker, Account, and Model is
  where the page opens.
- **F6** Rows are 9px apart. A row is `items-center` gap 12: a 132px label at
  12px `--muted`, truncated; a `flex:1` track 6px tall at radius 2 in `--text`
  7% carrying a fill at 45%; tokens right-aligned in a 52px column at 12px
  `--muted`; money right-aligned in a 44px column at 12px `--text`.

## G. Card 5 — Where the work landed

- **G1** Card as D1 with a 12px gap, at `flex:2` and `min-width:0`.
- **G2** Head is a baseline row, gap 10: "Where the work landed" 13/600, then
  12px `--subtle` "11 tasks".
- **G3** One 8px bar, radius 2, bands 2px apart: Merged `--settled`, PR open
  `--review`, Closed unmerged `--blocked`, No PR raised `--text` at 18%. **The
  only hues on the page**, and they are here because they name states.
- **G4** Legend rows are 7px apart: a 7px swatch at radius 2 in the band's tone,
  the label at `flex:1` 12px `--muted`, then the count 12px `--text`.
- **G5** Every category keeps its row at zero — a reader has to be able to tell
  a window that lost nothing from a surface that does not count losses.

## H. Both themes

- **H1** Every claim above holds in light with the light value of each token.
- **H2** In dark `--lift` and `--cardshadow` are `none`; in light they carry
  their shadows. That is the only structural difference between the variants.
- **H3** No mark on this page is a status hue except G3's four bands. A token
  volume, a heat cell, a meter fill and a legend chip are all `--text` at a
  fraction of itself.

## I. States the canvas does not draw

The canvas is one populated frame. These are the states the page still has to
have, and they are claims about *this* page rather than transcriptions.

- **I1 Loading.** Before any board answers: the header and its controls, then
  13px `--subtle` "Reading the board…". No card frames, no zeroes.
- **I2 No board.** After the sweep with no answer: the same, reading "No board
  answered."
- **I3 Empty window.** A board that answered with nothing: 13px `--subtle`
  prose naming the window, plus what the other boards hold when the sweep found
  any. No card frames.
- **I4 Empty card.** A card whose own question has no answer keeps its head and
  says so in one 12px `--subtle` sentence — never a row of em dashes over a
  chart of zeroes. The day band **collapses** rather than reserving 96px.
- **I5 Hover.** A segment in either track takes the hover wash; nothing else on
  the page is interactive, and nothing else takes one. Sampled in light: the
  track reads `(242,242,242)`, a hovered segment `(236,236,236)`, the chosen one
  `(255,255,255)` — three states, three tones, no overlap.
- **I6 Selected.** Selection on this page is only ever a segment, and it is
  B7/F4's surface change — never a hue.
- **I7 Host picker (gh#254).** When the sweep finds more than one board, a
  second track of B5's shape sits left of the window picker, one segment per
  board, and the subtitle stops naming the host as a fact. One board draws no
  picker.

## What this pass moved (gh#278)

Nine claims failed against the running app; all nine hold now. Captures either
side of them are in `screenshots/gh278-stats-{dark,light}-{before,after}.png`
(plus `-after-bottom` for the row below the fold and `-light-hover` for I5).

- **B3** — the headline was 15px (`page_header`). It is `dashboard_header` at
  `TEXT_HEADLINE`, and 20px is the only size on the page that is not on the UI
  ramp except the 34px figures it heads.
- **B4** — the subtitle was `--muted`. `page_subtitle` pays `--subtle` now, on
  all eight settings pages, which is what both canvases draw.
- **B5/F3** — both tracks bedded on a private 2% wash and now bed on
  `Theme::chip`. Sampled dark `(12,12,12)` → `(24,24,24)`; light `(250,250,250)`
  → `(242,242,242)`.
- **B7/F4** — the chosen segment painted `--selcard` and now paints `--card`
  plus `--lift`. Sampled `#191919` → `#070707` in dark and `#eeeef2` →
  `#ffffff` in light, which reverses its direction in both.
- **B5/F3** — track padding was `NEST_GUTTER` (4) and is 2.
- **F3** — the axis toggle wore the window picker's hairline and radius. The
  two are now `TrackSize::Page` and `TrackSize::Card`.
- **`--lift`** had no `Theme` field. `Theme::lift_shadow()` is it, asserted in
  the canvas-token test, and `tokens.md`'s row is corrected.
- **C13** — `Settings → Agents` is lifted to `--muted` inside the `--subtle`
  sentence, as two `TextRun`s over one shaped line so the sentence still wraps.
- **D1** — the day chart stacks at 14 where the other three cards stack at 12.

Nothing else in A–I moved, because nothing else was wrong: the spend band's
238px cells and 34px figures, the four class tones at 62/44/28/16, the day
band's 96px and its 2px quiet rule, the crossing's 92/34 gutters and its 4%→68%
heat, the breakdown's 132/52/44 columns and its 7%/45% track, the outcomes bar's
four hues and its zero-rows, and the 3:2 bottom row are all what the canvas
draws. `--card` and `--raised` sample exact in both themes.

## Deliberate deviations

- **B3's letter-spacing is not applied.** gpui's `Styled` has no tracking, so
  the headline is 20px/600 without the −.01em. Same for C4's −.02em. The size
  and weight are what carry the hierarchy; the tracking is a refinement the
  toolkit cannot express, and faking it by shaping per-glyph would cost the
  page its text selection.
- **F2 carries a caption the canvas does not draw.** Between the title and the
  spacer the page says what the rows are ordered by ("by cost", "by tokens"),
  because the bar is drawn against that quantity and it changes with the
  window. The canvas draws one populated state where the answer is always
  money.
- **C13 and the coverage sentence.** The canvas writes one sentence; the page
  appends the coverage ("From 26 of 33 attempts that reported usage") and, when
  a model has no rate, a second line. Both are in the canvas's own footer copy —
  it is the same claim, qualified. With two lines the footer's icon aligns to
  the top rather than the canvas's centre, which is where a leading icon belongs
  beside wrapped copy.

- **A2's `padding-bottom:0` is a 64px scroll tail.** The canvas is one frame
  that fits; the page scrolls, and a column that ends flush with the viewport
  reads as content cut off. The tail is below the last card, so nothing above it
  moves.

- **Card 5 carries two facts the canvas has no card for** (gh#252): the
  in-flight caption under the bar, and — under a hairline — what the work cost
  in time and in friction. The canvas deletes the cards these came from without
  saying where the numbers go; they are the only ones on the page about time
  rather than money, and they ride in the room this half of the row has spare
  beside a taller Breakdown.

- **The bar's slices are each rounded**, where the canvas rounds the track and
  squares the spans inside it. gpui has no `overflow:hidden` clip against a
  radius for a flex child, and four 2px corners at 8px tall is the same drawing
  either way.
