// Composer — the floating glass shell, a port of the old mobile app's
// composer (compact↔expanded morph, 36pt controls, focus-widen) carrying the
// desktop's Send→Steer→Stop semantics: live run + text = steer (same
// up-arrow), live run + empty = stop.
//
// The compact→expanded flip is deterministic (newline or >26 chars), NOT
// content-size measured — measurement oscillates at the boundary.

import PhotosUI
import SwiftUI

/// Shared glass shell + input + action row. `chips` (leading accessory views)
/// force the expanded layout — the desktop keeps new-session composers
/// expanded because the pickers need the full row.
struct ComposerShell<Chips: View>: View {
    @Binding var draft: String
    var placeholder = "Message"
    var sendEnabled: Bool
    var showStop: Bool
    var busy = false
    var onSend: () -> Void
    var onStop: () -> Void = {}
    /// Staged attachments, drawn inside the pill. Non-empty forces the
    /// expanded layout, like chips do.
    var attachments: [StagedAttachment] = []
    /// The attach menu's actions; nil hides the attach button entirely (the
    /// new-session composer has no chat to upload to yet).
    var attachActions: AttachActions? = nil
    var onRemoveAttachment: (String) -> Void = { _ in }
    var onRetryAttachment: (String) -> Void = { _ in }
    @ViewBuilder var chips: Chips

    @FocusState private var focused: Bool

    /// What the `+` button offers. Photos and Files are separate pickers on
    /// purpose: `PHPickerViewController` is the only one that shows the photo
    /// library, and `UIDocumentPicker` is the only one that shows everything
    /// else — a PDF, a log, a `.dc.html` design file (gh#535).
    struct AttachActions {
        var photos: () -> Void
        var files: () -> Void
        var paste: () -> Void
    }

    private var expanded: Bool {
        Chips.self != EmptyView.self || !attachments.isEmpty
            || draft.contains("\n") || draft.count > 26
    }

    // Switching between VStack/HStack via AnyLayout (rather than an if/else
    // that swaps container types) keeps `input`'s view identity stable across
    // the compact↔expanded flip — an if/else here would tear down and rebuild
    // the TextField, dropping keyboard focus mid-type.
    private var shellLayout: AnyLayout {
        expanded
            ? AnyLayout(VStackLayout(alignment: .leading, spacing: 0))
            : AnyLayout(HStackLayout(alignment: .center, spacing: 12))
    }

    var body: some View {
        shellLayout {
            if expanded, !attachments.isEmpty {
                AttachmentStripView(attachments: attachments,
                                    remove: onRemoveAttachment,
                                    retry: onRetryAttachment)
                    .padding(.horizontal, 16)
                    .padding(.top, 10)
            }
            if !expanded, attachActions != nil {
                attachButton
                    .padding(.leading, 7)
            }
            input
                .padding(.horizontal, expanded ? 20 : 0)
                .padding(.leading, expanded ? 0 : (attachActions == nil ? 20 : 4))
                .padding(.top, expanded ? 15 : 0)
                .padding(.vertical, expanded ? 0 : 15)
            if expanded {
                HStack(spacing: 10) {
                    if attachActions != nil {
                        attachButton
                    }
                    // Chips scroll; the send button stays pinned.
                    ScrollView(.horizontal, showsIndicators: false) {
                        HStack(spacing: 8) {
                            chips
                        }
                    }
                    .scrollClipDisabled(false)
                    actionButton
                }
                .padding(.horizontal, 10)
                .padding(.top, 10)
                .padding(.bottom, 10)
            } else {
                actionButton
                    .padding(.trailing, 7)
            }
        }
        .background(Theme.chip, in: RoundedRectangle(cornerRadius: Theme.radiusCard))
        .glassEffect(.regular.interactive(), in: RoundedRectangle(cornerRadius: Theme.radiusCard))
        .overlay(RoundedRectangle(cornerRadius: Theme.radiusCard).strokeBorder(Theme.border, lineWidth: 1))
        // Focus-widen: margins pull in slightly while typing (chat-session.tsx).
        .padding(.horizontal, focused ? 10 : 16)
        .motionAnimation(Motion.resize, value: focused)
        .motionAnimation(Motion.collapse, value: expanded)
    }

