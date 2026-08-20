// App session root: sign-in state machine, workspace connection, and the
// per-chat session store cache. Also hosts demo mode — an offline in-memory
// dataset so the UI can be exercised without an edge deployment.

import Foundation
import Observation
import SwiftUI

@MainActor
@Observable
final class AppModel {
    enum Phase {
        case signedOut
        case pickingOrg(AuthTokens, [AuthOrg])
        case ready
    }

    var phase: Phase = .signedOut
    var workspace: WorkspaceStore?
    var board: BoardStore?
    var demo: DemoDataset?
    private var sessionStores: [String: SessionStore] = [:]
    private var config: AppConfig?

    // Persisted connection settings.
    @ObservationIgnored @AppStorage("edgeURL") var edgeURLString = "https://edge.comet.offhand.dev"
    @ObservationIgnored @AppStorage("authMode") var authModeRaw = AppConfig.Mode.workos.rawValue
    @ObservationIgnored @AppStorage("userId") var storedUserId = ""
    @ObservationIgnored @AppStorage("orgId") var storedOrgId = ""
    @ObservationIgnored @AppStorage("deviceId") var storedDeviceId = ""
    /// The signed-in email, when WorkOS gave us one. Only a board dispatch
    /// reads it (gh#74's `viaUser`): a slot id or a WorkOS user id is not what
    /// somebody reading an attempt row is looking for.
    @ObservationIgnored @AppStorage("userEmail") var storedUserEmail = ""

    var deviceId: String {
        if storedDeviceId.isEmpty {
            storedDeviceId = "ios-" + UUID().uuidString.lowercased().prefix(8)
        }
        return storedDeviceId
    }

    var deviceName: String {
        UIDevice.current.name
    }

    /// Deep-link target applied by HomeView on first appearance (set by launch
    /// args in demo mode; simulator-driven screenshots use it).
    var launchRoute: Route?
    /// Screenshot rig: "newsession" / "newspace" presents that sheet on arrival.
    var launchSheet: String?
    /// Screenshot rig: auto-send a canned prompt from the new-session canvas.
    var launchAutosend = false

