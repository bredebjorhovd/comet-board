// Naming the board's orchestrator from the phone — gh#166.
//
// gh#144 gave the phone the kill switch and nothing else: `setOrchestrator`
// took a `chatId` from the day it was written, and the only caller passed
// `nil`. So a board pinned from the desktop could be unpinned here and never
// re-pinned. This is the affirmative half, and it is the same one act — one
// `[defaults] orchestrator_chat` key on the board's `routing.toml`, written
// through `WriteBoardConfig`.
//
// Saying it in the chat does not do it. The pin is a *delivery address* — the
// chat id `announce` queues prompts into — not a role a session adopts, and a
// chat cannot pin itself either: an agent knows its own id from
// `COMET_BOARD_CHAT_ID`, which only a dispatched run carries, and an
// orchestrator is by definition a chat you opened yourself.
//
// Offered where a person already is when they decide — the chat's own menu, and
// the long-press on its row — which is where the desktop (right-click) and the
// TUI (`m`) put it too. Never on a settings page: the thing being decided is
// "this chat", and the chat is what you are looking at.
//
// Nothing is applied optimistically anywhere below. The board republishes the
// pin on the watch stream as the write lands, so the ◆ slot appearing IS the
// box agreeing — and a refusal leaves this phone showing what the box thinks.

import SwiftUI

/// One spelling on every surface (the desktop's `shell.rs` and here): the same
/// act with two names is two acts as far as a reader is concerned.
let pinAsOrchestratorLabel = "Pin as orchestrator"
let unpinAsOrchestratorLabel = "Unpin as orchestrator"
/// The confirmation's title. The label itself would read as the button under it
/// said twice; this is the question that button answers.
let pinAsOrchestratorPrompt = "Pin this chat as the orchestrator?"

/// What saying yes costs, before it is said.
///
/// The pin spends this chat's attention — every settle, block, orphan and cap
/// warning on the board arrives in it as a prompt (docs/orchestrator.md) — and
/// the row it gains is the same slot a long-press already unpins, so the way
/// back is worth naming in the same breath.
///
/// Replacing is named because `orchestrator_chat` is a single key: a second pin
/// MOVES it. A menu that let you believe otherwise would be lying about the
/// config it writes.
func orchestratorPinExplainer(replacing: String?) -> String {
    let arrives = "Every settle, block, orphan and cap warning on the board "
        + "arrives in this chat as a prompt. It gets the ◆ slot on Home — "
        + "long-press there to unpin it again."
    guard let replacing else { return arrives }
    return arrives + "\n\nA board has one orchestrator, so this moves the pin "
        + "off “\(replacing)”, and the board stops writing to it."
}

/// A pin the operator has asked for and not yet confirmed.
struct OrchestratorPinRequest: Identifiable, Equatable {
    let chatId: String
    /// The chat this would unpin, by name — `nil` when the board has no pin.
    let replacing: String?

    var id: String { chatId }
}

/// Why the board did not do it. Same refusals as the unpin path has always
/// said, in the same words — they come from `BoardStore.setOrchestrator`, which
/// is one function for both halves; only the verb in the title differs, because
/// only the operator's request differs.
struct OrchestratorPinFailure: Equatable {
    let pinning: Bool
    let message: String

    var title: String {
        pinning ? "Couldn't pin the orchestrator" : "Couldn't unpin the orchestrator"
    }
}

/// Whether this chat has a pin item at all — the rule `orchestratorPinItem`
/// applies, said once so a surface that has to decide whether to draw a menu
/// around it decides the same way.
@MainActor
func orchestratorPinOffered(chatId: String, model: AppModel) -> Bool {
    model.orchestratorChatId == chatId || !model.boardDispatched(chatId: chatId)
}

