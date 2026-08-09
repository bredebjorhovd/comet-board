# The phone catches up with 0.3.4 — **done** (gh#157)

Three things the other surfaces had and the phone did not: the stats screen
§gh#143/§gh#151 built the derivations for, and two fixes §gh#144 made everywhere else.

- **Board stats on the phone, not the dashboard on the phone.** The desktop
  page is a two-column grid built for 1160px, and it was redesigned *away* from
  a scrolling stack of cards because assembling one answer out of five tiles
  and a scroll is not an answer. A phone is that problem in its extreme form,
  so only the part that carries over carried over: the headline panel — the
  count, the facts that qualify it in one line, the per-space split — and then
  one column of evidence under it (dispatches per day, tokens with their
  coverage, at-a-glance, by runtime). The tile rows, the side-by-side panels
  and hour-of-day did not come.
- **`comet_proto::view::stats`, ported not shared.** No Rust runs on the
  device, so `StatsModels.swift` is the Swift half of it, rule for rule — the
  same discipline `SpaceRows.swift` and `BoardModels.swift` keep. The wire
  struct decodes *strictly*: a field whose name skewed arrives as an error the
  screen says out loud, not as a zero that reads like a real one.
- **One spec, two consumers.** "Ported rule for rule" is a property that decays
  silently, and the first symptom is the phone disagreeing with the desktop
  about a number somebody is deciding on. The ported surface had tests on the
  Rust half only, so the *cases* left Rust as data: `mod spec` in
  `crates/proto/src/view/stats.rs` writes every rule's inputs and expected
  outputs to `apps/ios/Comet/Spec/stats-spec.json` and fails when the
  checked-in file stops matching the Rust (regenerate with
  `UPDATE_STATS_SPEC=1 cargo test -p comet-proto stats`); the phone's
  `SpecRunner` asserts its own functions against the same file, 79 checks, run
  by `scripts/ios-stats-spec.sh`. Whichever side moves is the side that fails —
  verified by breaking a rule in each language in turn. **A prompt, not an
  enforcement**, and said out loud in the README, the script and the failing
  test's own message: the cargo half runs in CI and the simulator half cannot,
  so regenerating the fixture without running the script turns the build green
  while leaving the phone wrong — the one failure mode a fixture like this is
  supposed to make impossible. Closing it needs a macOS runner with a
  simulator, not a different test framework. The struct cases carry
  a real serialized `BoardStats`, so the decode is checked with them. A launch
  arg rather than XCTest because this project has one target and one shared
  scheme, and a test target means editing `project.pbxproj` and
  `Comet.xcscheme`; `-bench` and `-e2e` already set that precedent.
- **The sweep, and one thing the desktop page did not need.** `BoardStats` is
  a plain relay call to whichever device hosts the board — the settled host
  first, then the rest of `host_candidates`. On a phone the screen can be
  opened before the board sweep has answered anybody, and the relay's honest
  reply then is "the device is offline"; a one-shot read would leave that on
  screen until somebody pulled to refresh. The read is keyed on the host as
  well as the window, so the sweep settling *is* the retry.
- **`spaceTitles` glued the qualifier back on.** The phone still returned
  `"{base} · {tail}"` as one string — exactly what §gh#144 fixed on the desktop —
  and its rows elide from the right, so the disambiguating tail was the first
  thing cut and two checkouts of one repo read identically again. It now
  returns `SpaceTitle { base, qualifier }` and the row ranks the two halves
  with the device tag, letting `· 3 running` be the one thing that gives way.
  Demo mode grew a second checkout of one repo so the case is explorable.
- **The orchestrator's kill switch was unreachable.** §gh#144 gave the slot a menu
  on the desktop and the TUI after the operator could reopen the orchestrator
  and not unpin it; on the phone there was no menu and no
  `comet-board routes defaults orchestrator_chat --unset` to fall back on. The
  slot is often the only row a pinned chat has — its session ends, and its
  space shelf may never have listed it. Long-press now writes the `[defaults]`
  key through `WriteBoardConfig`, nothing optimistic: the slot disappearing is
  the box agreeing, and a refusal says why instead of leaving the row gone.
