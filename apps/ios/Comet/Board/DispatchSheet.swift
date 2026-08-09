// Releasing a task from a phone — the desktop dispatch picker's two questions
// (which runtime, whose agent account) as a sheet, plus the billing guard's
// confirm (gh#74 / gh#101).
//
// The account strip matters MOST here. On a desktop the picker is a popover you
// arrow through; on a phone enter-enter is a thumb tap, and the row that a tap
// lands on without anybody having chosen it is the route's default — which is
// exactly the release that can quietly spend a teammate's Claude subscription.
// So the chips resolve to emails, not slot ids: `Route default · 8f2c1d0a` tells
// a teammate nothing about whose plan they are about to spend.
//
// Deliberately NOT here: the model picker. The desktop's third column loads a
// per-harness catalog from the host and filters it with a search field; a
// dispatch with no model override runs the harness default, which is what the
// route already meant. A phone releasing work in a parking lot wants one screen
// and two decisions, and the model is the one nobody changes at a bus stop.

import SwiftUI

/// What a sheet is being opened about. `wasLive` is only how the sheet titles
/// itself; whether the release actually ends a live attempt is decided at send
/// time — see `replaceNow`.
struct DispatchTarget: Identifiable {
    var row: TaskRow
    var wasLive: Bool

    var id: String { row.id }
}

