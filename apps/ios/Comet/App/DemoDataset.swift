// Offline demo dataset — realistic spaces/sessions/transcripts so the app can
// be explored (and screenshotted) with no edge deployment. The flagship chat
// streams a reply on demand, exercising the live-row pipeline: incremental
// re-parse, veil fade-in, stick-to-bottom.

import Foundation
import Observation

@MainActor
@Observable
final class DemoDataset {
    var devices: [DeviceRow]
    var spaces: [Space]
    var chats: [Chat]
    var sessions: [String: SessionRow]
    /// The board (gh#114). Two live attempts wired to real demo chats, so the
    /// Agents section and the board rows agree with each other and tapping a
    /// row opens a transcript that exists.
    var boardRows: [TaskRow] = []
    /// The board's pinned orchestrator (gh#122) — the demo pins its long-lived
    /// driver chat, so the slot above Spaces and the Needs-you inbox are
    /// explorable with no infrastructure.
    var orchestratorChatId: String? = "chat-orchestrator"
    private var stores: [String: SessionStore] = [:]
    private var streamTask: Task<Void, Never>?

    private static let dummyConfig = AppConfig(
        edgeURL: URL(string: "http://localhost:8787")!, mode: .dev,
        userId: "demo", orgId: "demo", deviceId: "ios-demo", deviceName: "iPhone")

    init(devices: [DeviceRow], spaces: [Space], chats: [Chat], sessions: [String: SessionRow]) {
        self.devices = devices
        self.spaces = spaces
        self.chats = chats
        self.sessions = sessions
    }

