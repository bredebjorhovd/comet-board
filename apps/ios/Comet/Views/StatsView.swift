// Board stats on the phone (gh#143, gh#151): what the board did with the work
// it was given, on the screen you have in your hand.
//
// **Not the desktop page.** That one is a two-column dashboard built for
// 1160px, and it was JUST redesigned away from a scrolling stack of cards
// because assembling one answer out of five tiles and a scroll is not an
// answer. A phone is that problem in its extreme form — so the part that
// carries over is the top-left panel, and only it: the count, the facts that
// qualify it in one line, and where the work went. Everything below that panel
// is evidence for it, and the reader is free to stop after the first screen.
//
// The numbers come from `BoardStats` on whichever device owns `board.db` — a
// call, not a stream, read when the screen opens and on every window change.
// Every derivation the rendering needs is in StatsModels.swift, the port of
// `comet_proto::view::stats`, so this screen and the desktop page and the CLI
// agree on the arithmetic.
//
// Honest empties are load-bearing here: no completion rate before anything has
// ended, no token totals for a window that metered nothing, and a coverage
// line beside the totals that did survive.

import SwiftUI

struct StatsView: View {
    @Environment(AppModel.self) private var model

    /// The window in days; nil is all time. Mirrors the CLI's `--since-days`
    /// and the desktop's picker, so all three surfaces ask one question.
    @State private var sinceDays: Int64? = 7
    @State private var stats: BoardStats?
    /// The device that answered — the board is on exactly one of them.
    @State private var host: String?
    @State private var error: String?
    @State private var loaded = false

    /// How many rows a tally shows before folding the rest into `n others`.
    private static let tallyRows = 5

