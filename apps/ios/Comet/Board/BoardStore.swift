// The board, on the phone: a standing `WatchBoard` subscription over the
// device-room relay, plus the dispatch/retry/cancel verbs.
//
// The board store lives on exactly ONE device — wherever the board service
// runs, usually the always-on box (§gh#55). The desktop finds it by asking its
// own engine to forward with `targetDeviceId`; the phone has no engine of its
// own, so it dials each candidate's device room directly and calls the same
// relay-forwardable methods there. Same sweep, same rule for ruling a
// candidate out — the engine refuses `WatchBoard` outright when it hosts no
// board, so a stream that ends without ever delivering a frame has said "not
// me".
//
// The subscription is STANDING, not opened when a board screen appears: the
// Agents section on Home is presence, and presence that only works after you
// have visited the board is not presence (the same correction gh#103 made to
// the desktop panel).

import Foundation
import Observation

@MainActor
@Observable
final class BoardStore {
    /// The board's rows, newest frame wins. Empty until a host answers.
    private(set) var rows: [TaskRow] = []
    /// Which chat takes this board's stray notices (gh#104/gh#122, gh#348) —
    /// the fallback-chat stream, ridden on the same host the board sweep
    /// settled on. `nil` is a board that sends them nowhere, which is a
    /// legitimate way to run one.
    private(set) var fallbackChatId: String?
    /// The device currently serving this board, once the sweep has settled.
    private(set) var hostDeviceId: String?
    /// True once any host has ever delivered a frame — the difference between
    /// "no board on this org" and "haven't looked yet".
    private(set) var attached = false
    /// Why the board is not showing, when it is not.
    private(set) var status: String?

    /// The host's runtime catalog and saved logins, loaded when a dispatch
    /// picker first needs them and then cached: the picker is a thumb tap away
    /// from a row, and two round trips at that moment is the whole latency.
    private(set) var runtimes: [BoardRuntime] = []
    private(set) var accounts: [BoardAccount] = []

    private let config: AppConfig
    private let devices: () -> [DeviceRow]
    private var relayClients: [String: DeviceRelayClient] = [:]
    private var watchTask: Task<Void, Never>?
    private var pinTask: Task<Void, Never>?

    init(config: AppConfig, devices: @escaping () -> [DeviceRow]) {
        self.config = config
        self.devices = devices
    }

    // MARK: Lifecycle

    func start() {
        guard watchTask == nil else { return }
        watchTask = Task { [weak self] in await self?.watchLoop() }
    }

    func stop() {
        watchTask?.cancel()
        watchTask = nil
        pinTask?.cancel()
        pinTask = nil
        let clients = relayClients.values
        relayClients.removeAll()
        Task { for client in clients { await client.close() } }
    }

    private func relay(for deviceId: String) -> DeviceRelayClient {
        if let existing = relayClients[deviceId] { return existing }
        let client = DeviceRelayClient(deviceId: deviceId, config: config)
        relayClients[deviceId] = client
        return client
    }

    // MARK: The sweep

    private enum WatchOutcome {
        /// It is the board host and we held it until its stream ended.
        case settled
        /// It hosts a board, but one with no dispatch evidence (gh#125) — held
        /// as a fallback while the sweep keeps asking.
        case held
        /// It hosts no board; the sweep moves on.
        case notMe
    }

