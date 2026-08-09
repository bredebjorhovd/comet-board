# Readable at 124 rows — **done** (gh#125)

An operator report with screenshots: at 124 rows the panel was "messy and hard
to read", the host chip whispered "This device ●" while showing the Mac's own
stale test board instead of the box's, and an undispatchable `no route` row was
the top selected row of the whole panel. Five fixes, derived once in
`comet_proto::view::board` and drawn by all three viewports:

- **Groups within sections** (`grouped_sections`): rows group by route — the
  same field `f` and `Filter::Route` partition on — biggest group first, ties
  alphabetical, each group collapsible. One routed group draws no header
  (`group_headers_shown`); a flat three-row WORKING section stays flat.
- **`no route` is a trailing, folded group** (`group_starts_collapsed`):
  visibility-only rows get a headline and a count, never pole position. The
  default fold applies only on the unfiltered board — `f` to the `no route`
  position or `/` matching an unrouted title must show what it asked for, so
  under any active filter the group opens.
- **Honest hosts.** The host moved from a corner chip into the panel's title —
  "Board on Tokenmaxxer9000 · 124" — on desktop (clicking it opens the old
  pin menu); the TUI header gained the count beside its host note; iOS already
  titled itself this way. And the automatic sweep stopped settling on the first
  frame: a frame proves a board *exists*, not that it is the org's board.
  `board_dispatched` (any row with an attempt on record — not `chat_id`, which
  only rides the live attempt) is the evidence the sweep settles on; a board
  without it is held as a *fallback* while the remaining candidates are asked,
  and settled on only when nobody with evidence answers. A lone device and a
  fresh install still see their own board; a laptop's stale test board loses to
  the box. All three sweeps (desktop watch loop, TUI link loop, iOS
  `BoardStore`) carry the same hold-and-return shape.
- **The leading token is repo-qualified** (`TaskRow::display_identifier`):
  `tally #507`, the CLI's form humanized, because `gh#507` vs `gh#44` are
  different repos distinguishable only by a muted sub-line. `gh_repo` /
  `gh_repo_name` moved from `comet_board::model` into proto (re-exported) so
  the viewports and the board crate parse one id format once. Linear ids show
  unchanged. `/` matches the rendered form. The TUI's id column stretches to
  the widest token on screen (capped), and the Ready sub-line dropped the
  route it used to name — the group header and leading token now say it — 
  keeping only a workspace that differs from the route's name plus the
  `[enter to dispatch]` / `no route` affordances.
- **Two-line titles under the cursor** *(reverted by §gh#132 — gh#132: a row that
  grows under the pointer reflows the list below it, which is what the operator
  then reported as jank)*: the desktop's selected/hovered row
  wraps its title to two lines (`line_clamp(2)`, row height min not fixed);
  iOS already wrapped. The TUI stays one terminal row per line — a grid where
  one row is sometimes two makes the scroll arithmetic lie — but its title
  column got wider with the metadata cuts above. Desktop section headers also
  took on the weight of what they manage: bold, always-visible counts, a
  chevron instead of a 10 px text button.
