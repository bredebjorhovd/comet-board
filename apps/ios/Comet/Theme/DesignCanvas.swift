// The design canvas's variables, in Swift (gh#279).
//
// `docs/design/canvas/comet-ios.dc.html` declares a `.cw` block and a
// `.cw[data-theme="light"]` block, and between them they are the whole palette
// this app paints. `docs/design/tokens.md` is the table both of them and
// `crates/ui/src/theme.rs` are reconciled against (gh#274). This file is that
// table's third and last declaration.
//
// # Why a separate type, and why transcribed
//
// The phone cannot reference `crates/ui/src/theme.rs` — no Rust runs on the
// device — so the values have to be typed in by hand a second time. The one
// thing that keeps a hand-port honest is that **the ported values are named,
// in the canvas's own names, in one place**: `--raised` is `DesignCanvas.raised`
// here and nowhere else, so `crates/ui/tests/ios_theme.rs` can read this file
// against `docs/design/tokens.md` line for line and a drift is a failing test
// rather than a screenshot somebody eventually squints at.
//
// The failure this replaces is gh#274's, one viewport later. The phone's dark
// surfaces were *derived* — `neutral(0.235)` for `--raised`, `grey(6)` for
// `--card` — and derivation lands near the number rather than on it:
// `neutral(0.235)` paints #202020 where the canvas draws #161616, which is a
// tone and a half of elevation nobody chose. Every value below is
// TRANSCRIBED from the canvas block that declares it, in the same notation
// the canvas uses (a hex is `hex()`, an `rgba()` is an alpha over white or
// black, an `oklch()` is `oklch()`), so reading one against the other is a
// character comparison and not a colour-space argument.
//
// # What this type is NOT
//
// It is not the app's vocabulary. **No view names a `DesignCanvas` token** — views
// paint `Theme.surfaceRaised`, which says what the surface is FOR, and `Theme`
// is the one place that says which canvas variable answers it. Two reasons:
// the canvas declares four variables the phone spends on nine jobs, and three
// of the phone's jobs (a sheet's ground, a chat bubble, a swipe plate) have no
// canvas variable at all and are composed from these. A screen that reached
// past `Theme` would be picking paint, which is the habit the whole system
// exists to end.
//
// Nothing here is a layout number. Radii, type sizes and spacing live in
// `Theme` and are shared with the desktop; the canvas's own pixel geometry is
// written down as claims in `docs/design/ios.md` instead.

import SwiftUI

/// One canvas variable: what it declares in dark, and what it declares in
/// light.
///
/// Held as a PAIR rather than as a resolved `Color` because the phone spends
/// several of them half at a time — `Theme.surface` is `--shell`'s dark and
/// `--card`'s light, which is the one place the phone's surface system
/// deliberately parts from the desktop's (see `Theme.surface`). A resolved
/// colour could not be taken apart again.
struct CanvasToken {
    /// The value the `.cw` block declares.
    let dark: Color
    /// The value the `.cw[data-theme="light"]` block declares.
    let light: Color

    /// The variable as paint, resolving against whatever it is drawn into —
    /// the common case, where the phone spends both halves of one token.
    var color: Color { themed(dark: dark, light: light) }
}

/// A canvas shadow variable: the ink it paints, and the blur and offset it
/// paints it at.
///
/// `--lift` and `--cardshadow` are `none` in dark — dark lifts with tone and a
/// ring, light lifts with shadow — so their dark ink is fully transparent and
/// the geometry never draws. That is the declaration, not an omission.
struct CanvasShadow {
    let dark: Color
    let light: Color
    /// SwiftUI's shadow radius: half the CSS blur, which is what makes the two
    /// read the same (`0 1px 2px` is `radius: 1, y: 1`).
    let radius: CGFloat
    let y: CGFloat

    var color: Color { themed(dark: dark, light: light) }
}

/// Every variable the canvas declares, under the canvas's own name.
enum DesignCanvas {
    // ---- Surfaces ----

