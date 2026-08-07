// Headless e2e rig — launch with `-e2e` (plus a local wrangler dev edge and a
// `comet headless` engine in dev mode) and the app exercises the full live
// stack with no taps: workspace room backfill, device-relay RPCs, space/chat
// creation, the command plane, and session-room streaming. Results append to
// Documents/e2e.log for the harness to read via simctl.

import Foundation

@MainActor
enum E2ERunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("e2e.log")
    }

    static func log(_ line: String) {
        let stamped = "[\(Int(Date().timeIntervalSince1970))] \(line)\n"
        print("E2E: \(line)")
        if let handle = try? FileHandle(forWritingTo: logURL) {
            handle.seekToEndOfFile()
            handle.write(Data(stamped.utf8))
            try? handle.close()
        } else {
            try? Data(stamped.utf8).write(to: logURL)
        }
    }

    static func run(model: AppModel) async {
        try? FileManager.default.removeItem(at: logURL)
        log("start")
        model.signInDev(edgeURL: URL(string: "http://localhost:8787")!,
                        userId: "devuser", orgId: "dev-org")

        // 1. Workspace room: wait for connection + the engine's device row.
        guard let workspace = model.workspace else {
            log("FAIL no workspace store")
            return
        }
        // Warm-start probe: rows visible BEFORE any network = disk hydration.
        log("warm-start devices=\(workspace.devices.count) chats=\(workspace.chats.count)")
        let device = await poll(timeout: 15, label: "workspace device") {
            workspace.connected ? workspace.devices.first { $0.platform != "ios" } : nil
        }
        guard let device else {
            log("FAIL workspace: connected=\(workspace.connected) devices=\(workspace.devices.map(\.id))")
            return
        }
        log("OK workspace synced; engine device \(device.id) (\(device.name))")

        // 2. Device relay: ListFolders on every engine device (stale rig
        // devices linger in the dev workspace doc — report each).
        var listing: FolderListing?
        for candidate in workspace.devices where candidate.platform != "ios" {
            do {
                let l = try await workspace.listFoldersDetailed(deviceId: candidate.id, path: nil)
                log("OK relay ListFolders[\(candidate.name)/\(candidate.id.prefix(8))]: \(l.path) → \(l.entries.count) entries")
                listing = l
            } catch {
                log("FAIL relay ListFolders[\(candidate.name)/\(candidate.id.prefix(8))]: \(error.localizedDescription)")
            }
        }

        // 2b. Live model catalog over the relay.
        let models = await workspace.listModels(deviceId: device.id, harness: "mock")
        log(models != nil ? "OK relay ListModels: \(models!.map(\.id))" : "FAIL relay ListModels nil")

        // 2c. The board's stream path (gh#114). This is the plumbing check, not
        // a board check: a dev engine usually hosts no board, and the engine
        // REFUSING the subscription is as good an answer as rows — both mean
        // the request reached the host and a ServerFrame came back. A hang is
        // the only failure, which is what a client that cannot read `{item}`
        // frames looked like before this existed.
        if let config = model.diagnosticsConfig {
            await probeBoardStream(config: config, devices: workspace.devices)
        }

        // 3. Space + chat + first run through the command plane (mock harness).
        let spaceId = await workspace.createSpace(deviceId: device.id,
                                                  path: listing?.path ?? "/tmp", gitDetected: false)
        log("space created \(spaceId)")
        // Relay-created spaces land via doc sync — eventually consistent.
        let space = await poll(timeout: 10, label: "space row sync") {
            workspace.spaces.first { $0.id == spaceId }
        }
        guard let space else {
            log("FAIL space row never synced")
            return
        }
        let chatId = workspace.createChat(
            space: space,
            config: ChatConfig(harness: "mock", model: nil, reasoning: nil, sandbox: "workspace-write"))
        guard let chat = workspace.chats.first(where: { $0.id == chatId }),
              let store = model.sessionStore(for: chat) else {
            log("FAIL chat/session store")
            return
        }
        store.sendRun(prompt: "e2e ping", chat: chat)
        log("run queued on \(chatId)")

        let entries = await poll(timeout: 30, label: "assistant reply") {
            store.entries.contains { $0.role == .assistant && !$0.parts.isEmpty } ? store.entries : nil
        }
        if let entries {
            log("OK transcript streamed: \(entries.count) entries")
        } else {
            log("FAIL no assistant reply; entries=\(store.entries.count) connected=\(store.connected)")
        }

        // 4. Big-doc backfill (fragmented): open the chat the seeder filled.
        let bigChatId = "e2e-big-doc"
        let bigChat = Chat(id: bigChatId, deviceId: device.id, title: "big", archived: false,
                           cwd: nil, branch: nil, checkoutId: nil, config: nil,
                           lastMessagePreview: nil, lastMessageAt: nil, createdAt: nowMs(),
                           spaceId: spaceId, lastSeenAt: nil)
        if let bigStore = model.sessionStore(for: bigChat) {
            let big = await poll(timeout: 20, label: "big doc backfill") {
                bigStore.entries.count >= 40 ? bigStore.entries : nil
            }
            if let big {
                let bytes = big.flatMap(\.parts).reduce(0) { acc, part in
                    if case .text(_, let t) = part { return acc + t.count }
                    return acc
                }
                log("OK big-doc backfill: \(big.count) entries, ~\(bytes / 1024)KB text")
            } else {
                log("FAIL big-doc backfill: entries=\(bigStore.entries.count) connected=\(bigStore.connected)")
            }
        }

        log("done")
    }

    /// `WatchBoard` over the device-room relay, per engine device (gh#114).
    ///
    /// What is under test is the STREAM path, which no iOS call had ever used:
    /// unary `{ok}` replies were the only frames this client understood, so an
    /// `{item}` used to fall through as "unexpected reply" and a subscription
    /// hung until its deadline. Three outcomes, all of them informative:
    /// rows (this device hosts the board), a refusal (it does not — the answer
    /// the host sweep is built on), or the 8s timeout, which is the only bug.
    static func probeBoardStream(config: AppConfig, devices: [DeviceRow]) async {
        for device in devices where device.platform != "ios" {
            let client = DeviceRelayClient(deviceId: device.id, config: config)
            let label = "\(device.name)/\(device.id.prefix(8))"
            let outcome: String = await withTaskGroup(of: String?.self) { group in
                group.addTask {
                    do {
                        for try await item in await client.subscribe(method: "WatchBoard",
                                                                     params: [:]) {
                            let rows = (try? JSONDecoder().decode([TaskRow].self, from: item))
                            return "OK board stream[\(label)]: first frame, "
                                + "\(rows?.count ?? -1) rows"
                        }
                        return "board stream[\(label)]: ended with no frame (hosts no board)"
                    } catch {
                        return "board stream[\(label)]: refused — \(error.localizedDescription)"
                    }
                }
                group.addTask {
                    try? await Task.sleep(nanoseconds: 8_000_000_000)
                    return "FAIL board stream[\(label)]: no frame and no error in 8s"
                }
                let first = await group.next() ?? nil
                group.cancelAll()
                return first ?? "board stream[\(label)]: no result"
            }
            log(outcome)
            await client.close()
        }
    }

    /// The dispatch half of gh#114, end to end against a real board: make the
    /// route's space, wait for the board to call a row dispatchable, release it,
    /// and watch the row move. Launch with `-e2e-board <repoPath>` against a
    /// dev edge and a headless engine whose `routing.toml` routes to that repo.
    ///
    /// Separate from `run` because it needs a board with a route, which the
    /// plain smoke deliberately does not: there, a board that refuses is a pass.
    static func runBoard(model: AppModel, repoPath: String) async {
        try? FileManager.default.removeItem(at: logURL)
        log("board start repo=\(repoPath)")
        model.signInDev(edgeURL: URL(string: "http://localhost:8787")!,
                        userId: "devuser", orgId: "dev-org")
        guard let workspace = model.workspace, let board = model.board else {
            log("FAIL no workspace/board store")
            return
        }
        guard let device = await poll(timeout: 20, label: "engine device", {
            workspace.connected ? workspace.devices.first { $0.platform != "ios" } : nil
        }) else {
            log("FAIL workspace never synced")
            return
        }
        // The route names a space; without it every row is "no route".
        let spaceId = await workspace.createSpace(deviceId: device.id, path: repoPath,
                                                  gitDetected: true)
        log("space \(spaceId) at \(repoPath) on \(device.name)")

        guard await poll(timeout: 30, label: "board host", { board.attached ? true : nil }) != nil
        else {
            log("FAIL board never attached: \(board.status ?? "no status")")
            return
        }
        log("OK board attached on \(board.hostDeviceId ?? "?") — \(board.rows.count) rows")

        // The board re-resolves routes on its sync interval and on space
        // changes, so a row that was "no route" a moment ago becomes
        // dispatchable without anything being poked.
        guard let ready = await poll(timeout: 90, label: "a dispatchable row", {
            board.rows.first { $0.boardState == .ready && $0.dispatchable }
        }) else {
            log("FAIL nothing dispatchable; rows="
                + board.rows.map { "\($0.identifier)/\($0.state)/route=\($0.route ?? "-")" }
                    .joined(separator: ", "))
            return
        }
        log("OK dispatchable: \(ready.identifier) → \(ready.workspace ?? "?") "
            + "runtime=\(ready.runtime ?? "-") cap=\(ready.maxDurationSecs.map(String.init) ?? "-")")

        switch await board.dispatch(taskId: ready.id, runtime: nil, account: nil,
                                    replace: false, billedTo: nil) {
        case .dispatched(let chatId):
            log("OK DispatchTask → chat \(chatId ?? "nil")")
        case .needsBillingConfirm(let who):
            log("OK DispatchTask refused for billing (guard reachable): bills \(who)")
            return
        case .failed(let message):
            log("FAIL DispatchTask: \(message)")
            return
        }

        // The proof is the row moving under its own steam: the board wrote an
        // attempt, and the stream delivered it back to the phone.
        if let live = await poll(timeout: 45, label: "row goes live", {
            board.rows.first { $0.id == ready.id && $0.boardState.holdsPane }
        }) {
            log("OK row is \(live.state) on chat \(live.chatId ?? "?") "
                + "branch=\(live.branch ?? "-") started=\(live.startedAt ?? "-")")
            let agents = model.liveAgents
            log(agents.isEmpty
                ? "note: no agent row yet (chat still syncing to this device)"
                : "OK agent row: \(agents.map { "\($0.identifier)/\($0.state.label)" }.joined(separator: ", "))")
            // The other half of the list (gh#117). The dispatched chat must NOT
            // be in it — the two groups partition what is running, and a chat
            // in both would double-count the box's load.
            let running = model.runningChats
            log(running.contains { $0.chatId == live.chatId }
                ? "FAIL the dispatched chat is in Running as well as Agents"
                : "OK running (non-board): \(running.map(\.title).joined(separator: ", "))")
            // The blocked row's Retry (gh#49): end the live attempt and release
            // a fresh one. Driven here against a `working` row because the
            // engine's `replace` means exactly "end what is live first" — which
            // is the same call the Retry chip makes, minus waiting for an agent
            // to get stuck.
            switch await board.dispatch(taskId: ready.id, runtime: nil, account: nil,
                                        replace: true, billedTo: nil) {
            case .dispatched(let retryChat):
                log("OK retry (replace) → chat \(retryChat ?? "nil")"
                    + (retryChat == live.chatId ? " — SAME chat, attempt not replaced" : ""))
                if let after = await poll(timeout: 45, label: "retry attempt lands", {
                    board.rows.first { $0.id == ready.id && $0.chatId != live.chatId }
                }) {
                    log("OK retried: attempts=\(after.attempts) state=\(after.state) "
                        + "chat=\(after.chatId ?? "-")")
                } else {
                    log("FAIL retry never produced a new attempt")
                }
            case .needsBillingConfirm(let who):
                log("retry refused for billing (guard reachable): bills \(who)")
            case .failed(let message):
                log("FAIL retry: \(message)")
            }
        } else {
            log("FAIL row never went live; state="
                + (board.rows.first { $0.id == ready.id }?.state ?? "gone"))
        }
        log("done")
    }

    private static func poll<T>(timeout: TimeInterval, label: String,
                                _ probe: @MainActor () -> T?) async -> T? {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if let value = probe() { return value }
            try? await Task.sleep(nanoseconds: 300_000_000)
        }
        log("timeout waiting for \(label)")
        return nil
    }
}

