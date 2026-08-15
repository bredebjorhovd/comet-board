// Composer — the floating glass shell, a port of the old mobile app's
// composer (compact↔expanded morph, 36pt controls, focus-widen) carrying the
// desktop's Send→Steer→Stop semantics: live run + text = steer (same
// up-arrow), live run + empty = stop.
//
// The compact→expanded flip is deterministic (newline or >26 chars), NOT
// content-size measured — measurement oscillates at the boundary.

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
    @ViewBuilder var chips: Chips

    @FocusState private var focused: Bool

    private var expanded: Bool {
        Chips.self != EmptyView.self || draft.contains("\n") || draft.count > 26
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
            input
                .padding(.horizontal, expanded ? 20 : 0)
                .padding(.leading, expanded ? 0 : 20)
                .padding(.top, expanded ? 15 : 0)
                .padding(.vertical, expanded ? 0 : 15)
            if expanded {
                HStack(spacing: 10) {
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

    private var actionButton: some View {
        Button {
            if showStop, draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
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
                } else if showStop, draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
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
        if showStop, draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return true }
        return sendEnabled && !draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty && !busy
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

    @State private var text = ""
    @State private var context: [ContextRef] = []
    @State private var contextMatches: [ContextRef] = []
    @State private var contextCheckoutId: String?
    @State private var showContextPicker = false
    @State private var editing: FollowupRow?
    @State private var editText = ""
    @State private var followupFailure: String?

    var body: some View {
        VStack(spacing: 8) {
            if !store.followups.isEmpty || store.followupsPaused {
                queueTray
            }
            ComposerShell(
                draft: $text,
                sendEnabled: true,
                showStop: runLive,
                onSend: send,
                onStop: { store.sendInterrupt() }
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
        .alert("Couldn’t save follow-up", isPresented: Binding(
            get: { followupFailure != nil },
            set: { if !$0 { followupFailure = nil } }
        )) {
            Button("OK", role: .cancel) {}
        } message: {
            Text(followupFailure ?? "The durable command could not be written.")
        }
    }

    private func send() {
        let prompt = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !prompt.isEmpty else { return }
        if runLive {
            store.sendSteer(prompt: prompt, context: context)
        } else {
            store.sendRun(prompt: prompt, chat: chat, context: context)
        }
        text = ""
        context = []
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
        guard store.queueFollowup(prompt: prompt, context: context) else {
            followupFailure = "Your draft is still here. Try again after reconnecting."
            return
        }
        text = ""
        context = []
        Task { @MainActor in text = "" }
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
                    store.setFollowupsPaused(!store.followupsPaused)
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
                    Button { store.runNext(id: row.id) } label: { Image(systemName: "play") }
                        .accessibilityLabel("Run next")
                    Button(role: .destructive) { store.removeFollowup(id: row.id) } label: {
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
        store.moveFollowup(id: row.id, after: after)
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