    /// Sweep the candidates until one answers, hold that host until its stream
    /// ends, then sweep again. Backoff is per *round*, not per candidate: a
    /// board that is simply down should not make the phone hammer every device
    /// in the org.
    ///
    /// A frame proves a board exists, not that it is the org's board (gh#125):
    /// a candidate whose board shows no dispatch evidence is held rather than
    /// settled on, so a laptop's stale test board loses to the box everyone
    /// works from — and still shows when nobody else answers.
    private func watchLoop() async {
        var backoff: UInt64 = 2
        while !Task.isCancelled {
            let candidates = boardHostCandidates(devices())
            if candidates.isEmpty {
                // No engine device has synced yet — the workspace doc is still
                // arriving. Say nothing rather than "no board": the org may
                // well have one.
                status = attached ? "Reconnecting to the board…" : nil
                try? await Task.sleep(nanoseconds: 2_000_000_000)
                continue
            }
            var answered = false
            var fallback: String?
            for deviceId in candidates where !Task.isCancelled {
                switch await watch(deviceId: deviceId, settling: false) {
                case .settled:
                    answered = true
                case .held:
                    // First held answer wins the slot — earliest in sweep order.
                    if fallback == nil { fallback = deviceId }
                    continue
                case .notMe:
                    continue
                }
                break  // its stream ended; sweep again from the top
            }
            if !answered, let fallback, !Task.isCancelled {
                // Nobody with dispatch evidence answered; the held board is
                // the best there is. Settle on it this round.
                if case .settled = await watch(deviceId: fallback, settling: true) {
                    answered = true
                }
            }
            if Task.isCancelled { return }
            if answered {
                backoff = 2
            } else {
                hostDeviceId = nil
                status = attached
                    ? "Lost the board — retrying"
                    : "No device in this org is hosting a board"
            }
            try? await Task.sleep(nanoseconds: backoff * 1_000_000_000)
            backoff = min(backoff * 2, 30)
        }
    }

    /// Watch one candidate. `settling` skips the dispatch-evidence check — the
    /// sweep already exhausted the org and is returning to its held fallback.
    private func watch(deviceId: String, settling: Bool) async -> WatchOutcome {
        let stream = await relay(for: deviceId).subscribe(method: "WatchBoard", params: [:])
        var delivered = false
        do {
            for try await item in stream {
                guard !Task.isCancelled else { return .settled }
                // A frame that will not decode is a schema skew, not a "no
                // board here" — keep the host and keep the last good rows.
                let decoded = try? JSONDecoder().decode([TaskRow].self, from: item)
                if !delivered, !settling, let decoded, !boardDispatched(decoded) {
                    // Dropping the stream cancels the subscription.
                    return .held
                }
                if let decoded { rows = decoded }
                if !delivered {
                    delivered = true
                    attached = true
                    hostDeviceId = deviceId
                    status = nil
                    // The pickers' data belongs to the host that will run the
                    // dispatch — reload it whenever the host changes.
                    Task { [weak self] in await self?.loadPickerCatalogs(host: deviceId) }
                    // It rides the same host: it answers a question about
                    // this board, and the sidebar's notices slot needs it
                    // before any board screen has been opened (gh#122).
                    pinTask?.cancel()
                    pinTask = watchPin(deviceId: deviceId)
                }
            }
        } catch {
            // An error mid-stream on a host that HAS answered is a dropped
            // link, not a verdict — `delivered` keeps that distinction.
            if delivered { status = "Board connection dropped — retrying" }
        }
        // The board stream ended, so the pin stream's host is over too; the
        // last-known pin is kept, exactly as the rows are, until the next host
        // answers with a fresh one.
        pinTask?.cancel()
        pinTask = nil
        return delivered ? .settled : .notMe
    }

    /// The fallback-chat stream (gh#104): the current address first, then
    /// every change. A frame that will not decode is schema skew — keep the
    /// last good value, the same discipline the row stream keeps.
    ///
    /// The method name is the pre-gh#348 spelling and stays that way: it is a
    /// wire identifier, and an engine that renamed it would go dark on every
    /// phone already installed.
    private func watchPin(deviceId: String) -> Task<Void, Never> {
        Task { [weak self] in
            guard let self else { return }
            struct Pin: Decodable { var chatId: String? }
            let stream = await self.relay(for: deviceId)
                .subscribe(method: "WatchBoardOrchestrator", params: [:])
            do {
                for try await item in stream {
                    guard !Task.isCancelled else { return }
                    if let pin = try? JSONDecoder().decode(Pin.self, from: item) {
                        self.fallbackChatId = pin.chatId
                    }
                }
            } catch {
                // A dropped pin stream is not a verdict on the board; the row
                // stream's own lifecycle drives the resweep.
            }
        }
    }

