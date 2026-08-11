//! Shared scaffolding for the settings pages — the page column, section cards,
//! row layout, badges and small buttons, so every page reads as the same
//! product surface (comet settings.devices.tsx / settings.agents.tsx /
//! settings.archived.tsx).
//!
//! # The grammar is the deliverable (gh#178)
//!
//! Eight settings pages are composed from the dozen functions below, which is
//! why they are worth tuning rather than the pages: six of the eight needed no
//! edit at all to pick up this file's last pass. Three rules hold it together,
//! and each replaced a habit that had spread one reasonable call site at a time:
//!
//! - **Severity comes from the theme, never from a literal.** [`error_strip`],
//!   [`warning_strip`] and [`badge_active`] paint from [`Theme::danger`] /
//!   [`Theme::warning`] / [`Theme::settled`] and their `*_text()` inversions.
//!   The red-300 and amber-200 they used to hardcode were legible on a
//!   near-black card and nowhere near legible on a white one, so every page
//!   that reported a failure reported it illegibly in light mode.
//! - **Two widths, both declared.** [`page_column`] is the form rhythm and
//!   [`dashboard_column`] the wide one; a page picks one instead of arguing
//!   with a comment (see the two functions).
//! - **The scale is [`Theme`]'s.** Four type sizes, three radii, four greys:
//!   no radius, size or tone is a literal here (gh#172, gh#173, gh#174).
//! - **Spacing has two numbers, and they are named** (gh#277). The canvas
//!   stacks a settings page out of [`BLOCK_GAP`] between blocks and
//!   [`HEADER_GAP`] between a block's header and its card; a row is
//!   [`ROW_PAD_Y`]×[`ROW_PAD_X`] with a [`ROW_GAP`] gap, and every small action
//!   is [`ACTION_HEIGHT`] tall. Locked by
//!   [`tests::the_canvas_measures_are_the_canvas_measures`], because a fourth
//!   number invented at a call site is exactly how the last pass drifted.

use gpui::{AnyElement, SharedString, div, prelude::*, px};

use crate::theme::Theme;

/// The settings *form* width — Accounts, Devices, Members, Routing, Shortcuts,
/// Appearance, Archived. A field, a row and a sentence of help all want the
/// same measure, and it is the one prose has: past ~768px a settings row is a
/// label marooned from the control it belongs to.
pub const FORM_WIDTH: f32 = 768.0;

/// The *dashboard* width — Stats, and anything else whose job is comparison.
///
/// A dashboard read as a 768px ribbon in a 1400px window is the whole of "hard
/// to get an overview without scrolling" (operator, 2026-08-08). Still capped:
/// a chart stretched across an ultrawide is not a better chart.
pub const DASHBOARD_WIDTH: f32 = 1160.0;

/// The reference's page frame: the named widths above are CONTENT widths, so
/// the centered wrapper has to include its two 24px gutters rather than eating
/// them out of the measure (gh#258).
const PAGE_GUTTER: f32 = 24.0;
const PAGE_TOP: f32 = 26.0;
const SUBTITLE_WIDTH: f32 = 560.0;

/// The canvas's two vertical rhythms (`docs/design/settings.md` C4, E1):
/// **18px** between the blocks of a page, **8px** between a block's header and
/// the card under it. Everything on a settings page is one of those two gaps;
/// a third number is a page inventing a rhythm.
pub const BLOCK_GAP: f32 = 18.0;
pub const HEADER_GAP: f32 = 8.0;

/// Centered page column at the form width — the default, and what seven of the
/// eight pages use.
pub fn page_column() -> gpui::Div {
    column(FORM_WIDTH)
}

/// Centered page column at the dashboard width, with the panel gap a grid of
/// cards wants (the cards themselves then drop their stack margin).
///
/// Declared here rather than hand-rolled on the one page that needs it: a page
/// choosing between two named widths is a decision, a page redefining the
/// column privately is a fork.
pub fn dashboard_column() -> gpui::Div {
    column(DASHBOARD_WIDTH).gap(px(12.0))
}

fn column(max_width: f32) -> gpui::Div {
    div()
        .w_full()
        .max_w(px(max_width + 2.0 * PAGE_GUTTER))
        .mx_auto()
        .px(px(PAGE_GUTTER))
        .pt(px(PAGE_TOP))
        .pb(px(64.0))
        .flex()
        .flex_col()
}

