//! Monochrome theme — concrete values, no indirection.
//!
//! Colors are precomputed from an oklch-derived neutral scale (perceptually even
//! lightness steps; the same scale comet's Tailwind theme used) into gpui [`Hsla`].
//! **Numbers drive layout, colors are paint**: layout constants live here as plain
//! numbers and never depend on which color is painted.
//!
//! Two variants, [`Theme::dark`] (the default — comet is always-dark by design)
//! and [`Theme::light`] (the same ramp inverted: text goes dark, surfaces light,
//! accents re-tuned for contrast on white). The chosen variant is installed as a
//! gpui [`Global`] at boot (`cx.set_global(Theme::dark())`) and can be flipped
//! live from the Appearance settings page; read with [`Theme::of`]. Hairlines and
//! interactive washes are white at low alpha in dark and black at low alpha in
//! light — see [`Theme::white_alpha`] / [`Theme::wash`].

use gpui::{App, Global, Hsla, SharedString, hsla};
use serde::{Deserialize, Serialize};

/// Which theme variant the app paints with. Persisted in [`crate::settings::UiSettings`]
/// (`theme` key) and overridable per-run with `COMET_THEME=dark|light`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// The default — comet's original always-dark monochrome.
    #[default]
    Dark,
    /// The inverted ramp: light surfaces, dark text, accents tuned for white.
    Light,
}

impl ThemeChoice {
    /// Every variant, in nav order (dark first — the default).
    pub const ALL: [ThemeChoice; 2] = [ThemeChoice::Dark, ThemeChoice::Light];

    /// `COMET_THEME=dark|light`; `None` when unset (callers fall back to the
    /// persisted setting).
    pub fn from_env() -> Option<Self> {
        match std::env::var("COMET_THEME").ok().as_deref() {
            Some("light") => Some(Self::Light),
            Some("dark") => Some(Self::Dark),
            _ => None,
        }
    }

    /// Row label + purpose copy for the Appearance page.
    pub fn describe(self) -> (&'static str, &'static str) {
        match self {
            ThemeChoice::Dark => (
                "Dark",
                "Comet's original palette — near-black surfaces, monochrome text.",
            ),
            ThemeChoice::Light => (
                "Light",
                "The same ramp inverted for bright environments — light surfaces, dark text.",
            ),
        }
    }
}

impl std::fmt::Display for ThemeChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ThemeChoice::Dark => "Dark",
            ThemeChoice::Light => "Light",
        })
    }
}

/// The app theme. One concrete instance per variant — dark by default, light
/// opt-in (see [`ThemeChoice`]).
#[derive(Debug, Clone)]
pub struct Theme {
    /// True for the light variant. Picks the wash/hairline/glass direction
    /// everywhere paint is generated from a primitive ([`Self::wash`],
    /// [`Self::white_alpha`], [`Self::glass`]).
    pub light: bool,
    // ---- paint: neutral surfaces (oklch chroma 0) ----
    /// App background — oklch(0.145 0 0) ≡ `#0a0a0a` (dark).
    pub bg: Hsla,
    /// Panel / sidebar surface — one scale step up.
    pub surface: Hsla,
    /// Raised surface: popovers, dialogs, cards.
    pub surface_raised: Hsla,
    /// Hover wash for interactive rows/buttons (white, low alpha).
    pub element_hover: Hsla,
    /// Active/selected wash (white, slightly higher alpha).
    pub element_active: Hsla,
    /// Hairline border — white at low alpha.
    pub border: Hsla,
    /// Stronger border for focused/raised edges.
    pub border_strong: Hsla,

    // ---- paint: text ----
    /// Primary text.
    pub text: Hsla,
    /// Muted text: timestamps, secondary labels.
    pub text_muted: Hsla,
    /// Faint text: placeholders, disabled.
    pub text_faint: Hsla,

    // ---- paint: accents ----
    /// Accent — indigo (working indicator, links, selection tint).
    pub accent: Hsla,
    /// Stronger accent for fills.
    pub accent_strong: Hsla,
    /// Danger — red (errors, stop button).
    pub danger: Hsla,
    /// Warning — amber (offline notices, awaiting-input).
    pub warning: Hsla,

    // ---- fonts ----
    /// UI font family (bundling of Geist lands with asset work; until then the
    /// text system falls back to the system sans when the family is missing).
    pub font_sans: SharedString,
    /// Monospace family for code/terminal.
    pub font_mono: SharedString,
    /// Explicit system fallbacks, for callers that want to skip the lookup.
    pub font_sans_fallback: SharedString,
    pub font_mono_fallback: SharedString,
}