    func restore() {
        if demo != nil { return }
        DocDisk.prune(keep: 80)
        let args = ProcessInfo.processInfo.arguments
        // Debug-rig config overrides (cfprefsd caching defeats external
        // defaults writes; the app applying them itself always sticks).
        func override(_ flag: String, _ apply: (String) -> Void) {
            if let ix = args.firstIndex(of: flag), ix + 1 < args.count {
                apply(args[ix + 1])
            }
        }
        override("-setedge") { edgeURLString = $0 }
        override("-setmode") { authModeRaw = $0 }
        override("-setuser") { storedUserId = $0 }
        override("-setorg") { storedOrgId = $0 }
        if args.contains("-bench") {
            Task { await BenchRunner.run() }
            return
        }
        // The ported stats rules against the fixture Rust generated (gh#157).
        // No network, no session: it is arithmetic against a bundled file.
        if args.contains("-spec") {
            SpecRunner.run()
            return
        }
        if args.contains("-fork-spec") {
            ForkSpecRunner.run()
            return
        }
        // The ported review reading against the fixture Rust generated
        // (gh#256). Same shape, same reason, its own file.
        if args.contains("-review-spec") {
            ReviewSpecRunner.run()
            return
        }
        // The room's redial schedule (gh#405). Arithmetic, not a socket —
        // deliberately, since a reconnect loop cannot be honestly checked
        // against an edge that is failing every request.
        if args.contains("-sync-spec") {
            SyncSpecRunner.run()
            return
        }
        // What the clipboard and the share sheet get (gh#534). Pure string
        // building over the row model — no network, no session.
        if args.contains("-copy-spec") {
            CopySpecRunner.run()
            return
        }
        if args.contains("-e2e") {
            Task { await E2ERunner.run(model: self) }
            return
        }
        if let ix = args.firstIndex(of: "-e2e-board"), ix + 1 < args.count {
            let repo = args[ix + 1]
            Task { await E2ERunner.runBoard(model: self, repoPath: repo) }
            return
        }
        if args.contains("-e2e-live") {
            // Reuse the signed-in session, then probe the live relay paths.
            Task {
                try? await Task.sleep(nanoseconds: 500_000_000)
                await E2ERunner.runLive(model: self)
            }
            // fall through to the normal restore below
        }
        // Route and sheet are rig args, not demo args: a board screenshot has
        // to be reachable against a LIVE board too, which is where the rows
        // that matter are. The demo-only extras (`-big`, `-stream`) stay inside
        // the demo branch, since they drive the scripted dataset.
        if let ix = args.firstIndex(of: "-route"), ix + 1 < args.count {
            let spec = args[ix + 1]
            if spec.hasPrefix("chat:") {
                launchRoute = .chat(String(spec.dropFirst("chat:".count)))
            } else if spec.hasPrefix("space:") {
                launchRoute = .space(String(spec.dropFirst("space:".count)))
            } else if spec == "board" {
                launchRoute = .board
            } else if spec == "stats" {
                launchRoute = .stats
            }
        }
        if let ix = args.firstIndex(of: "-sheet"), ix + 1 < args.count {
            launchSheet = args[ix + 1]
        }
        if args.contains("-demo") {
            enterDemoMode()
            if case .chat(let chatId)? = launchRoute, let demo {
                if args.contains("-big") {
                    // Scroll-settle stress. Injected BEFORE the transcript
                    // appears, which is the warm-session case: rows are already
                    // there at first layout, so neither the rows-arrived nor
                    // the streamed-growth anchor ever fires and `.task` is the
                    // only thing holding the bottom — against hundreds of
                    // lazily-estimated rows.
                    demo.sessionStore(for: chatId)
                        .setEntries(BenchRunner.syntheticEntries(turns: 120))
                }
                if args.contains("-stream") {
                    // Screenshot rig: kick off the scripted streaming reply.
                    let store = demo.sessionStore(for: chatId)
                    Task { @MainActor in
                        try? await Task.sleep(nanoseconds: 2_000_000_000)
                        store.demoResponder?("Show me the streamed reply path.")
                    }
                }
            }
            launchAutosend = args.contains("-autosend")
            return
        }
        guard let url = URL(string: edgeURLString), !storedUserId.isEmpty, !storedOrgId.isEmpty else {
            return
        }
        let mode = AppConfig.Mode(rawValue: authModeRaw) ?? .workos
        switch mode {
        case .dev:
            connect(url: url, mode: .dev, userId: storedUserId, orgId: storedOrgId,
                    tokens: nil, devBearer: devBearer(userId: storedUserId, orgId: storedOrgId))
        case .workos:
            guard let access = Keychain.load(key: "accessToken"),
                  let refresh = Keychain.load(key: "refreshToken") else { return }
            connect(url: url, mode: .workos, userId: storedUserId, orgId: storedOrgId,
                    tokens: AuthTokens(accessToken: access, refreshToken: refresh), devBearer: nil)
        }
    }

    // MARK: Sign-in flows

    /// WorkOS paste-code exchange. Returns the org list for the picker (or
    /// connects straight away when exactly one org exists).
    func signIn(edgeURL: URL, code: String) async throws {
        let client = AuthClient(baseURL: edgeURL)
        let (user, tokens) = try await client.exchange(code: code)
        edgeURLString = edgeURL.absoluteString
        authModeRaw = AppConfig.Mode.workos.rawValue
        storedUserId = user.id
        storedUserEmail = user.email ?? ""
        let orgs = try await client.orgs(accessToken: tokens.accessToken)
        if let only = orgs.first, orgs.count == 1 {
            try await selectOrg(only, tokens: tokens)
        } else if orgs.isEmpty {
            throw AuthError.http(403, "No organizations for this account")
        } else {
            phase = .pickingOrg(tokens, orgs)
        }
    }

