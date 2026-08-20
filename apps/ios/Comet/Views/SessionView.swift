// Session screen — transcript + status strip + composer (or question panel
// while input is requested, replacing the composer like the desktop). Reading
// marks the chat seen (the synced LWW marker behind the green dot everywhere).

import SwiftUI

struct SessionView: View {
    @Environment(AppModel.self) private var model
    let chatId: String
    @State private var showConfig = false
    /// The fork strip's expanded sentence (gh#425) — collapsed by default: the
    /// tags answer "what is this", the sentence answers "and what does that
    /// mean for what I am looking at".
    @State private var showForkDetail = false
    @State private var refs: [RepoRef] = []
    @State private var catalogs: [String: [ModelInfo]] = [:]

    /// The orchestrator pin, asked for and refused (gh#166) — the chat's own
    /// menu is where a person is standing when they decide this chat is the one
    /// the board should talk to.
    @State private var pinRequest: OrchestratorPinRequest?
    @State private var pinFailure: OrchestratorPinFailure?

    /// Width the nav bar's own controls need either side of the title — the
    /// back button leading, breathing room trailing.
    private static let headerChromeInset: CGFloat = 132

    /// The view's own width, the only reliable basis for capping the principal
    /// toolbar item (its container proposes an unbounded width).
    @State private var viewWidth: CGFloat = 0

    private var chat: Chat? { model.chat(id: chatId) }

    private var chatSpace: Space? {
        guard let spaceId = chat?.spaceId else { return nil }
        return model.spaces.first { $0.id == spaceId }
    }