impl Theme {
    // ---- numbers drive layout (px) ----
    /// Frost translucency over the blurred window background (macOS vibrancy).
    /// Opaque elsewhere: Linux/Windows get no compositor-blur guarantee, and a
    /// merely transparent window would show raw desktop through the sidebar.
    /// Darkness matched by eye to a reference Electron app's dark glass. That
    /// scrim is 0.76 over `hsl(0 0% 3%)`, but it sits on Electron's
    /// `under-window` vibrancy MATERIAL, which pre-darkens the blur; our bare
    /// backdrop blur has no material layer, so the scrim runs heavier to land
    /// on the same perceived tone (see [`Theme::glass`]).
    pub const GLASS_ALPHA: f32 = if cfg!(target_os = "macos") { 0.90 } else { 1.0 };
    /// Main-panel header height (comet `h-11`) — in-card headers (changes pane).
    pub const HEADER_HEIGHT: f32 = 44.0;
    /// The unified window titlebar (traffic lights + cluster + tabs). Content
    /// rides [`Self::TITLEBAR_TOP_PAD`] lower than center so the air above
    /// matches the perceived gap to the inset card below (border + card body).
    pub const TITLEBAR_HEIGHT: f32 = 38.0;
    /// Downward shift of titlebar content within the bar.
    pub const TITLEBAR_TOP_PAD: f32 = 2.0;
    /// Reserved status strip under the content outlet (comet `h-6`) — the
    /// WorkingIndicator row; reserving it keeps the composer from shifting.
    pub const STATUS_STRIP_HEIGHT: f32 = 24.0;
    /// Message bubble corner radius.
    pub const BUBBLE_RADIUS: f32 = 16.0;
    /// Panel / card corner radius.
    pub const PANEL_RADIUS: f32 = 10.0;
    /// Small control radius (buttons, chips).
    pub const CONTROL_RADIUS: f32 = 6.0;
    /// Base spacing steps.
    pub const SPACE_XS: f32 = 4.0;
    pub const SPACE_SM: f32 = 8.0;
    pub const SPACE_MD: f32 = 12.0;
    pub const SPACE_LG: f32 = 16.0;

    /// The frost tint painted over the blurred window background (macOS
    /// glass). Darker than `surface` — matched to the reference dark
    /// vibrancy scrim: `hsl(0 0% 3%)` (#080808) at [`Self::GLASS_ALPHA`].
    /// On light the scrim inverts: white at a lighter alpha, so the blur
    /// reads as a bright frosted panel rather than a dark plate. On opaque
    /// platforms this IS the surface tone (no tint swap).
    pub fn glass(&self) -> Hsla {
        if Self::GLASS_ALPHA < 1.0 {
            if self.light {
                white_alpha(0.80)
            } else {
                grey(8).opacity(Self::GLASS_ALPHA)
            }
        } else {
            self.surface
        }
    }

    /// Build the dark theme — comet's original. The surface tones are sampled
    /// straight from the reference screenshots of the original app
    /// (docs/reference): main panel `#060606`, shell/sidebar `#0d0d0d`.
    pub fn dark() -> Self {
        Self {
            light: false,
            bg: grey(6),      // main panel — sampled #060606
            surface: grey(13), // shell / sidebar — sampled #0d0d0d
            surface_raised: neutral(0.235),
            element_hover: wash(0.14),
            element_active: wash(0.16),
            border: white_alpha(0.08),
            border_strong: white_alpha(0.14),
            text: neutral(0.922),                        // ~neutral-200
            text_muted: neutral(0.708),                  // ~neutral-400
            text_faint: neutral(0.556),                  // ~neutral-500
            accent: oklch(0.673, 0.182, 276.935),        // indigo-400
            accent_strong: oklch(0.585, 0.233, 277.117), // indigo-500
            danger: oklch(0.704, 0.191, 22.216),         // red-400
            warning: oklch(0.828, 0.189, 84.429),        // amber-400
            font_sans: "Geist".into(),
            font_mono: "Geist Mono".into(),
            font_sans_fallback: system_sans().into(),
            font_mono_fallback: system_mono().into(),
        }
    }

