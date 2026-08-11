// Monochrome theme, in two variants — **what each surface is FOR**, answered
// with the design canvas's own variables.
//
// The values are not here. They are in `DesignCanvas.swift`, transcribed from
// `docs/design/canvas/comet-ios.dc.html` under the canvas's own names, and
// this file is the one place that decides which canvas variable answers which
// job: `Theme.surfaceRaised` is `--raised`, one line, and a reader can check
// that mapping against `docs/design/tokens.md` without reading a colour
// literal. **Numbers drive layout, colors are paint**: the radii, type sizes
// and spacing below are plain numbers, shared with the desktop, and never
// depend on which colour is painted.
//
// # One source of truth, hand-ported once (gh#279)
//
// The phone cannot reference `crates/ui/src/theme.rs`, so the palette exists
// here a second time whether anyone likes it or not. What gh#279 changed is
// that it exists ONCE on this side too. Before it, each token carried its own
// pair of literals and four of the dark ones had been *derived* rather than
// transcribed — `neutral(0.235)` painted #202020 where the canvas draws
// #161616 for `--raised`, and `grey(6)` painted #060606 for a `--card` the
// canvas draws at #070707. That is gh#274's bug, one viewport later and one
// row down: a ramp re-derives on every read, so what ships is whatever the
// oklch round-trip produces rather than what anyone chose.
//
// `crates/ui/tests/ios_theme.rs` now reads `docs/design/tokens.md` and holds
// `DesignCanvas.swift` to it, value for value, in both variants — so the doc,
// the desktop and the phone are three declarations of one table with a test
// between them.
//
// ## Where the light numbers come from
//
// **The canvas for this phone, and not the desktop's `Theme::light()`.** The
// two disagree, and the disagreement is deliberate on the canvas's side: it
// draws phone screens, so it declares a phone's surface system rather than a
// shell-beside-a-panel one. The page is white and elevation is a TINT DOWN
// from it (`--raised`), the hairlines are opaque hexes rather than a
// translucent ink, and the status ramp sits at L 0.52 / C 0.16 rather than the
// desktop light theme's 0.55 / 0.14.
//
// The phone spends the table on nine jobs where the canvas declares four
// variables, and exactly one token takes its two halves from two different
// variables — `surface`, which says why at its declaration. Everything else
// is a whole variable under a name for its job.
//
// A handful of tokens have no counterpart in the canvas, because the screens
// it draws never show them: `accentStrong` (no filled accent button), the
// three `*Text` foregrounds, the inline-code violet and the three syntax
// tones. Each says at its declaration where its value came from instead.
//
// Every paint token is built through `themed(dark:light:)` — one `Color` that
// resolves against the trait collection it is drawn into. The consequence is
// the point: **no view asks which scheme it is in**, and **no view names a
// colour**. A screen that reached past `Theme` would be picking paint, which
// is the habit the system exists to end.
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
    // Every one of these is a canvas variable under a name that says what it is
    // FOR. `DesignCanvas` holds the values; this is the only file that decides which
    // variable answers which job, and the mapping is one line long each so a
    // reader can check it against `docs/design/tokens.md` at a glance.
    /// The reading page: the transcript, the composer behind it, sign-in, the
    /// new-session canvas. `--card`, which on a phone is the page itself.
    static let bg = DesignCanvas.card.color
    /// The page every LIST scrolls over — Home, Board, Stats.
    ///
    /// **The one token whose two halves come from different variables.** Dark
    /// walks up to `--shell`, the ground the desktop's panel floats on. Light
    /// does not walk anywhere: the canvas gives a phone screen ONE page tone,
    /// and it draws `--card` for it, because there is no panel beside it to be
    /// a step away from. Painting light's `--shell` here would put a grey band
    /// under a white list for no reason a phone can see.
    static let surface = themed(dark: DesignCanvas.shell.dark, light: DesignCanvas.card.light)
    /// Raised surface — `--raised`: a stats bar's track, a tool card in the
    /// transcript, the plate behind a swipe action, a message bubble. Light
    /// TINTS DOWN from the white page rather than up; up is where a track
    /// painted over the page it would have matched goes invisible.
    static let surfaceRaised = DesignCanvas.raised.color
    /// The page a sheet presents on — `--card`, the same page tone as `bg`,
    /// which is what the canvas's review sheet paints. It keeps its own name
    /// because it is a different job with a different neighbour: what sits ON
    /// it is `card`, and the pair has to step the right way.
    static let sheetPanel = DesignCanvas.card.color
    /// A grouped card ON `sheetPanel` — `--raised`, a step up from the page in
    /// dark and a tint down from it in light, exactly as the canvas's "Asked
    /// for" band and composer bed are drawn.
    static let card = DesignCanvas.raised.color
    /// A SELECTED row on the page — `--sel`. Light lifts it to white, dark
    /// steps it up from the card; either way it wears `rowEdge`.
    static let rowSelected = DesignCanvas.sel.color
    /// The same row inside a card — `--selcard`. Identical to `rowSelected` in
    /// dark; in light it steps DOWN, because the card is already white.
    static let elementActive = DesignCanvas.selcard.color
    /// The wash a row takes while a finger is on it — `--hover`, which on a
    /// touch screen is what is held down rather than what a pointer rests on.
    static let elementHover = DesignCanvas.hover.color
    /// The translucent bed a chip, badge or key cap sits on — `--chip`.
    ///
    /// Separate from `elementHover`, which it was folded into until gh#279.
    /// One wash cannot be both: a chip lands on the page, on `--raised` and
    /// inside a selected row, and the alpha that reads on all three is not the
    /// alpha a press wants (gh#274 made the same split on the desktop).
    static let chip = DesignCanvas.chip.color
    /// Hairline border — `--line`. Translucent white over dark; an OPAQUE hex
    /// over light, because a hairline is the one thing that must not fade into
    /// the paper it is drawn on.
    static let border = DesignCanvas.line.color
    /// Stronger border for focused/raised edges — `--line2`.
    static let borderStrong = DesignCanvas.line2.color
    /// The hairline BETWEEN rows inside a grouped card, where `border` draws
    /// the card's own edge. The canvas draws ONE hairline tone and uses it for
    /// both, so this is `--line` too — the name survives because the job does.
    static let separator = DesignCanvas.line.color
    /// The 1px ring a selected row wears — `--sellift`.
    ///
    /// Stroked INSIDE the row's own shape, never hung behind it as a shadow: a
    /// drop shadow behind a translucent fill shows through as a plate.
    static let rowEdge = DesignCanvas.sellift.color

    // ---- paint: text — four tones, never multiplied (gh#172) ----
    // The same four contrast steps in both variants — 17.0 / 8.5 / 5.2 / 3.6 on
    // `bg` — measured against each variant's own page rather than inverted.
    //
    // Both sides are TRANSCRIBED hexes. The dark four were a `neutral(L)` oklch
    // ramp until gh#274 and landed a point or three under the design's numbers
    // (`#EAEAEA` where the canvas draws `#EDEDED`); the desktop theme had the
    // same bug on the same four values, which is what a shared ramp buys you.
    /// Headings, titles, the selected row. ~17.0:1 on `bg` — `--text`.
    static let text = DesignCanvas.text.color
    /// Body copy and unselected rows — the default reading tone. ~8.5:1.
    static let textMuted = DesignCanvas.muted.color
    /// Labels, metadata, captions, timestamps, sublines. ~5.2:1 — still AA body
    /// text, which the `.opacity(0.5)` sublines this token replaces were not.
    static let textSubtle = DesignCanvas.subtle.color
    /// Disabled controls and placeholders, and nothing else. ~3.6:1 — the
    /// floor, so anything a user is meant to READ sits at `textSubtle` or up.
    static let textFaint = DesignCanvas.faint.color

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
    static let statusL: Double = DesignCanvas.statusL
    /// The same anchor on the canvas's white page: darker, so the hues keep
    /// contrast, and carrying `statusCLight` with it.
    static let statusLLight: Double = DesignCanvas.statusLLight
    /// The chroma every status hue carries on a dark surface.
    static let statusC: Double = DesignCanvas.statusC
    /// The same on light. A touch more, because a hue loses saturation to the
    /// eye as it darkens and the four have to stay as distinct from each other
    /// as they are in dark.
    static let statusCLight: Double = DesignCanvas.statusCLight
    /// Blocked · failed · errored.
    static let hueBlocked: Double = DesignCanvas.hueBlocked
    /// Working — an agent is running.
    static let hueWorking: Double = DesignCanvas.hueWorking
    /// Review · a question · links · focus.
    static let hueReview: Double = DesignCanvas.hueReview
    /// Settled · seen · online.
    static let hueSettled: Double = DesignCanvas.hueSettled

    /// A status hue at whichever anchor the surface under it calls for.
    static func ramp(_ hue: Double) -> Color {
        DesignCanvas.ramp(hue).color
    }

    // ---- how much of a status hue a tint carries ----
    // The canvas mixes its status hues into transparency at four named
    // strengths (`color-mix(in srgb, var(--x) N%, transparent)`). Those
    // percentages are part of the spec, so they are named once here rather
    // than typed at each of the two dozen call sites that spend them.
    /// A chip's fill — `12%`.
    static let statusChipTint: Double = DesignCanvas.statusChipTint
    /// A count pill's and a badge's fill — `14%`.
    static let statusBadgeTint: Double = DesignCanvas.statusBadgeTint
    /// A warning card's border — `32%`.
    static let statusBorderTint: Double = DesignCanvas.statusBorderTint
    /// A warning card's header bed — `9%`.
    static let statusHeaderTint: Double = DesignCanvas.statusHeaderTint
    /// The divider under that header — `22%`.
    static let statusDividerTint: Double = DesignCanvas.statusDividerTint

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
    static let claudeBrand = DesignCanvas.claude.color

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

