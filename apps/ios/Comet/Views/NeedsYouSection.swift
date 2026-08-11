// The "Needs you" inbox and the orchestrator's pinned slot — gh#122, on the
// phone, where they matter most: the desktop critique's 8px dot vocabularies
// do not exist at arm's length at all. Every row here is words — WHO wants
// you, and one line of WHAT — and the empty state is words too.
//
// Pure presentation over `needsYou` / `orchestratorSlot` (BoardModels.swift,
// ports of `comet_proto::view::needs`). A tap opens the chat, which is where
// answering, retrying and reading all happen — and what marks the thread seen,
// the synced marker that clears the badge on every device.
//
// The one thing a row here writes is the orchestrator pin, on long-press
// (`OrchestratorPin.swift`, gh#166): both halves of it, in the words the
// desktop and the TUI use, since the slot below is only the pinned case of the
// same item.

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
                        .font(Theme.mono(Theme.textCaption))
                        .foregroundStyle(Theme.status(.settled))
                    Text(needsAllClear)
                        .font(Theme.sans(Theme.textDense))
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
                .buttonStyle(SelectRowButtonStyle())
                // A chat asking you something is a chat you are looking at, so
                // the pin is offered here too (gh#166) — an orchestrator is
                // usually pinned in the middle of the work that made you want
                // one, not from a settings page afterwards.
                .orchestratorPinMenu(chatId: need.chatId)
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
                .font(Theme.sans(Theme.textDense, weight: .medium))
                .foregroundStyle(Theme.textSubtle)
            // The count is the header's whole answer: how many things want me.
            if count > 0 {
                // round-ok: a count pill — the canvas draws it fully round
                Text("\(count)")
                    .font(Theme.mono(Theme.textCaption, weight: .medium))
                    .foregroundStyle(Theme.accent)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 1)
                    // round-ok: a count pill (ios.md B2.1)
                    .background(Theme.accent.opacity(Theme.statusBadgeTint), in: Capsule())
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

    /// Said in the ramp's vocabulary rather than by naming hues (gh#173): a
    /// question wants your eyes on a healthy run, a dead run is blocked, and a
    /// report is settled. The desktop's `render_need_row` splits them the same
    /// way and lands on the same three colours.
    private var accent: Color {
        switch need.kind {
        case .question: return Theme.status(.review)
        case .deadRun: return Theme.status(.blocked)
        case .report: return Theme.status(.settled)
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Text(need.kind.glyph)
                .font(Theme.mono(Theme.textCaption))
                .foregroundStyle(accent)
                .frame(width: 10)
            VStack(alignment: .leading, spacing: 2) {
                Text(need.who)
                    .font(Theme.sans(Theme.textBody, weight: .medium))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                Text(need.what)
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(Theme.textSubtle)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 13)
        .padding(.vertical, 11)
        .contentShape(RoundedRectangle(cornerRadius: Theme.radiusCard))
    }
}

/// The orchestrator's fixed slot, above Spaces: a pinned thread — ◆ identity,
/// the name, an unread badge, the latest report's preview. Rendered even
/// before it has ever spoken, so the place to look exists before the first
/// notice arrives. Absent only when no orchestrator is pinned.
struct OrchestratorSlotSection: View {
    @Environment(AppModel.self) private var model
    @Binding var path: [Route]

    var body: some View {
        if let slot = model.orchestratorSlotRow {
            Section {
                Button {
                    path.append(.chat(slot.chatId))
                } label: {
                    OrchestratorSlotView(slot: slot)
                }
                .buttonStyle(SelectRowButtonStyle())
                // The kill switch, and on the phone the ONLY one (gh#144).
                // This slot is often the only row a pinned chat has — its
                // session ends and its space shelf may never have listed it —
                // so without a menu here an operator who reopens the
                // orchestrator cannot unpin it from this device at all. The
                // desktop and the TUI grew this menu for exactly that; the
                // phone has no `comet-board routes defaults orchestrator_chat
                // --unset` to fall back to.
                //
                // The item, its words and its refusal are the same ones every
                // other chat's menu carries (gh#166) — this row is only the
                // pinned case of them.
                .orchestratorPinMenu(chatId: slot.chatId)
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 4, leading: 12, bottom: 1, trailing: 12))
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
                        .font(Theme.mono(Theme.textCaption))
                        .foregroundStyle(Theme.accent)
                }
            }
            .frame(width: 10, height: 16)
            VStack(alignment: .leading, spacing: 2) {
                Text(orchestratorName)
                    .font(Theme.sans(Theme.textBody, weight: .medium))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                // The latest report is the payload: brighter while unread.
                Text(slot.preview ?? orchestratorNoReports)
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(slot.unseen
                        ? Theme.text
                        : Theme.textSubtle)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer(minLength: 8)
            // Words on the right: "new" while something is unread, the time it
            // last spoke otherwise.
            if slot.unseen {
                // round-ok: a badge — the canvas draws it fully round
                Text("new")
                    .font(Theme.sans(Theme.textCaption, weight: .medium))
                    .foregroundStyle(Theme.status(.settled))
                    .padding(.horizontal, 7)
                    .padding(.vertical, 1)
                    // round-ok: the unread badge (ios.md B3.3)
                    .background(Theme.status(.settled).opacity(Theme.statusBadgeTint), in: Capsule())
            } else if let lastAt = slot.lastAt {
                Text(relativeTime(lastAt))
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(Theme.textSubtle)
                    .fixedSize()
            }
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 7)
        .contentShape(RoundedRectangle(cornerRadius: Theme.radiusCard))
    }
}
