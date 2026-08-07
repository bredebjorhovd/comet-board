// The board screen — the box's rows in your pocket (gh#114).
//
// Sections in board order with blocked first, because that is the order every
// other surface uses and the one that puts what wants a human at the top of the
// screen you just unlocked. Row content is `TaskRow`'s: the identifier, the
// title, and the per-state facts `boardRowDetail` decides are worth saying.
//
// What a tap does is the phone's own answer, and it is deliberately narrow:
// a row with a live chat opens that chat, a row without one opens the dispatch
// sheet. Everything that ends something — cancel — is a long-press away, on the
// desktop panel's rule that a glance which can kill an agent is a glance nobody
// trusts.

import SwiftUI

struct BoardView: View {
    @Environment(AppModel.self) private var model
    @Binding var path: [Route]

    /// One tick a second while a live attempt is on screen. The rows carry the
    /// start instant, not the age, so this moves the counter without anything
    /// being rebuilt — the same trade the TUI makes (`App::counting`).
    @State private var now = Date()
    @State private var dispatching: DispatchTarget?
    @State private var notice: String?
    /// Per-group fold overrides (gh#125), keyed `state:route`. Absent means the
    /// group's default — open for a named route, folded for `no route`.
    @State private var groupFolds: [String: Bool] = [:]

    private var sections: [(state: BoardState, groups: [BoardSectionGroup])] {
        groupedBoardSections(model.boardRows, now: now)
    }

    private var counting: Bool {
        model.boardRows.contains { $0.boardState.holdsPane }
    }