// MARK: - Selection, as the canvas draws it

extension View {
    /// A canvas shadow token, painted. Dark declares `none` for all three, so
    /// its ink is fully transparent and this draws nothing — which is the
    /// declaration, not a special case.
    func canvasShadow(_ shadow: CanvasShadow) -> some View {
        self.shadow(color: shadow.color, radius: shadow.radius, x: 0, y: shadow.y)
    }

    /// A selected row, as `--sel` + `--sellift`.
    ///
    /// The whole of the canvas's selection idiom in one modifier, so no screen
    /// gets to spell it a fourth way: the row takes `rowSelected`, and the ring
    /// is STROKED INSIDE its own shape rather than hung behind it — a drop
    /// shadow behind a translucent fill shows through as a plate. Light carries
    /// `--sellift`'s second layer as a real shadow beneath.
    ///
    /// `card: true` swaps the fill for `--selcard`, which is the same tone in
    /// dark and a step DOWN in light, for a row selected inside a card rather
    /// than on the page.
    func selectedRow(_ selected: Bool,
                     cornerRadius: CGFloat = Theme.radiusCard,
                     card: Bool = false) -> some View {
        let shape = RoundedRectangle(cornerRadius: cornerRadius)
        return self
            .background(selected ? (card ? Theme.elementActive : Theme.rowSelected) : .clear,
                        in: shape)
            .overlay(shape.strokeBorder(selected ? Theme.rowEdge : .clear, lineWidth: 1))
            .canvasShadow(selected ? DesignCanvas.selliftShadow
                                   : DesignCanvas.noShadow)
    }
}

