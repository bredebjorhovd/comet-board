# 32px single-line rows, right-aligned metadata, one visible verb — **done** (gh#176)

Step 5 of gh#171, and the only step in it that touches layout rather than
tokens. Two faults, one row.

- **A third of every ready row was reserved blank.** §gh#132 froze the row at
  47px — a 20px title line, a 15px metadata line, 12px of padding — because
  §gh#125 had let the hovered row grow and reflow everything under it. The fix
  was right and the price was the metadata line, held empty on every ready row
  by contract. The row is **32px and one line** now: the metadata moved onto
  the title's line as a right-hand column. The guarantee is untouched — every
  row is exactly `ROW_H` in every state, hover changes colour and nothing else
  — and the arithmetic that enforces it got shorter: `ROW_PAD_Y * 2 +
  ROW_LINE_H`, where the line is the action chip's height. Fifteen rows where
  ten fit, out of exactly the fifteen pixels that were being reserved.
- **The board's verbs were invisible until hover.** Dispatch, retry, cancel and
  open rendered only under the pointer or on the selection, and the footer
  compensated with a key legend that changes as you move — which works for the
  person who wrote it and for nobody arriving new. **One verb per row is now
  permanently visible**, and it is the one `enter` runs.

### The metadata is facts now, not a pseudo-table

`row_metadata` padded its fields into fixed cells (`fixed(runtime, 12)`,
`fixed(ws, 11)`) — a monospace grid, which is exactly right in a terminal and
nonsense in a proportional font, where a run of spaces is neither a column nor
invisible. The desktop had been pasting that grid into its second line and
trimming the trailing edge off it.

The facts are derived once and spaced twice: `state_metadata_fields` yields the
facts each state is worth saying, `row_metadata` pads them into the terminal's
grid (byte for byte what it produced before — the TUI is untouched), and
`row_metadata_line` joins them with the `·` the board joins facts with
everywhere else. The desktop reads the second one, right-aligned against the
row's far edge, capped at 150px and truncating. A row with nothing to say takes
no width; the rows that do all end in the same place.

### Which verb is the row's own

`row_actions` said which verbs a row *has* and nothing said which of them was
its own, so each surface picked for itself — the desktop's `enter` arm, the
TUI's key table, the phone's single chip. For a blocked row those disagree with
the action list itself: it leads with `Retry`, but `enter` opens the chat,
because a blocked agent is alive and waiting for you rather than needing
replacing (gh#49).

`primary_action` puts the designation beside the set it selects from, and its
answer is always a member of that set. `secondary_actions` is the remainder, so
nothing goes missing by being neither. On the desktop the primary chip draws
whether or not anything is hovering the row, hard against the metadata column,
and the hover verbs open to its left — the two things anchored to the right edge
are the two the eye had already found, so a hover moves nothing but where the
title truncates.

`enter` and the footer hint both read `primary_action` now instead of keeping
their own copies of the rule; `RowAction::verb` is the lower-case spelling a
sentence wants ("enter to open PR"), beside the existing `label`/`short_label`.
One consequence, and it is an addition rather than a change: `enter` on a review
row opens its PR, which is what the row's chip says it does. It previously did
nothing, and a permanently visible verb that the keyboard ignores would be worse
than no verb. Rows with nothing to be done to them — closed, unroutable, a
review with no PR raised — wear no chip: the verb is worth the space because it
is always true.

### Section headers lose the slab

Caps on a grey fill with the count in a second grey pill: three devices to say
one word, with the weight on the fill rather than the word. Sentence case at
12/600 in full-strength text, the count in the group headers' own words (`· 12`),
and a hairline between sections instead of a box around each. The published
spelling (`BoardState::label` — `BLOCKED`, `DONE TODAY`) is unchanged and still
what the TUI, the CLI and the phone say: capitals are a typographic choice, not
vocabulary, and this surface has stopped making it.