    func selectOrg(_ org: AuthOrg, tokens: AuthTokens) async throws {
        guard let url = URL(string: edgeURLString) else { return }
        // Re-scope the access token to the org (adds the org_id claim).
        let client = AuthClient(baseURL: url)
        let scoped = try await client.refresh(refreshToken: tokens.refreshToken,
                                              organizationId: org.organizationId)
        Keychain.save(scoped.accessToken, key: "accessToken")
        Keychain.save(scoped.refreshToken, key: "refreshToken")
        storedOrgId = org.organizationId
        connect(url: url, mode: .workos, userId: storedUserId, orgId: org.organizationId,
                tokens: scoped, devBearer: nil)
    }

    /// Dev-mode edge (AUTH_MODE=dev): bearer = "userId@orgId".
    func signInDev(edgeURL: URL, userId: String, orgId: String) {
        edgeURLString = edgeURL.absoluteString
        authModeRaw = AppConfig.Mode.dev.rawValue
        storedUserId = userId
        storedOrgId = orgId
        connect(url: edgeURL, mode: .dev, userId: userId, orgId: orgId,
                tokens: nil, devBearer: devBearer(userId: userId, orgId: orgId))
    }

    func enterDemoMode() {
        demo = DemoDataset.standard()
        phase = .ready
    }

    func signOut() {
        workspace?.stop()
        workspace = nil
        board?.stop()
        board = nil
        sessionStores.values.forEach { $0.stop() }
        sessionStores.removeAll()
        config = nil
        demo = nil
        Keychain.delete(key: "accessToken")
        Keychain.delete(key: "refreshToken")
        DocDisk.wipeAll()  // local doc state belongs to the signed-in identity
        storedUserId = ""
        storedUserEmail = ""
        storedOrgId = ""
        phase = .signedOut
    }

    private func devBearer(userId: String, orgId: String) -> String {
        orgId.isEmpty ? userId : "\(userId)@\(orgId)"
    }

    private func connect(url: URL, mode: AppConfig.Mode, userId: String, orgId: String,
                         tokens: AuthTokens?, devBearer: String?) {
        let config = AppConfig(edgeURL: url, mode: mode, userId: userId, orgId: orgId,
                               deviceId: deviceId, deviceName: deviceName,
                               tokens: tokens, devBearer: devBearer)
        self.config = config
        let store = WorkspaceStore(config: config)
        workspace = store
        store.start()
        // Standing, not opened with the board screen: the Agents section is
        // presence, and presence that only works after you have visited the
        // board is not presence (gh#103's correction to the desktop panel).
        let boardStore = BoardStore(config: config) { [weak store] in store?.devices ?? [] }
        board = boardStore
        boardStore.start()
        phase = .ready
    }

    // MARK: Unified data accessors (demo or live — one path for views)

    var spaces: [Space] { demo?.spaces ?? workspace?.spaces ?? [] }

    var connected: Bool { demo != nil || workspace?.connected == true }

    var overviewChats: [Chat] {
        if let demo {
            let liveIds = Set(demo.spaces.map(\.id))
            let live = demo.chats.filter { !$0.archived && $0.spaceId.map(liveIds.contains) == true }
            return sortActive(live)
        }
        return workspace?.overviewChats ?? []
    }

    /// Every chat this app holds, archived ones included — the join
    /// `activePlacements` needs, since an attempt names an issue and not a
    /// folder, and a chat that is working anyway may well be archived.
    var allChats: [Chat] { demo?.chats ?? workspace?.chats ?? [] }

    /// Where each Active row lives (gh#138): what the lists below subtract by,
    /// so a chat draws one full row — Active's while it runs, its own when idle.
    var activeRowPlacements: [(chatId: String, spaceId: String?)] {
        activePlacements(activeChats, chats: allChats)
    }

    /// Row titles, made unique within each device's spaces (gh#138): a repo
    /// slug names a repo, and one machine can hold several checkouts of it.
    ///
    /// Split into base and qualifier (gh#144), never glued: the row elides from
    /// the right, so a single string loses the disambiguating tail first and
    /// two rows for one repo read identically again.
    var spaceTitlesById: [String: SpaceTitle] {
        var out: [String: SpaceTitle] = [:]
        for (_, group) in Dictionary(grouping: spaces, by: \.deviceId) {
            let titles = spaceTitles(group)
            for (ix, space) in group.enumerated() { out[space.id] = titles[ix] }
        }
        return out
    }