    var body: some View {
        List {
            if sections.isEmpty {
                emptyState
            }
            ForEach(sections, id: \.state) { section in
                let headers = boardGroupHeadersShown(section.groups)
                Section {
                    ForEach(section.groups, id: \.label) { group in
                        if headers {
                            groupHeader(section.state, group)
                        }
                        if !headers || !isFolded(section.state, group) {
                            ForEach(group.rows) { row in
                                BoardTaskRowView(row: row, now: now,
                                                 onOpen: { open(row) },
                                                 onDispatch: { dispatching = target(for: row) },
                                                 onCancel: { cancel(row) })
                                    .listRowBackground(Color.clear)
                                    .listRowSeparator(.hidden)
                                    .listRowInsets(EdgeInsets(top: 1, leading: 12, bottom: 1, trailing: 12))
                            }
                        }
                    }
                } header: {
                    sectionHeader(section.state,
                                  count: section.groups.reduce(0) { $0 + $1.rows.count })
                }
            }
        }
        .listStyle(.plain)
        .environment(\.defaultMinListRowHeight, 10)
        .scrollContentBackground(.hidden)
        .scrollEdgeEffectStyle(.soft, for: .top)
        .background(Theme.surface.ignoresSafeArea())
        .navigationTitle("Board")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: 1) {
                    Text("Board")
                        .font(Theme.sans(13, weight: .medium))
                        .foregroundStyle(Theme.text)
                    if let host = model.board?.hostDeviceId {
                        Text("on \(model.deviceName(host))")
                            .font(Theme.sans(10.5))
                            .foregroundStyle(Theme.textMuted.opacity(0.6))
                    }
                }
            }
        }
        .sheet(item: $dispatching) { target in
            DispatchSheet(target: target) { message in
                notice = message
            }
        }
        .overlay(alignment: .bottom) {
            if let notice {
                NoticeBar(text: notice) { self.notice = nil }
            }
        }
        .onAppear {
            // Screenshot rig: land straight on the picker for the first row
            // that offers one.
            if model.launchSheet == "dispatch" {
                model.launchSheet = nil
                if let row = model.boardRows.first(where: {
                    $0.dispatchable && $0.boardState == .ready
                }) {
                    dispatching = target(for: row)
                }
            }
        }
        // Only while something is counting: a board of settled rows should not
        // wake the app once a second to redraw numbers that do not move.
        .task(id: counting) {
            guard counting else { return }
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                now = Date()
            }
        }
    }

    // MARK: Actions

    /// A live attempt opens its chat; anything else opens the dispatch sheet
    /// when it can be released, and does nothing when it cannot.
    private func open(_ row: TaskRow) {
        if let chatId = row.chatId, model.chat(id: chatId) != nil {
            path.append(.chat(chatId))
        } else if row.dispatchable {
            dispatching = target(for: row)
        }
    }

    private func target(for row: TaskRow) -> DispatchTarget {
        DispatchTarget(row: row, wasLive: row.boardState == .blocked)
    }

    private func cancel(_ row: TaskRow) {
        Task {
            if let error = await model.cancelBoardTask(taskId: row.id) {
                notice = "Couldn't cancel \(row.identifier): \(error)"
            } else {
                notice = "Ended \(row.identifier)'s attempt"
            }
        }
    }

    // MARK: Chrome

    private func foldKey(_ state: BoardState, _ group: BoardSectionGroup) -> String {
        "\(state.rawValue):\(group.label)"
    }

    private func isFolded(_ state: BoardState, _ group: BoardSectionGroup) -> Bool {
        groupFolds[foldKey(state, group)] ?? group.startsCollapsed
    }

    /// A route's group header inside a section (gh#125): chevron, name, count.
    /// `no route` is the trailing group and starts folded — visibility-only
    /// rows get a headline, never pole position.
    private func groupHeader(_ state: BoardState, _ group: BoardSectionGroup) -> some View {
        let folded = isFolded(state, group)
        return Button {
            groupFolds[foldKey(state, group)] = !folded
        } label: {
            HStack(spacing: 6) {
                Image(systemName: folded ? "chevron.right" : "chevron.down")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
                Text(group.label)
                    .font(Theme.sans(11, weight: .medium))
                    .foregroundStyle(group.route == nil
                        ? Theme.textFaint
                        : Theme.textMuted.opacity(0.85))
                Text("\(group.rows.count)")
                    .font(Theme.mono(10))
                    .foregroundStyle(Theme.textFaint.opacity(0.7))
                Spacer(minLength: 0)
            }
            .padding(.vertical, 4)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
        .listRowInsets(EdgeInsets(top: 2, leading: 20, bottom: 0, trailing: 12))
    }

    private func sectionHeader(_ state: BoardState, count: Int) -> some View {
        HStack(spacing: 6) {
            Text(state.glyph)
                .font(Theme.mono(10))
                .foregroundStyle(boardStateColor(state))
            Text(state.label)
                .font(Theme.sans(11, weight: .medium))
                .foregroundStyle(Theme.textMuted.opacity(0.6))
            Text("\(count)")
                .font(Theme.mono(10))
                .foregroundStyle(Theme.textFaint.opacity(0.7))
            Spacer(minLength: 0)
        }
        .textCase(nil)
        .listRowInsets(EdgeInsets(top: 10, leading: 16, bottom: 3, trailing: 16))
    }

    private var emptyState: some View {
        VStack(spacing: 12) {
            Image(systemName: "square.stack.3d.up")
                .font(.system(size: 28, weight: .light))
                .foregroundStyle(Theme.textFaint)
            Text(model.boardStatus ?? (model.boardAttached ? "Nothing on the board" : "Looking for the board…"))
                .font(Theme.sans(13))
                .foregroundStyle(Theme.textFaint)
                .multilineTextAlignment(.center)
            if !model.boardAttached && model.boardStatus == nil {
                ProgressView()
                    .controlSize(.small)
                    .tint(Theme.textMuted)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.vertical, 48)
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
    }
}

/// The state's colour, ported from the desktop panel's `state_color` so a row
/// does not change colour on its way between surfaces.
func boardStateColor(_ state: BoardState) -> Color {
    switch state {
    case .blocked, .failed: return Theme.danger
    case .working: return Theme.warning
    case .review: return Theme.accent
    case .ready: return Theme.text
    case .done: return Theme.textFaint
    }
}

// MARK: - Row

struct BoardTaskRowView: View {
    let row: TaskRow
    let now: Date
    let onOpen: () -> Void
    let onDispatch: () -> Void
    let onCancel: () -> Void

    private var subline: Color { Theme.textMuted.opacity(0.5) }

    var body: some View {
        let detail = boardRowDetail(row, now: now)
        Button(action: onOpen) {
            VStack(alignment: .leading, spacing: 3) {
                // Line 1: state glyph, identifier, elapsed against the cap.
                HStack(spacing: 8) {
                    Text(row.boardState.glyph)
                        .font(Theme.mono(11))
                        .foregroundStyle(boardStateColor(row.boardState))
                        .frame(width: 10)
                    // The repo-qualified token (gh#125): `tally #507`, because
                    // `gh#507` alone is ambiguous across repos.
                    Text(row.displayIdentifier)
                        .font(Theme.mono(11))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                    Spacer(minLength: 8)
                    if let elapsed = detail.elapsed {
                        // Past the cap the counter turns and bolds: gh#70's
                        // clock is about to end that attempt, and the number is
                        // the reason.
                        Text(elapsed)
                            .font(Theme.mono(11, weight: detail.overCap ? .semibold : .regular))
                            .foregroundStyle(detail.overCap ? Theme.warning : subline)
                            .fixedSize()
                    }
                }

                // Line 2: the issue title.
                Text(row.title)
                    .font(Theme.sans(13))
                    .foregroundStyle(row.boardState == .done ? Theme.textMuted : Theme.text)
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.leading, 18)

                // Line 3: what this state is worth saying, then the actions.
                HStack(spacing: 6) {
                    if !detail.text.isEmpty {
                        Text(detail.text)
                            .font(Theme.sans(11))
                            .foregroundStyle(subline)
                            .lineLimit(1)
                            .truncationMode(.tail)
                    }
                    if let billing = detail.billing {
                        // True of an attempt for its whole life, so it rides
                        // beside the state's own facts rather than inside them.
                        Text(billing)
                            .font(Theme.sans(11))
                            .foregroundStyle(Theme.warning.opacity(0.85))
                            .lineLimit(1)
                    }
                    Spacer(minLength: 4)
                    actions
                }
                .padding(.leading, 18)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
            .contentShape(RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(PressWashButtonStyle())
        .contextMenu {
            if row.boardState.holdsPane {
                Button("End this attempt", systemImage: "stop.circle", role: .destructive,
                       action: onCancel)
            }
            if let url = URL(string: row.url), !row.url.isEmpty {
                Link(destination: url) { Label("Open issue", systemImage: "arrow.up.right.square") }
            }
            if let pr = row.prUrl, let url = URL(string: pr) {
                Link(destination: url) { Label("Open PR", systemImage: "arrow.triangle.pull") }
            }
        }
    }

    /// The one chip a row gets, if any — the enter-key affordance the desktop
    /// row spells `[enter to dispatch]`, made a thumb target.
    @ViewBuilder
    private var actions: some View {
        switch row.boardState {
        case .ready where row.dispatchable:
            chip("Dispatch", action: onDispatch)
        case .blocked where row.dispatchable, .failed where row.dispatchable:
            chip("Retry", action: onDispatch)
        default:
            EmptyView()
        }
    }

    private func chip(_ title: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(title)
                .font(Theme.sans(11, weight: .medium))
                .foregroundStyle(Theme.text)
                .padding(.horizontal, 10)
                .padding(.vertical, 4)
                .background(whiteAlpha(0.07), in: Capsule())
        }
        .buttonStyle(ChipPressButtonStyle())
    }
}

// MARK: - Notice

/// Transient result line for a dispatch, a retry or a cancel — the panel's
/// `set_notice`, as a bar that gets out of the way on its own.
struct NoticeBar: View {
    let text: String
    let dismiss: () -> Void

    var body: some View {
        Text(text)
            .font(Theme.sans(12))
            .foregroundStyle(Theme.text)
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: 12))
            .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Theme.border, lineWidth: 1))
            .padding(.horizontal, 12)
            .padding(.bottom, 12)
            .transition(.move(edge: .bottom).combined(with: .opacity))
            .onTapGesture(perform: dismiss)
            .task(id: text) {
                try? await Task.sleep(nanoseconds: 6_000_000_000)
                dismiss()
            }
    }
}
