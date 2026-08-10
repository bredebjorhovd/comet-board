// Monochrome theme, in two variants. Dark and the shared scales are a direct
// port of crates/ui/src/theme.rs; light is the supplied iOS reference's palette.
//
// Colors are computed from the same oklch definitions the desktop app uses
// (Björn Ottosson's OKLab matrices, the ones CSS Color 4 specifies), so every
// surface and accent lands on identical sRGB values. **Numbers drive layout,
// colors are paint**: layout constants are plain numbers and never depend on
// which color is painted.
//
// # Light is a design, not an inversion (gh#257)
//
// Every paint token below is declared with `themed(dark:light:)`, which builds
// one `Color` that resolves against the trait collection it is drawn into. The
// consequence is the point: **no view asks which scheme it is in.** The 400-odd
// call sites across this app were written against `Theme.text` and
// `whiteAlpha(0.06)` and they are unchanged — a token knowing its own two
// values is what keeps `if colorScheme == .light` from spreading through the
// screens, which is how a light theme rots into a per-view opinion.
//
// ## Where the light numbers come from
//
// **The design doc for this phone, `Comet iOS.dc.html`, and not the desktop's
// `Theme::light()`.** The two disagree, and the disagreement is deliberate on
// the doc's side: it draws phone screens, so it declares a phone's surface
// system rather than a shell-beside-a-panel one. Every light value below is
// transcribed from its `.cw[data-theme="light"]` block, named in the comment
// that carries it, and asserted against that same table by
// `crates/ui/tests/ios_theme.rs`:
//
//     --card #ffffff   --raised #f4f4f7   --sel #ffffff   --selcard #eeeef2
//     --chip rgba(0,0,0,.05)   --line #e4e4e8   --line2 #d6d6dc
//     --text #171717   --muted #545454   --subtle #6b6b6b   --faint #8a8a8a
//     --blocked/working/review/settled  oklch(0.52 0.16 <hue>)
//     --claude #c15f3c
//
// So: the page is white and elevation is a TINT DOWN from it (`--raised`), the
// hairlines are opaque hexes rather than a translucent ink, and the status ramp
// sits at L 0.52 / C 0.16 rather than the desktop light theme's 0.55 / 0.14.
// **Dark is untouched and still the desktop's**, number for number — the phone
// and the laptop agree about dark, and about the whole of the scale, the radii
// and the four contrast steps. What the phone now owns alone is which paint
// those steps land on when the lights are on.
//
// A handful of tokens have no counterpart in the doc, because the screens it
// draws never show them: `accentStrong` (no filled accent button), the three
// `*Text` foregrounds, the inline-code violet and the three syntax tones. Each
// says at its declaration where its light value came from instead.
//
// The phone owns one more thing the desktop does not: `Appearance.swift` maps a
// stored preference onto the window. Its stored default is System, but the
// ring-fenced `Info.plist` key still forces Dark app-wide, so System cannot
// follow the device until that separate integration dependency is removed.
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
// selection here is navigation, not a resting state). gh#177's light theme was
// the fifth finding and it arrived last, in gh#257 — see above.
//
// The desktop guards these rules with `crates/ui/tests/{text_tones,scale}.rs`.
// The phone's half is `crates/ui/tests/ios_theme.rs` — the same three scans,
// reading Swift, plus a parity check that the numbers below still equal the
// Rust ones in BOTH variants. It walks source text, so unlike `SpecRunner` it
// needs no simulator and does run in CI.

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
    // ---- paint: neutral surfaces ----
    // Dark's are achromatic (oklch chroma 0), sampled from the original app;
    // light's are the reference's hexes, which carry their own faint blue in
    // the last digit (`#f4f4f7`, `#e4e4e8`) — a pure grey beside white makes
    // the white read yellow, and the doc's surfaces already account for it.
    /// The reading page: the transcript, the composer behind it, sign-in, the
    /// new-session canvas. Dark's sampled #060606; light's `--card`.
    static let bg = themed(dark: grey(6), light: hex(0xFFFFFF))
    /// The page every LIST scrolls over — Home, Board, Stats. Dark walks UP
    /// from `bg` to reach it (#0d0d0d, the desktop's shell). Light does not
    /// walk anywhere: the reference gives a phone screen ONE page tone
    /// (`--card`), because there is no panel beside it to be a step away from.
    static let surface = themed(dark: grey(13), light: hex(0xFFFFFF))
    /// Raised surface: a stats bar's track, a tool card in the transcript, the
    /// plate behind a swipe action. Light TINTS DOWN from the white page
    /// (`--raised`) rather than up — up is where a track painted at half alpha
    /// over the page it would have matched goes invisible.
    static let surfaceRaised = themed(dark: neutral(0.235), light: hex(0xF4F4F7))
    /// The page a sheet presents on. Between `bg` and `surfaceRaised` in dark;
    /// `--raised` in light, so the white cards on it read as the raised object.
    /// It shares a tone with `surfaceRaised` there, the way `card` and the
    /// shell share one in the desktop's dark: two jobs, one answer.
    static let sheetPanel = themed(dark: grey(0x14), light: hex(0xF4F4F7))
    /// A grouped card ON `sheetPanel` — a translucent lift in dark, the white
    /// object (`--sel`) in light. Never darker than the bed it sits on.
    static let card = themed(dark: Color.white.opacity(0.045), light: hex(0xFFFFFF))
    /// Pressed wash for interactive rows and the fill behind a chip — the
    /// desktop's `element_hover`, which on a touch screen is what a finger
    /// holds down rather than what a pointer rests on. `--chip` in both: white
    /// at 7% over dark, black at 5% over light.
    static let elementHover = themed(dark: wash(0.07), light: Color.black.opacity(0.05))
    /// Active/selected row. Dark keeps the app's existing pressed wash; light
    /// uses the reference's exact `--selcard`, including its faint blue tint.
    static let elementActive = themed(dark: wash(0.10), light: hex(0xEEEEF2))
    /// Hairline border — `--line`. Translucent white over dark; an OPAQUE hex
    /// over light, because a hairline is the one thing that must not fade into
    /// the paper it is drawn on.
    static let border = themed(dark: Color.white.opacity(0.08), light: hex(0xE4E4E8))
    /// Stronger border for focused/raised edges — `--line2`.
    static let borderStrong = themed(dark: Color.white.opacity(0.14), light: hex(0xD6D6DC))
    /// The hairline BETWEEN rows inside a grouped card, where `border` draws
    /// the card's own edge. Quieter than `border` in dark; in light the
    /// reference has two line tones and this is the lighter of them.
    static let separator = themed(dark: Color.white.opacity(0.06), light: hex(0xE4E4E8))

    // ---- paint: text — four tones, never multiplied (gh#172) ----
    // The same four contrast steps in both variants — 16.9 / 8.4 / 5.1 / 3.5 on
    // `bg` — measured against each variant's own page rather than inverted.
    /// Headings, titles, the selected row. ~16.9:1 on `bg` — `--text`.
    static let text = themed(dark: neutral(0.938), light: hex(0x171717))
    /// Body copy and unselected rows — the default reading tone. ~8.4:1.
    static let textMuted = themed(dark: neutral(0.728), light: hex(0x545454))
    /// Labels, metadata, captions, timestamps, sublines. ~5.1:1 — still AA body
    /// text, which the `.opacity(0.5)` sublines this token replaces were not.
    static let textSubtle = themed(dark: neutral(0.598), light: hex(0x6B6B6B))
    /// Disabled controls and placeholders, and nothing else. ~3.5:1 — the
    /// floor, so anything a user is meant to READ sits at `textSubtle` or up.
    static let textFaint = themed(dark: neutral(0.508), light: hex(0x8A8A8A))

    // ---- the status ramp: four hues, one L, one C (gh#173) ----
    // The four HUES are anchored in `comet_proto::view::status`, because the
    // desktop app, the terminal app and this one paint the same meanings and
    // must land on the same hues — `crates/ui/tests/ios_theme.rs` asserts they
    // still equal the Rust ones, in both variants. The two ANCHORS differ by
    // variant, and light's are the reference's rather than the desktop's: it
    // asks for a heavier ramp on a white page than `Theme::light()` wants on
    // its cool paper, and the page is the thing that changed.
    /// The lightness every status hue is anchored to on a dark surface. One
    /// number, so "how loud is this state" is decided by the state and never by
    /// its hue.
    static let statusL: Double = 0.74
    /// The same anchor on the reference's white page: darker, so the hues keep
    /// contrast, and carrying `statusCLight` with it.
    static let statusLLight: Double = 0.52
    /// The chroma every status hue carries on a dark surface.
    static let statusC: Double = 0.14
    /// The same on light. A touch more, because a hue loses saturation to the
    /// eye as it darkens and the four have to stay as distinct from each other
    /// as they are in dark.
    static let statusCLight: Double = 0.16
    /// Blocked · failed · errored.
    static let hueBlocked: Double = 25
    /// Working — an agent is running.
    static let hueWorking: Double = 75
    /// Review · a question · links · focus.
    static let hueReview: Double = 265
    /// Settled · seen · online.
    static let hueSettled: Double = 160

    /// A status hue at whichever anchor the surface under it calls for.
    static func ramp(_ hue: Double) -> Color {
        themed(dark: oklch(statusL, statusC, hue), light: oklch(statusLLight, statusCLight, hue))
    }

    /// Accent — indigo, `Status.review`'s hue: review, a question, links,
    /// focus, selection tint.
    static let accent = ramp(hueReview)
    /// The accent hue at fill weight — off the ramp on purpose: a filled button
    /// is not a status, and a status-weight fill under white text would not
    /// hold contrast. Same hue, so the two read as one colour.
    ///
    /// The reference draws no filled accent button, so light's value is derived
    /// rather than transcribed: the same step off the ramp that dark takes
    /// (−0.12 lightness, +0.05 chroma), applied to light's anchor.
    static let accentStrong = themed(dark: oklch(0.62, 0.19, hueReview),
                                     light: oklch(0.40, 0.21, hueReview))
    /// Danger — red, `Status.blocked`'s hue: errors, the stop button.
    static let danger = ramp(hueBlocked)
    /// Warning — amber, `Status.working`'s hue: a running agent, offline
    /// notices. It sat at L 0.828 before the anchor, where it read twice as
    /// loud as the accent even when it meant less — which is why a fifth hue
    /// (pink) had been minted for "working" in the first place.
    static let warning = ramp(hueWorking)
    /// Settled — emerald, `Status.settled`'s hue: finished chats, an online
    /// device, an active account.
    static let settled = ramp(hueSettled)

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

    // ---- the three foregrounds that sit ON a fill of their own colour ----
    // Dark needs its own tones here: its ramp is anchored for a dark PAGE, and
    // ramp-weight paint on a 12% wash of itself over near-black does not hold.
    // Light does not — the reference's ramp is already anchored for the white
    // page these washes sit on, so the foreground and the ramp coincide, and a
    // fifth red invented to go on a pink chip is a tone nobody asked for. The
    // three keep their own names because the DARK halves are real paint of
    // their own.
    /// Error chips and failure notices — on `danger.opacity(0.12…0.14)`.
    static let dangerText = themed(dark: oklch(0.808, 0.114, 19.571),  // red-300
                                   light: oklch(statusLLight, statusCLight, hueBlocked))
    /// Offline notices — on `warning`'s wash. amber-200 in dark.
    static let warningText = themed(dark: oklch(0.924, 0.12, 95.746),  // amber-200
                                    light: oklch(statusLLight, statusCLight, hueWorking))
    /// Presence UI: an "Active" badge, a copied id's confirmation.
    static let settledText = themed(dark: oklch(0.88, 0.11, hueSettled),
                                    light: oklch(statusLLight, statusCLight, hueSettled))

    /// Claude brand orange — kept even on the mono surface, because it
    /// identifies a harness rather than a state, and `Status` is about what a
    /// run is doing. `--claude` in both: the brand's own #D97757 on dark, and
    /// the reference's darker mix of it on white, where #D97757 carries barely
    /// 2.6:1 against the page.
    static let claudeBrand = themed(dark: hex(0xD97757), light: hex(0xC15F3C))

    // ---- paint: markdown inline code (violet family) ----
    // The reference draws no rendered markdown, so this pair and the three
    // syntax tones below keep the desktop's light values (`markdown::render`)
    // — the only other place in this repo that has ever declared them.
    /// violet-300 on the near-black code bed; violet-600 on paper, where the
    /// pale one would vanish.
    static let inlineCodeText = themed(dark: oklch(0.811, 0.111, 293.571),  // violet-300
                                       light: oklch(0.541, 0.281, 293.009)) // violet-600
    /// violet-400, run a touch stronger over the brighter light bed.
    static let inlineCodeWash = themed(dark: oklch(0.702, 0.183, 293.541).opacity(0.12),
                                       light: oklch(0.702, 0.183, 293.541).opacity(0.14))

    // ---- paint: syntax tokens (soft, paint-only) ----
    // Dark keeps the pastels the near-black code block was tuned for; light
    // deepens each hue to its 700-shade so it keeps contrast on white.
    static let tokenKeyword = themed(dark: oklch(0.709, 0.129, 20.0),     // soft rose
                                     light: oklch(0.514, 0.222, 16.935))  // rose-700
    static let tokenString = themed(dark: oklch(0.770, 0.110, 168.0),      // soft green
                                    light: oklch(0.508, 0.118, 165.612))  // emerald-700
    static let tokenNumber = themed(dark: oklch(0.780, 0.120, 80.0),       // soft amber
                                    light: oklch(0.555, 0.163, 48.998))   // amber-700

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