/// The menu item, for whichever menu a surface already has.
///
/// Exactly one of the two exists at a time, on the chat it acts on: unpinning
/// is the same item on the row that is pinned, so whoever wants the notices to
/// stop reaches for the session they pinned rather than for a settings page.
///
/// Unpin fires immediately — it is the kill switch (gh#144), and a kill switch
/// that asks a question is not one. Pin asks first: it is the half that spends
/// something.
///
/// A chat the board dispatched is offered nothing. That is the one thing
/// `comet-board doctor` says can be wrong with a pin — an attempt holds a
/// workspace slot and pinning it exempts it from its own time cap — so the
/// phone declines to offer the mistake rather than reporting it afterwards.
/// The unpin stays reachable regardless: whatever is pinned must be removable
/// here, however it got pinned.
@MainActor @ViewBuilder
func orchestratorPinItem(chatId: String, model: AppModel,
                         request: Binding<OrchestratorPinRequest?>,
                         failure: Binding<OrchestratorPinFailure?>) -> some View {
    if model.orchestratorChatId == chatId {
        Button(role: .destructive) {
            Task {
                if let error = await model.setOrchestrator(chatId: nil) {
                    failure.wrappedValue = OrchestratorPinFailure(pinning: false, message: error)
                }
            }
        } label: {
            Label(unpinAsOrchestratorLabel, systemImage: "pin.slash")
        }
    } else if orchestratorPinOffered(chatId: chatId, model: model) {
        Button {
            request.wrappedValue = OrchestratorPinRequest(
                chatId: chatId, replacing: model.pinnedOrchestratorName)
        } label: {
            Label(pinAsOrchestratorLabel, systemImage: "pin")
        }
    }
}

/// The confirmation the pin asks for, and both halves' refusals. Attached by
/// whichever view owns the state the menu item writes into.
struct OrchestratorPinPrompts: ViewModifier {
    @Environment(AppModel.self) private var model
    @Binding var request: OrchestratorPinRequest?
    @Binding var failure: OrchestratorPinFailure?

    func body(content: Content) -> some View {
        content
            .confirmationDialog(pinAsOrchestratorPrompt,
                                isPresented: Binding(get: { request != nil },
                                                     set: { if !$0 { request = nil } }),
                                titleVisibility: .visible,
                                presenting: request) { pending in
                Button(pinAsOrchestratorLabel) { pin(pending) }
                Button("Cancel", role: .cancel) { request = nil }
            } message: { pending in
                Text(orchestratorPinExplainer(replacing: pending.replacing))
            }
            .alert(failure?.title ?? "",
                   isPresented: Binding(get: { failure != nil },
                                        set: { if !$0 { failure = nil } })) {
                Button("OK", role: .cancel) { failure = nil }
            } message: {
                Text(failure?.message ?? "")
            }
    }

    private func pin(_ pending: OrchestratorPinRequest) {
        request = nil
        Task {
            if let error = await model.setOrchestrator(chatId: pending.chatId) {
                failure = OrchestratorPinFailure(pinning: true, message: error)
            }
        }
    }
}

extension View {
    /// The pin's confirmation and refusals, for a surface that puts the menu
    /// item in a menu of its own (the chat screen's ⋯).
    func orchestratorPinPrompts(request: Binding<OrchestratorPinRequest?>,
                                failure: Binding<OrchestratorPinFailure?>) -> some View {
        modifier(OrchestratorPinPrompts(request: request, failure: failure))
    }

    /// The whole thing on a row: long-press for the item, and the prompts it
    /// leads to. Its own state, so every row carries one dialog and not the
    /// list's.
    func orchestratorPinMenu(chatId: String) -> some View {
        modifier(OrchestratorPinRowMenu(chatId: chatId))
    }
}

/// Long-press on a row — the phone's right-click.
///
/// A row with nothing to offer keeps no menu at all: a long-press that lifts
/// the row and presents an empty sheet reads as a broken row, not as a refusal.
private struct OrchestratorPinRowMenu: ViewModifier {
    @Environment(AppModel.self) private var model
    let chatId: String

    @State private var request: OrchestratorPinRequest?
    @State private var failure: OrchestratorPinFailure?

    @ViewBuilder
    func body(content: Content) -> some View {
        if orchestratorPinOffered(chatId: chatId, model: model) {
            content
                .contextMenu {
                    orchestratorPinItem(chatId: chatId, model: model,
                                        request: $request, failure: $failure)
                }
                .orchestratorPinPrompts(request: $request, failure: $failure)
        } else {
            content
        }
    }
}