    static func standard() -> DemoDataset {
        let now = nowMs()
        let mac = DeviceRow(id: "dev-mac", name: "MacBook Pro", platform: "macos",
                            lastSeenAt: now, createdAt: now - 86_400_000 * 30)
        let vps = DeviceRow(id: "dev-vps", name: "hetzner-01", platform: "linux",
                            lastSeenAt: now - 600_000, createdAt: now - 86_400_000 * 12)
        let comet = Space(id: "space-comet", deviceId: "dev-mac",
                          path: "/Users/dev/comet-native", name: nil, gitDetected: true,
                          gitCheckedAt: now, checkoutId: nil, createdAt: now - 86_400_000 * 9)
        let edge = Space(id: "space-edge", deviceId: "dev-vps",
                         path: "/srv/deploys/edge", name: nil, gitDetected: true,
                         gitCheckedAt: now, checkoutId: nil, createdAt: now - 86_400_000 * 4)

        let claude = ChatConfig(harness: "claude-code", model: "claude-fable-5",
                                reasoning: "xhigh", sandbox: "workspace-write")
        let codex = ChatConfig(harness: "codex", model: "gpt-5.6-terra",
                               reasoning: "high", sandbox: "workspace-write")

        let chats = [
            Chat(id: "chat-veil", deviceId: "dev-mac", title: "Streaming veil on transcript rows",
                 archived: false, cwd: "/Users/dev/.comet-native/worktrees/comet-native-veil-fade",
                 branch: "veil-fade", checkoutId: nil,
                 config: claude, lastMessagePreview: "Porting the paint-only fade…",
                 lastMessageAt: now - 40_000, createdAt: now - 3_600_000,
                 spaceId: comet.id, lastSeenAt: now),
            // The board's blocked attempt (AGE-14) — a dispatched agent is a
            // chat among chats, which is the whole reason gh#103 exists.
            Chat(id: "chat-picker", deviceId: "dev-mac", title: "Model picker catalog sync",
                 archived: false,
                 cwd: "/Users/dev/.comet-native/worktrees/comet-native-catalog-sync",
                 branch: "fix/catalog-sync", checkoutId: nil,
                 config: claude, lastMessagePreview: "Which device owns the catalog?",
                 lastMessageAt: now - 120_000, createdAt: now - 7_200_000,
                 spaceId: comet.id, lastSeenAt: now - 130_000),
            // The pinned orchestrator (gh#122): working right now, with a
            // report you have not opened — the slot shows the spinner AND the
            // unread badge, which is the demo's whole pitch for it.
            Chat(id: "chat-orchestrator", deviceId: "dev-mac", title: "Board orchestrator",
                 archived: false, cwd: comet.path, branch: "main", checkoutId: nil,
                 config: claude, lastMessagePreview: "Two agents up in the tally space…",
                 lastMessageAt: now - 20_000, createdAt: now - 86_400_000 * 3,
                 spaceId: comet.id, lastSeenAt: now - 120_000),
            Chat(id: "chat-tabs", deviceId: "dev-mac", title: "Tool group header colors",
                 archived: false, cwd: comet.path, branch: "main", checkoutId: nil,
                 config: codex, lastMessagePreview: "Done — failed children stay quiet.",
                 lastMessageAt: now - 900_000, createdAt: now - 86_400_000,
                 spaceId: comet.id, lastSeenAt: now - 3_600_000),
            Chat(id: "chat-deploy", deviceId: "dev-vps", title: "Wrangler deploy hygiene",
                 archived: false, cwd: edge.path, branch: nil, checkoutId: nil,
                 config: claude, lastMessagePreview: "Hibernation-safe flush timer",
                 lastMessageAt: now - 86_400_000, createdAt: now - 86_400_000 * 2,
                 spaceId: edge.id, lastSeenAt: now - 86_400_000),
        ]
        let sessions: [String: SessionRow] = [
            "chat-veil": SessionRow(chatId: "chat-veil", deviceId: "dev-mac", status: .working,
                                    startedAt: now - 95_000, updatedAt: now - 5_000),
            "chat-picker": SessionRow(chatId: "chat-picker", deviceId: "dev-mac",
                                      status: .awaitingInput, startedAt: now - 400_000,
                                      updatedAt: now - 10_000),
            // No board row points here; being the pinned orchestrator, its
            // live state shows on the slot above Spaces (gh#122).
            // No board row points here, so it is an unmanaged Active row
            // (gh#117) — bare title, no chip.
            "chat-orchestrator": SessionRow(chatId: "chat-orchestrator", deviceId: "dev-mac",
                                            status: .working, startedAt: now - 6_600_000,
                                            updatedAt: now - 2_000),
        ]
        let dataset = DemoDataset(devices: [mac, vps], spaces: [comet, edge],
                                  chats: chats, sessions: sessions)
        dataset.boardRows = boardDemoRows(now: now)
        return dataset
    }

    // MARK: Board (offline)

    /// What `ListBoardRuntimes` answers on a box with the usual harnesses.
    static let runtimes: [BoardRuntime] = [
        BoardRuntime(name: "claude-code", label: "Claude Code", harness: "claude-code"),
        BoardRuntime(name: "opencode", label: "OpenCode", harness: "opencode"),
        BoardRuntime(name: "codex", label: "Codex", harness: "codex"),
    ]

    /// Two saved logins on the box, one of them somebody else's — the case the
    /// billing chips exist for (gh#101). MOCK data: not a real account.
    static let accounts: [BoardAccount] = [
        BoardAccount(id: "slot-brede", harness: "claude-code", email: "brede@tally.no",
                     planLabel: "Max", active: true, displayName: nil),
        BoardAccount(id: "slot-ana", harness: "claude-code", email: "ana@tally.no",
                     planLabel: "Max", active: false, displayName: nil),
        BoardAccount(id: "slot-codex", harness: "codex", email: "brede@tally.no",
                     planLabel: "Pro", active: true, displayName: nil),
    ]

