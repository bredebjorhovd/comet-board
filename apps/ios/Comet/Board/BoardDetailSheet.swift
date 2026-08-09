// One board row, opened for reading (gh#132) — the phone's answer to "a row is
// a door".
//
// A list row shows a truncated title and nothing else you can open, which is a
// tooltip pretending to be a link. Tapping the row now opens this: the whole
// title, the issue body as markdown, the labels, where the work sits, what has
// been tried on it, and every action the row has anywhere else.
//
// It is for *reading*, and it is not on the way to anything. The row keeps its
// own Dispatch chip, so releasing a task is still one tap from the list — the
// sheet offers the same release for the times you opened it first, and routes
// it through the same account picker, because a dispatch spends somebody's
// subscription wherever it was started from (gh#74).
//
// Which actions a row has is `boardDetailActions` — the port of the shared rule
// the desktop panel's buttons and the TUI's keys read. This file decides only
// how they look and what a tap runs.

import SwiftUI

struct BoardDetailSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.openURL) private var openURL

    let taskId: String
    /// Handed a result line, so it lands on the board behind the sheet rather
    /// than vanishing with it.
    let onResult: (String) -> Void
    /// Releasing goes through the account picker, which is the board screen's
    /// own sheet — asked for here rather than stacked on top of this one, and
    /// handed to the board so it can wait out this sheet's dismissal first.
    let onDispatch: (TaskRow) -> Void
    /// Opening the chat leaves the board screen entirely, so the board screen
    /// is what pushes it — for the same reason.
    let onOpenChat: (String) -> Void

    /// The issue text. Nil is still reading; the rest is [`TaskBody`]'s three
    /// answers.
    @State private var issue: TaskBody?
    @State private var now = Date()

    /// Read live off the board rather than captured at open: a frame landing
    /// under the sheet moves the row on, and a panel still calling it `ready`
    /// after it started running would be the stalest thing on screen.
    private var row: TaskRow? {
        model.boardRows.first { $0.id == taskId }
    }

    var body: some View {
        NavigationStack {
            Group {
                if let row {
                    content(row)
                } else {
                    // Reaped, settled, filtered away — the sheet outlived its
                    // row. Say so rather than drawing a card about nothing.
                    ContentUnavailableView("No longer on the board",
                                           systemImage: "square.slash",
                                           description: Text("That row has left the board."))
                }
            }
            .background(SheetStyle.panel.ignoresSafeArea())
            .scrollEdgeEffectStyle(.soft, for: .top)
            .navigationTitle(row?.displayIdentifier ?? "Task")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button("Done") { dismiss() }
                }
            }
        }
        .presentationDetents([.large])
        .task(id: taskId) {
            issue = await model.boardTaskDetail(taskId: taskId)
        }
    }

    private func content(_ row: TaskRow) -> some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 18) {
                head(row)
                bodyCard
                actionCard(row)
            }
            .padding(.horizontal, 16)
            .padding(.top, 8)
            .padding(.bottom, 24)
        }
    }

    // MARK: Pieces

    private func head(_ row: TaskRow) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            // The whole title. The list's one truncated line is what sent you
            // here; wrapping it is the entire point of the sheet.
            Text(row.title)
                .font(Theme.sans(Theme.textTitle, weight: .medium))
                .foregroundStyle(Theme.text)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)

            HStack(spacing: 6) {
                Text(row.boardState.glyph)
                    .font(Theme.mono(Theme.textCaption))
                    .foregroundStyle(boardStateColor(row.boardState))
                Text(row.boardState.label)
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(Theme.textMuted)
            }

            if let placement = boardPlacementLine(row) {
                Text(placement)
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(Theme.textSubtle)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let history = boardHistoryLine(row, now: now) {
                Text(history)
                    .font(Theme.sans(Theme.textDense))
                    .foregroundStyle(Theme.textSubtle)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if !row.labels.isEmpty {
                // The one thing on the row the list has never had room for.
                HStack(spacing: 5) {
                    ForEach(row.labels, id: \.self) { label in
                        Text(label)
                            .font(Theme.sans(Theme.textCaption))
                            .foregroundStyle(Theme.textMuted)
                            .padding(.horizontal, 8)
                            .padding(.vertical, 3)
                            .background(whiteAlpha(0.07), in: RoundedRectangle(cornerRadius: Theme.radiusChip))
                    }
                }
                .padding(.top, 2)
            }
        }
    }

    @ViewBuilder
    private var bodyCard: some View {
        switch issue {
        case .none:
            HStack(spacing: 8) {
                ProgressView().controlSize(.small).tint(Theme.textMuted)
                Text("Reading the issue…")
                    .font(Theme.sans(Theme.textBody))
                    .foregroundStyle(Theme.textFaint)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        case .failed(let message):
            Text("Couldn't read the issue: \(message)")
                .font(Theme.sans(Theme.textBody))
                .foregroundStyle(Theme.danger)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .empty:
            Text(noBodyText)
                .font(Theme.sans(Theme.textBody))
                .foregroundStyle(Theme.textFaint)
                .frame(maxWidth: .infinity, alignment: .leading)
        case .text(let text):
            // The transcript's own markdown pipeline, so an issue reads here
            // the way an agent's reply reads there.
            VStack(alignment: .leading, spacing: MD.blockGap) {
                ForEach(Array(MarkdownParser.parse(text).enumerated()), id: \.offset) { _, top in
                    MarkdownBlockView(block: top.block, cacheKey: "board-peek-\(taskId)")
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    @ViewBuilder
    private func actionCard(_ row: TaskRow) -> some View {
        let actions = boardDetailActions(row)
        if !actions.isEmpty {
            VStack(alignment: .leading, spacing: 8) {
                SheetLabel("Actions")
                SheetCard {
                    ForEach(Array(actions.enumerated()), id: \.offset) { index, action in
                        if index > 0 { SheetSeparator() }
                        actionRow(row, action)
                    }
                }
            }
        }
    }

    private func actionRow(_ row: TaskRow, _ action: BoardRowAction) -> some View {
        Button {
            run(row, action)
        } label: {
            HStack(spacing: 12) {
                Image(systemName: action.symbol)
                    .font(.system(size: 15))
                    .foregroundStyle(action.destructive ? Theme.danger : Theme.textMuted)
                    .frame(width: 22)
                Text(action.label)
                    .font(Theme.sans(Theme.textTitle))
                    .foregroundStyle(action.destructive ? Theme.danger : Theme.text)
                Spacer(minLength: 8)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }

    // MARK: Doing

    private func run(_ row: TaskRow, _ action: BoardRowAction) {
        switch action {
        case .dispatch, .retry:
            // Never straight to a release: the account picker is where the
            // question of whose subscription this spends gets asked, and the
            // sheet must not be the one place on the board that skips it.
            onDispatch(row)
        case .cancel:
            let taskId = row.id
            let identifier = row.identifier
            dismiss()
            Task {
                if let error = await model.cancelBoardTask(taskId: taskId) {
                    onResult("Couldn't cancel \(identifier): \(error)")
                } else {
                    onResult("Ended \(identifier)'s attempt")
                }
            }
        case .openChat:
            guard let chatId = row.chatId, model.chat(id: chatId) != nil else {
                onResult("\(row.identifier) is running but has no chat to open")
                dismiss()
                return
            }
            onOpenChat(chatId)
        case .openIssue, .openPR:
            guard let url = boardActionURL(row, action) else {
                onResult("Nothing to open there yet")
                return
            }
            openURL(url)
        }
    }
}