    /// How many of a space's chats the Active group is drawing above it.
    func spaceRunning(_ spaceId: String) -> Int {
        activeRowPlacements.filter { $0.spaceId == spaceId }.count
    }

    func chats(in spaceId: String) -> [Chat] {
        if let demo {
            return sortActive(demo.chats.filter { !$0.archived && $0.spaceId == spaceId })
        }
        return workspace?.chats(in: spaceId) ?? []
    }

    func chat(id: String) -> Chat? {
        (demo?.chats ?? workspace?.chats)?.first { $0.id == id }
    }

    /// state.rs `space_for_chat` — nil for a dangling/missing space_id.
    func space(for chat: Chat) -> Space? {
        guard let spaceId = chat.spaceId else { return nil }
        return spaces.first { $0.id == spaceId }
    }

    func indicator(for chat: Chat) -> ChatIndicator {
        if let demo {
            return chatIndicator(chat: chat, live: effectiveStatus(demo.sessions[chat.id], now: nowMs()))
        }
        return workspace?.indicator(for: chat) ?? .idle
    }

    func spaceIndicator(_ spaceId: String) -> ChatIndicator? {
        chats(in: spaceId).map { indicator(for: $0) }.min { $0.rawValue < $1.rawValue }
    }

    // MARK: Board (gh#114)

    /// The board's rows. Empty in every case that is not "a host answered":
    /// no board in the org, the sweep still running, demo mode without rows.
    var boardRows: [TaskRow] { demo?.boardRows ?? board?.rows ?? [] }

    /// Why the board is empty, when it is — nil while it is fine.
    var boardStatus: String? { demo != nil ? nil : board?.status }

    var boardAttached: Bool { demo != nil || board?.attached == true }

    /// The device the board sweep settled on, once it has. Read by surfaces
    /// that ASK the board something rather than watch it: a screen opened
    /// before the sweep answered has to know when to try again, and this
    /// changing is that moment.
    var boardHostDeviceId: String? {
        if demo != nil { return demo?.devices.first { $0.platform != "ios" }?.id }
        return board?.hostDeviceId
    }

    /// The home screen's Active group (gh#123): everything alive, most urgent
    /// first — live board attempts (gh#103) and the working chats no attempt
    /// accounts for (gh#117), one list. Joins the three standing streams the
    /// app already holds; the board rows are read only to subtract the
    /// attempts, so a phone attached to no board at all still answers.
    var activeChats: [ActiveRow] {
        let sessions = demo?.sessions ?? workspace?.sessions ?? [:]
        let chats = demo?.chats ?? workspace?.chats ?? []
        return activeRows(rows: boardRows, chats: chats, sessions: sessions,
                          orchestrator: orchestratorChatId)
    }

    /// Which chat the board has pinned as its orchestrator (gh#104), off the
    /// host the board sweep settled on.
    var orchestratorChatId: String? {
        demo != nil ? demo?.orchestratorChatId : board?.orchestratorChatId
    }

    /// What to call the pinned chat when a *different* chat is about to take
    /// the pin from it (gh#166) — the pin is one key, so saying yes moves it.
    ///
    /// A pin whose chat has not synced to this phone still has to be nameable:
    /// the operator is being told what they are about to displace, and "nothing
    /// is pinned" would be the one wrong answer.
    var pinnedOrchestratorName: String? {
        guard let pin = orchestratorChatId else { return nil }
        return chat(id: pin)?.displayTitle ?? orchestratorName
    }

    /// Whether the board dispatched this chat — an attempt of its own.
    ///
    /// The one thing `comet-board doctor` says can be wrong with a pin: an
    /// attempt holds a workspace slot and is exempted from its own time cap by
    /// being pinned. Rows the board never released answer false, which includes
    /// every chat on a phone attached to no board at all.
    func boardDispatched(chatId: String) -> Bool {
        boardRows.contains { $0.chatId == chatId }
    }

