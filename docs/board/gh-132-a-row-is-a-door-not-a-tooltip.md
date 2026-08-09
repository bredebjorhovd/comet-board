# A row is a door, not a tooltip — **done** (gh#132)

An operator report with a screenshot: "the animation feels a bit laggy or
jagged … that it shows more text but isn't openable either as a modal or
something doesn't sit right." Two faults with one root — §gh#125 answered "which
Signicat issue?" by making the row *bigger*, which is both the jank and a
promise the row could not keep.

- **Hover never changes layout again.** §gh#125's `line_clamp(2)` + `min_h` meant
  the row under the pointer grew and every row below it moved; the chips
  appearing on hover added a few pixels more. The desktop row is now a
  constant `ROW_H`, its two lines are constants too (`ROW_LINE_H` is the chip's
  height, so a row with chips is exactly as tall as one without), and the title
  is `truncate()` in every state. Of the issue's three options this is (c) —
  and (c) is not a consolation prize once (2) exists: with the full title one
  keypress away, a selected-row expansion would be the same sentence said
  twice, and arrowing down the list would reflow it on every step.
- **The row opens.** Desktop: a peek panel between the list and the footer —
  `space` toggles it, a click on a row opens it, escape shuts it before it
  shuts the board, and it follows the cursor once open. TUI: the help screen's
  full-screen shape, because a 24-row terminal has no beside; it owns the
  keyboard while up (`j`/`k` scroll the body) with **one deliberate exception**
  — `enter` still dispatches from inside it. iOS: a sheet, which is what a tap
  on a row now does. All three carry the whole title, the issue body as
  markdown, the labels, where the work sits, what has been tried on it, and the
  links.
- **Reading is never on the way to releasing.** `enter` still dispatches from
  the list on every surface, the phone row keeps its own Dispatch/Retry chip,
  and a release started from a detail surface goes through the same account
  picker — the detail must not become the one place on the board that skips the
  question of whose subscription a run spends (§gh#97).
- **The body is a call, not a field.** `ReadBoardTask {taskId}` → `{id, body}`,
  forwardable to the board's host like every other board verb, served off the
  loop thread that owns `board.db`. `WatchBoard` republishes every row on every
  sync cycle; a hundred issue bodies riding along would make each frame two
  orders of magnitude larger, relayed to a phone, to draw one truncated line.
- **The actions are one rule.** `row_actions` (a row's own affordances) and
  `detail_actions` (those plus the links a list has no room for) live in
  `comet_proto::view::board`, ported to `BoardModels.swift`. The desktop's
  per-state chip logic — hard-coded since §gh#73 — now reads from it, so the
  three surfaces cannot drift into offering a Retry one of them does not.
  `history_line` and `placement_line` join `row_metadata` as the shared
  formatting. `history_line` names `billed_to` *unconditionally*, unlike the
  row's own sub-line (`billing_note`, which speaks up only when somebody else
  is paying): the detail is where you go to ask, and an answer that appears
  only when there is a problem cannot be trusted to mean anything.