    private var input: some View {
        TextField(placeholder, text: $draft, axis: .vertical)
            .font(Theme.sans(Theme.textProse))
            .foregroundStyle(Theme.text)
            .tint(Theme.text)
            .lineLimit(1...7)
            .focused($focused)
    }

    /// round-ok: the send button — the one round thing on screen, and it is the
    /// one you press. Named rather than written twice so the shape and the
    /// reason for it stay together (gh#174).
    private var sendShape: Circle { Circle() }

    private var attachButton: some View {
        Menu {
            if let actions = attachActions {
                Button {
                    actions.photos()
                } label: {
                    Label("Photos", systemImage: "photo.on.rectangle")
                }
                Button {
                    actions.files()
                } label: {
                    Label("Files", systemImage: "folder")
                }
                Button {
                    actions.paste()
                } label: {
                    Label("Paste", systemImage: "doc.on.clipboard")
                }
            }
        } label: {
            Image(systemName: "plus")
                .font(.system(size: 15, weight: .medium))
                .foregroundStyle(Theme.textMuted)
                .frame(width: 36, height: 36)
                // round-ok: the attach button is the send button's twin — same
                // 36pt control, same shape, one on each end of the pill.
                .background(Theme.chip, in: Circle())
                // round-ok: the same button's hit area.
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .disabled(busy)
        .accessibilityLabel("Attach")
    }

    /// Attachments count as content: a photo-only send is a send, never a stop.
    private var hasContent: Bool {
        !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !attachments.isEmpty
    }

    private var actionButton: some View {
        Button {
            if showStop, !hasContent {
                UIImpactFeedbackGenerator(style: .medium).impactOccurred()
                onStop()
            } else {
                UIImpactFeedbackGenerator(style: .light).impactOccurred()
                onSend()
            }
        } label: {
            Group {
                if busy {
                    ProgressView()
                        .controlSize(.small)
                        .tint(Theme.bg)
                } else if showStop, !hasContent {
                    // scale-ok: the stop glyph DRAWN inside the send button —
                    // a 12pt square whose corner is part of the mark
                    RoundedRectangle(cornerRadius: 3.5)
                        .fill(Theme.bg)
                        .frame(width: 12, height: 12)
                } else {
                    Image(systemName: "arrow.up")
                        .font(.system(size: 16, weight: .semibold))
                        .foregroundStyle(buttonActive ? Theme.bg : Theme.textFaint)
                }
            }
            .frame(width: 36, height: 36)
            .background(buttonActive ? AnyShapeStyle(Theme.text) : AnyShapeStyle(Theme.chip),
                        in: sendShape)
            .contentShape(sendShape)
        }
        .buttonStyle(.plain)
        .disabled(!buttonActive)
        .motionAnimation(Motion.fadeQuick, value: showStop)
    }

    private var buttonActive: Bool {
        if showStop, !hasContent { return true }
        return sendEnabled && hasContent && !busy
    }
}

/// The live-chat composer: config is locked once the chat exists, so no chips —
/// just the input and the morphing action button.
struct ComposerView: View {
    let store: SessionStore
    let chat: Chat
    let runLive: Bool
    var searchContext: (String) async -> ContextSearch = {
        _ in ContextSearch(matches: [], truncated: false)
    }

    /// The composer's contents are the CHAT's, not this view's (gh#536). A
    /// `@State` string here died every time the view did — navigating away,
    /// the question panel taking the composer's place, the system reclaiming a
    /// backgrounded app — and took a half-written prompt with it.
    private var drafts: DraftStore { .shared }
    private var draftKey: String { DraftStore.key(chat: chat.id) }

    private var text: String {
        get { drafts.text(for: draftKey) }
        nonmutating set { drafts.setText(newValue, for: draftKey) }
    }

    private var context: [ContextRef] {
        get { drafts.context(for: draftKey) }
        nonmutating set { drafts.setContext(newValue, for: draftKey) }
    }

    @State private var contextMatches: [ContextRef] = []
    @State private var contextCheckoutId: String?
    @State private var showContextPicker = false
    @State private var editing: FollowupRow?
    @State private var editText = ""
    @State private var followupFailure: String?
    @State private var attachments: [StagedAttachment] = []
    @State private var pickerItems: [PhotosPickerItem] = []
    @State private var showPhotoPicker = false
    @State private var showFilePicker = false
    @State private var uploading = false
    /// Why the last send or staging attempt did not go through, shown above
    /// the pill and cleared by the next one that does. A file that did not make
    /// it — refused at the picker, lost on the way to the host, or written to a
    /// ledger that refused it — is never a silent drop (gh#535).
    @State private var sendNotice: String?

