# The board reports on itself — **done** (gh#143)

`comet-board stats` has answered "is delegating actually working" since the
port, on one text screen nobody opens: a shell verb is where you go when you
already suspect something. The numbers belong where the board is looked at.

- **Settings → Board stats.** A section beside Board routing, and for the same
  reason it is beside it: `board.db` lives on whichever device hosts the board,
  so the page sweeps `host_candidates` for the one that answers and a laptop
  reads the box's throughput without an ssh account on it. Headline tiles
  (dispatches, tasks, completion, median, live), then dispatches per day with
  the share that ended `done` filled in, where the work *landed*, how long runs
  take, friction, hour-of-day, and the tallies — space, runtime, tracker, whose
  subscription.
- **`BoardStats`, a call and not a stream.** Like §gh#132's `ReadBoardTask`: these
  are read when a page opens and stale by a poll interval at worst, and
  streaming a full aggregate on every board tick would cost every connected
  viewport a recompute nobody is looking at. Served on the board loop's own
  thread, which owns `board.db`.
- **One gatherer, one shape.** `comet_board::stats::gather` still produces it
  and the CLI still prints it; the *type* moved to
  `comet_proto::view::stats::BoardStats` so a viewport can deserialize the reply
  without linking a SQLite store — the `RuntimeOption` split. The renderer's
  arithmetic (ranking a tally, scaling a bar, phrasing a duration, folding a
  long tail into `n others`) lives there too, so the CLI, this page and whatever
  comes next cannot disagree about it.
- **What the record already knew.** Nothing new is stored. Landing comes from
  the task's `pr_merged`/`pr_open`/`pr_number` and is counted per *task* — three
  goes at one issue produce one pull request, and counting attempts would report
  the same merge three times. Friction is `reopened` + `blocked_count` +
  `overrun_warned_at`, which the board was already writing and nothing was
  reading. Whose subscription is gh#101's `billed_to`, with the dispatches that
  named no slot said out loud as the box's own login rather than hidden as
  unattributed.
- **Honest empties.** A completion rate is `None` until something has ended and
  renders as `—`: a `0%` on a board whose first agent is still running is a lie
  about the board. Day buckets are emitted for quiet days too — a gap that is
  simply absent reads as data the board failed to record.

Not on the TUI yet. The derivation is shared, so it is a rendering away — the
phone took it in §gh#155.
