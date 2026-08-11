# iOS — the canvas, as checkable claims

Source: `canvas/comet-ios.dc.html`. Tokens: `tokens.md`. Issue: gh#279.

Every claim is a thing you can look at one of and say yes or no. Written from
the canvas markup, screen by screen, in the order the markup draws them.

Numbers are canvas pixels at 393×852 (an iPhone at 1x). `--x` names a token; a
claim that says a token means the field in `Theme`, never a literal at the call
site — and on this side `Theme` spends `DesignCanvas`, which is the canvas's
own table transcribed into Swift.

**Three screens, two variants.** The canvas draws Home, Board and the review
sheet, in dark and then in light. The light half is not a second design: every
claim below is true of both variants, because a token knows its own two values.
Where the two genuinely differ, the claim says so.

## Why this file cannot be verified the way `window.md` is

The desktop surfaces are checked by sampling a `screencapture` PNG for a hex.
That is not available here, for three reasons that are all about the device
rather than about effort:

1. **`simctl` has no touch input.** Anything behind a tap, a long-press, a
   swipe or a scroll cannot be reached by the rig at all — `scripts/ios-theme-shots.sh`
   launches with a route and photographs what a COLD LAUNCH shows. Claims about
   a pressed row, an expanded group or an opened menu are marked **[manual]**
   and are checked by a human with the simulator in front of them.
2. **The captures are at 3x.** A 1px hairline in this file is 3 device pixels,
   and a 6pt dot is 18. Reading a claim off a capture means dividing.
3. **Light mode is forced per-launch, not per-device.** `Info.plist` still
   carries `UIUserInterfaceStyle = Dark`, which beats the simulator's own
   appearance setting; the rig passes `-theme light` instead. `xcrun simctl ui
   <sim> appearance light` would produce four dark screenshots and look like a
   bug in the theme. See `Comet/Theme/Appearance.swift`.

What IS mechanically checked is the palette: `crates/ui/tests/ios_theme.rs`
holds `DesignCanvas.swift` to `tokens.md` value for value in both variants, and
bans a colour literal outside `Theme/`. So a claim below that says "`--subtle`"
cannot be wrong about what `--subtle` IS; it can only be wrong about whether
this row spends it.

## A. The system

- **A1** Type is Geist; mono is Geist Mono. Four UI sizes and no others — 11
  (`textCaption`), 12 (`textDense`), 13 (`textBody`), 15 (`textTitle`) — plus
  14 (`textProse`) and one figure size (21).
- **A2** Three radii and no others: 6 (`radiusChip`), 10 (`radiusRow`), 14
  (`radiusCard`), each a `nestGutter` apart so `inner = outer − padding` is
  arithmetic. Where the canvas draws something off the scale it lands on the
  scale's own step — see **Deviations**.
- **A3** Full-round is a dot, a drawn cap, the send button, a verb chip or a
  count pill, and every survivor says which.
- **A4** Every screen's page is `--card`. There is no `--shell` on a phone:
  the canvas gives one page tone, and light would otherwise put a grey band
  under a white list for no reason a phone can see.
- **A5** A status colour is one of exactly four hues at one lightness and one
  chroma, and a state translates into that vocabulary exactly once
  (`Status.ofBoard` / `ofAgent` / `ofChat`).
- **A6** A tint of a status hue is one of the canvas's own five mixes: 12% (a
  chip's fill), 14% (a count pill, a badge), 9% (a warning card's header bed),
  22% (that header's divider), 32% (that card's border).

## B. Home

### B1 Header

- **B1.1** The bar is one row, 16px side padding, sitting under the safe area.
- **B1.2** A 19px board glyph leads, in `--muted`.
- **B1.3** A spacer, then an 18px "+" in `--muted`, then a 26px round avatar
  filled `--text` with its initial in `--card` at 12/600.

### B2 Needs you

- **B2.1** Header row: "Needs you" 12/500 `--subtle` on the left, and the
  count as a pill — `--review` at 14%, mono 11/500 `--review`, fully round.
- **B2.2** The header is omitted only when the count is zero, and then the
  section says so in words rather than leaving a gap.
- **B2.3** A row is radius `radiusCard`, padding 11×13, two lines: a 10px-wide
  status glyph + a 13/500 title, then a subline indented 18px at 11/16
  `--subtle`, ellipsised.
- **B2.4** The glyph is the status hue — a question is `--review`, a dead run
  is `--blocked` — and it is the only status colour on the row.
- **B2.5** [manual] The row under a finger paints `--sel` with the `--sellift`
  ring, at the row's own radius. No other row has a fill.

### B3 Orchestrator

- **B3.1** A 1px `--line` divider sits above the orchestrator row, margin
  10/4/0.
- **B3.2** The row is radius `radiusCard`, padding 7×9, gap 8: a 10px-wide
  `--review` ◆, the name 13/500 `--text`, the latest report at 11/16 beneath,
  then a right-hand badge.
- **B3.3** The badge is "new" — `--settled` at 14%, 11/500 `--settled`, fully
  round — while something is unread, and the time it last spoke otherwise.
