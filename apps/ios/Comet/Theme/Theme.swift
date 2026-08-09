// Always-dark monochrome theme — a direct port of crates/ui/src/theme.rs.
//
// Colors are computed from the same oklch definitions the desktop app uses
// (Björn Ottosson's OKLab matrices, the ones CSS Color 4 specifies), so every
// surface and accent lands on identical sRGB values. **Numbers drive layout,
// colors are paint**: layout constants are plain numbers and never depend on
// which color is painted.
//
// # The phone reads from the desktop's system (gh#181)
//
// Four of gh#171's findings were theme-level rather than screen-level, so they
// cross to the phone unchanged. They arrived here late on purpose: the desktop
// had to settle on four greys, four hues, three radii and four sizes first,
// because a phone that adopts a system still being argued about becomes the
// third opinion rather than the second surface.
//
// What the port is, finding by finding:
//
// 1. **Four text tones, and no multipliers** (gh#172). Text paints in exactly
//    four greys — `text` / `textMuted` / `textSubtle` / `textFaint` — and **a
//    text tone is never multiplied by an alpha at the call site**. Before this
//    rule the three tones that existed were multiplied by eleven different
//    factors across the app: sublines at `.5`, labels at `.6`, chip text at
//    `.85`, a folder row at `.4`. That is a dozen unnamed greys, none of them
//    contrast-checked, several landing two percent apart and reading as noise
//    rather than as levels. `ios_theme.rs` fails the build on a new one.
// 2. **Four status hues at one lightness** (gh#173). See `Status` below. The
//    phone had the exact disagreement the desktop fixed: `boardStateColor`
//    said amber and `ChatIndicator.dotColor` said pink about the same running
//    agent, one screen apart.
// 3. **Three radii with a nesting rule** (gh#174). Fourteen radii were in use
//    here — 3, 3.5, 4.5, 5, 6, 7, 8, 9, 10, 12, 14, 16, 26, 28 — plus capsules
//    on things that were not pills.
// 4. **Four type sizes** (gh#174). Fifteen were in use, including 10.5, 11.5,
//    12.5 and 13.5. A half-point step is not a level of hierarchy, it is a
//    different decision made on a different day.
//
// Not ported: gh#175's hover/selection split (a phone has no pointer, and
// selection here is navigation, not a resting state) and gh#177's light theme
// (this app is always-dark; when that changes it is its own ticket, because a
// light design is a design and not an inversion).
//
// The desktop guards these rules with `crates/ui/tests/{text_tones,scale}.rs`.
// The phone's half is `crates/ui/tests/ios_theme.rs` — the same three scans,
// reading Swift, plus a parity check that the numbers below still equal the
// Rust ones. It walks source text, so unlike `SpecRunner` it needs no simulator
// and does run in CI.

import SwiftUI

// MARK: - Status: what a state MEANS

/// What a state MEANS, in the only vocabulary the status ramp understands
/// (gh#173) — a port of `comet_ui::theme::Status`.
///
/// Four meanings, four hues. The types a state arrives in (a board row's
/// `BoardState`, a live attempt's `AgentState`, a chat's `ChatIndicator`) each
/// translate into this exactly once, so two screens rendering the same state
/// cannot pick different paint.
enum Status {
    /// Stopped, and it needs a human: blocked, failed, errored.
    case blocked
    /// Running on its own — an agent is working, on the board AND on Home.
    case working
    /// Finished and unlooked-at: review, a question, and the links and focus
    /// that lead there.
    case review
    /// Settled: seen, healthy, online.
    case settled

    /// A board row's state as a meaning. `nil` for the states that spend no
    /// colour: `ready` is a queue entry (it reads as its own text) and `done`
    /// is history.
    static func ofBoard(_ state: BoardState) -> Status? {
        switch state {
        // One hue for both — the glyph tells a dead run from a gate.
        case .blocked, .failed: return .blocked
        case .working: return .working
        case .review: return .review
        case .ready, .done: return nil
        }
    }

    /// A live attempt's state as a meaning. The COARSE reading: a question and
    /// a corpse are both `blocked` here, told apart by the row's glyph.
    static func ofAgent(_ state: AgentState) -> Status {
        switch state {
        case .blocked, .errored: return .blocked
        case .working: return .working
        }
    }

