// The Active group on the home screen — gh#123's one live list, phone-shaped.
//
// gh#103 put live board attempts on the home screen and gh#117 added the runs
// the board never released, each under its own header — a split by how a run
// started, which is a mechanism distinction the reader's question does not
// contain: "what is working, and which of it wants me" has one answer. So one
// group, needs-you first, then working, blind to origin in the order and
// visible on the row — an attempt wears its issue identifier as a chip and
// keeps its branch and cap, an unmanaged run is its own bare title.
//
// On a phone this is the surface that matters most: it is the whole answer to
// "are they even alive", which otherwise took an ssh session and `pgrep`.
// Pure presentation: everything drawn was already streamed, and a tap opens
// the chat and nothing else — retry and cancel live on the board, which is
// the deep view and has the confirmations. The one write a row here offers is
// the orchestrator pin on long-press (`OrchestratorPin.swift`, gh#166), and it
// carries its own confirmation for the same reason.

import SwiftUI

struct ActiveSection: View {
    @Environment(AppModel.self) private var model
    @Binding var path: [Route]

    /// One tick a second while the section is on screen: the rows carry the
    /// start instant, not the age, so a *blocked* row — which animates
    /// nothing — would otherwise sit at whatever the last frame caught.
    @State private var now = Date()

    var body: some View {
        let active = model.activeChats
        if !active.isEmpty {
            Section {
                ForEach(active) { row in
                    Button {
                        path.append(.chat(row.chatId))
                    } label: {
                        switch row {
                        case .agent(let agent):
                            AgentRowView(agent: agent, now: now)
                        case .unmanaged(let run):
                            RunningRowView(row: run, now: now)
                        }
                    }
                    .buttonStyle(PressWashButtonStyle())
                    // Long-press to pin this chat as the board's orchestrator
                    // (gh#166). An orchestrator is a long-lived chat you opened
                    // yourself, which is exactly what an `.unmanaged` row is —
                    // so more often than not it is already running, and this is
                    // the row it has. The item declines to appear on an
                    // `.agent` row: the board dispatched that one, and pinning
                    // an attempt is the one thing `comet-board doctor` says can
                    // be wrong with a pin.
                    .orchestratorPinMenu(chatId: row.chatId)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
                    .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
                }
                .motionAnimation(Motion.resort, value: active.map(\.id))
            } header: {
                header(needing: activeNeedingAttention(active))
            }
            .task {
                while !Task.isCancelled {
                    try? await Task.sleep(nanoseconds: 1_000_000_000)
                    now = Date()
                }
            }
        }
    }

    /// The count is what you look for first: three running, one of them stuck
    /// on a question you have not answered.
    private func header(needing: Int) -> some View {
        HStack(spacing: 6) {
            Text("Active")
                .font(Theme.sans(Theme.textCaption, weight: .medium))
                .foregroundStyle(Theme.textSubtle)
            if needing > 0 {
                Text("\(needing)")
                    .font(Theme.mono(Theme.textCaption, weight: .medium))
                    .foregroundStyle(Theme.dangerText)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(Theme.danger.opacity(0.14),
                                in: RoundedRectangle(cornerRadius: Theme.radiusChip))
            }
            Spacer(minLength: 0)
        }
        .textCase(nil)
        .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 3, trailing: 16))
    }
}

/// One unmanaged run (gh#117): state glyph, the chat's own title, elapsed since
/// the run started. One line — no branch is promised and no issue names it, so
/// a second line would be an empty one. The bare title is also the origin
/// telling: an attempt wears an identifier chip, and this row deliberately
/// does not.
struct RunningRowView: View {
    let row: RunningRow
    let now: Date

    private var subline: Color { Theme.textSubtle }

    private var accent: Color { agentStateColor(row.state) }

    var body: some View {
        HStack(spacing: 8) {
            Text(row.state.glyph)
                .font(Theme.mono(Theme.textCaption))
                .foregroundStyle(accent)
                .frame(width: 10)
            Text(row.title)
                .font(Theme.sans(Theme.textBody, weight: .medium))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
            Spacer(minLength: 8)
            // A blocked run says so in words: it has no issue identifier to be
            // recognised by, so the glyph alone would be doing too much.
            if row.state.needsAttention {
                Text(row.state.label)
                    .font(Theme.sans(Theme.textCaption))
                    .foregroundStyle(accent)
                    .fixedSize()
            }
            if let elapsed = row.elapsedLabel(now: now) {
                Text(elapsed)
                    .font(Theme.mono(Theme.textCaption))
                    .foregroundStyle(subline)
                    .fixedSize()
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
        .contentShape(RoundedRectangle(cornerRadius: Theme.radiusRow))
    }
}

/// One live attempt (gh#103): the issue identifier as a chip — in a mixed
/// Active list, the chip is what says "the board released this" at a glance
/// (gh#123) — the branch underneath, elapsed against the route's cap on the
/// right.
struct AgentRowView: View {
    let agent: AgentRow
    let now: Date

    private var subline: Color { Theme.textSubtle }

    /// The accent a live agent carries — routed through the same status ramp as
    /// the board's rows, so a running attempt does not change colour on its way
    /// from the board screen to this list (gh#173).
    private var accent: Color { agentStateColor(agent.state) }

    var body: some View {
        HStack(spacing: 8) {
            Text(agent.state.glyph)
                .font(Theme.mono(Theme.textCaption))
                .foregroundStyle(accent)
                .frame(width: 10)
            VStack(alignment: .leading, spacing: 2) {
                // The chip fill is the white-wash language, not an accent
                // tint — the accent stays on the state glyph.
                Text(agent.identifier)
                    .font(Theme.sans(Theme.textDense, weight: .medium))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(Theme.elementActive,
                                in: RoundedRectangle(cornerRadius: Theme.radiusChip))
                HStack(spacing: 4) {
                    if let branch = agent.branch {
                        LineIconView(.gitBranch, size: 11, color: subline)
                        Text(branch)
                            .font(Theme.sans(Theme.textCaption))
                            .foregroundStyle(subline)
                            .lineLimit(1)
                            .truncationMode(.tail)
                    } else {
                        Text(agent.state.label)
                            .font(Theme.sans(Theme.textCaption))
                            .foregroundStyle(subline)
                    }
                }
            }
            Spacer(minLength: 8)
            if let elapsed = agent.elapsedLabel(now: now) {
                let over = agent.overCap(now: now)
                Text(elapsed)
                    .font(Theme.mono(Theme.textCaption, weight: over ? .semibold : .regular))
                    .foregroundStyle(over ? Theme.warning : subline)
                    .fixedSize()
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
        .contentShape(RoundedRectangle(cornerRadius: Theme.radiusRow))
    }
}