/// Page headline row (`docs/design/settings.md` D2): the 15px/600 title, the
/// 13px count beside it, 10px apart and vertically CENTRED — the canvas centres
/// this row because the actions at its other end are 26px chips, and a baseline
/// shared with a 15px word hangs them a pixel low.
pub fn page_header(theme: &Theme, title: &str, count: Option<usize>) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .child(
            div()
                .text_size(px(Theme::TEXT_TITLE))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from(title.to_string())),
        )
        .when_some(count, |el, count| {
            el.child(
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(format!("{count}"))),
            )
        })
}

/// Subtitle under the headline (`docs/design/settings.md` D6/J1): 13px/20 in
/// `--subtle`, 4px under the title, capped at the prose measure.
///
/// `--subtle` and not `--muted`: the canvas keeps `--muted` for things you read
/// one of (a row's name, a nav label) and drops the standing explanation a step
/// below it. A subtitle set at row weight competes with the rows it introduces.
pub fn page_subtitle(theme: &Theme, copy: impl Into<SharedString>) -> gpui::Div {
    div()
        .mt(px(Theme::SPACE_XS))
        .max_w(px(SUBTITLE_WIDTH))
        .text_size(px(Theme::TEXT_BODY))
        .line_height(px(20.0))
        .text_color(theme.text_subtle)
        .child(copy.into())
}

/// Section card (`docs/design/settings.md` E4/J2): the `--raised` bed, a
/// `--line` hairline, radius 14, clipped — one block below whatever precedes it.
pub fn section_card(theme: &Theme) -> gpui::Div {
    div()
        .mt(px(BLOCK_GAP))
        .rounded(px(Theme::RADIUS_CARD))
        .border_1()
        .border_color(theme.border)
        .bg(theme.card)
        .overflow_hidden()
        .flex()
        .flex_col()
}

/// One card row (`docs/design/settings.md` F1/J3): padding 14×18, a `--line`
/// top border on every row after the first, no fill of its own — plus the quiet
/// hover wash the canvas cannot draw.
pub fn card_row(theme: &Theme, first: bool) -> gpui::Div {
    div()
        .px(px(ROW_PAD_X))
        .py(px(ROW_PAD_Y))
        .when(!first, |el| el.border_t_1().border_color(theme.border))
        .hover(|s| s.bg(theme.white_alpha(0.015)))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(ROW_GAP))
}

/// The row measures every settings card shares.
pub const ROW_PAD_X: f32 = 18.0;
pub const ROW_PAD_Y: f32 = 14.0;
pub const ROW_GAP: f32 = 12.0;

/// The icon that introduces a row — a glyph, not a box.
///
/// It used to be a 36px bordered tile with a 3% fill, which is a card inside a
/// row inside a card: a second boxed thing on the line, drawn at more weight
/// than the title it was there to introduce, and repeated down every row so the
/// eye read the column of tiles before it read a single name. What the row
/// needs is the glyph. The 20px box stays because callers hang things off it
/// (Devices pins a presence dot to its corner) and because a fixed slot keeps
/// titles aligned when one icon is narrower than the next.
pub fn row_tile(theme: &Theme, icon_path: &'static str) -> gpui::Div {
    div()
        .flex_none()
        .size(px(20.0))
        .flex()
        .items_center()
        .justify_center()
        .child(
            crate::icons::icon(icon_path)
                .size(px(16.0))
                .text_color(theme.text_subtle),
        )
}

/// Row title: `text-[13.5px] font-medium leading-tight`.
pub fn row_title(theme: &Theme, title: impl Into<SharedString>) -> gpui::Div {
    div()
        .min_w_0()
        .truncate()
        .text_size(px(Theme::TEXT_BODY))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text)
        .child(title.into())
}