    /// A chat's display status as a meaning. `nil` for idle — a chat nobody is
    /// waiting on is not a status, and its dot is a hairline, not a hue.
    ///
    /// This is the FINE reading, where a dot has no glyph to carry the
    /// difference: a question is the review hue (something wants your eyes, and
    /// the run is healthy), a dead run is the blocked hue.
    static func ofChat(_ indicator: ChatIndicator) -> Status? {
        switch indicator {
        case .working: return .working
        case .awaitingInput: return .review
        case .errored: return .blocked
        case .completed: return .settled
        case .idle: return nil
        }
    }
}

// MARK: - Theme

enum Theme {
    // ---- paint: neutral surfaces (oklch chroma 0) ----
    /// Main panel background — sampled #060606.
    static let bg = grey(6)
    /// Shell / sidebar surface — sampled #0d0d0d.
    static let surface = grey(13)
    /// Raised surface: popovers, dialogs, cards.
    static let surfaceRaised = neutral(0.235)
    /// Pressed wash for interactive rows — the desktop's `element_hover`, which
    /// on a touch screen is what a finger holds down rather than what a pointer
    /// rests on.
    static let elementHover = wash(0.07)
    /// Active/selected wash.
    static let elementActive = wash(0.10)
    /// Hairline border — white at low alpha so it reads on any surface.
    static let border = whiteAlpha(0.08)
    /// Stronger border for focused/raised edges.
    static let borderStrong = whiteAlpha(0.14)

    // ---- paint: text — four tones, never multiplied (gh#172) ----
    /// Headings, titles, the selected row. ~16.9:1 on `bg`.
    static let text = neutral(0.938)
    /// Body copy and unselected rows — the default reading tone. ~8.4:1.
    static let textMuted = neutral(0.728)
    /// Labels, metadata, captions, timestamps, sublines. ~5.1:1 — still AA body
    /// text, which the `.opacity(0.5)` sublines this token replaces were not.
    static let textSubtle = neutral(0.598)
    /// Disabled controls and placeholders, and nothing else. ~3.5:1 — the
    /// floor, so anything a user is meant to READ sits at `textSubtle` or up.
    static let textFaint = neutral(0.508)

    // ---- the status ramp: four hues, one L, one C (gh#173) ----
    // Anchored in `comet_proto::view::status`, because the desktop app, the
    // terminal app and this one paint the same meanings and must land on the
    // same hues. `crates/ui/tests/ios_theme.rs` asserts these five numbers
    // still equal the Rust ones.
    /// The lightness every status hue is anchored to. One number, so "how loud
    /// is this state" is decided by the state and never by its hue.
    static let statusL: Double = 0.74
    /// The chroma every status hue carries.
    static let statusC: Double = 0.14
    /// Blocked · failed · errored.
    static let hueBlocked: Double = 25
    /// Working — an agent is running.
    static let hueWorking: Double = 75
    /// Review · a question · links · focus.
    static let hueReview: Double = 265
    /// Settled · seen · online.
    static let hueSettled: Double = 160

    /// Accent — indigo, `Status.review`'s hue: review, a question, links,
    /// focus, selection tint.
    static let accent = oklch(statusL, statusC, hueReview)
    /// The accent hue at fill weight — off the ramp on purpose: a filled button
    /// is not a status, and a status-weight fill under white text would not
    /// hold contrast. Same hue, so the two read as one colour.
    static let accentStrong = oklch(0.62, 0.19, hueReview)
    /// Danger — red, `Status.blocked`'s hue: errors, the stop button.
    static let danger = oklch(statusL, statusC, hueBlocked)
    /// Warning — amber, `Status.working`'s hue: a running agent, offline
    /// notices. It sat at L 0.828 before the anchor, where it read twice as
    /// loud as the accent even when it meant less — which is why a fifth hue
    /// (pink) had been minted for "working" in the first place.
    static let warning = oklch(statusL, statusC, hueWorking)
    /// Settled — emerald, `Status.settled`'s hue: finished chats, an online
    /// device, an active account.
    static let settled = oklch(statusL, statusC, hueSettled)