    var body: some View {
        List {
            windowPicker
            if let error {
                errorStrip(error)
            }
            if let stats {
                if stats.isEmpty {
                    note("No dispatches in the \(stats.windowLabel). "
                         + "Release a task and this fills in.")
                } else {
                    answerPanel(stats)
                    dailySection(stats)
                    tokensSection(stats)
                    glanceSection(stats)
                    runtimeSection(stats)
                }
            } else if !loaded {
                loadingRow
            } else if error == nil {
                note("No board answered.")
            }
        }
        .listStyle(.plain)
        .environment(\.defaultMinListRowHeight, 10)
        .scrollContentBackground(.hidden)
        .scrollEdgeEffectStyle(.soft, for: .top)
        .background(Theme.surface.ignoresSafeArea())
        .navigationTitle("Stats")
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .principal) {
                VStack(spacing: 1) {
                    Text("Board stats")
                        .font(Theme.sans(13, weight: .medium))
                        .foregroundStyle(Theme.text)
                    if let host {
                        Text("on \(model.deviceName(host))")
                            .font(Theme.sans(10.5))
                            .foregroundStyle(Theme.textMuted.opacity(0.6))
                    }
                }
            }
        }
        // Reload on the window AND on the board's host changing. Opening the
        // app straight onto this screen beats the board sweep to an answer —
        // the relay says "the device is offline" while the host is still
        // attaching — and a one-shot read would leave that error on screen
        // until somebody pulled to refresh. The host settling is the moment to
        // ask again.
        //
        // The old numbers stay up while the new ones land: blanking on every
        // window change makes a two-tap comparison flicker.
        .task(id: loadKey) { await load() }
        .refreshable { await load() }
    }

    // MARK: Loading

    /// What a fresh read depends on: the window asked for, and which device is
    /// answering. Either changing is a reason to ask again.
    private var loadKey: String {
        "\(sinceDays.map(String.init) ?? "all")|\(model.boardHostDeviceId ?? "-")"
    }

    private func load() async {
        error = nil
        switch await model.boardStats(sinceDays: sinceDays) {
        case .read(let read, let answeredBy):
            stats = read
            host = answeredBy
        case .failed(let message):
            error = message
        }
        loaded = true
    }

    // MARK: The window

    /// Four chips, current one filled. A segmented control would read as a
    /// system form; the board's own chips are what the rest of the app uses.
    private var windowPicker: some View {
        HStack(spacing: 6) {
            ForEach(statsWindows, id: \.label) { window in
                let selected = sinceDays == window.days
                Button {
                    guard sinceDays != window.days else { return }
                    sinceDays = window.days
                } label: {
                    Text(window.label)
                        .font(Theme.sans(12, weight: selected ? .medium : .regular))
                        .foregroundStyle(selected ? Theme.text : Theme.textMuted)
                        .frame(maxWidth: .infinity)
                        .padding(.vertical, 6)
                        .background(selected ? Theme.elementActive : Color.clear,
                                    in: RoundedRectangle(cornerRadius: 7))
                }
                .buttonStyle(ChipPressButtonStyle())
            }
        }
        .padding(2)
        .background(whiteAlpha(0.02), in: RoundedRectangle(cornerRadius: 9))
        .overlay(RoundedRectangle(cornerRadius: 9).strokeBorder(Theme.border, lineWidth: 1))
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
        .listRowInsets(EdgeInsets(top: 8, leading: 12, bottom: 6, trailing: 12))
    }

    // MARK: The answer

    /// The count, the facts that qualify it, and where the work went — the one
    /// panel the desktop page and this screen share, because it is the whole
    /// answer to "is delegating working".
    private func answerPanel(_ stats: BoardStats) -> some View {
        let facts = statsHeadlineFacts(stats)
        let spaces = rankedTop(stats.byWorkspace, 4)
        let peak = spaces.first?.count ?? 0
        return Section {
            VStack(alignment: .leading, spacing: 14) {
                VStack(alignment: .leading, spacing: 3) {
                    Text(statsHeadline(stats))
                        .font(Theme.sans(30, weight: .semibold))
                        .foregroundStyle(Theme.text)
                    // Only facts that exist: a board with nothing ended has no
                    // rate and no median, and an em-dash for each would read as
                    // two failures rather than as a board that just started.
                    Text(facts.isEmpty
                         ? "across \(stats.tasksTouched) task\(stats.tasksTouched == 1 ? "" : "s")"
                         : facts.joined(separator: " · "))
                        .font(Theme.sans(12.5))
                        .foregroundStyle(Theme.textMuted)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if !spaces.isEmpty {
                    VStack(alignment: .leading, spacing: 7) {
                        ForEach(spaces) { row in
                            tallyRow(label: row.label, value: "\(row.count)",
                                     fraction: barFraction(row.count, peak))
                        }
                    }
                }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 16)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.surfaceRaised.opacity(0.5),
                        in: RoundedRectangle(cornerRadius: Theme.panelRadius))
            .overlay(RoundedRectangle(cornerRadius: Theme.panelRadius)
                .strokeBorder(Theme.border, lineWidth: 1))
            .listRowBackground(Color.clear)
            .listRowSeparator(.hidden)
            .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 4, trailing: 12))
        }
    }

    // MARK: Per day

    /// Dispatches per day, with the share that ended `done` filled in.
    ///
    /// Two tones in one bar rather than two bars: the question is what
    /// proportion of a day's work landed, and side-by-side bars make that a
    /// subtraction the reader has to do.
    @ViewBuilder
    private func dailySection(_ stats: BoardStats) -> some View {
        let peak = peakDispatches(stats.daily)
        if peak > 0 {
            Section {
                VStack(alignment: .leading, spacing: 8) {
                    HStack(alignment: .bottom, spacing: 3) {
                        ForEach(stats.daily, id: \.date) { day in
                            dayBar(day, peak: peak)
                        }
                    }
                    .frame(height: 72)
                    HStack {
                        Text(shortDay(stats.daily.first?.date ?? ""))
                        Spacer(minLength: 4)
                        Text("peak \(peak)/day")
                        Spacer(minLength: 4)
                        Text(shortDay(stats.daily.last?.date ?? ""))
                    }
                    .font(Theme.sans(10.5))
                    .foregroundStyle(Theme.textFaint)
                }
                .padding(.horizontal, 8)
                .padding(.vertical, 4)
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 6, trailing: 12))
            } header: {
                header("Dispatches per day", aside: "solid = ended in done")
            }
        }
    }

    private func dayBar(_ day: DayBucket, peak: Int) -> some View {
        // A day with something in it never draws as nothing: a 1-of-40 bar
        // rounds to a hairline, and a hairline that is missing reads as a
        // quiet day rather than a busy one somebody scaled away.
        let total = max(72 * barFraction(day.dispatches, peak), day.dispatches > 0 ? 2 : 0)
        let done = 72 * min(barFraction(day.done, peak), barFraction(day.dispatches, peak))
        return VStack(spacing: 0) {
            Spacer(minLength: 0)
            ZStack(alignment: .bottom) {
                RoundedRectangle(cornerRadius: 3)
                    .fill(Theme.accent.opacity(0.22))
                    .frame(height: total)
                RoundedRectangle(cornerRadius: 3)
                    .fill(Theme.accent.opacity(0.85))
                    .frame(height: done)
            }
        }
        .frame(maxWidth: .infinity)
    }

    // MARK: Tokens (gh#151)

    /// What the window cost, in counts only — what a token costs depends on a
    /// seat and a price table the board does not have, and a number that looks
    /// like money is read as money.
    @ViewBuilder
    private func tokensSection(_ stats: BoardStats) -> some View {
        let coverage = coverageNote(stats)
        if stats.hasTokens {
            Section {
                VStack(alignment: .leading, spacing: 10) {
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text(humanTokens(stats.tokens.total))
                            .font(Theme.sans(22, weight: .semibold))
                            .foregroundStyle(Theme.text)
                        Text("processed")
                            .font(Theme.sans(11.5))
                            .foregroundStyle(Theme.textMuted.opacity(0.75))
                        Spacer(minLength: 0)
                    }
                    ForEach(statsTokenLines(stats.tokens), id: \.label) { line in
                        factRow(line.label, line.value)
                    }
                }
                .padding(.horizontal, 8)
                .padding(.vertical, 4)
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 4, trailing: 12))

                // Where they went. One column of ranked rows, never the
                // desktop's four-column table — a table that has to scroll
                // sideways is a table nobody reads.
                let models = rankedTokens(stats.tokensByModel, Self.tallyRows)
                if !models.isEmpty {
                    let peak = models.first?.usage.total ?? 0
                    VStack(alignment: .leading, spacing: 7) {
                        Text("By model")
                            .font(Theme.sans(11, weight: .medium))
                            .foregroundStyle(Theme.textMuted.opacity(0.6))
                        ForEach(models) { row in
                            tallyRow(label: row.label,
                                     value: humanTokens(row.usage.total),
                                     fraction: barFraction(row.usage.total, peak))
                        }
                    }
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .listRowBackground(Color.clear)
                    .listRowSeparator(.hidden)
                    .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 6, trailing: 12))
                }
            } header: {
                header("Tokens", aside: coverage)
            }
        } else {
            Section {
                // Never a wall of zeroes: nothing reported is a fact about the
                // board's records, not about what the work cost.
                note("No attempt in this window reported token usage. Attempts from "
                     + "before the board recorded it stay blank rather than reading as free.")
            } header: {
                header("Tokens", aside: coverage)
            }
        }
    }

    // MARK: At a glance

    private func glanceSection(_ stats: BoardStats) -> some View {
        Section {
            VStack(alignment: .leading, spacing: 9) {
                // The coverage line earns a row of its own here, because
                // "reported tokens" qualifies everything above it.
                if let coverage = percentLabel(stats.tokenCoverage) {
                    factRow("Reported tokens", "\(coverage) of attempts")
                }
                ForEach(statsGlanceLines(stats), id: \.label) { line in
                    factRow(line.label, line.value)
                }
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 4)
            .listRowBackground(Color.clear)
            .listRowSeparator(.hidden)
            .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 6, trailing: 12))
        } header: {
            header("At a glance", aside: nil)
        }
    }

    // MARK: Who ran it

    @ViewBuilder
    private func runtimeSection(_ stats: BoardStats) -> some View {
        let rows = rankedTop(stats.byRuntime, Self.tallyRows)
        if !rows.isEmpty {
            let peak = rows.first?.count ?? 0
            Section {
                VStack(alignment: .leading, spacing: 7) {
                    ForEach(rows) { row in
                        tallyRow(label: row.label, value: "\(row.count)",
                                 fraction: barFraction(row.count, peak))
                    }
                }
                .padding(.horizontal, 8)
                .padding(.vertical, 4)
                .listRowBackground(Color.clear)
                .listRowSeparator(.hidden)
                .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 24, trailing: 12))
            } header: {
                header("By runtime", aside: nil)
            }
        }
    }

    // MARK: Pieces

    /// Label, bar, value — the tally shape, one line high.
    private func tallyRow(label: String, value: String, fraction: Double) -> some View {
        HStack(spacing: 10) {
            Text(label)
                .font(Theme.sans(12))
                .foregroundStyle(Theme.textMuted)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(width: 108, alignment: .leading)
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    Capsule().fill(whiteAlpha(0.05))
                    Capsule()
                        .fill(Theme.accent.opacity(0.55))
                        .frame(width: max(0, geo.size.width * fraction))
                }
            }
            .frame(height: 5)
            Text(value)
                .font(Theme.mono(11))
                .foregroundStyle(Theme.textMuted)
                .frame(width: 46, alignment: .trailing)
        }
    }

    /// `label — value`, the line a fact reads as.
    private func factRow(_ label: String, _ value: String) -> some View {
        HStack(spacing: 12) {
            Text(label)
                .font(Theme.sans(12.5))
                .foregroundStyle(Theme.textMuted)
            Spacer(minLength: 8)
            Text(value)
                .font(Theme.sans(12.5, weight: .medium))
                .foregroundStyle(Theme.text)
                .lineLimit(1)
        }
    }

    private func header(_ title: String, aside: String?) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(title)
                .font(Theme.sans(11, weight: .medium))
                .foregroundStyle(Theme.textMuted.opacity(0.6))
            if let aside {
                Text(aside)
                    .font(Theme.sans(10.5))
                    .foregroundStyle(Theme.textFaint.opacity(0.8))
                    .lineLimit(2)
            }
        }
        .textCase(nil)
        .frame(maxWidth: .infinity, alignment: .leading)
        .listRowInsets(EdgeInsets(top: 10, leading: 16, bottom: 3, trailing: 16))
    }

    private func note(_ text: String) -> some View {
        Text(text)
            .font(Theme.sans(12.5))
            .foregroundStyle(Theme.textFaint)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
            .listRowBackground(Color.clear)
            .listRowSeparator(.hidden)
            .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 6, trailing: 12))
    }

    private func errorStrip(_ message: String) -> some View {
        Text(message)
            .font(Theme.sans(12))
            .foregroundStyle(Theme.dangerSoft)
            .fixedSize(horizontal: false, vertical: true)
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(Theme.danger.opacity(0.10),
                        in: RoundedRectangle(cornerRadius: 8))
            .listRowBackground(Color.clear)
            .listRowSeparator(.hidden)
            .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 4, trailing: 12))
    }

    private var loadingRow: some View {
        HStack(spacing: 8) {
            ProgressView().controlSize(.small).tint(Theme.textMuted)
            Text("Reading the board…")
                .font(Theme.sans(12.5))
                .foregroundStyle(Theme.textFaint)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 10)
        .listRowBackground(Color.clear)
        .listRowSeparator(.hidden)
        .listRowInsets(EdgeInsets(top: 2, leading: 12, bottom: 6, trailing: 12))
    }
}