/// The quiet meta line under a row title: fragments joined by dots — metadata,
/// so `text_subtle`, with the dots themselves a step below.
///
/// [`Theme::TEXT_DENSE`], one step under the title above it. It sat at caption
/// size, two steps down, which is the size the badges are: a line of prose
/// people actually read, set at label size.
pub fn meta_line(theme: &Theme, fragments: Vec<AnyElement>) -> gpui::Div {
    let mut line = div()
        .mt(px(4.0))
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_x(px(8.0))
        .gap_y(px(2.0))
        .text_size(px(Theme::TEXT_DENSE))
        .text_color(theme.text_subtle);
    let mut first = true;
    for fragment in fragments {
        if !first {
            line = line.child(
                div()
                    .text_color(theme.text_faint)
                    .child(SharedString::from("·")),
            );
        }
        line = line.child(fragment);
        first = false;
    }
    line
}

/// The plain badge (`docs/design/settings.md` F8): padding 1×8, radius 6, 11px
/// `--subtle` inside a `--line` hairline and nothing else. A badge that names a
/// plan is a caption on the row's title, so it sits a step under it.
pub fn badge(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_none()
        .px(px(BADGE_PAD_X))
        .py(px(BADGE_PAD_Y))
        .rounded(px(Theme::RADIUS_CHIP))
        .border_1()
        .border_color(theme.border)
        .text_size(px(Theme::TEXT_CAPTION))
        .text_color(theme.text_subtle)
        .child(label.into())
}

/// The settled status pill (the Accounts "Active" badge, `settings.md` F7) —
/// the ramp's settled hue at a 14% fill, with the tone that inverts for light
/// on top.
pub fn badge_active(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex_none()
        .px(px(BADGE_PAD_X))
        .py(px(BADGE_PAD_Y))
        .rounded(px(Theme::RADIUS_CHIP))
        .bg(theme.settled.opacity(0.14))
        .text_size(px(Theme::TEXT_CAPTION))
        .text_color(theme.settled_text())
        .child(label.into())
}

const BADGE_PAD_X: f32 = 8.0;
const BADGE_PAD_Y: f32 = 1.0;

/// A small quiet ghost action (`docs/design/settings.md` D4). Caller adds id +
/// click + leading icon child AND its own `.hover(..)` — gpui panics on a second
/// hover, and the pages vary it (reveal opacity, 4% vs 6% washes).
///
/// The canvas's own measures: a 26px slot, radius 6, padding 0×9, 6px gap, 12px
/// `--muted`. Same height and radius as the chip beside it in the Accounts
/// header — the two actions there are one control each, and one of them happens
/// to carry a bed.
pub fn ghost_action(theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .h(px(ACTION_HEIGHT))
        .gap(px(6.0))
        .rounded(px(Theme::RADIUS_CHIP))
        .px(px(ACTION_PAD_X))
        .text_size(px(Theme::TEXT_DENSE))
        .text_color(theme.text_muted)
        .cursor_pointer()
}

/// The height of every small action on a settings page — the header chip, the
/// ghost actions, the one filled button.
pub const ACTION_HEIGHT: f32 = 26.0;
pub const ACTION_PAD_X: f32 = 9.0;

/// The default ghost-action hover wash (`hover:bg-white/[0.06]
/// hover:text-foreground`).
pub fn ghost_hover(theme: &Theme) -> impl Fn(gpui::StyleRefinement) -> gpui::StyleRefinement {
    let theme = theme.clone();
    move |s: gpui::StyleRefinement| s.bg(theme.white_alpha(0.06)).text_color(theme.text)
}

/// The dismissible error strip: the blocked hue as a hairline and a 6% wash,
/// with the danger text tone and a leading warning glyph.
///
/// Every colour comes from the theme. The literal red-400/red-300 pair it
/// carried was chosen against a near-black card, and in light mode the strip
/// kept them: a pale red on a white wash, which is the least legible any text
/// in the app got — on the one widget whose whole job is to be read.
pub fn error_strip(theme: &Theme, message: impl Into<SharedString>) -> gpui::Div {
    severity_strip(
        theme.danger,
        theme.danger_text(),
        px(STRIP_PAD_X),
        px(12.0),
        px(16.0),
        message,
    )
    .mt(px(Theme::SPACE_LG))
}

/// The warning strip (`docs/design/settings.md` H1/H2) — the working hue, and
/// one notch quieter than [`error_strip`] in padding and glyph: a notice, not a
/// failure.
pub fn warning_strip(theme: &Theme, message: impl Into<SharedString>) -> gpui::Div {
    severity_strip(
        theme.warning,
        theme.warning_text(),
        px(STRIP_PAD_X),
        px(10.0),
        px(14.0),
        message,
    )
}

