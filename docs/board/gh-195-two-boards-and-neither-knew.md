# Two boards, and neither knew — **done** (gh#195)

Noticed while working on the box on 2026-08-09: this account has **two** live
boards. The Mac's — `~/.comet-native/board/state/board.db`, one route,
`bredebjorhovd/attn`, the board that ran the 17 overnight attempts on 08-08. And
the box's — same path on `comet@<box>`, 17 routes over 8 repos, 550
rows. Each polls GitHub on its own timer. Neither knows the other is there.

Nothing was broken, and nothing was keeping it that way. The two `[github]
repos` lists happen to be disjoint, which is the only reason this is quiet:
`repos` is per-board config, so the day one slug appears in both, both boards
derive the same issue as `ready` and `dispatchable`, and either can release it.
Two agents, two worktrees, two branches on one ticket — and **each board's row
looks perfectly normal**, because the other's attempt is invisible to it. The
first symptom would be two pull requests differing only by which board cut the
branch.

- **`doctor` sweeps for the other boards, and names what they poll.** A new
  `board hosts` check, fed by one relayed `ReadBoardConfig` per registered
  device — the same call the settings page reads a remote `routing.toml` with,
  so what comes back is what that board actually polls rather than what this box
  believes about it. The peers are named with their slugs, not counted: the fix
  for a collision is deleting a line from one of two files, and that needs the
  line.
- **A second board is reported; a shared source fails.** Two boards over
  disjoint repos is a legitimate setup — §gh#55's "still one host device" is
  about where the store lives, not about how many may exist — so the census
  alone is `ok`. What exits non-zero is a repo, or a Linear team, on both lists:
  that is the race itself rather than the shape that permits it. Slugs are
  compared case-folded, because GitHub reads `Tally` and `tally` as one repo and
  a check that did not would miss the collision on the day somebody retyped a
  slug from memory.
- **"Could not be asked" stays its own answer (§gh#155).** `Refused` and
  `UnknownMethod` are the device's own reply and rule it out for free; a
  transport failure, a relay 500 or a timeout mean nobody was asked, and the
  line says so rather than printing the reassuring "the only board on this
  account" over an incomplete sweep. That warning is gated on §gh#126's presence
  verdict for the same reason the picker's is: this fleet includes a phone, a
  phone is asleep essentially always, and a warning that fires every run stops
  being read before the day it matters.
- **The sweep is its own conversation, on its own clock.** Its calls leave the
  machine, and `doctor` is a report somebody is waiting on, so it gets an 8s
  budget across all devices and its own connection — `init` and `onboard` do not
  pay for it. A sweep that fails outright is `None`, which reads "not checked":
  the same rule every other engine-fed check follows.
- **Deliberately not done: refusing the overlap.** A board could decline to
  dispatch a row whose slug another host also polls, the way `dispatchable =
  false` already says "not mine to run". That puts a cross-device call in the
  dispatch path, where a peer that is merely slow becomes work that does not
  start — a worse failure than the one being prevented, and not worth taking
  before an operator has seen the warning and decided the topology is intended.
- **Deliberately not done: retiring the Mac's board.** If the box is to own the
  board, `attn` moves to the box's `routing.toml` and the Mac stops polling.
  That is an operator action on two live machines, not a code change, and this
  check is what makes it visible either way.