- **B3.4** While a turn is running the ◆ is replaced by the spinner, so an
  8h-old report cannot be mistaken for one running now.

### B4 Spaces

- **B4.1** A 1px `--line` divider sits above the Spaces header, margin 10/4/0.
- **B4.2** Header: "Spaces" 12/500 `--subtle` left, the count 12 `--subtle`
  right.
- **B4.3** A space row is radius `radiusCard`, padding 11×13, gap 10: a 6px
  status dot, a 17px source icon, the name 15/500, a spacer, the running count
  12 `--subtle`, the device tag 12 `--subtle`, then a 12px `--faint` chevron.
- **B4.4** The leading dot carries the space's most urgent member's hue, and is
  `--faint` when nothing in it is live.
- **B4.5** The icon says whether this is a repo or a plain folder: the branch
  mark for a git space, the folder for one without.

  The canvas draws the GitHub mark here, and the phone does not, because the
  workspace doc carries `gitDetected` and nothing about where the remote
  points. Painting a vendor's mark off a boolean would be asserting a fact
  nobody sent us — the shape of thing this repo does not do with register
  data. When the doc carries a remote host, this claim becomes the canvas's.
- **B4.6** A space whose device is offline says so in the device tag, in
  `--blocked`, and its name drops to `--muted`.
- **B4.7** [manual] The space under a finger paints `--sel` with the
  `--sellift` ring.

### B5 Sessions

- **B5.1** Header: "Sessions" 12/500 `--subtle` left, the count 12 `--subtle`
  right.
- **B5.2** Rows are 2px apart, radius `radiusRow`, padding 7×9.
- **B5.3** A row is three lines. Line 1: a 6px status dot, "space · device" at
  12 `--subtle`, and the elapsed time 12 `--subtle` hard right. Line 2: the
  title at 14 `--text`, indented 14px. Line 3, indented 14px: the harness mark
  at 11px in `--claude`, the branch glyph at 11px, and the branch at 12.
- **B5.4** A session with no branch and no harness draws no third line at all,
  and its title drops to `--muted`.
- **B5.5** The dot is `--faint` when the session is idle — a chat nobody is
  waiting on is not a status.

## C. Board

### C1 Header

- **C1.1** A 18px back chevron in `--muted` leads.
- **C1.2** The centre is two stacked lines: "Board" 13/500 `--text`, and "on
  &lt;device&gt;" 11 `--subtle` under it.
- **C1.3** An 18px chart glyph in `--muted` closes the bar, and it is the way
  to the stats screen.

### C2 Section headers

