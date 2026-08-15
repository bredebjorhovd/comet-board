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
    /// being rebuilt — every viewport makes this trade (§gh#103).
    @State private var now = Date()
    @State private var dispatching: DispatchTarget?
    /// The row opened for reading (gh#132). A task id, not a row: the sheet
    /// reads the row live off the board so a frame landing under it moves the
    /// panel on with the work.
    @State private var opened: OpenedRow?
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
                                    // ios.md C4.1: full-bleed. The section is
                                    // the card; a row inside it is a line.
                                    .listRowInsets(EdgeInsets(top: 0, leading: 0, bottom: 0, trailing: 0))
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
            ToolbarItem(placement: .topBarTrailing) {
                // What the board did with the work it was given (gh#143). Off
                // the board rather than off Home: it is a question about this
                // board, and it is read on the host the rows came from.
                Button {
                    path.append(.stats)
                } label: {
                    Image(systemName: "chart.bar")
                }
                .accessibilityLabel("Board stats")
            }
            ToolbarItem(placement: .principal) {
                VStack(spacing: 1) {
                    Text("Board")
                        .font(Theme.sans(Theme.textBody, weight: .medium))
                        .foregroundStyle(Theme.text)
                    if let host = model.board?.hostDeviceId {
                        Text("on \(model.deviceName(host))")
                            .font(Theme.sans(Theme.textCaption))
                            .foregroundStyle(Theme.textSubtle)
                    }
                }
            }
        }
        .sheet(item: $dispatching) { target in
            DispatchSheet(target: target) { message in
                notice = message
            }
        }
        .sheet(item: $opened) { row in
            BoardDetailSheet(taskId: row.id,
                             onResult: { notice = $0 },
                             onDispatch: { row in
                                 afterDetail { dispatching = target(for: row) }
                             },
                             onOpenChat: { chatId in
                                 afterDetail { path.append(.chat(chatId)) }
                             },
                             openReview: row.openReview)
                // ios.md D1.1: the canvas draws a grabber at the top of this
                // sheet. It is system chrome here rather than a drawn bar —
                // asking for it is how a phone says "this drags away".
                .presentationDragIndicator(.visible)
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
            // …or on a review (gh#256). The review gate first and any ended
            // attempt after it: a row parked in `review` is the state the
            // design file draws, and a rig that took whichever reviewable row
            // sorted first would photograph the blocked one.
            if let spec = model.launchSheet, spec == "review" || spec.hasPrefix("review:") {
                model.launchSheet = nil
                let wanted = spec.hasPrefix("review:")
                    ? String(spec.dropFirst("review:".count)) : nil
                let reviewable = model.boardRows.filter(boardReviewable)
                if let row = reviewable.first(where: { $0.id == wanted })
                    ?? (wanted == nil ? reviewable.first { $0.boardState == .review } : nil)
                    ?? (wanted == nil ? reviewable.first : nil) {
                    opened = OpenedRow(id: row.id, openReview: true)
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

    /// Tapping a row opens it for reading (gh#132).
    ///
    /// It used to open the row's *chat*, or its dispatch sheet, or nothing at
    /// all depending on state — three answers to one gesture, and on an
    /// unroutable row the answer was silence. Now a tap always does the same
    /// thing, and the sheet offers every one of those as a named action.
    ///
    /// Releasing stays one tap from the list: the row keeps its own Dispatch /
    /// Retry chip, which goes straight to the account picker.
    private func open(_ row: TaskRow) {
        opened = OpenedRow(id: row.id)
    }

    /// Leave the detail sheet for something else on this screen.
    ///
    /// SwiftUI will not present a second sheet while the first is still
    /// dismissing, and a push that races the dismissal animation lands
    /// unreliably — so the next thing waits for this one to be gone. The
    /// alternative (stacking the account picker on top of the detail) would put
    /// two modals between a thumb and a release.
    private func afterDetail(_ next: @escaping () -> Void) {
        opened = nil
        Task { @MainActor in
            try? await Task.sleep(nanoseconds: 350_000_000)
            next()
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
                    .font(.system(size: 10, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
                Text(group.label)
                    .font(Theme.sans(Theme.textDense, weight: .medium))
                    .foregroundStyle(group.route == nil
                        ? Theme.textFaint
                        : Theme.textSubtle)
                Text("\(group.rows.count)")
                    .font(Theme.mono(Theme.textCaption))
                    .foregroundStyle(Theme.textFaint)
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
                .font(Theme.mono(Theme.textCaption))
                .foregroundStyle(boardStateColor(state))
            // ios.md C2.2: `--text`, not `--subtle`. On the board the
            // section IS the structure; on Home a section header labels rows
            // that can each be read alone.
            Text(state.sectionTitle)
                .font(Theme.sans(Theme.textDense, weight: .semibold))
                .foregroundStyle(Theme.text)
            Text("\(count)")
                .font(Theme.mono(Theme.textCaption))
                .foregroundStyle(Theme.textSubtle)
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
                .font(Theme.sans(Theme.textBody))
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

/// The accent a board state carries — the status ramp's answer, never this
/// file's (gh#173), and a port of the desktop panel's `state_color`.
///
/// Blocked and failed share red (the glyph tells them apart), working is amber,
/// review indigo; ready and done spend no colour at all, so they land on the
/// row's own text tones — plain for a queued row, dim for history.
func boardStateColor(_ state: BoardState) -> Color {
    if let status = Status.ofBoard(state) { return Theme.status(status) }
    return state == .done ? Theme.textFaint : Theme.text
}

/// The accent a *live agent* carries — the same ramp as `boardStateColor`, so a
/// running attempt does not change colour on its way from the board to Home
/// (gh#103, gh#173).
func agentStateColor(_ state: AgentState) -> Color {
    Theme.status(Status.ofAgent(state))
}

// MARK: - Row

struct BoardTaskRowView: View {
    let row: TaskRow
    let now: Date
    let onOpen: () -> Void
    let onDispatch: () -> Void
    let onCancel: () -> Void

    private var subline: Color { Theme.textSubtle }

    var body: some View {
        let detail = boardRowDetail(row, now: now)
        Button(action: onOpen) {
            VStack(alignment: .leading, spacing: 3) {
                // Line 1: state glyph, identifier, elapsed against the cap.
                HStack(spacing: 8) {
                    Text(row.boardState.glyph)
                        .font(Theme.mono(Theme.textCaption))
                        .foregroundStyle(boardStateColor(row.boardState))
                        .frame(width: 10)
                    // The repo-qualified token (gh#125): `tally #507`, because
                    // `gh#507` alone is ambiguous across repos.
                    Text(row.displayIdentifier)
                        .font(Theme.mono(Theme.textCaption))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                    Spacer(minLength: 8)
                    if let elapsed = detail.elapsed {
                        // Past the cap the counter turns and bolds: gh#70's
                        // clock is about to end that attempt, and the number is
                        // the reason.
                        Text(elapsed)
                            .font(Theme.mono(Theme.textCaption, weight: detail.overCap ? .semibold : .regular))
                            .foregroundStyle(detail.overCap ? Theme.warning : subline)
                            .fixedSize()
                    }
                }

                // Line 2: the issue title.
                // ios.md C4.4 / Deviations: prose size, as on Home.
                Text(row.title)
                    .font(Theme.sans(Theme.textProse))
                    .foregroundStyle(row.boardState == .done ? Theme.textMuted : Theme.text)
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.leading, 18)

                // Line 3: what this state is worth saying, then the actions.
                HStack(spacing: 6) {
                    if !detail.text.isEmpty {
                        Text(detail.text)
                            .font(Theme.sans(Theme.textDense))
                            .foregroundStyle(subline)
                            .lineLimit(1)
                            .truncationMode(.tail)
                    }
                    if let billing = detail.billing {
                        // True of an attempt for its whole life, so it rides
                        // beside the state's own facts rather than inside them.
                        Text(billing)
                            .font(Theme.sans(Theme.textDense))
                            .foregroundStyle(Theme.warning)
                            .lineLimit(1)
                    }
                    Spacer(minLength: 4)
                    actions
                }
                .padding(.leading, 18)
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 6)
            .frame(maxWidth: .infinity, alignment: .leading)
            .contentShape(Rectangle())
        }
        .buttonStyle(.fullBleedSelect)
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
            // round-ok: the board row's verb chip, which the canvas draws
            // as a pill (ios.md C4.6) — the one thumb target on the row, and
            // the shape is what makes it read as one rather than as a label.
            Text(title)
                .font(Theme.sans(Theme.textDense, weight: .medium))
                .foregroundStyle(Theme.text)
                .padding(.horizontal, 11)
                .padding(.vertical, 4)
                // round-ok: the verb pill (ios.md C4.6)
                .background(Theme.chip, in: Capsule())
        }
        .buttonStyle(ChipPressButtonStyle(shape: .capsule))
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
            .font(Theme.sans(Theme.textDense))
            .foregroundStyle(Theme.text)
            .padding(.horizontal, 14)
            .padding(.vertical, 10)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.surfaceRaised, in: RoundedRectangle(cornerRadius: Theme.radiusCard))
            .overlay(RoundedRectangle(cornerRadius: Theme.radiusCard).strokeBorder(Theme.border, lineWidth: 1))
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