    /// Build the light theme — the same ramp inverted: surfaces walk DOWN from
    /// a near-white app background, text walks up to near-black, and the
    /// accents move from the 400-shades to the 600-shades so they keep contrast
    /// on white. The default remains dark; this is the opt-in variant.
    pub fn light() -> Self {
        Self {
            light: true,
            bg: neutral(0.985),         // main panel — near-white
            surface: neutral(0.965),    // shell / sidebar — one step down
            surface_raised: neutral(0.92),
            element_hover: black_wash(0.08),
            element_active: black_wash(0.10),
            border: black_wash(0.10),
            border_strong: black_wash(0.16),
            text: neutral(0.22),      // ~neutral-900
            text_muted: neutral(0.48), // ~neutral-600
            text_faint: neutral(0.66), // ~neutral-400
            accent: oklch(0.511, 0.262, 276.966), // indigo, darkened for white
            accent_strong: oklch(0.457, 0.24, 277.023),
            danger: oklch(0.60, 0.22, 26.0),
            warning: oklch(0.70, 0.16, 76.0),
            font_sans: "Geist".into(),
            font_mono: "Geist Mono".into(),
            font_sans_fallback: system_sans().into(),
            font_mono_fallback: system_mono().into(),
        }
    }

    /// The variant for a [`ThemeChoice`] — the bridge between the persisted
    /// preference and the painted theme.
    pub fn for_choice(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::Dark => Self::dark(),
            ThemeChoice::Light => Self::light(),
        }
    }

    /// Which choice this theme corresponds to (so the Appearance page can show
    /// the active variant without carrying state).
    pub fn choice(&self) -> ThemeChoice {
        if self.light {
            ThemeChoice::Light
        } else {
            ThemeChoice::Dark
        }
    }

    /// Interactive-state wash for THIS theme's surface tone: soft-white over
    /// dark, soft-black over light. Hover fades must rest on `wash(0.0)`,
    /// never transparent black, so the interpolation stays theme-toned.
    pub fn wash(&self, alpha: f32) -> Hsla {
        if self.light {
            black_wash(alpha)
        } else {
            wash(alpha)
        }
    }

    /// The hairline/wash primitive for this theme: white at low alpha over dark
    /// surfaces, black at low alpha over light ones (a white hairline on white
    /// would vanish).
    pub fn white_alpha(&self, alpha: f32) -> Hsla {
        if self.light {
            black_wash(alpha)
        } else {
            white_alpha(alpha)
        }
    }

    /// Selected-state glass treatment for this theme (tabs, session rows, space
    /// rows): a TRANSLUCENT wash the vibrancy reads through — heavier flat
    /// washes blocked the glass (user request).
    pub fn glass_selected_bg(&self) -> Hsla {
        self.wash(0.14)
    }

    /// The selected chip's bright outline, as an INSET shadow — see the dark
    /// variant's notes; direction flips with [`Self::white_alpha`].
    pub fn glass_selected_shadows(&self) -> Vec<gpui::BoxShadow> {
        vec![gpui::BoxShadow {
            color: self.white_alpha(0.09),
            offset: gpui::point(gpui::px(0.0), gpui::px(0.0)),
            blur_radius: gpui::px(0.0),
            spread_radius: gpui::px(1.0),
            inset: true,
        }]
    }

    /// The danger text/icon tone for THIS theme's error UI (error chips,
    /// failure notices): dark uses the light red-300, readable on the
    /// near-black chip; light uses the darker red-700 so it keeps contrast on
    /// white. Borders/washes stay on [`Self::danger`]; only the foreground
    /// inverts (a light tone on a light wash would wash out).
    pub fn danger_text(&self) -> Hsla {
        if self.light {
            oklch(0.505, 0.213, 27.518) // red-700
        } else {
            oklch(0.808, 0.114, 19.571) // red-300
        }
    }

    /// The warning text/icon tone for THIS theme's notice UI (offline notices):
    /// amber-200 in dark, the darker amber-700 in light — see [`Self::danger_text`].
    pub fn warning_text(&self) -> Hsla {
        if self.light {
            oklch(0.555, 0.163, 48.998) // amber-700
        } else {
            oklch(0.924, 0.12, 95.746) // amber-200
        }
    }

    /// The floating card tone (popovers, dialogs, palettes): over the frosted
    /// backdrop blur on macOS the card is a translucent tint the vibrancy reads
    /// through; elsewhere it is the opaque tone it composites to. Dark uses the
    /// near-black `#161616` plate; light inverts to a bright translucent white.
    pub fn float_card(&self) -> Hsla {
        if Self::GLASS_ALPHA < 1.0 {
            if self.light {
                white_alpha(0.72)
            } else {
                grey(0x16).opacity(0.65)
            }
        } else if self.light {
            self.surface_raised
        } else {
            grey(0x16)
        }
    }

    /// Read the theme global.
    pub fn of(cx: &App) -> &Theme {
        cx.global::<Theme>()
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Global for Theme {}

fn system_sans() -> &'static str {
    if cfg!(target_os = "macos") {
        "Helvetica"
    } else if cfg!(target_os = "windows") {
        "Segoe UI"
    } else {
        "DejaVu Sans"
    }
}