    // MARK: Picker catalogs

    private func loadPickerCatalogs(host: String) async {
        let client = relay(for: host)
        if let list: [BoardRuntime] = try? await client.call(method: "ListBoardRuntimes",
                                                             params: [:]) {
            runtimes = list
        }
        // The saved logins are the host's: a run executes over there, so the
        // subscription a dispatch can spend is one the BOX has, never the
        // phone's (gh#74).
        struct Snapshot: Decodable { var accounts: [BoardAccount] }
        if let snapshot: Snapshot = try? await client.call(method: "ListAgentAccounts",
                                                           params: [:]) {
            accounts = snapshot.accounts
        }
    }

    /// The logins a runtime could spend. A Claude slot is not lendable to a
    /// codex run — the two harnesses read different config-dir variables — so
    /// offering every slot for every runtime would be offering dispatches that
    /// refuse themselves (gh#59).
    func accounts(forHarness harness: String?) -> [BoardAccount] {
        guard let harness, !harness.isEmpty else { return [] }
        return accounts.filter { $0.harness == harness }
    }

    // MARK: The repo picker (gh#118)

    /// One device that answered `ListRepoSpaces`, i.e. one board host.
    struct RepoHost: Identifiable {
        var id: String { deviceId }
        var deviceId: String
        var links: [SpaceSlug]
        var offers: [RepoOffer]
        /// Why the host offered no repos, when it offered none. Not an error: a
        /// board on a `GITHUB_TOKEN` has no App installations to enumerate
        /// (gh#97), and its spaces are still spaces.
        var note: String?
    }

    private struct RepoSpacesReply: Decodable {
        var deviceId: String
        var spaces: [SpaceSlug]?
        var repos: [RepoOffer]?
        var reposNote: String?
    }

    /// Ask every device whether it hosts a board, and take the whole list of
    /// answers.
    ///
    /// Sweeping past the first answer costs almost nothing and buys the one fact
    /// onboarding needs: a device hosting no board refuses `ListRepoSpaces`
    /// before it does any git or GitHub work — the same "said nothing at all"
    /// contract this store's own sweep rules candidates out with — so only real
    /// hosts are slow, and only real hosts are counted. One host is the box and
    /// nothing needs asking; two is a choice the phone cannot make for you.
    func repoHosts() async -> [RepoHost] {
        var found: [RepoHost] = []
        for deviceId in boardHostCandidates(devices()) {
            guard let reply: RepoSpacesReply = try? await relay(for: deviceId)
                .call(method: "ListRepoSpaces", params: [:]) else { continue }
            found.append(RepoHost(deviceId: reply.deviceId,
                                  links: reply.spaces ?? [],
                                  offers: reply.repos ?? [],
                                  note: reply.reposNote))
        }
        return found
    }

    /// What one onboard did, or why it did not. Its refusals are written to be
    /// read ("the board's GitHub App (id 123) cannot see it — install it on
    /// acme"), so they are carried back whole rather than reduced to a flag.
    enum OnboardOutcome {
        case connected(Onboarded)
        case failed(String)
    }

    /// Clone + createSpace + adopt on `host`, in one call (gh#97). Slow by
    /// nature — it is a `git clone` on another machine.
    func onboard(slug: String, host: String) async -> OnboardOutcome {
        do {
            let done: Onboarded = try await relay(for: host)
                .call(method: "OnboardRepo", params: ["slug": slug])
            return .connected(done)
        } catch {
            return .failed(error.localizedDescription)
        }
    }

    // MARK: Verbs

    enum DispatchOutcome {
        case dispatched(chatId: String?)
        /// The host refuses a cross-billed release under `billing_guard =
        /// "require-own"` (gh#101). Not a dead end whose only remedy is editing
        /// somebody else's routing.toml — it is the confirm the CLI spells
        /// `--bill`, carried back so the sheet can offer it.
        case needsBillingConfirm(billedTo: String)
        case failed(String)
    }

