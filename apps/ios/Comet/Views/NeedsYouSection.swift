// The "Needs you" inbox and the orchestrator's pinned slot — gh#122, on the
// phone, where they matter most: the desktop critique's 8px dot vocabularies
// do not exist at arm's length at all. Every row here is words — WHO wants
// you, and one line of WHAT — and the empty state is words too.
//
// Pure presentation over `needsYou` / `orchestratorSlot` (BoardModels.swift,
// ports of `comet_proto::view::needs`). A tap opens the chat, which is where
// answering, retrying and reading all happen — and what marks the thread seen,
// the synced marker that clears the badge on every device.

import SwiftUI

/// The first section on Home: everything waiting on a human, most owed first.
/// Never omitted — a quiet board says "Nothing needs you" instead of leaving
/// a gap, which is what licenses every section below it to stay calm.
struct NeedsYouSection: View {
    @Environment(AppModel.self) private var model
    @Binding var path: [Route]

    var body: some View {
        let needs = model.needsYouRows
        Section {
            if needs.isEmpty {
                HStack(spacing: 6) {
                    Text("✓")
                        .font(Theme.mono(11))
                        .foregroundStyle(ChatIndicator.completed.dotColor)
                    Text(needsAllClear)
                        .font(Theme.sans(12))
                        .foregroundStyle(Theme.textFaint)
                }
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 2, leading: 20, bottom: 2, trailing: 12))
            }
            ForEach(needs) { need in
                Button {
                    path.append(.chat(need.chatId))
                } label: {
                    NeedRowView(need: need)
                }
                .buttonStyle(PressWashButtonStyle())
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
            }
            .motionAnimation(Motion.resort, value: needs.map(\.id))
        } header: {
            header(count: needs.count)
        }
    }

    private func header(count: Int) -> some View {
        HStack(spacing: 6) {
            Text(needsYouTitle)
                .font(Theme.sans(11, weight: .medium))
                .foregroundStyle(Theme.textMuted.opacity(0.6))
            // The count is the header's whole answer: how many things want me.
            if count > 0 {
                Text("\(count)")
                    .font(Theme.mono(10, weight: .medium))
                    .foregroundStyle(Theme.accent)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(Theme.accent.opacity(0.14), in: Capsule())
            }
            Spacer(minLength: 0)
        }
        .textCase(nil)
        .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 3, trailing: 16))
    }
}

/// One thing waiting: the kind's glyph, WHO, and the one-line WHAT under it.
struct NeedRowView: View {
    let need: NeedRow

    private var accent: Color {
        switch need.kind {
        case .question: return Theme.accent
        case .deadRun: return Theme.danger
        case .report: return ChatIndicator.completed.dotColor
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Text(need.kind.glyph)
                .font(Theme.mono(11))
                .foregroundStyle(accent)
                .frame(width: 10)
            VStack(alignment: .leading, spacing: 2) {
                Text(need.who)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                Text(need.what)
                    .font(Theme.sans(11))
                    .foregroundStyle(Theme.textMuted.opacity(0.5))
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }
}

/// The orchestrator's fixed slot, above Spaces: a pinned thread — ◆ identity,
/// the name, an unread badge, the latest report's preview. Rendered even
/// before it has ever spoken, so the place to look exists before the first
/// notice arrives. Absent only when no orchestrator is pinned.
struct OrchestratorSlotSection: View {
    @Environment(AppModel.self) private var model
    @Binding var path: [Route]

    /// Why the unpin did not happen, when it did not. Nothing is said on
    /// success — the slot disappearing IS the board agreeing.
    @State private var failure: String?

    var body: some View {
        if let slot = model.orchestratorSlotRow {
            Section {
                Button {
                    path.append(.chat(slot.chatId))
                } label: {
                    OrchestratorSlotView(slot: slot)
                }
                .buttonStyle(PressWashButtonStyle())
                // The kill switch, and on the phone the ONLY one (gh#144).
                // This slot is often the only row a pinned chat has — its
                // session ends and its space shelf may never have listed it —
                // so without a menu here an operator who reopens the
                // orchestrator cannot unpin it from this device at all. The
                // desktop and the TUI grew this menu for exactly that; the
                // phone has no `comet-board routes defaults orchestrator_chat
                // --unset` to fall back to.
                .contextMenu {
                    Button("Unpin orchestrator", systemImage: "pin.slash",
                           role: .destructive) {
                        unpin()
                    }
                }
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 4, leading: 12, bottom: 1, trailing: 12))
                .alert("Couldn't unpin the orchestrator",
                       isPresented: Binding(get: { failure != nil },
                                            set: { if !$0 { failure = nil } })) {
                    Button("OK", role: .cancel) { failure = nil }
                } message: {
                    Text(failure ?? "")
                }
            }
        }
    }

    /// Clear `[defaults] orchestrator_chat` on the board's routing.toml.
    ///
    /// Nothing is applied optimistically: the board republishes the pin on the
    /// watch stream as the write lands, so a refusal must not leave the slot
    /// gone on a phone the box disagrees with.
    private func unpin() {
        Task {
            if let error = await model.setOrchestrator(chatId: nil) {
                failure = error
            }
        }
    }
}

struct OrchestratorSlotView: View {
    let slot: OrchestratorSlot

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            // The ◆ is identity; state rides beside it — the spinner while a
            // turn runs, so an 8h-old report can never be mistaken for one
            // running now.
            Group {
                if slot.indicator == .working {
                    MiniSpinner()
                } else {
                    Text("◆")
                        .font(Theme.mono(11))
                        .foregroundStyle(Theme.accent)
                }
            }
            .frame(width: 10, height: 16)
            VStack(alignment: .leading, spacing: 2) {
                Text(orchestratorName)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                // The latest report is the payload: brighter while unread.
                Text(slot.preview ?? orchestratorNoReports)
                    .font(Theme.sans(11))
                    .foregroundStyle(slot.unseen
                        ? Theme.text.opacity(0.7)
                        : Theme.textMuted.opacity(0.5))
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer(minLength: 8)
            // Words on the right: "new" while something is unread, the time it
            // last spoke otherwise.
            if slot.unseen {
                Text("new")
                    .font(Theme.sans(10, weight: .medium))
                    .foregroundStyle(ChatIndicator.completed.dotColor)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(ChatIndicator.completed.dotColor.opacity(0.14), in: Capsule())
            } else if let lastAt = slot.lastAt {
                Text(relativeTime(lastAt))
                    .font(Theme.sans(11))
                    .foregroundStyle(Theme.textMuted.opacity(0.5))
                    .fixedSize()
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }
}
