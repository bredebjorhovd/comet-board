// Sheet design language — the mobile app's grouped-card system (a port of the
// old app's sheet-ui.tsx: panel cards, hairline-separated rows, centered
// headers), restated in this app's monochrome theme. Every sheet composes
// these pieces so they all feel like one product.

import SwiftUI

enum SheetStyle {
    static let cardRadius: CGFloat = Theme.radiusCard
    /// A translucent lift off the panel in dark; the white object on it in
    /// light, where a card that got DARKER than its page would read as a hole
    /// (gh#257 / gh#177's first finding).
    static let cardFill = Theme.card
    static let rowSeparator = Theme.separator
    static let panel = Theme.sheetPanel
}

/// Grouped card: rows separated by inset hairlines.
struct SheetCard<Content: View>: View {
    @ViewBuilder var content: Content

    var body: some View {
        VStack(spacing: 0) {
            content
        }
        .background(SheetStyle.cardFill, in: RoundedRectangle(cornerRadius: SheetStyle.cardRadius))
        .overlay(RoundedRectangle(cornerRadius: SheetStyle.cardRadius)
            .strokeBorder(Theme.border, lineWidth: 1))
    }
}

/// Inset hairline between card rows.
struct SheetSeparator: View {
    var body: some View {
        Rectangle()
            .fill(SheetStyle.rowSeparator)
            .frame(height: 1)
            .padding(.leading, 16)
    }
}

/// Selectable row: title + optional subtitle, accent check when selected.
struct SheetSelectRow: View {
    let title: String
    var subtitle: String?
    var selected: Bool
    var leading: AnyView?
    let action: () -> Void

    var body: some View {
        Button {
            UISelectionFeedbackGenerator().selectionChanged()
            action()
        } label: {
            HStack(spacing: 12) {
                if let leading {
                    leading
                }
                VStack(alignment: .leading, spacing: 2) {
                    Text(title)
                        .font(Theme.sans(Theme.textTitle))
                        .foregroundStyle(Theme.text)
                    if let subtitle, !subtitle.isEmpty {
                        Text(subtitle)
                            .font(Theme.sans(Theme.textDense))
                            .foregroundStyle(Theme.textMuted)
                            .lineLimit(2)
                    }
                }
                Spacer(minLength: 8)
                Image(systemName: "checkmark")
                    .font(.system(size: 14, weight: .semibold))
                    .foregroundStyle(Theme.text)
                    .opacity(selected ? 1 : 0)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }
}

/// Navigation-style row: title + trailing detail + chevron.
struct SheetLinkRow: View {
    let title: String
    var detail: String?
    var systemImage: String?
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 12) {
                if let systemImage {
                    Image(systemName: systemImage)
                        .font(.system(size: 15))
                        .foregroundStyle(Theme.textMuted)
                        .frame(width: 22)
                }
                Text(title)
                    .font(Theme.sans(Theme.textTitle))
                    .foregroundStyle(Theme.text)
                Spacer(minLength: 8)
                if let detail {
                    Text(detail)
                        .font(Theme.sans(Theme.textBody))
                        .foregroundStyle(Theme.textMuted)
                        .lineLimit(1)
                }
                Image(systemName: "chevron.right")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.textFaint)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)
            .contentShape(Rectangle())
        }
        .buttonStyle(SheetRowButtonStyle())
    }
}

/// Uppercase tracked section label above a card.
struct SheetLabel: View {
    let text: String

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        Text(text.uppercased())
            .font(Theme.sans(Theme.textCaption, weight: .medium))
            .kerning(1)
            .foregroundStyle(Theme.textSubtle)
            .padding(.horizontal, 4)
    }
}

/// Row press feedback: brief white wash, like UIKit cell highlight.
struct SheetRowButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(configuration.isPressed ? Theme.elementHover : .clear)
    }
}

/// Primary pill button pinned at a sheet's bottom.
struct SheetPrimaryButton: View {
    let title: String
    var enabled = true
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(Theme.sans(Theme.textTitle, weight: .semibold))
                .foregroundStyle(enabled ? Theme.bg : Theme.textFaint)
                .frame(maxWidth: .infinity)
                .frame(height: 50)
                .background(enabled ? AnyShapeStyle(Theme.text) : AnyShapeStyle(Theme.chip),
                            in: RoundedRectangle(cornerRadius: Theme.radiusRow))
        }
        .buttonStyle(.plain)
        .disabled(!enabled)
    }
}

/// Pressed-state wash for tappable rows — the canvas's `--hover`, which on a
/// touch screen is what a finger holds down rather than what a pointer rests
/// on. Fades out on release.
struct PressWashButtonStyle: ButtonStyle {
    var cornerRadius: CGFloat = Theme.radiusRow

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(configuration.isPressed ? Theme.elementHover : Color.clear,
                        in: RoundedRectangle(cornerRadius: cornerRadius))
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}

/// A row that lights the way the canvas draws a selected one — `--sel` (or
/// `--selcard` inside a card) under the `--sellift` ring (ios.md B2.5 / B4.7 /
/// C4.7).
///
/// **A phone has no resting selection**, which is why this is a press style and
/// not a `selected: Bool`. The desktop keeps a cursor row because a pointer can
/// hover somewhere it has not committed to; here a tap IS the commit, and what
/// the row does in the instant before the push is the only selected state that
/// exists. So the canvas's lit row is drawn at the moment it is being chosen
/// rather than for as long as it stays chosen.
///
/// `PressWashButtonStyle` remains for rows the canvas never lights — sheet
/// rows, chips, pickers — where `--hover` is the whole of the feedback.
struct SelectRowButtonStyle: ButtonStyle {
    var cornerRadius: CGFloat = Theme.radiusCard
    /// The row sits inside a card, so it takes `--selcard` rather than `--sel`.
    var card = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .selectedRow(configuration.isPressed, cornerRadius: cornerRadius, card: card)
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}

extension ButtonStyle where Self == SelectRowButtonStyle {
    /// A board row, which the canvas lights full-bleed and un-rounded: the
    /// section is the card, and a row inside it is a line (ios.md C4.1).
    static var fullBleedSelect: SelectRowButtonStyle {
        // scale-ok: full bleed is the absence of a radius, not a fourth step.
        SelectRowButtonStyle(cornerRadius: 0, card: true)
    }
}

extension ButtonStyle where Self == PressWashButtonStyle {
    /// The same wash on a row that runs edge to edge — a board row, which the
    /// canvas draws as a line in a ledger rather than as a card (ios.md C4.1).
    ///
    static var fullBleedPressWash: PressWashButtonStyle {
        // scale-ok: the absence of a radius is not a fourth step on the
        // scale, and it is spelled once, here, rather than at the call site.
        PressWashButtonStyle(cornerRadius: 0)
    }
}

/// Chip press feedback: deepens the existing fill while pressed, in the chip's
/// own shape — a rounded `radiusChip` box by default, and a pill for the two
/// chips the canvas draws as pills (a board row's verb, ios.md C4.6).
struct ChipPressButtonStyle: ButtonStyle {
    enum Shape {
        case chip, capsule
    }

    var shape: Shape = .chip

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .overlay {
                let wash = configuration.isPressed ? Theme.elementHover : Color.clear
                switch shape {
                // round-ok: the pill variant of a chip's own press feedback
                case .capsule: Capsule().fill(wash)
                case .chip: RoundedRectangle(cornerRadius: Theme.radiusChip).fill(wash)
                }
            }
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}