    /// Release a task (or retry a live attempt). `replace` ends the attempt the
    /// row is stuck on before releasing — the blocked row's Retry (gh#49).
    ///
    /// `viaUser` is the signed-in email: provenance the attempt records and the
    /// board compares against the account being spent. A claim, never authority
    /// — the box cannot check it, and it is not a reason to spend anybody's
    /// subscription.
    func dispatch(taskId: String, runtime: String?, account: BoardAccount?,
                  replace: Bool, bill: String? = nil,
                  billedTo: String?) async -> DispatchOutcome {
        guard let host = hostDeviceId else { return .failed("No board host") }
        var params: [String: Any] = ["taskId": taskId]
        if replace { params["replace"] = true }
        if let runtime { params["runtime"] = runtime }
        if let account { params["account"] = account.id }
        if let bill { params["bill"] = bill }
        params["viaDevice"] = config.deviceId
        if let email = signedInEmail { params["viaUser"] = email }

        struct Reply: Decodable { var chatId: String? }
        do {
            let reply: Reply = try await relay(for: host).call(method: "DispatchTask",
                                                               params: params)
            return .dispatched(chatId: reply.chatId)
        } catch {
            let message = error.localizedDescription
            if let billedTo, message.contains(requireOwnRefusal) {
                return .needsBillingConfirm(billedTo: billedTo)
            }
            return .failed(message)
        }
    }

    /// The issue text behind one row, for the detail sheet (gh#132).
    ///
    /// An issue with no description and a read that did not happen are
    /// different answers — see [`TaskBody`] — because a sheet that drew both as
    /// a blank body would be lying about one of them.
    func taskDetail(taskId: String) async -> TaskBody {
        guard let host = hostDeviceId else { return .failed("No board host") }
        do {
            let reply: TaskDetail = try await relay(for: host).call(method: "ReadBoardTask",
                                                                    params: ["taskId": taskId])
            let body = reply.body?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
            return body.isEmpty ? .empty : .text(body)
        } catch {
            return .failed(error.localizedDescription)
        }
    }

    // MARK: The review (gh#256)

    /// What one attempt is worth: the brief, the claims, the effects the board
    /// derived from the branch, and everything nobody accounted for (§gh#183,
    /// §gh#236).
    ///
    /// Swept the same way the stats page is, and for the same reason: `board.db`
    /// lives on whichever device hosts the board, so a candidate that refuses
    /// has said "I host no board" and the sweep moves on. A reply that will not
    /// decode ends it instead — that host HAS a board and is running a different
    /// version, and asking the next device would report the wrong problem.
    enum ReviewOutcome {
        case read(AttemptReview, host: String)
        case failed(String)
    }

    func attemptReview(taskId: String) async -> ReviewOutcome {
        var candidates = boardHostCandidates(devices())
        if let host = hostDeviceId {
            candidates.removeAll { $0 == host }
            candidates.insert(host, at: 0)
        }
        guard !candidates.isEmpty else {
            return .failed("No device in this org is hosting a board")
        }
        var last: String?
        for deviceId in candidates {
            do {
                let review: AttemptReview = try await relay(for: deviceId)
                    .call(method: "ReadAttemptReview", params: reviewReadParams(taskId: taskId))
                return .read(review, host: deviceId)
            } catch is DecodingError {
                return .failed("Unreadable review — the board is on another version")
            } catch {
                last = error.localizedDescription
            }
        }
        return .failed(last ?? "No device in this org is hosting a board")
    }

    /// Send the verdict (§gh#239): one review on the pull request, one prompt
    /// into the checkout the agent is still in.
    ///
    /// Asked of the host that answered the read, not swept: a verdict is a
    /// write, and trying the next device after a refusal would be looking for
    /// somebody willing to accept it.
    enum VerdictOutcome {
        case sent(VerdictReceipt)
        case failed(String)
    }

    func submitVerdict(target: ReviewTarget, kind: VerdictKind,
                       comment: String) async -> VerdictOutcome {
        do {
            let receipt: VerdictReceipt = try await relay(for: target.host)
                .call(method: "SubmitVerdict",
                      params: reviewVerdictParams(target, kind: kind, comment: comment))
            return .sent(receipt)
        } catch {
            return .failed(error.localizedDescription)
        }
    }