fn system_mono() -> &'static str {
    if cfg!(target_os = "macos") {
        "Menlo"
    } else if cfg!(target_os = "windows") {
        "Consolas"
    } else {
        "DejaVu Sans Mono"
    }
}

/// A neutral (chroma 0) oklch tone as Hsla. Chroma 0 means r == g == b exactly,
/// so this goes straight to an achromatic Hsla (skipping the hue math avoids
/// float-noise saturation).
pub fn neutral(lightness: f32) -> Hsla {
    let [v, _, _] = oklch_to_srgb(lightness, 0.0, 0.0);
    hsla(0.0, 0.0, v, 1.0)
}

/// Interactive-state wash: TRANSLUCENT soft-white, with alphas high enough to
/// stay visible at the brightest backdrop the 0.90 glass scrim can produce
/// (~L 0.13 over pure white — a 12% wash still adds ~+24 luma there). Fully
/// opaque washes killed the glass and flashed dark mid-fade (user reports);
/// hover fades must rest on `wash(0.0)`, never transparent BLACK, so the
/// interpolation stays white-toned. The DARK-theme primitive — callers that
/// also serve light mode use [`Theme::wash`] instead.
pub fn wash(alpha: f32) -> Hsla {
    hsla(0.0, 0.0, 0.92, alpha)
}

/// The light-theme inverse of [`wash`]: soft-black, so hover/selected washes
/// darken a near-white surface instead of vanishing.
fn black_wash(alpha: f32) -> Hsla {
    hsla(0.0, 0.0, 0.0, alpha)
}

/// White at the given alpha — the dark-theme hairline/wash primitive (light
/// mode uses [`Theme::white_alpha`]).
pub fn white_alpha(alpha: f32) -> Hsla {
    hsla(0.0, 0.0, 1.0, alpha)
}

/// Selected-state glass treatment (tabs, session rows, space rows): a
/// TRANSLUCENT wash the vibrancy reads through — heavier flat washes blocked
/// the glass (user request). DARK-theme primitive; light mode uses
/// [`Theme::glass_selected_bg`].
pub fn glass_selected_bg() -> Hsla {
    wash(0.14)
}

/// The selected chip's bright outline, as an INSET shadow: gpui paints inset
/// shadows ON TOP of the background, edges only — a border with zero layout
/// cost. Drop shadows are filled rects painted BEHIND the element, and behind
/// a 5% fill they showed straight through as an opaque dark plate with a
/// greyed ring (user report) — nothing may paint behind a glass chip.
/// DARK-theme primitive; light mode uses [`Theme::glass_selected_shadows`].
pub fn glass_selected_shadows() -> Vec<gpui::BoxShadow> {
    vec![gpui::BoxShadow {
        color: white_alpha(0.09),
        offset: gpui::point(gpui::px(0.0), gpui::px(0.0)),
        blur_radius: gpui::px(0.0),
        spread_radius: gpui::px(1.0),
        inset: true,
    }]
}

/// An exact achromatic tone from an 8-bit channel value (`grey(13)` ≡ `#0d0d0d`)
/// — for surfaces matched against reference-screenshot samples.
pub fn grey(value: u8) -> Hsla {
    hsla(0.0, 0.0, value as f32 / 255.0, 1.0)
}

/// Convert an oklch color (CSS notation: L 0..1, C, H in degrees) to gpui Hsla.
pub fn oklch(l: f32, c: f32, h_deg: f32) -> Hsla {
    let [r, g, b] = oklch_to_srgb(l, c, h_deg);
    let (h, s, l) = rgb_to_hsl(r, g, b);
    hsla(h, s, l, 1.0)
}