    /// **The** answer to "what colour is this state" (gh#173). The board rows,
    /// the Agents section, the chat dots and the Needs-you inbox all arrive
    /// here, so a state cannot be one colour on one screen and another colour
    /// a tap away.
    static func status(_ status: Status) -> Color {
        switch status {
        case .blocked: return danger
        case .working: return warning
        case .review: return accent
        case .settled: return settled
        }
    }

    /// The danger text/icon tone for error UI (error chips, failure notices) —
    /// the light red-300, readable on a near-black chip. Borders and washes
    /// stay on `danger`; only the foreground lightens.
    static let dangerText = oklch(0.808, 0.114, 19.571) // red-300
    /// The warning text/icon tone for notice UI (offline notices) — amber-200.
    static let warningText = oklch(0.924, 0.12, 95.746) // amber-200
    /// The settled text/icon tone for presence UI (an "Active" badge, a copied
    /// id's confirmation): the ramp's hue, off its lightness, because this one
    /// sits ON a fill of its own colour.
    static let settledText = oklch(0.88, 0.11, hueSettled)

    /// Claude brand orange — kept even on the mono surface. Not a status: it
    /// identifies a harness, and `Status` is about what a run is doing.
    static let claudeBrand = Color(red: 0xD9 / 255.0, green: 0x77 / 255.0, blue: 0x57 / 255.0)

    // ---- paint: markdown inline code (violet family) ----
    static let inlineCodeText = oklch(0.811, 0.111, 293.571)  // violet-300
    static let inlineCodeWash = oklch(0.702, 0.183, 293.541).opacity(0.12) // violet-400 @ 0.12

    // ---- paint: syntax tokens (soft, paint-only) ----
    static let tokenKeyword = oklch(0.709, 0.129, 20.0)   // soft rose
    static let tokenString = oklch(0.770, 0.110, 168.0)   // soft green
    static let tokenNumber = oklch(0.780, 0.120, 80.0)    // soft amber

    // ---- three radii, and the nesting rule (gh#174) ----
    /// The innermost step: chips, badges, key caps, small icon buttons,
    /// avatars, swatches — anything that sits INSIDE a `radiusRow` row.
    static let radiusChip: CGFloat = 6
    /// The middle step: rows, menu items, inputs, tabs, tiles — the things that
    /// sit inside a `radiusCard` card.
    static let radiusRow: CGFloat = 10
    /// The outermost step: cards, dialogs, sheets, message bubbles, the
    /// composer. Nothing is rounder than this except the send button.
    static let radiusCard: CGFloat = 14
    /// The gutter that makes the nesting rule true. `inner = outer − padding`:
    /// a `radiusRow` row inset by this much inside a `radiusCard` card keeps
    /// the two curves concentric, and the same step takes a chip out of a row.
    /// It is `spaceXS` — the scale was chosen so the gutter the cards already
    /// used is the one the rule wants.
    static let nestGutter: CGFloat = radiusCard - radiusRow

    // ---- four type sizes, plus prose and one figure (gh#174) ----
    /// Captions: labels, metadata, key hints, timestamps, badge text — the
    /// smallest type that ships.
    static let textCaption: CGFloat = 11
    /// Dense rows: the board list, Home's sections, tables — where many lines
    /// stack and every point of height is spent twice.
    static let textDense: CGFloat = 12
    /// UI body — the default. Buttons, menu items, form fields, section copy.
    static let textBody: CGFloat = 13
    /// Titles: screen headers, sheet titles, empty-state headings. The top of
    /// the UI ramp; anything louder is a matter of weight, not size.
    static let textTitle: CGFloat = 15
    /// Prose, and only prose: rendered markdown, the message bubbles, and the
    /// composer you type them into. Reserved — a transcript is reading, not
    /// chrome, and it does not share the UI ramp.
    static let textProse: CGFloat = 14
    /// The line height `textProse` is set on.
    static let proseLineHeight: CGFloat = 22
    /// A number shown AS a number: the stats headline, a login code read
    /// aloud. The one size off the UI ramp, because a figure at title size
    /// stops being a figure.
    static let textFigure: CGFloat = 21

