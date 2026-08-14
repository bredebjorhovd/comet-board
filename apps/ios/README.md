# Comet for iOS

A native SwiftUI viewport onto the comet-native mesh. The phone is a **peer
device**: it joins the same Loro CRDT rooms as every other device (workspace
doc + per-chat session docs over the edge's Durable Objects), renders the
mirrors, and drives remote engines through the durable command queue. No
engine runs on the phone.

## Build & run

Requires Xcode 26+ (iOS 26 SDK — Liquid Glass APIs).

```sh
cd apps/ios
xcodebuild -project Comet.xcodeproj -scheme Comet \
  -destination 'platform=iOS Simulator,name=iPhone 17 Pro' build
```

Or open `Comet.xcodeproj` in Xcode and run. That is the **simulator**: it needs
no signing and works from a clean checkout. A **device** does not — it needs one
local file that is not in the repo, and the section below is how to write it.

Dependencies (SPM, resolved
automatically): [loro-swift 1.13.x](https://github.com/loro-dev/loro-swift)
(matches the engine's loro 1.13), [swift-markdown](https://github.com/swiftlang/swift-markdown)
(cmark-gfm: tables/strikethrough/tasklists — the same feature set as the
desktop's pulldown-cmark config).

### Building for a device (gh#196)

**A clean checkout cannot build for a device until you write one local file.**
This is deliberate — the repo is public and an Apple team id is personal — but
the failure is not self-explaining, so here is the whole of it.

```sh
cd apps/ios
cp Signing.local.xcconfig.example Signing.local.xcconfig
$EDITOR Signing.local.xcconfig     # your DEVELOPMENT_TEAM and a bundle id
xcodebuild -project Comet.xcodeproj -scheme Comet \
  -destination 'generic/platform=iOS' build
```

`Signing.local.xcconfig` is gitignored. `Signing.xcconfig` — committed, and the
target's base configuration — pulls it in with `#include?`, an *optional*
include, so a checkout without it still builds for the simulator with no
warning. Once the file exists, Xcode and `xcodebuild` both pick it up with no
extra arguments; there is nothing to remember on the command line.

Set **both** keys. Two different errors are waiting:

| What you left out | What Xcode says |
| --- | --- |
| No file, or no `DEVELOPMENT_TEAM` | `Signing for "Comet" requires a development team.` |
| Team set, bundle id left at the default | `No profiles for 'dev.cometnative.Comet' were found` |

The second one is the trap, because it does not sound like a bundle-id problem.
`dev.cometnative.Comet` is the project's default and **no personal team can sign
it** — a free team issues profiles on demand only for ids nobody else has
claimed. Pick your own (`dev.comet.native.<yourname>`) and Xcode mints the
profile the first time you build.

One consequence worth knowing: the override applies to **every** destination,
so once you set it your simulator builds carry your bundle id too. Anything
driving the app through `simctl` — `get_app_container`, `launch`, `terminate` —
must therefore read the id rather than assume it:

```sh
plutil -extract CFBundleIdentifier raw "$APP/Info.plist"
```

`scripts/ios-stats-spec.sh` does exactly that. Copy it, do not hardcode.

An ignored file rather than settings in `Comet.xcodeproj/project.pbxproj`: put a
team id in the tracked `.pbxproj` and it is a permanent dirty diff, one that has
already been swept into an unrelated `git stash` and nearly lost. It cannot
happen to a file git never looks at.

### Connecting

- **WorkOS**: enter the edge URL, open the sign-in page on any device, paste
  the code it shows (`/auth/exchange`), pick an org (`/auth/refresh` re-scopes
  the token with the `org_id` claim).
- **Dev**: against an `AUTH_MODE=dev` edge (e.g. `wrangler dev`), enter a user
  id + org id; the bearer is `userId@orgId`.
- **Demo mode**: fully offline dataset with a scripted streaming reply and a
  board (rows in every state, two live attempts wired to real demo chats) —
  explore the UI with no infrastructure. Launch args for screenshot rigs:
  `-demo [-route chat:<id>|space:<id>|board|stats] [-sheet dispatch|review|review:<taskId>] [-stream]`.
  `-route`/`-sheet` work against a live edge too, which is where the rows that
  matter are. `-sheet review` lands on the first row parked in `review`;
  `-sheet review:<taskId>` names one, which is how the states that are not the
  happy one get photographed. `-theme light|dark|system` (or `COMET_THEME`)
  picks the variant for a rig without touching the stored preference — see
  below.

### Keeping the ported rules honest (gh#157)

Several files here are second implementations of rules that live in Rust —
`StatsModels.swift`, `SpaceRows.swift`, `BoardModels.swift`, `ReviewModels.swift`
— because no Rust runs on this device. Two implementations of one rule is how a
phone comes to disagree with a laptop about a number somebody is deciding on, so
for those rules the *cases* live outside both languages:

```sh
cargo test -p comet-proto stats               # the Rust half + the fixture guard
scripts/ios-stats-spec.sh                     # the Swift half, in the simulator

cargo test -p comet-board --test ios_review_spec  # the same pair for the review
scripts/ios-review-spec.sh                    # reading (gh#256)
```

`crates/proto/src/view/stats.rs` (mod `spec`) generates
`Comet/Spec/stats-spec.json` — every rule's inputs and expected outputs, plus
real serialized `BoardStats` values so the decode is checked too — and fails
when the checked-in file stops matching the Rust. `SpecRunner` (launch arg
`-spec`) asserts the Swift functions against the same file. Whichever side
moves is the side that fails. After changing a rule in Rust:

```sh
UPDATE_STATS_SPEC=1 cargo test -p comet-proto stats && scripts/ios-stats-spec.sh
```

**Run the second command. The fixture is a prompt, not an enforcement.** Only
the Rust half runs in CI — the Swift half needs a simulator, and no CI here has
one. So the failure mode is quiet and it looks like success: change a rule in
Rust, the guard goes red, you regenerate, CI goes green, and the phone is now
wrong about that rule until somebody runs the script. Regenerating the fixture
is not the end of the job; it is the *notice* that the other half of the job
exists. Treat a `UPDATE_STATS_SPEC=1` run without an `ios-stats-spec.sh` run in
the same change as an unfinished change.

The review fixture (gh#256) is the same shape with different nouns:
`crates/board/tests/ios_review_spec.rs` generates `Comet/Spec/review-spec.json`
from `comet_board::claims` and `comet_board::effects` — eight whole reviews with
every verdict, finding, chip and claim mark they produce — and
`ReviewSpecRunner` (launch arg `-review-spec`) asserts `ReviewModels.swift`
against it. Regenerate with `UPDATE_REVIEW_SPEC=1`, and the paragraph above
applies word for word.

A launch-arg runner rather than XCTest: this project has one target and one
shared scheme, and a test target means editing `project.pbxproj` and
`Comet.xcscheme`. `-bench` and `-e2e` already work this way. (A test target
would not close the gap either — what is missing is a macOS runner with a
simulator, not a test framework.)

### Keeping the redial schedule honest (gh#405)

`Sync/RoomClient.swift` is a port of `crates/sync/src/room.rs`, and its own
header says "Constants mirrored from room.rs" — mirrored by hand, with nothing
checking the mirror. gh#396 changed two rules there (a session must outlive
`HEALTHY_SESSION` before its end resets the backoff ladder; every redial wait is
jittered) and the phone kept the old ones for a release: a room whose Durable
Object answered the join and then died redialed four times a second, with no
ceiling, on a battery, against an edge that was already failing.

The decision now lives in `Sync/ReconnectBackoff.swift` — a value type with no
socket in it, which is what makes it checkable at all — and two things hold it:

```sh
cargo test -p comet-sync --test ios_room   # the constants + the shape, read out
                                           # of both sources as text — in CI
scripts/ios-sync-spec.sh                   # the schedule they produce, in the
                                           # simulator (`-sync-spec`)
```

Unlike the fixtures above, the first half needs no simulator, so this class of
drift fails on the same runner that builds the desktop. It is not a blanket
parity claim: the phone deliberately pings at half the desktop's rate and
tolerates a longer silence, because a radio is a battery. It covers the three
constants of the redial ladder and the shape of the decision.

The second half is deliberately not an end-to-end check. A reconnect loop is
exactly the thing that cannot be verified against an edge that is failing every
request, which is the state the edge is in whenever the Durable Objects
free-tier duration cap is tripped — the outage the schedule exists for.

### Keeping the design system honest (gh#181)

`Theme/Theme.swift` is the other second implementation — the design system this
repo owns lives in `crates/ui/src/theme.rs` and `comet_proto::view::status`, and
the phone has to restate it because no Rust runs there. Restating it
*differently* is a real bug and it happened: `boardStateColor` said amber and
`ChatIndicator.dotColor` said pink about the same running agent, one screen
apart, long after the desktop had settled that argument.

```sh
cargo test -p comet-ui --test ios_theme
```

Eight checks, and unlike the stats fixture above **these run in CI** — they read
Swift as text, so they need no simulator:

- **Parity, in both variants.** Shared dark paint plus the radii/type scale stay
  equal to `Theme::dark()`; the four surfaces gh#258 retuned only for desktop
  remain pinned to the phone's supplied dark design. Light paint stays equal
  to the supplied iOS reference's exact neutral, status and Claude variables,
  deliberately not the desktop `Theme::light()` palette.
- **Every paint token declares both variants** (gh#257) — hatch
  `one-tone-ok:`, which one brand mark uses. A token with a single value is
  where a scattered `colorScheme ==` check at a call site starts.
- **No Swift view pins a colour scheme** (gh#257). Four call sites used to.
- **No text tone multiplied by an alpha** (gh#172) — hatch `theme-opacity-ok:`,
  and it is for animation fades only.
- **No literal radius or font size outside `Theme.swift`** (gh#174) — hatch
  `scale-ok:`, for marks that are *drawn* rather than boxed.
- **`Capsule()` / `Circle()` is a dot, a drawn cap or the send button** —
  hatch `round-ok:`. One round thing on screen, and it is the one you press.
- **The ring-fenced bundle override remains explicit**: the contract names the
  still-shipped `UIUserInterfaceStyle = Dark` key and its handling.
- **The window activation observer has a bounded lifetime**: weak capture plus
  deinit cleanup prevents dismissed sheets accumulating callbacks.

A number that genuinely does not fit becomes a token on *both* ends, never an
alpha or a literal on this one.

### Light mode, and the ring-fenced integration dependency (gh#257)

The theme implementation can follow the system once the remaining bundle
override is removed; in the currently shipped app, the account menu offers the
explicit Light and Dark choices. What a text scan cannot check is what the paint
looks like, so:

```sh
scripts/ios-theme-shots.sh docs/screenshots "iPhone 15 Pro"
             # Home + Board + the review sheet, both variants, 393x852
```

The claims those six are read against are **`docs/design/ios.md`** — the
canvas's screens as numbered yes-or-no statements, the same shape as
`docs/design/window.md` for the desktop. It also says which claims `simctl`
cannot reach (anything behind a touch) and are checked by hand instead.

The palette itself is not in that file. `Theme/DesignCanvas.swift` transcribes
`docs/design/tokens.md` under the canvas's own variable names, `Theme.swift`
says which variable answers which job, and
`crates/ui/tests/ios_theme.rs` reads the doc against the Swift so a drifting
value fails in `cargo test` rather than in a screenshot (gh#279).

**`Comet/Info.plist` still carries `UIUserInterfaceStyle = Dark`.** That key
forces every window in the app and beats the device setting, so while it is
there "System" resolves to dark and only the two explicit choices do anything.
gh#257 ring-fenced the file, so `Theme/Appearance.swift` reads the key rather
than removing it and stays honest about it in two ways: `Appearance.system`
resolves to the forced style, and the picker does not offer System at all.
Delete those two lines from `Info.plist` and both behaviours correct themselves
with no code change — that is the whole of the remaining work.

The same key is why the shot script passes `-theme light` rather than
`xcrun simctl ui <sim> appearance light`: the explicit choice installs a
window-level override after attachment, while a device setting cannot beat the
bundle key.

### Verifying the board against a real box

`-e2e-board <repoPath>` drives gh#114's exit criteria headlessly, against a dev
edge (`wrangler dev --var AUTH_MODE:dev` on :8787) and a `comet headless` whose
`routing.toml` routes to that repo: it makes the route's space, waits for the
board to call a row dispatchable, releases it, watches the row go `working` with
its branch cut, and then retries it with `replace`. Results land in
`Documents/e2e.log` (read it with `simctl get_app_container`). The plain `-e2e`
smoke also probes `WatchBoard` on every engine device, where a board-less device
*refusing* is a pass — what that asserts is that a stream frame comes back at
all.

### Distribution

Personal-device sideload via Xcode free provisioning works and re-signs every
7 days. That is the free personal team's whole profile lifetime — the app on the
phone stops launching after a week and has to be rebuilt and reinstalled, which
is a recurring chore, not a one-off. The $99/yr Apple Developer Program issues
twelve-month profiles and ends it.

TestFlight needs that same paid account — and so does gh#100's signing tier, so
it is one purchase for three things. Buying it is a spending decision, not a
code one; until it is made, expect the weekly reinstall.

## Architecture

```
Board/
  BoardModels.swift     view/board.rs port: BoardState + section order/glyphs,
                        the `list --json` TaskRow (snake_case wire), the
                        done-today bound, elapsed/cap spellings, the gh#101
                        billing vocabulary, agent_rows/running_rows/active_rows
                        whole
  BoardStore.swift      standing `WatchBoard` over the device-room relay: the
                        host sweep (each candidate's own room — no engine here
                        to forward with `targetDeviceId`), dispatch/retry/cancel
  BoardView.swift       sections in board order, blocked first; per-state row
                        content, elapsed against the route's cap
  DispatchSheet.swift   runtime + account pickers with billing chips (gh#74/#101)
                        and the `require-own` confirm
  StatsModels.swift     view/stats.rs port (gh#143/gh#151): the `BoardStats`
                        wire shape (decoded strictly — a skewed field is an
                        error, never a zero) plus the renderer's arithmetic —
                        ranking a tally, scaling a bar, phrasing a duration,
                        folding a tail into `n others`, and the honest empties.
                        Held to the Rust by a shared fixture — see below
  ActiveSection.swift   the one live group (gh#103 + gh#117, merged by gh#123),
                        phone-shaped: attempts with identifier chips, unmanaged
                        runs by bare title, needs-you first
Models/
  RepoRows.swift        view/repos.rs port (gh#118): the repo-first picker's
                        union of spaces + the board App's grant, its box-first
                        order, and the name-before-owner search rank
  SpaceRows.swift       view/spaces.rs port (gh#138): names made unique within a
                        device's spaces — as `SpaceTitle { base, qualifier }`,
                        two fields so a row that elides from the right cannot
                        cut the half that tells it from its twin (gh#144) —
                        and the split that gives a chat one full row: Active's
                        while it runs, its own when idle
Sync/
  LoroProtocol.swift    loro-protocol 0.3 wire codec (byte-compatible port of
                        the crate's encoding.rs: magic/varBytes/type/payload)
  RoomClient.swift      room.rs port: join with oplog VV, snapshot backfill,
                        resubmit-from-server-VV, DocUpdate+Ack, fragments,
                        %EPH presence sub-room, ping/pong lease, backoff
  ReconnectBackoff.swift  the redial schedule alone (gh#405), as a value type
                        with no socket in it: the ladder, the session lifetime
                        that earns a reset, and the jitter on every wait —
                        room.rs + jitter.rs, and the only part of the port a
                        test can reach without an edge
  WorkspaceStore.swift  ws4/{org}/{user} mirror: devices/spaces/chats/sessions
                        rows, presence heartbeats, viewer-side writes
                        (createChat, archive, lastSeenAt, own device row).
                        The room generation is a label only — the on-disk
                        snapshot is keyed `workspace2/{org}/{user}` and must
                        NOT follow it (gh#148): abandoning a room's edge
                        storage is survivable precisely because the local
                        copy outlives it and re-seeds the new one
  SessionStore.swift    session doc mirror: entries/parts (continuations
                        joined), command ledger appends (rule 1), host nudge
Markdown/
  MarkdownModel.swift   block model + incremental tail re-parser (re-parse
                        from the 2nd-to-last top-level block; link-defs force
                        full parses) — parser.rs port
  Highlight.swift       line tokenizer with carry state, paint-only
  MarkdownBlockView.swift  desktop metrics: body 14/22, headings 19/27…14/22,
                        code 12.5/18 (analytic line rows), violet inline code,
                        accent blockquotes, hairline tables
Transcript/
  TranscriptRows.swift  rows_for_entry port: block-granularity rows, stable
                        ids ({msg}#{part}.{block}, {msg}#g{n}), fingerprint
                        versions, consecutive-tool grouping
  TranscriptView.swift  lazy stack + stick-to-bottom (pin breaks only on user
                        scroll, 70pt re-engage band, 320pt jump button),
                        tool-group folds, error/input chips
  Veil.swift            paint-only streaming fade (EMA-tracked duration,
                        1−(1−p)^1.6 curve)
Composer/               glass card, Send→Steer→Stop morph, QuestionPanel
                        (paged, numbered options, 220ms auto-advance)
Theme/                  theme.rs port (gh#181): oklch→sRGB converter, the four
                        text greys, the `Status` vocabulary and the one function
                        that turns a meaning into paint, three radii + the
                        nesting rule, four type sizes + the reserved prose pair,
                        Geist/Geist Mono, motion timings + flavour words.
                        Two variants since gh#257: every paint token is
                        `themed(dark:light:)`, one Color that resolves against
                        the trait collection, so no screen asks which scheme it
                        is in. Appearance.swift holds the preference and is the
                        one place a scheme is chosen; the ring-fenced plist key
                        still makes Dark the shipped default
```

### Parity notes (desktop ⇄ mobile translations)

| Desktop | iOS |
| --- | --- |
| Sidebar: Spaces + attention-sorted Sessions | Home screen sections (same sort ranks: awaiting > errored > working > completed > idle) |
| Horizontal session tabs per space | Space detail: vertical session list (creation order) |
| Tab close = archive | Swipe-to-archive |
| Composer `white_alpha(0.03)` pill + hairline | Liquid Glass pill (`glassEffect`) + hairline |
| Harness brand SVG marks (icons.rs) | Same path data via a native SVG path parser (`BrandMarks.swift`) |
| Harness/model picker popover + curated catalogs | Brand-mark cards + catalog menu + reasoning-ladder chips (`HarnessCatalog.swift`, ported from crates/harness) |
| Add-space palette: repo list, folder browser behind it (gh#118) | "Add a repo" sheet: same repo list (search, connect-inline), folder browser one tap down |
| Repo-first picker's host sweep (`ListRepoSpaces`, all answers kept) | Same sweep over each device's own room; one host clones silently, two ask |
| Onboard inline from the palette (`OnboardRepo`) | Same, with the clone's spinner and its refusals on the row |
| Folder browser: device tabs + remote listing | Same, as a pushed screen (ListFolders over the device-room relay, git repos badged) |
| ControlRpc over device-room relay | `DeviceRelayClient` — binary `uleb128(len)+header+payload` frames, `{"s","k","to","from"}` header, ndjson ControlRpc; unary `call` **and** streaming `subscribe` (`{item}`/`{done}`, `{id,cancel}` on drop); used for ListFolders, direct-to-host `Mutate {createSpace}`, and the board four |
| Board panel (`ui/src/board.rs`) | Board screen: same sections/glyphs/metadata, dispatch + retry as a sheet (`Board/`) |
| Settings → Board stats: a 1160px two-column dashboard (gh#143/gh#151) | Stats screen off the board: the headline panel (count, qualifying facts, per-space split) and one column of evidence under it — no tile rows, no side-by-side panels, no hour-of-day |
| Sidebar chat menu: pin / unpin the orchestrator (gh#104, gh#144) | Same item, same words, on three surfaces (gh#166): the chat screen's ⋯ menu, a long-press on its Active or Needs-you row, and a long-press on the pinned slot itself. Pinning asks first and names what it replaces — one `orchestrator_chat` key, so a second pin moves it; unpinning is immediate, and is the phone's only route to `routes defaults orchestrator_chat --unset`. Never offered on a chat the board dispatched |
| Sidebar Agents section (gh#103) | Same section on Home, between Spaces and Sessions |
| Board host sweep with `targetDeviceId` | Same candidate order, dialling each device's room directly — the phone has no local engine to forward through |
| Hover timestamps / copy | Context menus |
| gpui `list()` sum-tree virtualization | `LazyVStack` + stable row ids + version fingerprints |
| Stick-to-bottom spring, wheel-up breaks pin | Scroll-phase-gated pin + spring scrollTo, same 70/320pt thresholds |
| `Theme::status` — four hues at one lightness, and every state type maps into `Status` once (gh#173) | `Theme.status` and the same `Status.ofBoard` / `ofAgent` / `ofChat`, so a working agent is the same amber on the board and on Home |
| Three radii, four type sizes, four text greys (gh#172/gh#174) | The same numbers, asserted equal by `crates/ui/tests/ios_theme.rs` |
| Light theme (gh#177) | Implemented in gh#257 from the supplied iOS reference's exact neutral, status and Claude variables, deliberately distinct from the desktop's cool-blue light palette. Chosen once in `Appearance.swift`, resolved per token, never per view |
| Hover wash vs selection lift (gh#175) | Not ported — no pointer; `elementHover` is what a finger holds down |

Status colors, fonts, spacing, markdown metrics, veil timing, command-ledger
shapes, and the wire protocol are ports, not approximations — constants match
the desktop sources cited in each file header.

### Writer discipline (what the phone writes)

- Workspace doc: its own device row, chat creates (host = the space's owning
  device), `archived`/`title`/`lastSeenAt` LWW sets, presence heartbeats.
- Session docs: command ledger appends only (`run`/`steer`/`interrupt`/
  `respondInput`), with client-minted message ids for optimistic echo. The
  host writes all transcript entries and command outcomes.
- After queuing a command it POSTs `/device/{host}/nudge` so a cold host
  opens the doc and drains — delivery stays durable in the doc regardless.