    var body: some View {
        VStack(spacing: 8) {
            if !store.followups.isEmpty || store.followupsPaused {
                queueTray
            }
            if let sendNotice {
                noticeLine(sendNotice)
            }
            ComposerShell(
                draft: drafts.textBinding(for: draftKey),
                sendEnabled: true,
                showStop: runLive,
                busy: uploading,
                onSend: send,
                onStop: { store.sendInterrupt() },
                attachments: attachments,
                attachActions: .init(
                    photos: { showPhotoPicker = true },
                    files: { showFilePicker = true },
                    paste: pasteAttachment
                ),
                onRemoveAttachment: { id in attachments.removeAll { $0.id == id } },
                onRetryAttachment: { _ in send() }
            ) {
                ForEach(context) { reference in
                    contextChip(reference)
                }
            }
            if runLive && !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                Button {
                    queue()
                } label: {
                    Label("Do after this turn", systemImage: "clock.badge.plus")
                        .font(Theme.sans(Theme.textCaption, weight: .medium))
                }
                .buttonStyle(.plain)
                .foregroundStyle(Theme.textMuted)
                .accessibilityLabel("Do after this turn")
            }
        }
        .onChange(of: text) { _, value in searchAtToken(value) }
        .sheet(isPresented: $showContextPicker) { contextPicker }
        .sheet(item: $editing) { row in editSheet(row) }
        .photosPicker(isPresented: $showPhotoPicker, selection: $pickerItems,
                      maxSelectionCount: 8, matching: .images)
        // `.data` rather than a type list: a PDF, a log, a `.dc.html` design
        // file and a `.txt` are all things somebody has wanted to hand an agent
        // from the phone, and enumerating types is how one of them stays
        // un-attachable (gh#535). The size cap is what bounds this, not the
        // type.
        .fileImporter(isPresented: $showFilePicker, allowedContentTypes: [.data],
                      allowsMultipleSelection: true) { result in
            stageFiles(result)
        }
        .onChange(of: pickerItems) { _, items in
            guard !items.isEmpty else { return }
            stagePhotos(items)
        }
        .alert("Couldn’t save follow-up", isPresented: Binding(
            get: { followupFailure != nil },
            set: { if !$0 { followupFailure = nil } }
        )) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(followupFailure ?? "The durable command could not be written.")
        }
    }

    /// The failure line above the pill: what did not make it, and what to do.
    private func noticeLine(_ message: String) -> some View {
        HStack(alignment: .top, spacing: 6) {
            Image(systemName: "exclamationmark.triangle.fill")
                .font(.system(size: 11))
            Text(message)
                .font(Theme.sans(Theme.textCaption))
                .lineLimit(3)
            Spacer(minLength: 0)
        }
        .foregroundStyle(Theme.dangerText)
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .background(Theme.danger.opacity(0.12), in: RoundedRectangle(cornerRadius: Theme.radiusChip))
        .padding(.horizontal, 16)
        .accessibilityLabel("Send problem: \(message)")
    }

    // MARK: Staging

    /// Load picked photos into staged attachments (HEIC transcodes to JPEG;
    /// unsupported or oversized picks surface on the error line).
    private func stagePhotos(_ items: [PhotosPickerItem]) {
        Task { @MainActor in
            var failed = 0
            for item in items {
                guard let data = try? await item.loadTransferable(type: Data.self),
                      let staged = StagedAttachment.stagePhoto(data: data) else {
                    failed += 1
                    continue
                }
                attachments.append(staged)
            }
            pickerItems = []
            sendNotice = failed == 0 ? nil : failed == 1
                ? "One photo couldn’t be attached (unreadable, or over 24 MB)."
                : "\(failed) photos couldn’t be attached (unreadable, or over 24 MB)."
        }
    }

    /// Load documents picked from Files. Each URL is security-scoped: the read
    /// has to happen between start/stop, and it has to happen HERE — the bytes
    /// are what uploads, and the phone's copy of the file is gone by the time
    /// the send runs.
    private func stageFiles(_ result: Result<[URL], Error>) {
        switch result {
        case .failure(let error):
            sendNotice = "Couldn’t open that file — \(error.localizedDescription)"
        case .success(let urls):
            var refused: [String] = []
            for url in urls {
                let scoped = url.startAccessingSecurityScopedResource()
                defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                guard let data = try? Data(contentsOf: url),
                      let staged = StagedAttachment.stageFile(data: data,
                                                              name: url.lastPathComponent) else {
                    refused.append(url.lastPathComponent)
                    continue
                }
                attachments.append(staged)
            }
            sendNotice = refused.isEmpty
                ? nil
                : "Couldn’t attach \(refused.joined(separator: ", ")) (unreadable, empty, or over 24 MB)."
        }
    }

    /// Paste whatever the pasteboard holds — a screenshot copied out of another
    /// app, a file copied in Files, or plain text saved as `.txt`.
    private func pasteAttachment() {
        guard let staged = stageFromPasteboard() else {
            sendNotice = "Nothing on the clipboard could be attached."
            return
        }
        attachments.append(staged)
        sendNotice = nil
    }

    // MARK: Send

    private func send() {
        let prompt = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !prompt.isEmpty || !attachments.isEmpty else { return }
        guard !uploading else { return }
        if attachments.isEmpty {
            guard deliver(content: prompt, paths: []) else {
                sendNotice = refusedNotice
                return
            }
            clearDraft()
            return
        }
        Task { @MainActor in
            guard let paths = await uploadStaged(verb: "sent") else { return }
            // Gated on the durable append, exactly as the follow-up writes are:
            // a send the ledger refused must leave the draft AND the staged
            // files where they were. Losing an upload to a lost socket after
            // the bytes already crossed the relay is the silent drop gh#535 is
            // about; the files keep their committed paths, so retrying is one
            // ledger write, not five uploads.
            guard deliver(content: withAttachments(text: prompt, paths: paths),
                          paths: paths) else {
                sendNotice = refusedNotice
                return
            }
            attachments = []
            sendNotice = nil
            clearDraft()
        }
    }

    /// What a refused durable append says. The files are still staged and still
    /// committed on the host; only the ledger entry is missing.
    private var refusedNotice: String {
        "Not sent — the durable command could not be written. "
            + "Your draft and attachments are still here. Tap send to retry."
    }

    /// Get every staged file onto the chat's host, or report why not.
    ///
    /// Upload first, send after: the trailer needs committed paths, and a doc
    /// entry must never point at files that are not on the host. If any one of
    /// them cannot get there this returns nil and the caller sends NOTHING —
    /// the draft and the files stay put, the error line names what failed, and
    /// pressing send again retries only those (a committed path is kept, so a
    /// five-file send that lost one does not re-push the other four).
    ///
    /// Indexed by id, not by position: the strip's × stays live while chunks
    /// stream, so the array can shrink under this loop.
    private func uploadStaged(verb: String) async -> [String]? {
        uploading = true
        sendNotice = nil
        defer { uploading = false }
        var failures: [String] = []
        for staged in attachments where staged.state.committedPath == nil {
            guard let start = attachments.firstIndex(where: { $0.id == staged.id }) else { continue }
            attachments[start].state = .uploading
            do {
                let path = try await store.uploadAttachment(name: staged.name, data: staged.data)
                guard let landed = attachments.firstIndex(where: { $0.id == staged.id }) else {
                    continue  // removed mid-upload; the host keeps the file, we forget it
                }
                attachments[landed].state = .uploaded(path: path)
                if staged.isImage {
                    // Seed the cache so our own bubble renders from local bytes
                    // instead of a round-trip back to the host.
                    AttachmentImageCache.shared.seed(deviceId: chat.deviceId, path: path,
                                                     name: staged.name, data: staged.data)
                }
            } catch {
                let reason = (error as? LocalizedError)?.errorDescription
                    ?? error.localizedDescription
                if let failed = attachments.firstIndex(where: { $0.id == staged.id }) {
                    attachments[failed].state = .failed(reason: reason)
                }
                failures.append("\(staged.name) — \(reason)")
            }
        }
        guard failures.isEmpty else {
            sendNotice = failures.count == 1
                ? "Not \(verb). \(failures[0]). Tap send to retry."
                : "Not \(verb). \(failures.count) attachments didn’t reach the host: "
                    + failures.joined(separator: "; ") + ". Tap send to retry."
            return nil
        }
        return attachments.compactMap(\.state.committedPath)
    }

    /// Returns whether the durable command landed — the caller clears nothing
    /// until it did.
    private func deliver(content: String, paths: [String]) -> Bool {
        if runLive {
            // A steer carries no structured attachment list — the trailer is
            // the transport, and the agent opens the paths from the host's
            // disk exactly as it does for a fresh run.
            return store.sendSteer(prompt: content, context: context)
        }
        return store.sendRun(prompt: content, chat: chat, context: context, attachments: paths)
    }

    /// Called only once a send is known to have landed (gh#535): the draft is
    /// the chat's, so clearing it on a write the ledger refused would lose a
    /// prompt that was never sent.
    private func clearDraft() {
        drafts.clear(draftKey)
        // The clear above is unconditional, so a prompt left sitting in the
        // composer after a successful send is not this path failing to run —
        // it is the text view writing the pre-send string back. A focused
        // multiline TextField commits pending autocorrect/marked text through
        // the binding AFTER a programmatic change, which restores the prompt.
        // Re-clear once that has drained; a keystroke can't land inside the
        // same main-actor turn, so this can never eat real input.
        Task { @MainActor in text = "" }
    }

    private func queue() {
        let prompt = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !prompt.isEmpty else { return }
        guard !uploading else { return }
        // A queued follow-up's files are uploaded now, not when its turn comes:
        // the phone that holds the bytes may be asleep by then, and the row has
        // to name paths that already exist on the host.
        Task { @MainActor in
            var paths: [String] = []
            if !attachments.isEmpty {
                guard let uploaded = await uploadStaged(verb: "queued") else { return }
                paths = uploaded
            }
            // Every clear below is on the far side of this guard: a refused
            // write leaves the draft, the context refs AND the staged files
            // exactly as they were.
            guard store.queueFollowup(prompt: withAttachments(text: prompt, paths: paths),
                                      context: context, attachments: paths) else {
                followupFailure = "Your draft and attachments are still here. "
                    + "Try again after reconnecting."
                return
            }
            attachments = []
            sendNotice = nil
            clearDraft()
        }
    }

    private func searchAtToken(_ value: String) {
        guard let token = value.split(whereSeparator: { $0.isWhitespace }).last,
              token.first == "@", token.count > 1 else { return }
        let query = String(token.dropFirst())
        Task {
            let result = await searchContext(query)
            guard text.hasSuffix("@\(query)") else { return }
            contextCheckoutId = result.checkoutId
            contextMatches = result.matches
            showContextPicker = !result.matches.isEmpty
        }
    }

    private var contextPicker: some View {
        NavigationStack {
            List(contextMatches) { match in
                Button {
                    selectContext(match)
                } label: {
                    Label(match.path,
                          systemImage: match.kind == .directory ? "folder" : "doc")
                }
            }
            .navigationTitle("Reference from checkout")
            .navigationBarTitleDisplayMode(.inline)
        }
        .presentationDetents([.medium, .large])
    }

    private func selectContext(_ reference: ContextRef) {
        var reference = reference
        reference.checkoutId = contextCheckoutId
        if let token = text.split(whereSeparator: { $0.isWhitespace }).last,
           token.first == "@", let range = text.range(of: String(token), options: .backwards) {
            text.replaceSubrange(range, with: "@\(reference.path) ")
        }
        if !context.contains(reference) { context.append(reference) }
        showContextPicker = false
    }

    private func contextChip(_ reference: ContextRef) -> some View {
        HStack(spacing: 4) {
            Image(systemName: reference.kind == .directory ? "folder" : "doc")
            Text((reference.path as NSString).lastPathComponent).lineLimit(1)
            Button { context.removeAll { $0 == reference } } label: {
                Image(systemName: "xmark")
            }
            .buttonStyle(.plain)
        }
        .font(Theme.sans(Theme.textCaption))
        .padding(.horizontal, 8)
        .frame(height: 26)
        .background(Theme.chip, in: RoundedRectangle(cornerRadius: Theme.radiusChip))
        .accessibilityLabel("\(reference.kind == .directory ? "Directory" : "File") reference \(reference.path)")
    }

    private var queueTray: some View {
        VStack(spacing: 4) {
            HStack {
                Text("\(store.followups.count) after this turn")
                    .font(Theme.sans(Theme.textCaption, weight: .medium))
                Spacer()
                Button(store.followupsPaused ? "Resume" : "Pause") {
                    saveQueueControl(store.setFollowupsPaused(!store.followupsPaused))
                }
                .font(Theme.sans(Theme.textCaption))
            }
            ForEach(Array(store.followups.enumerated()), id: \.element.id) { index, row in
                HStack(spacing: 8) {
                    Text("\(index + 1)").foregroundStyle(Theme.textFaint)
                    Button { editText = row.prompt; editing = row } label: {
                        Text(row.prompt).lineLimit(1).frame(maxWidth: .infinity, alignment: .leading)
                    }
                    .buttonStyle(.plain)
                    Button { move(row, index: index, delta: -1) } label: { Image(systemName: "arrow.up") }
                        .disabled(index == 0)
                    Button { move(row, index: index, delta: 1) } label: { Image(systemName: "arrow.down") }
                        .disabled(index + 1 == store.followups.count)
                    Button { saveQueueControl(store.runNext(id: row.id)) } label: { Image(systemName: "play") }
                        .accessibilityLabel("Run next")
                    Button(role: .destructive) { saveQueueControl(store.removeFollowup(id: row.id)) } label: {
                        Image(systemName: "xmark")
                    }
                }
                .font(Theme.sans(Theme.textCaption))
            }
        }
        .padding(10)
        .background(Theme.chip, in: RoundedRectangle(cornerRadius: Theme.radiusCard))
        .padding(.horizontal, 16)
    }

    private func move(_ row: FollowupRow, index: Int, delta: Int) {
        let target = index + delta
        guard store.followups.indices.contains(target) else { return }
        let after: String? = target == 0 ? nil : store.followups[target - (delta > 0 ? 0 : 1)].id
        saveQueueControl(store.moveFollowup(id: row.id, after: after))
    }

    private func saveQueueControl(_ saved: Bool) {
        if !saved {
            followupFailure = "The queue change was not saved. Try again after reconnecting."
        }
    }

    private func editSheet(_ row: FollowupRow) -> some View {
        NavigationStack {
            TextEditor(text: $editText).padding()
                .navigationTitle("Edit follow-up")
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Cancel") { editing = nil }
                    }
                    ToolbarItem(placement: .confirmationAction) {
                        Button("Save") {
                            if store.editFollowup(id: row.id, prompt: editText, context: row.context) {
                                editing = nil
                            } else {
                                followupFailure = "Your edit is still open. Try again after reconnecting."
                            }
                        }
                    }
                }
        }
    }
}

