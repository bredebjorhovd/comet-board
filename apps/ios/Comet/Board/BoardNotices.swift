// Naming the chat that takes the board's stray notices, from the phone — gh#166.
//
// gh#144 gave the phone the kill switch and nothing else: `setFallbackChat`
// took a `chatId` from the day it was written, and the only caller passed
// `nil`. So a board addressed from the desktop could be cleared here and never
// set again. This is the affirmative half, and it is the same one act — one
// `[defaults] fallback_chat` key on the board's `routing.toml`, written
// through `WriteBoardConfig`.
//
// It is a *delivery address* and not a role (gh#348): the chat id `announce`
// queues prompts into when no dispatcher can be told. Nobody is being appointed
// to run the board — a chat that dispatches is already doing that, and needs
// nothing set here. Saying it in the chat does not do it either, and a chat
// cannot name itself: an agent knows its own id from `COMET_BOARD_CHAT_ID`,
// which only a dispatched run carries, and this is a chat you opened yourself.
//
// Offered where a person already is when they decide — the chat's own menu, and
// the long-press on its row — which is where the desktop (right-click) puts it
// too. Never on a settings page: the thing being decided is "this chat", and
// the chat is what you are looking at.
//
// Nothing is applied optimistically anywhere below. The board republishes the
// address on the watch stream as the write lands, so the ◆ slot appearing IS
// the box agreeing — and a refusal leaves this phone showing what the box
// thinks.

import SwiftUI

/// One spelling on every surface (the desktop's `shell.rs` and here): the same
/// act with two names is two acts as far as a reader is concerned.
let sendBoardNoticesLabel = "Send board notices here"
let stopBoardNoticesLabel = "Stop sending board notices here"
/// The confirmation's title. The label itself would read as the button under it
/// said twice; this is the question that button answers.
let sendBoardNoticesPrompt = "Send this board's notices to this chat?"

/// What saying yes costs, before it is said.
///
/// It spends this chat's attention — every settle, block, orphan and cap
/// warning the board cannot address to a dispatcher arrives in it as a prompt
/// (docs/fallback-chat.md) — and the row it gains is the same slot a long-press
/// already clears, so the way back is worth naming in the same breath.
///
/// Replacing is named because `fallback_chat` is a single key: a second chat
/// MOVES it. A menu that let you believe otherwise would be lying about the
/// config it writes.
func boardNoticesExplainer(replacing: String?) -> String {
    let arrives = "Every settle, block, orphan and cap warning the board can't "
        + "address to the chat that released it arrives here as a prompt. It "
        + "gets the ◆ slot on Home — long-press there to stop it again."
    guard let replacing else { return arrives }
    return arrives + "\n\nA board has one such address, so this moves it "
        + "off “\(replacing)”, and the board stops writing to it."
}

/// An address the operator has asked for and not yet confirmed.
struct BoardNoticesRequest: Identifiable, Equatable {
    let chatId: String
    /// The chat this would take it off, by name — `nil` when the board has none.
    let replacing: String?

    var id: String { chatId }
}

/// Why the board did not do it. Same refusals as the clear path has always
/// said, in the same words — they come from `BoardStore.setFallbackChat`, which
/// is one function for both halves; only the verb in the title differs, because
/// only the operator's request differs.
struct BoardNoticesFailure: Equatable {
    let setting: Bool
    let message: String

    var title: String {
        setting ? "Couldn't send notices here" : "Couldn't stop the notices"
    }
}

/// Whether this chat has an item at all — the rule `boardNoticesItem` applies,
/// said once so a surface that has to decide whether to draw a menu around it
/// decides the same way.
@MainActor
func boardNoticesOffered(chatId: String, model: AppModel) -> Bool {
    model.fallbackChatId == chatId || !model.boardDispatched(chatId: chatId)
}