- **C2.1** A section header is the state's glyph in the state's hue (mono 11),
  the label 12/**600** `--text`, then the count in mono 11 `--subtle`. Padding
  10/16/3.
- **C2.2** The label is `--text` and not `--subtle`: on the board the section
  IS the structure, where on Home a section header is a label above rows that
  can each be read alone.
- **C2.3** Sections run blocked, working, review, ready — what wants a human
  first.

### C3 Group headers

- **C3.1** A group header is a 10px `--faint` chevron, the route's name 12/500
  `--subtle`, then the count in mono 11 `--faint`. Padding 2/20/0.
- **C3.2** The chevron points DOWN on an open group and RIGHT on a folded one.
- **C3.3** A group header appears only when a section has more than one route
  to distinguish.

### C4 Rows

- **C4.1** A row is padding 6×20, three lines, 3px apart — and it is
  FULL-BLEED: no radius, no inset, because a board row is a line in a ledger
  and the section it sits under is the card.
- **C4.2** Line 1: a 10px-wide state glyph in the state's hue, the
  repo-qualified id in mono 11 `--muted`, a spacer, and the elapsed time in
  mono 11 `--subtle`.
- **C4.3** Past its cap the elapsed time turns `--working` and bolds to 600 —
  the clock is about to end that attempt, and the number is the reason.
- **C4.4** Line 2: the title at 14/19 `--text`, indented 18px. A row in `done`
  drops to `--muted`.
- **C4.5** Line 3, indented 18px: the harness and space at 12 `--subtle`, and
  the row's one verb chip hard right.
- **C4.6** The verb chip is a PILL — `--chip` bed, 12/500 `--text`, padding
  4×11. One per row at most: "Dispatch" on a ready row, "Retry" on a blocked
  or failed one, and nothing on the rest.
- **C4.7** [manual] The row under a finger paints `--selcard` — the card
  variant, because a board row sits inside its section — with the `--sellift`
  ring, full-bleed like the row itself.

## D. Review sheet

### D1 Chrome

- **D1.1** The sheet carries a grabber at the top. It is the system's own
  (`presentationDragIndicator`) rather than the drawn bar the canvas shows —
  a phone already has a vocabulary for "this drags away", and a hand-drawn
  copy of it would be one pixel off the one the user knows.
- **D1.2** The header is the title at `textTitle`/600 over the ids at 12
  `--subtle`, with a status pill hard right — the state's hue at 14%, radius
  `radiusChip`, 12px.
- **D1.3** A 1px `--line` rule closes the header.

### D2 Bands

- **D2.1** "Asked for" is a band on `--raised`: the label 12/600 `--subtle`,
  the ask at 14/20 `--muted`. It is the only band with a fill.
- **D2.2** "Effects" is a band on the page: the label 12/600 `--subtle`, then a
  wrapping row of chips.
- **D2.3** An effect chip is 24pt tall, radius `radiusChip`, padding 0×9, a 5px
  leading dot, 13px text. A neutral chip is `--chip` bed with `--muted` text; a
  chip that MEANS something is its hue at 12% with its hue as text.
- **D2.4** Each band is closed by a 1px `--line` rule.

### D3 Claims

- **D3.1** The heading is "What it says it did" 13/600 `--text` beside the
  count at 13 `--subtle`.
- **D3.2** A claim is a card: 1px `--line` border, radius `radiusCard`, padding
  10×12, gap 9.
- **D3.3** The mark is 12px in a 12px column — `--settled` ✓ for a supported
  claim, `--subtle` · for a bare one — and a bare claim's text drops to
  `--muted`.
- **D3.4** A claim's evidence is a 22pt chip, radius `radiusChip`, padding 0×8,
  12px: `--settled` at 12% when it passed, `--working` at 12% when nothing
  covers it.

### D4 Unaccounted changes

- **D4.1** The card is bordered `--blocked` at 32%, radius `radiusCard`,
  clipped to it.
- **D4.2** Its header sits on `--blocked` at 9% under a 1px `--blocked` at 22%
  divider: a 14px warning glyph and the count at 13/600, both `--blocked`.
- **D4.3** Each change is one row, padding 9×12, 13px: the path in mono
  `--muted` ellipsised from the right, the fact in `--subtle` hard right, and a
  1px `--line` rule between rows.
- **D4.4** The card is absent when every change is accounted for. It is never
  drawn empty.

### D5 Verdict bar

- **D5.1** The bar is pinned to the bottom on `--raised`, over a 1px `--line`
  top rule, padding 11×16.
- **D5.2** The note field is `--card` with a 1px `--line2` border, radius
  `radiusRow`, padding 9×12, `--text`. It is an input, and `radiusRow` is the
  step the scale gives inputs.
- **D5.3** Under it: the delivery line at 12 `--subtle`, then "Approve" as bare
  `--settled` text, then "Request changes" filled `--text` with `--card` copy —
  both 32pt tall, radius `radiusRow`.
- **D5.3a** "Request changes" only wears that fill once it can be SENT. With
  the note empty it is `--chip` with `--faint` copy, because GitHub refuses an
  empty `REQUEST_CHANGES` and a button that looks ready and then fails is
  worse than one that says it is not. The canvas draws the ready state; the
  committed capture shows the other one.
- **D5.4** The destructive verb is the FILLED one. The canvas puts the weight
  on sending it back, because approving is the default outcome and does not
  need the affordance.

## Selection is a press

The canvas draws one row lit on each of its three screens, and calls it
selection. **A phone has no resting selection.** The desktop keeps a cursor row
because a pointer can hover somewhere it has not committed to; here a tap IS
the commit and the screen changes underneath it, so there is nothing for a
selected row to persist as.

So the phone draws the canvas's lit row at the moment it is being chosen: the
press paints `--sel` (or `--selcard` inside a card) under the `--sellift` ring,
which is `SelectRowButtonStyle`. `--hover` is still spent, on the rows the
canvas never lights — sheet rows, chips, pickers — where the wash is the whole
of the feedback.

The consequence for this file is the **[manual]** marks: `simctl` cannot hold a
finger down, so every one of these claims is checked by a human with the
simulator in front of them, and none of them appears in the committed captures.

## Deviations

Three, each a case where the canvas and the locked scale disagree and the scale
wins. They are listed rather than silently applied, and the reason in every
case is that a number the canvas drew once is not worth a fourth step in a
system three surfaces share.

- **The off-scale numbers.** The canvas draws a 12px radius (Home session
  rows, review claim cards) and a 16px review title. Each lands on the
  scale's own step instead — `radiusRow` / `radiusCard`, and `textTitle` at
  600. The radii matter most: the nesting rule (`inner = outer − 4`) is what
  makes concentric curves possible, and a 12 between the two steps breaks it
  wherever it nests. Each of these is a number the canvas drew once; none is
  worth a fourth step in a system three surfaces share.
- **The orchestrator preview's `opacity:.72`** becomes `--text` when unread and
  `--subtle` when seen. A text tone multiplied by an alpha is precisely what
  gh#172 retired, and `ios_theme.rs` fails the build on a new one; the canvas's
  .72 grey is not one of the four and is not contrast-checked.
- **The 14px row title** (Home sessions, board rows) is `textProse`, which the
  desktop reserves for rendered markdown. On a phone the list IS the reading
  surface — a 393pt row holds one title and nothing beside it — so the phone
  spends prose size on the one line of each row a thumb is aiming at. Nothing
  else moves: the metadata around it stays on the UI ramp.

Anything else that differs from the claims above is a bug, not a deviation.
Add to this list only with the reason.
