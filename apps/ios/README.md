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

Or open `Comet.xcodeproj` in Xcode and run. Dependencies (SPM, resolved
automatically): [loro-swift 1.13.x](https://github.com/loro-dev/loro-swift)
(matches the engine's loro 1.13), [swift-markdown](https://github.com/swiftlang/swift-markdown)
(cmark-gfm: tables/strikethrough/tasklists — the same feature set as the
desktop's pulldown-cmark config).

### Connecting

- **WorkOS**: enter the edge URL, open the sign-in page on any device, paste
  the code it shows (`/auth/exchange`), pick an org (`/auth/refresh` re-scopes
  the token with the `org_id` claim).
- **Dev**: against an `AUTH_MODE=dev` edge (e.g. `wrangler dev`), enter a user
  id + org id; the bearer is `userId@orgId`.
- **Demo mode**: fully offline dataset with a scripted streaming reply and a
  board (rows in every state, two live attempts wired to real demo chats) —
  explore the UI with no infrastructure. Launch args for screenshot rigs:
  `-demo [-route chat:<id>|space:<id>|board] [-sheet dispatch] [-stream]`.
  `-route`/`-sheet` work against a live edge too, which is where the rows that
  matter are.

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
7 days. TestFlight needs the $99 Apple Developer account — the same one gh#100's
signing tier wants, so it is one purchase for both.

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
  ActiveSection.swift   the one live group (gh#103 + gh#117, merged by gh#123),
                        phone-shaped: attempts with identifier chips, unmanaged
                        runs by bare title, needs-you first
Models/
  RepoRows.swift        view/repos.rs port (gh#118): the repo-first picker's
                        union of spaces + the board App's grant, its box-first
                        order, and the name-before-owner search rank
Sync/
  LoroProtocol.swift    loro-protocol 0.3 wire codec (byte-compatible port of
                        the crate's encoding.rs: magic/varBytes/type/payload)
  RoomClient.swift      room.rs port: join with oplog VV, snapshot backfill,
                        resubmit-from-server-VV, DocUpdate+Ack, fragments,
                        %EPH presence sub-room, ping/pong lease, backoff
  WorkspaceStore.swift  ws3/{org}/{user} mirror: devices/spaces/chats/sessions
                        rows, presence heartbeats, viewer-side writes
                        (createChat, archive, lastSeenAt, own device row)
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
Composer/               glass pill, Send→Steer→Stop morph, QuestionPanel
                        (paged, numbered options, 220ms auto-advance)
Theme/                  theme.rs port: oklch→sRGB converter, exact palette,
                        Geist/Geist Mono, motion timings + flavour words
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
| Sidebar Agents section (gh#103) | Same section on Home, between Spaces and Sessions |
| Board host sweep with `targetDeviceId` | Same candidate order, dialling each device's room directly — the phone has no local engine to forward through |
| Hover timestamps / copy | Context menus |
| gpui `list()` sum-tree virtualization | `LazyVStack` + stable row ids + version fingerprints |
| Stick-to-bottom spring, wheel-up breaks pin | Scroll-phase-gated pin + spring scrollTo, same 70/320pt thresholds |

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
