# Main window — the canvas, as checkable claims

Source: `canvas/comet-window.dc.html`. Tokens: `tokens.md`. Issue: gh#275.

Every claim is a thing you can look at one of and say yes or no. Written from
the canvas markup before the app was opened, so the list is not a description of
what we already do.

Numbers are canvas pixels at 1320×880. `--x` names a token; a claim that says a
token means the field in `Theme`, never a literal at the call site.

## A. Window

- **A1** 1320×880, corner radius 12, `--shell` ground, clipped to the radius.
- **A2** Base type is Geist 13px; mono is Geist Mono.
- **A3** A 1px `--line` divider runs the full height at x=256, separating the
  sidebar from everything right of it — full height, including behind the
  titlebar, not just beside the body.

## B. Titlebar (38px)

- **B1** Height 38, and the traffic-light cluster occupies the first 78px
  (12px dots, 8px apart, 4px in from the left).
- **B2** Two 24px icon buttons follow the lights: sidebar toggle, then back.
  A third, forward, sits after them. Back and forward are `--faint` when
  unavailable, `--subtle` when live.
- **B3** The tab strip starts at x=172 and lays out with 4px gaps.
- **B4** A tab is 150×28, radius 10, 12px type.
- **B5** The selected tab paints `--sel` with the `--sellift` ring and `--text`
  copy. Every other tab is bare — no fill, no ring — with `--subtle` copy.
- **B6** A tab leads with a 5px status dot in its status hue, and the dot is the
  only status colour on the tab.
- **B7** Only the selected tab carries a close button (18px, radius 6,
  `--subtle`).
- **B8** A 24px "+" sits after the last tab.
- **B9** A 24px panel-toggle icon sits at the far right of the titlebar.

## C. Sidebar (256px, 8px side padding)

### C1 Needs-you

- **C1.1** Header row: "Needs you" 11px/600 `--subtle` on the left, the count
  11px/500 `--subtle` on the right, padding 10/8/6.
- **C1.2** Rows are 3px apart, radius 10, padding 6×9.
- **C1.3** A row is two lines: a 9px-wide status glyph + title 13/18, then a
  subline indented 16px at 11/15 `--subtle`.
- **C1.4** The glyph is the status hue; a question is `--review`, a dead run is
  `--blocked`. The title is `--text` when selected, `--muted` otherwise.
- **C1.5** The selected row paints `--sel` + `--sellift`. No other row has a fill.

### C2 Spaces

- **C2.1** A 1px `--line` divider sits above the Spaces header, margin 12/8/0.
- **C2.2** Header: "Spaces" 11px/600 `--subtle`, and a 20px "+" button right.
- **C2.3** **Spaces is the primary group and Orchestrator lives inside the
  selected space** — not as a sibling group, and not displaced by Active.
- **C2.4** A space row is radius 10, padding 6×9, gap 8: a 15px source icon,
  the name 13/18/500, the branch 12px `--subtle`, a spacer, a running count
  ("3 running") led by a 5px dot, then a 16px chevron.
- **C2.5** The chevron points DOWN on the expanded space and RIGHT on collapsed
  ones.
- **C2.6** The expanded space is the selected row: `--sel` + `--sellift`.
- **C2.7** **The children hang off a rail**: the child list is inset
  `padding-left:14` at `margin-left:9` with a 1px `--line` left border. This is
  what makes them read as inside the space rather than under it.
- **C2.8** Orchestrator is the first child: a 5px `--settled` dot, a `--review`
  diamond, the name 13/18/500 `--text`, and an elapsed time 12px `--subtle`.
- **C2.9** A 1px `--line` divider separates Orchestrator from the chats below
  it, margin 3/9/4 — inside the rail, not across the sidebar.
- **C2.10** A chat row with an agent shows a subline indented 13px: the Claude
  mark at 10px in `--claude`, then the branch at 11/15.
- **C2.11** An agent row carries its issue id as a chip — `--chip` bed, mono
  11px, radius 6, padding 0×6 — then the branch 12px `--subtle`, then elapsed.
- **C2.12** A collapsed space is the same row shape with `--muted` name and a
  right-pointing chevron, and no children.
- **C2.13** A non-git space (`scratch`) uses the folder icon, not the source icon.

### C3 Account footer

- **C3.1** The footer is pinned to the bottom: a flex spacer above it, then a
  1px `--line` divider at margin 0/8.
- **C3.2** The row is padding 8×9, margin 8/0, gap 10: a 26px circle filled
  `--text` with the initial in `--card` at 12px/600, then the name 13/18/500
  `--text` over "Alpha" at 11/15 `--subtle`.

## D. Conversation panel

- **D1** It is a card: `--card` bed, 1px `--line`, radius 14, `--cardshadow`,
  margin 0/4/8/8 — so it floats off the shell with a gutter on every side.