    /// End a task's live attempt (interrupt + archive the chat). The issue
    /// stays open: cancel ends attempts, never tasks.
    func cancel(taskId: String) async -> String? {
        guard let host = hostDeviceId else { return "No board host" }
        struct Reply: Decodable { var ok: Bool? }
        do {
            let _: Reply = try await relay(for: host).call(method: "CancelTask",
                                                           params: ["taskId": taskId])
            return nil
        } catch {
            return error.localizedDescription
        }
    }

    // MARK: Throughput (gh#143)

    enum StatsOutcome {
        case read(AggregateBoardStats)
        case failed(String)
    }

    /// What the board did with the work it was given, over a window.
    ///
    /// Any engine can collect this: it fans out concurrently to every engine
    /// device under a fixed budget and returns canonical board ids, individual
    /// board answers, and explicit missing hosts. The phone tries candidate
    /// collectors only until one answers; it never performs or merges the
    /// board reads itself.
    ///
    /// A reply that will not decode ends the sweep rather than continuing it:
    /// that host HAS a board and is running a different version, and asking
    /// the next device would report the wrong problem.
    func stats(sinceDays: Int64?) async -> StatsOutcome {
        var candidates = boardHostCandidates(devices())
        if let host = hostDeviceId {
            candidates.removeAll { $0 == host }
            candidates.insert(host, at: 0)
        }
        guard !candidates.isEmpty else {
            return .failed("No device in this org is hosting a board")
        }
        var params: [String: Any] = [:]
        // Omitted, never null: all time is the absence of a window.
        if let sinceDays { params["sinceDays"] = sinceDays }
        var last: String?
        for deviceId in candidates {
            do {
                let stats: AggregateBoardStats = try await relay(for: deviceId)
                    .call(method: "AggregateBoardStats", params: params)
                return .read(stats)
            } catch is DecodingError {
                return .failed("Unreadable stats — the board is on another version")
            } catch {
                last = error.localizedDescription
            }
        }
        return .failed(last ?? "No device in this org is hosting a board")
    }

    // MARK: Where the board's notices go (gh#104, gh#144, gh#348)

    /// Send the board's stray notices to a chat, or stop.
    ///
    /// One `[defaults]` key on the board's `routing.toml`, written through the
    /// same validated path a settings page uses. `nil` removes it, which is the
    /// kill switch: the notices stop and nothing else about the chat changes.
    ///
    /// Nothing is applied optimistically — the board republishes the address on
    /// the watch stream as the write lands, so the slot disappearing IS the box
    /// agreeing. Returns an error to say, or nil.
    func setFallbackChat(chatId: String?) async -> String? {
        var candidates = boardHostCandidates(devices())
        if let host = hostDeviceId {
            candidates.removeAll { $0 == host }
            candidates.insert(host, at: 0)
        }
        guard !candidates.isEmpty else { return "No board host" }
        var params: [String: Any] = ["op": "default", "key": "fallback_chat"]
        if let chatId { params["value"] = chatId }
        // The host replies with the board's whole config view; nothing here
        // reads it — the pin comes back on the watch stream — so this decodes
        // any object and cares only that the write was accepted.
        struct Ack: Decodable {}
        var last: String?
        for deviceId in candidates {
            do {
                let _: Ack = try await relay(for: deviceId)
                    .call(method: "WriteBoardConfig", params: params)
                return nil
            } catch {
                last = error.localizedDescription
            }
        }
        return last ?? "No device in this org is hosting a board"
    }

    /// The email the attempt row should carry as its releaser. WorkOS mode
    /// knows one; dev mode's bearer is a user id, which is what a reader gets.
    private var signedInEmail: String? {
        let stored = UserDefaults.standard.string(forKey: "userEmail")
        if let stored, !stored.isEmpty { return stored }
        return config.userId.isEmpty ? nil : config.userId
    }
}