    /// The "Needs you" inbox (gh#122): everything waiting on a human, most
    /// owed first, joined from the four streams the app already holds.
    var needsYouRows: [NeedRow] {
        let sessions = demo?.sessions ?? workspace?.sessions ?? [:]
        let chats = demo?.chats ?? workspace?.chats ?? []
        return needsYou(orchestrator: orchestratorChatId, rows: boardRows,
                        chats: chats, sessions: sessions)
    }

    /// The orchestrator's pinned slot (gh#122), or `nil` when none is pinned.
    var orchestratorSlotRow: OrchestratorSlot? {
        let sessions = demo?.sessions ?? workspace?.sessions ?? [:]
        let chats = demo?.chats ?? workspace?.chats ?? []
        return orchestratorSlot(orchestrator: orchestratorChatId,
                                chats: chats, sessions: sessions)
    }

    /// The runtimes and logins a dispatch picker offers. Both belong to the
    /// board's HOST — the run executes over there, so the catalog it picks from
    /// and the subscription it can spend are the box's, never the phone's.
    var boardRuntimes: [BoardRuntime] { demo != nil ? DemoDataset.runtimes : (board?.runtimes ?? []) }
    var boardAccounts: [BoardAccount] { demo != nil ? DemoDataset.accounts : (board?.accounts ?? []) }

    /// The email a dispatch claims as its releaser (gh#74's `viaUser`) and the
    /// billing chips compare against. A claim, never authority — the box cannot
    /// check it, and it is not a reason to spend anybody's subscription.
    ///
    /// Demo mode answers with the dataset's own operator, so the cross-billed
    /// case the chips exist for is explorable with no infrastructure.
    var viewerEmail: String? {
        if demo != nil { return "brede@tally.no" }
        return storedUserEmail.isEmpty ? nil : storedUserEmail
    }

    func boardAccounts(forHarness harness: String?) -> [BoardAccount] {
        guard let harness, !harness.isEmpty else { return [] }
        return boardAccounts.filter { $0.harness == harness }
    }

    func dispatchBoardTask(taskId: String, runtime: String?, account: BoardAccount?,
                           replace: Bool, bill: String?,
                           billedTo: String?) async -> BoardStore.DispatchOutcome {
        if let demo {
            try? await Task.sleep(nanoseconds: 400_000_000)
            return .dispatched(chatId: demo.dispatch(taskId: taskId, runtime: runtime,
                                                     account: account?.id))
        }
        guard let board else { return .failed("Not connected to a board") }
        return await board.dispatch(taskId: taskId, runtime: runtime, account: account,
                                    replace: replace, bill: bill, billedTo: billedTo)
    }

    /// The issue text behind one row, for the detail sheet (gh#132).
    ///
    /// An issue with no description and a read that did not happen come back
    /// differently, because a blank panel that could mean either is a panel
    /// nobody can trust.
    func boardTaskDetail(taskId: String) async -> TaskBody {
        if demo != nil {
            // Feel like a read, so the sheet's loading state is explorable.
            try? await Task.sleep(nanoseconds: 200_000_000)
            guard let body = DemoDataset.boardBodies[taskId] else { return .empty }
            return .text(body)
        }
        guard let board else { return .failed("Not connected to a board") }
        return await board.taskDetail(taskId: taskId)
    }

    /// What one attempt is worth, for the review screen (gh#256).
    ///
    /// A row with nothing to review is not an error the board reports — the
    /// call refuses with "has no attempts to review" — so the screen's own
    /// unavailable state is where that lands, and it says so in words rather
    /// than drawing an empty review.
    func attemptReview(taskId: String) async -> ReviewLoad {
        if let demo {
            // Feel like a read, so the screen's loading state is explorable.
            try? await Task.sleep(nanoseconds: 250_000_000)
            guard let review = DemoDataset.review(taskId: taskId) else {
                return .failed("This row has no attempt to review.")
            }
            let host = demo.devices.first { $0.platform != "ios" }?.id ?? "demo-board"
            return .read(LoadedAttemptReview(review: review, host: host))
        }
        guard let board else { return .failed("Not connected to a board") }
        switch await board.attemptReview(taskId: taskId) {
        case .read(let review, let host):
            return .read(LoadedAttemptReview(review: review, host: host))
        case .failed(let message): return .failed(message)
        }
    }