struct DispatchSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    let target: DispatchTarget
    /// Handed the result line, so it lands on the board behind the sheet rather
    /// than vanishing with it.
    let onResult: (String) -> Void

    @State private var runtimeName: String?
    /// nil = the route's own account (row 0, no override).
    @State private var accountId: String?
    @State private var sending = false
    @State private var error: String?
    /// Set when the host refused a cross-billed release: the second send is the
    /// acknowledgement, never the first.
    @State private var confirmingBill: String?

    private var runtimes: [BoardRuntime] { model.boardRuntimes }

    private var harness: String? {
        runtimes.first { $0.name == runtimeName }?.harness
    }

    private var accountOptions: [BoardAccount] {
        model.boardAccounts(forHarness: harness)
    }

    /// The route's own account, for labelling row 0 — what "no override" will
    /// actually spend.
    private var routeAccount: String? { target.row.account }

    /// Who the current selection charges, whoever that is — the payer the
    /// confirm has to name if the host refuses this release.
    private var billedTo: String? {
        billedEmail(accounts: model.boardAccounts, harness: harness,
                    slot: accountId ?? routeAccount)
    }

    private var verb: String { target.wasLive ? "Retry" : "Dispatch" }

    /// Whether this release must end a live attempt first (gh#49). Read from
    /// the CURRENT rows, not from the row the sheet opened on: if the attempt
    /// cleared meanwhile — cancelled, or the agent moved on — a plain dispatch
    /// is the right call, and the engine's one-live-attempt guard is free to
    /// refuse if it did not.
    private var replaceNow: Bool {
        model.boardRows.first { $0.id == target.row.id }?.boardState == .blocked
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    header
                    if let confirmingBill {
                        billingConfirm(billedTo: confirmingBill)
                    } else {
                        runtimeCard
                        accountCard
                    }
                    if let error {
                        Text(error)
                            .font(Theme.sans(12))
                            .foregroundStyle(Theme.danger)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                .padding(.horizontal, 16)
                .padding(.top, 8)
                .padding(.bottom, 16)
            }
            .background(SheetStyle.panel.ignoresSafeArea())
            .scrollEdgeEffectStyle(.soft, for: .top)
            .navigationTitle(target.wasLive ? "Retry task" : "Dispatch task")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button("Cancel") { dismiss() }
                }
            }
            .safeAreaInset(edge: .bottom) {
                SheetPrimaryButton(title: sending ? "Releasing…" : sendTitle,
                                   enabled: !sending && !runtimes.isEmpty) {
                    send(confirmedBill: confirmingBill)
                }
                .padding(.horizontal, 16)
                .padding(.bottom, 12)
                .background(.clear)
            }
        }
        // Large, not medium: the account row is the decision this sheet exists
        // for, and a detent that hides it under the send button is a picker
        // that lands on the route's default because nobody could see it.
        .presentationDetents([.large])
        .task { primeSelection() }
    }

    private var sendTitle: String {
        confirmingBill == nil ? verb : "\(verb) on their plan"
    }

    // MARK: Pieces

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(target.row.identifier)
                .font(Theme.mono(11))
                .foregroundStyle(Theme.textMuted)
            Text(target.row.title)
                .font(Theme.sans(15))
                .foregroundStyle(Theme.text)
                .fixedSize(horizontal: false, vertical: true)
            if let workspace = target.row.workspace {
                Text("→ \(workspace)")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textMuted.opacity(0.7))
            }
            if target.wasLive {
                Text("Ends the live attempt, then releases a fresh one.")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textMuted.opacity(0.7))
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var runtimeCard: some View {
        VStack(alignment: .leading, spacing: 8) {
            SheetLabel("Runtime")
            SheetCard {
                if runtimes.isEmpty {
                    loadingRow("Asking the board…")
                }
                ForEach(Array(runtimes.enumerated()), id: \.element.id) { index, runtime in
                    if index > 0 { SheetSeparator() }
                    // A runtime the host cannot start is still listed, saying
                    // why (gh#187): an operator who expects OpenCode on the box
                    // needs to read "not installed", not find the row absent.
                    // The reason takes the subtitle over "the route's runtime",
                    // because a route pointing at a harness the box cannot run
                    // is the more urgent half of that sentence.
                    SheetSelectRow(title: runtime.label,
                                   subtitle: runtimeSubtitle(runtime),
                                   selected: runtime.name == runtimeName) {
                        runtimeName = runtime.name
                        // A Claude slot is not lendable to a codex run (gh#59):
                        // switching harness invalidates the pick, so drop back
                        // to the route's default rather than send a slot the
                        // new runtime cannot use.
                        accountId = nil
                    }
                }
            }
        }
    }

    /// What a runtime row says under its label: why the host cannot start it
    /// (gh#187), else whether it is the one the route already picked.
    ///
    /// The reason wins: a route pointing at a harness the box cannot run is the
    /// more urgent half of that sentence, and it is the half the dispatch will
    /// be refused over.
    private func runtimeSubtitle(_ runtime: BoardRuntime) -> String? {
        if let note = runtime.note { return note }
        return runtime.name == target.row.runtime ? "the route's runtime" : nil
    }

    private var accountCard: some View {
        VStack(alignment: .leading, spacing: 8) {
            SheetLabel("Account")
            SheetCard {
                SheetSelectRow(title: "Route default",
                               subtitle: routeDefaultSubtitle,
                               selected: accountId == nil) { accountId = nil }
                ForEach(accountOptions) { account in
                    SheetSeparator()
                    SheetSelectRow(title: account.label,
                                   subtitle: billsNote(slot: account.id)
                                       ?? (account.active ? "the box's active login" : nil),
                                   selected: accountId == account.id) {
                        accountId = account.id
                    }
                }
                if accountOptions.isEmpty && harness != nil {
                    SheetSeparator()
                    loadingRow("No saved logins for this runtime on the board's host")
                }
            }
            Text("Whose subscription the run spends. The board's host owns these logins — not this phone.")
                .font(Theme.sans(11))
                .foregroundStyle(Theme.textFaint)
                .padding(.horizontal, 4)
        }
    }

    /// Row 0's subtitle: what "no override" resolves to, said as an email.
    private var routeDefaultSubtitle: String? {
        billsNote(slot: routeAccount)
            ?? billedEmail(accounts: model.boardAccounts, harness: harness,
                           slot: routeAccount)
    }

    /// What an account row has to say about whose subscription it spends — nil
    /// when it spends the viewer's own, or when we cannot tell whose it is.
    private func billsNote(slot: String?) -> String? {
        guard let billed = billedEmail(accounts: model.boardAccounts,
                                       harness: harness, slot: slot) else { return nil }
        guard crossBilled(billedTo: billed, dispatcher: viewerEmail) else { return nil }
        return billsLabel(billed)
    }

    private var viewerEmail: String? { model.viewerEmail }

    private func billingConfirm(billedTo: String) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            SheetLabel("Confirm")
            VStack(alignment: .leading, spacing: 8) {
                Text(billsWarning(billedTo: billedTo, harness: harness))
                    .font(Theme.sans(14))
                    .foregroundStyle(Theme.text)
                    .fixedSize(horizontal: false, vertical: true)
                Text("The board is set to refuse this unless somebody says it out loud. "
                     + "Back out and pick your own account instead, or release it on theirs.")
                    .font(Theme.sans(12))
                    .foregroundStyle(Theme.textMuted)
                    .fixedSize(horizontal: false, vertical: true)
                Button("Pick a different account") { confirmingBill = nil }
                    .font(Theme.sans(13, weight: .medium))
                    .foregroundStyle(Theme.accent)
                    .padding(.top, 2)
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.warning.opacity(0.10),
                        in: RoundedRectangle(cornerRadius: SheetStyle.cardRadius))
            .overlay(RoundedRectangle(cornerRadius: SheetStyle.cardRadius)
                .strokeBorder(Theme.warning.opacity(0.35), lineWidth: 1))
        }
    }

    private func loadingRow(_ text: String) -> some View {
        Text(text)
            .font(Theme.sans(13))
            .foregroundStyle(Theme.textFaint)
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
            .frame(maxWidth: .infinity, alignment: .leading)
    }

    // MARK: Behaviour

    /// The picker's starting row: the route's runtime when the list offers it
    /// by its canonical name, else the first option the host can actually start.
    /// A route configured with an alias lands on its harness's canonical entry.
    ///
    /// The route's own runtime wins even when the host cannot start it
    /// (gh#187) — "the route sends this to OpenCode, which is not installed on
    /// the box" is precisely the sentence the sheet exists to show. Only the
    /// fallback prefers an available option, so the sheet never *invents* a
    /// dead end nobody chose.
    private func primeSelection() {
        guard runtimeName == nil else { return }
        if let route = target.row.runtime, runtimes.contains(where: { $0.name == route }) {
            runtimeName = route
        } else {
            runtimeName = (runtimes.first { $0.available } ?? runtimes.first)?.name
        }
    }

    private func send(confirmedBill: String?) {
        sending = true
        error = nil
        let account = accountOptions.first { $0.id == accountId }
        let identifier = target.row.identifier
        // The acknowledgement `[defaults] billing_guard = "require-own"` takes:
        // an account slot id, which also selects the account, or the email on
        // the login being spent (the only spelling there is when that login is
        // the box's own). Never on the first send — the guard exists to make a
        // person say it out loud, and a picker that pre-consented would be
        // ticking the box for them.
        let bill: String? = confirmedBill.map { account?.id ?? $0 }
        Task {
            let outcome = await model.dispatchBoardTask(
                taskId: target.row.id,
                // Only when it differs from the route's: the route's runtime is
                // what the board would have used anyway.
                runtime: runtimeName == target.row.runtime ? nil : runtimeName,
                account: account,
                replace: replaceNow,
                bill: bill,
                billedTo: billedTo)
            sending = false
            switch outcome {
            case .dispatched(let chatId):
                let detail = chatId.map { " — chat \($0) is on it" } ?? ""
                onResult("\(target.wasLive ? "Retried" : "Dispatched") \(identifier)\(detail)")
                dismiss()
            case .needsBillingConfirm(let billedTo):
                confirmingBill = billedTo
            case .failed(let message):
                error = message
            }
        }
    }
}