// MARK: - The two variants

extension Theme {
    /// What a `whiteAlpha` overlay's alpha becomes when it is made of black
    /// instead. Set by the one value the reference declares for this job:
    /// `--chip` is white at 7% in dark and black at 5% in light, so 5/7. Every
    /// other translucent call site rides the same ratio rather than being
    /// retuned one at a time — an overlay reads heavier as ink on paper than as
    /// light on near-black, and by the same factor throughout.
    static let lightInkScale: Double = 5.0 / 7.0
}

/// One token, two values — resolved against the trait collection it is drawn
/// into, so `Theme.text` is the right grey in a light sheet and a dark list
/// without either of them asking (gh#257).
///
/// This is deliberately the ONLY way a paint token differs by scheme. The
/// alternative — a `@Environment(\.colorScheme)` read at the call site — puts
/// the design system's decisions in the views, where the two halves drift and
/// nothing can check them; `crates/ui/tests/ios_theme.rs` reads the `dark:` and
/// `light:` arguments below and holds dark to `crates/ui/src/theme.rs` and
/// light to the reference's own table.
func themed(dark: Color, light: Color) -> Color {
    Color(UIColor { traits in
        UIColor(traits.userInterfaceStyle == .light ? light : dark)
    })
}

// MARK: - Color primitives (ported from theme.rs)

