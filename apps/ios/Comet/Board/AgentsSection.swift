// Live agents on the home screen — gh#103's sidebar section, phone-shaped.
//
// The problem it solves is the same one it solved on the desktop, and worse
// here: a dispatched agent is a chat among chats, so three of them are three
// rows somewhere in a recency-sorted list, indistinguishable from the session
// you opened yesterday. On a phone that list is also the only thing on screen.
//
// One row per live attempt: the issue identifier as the title (what the agent
// is *for* — a better name than the chat's, which the agent writes about
// itself), the branch underneath, elapsed against the route's cap on the right.
// Blocked floats, with a count on the header. Pure presentation: everything
// drawn was already streamed, and a tap opens the chat and nothing else —
// retry and cancel live on the board, which is the deep view and has the
// confirmations.

import SwiftUI

struct AgentsSection: View {
    @Environment(AppModel.self) private var model
    @Binding var path: [Route]

    /// One tick a second while the section is on screen: the rows carry the
    /// start instant, not the age, so a *blocked* agent — which animates
    /// nothing — would otherwise sit at whatever the last frame caught.
    @State private var now = Date()

    var body: some View {
        let agents = model.liveAgents
        if !agents.isEmpty {
            Section {
                ForEach(agents) { agent in
                    Button {
                        path.append(.chat(agent.chatId))
                    } label: {
                        AgentRowView(agent: agent, now: now)
                    }
                    .buttonStyle(PressWashButtonStyle())
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
                    .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
                }
                .motionAnimation(Motion.resort, value: agents.map(\.id))
            } header: {
                header(needing: agentsNeedingAttention(agents))
            }
            .task {
                while !Task.isCancelled {
                    try? await Task.sleep(nanoseconds: 1_000_000_000)
                    now = Date()
                }
            }
        }
    }

    private func header(needing: Int) -> some View {
        HStack(spacing: 6) {
            Text("Agents")
                .font(Theme.sans(11, weight: .medium))
                .foregroundStyle(Theme.textMuted.opacity(0.6))
            if needing > 0 {
                Text("\(needing)")
                    .font(Theme.mono(10, weight: .medium))
                    .foregroundStyle(Theme.danger)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(Theme.danger.opacity(0.14), in: Capsule())
            }
            Spacer(minLength: 0)
        }
        .textCase(nil)
        .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 3, trailing: 16))
    }
}

/// The other half of the list (gh#117): every working chat the board did NOT
/// release — the orchestrator, an ad-hoc agent chat, anything started by hand.
///
/// Below the Agents rather than mixed in, because the two answer different
/// questions: one row has an issue, a branch, a cap and a bill behind it, and
/// the other is a run somebody is responsible for that the board has never
/// heard of. Mixing them would make the second look like the first.
///
/// On the phone this is the surface that matters most: it is the whole answer
/// to "are they even alive", which otherwise took an ssh session and `pgrep`.
struct RunningSection: View {
    @Environment(AppModel.self) private var model
    @Binding var path: [Route]

    /// One tick a second, same as the Agents section: these rows carry the
    /// start instant, not the age.
    @State private var now = Date()

    var body: some View {
        let running = model.runningChats
        if !running.isEmpty {
            Section {
                ForEach(running) { row in
                    Button {
                        path.append(.chat(row.chatId))
                    } label: {
                        RunningRowView(row: row, now: now)
                    }
                    .buttonStyle(PressWashButtonStyle())
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
                    .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
                }
                .motionAnimation(Motion.resort, value: running.map(\.id))
            } header: {
                header(needing: runningNeedingAttention(running))
            }
            .task {
                while !Task.isCancelled {
                    try? await Task.sleep(nanoseconds: 1_000_000_000)
                    now = Date()
                }
            }
        }
    }

    private func header(needing: Int) -> some View {
        HStack(spacing: 6) {
            Text("Running")
                .font(Theme.sans(11, weight: .medium))
                .foregroundStyle(Theme.textMuted.opacity(0.6))
            if needing > 0 {
                Text("\(needing)")
                    .font(Theme.mono(10, weight: .medium))
                    .foregroundStyle(Theme.danger)
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(Theme.danger.opacity(0.14), in: Capsule())
            }
            Spacer(minLength: 0)
        }
        .textCase(nil)
        .listRowInsets(EdgeInsets(top: 8, leading: 16, bottom: 3, trailing: 16))
    }
}

/// One unmanaged run: state glyph, the chat's own title, elapsed since the run
/// started. One line — no branch is promised and no issue names it, so a second
/// line would be an empty one.
struct RunningRowView: View {
    let row: RunningRow
    let now: Date

    private var subline: Color { Theme.textMuted.opacity(0.5) }

    private var accent: Color {
        switch row.state {
        case .blocked: return boardStateColor(.blocked)
        case .errored: return boardStateColor(.failed)
        case .working: return boardStateColor(.working)
        }
    }

    var body: some View {
        HStack(spacing: 8) {
            Text(row.state.glyph)
                .font(Theme.mono(11))
                .foregroundStyle(accent)
                .frame(width: 10)
            Text(row.title)
                .font(Theme.sans(13, weight: .medium))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
            Spacer(minLength: 8)
            // A blocked run says so in words: it has no issue identifier to be
            // recognised by, so the glyph alone would be doing too much.
            if row.state.needsAttention {
                Text(row.state.label)
                    .font(Theme.sans(11))
                    .foregroundStyle(accent.opacity(0.9))
                    .fixedSize()
            }
            if let elapsed = row.elapsedLabel(now: now) {
                Text(elapsed)
                    .font(Theme.mono(11))
                    .foregroundStyle(subline)
                    .fixedSize()
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }
}

struct AgentRowView: View {
    let agent: AgentRow
    let now: Date

    private var subline: Color { Theme.textMuted.opacity(0.5) }

    /// The accent a live agent carries — routed through the board's own state
    /// colours so a running attempt does not change colour on its way from the
    /// board screen to this list.
    private var accent: Color {
        switch agent.state {
        case .blocked: return boardStateColor(.blocked)
        case .errored: return boardStateColor(.failed)
        case .working: return boardStateColor(.working)
        }
    }

    var body: some View {
        HStack(spacing: 8) {
            Text(agent.state.glyph)
                .font(Theme.mono(11))
                .foregroundStyle(accent)
                .frame(width: 10)
            VStack(alignment: .leading, spacing: 2) {
                Text(agent.identifier)
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.text)
                    .lineLimit(1)
                HStack(spacing: 4) {
                    if let branch = agent.branch {
                        LineIconView(.gitBranch, size: 11, color: subline)
                        Text(branch)
                            .font(Theme.sans(11))
                            .foregroundStyle(subline)
                            .lineLimit(1)
                            .truncationMode(.tail)
                    } else {
                        Text(agent.state.label)
                            .font(Theme.sans(11))
                            .foregroundStyle(subline)
                    }
                }
            }
            Spacer(minLength: 8)
            if let elapsed = agent.elapsedLabel(now: now) {
                let over = agent.overCap(now: now)
                Text(elapsed)
                    .font(Theme.mono(11, weight: over ? .semibold : .regular))
                    .foregroundStyle(over ? Theme.warning : subline)
                    .fixedSize()
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 7)
        .contentShape(RoundedRectangle(cornerRadius: 8))
    }
}
