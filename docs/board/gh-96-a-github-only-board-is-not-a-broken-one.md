# A GitHub-only board is not a broken one — **done** (gh#96)

Inherited straight from herdr-board, where Linear always existed: `doctor`
reported `FAIL LINEAR_API_KEY missing — add it to …/.env` on a board that polls
GitHub only. Everything worked; the report said otherwise, which on a fresh box
is the first thing anyone sees.

The credential now reads the way `operator notice` does — three states, and only
the ones with a consequence fail:

- **Absent, and no route matches on `linear_team`** — `ok`, *not configured —
  the board polls GitHub only*. A supported configuration, worded so nobody
  reads it as half-installed.
- **Absent, but a route matches on `linear_team`** — `FAIL`, naming the teams.
  The same shape the GitHub credential already had (`ok` until repos are
  configured): without the key those tickets never arrive and the route can
  never fire, silently, which is what `doctor` is for.
- **Present** — probed against the API (`Linear::viewer`, injected into the
  check so it is testable off the wire). Accepted names who the board polls as;
  rejected fails with Linear's own reason. An API that could not be *reached* —
  a `reqwest` error, or a 429 — is `not checked`, never a rejection: failing a
  laptop on a train is the same false alarm from the other direction.

`linear review state` is not printed at all on a board with neither a key nor a
Linear route, on the rule the per-route `account` check already follows (gh#59):
a board is not told about a feature it is not using. `init` stops listing
`LINEAR_API_KEY` beside the GitHub credential as something to "add" and offers
it instead.

Empty reads as absent everywhere, which `config::credential` already did for
both the shell and the file — now with a test that says why: the box wizard
writes `LINEAR_API_KEY=` when the stage is skipped with Enter, and a skipped
stage has to look exactly like a board nobody configured. Same for the App pair,
where an empty `GITHUB_APP_ID` would otherwise read as "half configured".