    var body: some View {
        Group {
            if let chat, let store = model.sessionStore(for: chat) {
                content(chat: chat, store: store)
                    .onGeometryChange(for: CGFloat.self) { $0.size.width } action: { viewWidth = $0 }
            } else {
                VStack(spacing: 12) {
                    CometPulse()
                    Text("Opening session…")
                        .font(Theme.sans(Theme.textDense))
                        .foregroundStyle(Theme.textFaint)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Theme.bg)
            }
        }
        .navigationTitle(chat?.displayTitle ?? "Session")  // feeds the back menu
        .navigationBarTitleDisplayMode(.inline)
        .toolbarBackground(.hidden, for: .navigationBar)
        .toolbar {
            if let chat {
                ToolbarItem(placement: .principal) {
                    // Tapping the header reconfigures model/effort mid-chat
                    // (the old app's header model pill); harness stays locked.
                    Button {
                        showConfig = true
                    } label: {
                        VStack(spacing: 1) {
                            HStack(spacing: 6) {
                                HarnessBadge(harness: chat.config?.harness ?? "claude-code", size: 12)
                                // The badge and chevron are fixed; only the
                                // title gives way, so a long name truncates
                                // instead of pushing the chevron off-screen.
                                Text(chat.displayTitle)
                                    .font(Theme.sans(Theme.textBody, weight: .medium))
                                    .foregroundStyle(Theme.text)
                                    .lineLimit(1)
                                    .truncationMode(.tail)
                                    .layoutPriority(1)
                                Image(systemName: "chevron.down")
                                    .font(.system(size: 8, weight: .semibold))
                                    .foregroundStyle(Theme.textFaint)
                                    .layoutPriority(2)
                            }
                            if let subtitle {
                                // Middle-truncated: the tail (device) identifies
                                // the session as much as the leading repo does.
                                Text(subtitle)
                                    .font(Theme.sans(Theme.textCaption))
                                    .foregroundStyle(Theme.textSubtle)
                                    .lineLimit(1)
                                    .truncationMode(.middle)
                            }
                        }
                        // A principal toolbar item is handed its IDEAL width, so
                        // an unconstrained header just runs past the bar and off
                        // the screen. Cap it to the centre region — the back
                        // button and any trailing item own the rest.
                        // A principal toolbar item is handed its IDEAL width, so
                        // an unconstrained header runs past the bar and off the
                        // screen. Cap it against the view's own width, leaving
                        // the back button and trailing padding their room.
                        .frame(maxWidth: max(140, viewWidth - Self.headerChromeInset))
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
                // The chat's own menu: whole-transcript copy/share, and the pin
                // when it is offered. The pin is here because it has nowhere
                // else to be for a chat that is not running — an idle
                // orchestrator has no Active row, and before it is pinned it
                // has no slot either; it is absent when the board dispatched
                // this chat, which is the one chat that must not be pinned.
                //
                // The export is here because a session is the unit a person
                // wants OUT of the phone: during the 2026-08-19 incident the
                // operator could get nothing off the failing app at all, and
                // diagnosis ran on paraphrases over SSH (gh#534, gh#527).
                ToolbarItem(placement: .topBarTrailing) {
                    Menu {
                        transcriptExportItems(chat: chat)
                        if orchestratorPinOffered(chatId: chatId, model: model) {
                            Divider()
                            orchestratorPinItem(chatId: chatId, model: model,
                                                request: $pinRequest,
                                                failure: $pinFailure)
                        }
                    } label: {
                        Image(systemName: "ellipsis")
                    }
                    .accessibilityLabel("Session menu")
                }
            }
        }
        .orchestratorPinPrompts(request: $pinRequest, failure: $pinFailure)
        .sheet(isPresented: $showConfig) {
            if let chat {
                let harness = chat.config?.harness ?? "claude-code"
                ModelPickerSheet(
                    harness: .constant(harness),
                    modelId: Binding(
                        get: {
                            chat.config?.model
                                ?? HarnessCatalog.defaultModel(for: harness).id
                        },
                        set: { newModel in
                            writeConfig(model: newModel, reasoning: chat.config?.reasoning)
                        }
                    ),
                    reasoning: Binding(
                        get: { chat.config?.reasoning },
                        set: { newReasoning in
                            writeConfig(model: chat.config?.model, reasoning: newReasoning)
                        }
                    ),
                    lockedHarness: true,
                    catalogs: catalogs,
                    checkout: checkoutContext(chat: chat)
                )
            }
        }
        .task(id: chatId) {
            guard let space = chatSpace else { return }
            let harness = chat?.config?.harness ?? "claude-code"
            catalogs[harness] = await model.listModels(space: space, harness: harness)
            guard space.gitDetected else { return }
            if let loaded = await model.listRefs(space: space) {
                refs = loaded
            }
        }
        .onAppear {
            model.markSeen(chatId: chatId)
            if model.launchSheet == "config" {
                model.launchSheet = nil
                showConfig = true
            }
        }
        .onDisappear {
            model.markSeen(chatId: chatId)
            model.releaseSessionStore(chatId: chatId)
        }
    }

    /// Live-chat checkout context (git spaces only): read-only kind + the
    /// switchable ref list.
    private func checkoutContext(chat: Chat) -> SessionCheckoutContext? {
        guard let space = chatSpace, space.gitDetected, let cwd = chat.cwd else { return nil }
        return SessionCheckoutContext(
            isWorktree: cwd != space.path,
            cwd: cwd,
            refs: refs,
            currentBranch: chat.branch,
            onPick: { ref in
                let error = await model.switchSessionRef(chat: chat, ref: ref)
                if error == nil, let reloaded = await model.listRefs(space: space) {
                    refs = reloaded
                }
                return error
            }
        )
    }

    /// Merge a model/effort change into the chat's config row (LWW; the host
    /// picks it up on the next run dispatch).
    private func writeConfig(model newModel: String?, reasoning newReasoning: String?) {
        guard let chat else { return }
        var config = chat.config ?? ChatConfig(harness: "claude-code", model: nil,
                                               reasoning: nil, sandbox: "workspace-write")
        config.model = newModel
        config.reasoning = newReasoning
        model.setChatConfig(chatId: chat.id, config: config)
    }

    private var subtitle: String? {
        guard let chat else { return nil }
        var parts: [String] = []
        if let cwd = chat.cwd { parts.append((cwd as NSString).lastPathComponent) }
        if let branch = chat.branch, !branch.isEmpty { parts.append(branch) }
        parts.append(model.deviceName(chat.deviceId))
        return parts.joined(separator: " · ")
    }

    /// The strip that says this session is a fork (gh#425), above everything
    /// else because it changes how the rest of the screen should be read: in a
    /// shared checkout this agent is read-only while another session's edits
    /// can still appear underneath it. Tapping expands the sentence.
    @ViewBuilder
    private func forkStrip(chat: Chat) -> some View {
        if let lineage = chat.forkedFrom {
            let sourceTitle = model.chat(id: lineage.sourceChatId)?.title
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Image(systemName: "arrow.triangle.branch")
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(Theme.textSubtle)
                    Text(lineage.compact(sourceTitle: sourceTitle))
                        .font(Theme.sans(Theme.textCaption))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                        .truncationMode(.middle)
                }
                if showForkDetail {
                    Text(lineage.explanation)
                        .font(Theme.sans(Theme.textCaption))
                        .foregroundStyle(Theme.textFaint)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 14)
            .padding(.vertical, 6)
            // The shared-checkout fork is the one a reader can be misled by, so
            // it is the one that is louder.
            .background(lineage.isShared ? Theme.elementActive : Theme.surface)
            .contentShape(Rectangle())
            .onTapGesture { showForkDetail.toggle() }
        }
    }

    /// Copy / share the whole transcript. Built inside a `Menu`, so the
    /// serialization runs when the menu OPENS rather than on every body
    /// evaluation — and it walks the entries directly (no markdown re-parse),
    /// so opening it on a long session does not stall (see TranscriptText).
    @ViewBuilder
    private func transcriptExportItems(chat: Chat?) -> some View {
        if let chat, let store = model.sessionStore(for: chat) {
            let text = TranscriptText.transcript(entries: store.entries,
                                                 pendingSends: store.pendingSends)
            Button {
                UIPasteboard.general.string = text
            } label: {
                Label("Copy transcript", systemImage: "doc.on.doc")
            }
            .disabled(text.isEmpty)
            if !text.isEmpty {
                ShareLink(item: text, subject: Text(chat.displayTitle)) {
                    Label("Share transcript", systemImage: "square.and.arrow.up")
                }
            }
        }
    }

    private func content(chat: Chat, store: SessionStore) -> some View {
        let status = liveStatus(chat: chat)
        return VStack(spacing: 0) {
            forkStrip(chat: chat)
            // The status strip floats over the transcript's faded bottom edge
            // instead of stacking below it — the loader sits on the
            // transparent zone and content is never pushed around.
            TranscriptView(store: store, chatId: chat.id)
                .overlay(alignment: .bottom) {
                    statusStrip(chat: chat, status: status)
                        .allowsHitTesting(false)
                }

            if let request = store.openInputRequest {
                QuestionPanel(requestId: request.requestId, questions: request.questions) { requestId, answers in
                    store.respondInput(requestId: requestId, answers: answers)
                }
                .padding(.bottom, 8)
            } else {
                ComposerView(
                    store: store,
                    chat: chat,
                    runLive: status == .working,
                    searchContext: { query in
                        await model.searchContext(chat: chat, query: query)
                    }
                )
                    .padding(.bottom, 8)
            }
        }
        .background(Theme.bg.ignoresSafeArea())
        .motionAnimation(Motion.fadeQuick, value: store.openInputRequest?.requestId)
    }

    private func liveStatus(chat: Chat) -> SessionStatus? {
        if let demo = model.demo {
            return effectiveStatus(demo.sessions[chat.id], now: nowMs())
        }
        return effectiveStatus(model.workspace?.sessions[chat.id], now: nowMs())
    }

    /// Reserved 24pt status strip (shell.rs render_status_strip) — Working
    /// shows the sunrise spinner + rotating flavour word + elapsed; Errored
    /// shows "Run failed"; the strip always reserves its height so the
    /// composer never shifts.
    private func statusStrip(chat: Chat, status: SessionStatus?) -> some View {
        TimelineView(.periodic(from: .now, by: 1)) { _ in
            HStack(spacing: 6) {
                switch status {
                case .working:
                    WorkingSpinner()
                    let startedAt = sessionStartedAt(chat: chat)
                    let elapsed = (nowMs() - startedAt) / 1000
                    Text("\(Motion.flavourWord(seed: Motion.flavourSeed(chat.id), elapsedSecs: elapsed))…")
                        .font(Theme.sans(Theme.textDense))
                        .foregroundStyle(Theme.textMuted)
                    Text(Motion.formatElapsed(elapsed))
                        .font(Theme.sans(Theme.textCaption))
                        .foregroundStyle(Theme.textFaint)
                        .monospacedDigit()
                case .errored:
                    Text("Run failed")
                        .font(Theme.sans(Theme.textCaption))
                        .foregroundStyle(Theme.danger)
                default:
                    EmptyView()
                }
            }
            .frame(height: 24)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.leading, 26)  // aligns with the composer's text start
        }
    }

    private func sessionStartedAt(chat: Chat) -> Int64 {
        let row = model.demo?.sessions[chat.id] ?? model.workspace?.sessions[chat.id]
        return row?.startedAt ?? row?.updatedAt ?? nowMs()
    }
}