    /// The repo picker's world (gh#118), demo edition: the box hosts the board,
    /// two of its spaces are checkouts, and the App can see a third repo nobody
    /// has connected yet — which is the row the whole feature exists for.
    ///
    /// MOCK data: not real repos, and `bredebjorhovd/itsm-agent` is here as a
    /// name to tap, not as a claim about anybody's GitHub.
    static let repoLinks: [SpaceSlug] = [
        SpaceSlug(spaceId: "space-comet", slug: "bredebjorhovd/comet-board"),
        SpaceSlug(spaceId: "space-edge", slug: "bredebjorhovd/comet-edge"),
    ]
    static let repoOffers: [RepoOffer] = [
        RepoOffer(slug: "bredebjorhovd/comet-board", private: false, archived: false,
                  missing: nil),
        RepoOffer(slug: "bredebjorhovd/comet-edge", private: false, archived: false,
                  missing: nil),
        RepoOffer(slug: "bredebjorhovd/itsm-agent", private: true, archived: false,
                  missing: "both"),
    ]

    /// Onboard, demo edition: mint the space the box would have made, so the
    /// picker's exit — standing in a repo you just connected — is explorable
    /// with no infrastructure at all.
    func onboard(slug: String) -> Onboarded {
        let name = slug.split(separator: "/").last.map(String.init) ?? slug
        let device = devices.first { $0.platform != "ios" }?.id ?? "dev-vps"
        let path = "/srv/repos/\(name)"
        if let existing = spaces.first(where: { $0.deviceId == device && $0.path == path }) {
            return Onboarded(slug: slug, deviceId: device, path: path,
                             spaceId: existing.id, spaceName: existing.displayName)
        }
        let id = "space-\(UUID().uuidString.lowercased().prefix(8))"
        spaces.append(Space(id: id, deviceId: device, path: path, name: nil,
                            gitDetected: true, gitCheckedAt: nowMs(), checkoutId: nil,
                            createdAt: nowMs()))
        return Onboarded(slug: slug, deviceId: device, path: path,
                         spaceId: id, spaceName: name)
    }