/// oklch → sRGB (each 0..1, clamped/gamut-clipped per channel).
/// Reference: Björn Ottosson's OKLab definition (the same matrices CSS Color 4 uses).
pub(crate) fn oklch_to_srgb(l: f32, c: f32, h_deg: f32) -> [f32; 3] {
    let h = h_deg.to_radians();
    let a = c * h.cos();
    let b = c * h.sin();

    // OKLab → LMS (cube roots undone)
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let (l3, m3, s3) = (l_ * l_ * l_, m_ * m_ * m_, s_ * s_ * s_);

    // LMS → linear sRGB
    let r = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_93 * s3;
    let g = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_4 * s3;
    let b = -0.004_196_086_3 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;

    [gamma_encode(r), gamma_encode(g), gamma_encode(b)]
}

fn gamma_encode(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB (0..1 components) → HSL, all components 0..1 (gpui's Hsla convention).
pub(crate) fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let delta = max - min;
    if delta < f32::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = if l > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let h = if (max - r).abs() < f32::EPSILON {
        ((g - b) / delta).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    } / 6.0;
    (h, s, l)
}

/// Linear per-component mix of two colors (paint helper for the gradient spinner).
pub fn mix(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: f32, y: f32| x + (y - x) * t;
    // Mix through hue naively — both spinner endpoints sit close enough on the
    // wheel that shortest-arc handling isn't needed for our palette.
    hsla(
        lerp(a.h, b.h),
        lerp(a.s, b.s),
        lerp(a.l, b.l),
        lerp(a.a, b.a),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srgb_u8(c: [f32; 3]) -> [u8; 3] {
        [
            (c[0] * 255.0).round() as u8,
            (c[1] * 255.0).round() as u8,
            (c[2] * 255.0).round() as u8,
        ]
    }

    #[test]
    fn neutral_950_is_0a0a0a() {
        // oklch(0.145 0 0) is Tailwind neutral-950, comet's app background.
        let rgb = srgb_u8(oklch_to_srgb(0.145, 0.0, 0.0));
        assert_eq!(rgb, [10, 10, 10]);
    }

    #[test]
    fn oklch_accents_match_reference() {
        // Reference values computed independently (CSS Color 4 matrices).
        assert_eq!(
            srgb_u8(oklch_to_srgb(0.673, 0.182, 276.935)),
            [124, 134, 255]
        ); // indigo-400
        assert_eq!(
            srgb_u8(oklch_to_srgb(0.704, 0.191, 22.216)),
            [255, 100, 103]
        ); // red-400
        assert_eq!(srgb_u8(oklch_to_srgb(0.828, 0.189, 84.429)), [255, 185, 0]); // amber-400
    }

    #[test]
    fn neutral_scale_is_ordered() {
        let t = Theme::dark();
        assert!(t.bg.l < t.surface.l);
        assert!(t.surface.l < t.surface_raised.l);
        assert!(t.surface_raised.l < t.text_faint.l);
        assert!(t.text_faint.l < t.text_muted.l);
        assert!(t.text_muted.l < t.text.l);
        // Monochrome: neutrals carry no saturation.
        for c in [
            t.bg,
            t.surface,
            t.surface_raised,
            t.text,
            t.text_muted,
            t.text_faint,
        ] {
            assert_eq!(c.s, 0.0);
            assert_eq!(c.a, 1.0);
        }
    }

    #[test]
    fn hairlines_are_white_and_washes_are_mid_grey() {
        let t = Theme::dark();
        // Hairlines stay white — they only need to read on dark surfaces.
        for c in [t.border, t.border_strong] {
            assert_eq!(c.l, 1.0, "hairlines are white");
            assert!(c.a > 0.0 && c.a < 0.25, "low alpha, got {}", c.a);
        }
        // Washes are translucent soft-white with enough alpha to read at the
        // glass scrim's brightness ceiling.
        for c in [t.element_hover, t.element_active] {
            assert_eq!(c.l, 0.92, "washes are soft-white");
            assert!(c.a >= 0.05 && c.a < 0.35, "alpha in band, got {}", c.a);
        }
        assert!(t.border.a < t.border_strong.a);
        // Hover intentionally equals the active fill (selection differs by
        // its ring, not brightness — user request).
        assert!(t.element_hover.a <= t.element_active.a);
    }

    #[test]
    fn accent_hues_land_in_their_bands() {
        let t = Theme::dark();
        // Hsla hue is 0..1 of the wheel. Indigo ≈ 230-250°, red < 15°, amber ≈ 40-55°.
        let deg = |c: Hsla| c.h * 360.0;
        assert!(
            (215.0..265.0).contains(&deg(t.accent)),
            "indigo hue {}",
            deg(t.accent)
        );
        assert!(
            deg(t.danger) < 15.0 || deg(t.danger) > 345.0,
            "red hue {}",
            deg(t.danger)
        );
        assert!(
            (35.0..60.0).contains(&deg(t.warning)),
            "amber hue {}",
            deg(t.warning)
        );
    }

    #[test]
    fn mix_endpoints_and_midpoint() {
        let a = hsla(0.0, 0.0, 0.0, 1.0);
        let b = hsla(0.5, 1.0, 1.0, 0.0);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
        let mid = mix(a, b, 0.5);
        assert!((mid.l - 0.5).abs() < 1e-6 && (mid.a - 0.5).abs() < 1e-6);
        // Out-of-range t clamps.
        assert_eq!(mix(a, b, 2.0), b);
    }

    #[test]
    fn layout_numbers_match_comet() {
        assert_eq!(Theme::HEADER_HEIGHT, 44.0); // h-11
        assert_eq!(Theme::STATUS_STRIP_HEIGHT, 24.0); // h-6
        assert_eq!(Theme::BUBBLE_RADIUS, 16.0);
    }

    // ---- light variant ----

    #[test]
    fn light_neutral_scale_is_inverted_ordered() {
        let t = Theme::light();
        // The light ramp walks the OTHER way: backgrounds are the lightest
        // tones, text the darkest.
        assert!(t.bg.l > t.surface.l);
        assert!(t.surface.l > t.surface_raised.l);
        assert!(t.surface_raised.l > t.text_faint.l);
        assert!(t.text_faint.l > t.text_muted.l);
        assert!(t.text_muted.l > t.text.l);
        // Monochrome: neutrals carry no saturation, same as dark.
        for c in [
            t.bg,
            t.surface,
            t.surface_raised,
            t.text,
            t.text_muted,
            t.text_faint,
        ] {
            assert_eq!(c.s, 0.0);
            assert_eq!(c.a, 1.0);
        }
        // And the two variants share their ramp structure — a surface that is
        // raised in dark is raised in light, text that is prominent in dark is
        // prominent in light.
        let d = Theme::dark();
        assert_eq!(d.bg.l < d.surface.l, t.bg.l > t.surface.l);
    }

    #[test]
    fn light_hairlines_are_black_and_washes_are_soft_black() {
        let t = Theme::light();
        // Hairlines flip to black — a white hairline would vanish on white.
        for c in [t.border, t.border_strong] {
            assert_eq!(c.l, 0.0, "hairlines are black");
            assert!(c.a > 0.0 && c.a < 0.25, "low alpha, got {}", c.a);
        }
        // Washes are translucent soft-black that darken a near-white surface.
        for c in [t.element_hover, t.element_active] {
            assert_eq!(c.l, 0.0, "washes are soft-black");
            assert!(c.a >= 0.05 && c.a < 0.35, "alpha in band, got {}", c.a);
        }
        assert!(t.border.a < t.border_strong.a);
        assert!(t.element_hover.a <= t.element_active.a);
        // The theme primitives mirror the fields.
        assert_eq!(t.wash(0.14).l, 0.0);
        assert_eq!(t.white_alpha(0.09).l, 0.0);
        assert_eq!(t.glass_selected_bg().l, 0.0);
        assert!(t.glass_selected_shadows()[0].color.l < 0.5);
    }

    #[test]
    fn light_accent_hues_land_in_their_bands() {
        let t = Theme::light();
        let deg = |c: Hsla| c.h * 360.0;
        assert!(
            (215.0..265.0).contains(&deg(t.accent)),
            "indigo hue {}",
            deg(t.accent)
        );
        assert!(
            deg(t.danger) < 15.0 || deg(t.danger) > 345.0,
            "red hue {}",
            deg(t.danger)
        );
        assert!(
            (35.0..60.0).contains(&deg(t.warning)),
            "amber hue {}",
            deg(t.warning)
        );
    }

    #[test]
    fn light_accents_are_the_darker_shades() {
        // Light mode darkens the accents so they keep contrast on white (dark
        // uses the bright 400-shades). Pinned to this crate's own oklch
        // conversion (which differs a hair from CSS Color 4's matrices).
        let t = Theme::light();
        assert_eq!(srgb_u8(oklch_to_srgb(0.511, 0.262, 276.966)), [79, 57, 246]);
        assert_eq!(srgb_u8(oklch_to_srgb(0.457, 0.24, 277.023)), [67, 45, 215]);
        assert_eq!(srgb_u8(oklch_to_srgb(0.60, 0.22, 26.0)), [230, 43, 48]);
        assert_eq!(srgb_u8(oklch_to_srgb(0.70, 0.16, 76.0)), [214, 141, 0]);
        // The light accents are visibly darker than the dark theme's.
        assert!(t.accent.l < Theme::dark().accent.l);
        assert!(t.danger.l < Theme::dark().danger.l);
    }

    #[test]
    fn error_notice_text_inverts_with_theme() {
        // Error/notice bars: dark keeps the pale 300/200 tones (readable on the
        // near-black chip), light uses the darker 700-shades so the text keeps
        // contrast on white.
        let dark = Theme::dark();
        let light = Theme::light();
        assert!(light.danger_text().l < dark.danger_text().l);
        assert!(light.warning_text().l < dark.warning_text().l);
        // Dark mode pins the previous hardcoded tones unchanged.
        assert_eq!(dark.danger_text(), oklch(0.808, 0.114, 19.571)); // red-300
        assert_eq!(dark.warning_text(), oklch(0.924, 0.12, 95.746)); // amber-200
        // Light mode uses the darker equivalents.
        assert_eq!(light.danger_text(), oklch(0.505, 0.213, 27.518)); // red-700
        assert_eq!(light.warning_text(), oklch(0.555, 0.163, 48.998)); // amber-700
        // The light tones still hold contrast against the light surfaces they
        // sit on (darker than the chip fills, like the rest of the ramp).
        assert!(light.danger_text().l < light.surface.l);
        assert!(light.warning_text().l < light.surface.l);
    }

    #[test]
    fn light_surfaces_stay_legible() {
        let t = Theme::light();
        // Text on every surface keeps real contrast (dark text on light fills).
        for (fg, bg) in [
            (t.text, t.bg),
            (t.text, t.surface),
            (t.text, t.surface_raised),
            (t.text_muted, t.bg),
            (t.text_faint, t.surface),
        ] {
            assert!(fg.l < bg.l - 0.15, "fg {} vs bg {}", fg.l, bg.l);
        }
        // Accents contrast against white: darker than the surface they sit on.
        assert!(t.accent.l < t.surface.l);
        assert!(t.danger.l < t.surface.l);
        assert!(t.warning.l < t.surface.l);
    }

    #[test]
    fn theme_choice_round_trips() {
        // The persisted form is `theme: "light"` / `"dark"` (camelCase file,
        // lowercase values).
        assert_eq!(serde_json::from_str::<ThemeChoice>("\"light\"").unwrap(), ThemeChoice::Light);
        assert_eq!(serde_json::from_str::<ThemeChoice>("\"dark\"").unwrap(), ThemeChoice::Dark);
        assert_eq!(
            serde_json::to_string(&ThemeChoice::Light).unwrap(),
            "\"light\""
        );
        // Default is dark.
        assert_eq!(ThemeChoice::default(), ThemeChoice::Dark);
        assert!(!Theme::for_choice(ThemeChoice::Dark).light);
        assert!(Theme::for_choice(ThemeChoice::Light).light);
        assert_eq!(Theme::dark().choice(), ThemeChoice::Dark);
        assert_eq!(Theme::light().choice(), ThemeChoice::Light);
    }

    #[test]
    fn theme_choice_from_env() {
        // Reads COMET_THEME; unset or unknown values yield None (callers fall
        // back to the persisted choice).
        unsafe {
            std::env::set_var("COMET_THEME", "light");
        }
        assert_eq!(ThemeChoice::from_env(), Some(ThemeChoice::Light));
        unsafe {
            std::env::set_var("COMET_THEME", "dark");
        }
        assert_eq!(ThemeChoice::from_env(), Some(ThemeChoice::Dark));
        unsafe {
            std::env::set_var("COMET_THEME", "sepia");
        }
        assert_eq!(ThemeChoice::from_env(), None);
        unsafe {
            std::env::remove_var("COMET_THEME");
        }
        assert_eq!(ThemeChoice::from_env(), None);
    }
}