/// One severity strip, in whichever hue means what happened.
///
/// Radius 10, not the card's 14: a strip is a notice laid ON the page, the size
/// of a row, and the canvas curves it like one. The hue carries the strip —
/// 30% for the hairline, 9% for the wash — which is what makes it legible
/// against `--raised` in light, where a 20/6 pair had almost nothing to say.
fn severity_strip(
    hue: gpui::Hsla,
    text: gpui::Hsla,
    px_x: gpui::Pixels,
    px_y: gpui::Pixels,
    glyph: gpui::Pixels,
    message: impl Into<SharedString>,
) -> gpui::Div {
    div()
        .px(px_x)
        .py(px_y)
        .rounded(px(Theme::RADIUS_ROW))
        .border_1()
        .border_color(hue.opacity(0.30))
        .bg(hue.opacity(0.09))
        .text_size(px(Theme::TEXT_DENSE))
        .line_height(px(18.0))
        .text_color(text)
        .flex()
        .flex_row()
        .items_start()
        .gap(px(9.0))
        .child(
            div().flex_none().mt(px(1.0)).child(
                crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                    .size(glyph)
                    .text_color(text),
            ),
        )
        .child(div().min_w_0().child(message.into()))
}

const STRIP_PAD_X: f32 = 14.0;

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect gh#178 names: a severity widget that hardcodes its paint is
    /// a widget that is only legible in one theme. Both strips and the settled
    /// badge must move when the theme does.
    #[test]
    fn severity_paint_inverts_with_the_theme() {
        let (dark, light) = (Theme::dark(), Theme::light());
        for (d, l) in [
            (dark.danger_text(), light.danger_text()),
            (dark.warning_text(), light.warning_text()),
            (dark.settled_text(), light.settled_text()),
        ] {
            assert_ne!(d, l, "a severity tone that does not invert");
            assert!(l.l < d.l, "the light tone must be the darker one");
        }
        // And the fills they sit on come off the anchored ramp, so no severity
        // shouts louder than another by accident (gh#173).
        for theme in [Theme::dark(), Theme::light()] {
            for hue in [theme.danger, theme.warning, theme.settled] {
                assert_eq!(hue.a, 1.0, "a ramp hue carries its own alpha nowhere");
            }
        }
    }

    /// The numbers `canvas/comet-settings-window.dc.html` declares, transcribed
    /// (`docs/design/settings.md` C4, E1, F1, F7, F8, D4). Change one here and
    /// this fails, which is the point: prose let four surfaces land half-done.
    #[test]
    fn the_canvas_measures_are_the_canvas_measures() {
        // The page frame.
        assert_eq!(PAGE_GUTTER, 24.0);
        assert_eq!(PAGE_TOP, 26.0);
        assert_eq!(SUBTITLE_WIDTH, 560.0);
        // The two vertical rhythms, and the block gap is the wider one.
        assert_eq!(BLOCK_GAP, 18.0);
        assert_eq!(HEADER_GAP, 8.0);
        const { assert!(BLOCK_GAP > HEADER_GAP) };
        // A row.
        assert_eq!(ROW_PAD_X, 18.0);
        assert_eq!(ROW_PAD_Y, 14.0);
        assert_eq!(ROW_GAP, 12.0);
        // The badges and the actions.
        assert_eq!((BADGE_PAD_X, BADGE_PAD_Y), (8.0, 1.0));
        assert_eq!((ACTION_HEIGHT, ACTION_PAD_X), (26.0, 9.0));
        assert_eq!(STRIP_PAD_X, 14.0);
    }

    /// Two widths, both named — the page picks one instead of redefining the
    /// column privately with a comment explaining why.
    #[test]
    fn there_are_exactly_two_page_widths() {
        assert_eq!(FORM_WIDTH, 768.0);
        assert_eq!(DASHBOARD_WIDTH, 1160.0);
        assert_eq!(PAGE_GUTTER, 24.0);
        assert_eq!(PAGE_TOP, 26.0);
        assert_eq!(SUBTITLE_WIDTH, 560.0);
        // A width apiece, and the wide one is the wide one.
        const { assert!(DASHBOARD_WIDTH > FORM_WIDTH) };
    }
}