    // ---- numbers drive layout (pt) ----
    static let spaceXS: CGFloat = 4
    static let spaceSM: CGFloat = 8
    static let spaceMD: CGFloat = 12
    static let spaceLG: CGFloat = 16
}

// MARK: - Fonts

extension Theme {
    static let fontSansName = "Geist"
    static let fontMonoName = "GeistMono-Regular"

    static func sans(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        // Static weight cuts register as separate families — select by
        // PostScript name so weights actually resolve.
        let name: String
        if weight == .medium {
            name = "Geist-Medium"
        } else if weight == .semibold {
            name = "Geist-SemiBold"
        } else if weight == .bold {
            name = "Geist-Bold"
        } else {
            name = "Geist-Regular"
        }
        return .custom(name, size: size)
    }

    static func mono(_ size: CGFloat, weight: Font.Weight = .regular) -> Font {
        .custom(fontMonoName, size: size).weight(weight)
    }

    static func sansUI(_ size: CGFloat, weight: UIFont.Weight = .regular) -> UIFont {
        let traits: [UIFontDescriptor.TraitKey: Any] = [.weight: weight]
        let descriptor = UIFontDescriptor(fontAttributes: [
            .family: "Geist",
            .traits: traits,
        ])
        return UIFont(descriptor: descriptor, size: size)
    }

    static func monoUI(_ size: CGFloat) -> UIFont {
        UIFont(name: fontMonoName, size: size)
            ?? .monospacedSystemFont(ofSize: size, weight: .regular)
    }
}

// MARK: - Color primitives (ported from theme.rs)

/// A neutral (chroma 0) oklch tone. Chroma 0 means r == g == b exactly.
func neutral(_ lightness: Double) -> Color {
    let v = Double(oklchToSrgb(l: lightness, c: 0, hDeg: 0)[0])
    return Color(red: v, green: v, blue: v)
}

/// White at the given alpha — the hairline primitive.
func whiteAlpha(_ alpha: Double) -> Color {
    Color.white.opacity(alpha)
}

/// Interactive-state wash: translucent soft-white rather than pure white, so a
/// press fades from the surface's own tone instead of flashing.
func wash(_ alpha: Double) -> Color {
    Color(red: 0.92, green: 0.92, blue: 0.92).opacity(alpha)
}

/// An exact achromatic tone from an 8-bit channel value (`grey(13)` ≡ #0d0d0d).
func grey(_ value: UInt8) -> Color {
    let v = Double(value) / 255.0
    return Color(red: v, green: v, blue: v)
}

/// oklch (CSS notation: L 0..1, C, H degrees) → sRGB Color.
func oklch(_ l: Double, _ c: Double, _ hDeg: Double) -> Color {
    let rgb = oklchToSrgb(l: l, c: c, hDeg: hDeg)
    return Color(red: Double(rgb[0]), green: Double(rgb[1]), blue: Double(rgb[2]))
}

/// oklch → sRGB (each 0..1, clamped/gamut-clipped per channel).
func oklchToSrgb(l: Double, c: Double, hDeg: Double) -> [Double] {
    let h = hDeg * .pi / 180
    let a = c * cos(h)
    let b = c * sin(h)

    // OKLab → LMS (cube roots undone)
    let l_ = l + 0.39633778 * a + 0.21580376 * b
    let m_ = l - 0.105561346 * a - 0.06385417 * b
    let s_ = l - 0.08948418 * a - 1.2914855 * b
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_)

    // LMS → linear sRGB
    let r = 4.0767417 * l3 - 3.3077116 * m3 + 0.23096993 * s3
    let g = -1.268438 * l3 + 2.6097574 * m3 - 0.3413194 * s3
    let bl = -0.0041960863 * l3 - 0.7034186 * m3 + 1.7076147 * s3

    return [gammaEncode(r), gammaEncode(g), gammaEncode(bl)]
}

private func gammaEncode(_ x: Double) -> Double {
    let x = min(max(x, 0), 1)
    return x <= 0.0031308 ? 12.92 * x : 1.055 * pow(x, 1.0 / 2.4) - 0.055
}