    /// Deliver the verdict (§gh#239). Demo mode answers with a receipt rather
    /// than pretending to post: the bar's after-state is the half of this
    /// screen a screenshot cannot otherwise reach.
    func submitVerdict(target: ReviewTarget, kind: VerdictKind,
                       comment: String) async -> BoardStore.VerdictOutcome {
        if demo != nil {
            try? await Task.sleep(nanoseconds: 400_000_000)
            return .sent(DemoDataset.receipt(target: target, kind: kind, comment: comment))
        }
        guard let board else { return .failed("Not connected to a board") }
        return await board.submitVerdict(target: target, kind: kind, comment: comment)
    }

    /// What the board did with the work it was given, over a window (gh#143).
    ///
    /// Read when the screen opens and on every window change, never streamed:
    /// a full aggregate on every board tick would cost a phone a recompute
    /// nobody is looking at — the same reason `BoardStats` is a call on every
    /// other surface.
    func boardStats(sinceDays: Int64?) async -> BoardStore.StatsOutcome {
        if demo != nil {
            try? await Task.sleep(nanoseconds: 250_000_000)  // feel like a sweep
            let id = demo?.devices.first { $0.platform != "ios" }?.id ?? "dev-vps"
            let host = StatsDevice(deviceId: id, label: deviceName(id))
            let stats = DemoDataset.stats(sinceDays: sinceDays)
            return .read(AggregateBoardStats(
                sinceDays: sinceDays,
                stats: stats,
                boards: [AggregateBoardStatsSource(
                    boardId: "demo-board", host: host, stats: stats)],
                hosts: [StatsHost(device: host, status: .answered,
                                  boardId: "demo-board", error: nil)],
                complete: true))
        }
        guard let board else { return .failed("Not connected to a board") }
        return await board.stats(sinceDays: sinceDays)
    }

    /// Pin a chat as the board's orchestrator, or unpin whatever is (gh#144).
    ///
    /// The phone's only route to `comet-board routes defaults orchestrator_chat
    /// --unset`: the slot is often the ONLY row a pinned chat has, since its
    /// session ends and its space shelf may never have listed it.
    func setOrchestrator(chatId: String?) async -> String? {
        if let demo {
            demo.orchestratorChatId = chatId
            return nil
        }
        guard let board else { return "Not connected to a board" }
        return await board.setOrchestrator(chatId: chatId)
    }

    func cancelBoardTask(taskId: String) async -> String? {
        if let demo {
            demo.cancelAttempt(taskId: taskId)
            return nil
        }
        guard let board else { return "Not connected to a board" }
        return await board.cancel(taskId: taskId)
    }

    // MARK: The repo picker (gh#118)

    /// The board hosts, and what each knows about repos. Demo mode answers with
    /// the dataset's own box, so the whole picker — including onboarding a repo
    /// that has never been connected — is explorable with no infrastructure.
    func repoHosts() async -> [BoardStore.RepoHost] {
        if let demo {
            try? await Task.sleep(nanoseconds: 250_000_000)  // feel like a sweep
            let box = demo.devices.first { $0.platform != "ios" }?.id ?? "dev-vps"
            return [BoardStore.RepoHost(deviceId: box, links: DemoDataset.repoLinks,
                                        offers: DemoDataset.repoOffers, note: nil)]
        }
        guard let board else { return [] }
        return await board.repoHosts()
    }

    /// Clone a repo onto the board's host, give it a space, and put it on the
    /// board — the gh#97 verb, run from the picker.
    func onboardRepo(slug: String, host: String) async -> BoardStore.OnboardOutcome {
        if let demo {
            try? await Task.sleep(nanoseconds: 900_000_000)  // it is a git clone
            return .connected(demo.onboard(slug: slug))
        }
        guard let board else { return .failed("Not connected to a board") }
        return await board.onboard(slug: slug, host: host)
    }

