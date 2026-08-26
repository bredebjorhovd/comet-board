# comet-native — Architecture

A ground-up native rewrite of [comet](../comet) — a multi-device controller for coding agents
(Claude Code / Codex) — in Rust, with a gpui UI. Fresh app; no backwards compatibility required.

**Pillars (from the goal):**
- Sync is Loro CRDT docs (loro-mirror model) through Cloudflare Durable Objects.
- Durable Objects stay **TypeScript** (decision + evidence: `docs/research/durable-objects-language.md`).
  Everything device-side is Rust.
- Feature parity with comet **except token-usage display in the transcript/docs** (poor fit for
  CRDTs; excluded). The board keeps its own token totals in `board.db` for its stats page — a
  different store with a different lifetime (§gh#151).
- Frontend is **gpui** (pinned Zed rev). Virtualization + markdown techniques ported from
  **mugen + pretext** (`docs/research/mugen-pretext.md`).
- One binary, **headed or headless**. Smooth transitions/animations matching the original
  (catalog in `docs/research/feature-inventory.md` §1.12).

## 1. Topology (unchanged shape, new materials)

```
gpui UI ─ in-proc/localhost RPC ─ engine A ══ DeviceRoom DO relay ══ engine B ─ RPC ─ gpui UI
                    │            (edge Worker: auth, rooms, R2)          │
                    └── Loro sync ── SessionRoom DO (per chat) ──────────┘
                                └── Workspace doc room (per org) ────────┘
```

- **Engine = backend** (was `@comet/backend`): runs agents, owns auth, terminals, repos/worktrees,
  diff sync, doc hosting. Pure Rust daemon, fully functional headless.
- **UI = viewport** (was Electron): gpui app rendering engine state. Talks the same typed RPC
  whether the engine is in-process or a separate daemon. Organized around **spaces** — synced
  (device, folder) pairs: the sidebar is two sections and a pin (gh#547) — "Needs you" (the inbox
  projecting what wants a human), the board-notices slot, and Spaces, whose rows disclose
  their sessions inline; the main area shows the selected space's sessions as horizontal tabs
  (closing a tab archives); new sessions are minted onto the space's device via relay-forwardable
  RPCs.
- **Edge (TypeScript, ported from comet `apps/edge`)**: Worker + SessionRoom DO (per chat) +
  DeviceRoom DO (per device) + R2 attachments + WorkOS JWKS auth. Absorbs the old `apps/server`
  responsibilities (WorkOS code exchange/refresh, orgs) so **Postgres, the Hono server, and
  the WebRTC/signaling stack are all gone**.

### Headed / headless
Single binary `comet`:
- `comet` — headed. If a local engine daemon is already listening on the IPC port, connect to it;
  otherwise run the engine **in-process** (RPC over an in-memory duplex — same protocol, zero
  serialization shortcuts, so the boundary stays honest) **and serve that same engine on the IPC
  port**. The embedded engine is not private: any other viewport can attach to the running app
  without it first being restarted as a daemon. Binding is best-effort — if the port is taken the
  window still opens, having lost only the ability to host peers.
- `comet headless` — engine only; prints sign-in URL on TTY (paste-code flow), serves IPC on
  localhost + hosts its DeviceRoom for remote control. A VPS runs this; a laptop's UI drives it.

### Second viewport (the iOS app)
The gpui app is not the only surface, which is why the derived view logic lives in
`comet_proto::view` (sort orders, staleness gating, sidebar grouping, the boot gate — shared so
row order can never diverge per surface): the iOS app mirrors those derivations in Swift against
the same wire shapes. A terminal viewport (`comet-tui`) was the original second frontend; it was
removed in gh#416 after nobody ran it, but the factoring it forced is the factoring the phone
now depends on.

## 2. Data model — all Loro

Two doc kinds, one room protocol (loro-protocol over WebSocket, the same protocol the TS edge
already speaks; Rust side uses the official `loro-protocol`/`loro-websocket-client` crates or a
thin hand-rolled client over `loro` 1.13.x — verify interop early, M1 exit criterion):

1. **Session doc** (per chat) — the transcript + durable command queue. Schema is a Rust port of
   `packages/session-doc` (same container names/shapes so the edge's tail materializer keeps
   working): `meta` map, `messages` list (parts as list-of-maps with **LoroText bodies** — the
   measured 1.03× oplog shape; never LWW value rewrites), `commands` list with ledger rules 1–3
   (append-only per-device entries; host-only outcomes; dedupe/TTL/supersede evaluation).
   Continuation splitting at 256KB, render-only tool parts (full inputs stay in the host's local
   run journal), tail/diff sidecars. Constants carried over (`STREAM_COMMIT_MS=120`,
   `DO_FLUSH_MS=5s`, compaction at 8MB, retain 30d, tail 64).

2. **Workspace doc** (per org — NEW; replaces comet's residual entity sync) — **spaces**
   registry (id, deviceId, path, name?, gitDetected, checkoutId — a space is a synced
   device+folder pair, the app's unit of organization; the owning device's SpacesSync stamps git
   presence so branch pickers / the diff sidebar gate on a synced bool, no RPC), chats index
   (id, deviceId, title, archived, cwd, branch, checkoutId, spaceId, lastSeenAt,
   lastMessagePreview/At, config), devices registry (id, name, platform, lastSeenAt), session
   status rows (Working indicator; staleness-checked client-side so a crashed backend never shows
   eternal "Working"; plus the run's current in-flight tool call — what it is doing and since when,
   gh#605 — so a healthy run inside a 40-minute command is distinguishable from a hung one without
   opening the transcript), checkout-diff summary pointers. `lastSeenAt` is the synced LWW seen marker
   behind the "completed (unseen)" indicator. Lives in its own DO room (same SessionRoom DO
   class, doc id `ws4/{orgId}/{userId}` — see the generation ladder below), with presence
   via Loro `EphemeralStore` (replaces the 15s heartbeat writes). Presence is DERIVED by the edge
   from each room's socket set and pushed on join/close, never beaten in by clients: a `%EPH`
   frame wakes the Durable Object, so a 15s client heartbeat meant a room that could never
   hibernate — 10,800 GB-s/day per room, 83% of the free tier for an idle fleet (gh#145). Clients
   read it as membership and ask (`GET /workspace/{orgId}/presence`) when someone looks; a
   device's own `DeviceRoom` (`/status`, derived the same hibernation-safe way) is the independent
   check that can retire a socket whose uplink died silently. Writer discipline: each device
   writes only its own device/session/chat rows and the git stamps of spaces it owns;
   creates/renames/archives/seen-marks are LWW map sets from any device. `deleteSpace` cascades:
   the space row and every chat/session row in it tombstone in one commit.

   *Why a workspace doc and not N tiny docs:* the sidebar needs one subscription for the whole
   list (grouping, resort animations, unseen markers); one doc = one room connection + one mirror.
   Volume is tiny (index rows, no transcripts), so oplog growth is negligible and daily compaction
   applies anyway.

   *Per-user, with one exception:* the room is really `ws4/{orgId}/{userId}` — spaces, chats and
   sessions are private to the person who made them. Devices are the exception (gh#66): a
   teammate who cannot see the box cannot address it, cannot sweep it for a board, and cannot
   reach the shared work on it. So device rows are ALSO published to an org-wide registry doc
   (`orgdev1/{orgId}`, `comet_engine::org_devices`), and `WatchDevices` serves the union.
   Presence beats on both rooms.

   *The generation counter, and why abandoning storage is cheap:* the leading number is a
   destructive break — `ws2` the spaces overhaul, `ws3` the per-user privacy split
   (`ws2/{orgId}` → `ws3/{orgId}/{userId}`), `ws4` the 2026-08-04 incident break (gh#148).
   A room name is the Durable Object's identity (`idFromName`), so bumping it allocates virgin
   storage and orphans the old instance, which is never dialed again and hibernates at ~zero
   cost. That is survivable because **the edge is not authoritative for this doc**: every
   signed-in device holds a complete local replica (`workspace2` in its own `DocsStore`), so a
   virgin room is re-seeded by the first device to join — the ordinary
   resubmit-from-version-vector path on a server whose version vector is empty — and merged into
   by the rest. There is no migration script, no cutover window, and no operator step. The
   corollary is a rule: **a room generation bump must never bump the local snapshot row id.**
   Bump both and there is nothing left to re-seed from, and an edge-side break that loses
   nothing becomes real data loss on every device at once.

   Two properties keep the bump transparent rather than a flag day, and both are pinned by
   tests. First, generations are **worker-internal**: clients dial `/workspace/{orgId}/ws`,
   which names no generation, and the room id inside protocol frames is an echo label that
   neither side routes on — so an engine still saying `ws3/…` lands in the ws4 room and
   converges. This matters because our engines retry a failed join *forever*: a version skew
   that did bite would be a silent outage, not an error. Second, the force-trim guard
   (`isConcurrentWriteRoom`) classifies rooms **by name**, and upstream's version matched the
   literal `ws3/` — when the room became `ws4/` the protection silently evaporated and re-broke
   the incident it was written for. Hence a generation-agnostic pattern, and hence
   `edge/src/rooms.test.ts` asserting the name generator and the guard still agree.

   The org device registry is deliberately **not** bumped in sympathy. It is a separate room
   with its own lifetime, it did not carry ws3's damaged storage, and abandoning it would blank
   the one index by which a teammate can find the box at all.

3. **Mirror layer** (`comet-doc` crate) — Rust equivalent of loro-mirror: typed structs for the
   schema, **incremental** application of `doc.subscribe` diffs into cached state (no full
   re-hydration per change — this is also what fixes comet's known O(transcript) re-projection
   inefficiency, remaining-work item 1a), and a diff-reconcile write path (evaluate `lorosurgeon`
   0.2.x as a dep; our schema is small enough to hand-roll if it doesn't fit). The UI renders
   mirror state directly with per-entry change notifications — the "endgame" the TS
   implementation documented but never reached.

### Command plane
Send/steer/interrupt/respondInput = durable command entries in the session doc (`QueueCommand`),
executed by the chat's **host** device (executor gated on chat ownership; mark-processed BEFORE
execute; steer with no live run dispatches as the next turn). Offline sends queue in the doc.
This is comet's proven design, kept verbatim.

### Convergence recovery (gh#483)
The edge's daily **history trim** discards op history and keeps state (§3.1). A replica whose
local commits depend on ops behind that new shallow root can no longer merge with the room —
`ImportUpdatesThatDependsOnOutdatedVersion` — and the only repair is to adopt the server's
snapshot and put local content back on top of it. `crates/sync/src/convergence.rs` is the one
place either client may do that, and `apps/ios/Comet/Sync/Convergence.swift` is the phone's copy
of it (held to the same shape by `crates/sync/tests/ios_room.rs`). The invariant:

> No locally committed semantic entry may be discarded until the edge has acknowledged it and a
> fresh independent client can retrieve its stable id.

Three mechanisms, in order of how much they are trusted: a durable **quarantine** of the whole
pre-replacement document, retained until a fresh client (not the recovering one) reads every
stable id back out of the room; a durable **semantic outbox** of locally committed entries and
commands *as content* — parts, timestamps, statuses, continuations, attachment refs, provenance —
which survives losing the graph that carried them, and drains only when the edge's advertised
version proves it holds them; and **replay that is idempotent by stable id**, so a crash between
any two phases (quarantined → reseeded → replayed → acknowledged) is finished by the next open
rather than stranding content. Retaining more history at the edge makes the reseed rarer; it is
not the safety mechanism.

Truthful state rides with it: `RoomClient::convergence()` — converged / pending N / recovering /
blocked-local-only — is independent of `connected()`, and `EdgeHealth` counts the two separately,
because the incident was a Mac whose room was joined, ponging and presence-live for a week while
the cloud sat 74 transcript entries behind it. Transport liveness is not content convergence.

## 3. Cargo workspace

```
comet-native/
  Cargo.toml                 # workspace
  crates/
    proto/        comet-proto    # wire types: AgentEvent, ToolCall, RunRequest, Model,
                                 # entities, RPC envelopes (serde; ndjson framing);
                                 # `view` = the pure derivations both frontends share
                                 # (sort orders, staleness gating, grouping, boot gate)
    doc/          comet-doc      # session-doc + workspace-doc schemas, mirror layer,
                                 # parts fold, continuations, command ledger, sidecars
    sync/         comet-sync     # loro room client (join/VV backfill/fragments/backoff),
                                 # ephemeral presence, DocsStore (SQLite snapshots +
                                 # processed-command ledger + the semantic outbox and
                                 # recovery quarantine), convergence recovery (gh#483)
    harness/      comet-harness  # Harness trait + claude-code (stream-json subprocess),
                                 # codex (app-server JSON-RPC), mock; steering mailbox,
                                 # requestInput, models/reasoning/options catalogs
    engine/       comet-engine   # sessions engine (pub/sub, run journal, recovery, stall
                                 # watchdog), doc host + command executor, repos/worktrees,
                                 # checkout-diff sync, terminals (portable-pty), uploads,
                                 # agent accounts (cred swap + per-run dirs), auth (WorkOS),
                                 # device-room host/peers, identity
    rpc/          comet-rpc      # UiRpc/ControlRpc: typed req/resp/stream over WS (tokio-
                                 # tungstenite) + in-memory transport; device-room virtual
                                 # sockets ({s,k,to,from} frames)
    ui/           comet-ui       # gpui app: shell, sidebar, conversation, composer,
                                 # terminal view, diff pane, settings, animation kit
  apps/
    comet/                       # the binary (headed default, `headless` subcommand)
    board-cli/                   # the `comet-board` binary — ships in the same release
                                 # payload as `comet` and must not drift from it (gh#156)
  edge/                          # TypeScript Worker + DOs (ported from comet/apps/edge,
                                 # + auth-exchange routes absorbed from apps/server)
  docs/                          # this file + research reports
```

Engine async runtime: **tokio** throughout; the UI bridges via `gpui_tokio` (`Tokio::spawn`
futures surfaced as gpui `Task`s). In-process mode runs the engine on its own tokio runtime
thread; the UI never blocks on it.

## 4. UI plan (gpui) — parity + smoothness

Reference: `docs/research/gpui.md`, `docs/research/mugen-pretext.md`,
feature spec `docs/research/feature-inventory.md` §1.

- **Deps**: `gpui` + `gpui_platform` pinned to one Zed rev (Apache-2.0). **We do not use Zed's
  GPL crates** (`markdown`, `ui`, `theme`, `editor`) — markdown, components, and theme are ours.
- **Transcript**: gpui `list()` + `ListState::new(n, ListAlignment::Bottom, overdraw)` (sum-tree
  offsets, follow-tail). On top of it, port the mugen behaviors that gpui doesn't give us:
  - stick-to-bottom **spring** with feed-forward tracking of streaming growth; interrupt from
    *user input* (wheel-up / drag), re-engage within a 70px band; own-send re-engages + smooth
    scrolls;
  - **block-granularity rows** (one row = one markdown block / tool group, not one message) with
    stable ids `msgId#blockId`; live turn stays unsplit, re-splits on persist; optimistic echo
    rows share the client-minted id so persistence never flickers;
  - row height memoization keyed by (row id, content length, width) so a streamed token
    re-measures one row;
  - scroll-anchor absorption for above-viewport height changes.
- **Markdown** (`comet-ui::markdown`): `pulldown-cmark` parsing on `background_spawn` with
  coalescing (Zed's proven pattern), block-level incremental re-parse of the streaming tail
  (incremark's O(delta) idea: only re-parse from the last stable block boundary), monochrome
  theme where **numbers drive layout, colors are paint**. Code blocks: monospace, no wrap ⇒
  height = lines × line-height (layout independent of highlight); syntax highlighting via
  `synoptic`/`syntect`-class tokenizer run time-sliced in the background, colors applied as text
  runs (paint-only). Streaming **fade-in veil** on newly appended text via `with_animation`
  opacity (paint-layer, never affects layout). `prefers-reduced-motion` honored.
- **Composer**: hand-rolled gpui text input (start from Zed's `examples/input.rs`: IME, selection,
  clipboard, key actions), compact↔expanded auto-flip by measured text width, auto-grow 76–260px,
  Enter/Shift+Enter, Send→Steer→Stop morph, drafts + attachments per chat, drag-drop/paste
  images, QuestionPanel (paged, 1-9 keys, 220ms auto-advance) replacing the composer while input
  is requested. Pickers (harness/model, traits, repo w/ folder browser, branch w/ worktree
  toggle) as gpui popovers with `menu-in` scale/fade.
- **Terminal**: `alacritty_terminal` (vte state machine, MIT/Apache) + `portable-pty` on the
  engine side; custom gpui grid element; tabs w/ drag-reorder (150ms sliding transforms), height
  drag 160px–55vh, 12ms input coalescing / 80ms resize debounce, 1MB replay, detach ≠ close.
- **Diff pane**: unified-patch parser → virtualized file/hunk/line rows, per-file collapse
  (180ms height tween), time-sliced highlight, 200ms width transition on the pane itself.
- **Animation kit** (`comet-ui::motion`): small helpers over gpui `Animation` reproducing the
  comet catalog — `fade-in` (0.5s, cubic-bezier(0.16,1,0.3,1), translateY 4→0), `splash-out`,
  `comet-pulse` staggered cell wave (boot splash + loaders), `gradient-spin-pulse` matrix
  spinner (WorkingIndicator + rotating flavour word), `menu-in`/`dialog-in` scale-fades, 200ms
  ease-out width/height transitions for sidebar/panes, sidebar-resort **slide animation**
  (we own the list, so animate row positions directly — the View Transitions equivalent, 260ms
  cubic-bezier(0.22,1,0.36,1)), reduced-motion switch.
- **Theme**: always-dark monochrome, oklch-derived neutral scale precomputed to Hsla, hairline
  borders, Geist/Geist Mono bundled fonts.

## 5. Engine plan

Direct ports of comet behaviors (spec: feature-inventory §3):
- **Sessions engine**: per-session broadcast hub; on-disk run journal (resumable `seq` replay,
  crash auto-resume); persistent steerable sessions (steering mailbox at step/turn boundary; 30min
  idle reaper; 10min stall watchdog, tiered — terminal only for a run that never emitted anything,
  advisory once it has, since no timeout can bound a legitimate silent tool call); recovery stamps
  `aborted`; crash shield (panic → bounded drain → exit) on the headless daemon.
- **Doc host**: per-chat handle (join room, VV backfill, write user entries + stream assistant
  segments at 120ms commits, drain commands host-only with processed-ledger idempotence, publish
  diff sidecar, presence); warm-open recent chats (14d/cap 30); nudge-driven cold open; SQLite
  snapshot store. The handle map is a **cache**: chats nobody is watching, running or holding are
  released after 5min idle (LRU-bounded at 32), because every open chat is a standing edge socket
  and an insert-only map made per-chat rooms the dominant load on the edge (gh#395). Re-opening is
  transparent — the snapshot is the doc — and a command for a released chat still arrives by nudge.
- **Harness** (research pending — `docs/research/harness.md`): trait mirroring comet's
  `HarnessShape`; Claude Code via `claude` CLI stream-json in/out (control protocol for
  permissions/AskUserQuestion→requestInput, resume, steering); Codex via app-server JSON-RPC or
  `codex exec --json`; model/reasoning/option catalogs ported from `packages/harness`.
- **Repos/diffs**: git2 or `git` subprocess (subprocess — matches comet, avoids libgit2 edge
  cases); worktrees under `~/.comet-native/worktrees`; fs watchers (`notify`) + 2min repair; diff
  capture (patch + numstat + untracked, 3MiB cap, sha256) → workspace doc summary + DO diff
  sidecar.
- **Agent accounts**: credential-slot swap (macOS Keychain via `security-framework`, files
  elsewhere), plan labels, usage probes, paste-code/browser-poll OAuth flows. Plus a
  fork addition (gh#59): a slot also materializes into a config dir of its own
  (`{data_dir}/accounts/{slotId}/`) that a run points its harness child at with
  `CLAUDE_CONFIG_DIR` / `CODEX_HOME`, so several teammates' subscriptions coexist on one
  box and no swap happens under a live run.
- **Auth**: WorkOS through edge routes (`/auth/exchange`, `/auth/refresh`, orgs, member
  invitations); loopback callback server headed, paste-code headless; dev mode (no key ⇒
  bearer = configured user id). The background refresh loop is held to one invariant
  (gh#153): **every iteration waits** — on the cached token's own life, or on a retry floor
  when the attempt left no usable token. Two thresholds are in play (a dial needs 30s of
  token, the loop wants 60s) and both are passed to the refresh as `min_remaining`, because
  a loop that asks for 60s and is handed back a 45s token has nothing left to sleep on.

*What an idle engine costs.* The other half of the DO-duration story above is the client
side: an engine hosting no chats should be indistinguishable from a parked process. Measured
(2026-08-08, signed in, zero chats, over ten minutes and two token cycles): **0.73 CPU-seconds
in 586 — about 0.12% of a core**, of which the periodic work is one local watch republish per
15s, one relay-status probe per 30s, and one token refresh per 240s. That is the number to
re-measure against; anything materially above it is a spin, and the first place to look is a
`select!` branch that is always ready or a loop whose "retry" path can complete without
waiting. Both shapes have now cost real money here — gh#145 on the bill, gh#153 on the box,
where an idle engine burned 4h46m of CPU in 21h with zero agents ever dispatched.

## 6. Edge plan (TypeScript, `edge/`)

Port `comet/apps/edge` nearly verbatim (it is already Loro-native and smoke-tested: session room
w/ hibernation + two-level compaction + daily alarm backups, device room byte relay + nudges +
sidecar slots, R2 attachments, JWKS auth). Additions:
1. Workspace-doc rooms (`ws/{orgId}`) — same DO class, org-membership authz instead of
   claim-on-first-join.
2. `/auth/*` routes absorbed from `apps/server` (WorkOS API key in Worker secret).
3. Drop `/seed` migration path and legacy sync anything (fresh app).
4. Org-shared visibility (gh#66) — what a SECOND user of an org can reach. The Worker
   stamps the verified `org_id` on every DO forward, and: the org device registry room
   (`orgdev1/{orgId}`) is org-membership authz'd like a workspace room; device rooms record
   the org that claimed them and admit any member as a *client* (hosting stays owner-only);
   chat rooms stay owner-only until the owner marks one shared (`POST /share/{chatId}`,
   which the board does for every task it dispatches), after which the org may read and
   write it. Private chats never become org-visible by being in an org.
5. Member invitations (gh#76) — `/auth/orgs/:id/invites` (list/create/revoke) and
   `/auth/invites/accept`, so adding a teammate is Settings → Members instead of a
   hand-made `organization_membership` in the WorkOS dashboard. Admin-gated on the org's
   actual membership list, never on the caller's `org_id` claim (which says which org a
   session is scoped to, not what role it holds); an invitation is redeemable only by the
   address it names, while it is pending.
Hibernation hygiene: no idle timers (flush timer only while dirty), auto-response ping/pong —
per `docs/research/durable-objects-language.md`.

## 7. Parity exclusions & deliberate changes

- **Excluded**: token-usage display *in the CRDT docs* (profile heatmap, lifetime stats,
  per-message token columns, `WatchUsage`). Rate-limit meters on agent accounts are *kept*
  (separate concern; probed from CLIs, not CRDT-synced), and so are the board's own per-attempt
  totals — summed off the host's run journal into `board.db`, never into a session doc (gh#151).
  **Context fullness follows the same rule and gains a reason of its own** (gh#271): a level
  rather than a flow, so a doc that kept every reading would replay a hundred stale gauges to
  anybody scrolling back. It rides `AgentEvent::ContextUsage` to the run journal and, from
  there, onto the attempt row — live-attempt state, not transcript content.
- **Changed**: Postgres entity sync/server → workspace doc + edge; Electron/React/mugen → gpui with
  ported techniques; Node harness SDKs → subprocess protocols; WebRTC → device-room relay (comet
  had already made this move); mobile app → out of scope for this repo.
- **Kept verbatim**: session-doc schema shape + constants, command ledger rules, edge DO design,
  render-parts privacy policy, UX behaviors and animation timings.

## 8. Milestones

Status legend: ✅ shipped · 🟡 shipped with named gaps (see `docs/PARITY.md`).

- ✅ **M0 Scaffold** — workspace builds; `proto`/`doc` crates with ledger + parts + continuation
  unit tests; gpui hello-window runs.
- ✅ **M1 Doc + sync core** — `comet-doc` mirror over loro 1.13; room client syncs with the edge
  running under `wrangler dev`; Rust⇄edge⇄Rust convergence test (M1 exit: two Rust peers converge
  through a real SessionRoom DO, tail endpoint serves).
- ✅ **M2 Engine core** — Claude harness end-to-end headless: `comet headless` + dev auth runs a
  turn, journal + doc writes, recovery test.
- ✅ **M3 UI core** — shell (sidebar/panes/header), transcript (virtualized, markdown, streaming,
  stick-to-bottom), composer (send/steer/stop, question panel); local chat fully usable headed.
- ✅ **M4 Multi-device** — device-room host/client virtual sockets, remote device control, workspace
  doc entity sync, WorkOS auth + org gate, presence. Proven live by `scripts/e2e-smoke.sh`:
  two headless engines against a real edge — B queues a run into the chat doc, the durable
  nudge wakes host A, A executes (mock harness), transcript + session status sync back to B.
  Its two-USER sibling `scripts/e2e-org-smoke.sh` (gh#66) proves the org gates the same way:
  a second WorkOS user of the org sees the box in `WatchDevices`, relays a `targetDeviceId`
  RPC to it, and opens + steers a chat the box shared — with the run executing on the box.
- 🟡 **M5 Full surface** — terminals, diff pane, repo/branch/folder pickers + worktrees,
  agent accounts UI, settings (devices/shortcuts/archived), Codex harness. Gaps: composer
  attachment UI (engine upload RPCs exist), Cursor harness.
- 🟡 **M6 Polish** — wire reconciliation (proto AuthState on the wire, `LocalDevice`),
  two-device e2e smoke, keyboard map, clippy/fmt sweep, Linux packaging
  (`scripts/package-linux.sh` + release profile), macOS packaging (`scripts/package-macos.sh`
  + `dist/macos/`, executed — dmg with app bundle, icns, `/Applications` symlink).
  Gaps: prefers-reduced-motion, parent-PID watchdog, edge production deploy, and a
  Developer ID for macOS — releases are ad-hoc signed, so a downloaded `Comet.app` is
  Gatekeeper-rejected until the user clears quarantine ([docs/macos-install.md](docs/macos-install.md));
  the signing + notarization path in `release.yml` is written and waits only on the cert.

## 9. Open questions (tracked, non-blocking)

1. loro-protocol Rust client ⇄ TS edge interop — verify at M1; fallback is a ~300-line hand-rolled
   client (the frame protocol is small and we control both ends).
2. `lorosurgeon` fit for the mirror write path vs hand-rolled reconcile.
3. Cursor harness (comet has it; CLI surface for Rust TBD) — parity item, scheduled after Codex.
4. Text shaping performance for analytic row heights: gpui measures shaped text natively (Rust ⇒
   cheap), so we start with gpui `list()` measurement + memoization rather than porting pretext's
   full analytic kernel; revisit only if cold-open of huge transcripts measures slow.
