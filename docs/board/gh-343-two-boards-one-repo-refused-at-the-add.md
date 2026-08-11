# Two boards on one repo, refused at the add — **done** (gh#343)

`comet-board doctor` on the Mac, 2026-08-11:

```
FAIL board hosts    bredebjorhovd/itsm-agent is on Tokenmaxxer9000 too — polled by this
                    board and by the other. Both see the same issue as ready, either can
                    dispatch it, and neither records the other's attempt: two agents, two
                    worktrees, two branches on one ticket.
```

The detection is §gh#195's and the sentence is right. What was missing is
anything *before* the fact: the repo went onto a second board with no objection
at the moment of adding, and the hazard only appeared the next time somebody ran
`doctor`. It happened twice in one day — once during an investigation, once
during live testing — which is the signal that it is easy to reach rather than
exotic.

- **The check moved to write time.** `onboard` and `routes add` both know the
  repo being added, and `doctor` already knew how to ask the other boards on the
  account what they poll. Nothing new had to be learned; it just had to be asked
  before the config is written rather than after. The comparison is
  `comet_board::doctor::already_polled`, beside `board_hosts_check`, and both
  sides read a peer's answer through the same `peer_board` — the check that
  refuses and the check that reports cannot disagree about what a board polls.
- **Refusing, not warning.** The failure is not visible in the moment: two
  agents on one ticket looks like one agent working, right up until two pull
  requests appear on the same issue, and by then both have burned a run. A
  refusal costs one flag; a warning costs a duplicate attempt.
- **`--force`, because sharing is sometimes the point.** Two boards polling one
  repo is a legitimate *choice* on a board where nobody dispatches. The refusal
  names both ways out — take the slug off the other board's `[github] repos`, or
  say out loud that this board is meant to share it — because the settings
  page's "Onboard a repo…" surfaces the same refusal with no flag to type, and
  the half it can act on is the first one.
- **Where it sits in each verb.** In `onboard`, between the GitHub resolution
  and the clone: a repo that does not exist still fails on the sentence about
  the repo, and a refusal leaves nothing on the disk behind it. In the `adopt`
  op, before the config is even read. Both are on the *board's host*, which is
  where the write happens and where the relay links are — a laptop driving the
  box needs nothing new.
- **Only a known collision refuses.** A peer that could not be asked, and a peer
  whose own `routing.toml` does not parse, are both "unknown" rather than
  "collides". Refusing on either would block every add on the account for as
  long as somebody's laptop is shut or somebody's config is broken — failure
  modes with no bound on how long they last, and worse than the one being
  prevented. The unasked are logged by the host and named by `doctor`, which is
  the surface that can afford to be uncertain out loud.
- **The sweep is concurrent, with a per-device budget.** 5s each, in parallel,
  rather than §gh#195's shared 8s taken in order: this fleet includes a phone,
  a phone is asleep essentially always, and a sequential sweep would let it eat
  the budget the box's answer needed. Somebody is waiting on an `onboard`, so
  the number is small on purpose.
- **No recursion to guard against.** The fan-out lives in the two *write*
  handlers and asks `ReadBoardConfig`, which fans out to nobody: a peer cannot
  answer this by asking us back. It holds no `watch` borrow across the await
  either — that would stall every workspace subscriber for the length of the
  sweep.
- **This is not §gh#195's "deliberately not done".** That entry declined to
  refuse the overlap *at dispatch*, because it puts a cross-device call in the
  dispatch path, where a peer that is merely slow becomes work that does not
  start. Adding a repo is not the dispatch path: it is a thing a person does
  once, already over the relay, already waiting on the box.
- **Deliberately not done: warning about the unasked at write time.** `onboard`
  could carry "could not ask Tokenmaxxer9000" back as a note beside its other
  ones, and `routes add` would need a new field on the write reply to say the
  same thing. `doctor` already says it, in the report built for exactly that
  kind of uncertainty, and a line printed on every add from a fleet with a
  sleeping phone is a line that stops being read.
- **Deliberately not done: the same gate on `routes edit`.** Hand-editing the
  file is the escape hatch that keeps the typed surface honest, and a repo added
  that way is still caught by `doctor`.