    func deviceName(_ deviceId: String) -> String {
        (demo?.devices ?? workspace?.devices)?.first { $0.id == deviceId }?.name ?? deviceId
    }

    func deviceOnline(_ deviceId: String) -> Bool {
        if let demo {
            guard let seen = demo.devices.first(where: { $0.id == deviceId })?.lastSeenAt else { return false }
            return nowMs() - seen < presenceFreshMs
        }
        return workspace?.deviceOnline(deviceId) ?? false
    }

    /// Live model catalog from the space's owning device (the desktop's
    /// "catalog source = the device that runs the session" rule); static
    /// fallback when the device is unreachable.
    func listModels(space: Space, harness: String) async -> [ModelInfo] {
        if demo != nil {
            try? await Task.sleep(nanoseconds: 100_000_000)
            return HarnessCatalog.models(for: harness)
        }
        if let live = await workspace?.listModels(deviceId: space.deviceId, harness: harness),
           !live.isEmpty {
            return live
        }
        return HarnessCatalog.models(for: harness)
    }

    /// Refs of the space's repo (git spaces only).
    func listRefs(space: Space) async -> [RepoRef]? {
        if let demo {
            try? await Task.sleep(nanoseconds: 120_000_000)
            return demo.listRefs(spacePath: space.path)
        }
        return await workspace?.listRefs(deviceId: space.deviceId, repoPath: space.path)
    }

    func searchContext(chat: Chat, query: String) async -> ContextSearch {
        guard demo == nil else { return ContextSearch(matches: [], truncated: false) }
        return await workspace?.searchContext(
            deviceId: chat.deviceId,
            chatId: chat.id,
            query: query
        ) ?? ContextSearch(matches: [], truncated: false)
    }

    /// Draft-mode checkout switch: `git checkout` in the SPACE's folder.
    /// Returns an error message, or nil on success.
    func switchSpaceRef(space: Space, refName: String) async -> String? {
        if let demo {
            try? await Task.sleep(nanoseconds: 200_000_000)
            demo.switchRef(path: space.path, refName: refName)
            return nil
        }
        guard let workspace else { return "Not connected" }
        return await workspace.switchRef(deviceId: space.deviceId,
                                         repoPath: space.path, refName: refName)
    }

    /// Mid-session ref switch (desktop switch_session_ref): retarget onto the
    /// ref's existing worktree (row writes, no git), else checkout in the
    /// session's own cwd on the host. Returns an error message or nil.
    func switchSessionRef(chat: Chat, ref: RepoRef) async -> String? {
        guard let cwd = chat.cwd else { return "Session has no working folder" }
        if let worktree = ref.worktreePath {
            if worktree == cwd { return nil }  // already here
            if let demo {
                if let ix = demo.chats.firstIndex(where: { $0.id == chat.id }) {
                    demo.chats[ix].cwd = worktree
                    demo.chats[ix].branch = ref.name
                }
                return nil
            }
            workspace?.setChatCheckout(chatId: chat.id, cwd: worktree, branch: ref.name)
            return nil
        }
        if let demo {
            try? await Task.sleep(nanoseconds: 200_000_000)
            demo.switchRef(path: cwd, refName: ref.name)
            if let ix = demo.chats.firstIndex(where: { $0.id == chat.id }) {
                demo.chats[ix].branch = ref.name
            }
            return nil
        }
        guard let workspace else { return "Not connected" }
        let error = await workspace.switchRef(deviceId: chat.deviceId,
                                              repoPath: cwd, refName: ref.name)
        if error == nil {
            // The host's HEAD watcher reconciles chat.branch eventually;
            // stamp it optimistically so the UI answers immediately.
            workspace.setChatCheckout(chatId: chat.id, cwd: cwd, branch: ref.name)
        }
        return error
    }

    /// CreateWorktree off the base ref; returns the new worktree's path.
    func createWorktree(space: Space, base: String) async -> String? {
        if let demo {
            try? await Task.sleep(nanoseconds: 250_000_000)
            return demo.createWorktree(spacePath: space.path, base: base)
        }
        return await workspace?.createWorktree(deviceId: space.deviceId,
                                               repoPath: space.path, branch: base)
    }

