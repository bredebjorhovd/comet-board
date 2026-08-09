//! The sidebar's account footer (gh#230): avatar, display name, plan tier —
//! and the row chrome the settings-nav `Back` row directly above it shares.
//!
//! The block closes the sidebar on *every* route, which is the point. The
//! board sweeps hosts, so the window can be showing a device you are not
//! sitting at, and the pages that price work read rates off whichever account
//! is signed in — a "who am I, on what plan" anchor only stops that being
//! ambiguous if it is still there when you walk over to Settings to check.
//!
//! Two things live here and nowhere else:
//!
//! **The row chrome.** `Back` and the account block are the same construction
//! in the design — one radius, one padding, one gap, one hover wash — so they
//! are one function ([`footer_row`]) rather than two call sites that agree
//! today. The block's *content* is [`account_block`]; the menu it triggers and
//! the click handlers stay with `Shell`, which owns that state.
//!
//! **No strings.** Every word the block shows comes from
//! [`comet_proto::view::account`], including the signed-out row. A literal
//! here is how the old footer ended up telling a signed-out window it was on
//! the Alpha plan.

use gpui::{Div, ElementId, SharedString, Stateful, div, prelude::*, px};

use comet_proto::view::account::AccountFooter;

use crate::motion;
use crate::theme::Theme;

/// Avatar diameter (design: 26px circle). Small enough that the block reads as
/// a row rather than a card, big enough for the mark to be legible.
const AVATAR: f32 = 26.0;
/// The footer row's own padding — a hair wider than it is tall, so the avatar
/// sits off the edge by the same optical amount it sits off the top.
const PAD_X: f32 = 9.0;
const PAD_Y: f32 = 8.0;
/// Avatar → text, and icon → label on the `Back` row.
const GAP: f32 = 10.0;
/// Name and plan line heights (13/18 and 11/15 in the design). Set explicitly:
/// the two lines stack inside a 26px avatar's height, and the default leading
/// would push the block a few px taller than the row beside it.
const NAME_LEADING: f32 = 18.0;
const PLAN_LEADING: f32 = 15.0;

/// The hairline that separates the footer from the list above it.
///
/// Inset inside the sidebar's own 8px gutter, so it stops short of the rows
/// rather than running the full width of the column — a rule under the list,
/// not a border around the footer.
pub fn footer_divider(theme: &Theme) -> Div {
    div()
        .h(px(1.0))
        .flex_none()
        .mx(px(Theme::SPACE_SM))
        .bg(theme.border)
}

/// The chrome both footer rows wear: the sidebar's row radius, the design's
/// 8×9 padding, a 10px gap, and the one hover wash the shell uses everywhere.
///
/// `key` is both the element id and the hover-fade key, so a row cannot end up
/// animating off another row's hover state.
pub fn footer_row(theme: &Theme, key: impl Into<SharedString>) -> Stateful<Div> {
    let key: SharedString = key.into();
    div()
        .id(ElementId::Name(key.clone()))
        .flex_none()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(GAP))
        .rounded(px(Theme::RADIUS_ROW))
        .px(px(PAD_X))
        .py(px(PAD_Y))
        .cursor_pointer()
        .bg(motion::hover_blend(&key, theme.wash(0.0), theme.wash(0.11)))
        .on_hover(motion::hover_listener(key))
}

/// The avatar: the name's initial, knocked out of a solid `text`-toned circle.
///
/// Inverted on purpose — filling with the text tone and lettering in the card
/// tone means one construction reads in both themes, with no second colour to
/// keep in sync.
pub fn avatar(theme: &Theme, initial: impl Into<SharedString>) -> Div {
    let initial: SharedString = initial.into();
    div()
        .size(px(AVATAR))
        .flex_none()
        // round-ok: the account avatar. An identity mark is a face, not a box —
        // the one place in the shell where roundness is the meaning (gh#230).
        .rounded_full()
        .bg(theme.text)
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(Theme::TEXT_DENSE))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.card)
        .child(initial)
}

/// The account block, less its click handler and its menu: avatar, name, plan.
///
/// `open` paints the menu's held state — the same tone the row hovers to, but
/// flat, so the row does not fade back out while the menu it opened is up.
pub fn account_block(theme: &Theme, footer: &AccountFooter, open: bool) -> Stateful<Div> {
    let name: SharedString = footer.name.clone().into();
    let plan: SharedString = footer.plan.clone().into();
    footer_row(theme, "account-footer")
        .when(open, |el| el.bg(theme.element_hover))
        .child(avatar(theme, footer.initial.clone()))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .line_height(px(NAME_LEADING))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .truncate()
                        .child(name),
                )
                .child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .line_height(px(PLAN_LEADING))
                        .text_color(theme.text_subtle)
                        .truncate()
                        .child(plan),
                ),
        )
}