/// The menu item, for whichever menu a surface already has.
///
/// Exactly one of the two exists at a time, on the chat it acts on: stopping is
/// the same item on the chat that has it, so whoever wants the notices to stop
/// reaches for the session they set rather than for a settings page.
///
/// Stop fires immediately — it is the kill switch (gh#144), and a kill switch
/// that asks a question is not one. Setting asks first: it is the half that
/// spends something.
///
/// A chat the board dispatched is offered nothing. That is the one thing
/// `comet-board doctor` says can be wrong here — the board's own events landing
/// in a chat that is mid-task on something else — so the phone declines to
/// offer the mistake rather than reporting it afterwards. Stopping stays
/// reachable regardless: whatever is set must be removable here, however it got
/// set.
@MainActor @ViewBuilder
func boardNoticesItem(chatId: String, model: AppModel,
                      request: Binding<BoardNoticesRequest?>,
                      failure: Binding<BoardNoticesFailure?>) -> some View {
    if model.fallbackChatId == chatId {
        Button(role: .destructive) {
            Task {
                if let error = await model.setFallbackChat(chatId: nil) {
                    failure.wrappedValue = BoardNoticesFailure(setting: false, message: error)
                }
            }
        } label: {
            Label(stopBoardNoticesLabel, systemImage: "pin.slash")
        }
    } else if boardNoticesOffered(chatId: chatId, model: model) {
        Button {
            request.wrappedValue = BoardNoticesRequest(
                chatId: chatId, replacing: model.fallbackChatName)
        } label: {
            Label(sendBoardNoticesLabel, systemImage: "pin")
        }
    }
}

/// The confirmation setting it asks for, and both halves' refusals. Attached by
/// whichever view owns the state the menu item writes into.
struct BoardNoticesPrompts: ViewModifier {
    @Environment(AppModel.self) private var model
    @Binding var request: BoardNoticesRequest?
    @Binding var failure: BoardNoticesFailure?

    func body(content: Content) -> some View {
        content
            .confirmationDialog(sendBoardNoticesPrompt,
                                isPresented: Binding(get: { request != nil },
                                                     set: { if !$0 { request = nil } }),
                                titleVisibility: .visible,
                                presenting: request) { pending in
                Button(sendBoardNoticesLabel) { send(pending) }
                Button("Cancel", role: .cancel) { request = nil }
            } message: { pending in
                Text(boardNoticesExplainer(replacing: pending.replacing))
            }
            .alert(failure?.title ?? "",
                   isPresented: Binding(get: { failure != nil },
                                        set: { if !$0 { failure = nil } })) {
                Button("OK", role: .cancel) { failure = nil }
            } message: {
                Text(failure?.message ?? "")
            }
    }

    private func send(_ pending: BoardNoticesRequest) {
        request = nil
        Task {
            if let error = await model.setFallbackChat(chatId: pending.chatId) {
                failure = BoardNoticesFailure(setting: true, message: error)
            }
        }
    }
}

extension View {
    /// The confirmation and refusals, for a surface that puts the menu item in
    /// a menu of its own (the chat screen's ⋯).
    func boardNoticesPrompts(request: Binding<BoardNoticesRequest?>,
                             failure: Binding<BoardNoticesFailure?>) -> some View {
        modifier(BoardNoticesPrompts(request: request, failure: failure))
    }

    /// The whole thing on a row: long-press for the item, and the prompts it
    /// leads to. Its own state, so every row carries one dialog and not the
    /// list's.
    func boardNoticesMenu(chatId: String) -> some View {
        modifier(BoardNoticesRowMenu(chatId: chatId))
    }
}

/// Long-press on a row — the phone's right-click.
///
/// A row with nothing to offer keeps no menu at all: a long-press that lifts
/// the row and presents an empty sheet reads as a broken row, not as a refusal.
private struct BoardNoticesRowMenu: ViewModifier {
    @Environment(AppModel.self) private var model
    let chatId: String

    @State private var request: BoardNoticesRequest?
    @State private var failure: BoardNoticesFailure?

    @ViewBuilder
    func body(content: Content) -> some View {
        if boardNoticesOffered(chatId: chatId, model: model) {
            content
                .contextMenu {
                    boardNoticesItem(chatId: chatId, model: model,
                                     request: $request, failure: $failure)
                }
                .boardNoticesPrompts(request: $request, failure: $failure)
        } else {
            content
        }
    }
}
