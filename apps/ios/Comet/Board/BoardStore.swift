// The board, on the phone: a standing `WatchBoard` subscription over the
// device-room relay, plus the dispatch/retry/cancel verbs.
//
// The board store lives on exactly ONE device — wherever the board service runs,
// usually the always-on box (docs/BOARD.md §H9). The desktop finds it by asking
// its own engine to forward with `targetDeviceId`; the phone has no engine of
// its own, so it dials each candidate's device room directly and calls the same
// relay-forwardable methods there. Same sweep, same rule for ruling a candidate
// out — the engine refuses `WatchBoard` outright when it hosts no board, so a
// stream that ends without ever delivering a frame has said "not me".
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

    /// Sweep the candidates until one answers, hold that host until its stream
    /// ends, then sweep again. Backoff is per *round*, not per candidate: a
    /// board that is simply down should not make the phone hammer every device
    /// in the org.
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
            for deviceId in candidates where !Task.isCancelled {
                if await watch(deviceId: deviceId) {
                    answered = true
                    backoff = 2
                    break  // its stream ended; sweep again from the top
                }
            }
            if Task.isCancelled { return }
            if !answered {
                hostDeviceId = nil
                status = attached
                    ? "Lost the board — retrying"
                    : "No device in this org is hosting a board"
            }
            try? await Task.sleep(nanoseconds: backoff * 1_000_000_000)
            backoff = min(backoff * 2, 30)
        }
    }

    /// Watch one candidate. Returns true if it ever delivered a frame (i.e. it
    /// IS the board host and we held it until the stream ended); false means it
    /// hosts no board and the sweep should move on.
    private func watch(deviceId: String) async -> Bool {
        let stream = await relay(for: deviceId).subscribe(method: "WatchBoard", params: [:])
        var delivered = false
        do {
            for try await item in stream {
                guard !Task.isCancelled else { return true }
                // A frame that will not decode is a schema skew, not a "no
                // board here" — keep the host and keep the last good rows.
                if let decoded = try? JSONDecoder().decode([TaskRow].self, from: item) {
                    rows = decoded
                }
                if !delivered {
                    delivered = true
                    attached = true
                    hostDeviceId = deviceId
                    status = nil
                    // The pickers' data belongs to the host that will run the
                    // dispatch — reload it whenever the host changes.
                    Task { [weak self] in await self?.loadPickerCatalogs(host: deviceId) }
                }
            }
        } catch {
            // An error mid-stream on a host that HAS answered is a dropped
            // link, not a verdict — `delivered` keeps that distinction.
            if delivered { status = "Board connection dropped — retrying" }
        }
        return delivered
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

    /// The email the attempt row should carry as its releaser. WorkOS mode
    /// knows one; dev mode's bearer is a user id, which is what a reader gets.
    private var signedInEmail: String? {
        let stored = UserDefaults.standard.string(forKey: "userEmail")
        if let stored, !stored.isEmpty { return stored }
        return config.userId.isEmpty ? nil : config.userId
    }
}
