# Stats and routing learn the furniture rule — **done** (gh#434)

Reported live from the Mac frontend: Settings → Board stats "defaults to the
mac board" and shows numbers way out of date beside the box that actually
hosts the board. The board *pane* solved this in §gh#125: `host_candidates`
asks the local device first because asking locally is free, and
`board_dispatched` — any row with an attempt on record — is what a sweep
settles on, because a board somebody has released work from is the org's board
and one that only ever collected rows is furniture. Two pages never got that
lesson. The stats page swept every candidate (§gh#254) but defaulted to the
first answer in sweep order; the routing page stopped on the first
`ReadBoardConfig` that answered at all. On any Mac with a leftover local
`board.db`, both read the furniture — and routing would have *written into*
it.

### The evidence bit rides the replies

The pane asks its question of the rows it is already streaming. These pages
stream nothing, so the answer rides what they do read:

- **`BoardStats.dispatched`** — computed in `comet_board::stats::gather_with`
  over every task and every attempt, **never the window**: the box's board on
  a quiet week is not furniture, and a windowed count (`attempts`) would say
  it was. Counted the way `rows.rs` counts — all attempts, unfiltered — so
  the stats sweep and the board pane settle on the same host.
- **`dispatched` on the config reply** — `config_reply` answers it from the
  board service's own rows watch, through the same
  `comet_proto::view::board::board_dispatched` the pane calls.

Both are defaulted on the wire. A board that predates the field answers
without it and reads as furniture, which costs it only the tie against a
board that says otherwise.

### The sweeps hold instead of settling

Same hold-and-return shape as the pane's three sweeps (§gh#125):

- **Stats** (`resolve`): the pin still wins outright — an operator who picked
  a board gets that board, whatever the evidence says. The sweep's own
  default is now the first answer *with* dispatch evidence; a furniture
  answer is held while the sweep keeps asking and settles only once every
  candidate has been asked, so a lone device and a fresh install still see
  their own board. Held also means *not painted*: flashing the local board's
  numbers and then flipping to the box's is exactly the reported bug, so
  mid-sweep with only a furniture answer the page still says "Reading the
  board…" (keyed on `swept`; the never-read `loaded` flag is gone).
- **Routing** (`reload`): a config with evidence settles the sweep on
  arrival; a furniture config is held — first held answer wins the slot,
  which is the old tie-break — and shown only when nobody better answers.
  An unreadable answer no longer ends the sweep; it is carried as the error
  the page falls back to when nothing settles.

The "No board answered" half of the live report was gh#433 (the box's /tmp
quota), not this page — but first-answer-wins is what made the Mac's stale
board the default view once the box went quiet.