/// A neutral (chroma 0) oklch tone. Chroma 0 means r == g == b exactly. The
/// DARK primitive — light's neutrals are `cool`.
func neutral(_ lightness: Double) -> Color {
    let v = Double(oklchToSrgb(l: lightness, c: 0, hDeg: 0)[0])
    return Color(red: v, green: v, blue: v)
}

/// An exact sRGB colour from a 24-bit hex literal — how the reference declares
/// every one of its light surfaces, hairlines and text tones, so this is how
/// they are transcribed.
func hex(_ rgb: UInt32) -> Color {
    Color(red: Double((rgb >> 16) & 0xFF) / 255.0,
          green: Double((rgb >> 8) & 0xFF) / 255.0,
          blue: Double(rgb & 0xFF) / 255.0)
}

/// The translucent-overlay primitive, in whichever ink the surface calls for:
/// white at low alpha over dark, black at `Theme.lightInkScale` of that alpha
/// over light — the reference's `--chip`, generalised.
///
/// Dynamic rather than dark-only because it is the one primitive the SCREENS
/// call — chips, card fills and pressed states all reach for it directly, and
/// rewriting those two dozen call sites into paired tokens would have bought
/// nothing the resolution below does not. The two places where an overlay is
/// the wrong answer in light are the two hairline tokens, which the reference
/// declares as opaque hexes and `Theme` therefore declares in full.
func whiteAlpha(_ alpha: Double) -> Color {
    themed(dark: Color.white.opacity(alpha),
           light: Color.black.opacity(alpha * Theme.lightInkScale))
}

/// Interactive-state wash: translucent soft-white rather than pure white, so a
/// press fades from the surface's own tone instead of flashing. The DARK
/// primitive — light presses are `ink` (see `Theme.elementHover`).
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