// MARK: - Question panel (composer.rs Wizard)

struct QuestionPanel: View {
    let requestId: String
    let questions: [UserInputQuestion]
    let respond: (String, [UserInputAnswer]) -> Void

    @State private var page = 0
    @State private var picked: [String: Set<String>] = [:]  // questionId → labels
    @State private var typed: [String: String] = [:]
    @State private var autoAdvanceTask: Task<Void, Never>?

    var body: some View {
        // `questions[min(page, count - 1)]` traps on an empty list (count - 1
        // is -1). A request whose questions fail to decode reaches here empty,
        // so this crashed the app on any session holding one.
        if questions.isEmpty {
            EmptyView()
        } else {
            panel(for: questions[min(max(page, 0), questions.count - 1)])
        }
    }

    private func panel(for question: UserInputQuestion) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text(question.header.uppercased())
                    .font(Theme.sans(Theme.textCaption, weight: .medium))
                    .kerning(1)
                    .foregroundStyle(Theme.textSubtle)
                Spacer()
                if questions.count > 1 {
                    Text("\(page + 1)/\(questions.count)")
                        .font(Theme.sans(Theme.textCaption))
                        .foregroundStyle(Theme.textMuted)
                        .padding(.horizontal, 6)
                        .frame(height: 20)
                        .background(Theme.chip, in: RoundedRectangle(cornerRadius: Theme.radiusChip))
                }
            }

            Text(question.question)
                .font(Theme.sans(Theme.textTitle, weight: .medium))
                .foregroundStyle(Theme.text)
                .fixedSize(horizontal: false, vertical: true)

            if question.multiSelect == true {
                Text("Select one or more options.")
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(Theme.textMuted)
            }

            VStack(spacing: 4) {
                ForEach(Array(question.options.enumerated()), id: \.offset) { ix, option in
                    optionRow(question: question, ix: ix, option: option)
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Rectangle().fill(Theme.border).frame(height: 1)
                TextField("Or type your own answer", text: Binding(
                    get: { typed[question.id] ?? "" },
                    set: { typed[question.id] = $0 }
                ))
                .font(Theme.sans(Theme.textBody))
                .foregroundStyle(Theme.text)
                .padding(.top, 6)
            }

            HStack {
                if page > 0 {
                    Button("Back") {
                        page -= 1
                    }
                    .font(Theme.sans(Theme.textBody, weight: .medium))
                    .foregroundStyle(Theme.textMuted)
                }
                Spacer()
                Button(page < questions.count - 1 ? "Next" : "Submit") {
                    advance()
                }
                .font(Theme.sans(Theme.textBody, weight: .medium))
                .foregroundStyle(Theme.bg)
                .padding(.horizontal, 16)
                .frame(height: 34)
                .background(Theme.text, in: RoundedRectangle(cornerRadius: Theme.radiusRow))
                .opacity(canAdvance(question) ? 1 : 0.4)
                .disabled(!canAdvance(question))
            }
        }
        .padding(16)
        .glassEffect(.regular, in: RoundedRectangle(cornerRadius: Theme.radiusCard))
        .overlay(RoundedRectangle(cornerRadius: Theme.radiusCard).strokeBorder(Theme.border, lineWidth: 1))
        .padding(.horizontal, 12)
        .transition(.opacity)
    }

    private func optionRow(question: UserInputQuestion, ix: Int, option: String) -> some View {
        let isPicked = (typed[question.id] ?? "").isEmpty
            && picked[question.id, default: []].contains(option)
        return Button {
            pick(question: question, option: option)
        } label: {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(option)
                        .font(Theme.sans(Theme.textBody, weight: .medium))
                        .foregroundStyle(Theme.text)
                        .multilineTextAlignment(.leading)
                }
                Spacer(minLength: 0)
                if ix < 9 {
                    Text("\(ix + 1)")
                        .font(Theme.sans(Theme.textCaption))
                        .foregroundStyle(Theme.textMuted)
                        .frame(width: 22, height: 22)
                        .background(Theme.chip, in: RoundedRectangle(cornerRadius: Theme.radiusChip))
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .background(isPicked ? Theme.elementActive : Theme.chip,
                        in: RoundedRectangle(cornerRadius: Theme.radiusRow))
            .overlay(RoundedRectangle(cornerRadius: Theme.radiusRow)
                .strokeBorder(isPicked ? Theme.borderStrong : .clear, lineWidth: 1))
        }
        .buttonStyle(.plain)
    }

    private func pick(question: UserInputQuestion, option: String) {
        typed[question.id] = nil
        if question.multiSelect == true {
            var set = picked[question.id, default: []]
            if set.contains(option) { set.remove(option) } else { set.insert(option) }
            picked[question.id] = set
        } else {
            picked[question.id] = [option]
            // Single-select auto-advances after 220ms (AUTO_ADVANCE_MS).
            autoAdvanceTask?.cancel()
            autoAdvanceTask = Task {
                try? await Task.sleep(nanoseconds: 220_000_000)
                guard !Task.isCancelled else { return }
                advance()
            }
        }
    }

    private func canAdvance(_ question: UserInputQuestion) -> Bool {
        !(typed[question.id] ?? "").isEmpty || !picked[question.id, default: []].isEmpty
    }

    private func advance() {
        let question = questions[min(page, questions.count - 1)]
        guard canAdvance(question) else { return }
        if page < questions.count - 1 {
            page += 1
            return
        }
        let answers = questions.map { q -> UserInputAnswer in
            let typedAnswer = (typed[q.id] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if !typedAnswer.isEmpty {
                return UserInputAnswer(questionId: q.id, labels: [typedAnswer])
            }
            return UserInputAnswer(questionId: q.id, labels: Array(picked[q.id, default: []]))
        }
        respond(requestId, answers)
    }
}