    @discardableResult
    func createChat(space: Space, config chatConfig: ChatConfig,
                    branch: String? = nil, cwd: String? = nil) -> String? {
        if let demo {
            let id = "chat-\(UUID().uuidString.lowercased().prefix(8))"
            demo.chats.append(Chat(id: id, deviceId: space.deviceId, title: nil, archived: false,
                                   cwd: cwd ?? space.path, branch: branch, checkoutId: nil,
                                   config: chatConfig, lastMessagePreview: nil, lastMessageAt: nil,
                                   createdAt: nowMs(), spaceId: space.id, lastSeenAt: nowMs()))
            return id
        }
        return workspace?.createChat(space: space, config: chatConfig, branch: branch, cwd: cwd)
    }

    /// Browse folders on a remote device (the desktop add-space palette's data
    /// path). Demo mode serves a canned tree; live mode asks the device over
    /// the relay.
    func listFolders(deviceId: String, path: String?) async -> FolderListing? {
        if let demo {
            try? await Task.sleep(nanoseconds: 120_000_000)  // feel like a network hop
            let target = path ?? demo.homePath(deviceId: deviceId)
            return demo.listFolders(deviceId: deviceId, path: target)
        }
        return await workspace?.listFolders(deviceId: deviceId, path: path)
    }

    @discardableResult
    func createSpace(deviceId: String, path: String, gitDetected: Bool = false) async -> String? {
        if let demo {
            if let existing = demo.spaces.first(where: { $0.deviceId == deviceId && $0.path == path }) {
                return existing.id
            }
            let id = "space-\(UUID().uuidString.lowercased().prefix(8))"
            demo.spaces.append(Space(id: id, deviceId: deviceId, path: path, name: nil,
                                     gitDetected: gitDetected, gitCheckedAt: nil, checkoutId: nil,
                                     createdAt: nowMs()))
            return id
        }
        return await workspace?.createSpace(deviceId: deviceId, path: path, gitDetected: gitDetected)
    }

    func archive(chatId: String) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].archived = true
            }
            return
        }
        workspace?.setArchived(chatId: chatId, archived: true)
    }

    func setChatConfig(chatId: String, config: ChatConfig) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].config = config
            }
            return
        }
        workspace?.setChatConfig(chatId: chatId, config: config)
    }

    func markSeen(chatId: String) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].lastSeenAt = nowMs()
            }
            return
        }
        workspace?.markSeen(chatId: chatId)
    }

    /// Persist every open doc now (app backgrounding).
    func flushDocs() {
        workspace?.flushToDisk()
        sessionStores.values.forEach { $0.flushToDisk() }
    }

    /// Somebody is looking: ask the workspace room who it can see (gh#145).
    ///
    /// Presence stopped being pushed on a timer, because that timer kept the
    /// room's Durable Object permanently awake. What replaced it is this — the
    /// poll driven by a human opening the app, at a human's cadence, plus the
    /// room volunteering an answer whenever a device joins or leaves.
    func refreshPresence() {
        guard let workspace else { return }
        Task { await workspace.refreshPresence() }
    }

    /// Diagnostics access (live e2e probe).
    var diagnosticsConfig: AppConfig? { config }

    // MARK: Session stores

    func sessionStore(for chat: Chat) -> SessionStore? {
        if let demo { return demo.sessionStore(for: chat.id) }
        guard let config else { return nil }
        if let existing = sessionStores[chat.id] {
            existing.hostDeviceId = chat.deviceId
            return existing
        }
        let store = SessionStore(chatId: chat.id, config: config)
        store.hostDeviceId = chat.deviceId
        sessionStores[chat.id] = store
        store.start()
        return store
    }

    func releaseSessionStore(chatId: String) {
        // Preloaded stores stay warm — nothing to evict on navigation.
    }

    /// Warm every non-archived session: stores hydrate from disk instantly
    /// and keep their rooms syncing, so opening a session never shows a
    /// loading state.
    func preloadSessions() {
        for chat in overviewChats {
            _ = sessionStore(for: chat)
        }
    }
}