extension E2ERunner {
    /// Live-relay probe: runs inside the user's real signed-in session and
    /// interrogates every engine device — workspace presence, the device
    /// room's host attachment, and a real ListFolders with the exact error.
    @MainActor
    static func runLive(model: AppModel) async {
        try? FileManager.default.removeItem(at: logURL)
        log("live start edge=\(model.edgeURLString) mode=\(model.authModeRaw) user=\(model.storedUserId.prefix(18)) org=\(model.storedOrgId.prefix(18))")
        let workspace = await poll(timeout: 25, label: "workspace connect") {
            model.workspace?.connected == true ? model.workspace : nil
        }
        guard let workspace else {
            log("FAIL workspace never connected: store=\(model.workspace != nil) "
                + "userId=\(model.storedUserId.isEmpty ? "EMPTY" : "set") "
                + "orgId=\(model.storedOrgId.isEmpty ? "EMPTY" : "set") "
                + "access=\(Keychain.load(key: "accessToken") != nil) "
                + "refresh=\(Keychain.load(key: "refreshToken") != nil) "
                + "mode=\(model.authModeRaw)")
            return
        }
        log("devices: " + workspace.devices.map {
            "\($0.name)[\($0.platform)] id=\($0.id) presence=\(workspace.deviceOnline($0.id))"
        }.joined(separator: ", "))
        guard let config = model.diagnosticsConfig else {
            log("FAIL no config")
            return
        }
        for device in workspace.devices where device.platform != "ios" {
            let status = await config.deviceStatus(deviceId: device.id)
            log("\(device.name) /status → \(status)")
            do {
                let listing = try await workspace.listFoldersDetailed(deviceId: device.id, path: nil)
                log("OK \(device.name) ListFolders → \(listing.path) (\(listing.entries.count) entries)")
            } catch {
                log("FAIL \(device.name) ListFolders → \(error.localizedDescription)")
            }
        }
        await probeBoardStream(config: config, devices: workspace.devices)
        log("done")
    }
}
