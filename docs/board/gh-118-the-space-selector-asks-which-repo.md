# The space selector asks "which repo?" — **done** (gh#118)

The space picker was machine-first: it offered *this device's* folders, and
reaching the box meant already knowing that hosts and spaces exist. Upstream that
is right — comet is a mesh of your own machines. For this fork it is backwards:
work lives in GitHub repos and the box is where they run, so the front door is a
repo list and everything else follows from picking one.

- **One list, repo-shaped.** The union of every space you have (named by its
  repo where the box could resolve one, box first) and every repo the board's
  App can see that has no space yet, marked "not connected yet". Search matches
  the repo name people actually say, not the owner they rarely do — `comet`
  finds `bredebjorhovd/comet-board`.
- **Picking a connected repo opens its space**, on whatever device it lives on.
  There is no host step because there is nothing to ask: a space knows its
  device. This is the part that makes the phone usable — it hosts no folders at
  all, so a folder browser could never have been its front door.
- **Picking an unconnected repo runs `OnboardRepo` inline** (gh#97's verb:
  clone on the box, `createSpace`, adopt), with its progress and its refusals in
  the picker rather than a bounce to Settings. On success the space row is echoed
  optimistically and you are standing in it, with its issues on the board.
- **The box is the default target**, and the sweep is what knows. The picker
  asks every device `ListRepoSpaces` and keeps *all* the answers rather than
  stopping at the first: a device hosting no board refuses before it does any git
  or GitHub work, so the sweep costs one cheap round trip per non-host and
  answers "how many boards are there?" for free. One → clone there silently.
  Two → the one question the picker asks.

**The new RPC exists because of a gap in the doc.** A `Space` knows its device
and its folder but not its *repo*, and the folder cannot supply one — `~/src/comet`
is a name, not an owner. Only the device holding the checkout can ask git. So
`ListRepoSpaces` (relay-forwardable, board-hosts-only) answers with both halves
the frontends cannot compute: this host's `space → owner/repo` links, and the
App's grant (`ListAppRepos`, reused). The grant is best-effort — a board on a
`GITHUB_TOKEN` has no installations to enumerate (gh#96), which is a supported
board, not a broken one — so it degrades to a note beside a list of spaces that
is otherwise complete.

- **`crates/proto/src/view/repos.rs`** is the merge, the order and the search,
  pure and tested, for the same reason the rest of `view` is: three frontends, and
  a picker that ordered its rows differently on each would be three products.
  `RepoRows.swift` ports it rule for rule (the Rust tests are the spec), as
  `BoardModels.swift` ports `agent_rows`.
- **`comet_board::onboard::space_links`** is the git half, injected-probe
  testable exactly as `adopt::detect` is, and deliberately *not* filtered by the
  routing config: "what repo is this space?" is true whether or not the board
  watches it. Whether it is adopted stays `adopt::missing_for`'s single answer.
- **Local folders stay reachable.** The folder browser is the second door, in
  the same card on the desktop (→ / the rail's "This device") and one tap down on
  the phone. A scratch directory that is nobody's repo is a real place to work,
  and the repo list cannot offer it.
- **Not done: the TUI.** It has no add-space surface at all today — spaces get
  made from the desktop, the phone, or `comet-board onboard` — so "the TUI's
  picker follows if cheap" was not cheap; it is a new screen, not a re-shaped one.