    /// `--card` — the main panel, and on a phone the page itself.
    static let card = CanvasToken(dark: hex(0x070707), light: hex(0xFFFFFF))
    /// `--shell` — the window ground the desktop's panel floats on. A phone has
    /// no window, so only the dark half is spent (see `Theme.surface`), but the
    /// variable is the table's and is transcribed whole.
    static let shell = CanvasToken(dark: hex(0x0D0D0D), light: hex(0xE9E9EC))
    /// `--raised` — an inner panel ON the card: a band, a grouped card, a
    /// track, the composer's bed.
    static let raised = CanvasToken(dark: hex(0x161616), light: hex(0xF4F4F7))
    /// `--sel` — a selected row on the shell. Light lifts it to white; dark
    /// steps it up from the card.
    static let sel = CanvasToken(dark: hex(0x191919), light: hex(0xFFFFFF))
    /// `--selcard` — the same row inside a card. Identical to `--sel` in dark;
    /// in light it steps DOWN, because the card it sits in is already white.
    static let selcard = CanvasToken(dark: hex(0x191919), light: hex(0xEEEEF2))
    /// `--hover` — the wash a touched row takes.
    ///
    /// Dark is the palette's one deliberate deviation (`tokens.md`): the canvas
    /// draws pure white at 5%, the apps paint soft-white (L 0.92) at the same
    /// alpha, because a wash resting on pure white flashed dark mid-fade over
    /// the desktop's glass sidebar. Same alpha, same neutral, one step off
    /// pure — carried here so the two viewports deviate identically, and
    /// asserted so it stays a decision.
    static let hover = CanvasToken(dark: wash(0.05), light: Color.black.opacity(0.025))
    /// `--chip` — the translucent bed a chip, badge or key cap sits on. The one
    /// variable that must NOT be opaque: it composites over the card, over
    /// `--raised` and inside a selected row, three beds where a single opaque
    /// tone is right on at most one (gh#274).
    static let chip = CanvasToken(dark: Color.white.opacity(0.07), light: Color.black.opacity(0.05))
    /// `--line` — the hairline. Translucent over dark; an OPAQUE hex over
    /// light, because a hairline is the one thing that must not fade into the
    /// paper it is drawn on.
    static let line = CanvasToken(dark: Color.white.opacity(0.08), light: hex(0xE4E4E8))
    /// `--line2` — the stronger hairline: a focused edge, a raised edge, the
    /// sheet grabber.
    static let line2 = CanvasToken(dark: Color.white.opacity(0.13), light: hex(0xD6D6DC))

    // ---- Text: four tones, never multiplied ----

    /// `--text` — headings, titles, the selected row.
    static let text = CanvasToken(dark: hex(0xEDEDED), light: hex(0x171717))
    /// `--muted` — body copy and unselected rows.
    static let muted = CanvasToken(dark: hex(0xA8A8A8), light: hex(0x545454))
    /// `--subtle` — labels, metadata, captions, timestamps, sublines.
    static let subtle = CanvasToken(dark: hex(0x808080), light: hex(0x6B6B6B))
    /// `--faint` — disabled controls, placeholders, an idle dot. The floor.
    static let faint = CanvasToken(dark: hex(0x666666), light: hex(0x8A8A8A))

    // ---- The status ramp: one lightness, one chroma, four hues ----

    /// The lightness every status hue is anchored to on a dark surface.
    static let statusL: Double = 0.74
    /// The chroma every status hue carries on a dark surface.
    static let statusC: Double = 0.14
    /// The same anchor on the white page: darker, so the hues keep contrast.
    static let statusLLight: Double = 0.52
    /// A touch more chroma, because a hue loses saturation to the eye as it
    /// darkens and the four have to stay as distinct as they are in dark.
    static let statusCLight: Double = 0.16

    /// Blocked · failed · errored.
    static let hueBlocked: Double = 25
    /// Working — an agent is running.
    static let hueWorking: Double = 75
    /// Review · a question · links · focus.
    static let hueReview: Double = 265
    /// Settled · seen · online.
    static let hueSettled: Double = 160