- **D2** The transcript column is centred, max-width 736, with 48px side padding,
  and content sits at the BOTTOM (`justify-content:flex-end`) when it underfills.
- **D3** Your own message is a bubble: `--raised` + `--sellift`, radius 14,
  padding 10×16, 14/22, max-width 588, right-aligned, 24px above it.
- **D4** A tool line is an 18px `--chip` square (radius 6) plus 12px `--subtle`
  copy, 16px above it.
- **D5** Assistant prose is 14/22 `--text`, paragraphs 12px apart.
- **D6** Inline code is mono 12px on `--chip`, padding 2×5, radius 6.
- **D7** A 24px gap separates the transcript from the composer.

### D8 Composer

- **D8.1** The composer column is max-width 736, padding 0/10/12, gap 8.
- **D8.2** The input is 48px tall, radius 14, `--line2` border, `--raised` bed,
  `--lift` shadow, padding 0/10/0/16, gap 8.
- **D8.3** The placeholder is 14px `--faint`.
- **D8.4** The model chip is 26px tall, radius 6, padding 0×8: the Claude mark
  12px `--claude`, the model name `--text`, the effort `--subtle`, all 12px.
- **D8.5** A 26px attach button sits between the model chip and send.
- **D8.6** Send is a 28px circle filled `--text` with a `--card` arrow.
- **D8.7** Under the input, a 12px `--subtle` row padded 0×6: the checkout on
  the left with a folder icon, the branch on the right with a branch icon and a
  chevron.

### D deviations

Two, both from gh#297.

- **D6's 12px does not land; everything else in D6 does.** Inline code paints
  the mono face, `--text` copy, the `--chip` bed, padding 2×5 and radius 6 — but
  at the paragraph's 14px, not 12. A paragraph is shaped as ONE run list and
  gpui's `TextRun` carries a font but no size (`shape_text` takes a single
  `font_size` for the whole block), so a 12px span inside a 14px line would mean
  splitting every paragraph into separate elements and losing the wrap. The size
  was never what the gap was about: the span read as a status hue, and it no
  longer does.
- **D8.7 on a non-git space shows the left side only.** The canvas's space is a
  repo; a plain folder has no branch and no worktree to offer, so the row keeps
  the checkout label and drops the ref rather than naming a branch that does not
  exist.

## E. Board panel (520px)

- **E1** Width 520, padding 0/8/8/0, and the card is radius 14, `--card`,
  1px `--line`, `--cardshadow`.
- **E2** Header 40px, padding 0×14, gap 8: "Board" 13/600 `--text`, the device
  name 12px `--subtle`, a 5px `--settled` dot, spacer, a route chip, a 24px
  search icon.
- **E3** The route chip is 24px tall, radius 6, `--chip`: the word "route" in
  `--subtle` then the value in `--text`.
- **E4** A group header is 26px, padding 0×14, gap 8: a 12px glyph in the
  group's status hue, the label 12/600 `--text`, the count 12px `--subtle`.
  It has a `--line` border top AND bottom; the first group has bottom only.
- **E5** The glyphs are fixed per group: Blocked ▲, Working ●, Ready ▸,
  Review ✓, Failed ✕, Done ·.
- **E6** A task row is 32px, padding 0×14, gap 10, with fixed columns: glyph 10,
  id mono 11 at width 56, title 13px flexible, repo 12px right-aligned at width
  76, time 12px right-aligned at width 60, then the action chip.
- **E7** An unselected row's title is `--muted` and its id is `--subtle`.
- **E8** The selected row paints `--selcard` + `--sellift`, its title goes
  `--text` and its id `--muted`.
- **E9** The action chip is 22px, radius 6, `--chip`, `--text`, padding 0×9, and
  its verb follows the group: Open, Dispatch, Review, Retry.
- **E10** The selected working row shows a second action, Cancel, in `--blocked`
  with NO chip bed — a bare verb beside the chipped one.
- **E11** Inside Working, rows group by repo under a 24px subheader indented to
  x=24: the repo 12px `--subtle` then its count 12px `--faint`.
- **E12** "Done today" is quieter than the other groups by design: the label is
  `--muted` (not `--text`), the glyph `--faint`, and it carries a collapse
  chevron on the right.
- **E13** A done row has no action chip; its id is `--faint`, its title
  `--subtle`, and the agent that did it sits in the time column in `--faint`.
- **E14** Footer 28px, `--line` top border, padding 0×14, 12px `--subtle`:
  "↵ dispatch · space peek · / find".

## F. Both themes

- **F1** Every claim above holds in light with the light value of each token.
- **F2** In dark `--lift` and `--cardshadow` are `none` — the panels are told
  apart by tone and hairline alone. In light they carry their shadows, and that
  is the only structural difference between the variants.