    /// Release a demo row: flip it to `working` on a fresh chat in the routed
    /// space, so the board, the Agents section and the transcript all agree.
    func dispatch(taskId: String, runtime: String?, account: String?) -> String? {
        guard let ix = boardRows.firstIndex(where: { $0.id == taskId }) else { return nil }
        let row = boardRows[ix]
        let space = spaces.first { $0.displayName == row.workspace } ?? spaces.first
        guard let space else { return nil }
        let chatId = "chat-\(UUID().uuidString.lowercased().prefix(8))"
        let branch = row.branch ?? row.identifier.lowercased()
            .replacingOccurrences(of: "#", with: "-")
        chats.append(Chat(id: chatId, deviceId: space.deviceId, title: row.title,
                          archived: false, cwd: space.path, branch: branch, checkoutId: nil,
                          config: ChatConfig(harness: runtime ?? row.runtime ?? "claude-code",
                                             model: nil, reasoning: nil,
                                             sandbox: "workspace-write"),
                          lastMessagePreview: nil, lastMessageAt: nil, createdAt: nowMs(),
                          spaceId: space.id, lastSeenAt: nowMs()))
        sessions[chatId] = SessionRow(chatId: chatId, deviceId: space.deviceId,
                                      status: .working, startedAt: nowMs(), updatedAt: nowMs())
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime]
        let stamp = formatter.string(from: Date())
        var updated = row
        updated.state = BoardState.working.rawValue
        updated.chatId = chatId
        updated.branch = branch
        updated.runtime = runtime ?? row.runtime
        updated.account = account ?? row.account
        updated.attempts += 1
        updated.startedAt = stamp
        updated.updatedAt = stamp
        boardRows[ix] = updated
        return chatId
    }

    /// End a demo attempt: the row derives back to `ready` and the chat is
    /// archived — cancel ends attempts, never tasks.
    func cancelAttempt(taskId: String) {
        guard let ix = boardRows.firstIndex(where: { $0.id == taskId }) else { return }
        if let chatId = boardRows[ix].chatId,
           let chatIx = chats.firstIndex(where: { $0.id == chatId }) {
            chats[chatIx].archived = true
            sessions[chatId] = nil
        }
        boardRows[ix].state = BoardState.ready.rawValue
        boardRows[ix].chatId = nil
        boardRows[ix].startedAt = nil
        boardRows[ix].lastOutcome = "cancelled"
    }

    /// The issue text the demo's detail sheet reads (gh#132).
    ///
    /// Only some rows have one, on purpose: an issue with no description is the
    /// common case on a real board, and the sheet's empty state should be
    /// reachable in the demo rather than only in production.
    static let boardBodies: [String: String] = [
        "linear:AGE-14": """
            The picker asks the host for its catalog on open. On a host that has \
            just come up the answer is empty, and the picker caches the empty \
            list for the session.

            - repro: restart the box, open the picker within ~10s
            - expected: a second ask once the harness has answered
            """,
        "gh:comet#121": """
            The board is readable on the phone but not actionable: dispatching \
            still means opening a laptop.

            **Wanted:** the account picker as a sheet, and a release that names \
            who is paying for it.
            """,
    ]

    /// One row per board state that has anything to say, in board order. The
    /// two live attempts point at real demo chats so `agentRows` keeps them
    /// (its membership rule drops a row whose chat has not synced).
    private static func boardDemoRows(now: Int64) -> [TaskRow] {
        func stamp(_ msAgo: Int64) -> String {
            let formatter = ISO8601DateFormatter()
            formatter.formatOptions = [.withInternetDateTime]
            return formatter.string(from: Date(timeIntervalSince1970: Double(now - msAgo) / 1000))
        }
        return [
            TaskRow(id: "linear:AGE-14", identifier: "AGE-14",
                    title: "Model picker catalog sync stalls on cold host",
                    state: .blocked, source: "linear", route: "comet-native",
                    workspace: "comet-native", runtime: "claude-code",
                    chatId: "chat-picker", branch: "fix/catalog-sync",
                    attempts: 1, updatedAt: stamp(400_000),
                    startedAt: stamp(400_000), maxDurationSecs: 7200),
            TaskRow(id: "gh:comet#118", identifier: "gh#118",
                    title: "Streaming veil on transcript rows",
                    state: .working, route: "comet-native",
                    workspace: "comet-native", runtime: "claude-code",
                    chatId: "chat-veil", branch: "veil-fade",
                    attempts: 1, updatedAt: stamp(95_000), startedAt: stamp(95_000),
                    account: "slot-ana", dispatchedByUser: "brede@tally.no",
                    billedTo: "ana@tally.no", maxDurationSecs: 7200),
            TaskRow(id: "gh:comet#121", identifier: "gh#121",
                    title: "Board rows on the phone, dispatch from a parking lot",
                    state: .ready, route: "comet-native", workspace: "comet-native",
                    runtime: "claude-code", updatedAt: stamp(3_600_000)),
            TaskRow(id: "gh:edge#42", identifier: "gh#42",
                    title: "Hibernation-safe flush timer for SessionRoom",
                    state: .ready, dispatchable: false, route: nil,
                    updatedAt: stamp(7_200_000)),
            TaskRow(id: "gh:comet#117", identifier: "gh#117",
                    title: "Tool group header colors",
                    state: .review, route: "comet-native", workspace: "comet-native",
                    runtime: "codex", prUrl: "https://github.com/x/y/pull/117",
                    prNumber: 117, branch: "fix/tool-colors",
                    lastOutcome: "done", attempts: 1, updatedAt: stamp(900_000)),
            TaskRow(id: "gh:edge#39", identifier: "gh#39",
                    title: "Wrangler deploy hygiene",
                    state: .failed, route: "edge", workspace: "edge",
                    runtime: "claude-code", branch: "deploy-hygiene",
                    lastOutcome: "failed", attempts: 2, updatedAt: stamp(86_400_000)),
            TaskRow(id: "gh:comet#110", identifier: "gh#110",
                    title: "Pin one chat as the orchestrator",
                    state: .done, route: "comet-native", workspace: "comet-native",
                    runtime: "claude-code", lastOutcome: "done", attempts: 1,
                    updatedAt: stamp(5_400_000)),
        ]
    }

    // MARK: Fake filesystem (folder browser demo)

    static let fileTree: [String: [String]] = [
        "/Users/dev": ["Documents", "Downloads", "Projects", "scratch"],
        "/Users/dev/Documents": ["notes", "specs"],
        "/Users/dev/Projects": ["comet-native", "dotfiles", "blog", "playground"],
        "/Users/dev/Projects/comet-native": ["apps", "crates", "docs", "edge"],
        "/Users/dev/Projects/blog": ["content", "public"],
        "/srv": ["deploys", "backups"],
        "/srv/deploys": ["edge", "landing"],
    ]

    func homePath(deviceId: String) -> String {
        deviceId == "dev-vps" ? "/srv" : "/Users/dev"
    }

    private static let repoNames: Set<String> = ["comet-native", "dotfiles", "blog", "playground", "edge", "landing"]

    func listFolders(deviceId: String, path: String) -> FolderListing {
        let entries = (Self.fileTree[path] ?? []).map { name in
            FolderEntry(name: name, isDir: true, isRepo: Self.repoNames.contains(name))
        }
        return FolderListing(path: path, entries: entries, truncated: false)
    }

    private var refsByPath: [String: [RepoRef]] = [:]

    func listRefs(spacePath: String) -> [RepoRef] {
        if let cached = refsByPath[spacePath] { return cached }
        let seeded: [RepoRef]
        if spacePath.contains("comet-native") {
            seeded = [
                RepoRef(name: "main", current: true, worktreePath: nil),
                RepoRef(name: "veil-fade", current: false,
                        worktreePath: "/Users/dev/.comet-native/worktrees/comet-native-veil-fade"),
                RepoRef(name: "feature/diff-pane", current: false, worktreePath: nil),
                RepoRef(name: "fix/tool-colors", current: false, worktreePath: nil),
            ]
        } else {
            seeded = [
                RepoRef(name: "main", current: true, worktreePath: nil),
                RepoRef(name: "staging", current: false, worktreePath: nil),
            ]
        }
        refsByPath[spacePath] = seeded
        return seeded
    }

    /// git checkout simulation: move the `current` marker in the repo at path.
    func switchRef(path: String, refName: String) {
        var refs = listRefs(spacePath: path)
        for ix in refs.indices {
            refs[ix].current = refs[ix].name == refName
        }
        refsByPath[path] = refs
    }

    func createWorktree(spacePath: String, base: String) -> String {
        let slug = base.replacingOccurrences(of: "/", with: "-")
        let path = "/Users/dev/.comet-native/worktrees/\((spacePath as NSString).lastPathComponent)-\(slug)"
        var refs = listRefs(spacePath: spacePath)
        if let ix = refs.firstIndex(where: { $0.name == base }), refs[ix].worktreePath == nil {
            refs[ix].worktreePath = path
        }
        refsByPath[spacePath] = refs
        return path
    }

    func sessionStore(for chatId: String) -> SessionStore {
        if let existing = stores[chatId] { return existing }
        let store = SessionStore(chatId: chatId, config: Self.dummyConfig, offline: true)
        store.setEntries(Self.transcript(for: chatId))
        store.demoResponder = { [weak self, weak store] prompt in
            guard let self, let store else { return }
            self.simulateTurn(store: store, chatId: chatId, prompt: prompt)
        }
        stores[chatId] = store
        return store
    }

    // MARK: Scripted transcripts

    private static func transcript(for chatId: String) -> [MessageEntry] {
        let now = nowMs()
        switch chatId {
        case "chat-veil":
            return [
                MessageEntry(id: "m1", role: .user, parts: [
                    .text(id: "t0", text: "Port the streaming fade-in veil from the desktop transcript. It must never affect layout — opacity only, split at chunk boundaries."),
                ], createdAt: now - 3_500_000, deviceId: "ios-demo", status: .complete, continuationOf: nil),
                MessageEntry(id: "m2", role: .assistant, parts: [
                    .text(id: "t0", text: """
                    ## Veil port plan

                    The desktop veil (`veil.rs`) multiplies a fading alpha into each appended \
                    chunk's text color — **paint-layer only**, so shaping and wrapping never change. \
                    Three invariants to carry over:

                    1. Chunk spans keep their *exact* byte length when split
                    2. Fade duration tracks the append cadence: `clamp(ema × 3, 120, 400)` ms
                    3. Re-attach seeds the baseline — only post-switch appends animate

                    | Constant | Value |
                    | --- | --- |
                    | `VEIL_MIN_FADE_MS` | 120 |
                    | `VEIL_MAX_FADE_MS` | 400 |
                    | `VEIL_CURVE_POW` | 1.6 |

                    > The curve is `1 − (1−p)^1.6` — fast attack, soft landing.
                    """),
                    .tool(id: "tool1", call: RenderToolCall(tag: "readFile", fields: ["path": "crates/ui/src/markdown/veil.rs"]), isError: false, resolved: true),
                    .tool(id: "tool2", call: RenderToolCall(tag: "editFile", fields: ["path": "Comet/Transcript/Veil.swift"]), isError: false, resolved: true),
                    .tool(id: "tool3", call: RenderToolCall(tag: "exec", fields: ["command": "xcodebuild -scheme Comet build"]), isError: false, resolved: true),
                    .text(id: "t1", text: """
                    Implementation lands in `Veil.swift`:

                    ```swift
                    func veilOpacity(_ p: Double) -> Double {
                        1 - pow(1 - p, 1.6)  // fast attack, soft landing
                    }

                    // Duration follows the streaming cadence EMA.
                    let duration = min(max(ema * 3, 120), 400)
                    ```

                    The row keeps one `RowVeil` while streaming and drops it on the \
                    live→complete flip, exactly like the desktop lifecycle.
                    """),
                ], createdAt: now - 3_400_000, deviceId: "dev-mac", status: .complete, continuationOf: nil),
            ]
        case "chat-picker":
            return [
                MessageEntry(id: "m1", role: .user, parts: [
                    .text(id: "t0", text: "The model picker shows stale catalogs after switching devices — where should the catalog come from?"),
                ], createdAt: now - 400_000, deviceId: "ios-demo", status: .complete, continuationOf: nil),
                MessageEntry(id: "m2", role: .assistant, parts: [
                    .text(id: "t0", text: "Two viable sources — the local device's harness install, or the space's owning device. The desktop recently moved to the latter (`aa128a6`). Before I wire the RPC, one decision:"),
                    .input(id: "req-1", requestId: "req-1", questions: [
                        UserInputQuestion(id: "q1", header: "Catalog source",
                                          question: "Which device should serve harness/model catalogs for the picker?",
                                          options: [
                                            "Space's device (Recommended)",
                                            "Local device",
                                            "Union of both",
                                          ], multiSelect: false),
                    ], resolved: false),
                ], createdAt: now - 380_000, deviceId: "dev-mac", status: .complete, continuationOf: nil),
            ]
        case "chat-tabs":
            return [
                MessageEntry(id: "m1", role: .user, parts: [
                    .text(id: "t0", text: "Tool group headers turn red when any child fails — they should stay quiet, chips carry the error."),
                ], createdAt: now - 1_000_000, deviceId: "ios-demo", status: .complete, continuationOf: nil),
                MessageEntry(id: "m2", role: .assistant, parts: [
                    .tool(id: "tool1", call: RenderToolCall(tag: "search", fields: ["pattern": "group_header_color"]), isError: false, resolved: true),
                    .tool(id: "tool2", call: RenderToolCall(tag: "exec", fields: ["command": "cargo test -p comet-ui tool_group"]), isError: true, resolved: true),
                    .tool(id: "tool3", call: RenderToolCall(tag: "editFile", fields: ["path": "crates/ui/src/shell/transcript.rs"]), isError: false, resolved: true),
                    .text(id: "t0", text: "Done — the header keeps `text_muted` even on failure; only the chip label and the summary segment (\"1 failed\") pick up `danger`. Matches the desktop fix in `1749890`."),
                ], createdAt: now - 950_000, deviceId: "dev-mac", status: .complete, continuationOf: nil),
            ]
        case "chat-deploy":
            return [
                MessageEntry(id: "m1", role: .user, parts: [
                    .text(id: "t0", text: "Audit the wrangler config for hibernation hygiene."),
                ], createdAt: now - 86_500_000, deviceId: "ios-demo", status: .complete, continuationOf: nil),
                MessageEntry(id: "m2", role: .assistant, parts: [
                    .text(id: "t0", text: "Flush timer now only arms while dirty; ping/pong uses the auto-response path so the DO never wakes for keepalives."),
                ], createdAt: now - 86_400_000, deviceId: "dev-vps", status: .complete, continuationOf: nil),
            ]
        default:
            return []  // freshly minted chats start empty
        }
    }

    // MARK: Streaming simulation

    private func simulateTurn(store: SessionStore, chatId: String, prompt: String) {
        streamTask?.cancel()
        let now = nowMs()
        var entries = store.entries
        entries.append(MessageEntry(id: "u-\(now)", role: .user, parts: [
            .text(id: "t0", text: prompt),
        ], createdAt: now, deviceId: "ios-demo", status: .complete, continuationOf: nil))
        let liveId = "a-\(now)"
        entries.append(MessageEntry(id: liveId, role: .assistant, parts: [
            .text(id: "t0", text: ""),
        ], createdAt: now, deviceId: "dev-mac", status: .streaming, continuationOf: nil))
        store.setEntries(entries)
        sessions[chatId] = SessionRow(chatId: chatId, deviceId: "dev-mac", status: .working,
                                      startedAt: now, updatedAt: now)

        let reply = """
        Here's how the streamed reply renders on this device:

        - Markdown re-parses **only the tail** — the last two top-level blocks
        - New text fades in through the paint-only veil
        - The transcript stays glued to the bottom until you scroll up

        ```rust
        // The desktop constant carries over verbatim.
        const STREAM_COMMIT_MS: u64 = 120;
        ```

        When the turn settles, this entry flips `streaming → complete`, the veil \
        drops, and the row ids stay stable so nothing flickers.
        """
        let words = reply.split(separator: " ", omittingEmptySubsequences: false)

        streamTask = Task { [weak self, weak store] in
            var text = ""
            for (ix, word) in words.enumerated() {
                if Task.isCancelled { return }
                text += (ix == 0 ? "" : " ") + word
                guard let store else { return }
                var current = store.entries
                guard let last = current.indices.last, current[last].id == liveId else { return }
                current[last].parts = [.text(id: "t0", text: text)]
                store.setEntries(current)
                try? await Task.sleep(nanoseconds: UInt64.random(in: 30_000_000...140_000_000))
            }
            guard let self, let store else { return }
            var current = store.entries
            if let last = current.indices.last, current[last].id == liveId {
                current[last].status = .complete
                store.setEntries(current)
            }
            let end = nowMs()
            self.sessions[chatId] = SessionRow(chatId: chatId, deviceId: "dev-mac", status: .idle,
                                               startedAt: nil, updatedAt: end)
            if let ix = self.chats.firstIndex(where: { $0.id == chatId }) {
                self.chats[ix].lastMessageAt = end
                self.chats[ix].lastMessagePreview = "When the turn settles, this entry flips…"
                self.chats[ix].lastSeenAt = end
            }
        }
    }
}