    /// A status hue at whichever anchor the surface under it calls for — the
    /// canvas's `oklch(L C H)`, both blocks at once.
    static func ramp(_ hue: Double) -> CanvasToken {
        CanvasToken(dark: oklch(statusL, statusC, hue),
                    light: oklch(statusLLight, statusCLight, hue))
    }

    /// `--blocked`.
    static let blocked = ramp(hueBlocked)
    /// `--working`.
    static let working = ramp(hueWorking)
    /// `--review`.
    static let review = ramp(hueReview)
    /// `--settled`.
    static let settled = ramp(hueSettled)

    /// The fraction of a status hue a chip's fill carries
    /// (`color-mix(in srgb, var(--x) 12%, transparent)`). Part of the spec, so
    /// it is named rather than typed at each of the two dozen call sites.
    static let statusChipTint: Double = 0.12
    /// The same mix at badge weight — the canvas's `14%`, which is what a count
    /// pill and a "new" badge carry.
    static let statusBadgeTint: Double = 0.14
    /// The mix a warning's BORDER carries (`32%`), and the two the unaccounted
    /// -changes card is built from: a `9%` header bed under a `22%` divider.
    static let statusBorderTint: Double = 0.32
    static let statusHeaderTint: Double = 0.09
    static let statusDividerTint: Double = 0.22

    // ---- Shadow and lift ----

    /// `--sellift`'s ring — the 1px edge a selected row wears. Dark's is the
    /// whole of the token; light's carries a drop shadow beside it.
    ///
    /// Painted as an INSET ring (a stroked border inside the row's own shape),
    /// never as a drop shadow: a shadow behind a translucent fill shows through
    /// as a plate.
    static let sellift = CanvasToken(dark: Color.white.opacity(0.13), light: hex(0xDCDCE2))
    /// `--sellift`'s second layer, light only: `0 1px 2px rgba(0,0,0,.06)`.
    static let selliftShadow = CanvasShadow(dark: Color.black.opacity(0),
                                            light: Color.black.opacity(0.06),
                                            radius: 1, y: 1)
    /// `none` — what dark declares for all three drop shadows, and what an
    /// unselected row carries in either variant. Named so "no shadow" is a
    /// value the canvas states rather than a branch a view takes.
    static let noShadow = CanvasShadow(dark: Color.black.opacity(0),
                                       light: Color.black.opacity(0),
                                       radius: 0, y: 0)
    /// `--lift` — `none` in dark, `0 1px 2px rgba(0,0,0,.05)` in light. Dark
    /// lifts with tone; light lifts with shadow.
    static let lift = CanvasShadow(dark: Color.black.opacity(0),
                                   light: Color.black.opacity(0.05),
                                   radius: 1, y: 1)
    /// `--cardshadow`'s key layer — `0 1px 2px rgba(0,0,0,.04)` in light.
    static let cardshadow = CanvasShadow(dark: Color.black.opacity(0),
                                         light: Color.black.opacity(0.04),
                                         radius: 1, y: 1)
    /// `--cardshadow`'s ambient layer — `0 10px 30px -18px rgba(0,0,0,.18)`.
    /// The negative spread has no SwiftUI equivalent; what survives the
    /// translation is a soft shadow well below the card, which is the job.
    static let cardshadowAmbient = CanvasShadow(dark: Color.black.opacity(0),
                                                light: Color.black.opacity(0.18),
                                                radius: 15, y: 10)

    // ---- Borrowed marks ----

    /// `--claude` — Anthropic's orange. Outside the status ramp on purpose: it
    /// identifies a vendor, and the four hues mean state. Nothing but the
    /// Claude mark paints with it.
    static let claude = CanvasToken(dark: hex(0xD97757), light: hex(0xC15F3C))

    // `--desk` is deliberately absent. It is the fake desktop the canvas draws
    // BEHIND the window so the corner radius and shadow read; `tokens.md` says
    // in as many words that it has no counterpart in the app and must not
    // acquire one. A phone has no desktop at all.
}