/// One token, two values — resolved against the trait collection it is drawn
/// into, so `Theme.text` is the right grey in a light sheet and a dark list
/// without either of them asking (gh#257).
///
/// This is deliberately the ONLY way a paint token differs by scheme. The
/// alternative — a `@Environment(\.colorScheme)` read at the call site — puts
/// the design system's decisions in the views, where the two halves drift and
/// nothing can check them; `crates/ui/tests/ios_theme.rs` reads the `dark:` and
/// `light:` arguments of every token and holds dark to `crates/ui/src/theme.rs`
/// and the whole canvas table to `docs/design/tokens.md`.
func themed(dark: Color, light: Color) -> Color {
    Color(UIColor { traits in
        UIColor(traits.userInterfaceStyle == .light ? light : dark)
    })
}

// MARK: - Colour primitives
//
// The three notations the canvas writes its variables in, and nothing else.
// They exist so `DesignCanvas.swift` can transcribe rather than convert: a
// `#161616` in the canvas is `hex(0x161616)` here, an `rgba(255,255,255,.08)`
// is `Color.white.opacity(0.08)`, an `oklch(0.74 0.14 25)` is `oklch(...)`.
//
// gh#279 removed the two that were CONVERSIONS — `neutral(L)`, which derived a
// grey from a lightness and landed near the canvas's number instead of on it,
// and `whiteAlpha(a)`, the generalised overlay that let forty call sites each
// invent their own bed. What replaced them is `DesignCanvas` plus the named
// `Theme` tokens that spend it.

/// An exact sRGB colour from a 24-bit hex literal — how the canvas declares
/// every surface, hairline and text tone it does not write in `oklch`.
func hex(_ rgb: UInt32) -> Color {
    Color(red: Double((rgb >> 16) & 0xFF) / 255.0,
          green: Double((rgb >> 8) & 0xFF) / 255.0,
          blue: Double(rgb & 0xFF) / 255.0)
}

/// Interactive-state wash: translucent soft-white rather than pure white, so a
/// press fades from the surface's own tone instead of flashing. `--hover`'s
/// deliberate deviation, and the only place it is spent — see
/// `DesignCanvas.hover`.
func wash(_ alpha: Double) -> Color {
    Color(red: 0.92, green: 0.92, blue: 0.92).opacity(alpha)
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
