//! Settings → Appearance: the theme preference. comet is always-dark by
//! default; this page flips to the light variant and persists the choice.
//! Changes emit [`AppearanceEvent::ThemeChanged`]; the shell re-sets the theme
//! global and saves `ui-settings.json`, so the flip is live.
//!
//! The page keeps a working copy of the choice (like the Shortcuts page's
//! keymap); it is the only writer, so the copy never drifts.

use gpui::{Context, Entity, SharedString, Window, div, prelude::*, px};

use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::{Bed, ListRow as _, Theme, ThemeChoice};

/// A theme change the shell must apply + persist.
#[derive(Debug, Clone, Copy)]
pub enum AppearanceEvent {
    ThemeChanged(ThemeChoice),
}

pub struct AppearancePage {
    /// Working copy — kept in sync with the shell via `ThemeChanged`.
    choice: ThemeChoice,
    // The page never talks RPC; state is kept for parity with sibling pages.
    _state: Entity<AppState>,
}

impl gpui::EventEmitter<AppearanceEvent> for AppearancePage {}

impl AppearancePage {
    pub fn new(state: Entity<AppState>, choice: ThemeChoice) -> Self {
        Self {
            choice,
            _state: state,
        }
    }

    fn set(&mut self, choice: ThemeChoice, cx: &mut Context<Self>) {
        if self.choice == choice {
            return;
        }
        self.choice = choice;
        cx.emit(AppearanceEvent::ThemeChanged(choice));
        cx.notify();
    }
}

impl gpui::Render for AppearancePage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = Theme::of(cx).clone();
        let choice = self.choice;

        let rows = ThemeChoice::ALL.into_iter().map(|option| {
            let selected = option == choice;
            let (title, description) = option.describe();
            div()
                .id(SharedString::from(format!("appearance-{option}")))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(14.0))
                .px(px(20.0))
                .py(px(16.0))
                .min_h(px(72.0))
                .cursor_pointer()
                // The chosen theme is a selected row in a card that is WHITE
                // in one of the two options this page offers: it steps down
                // into the card and takes the hairline (gh#175). The radio
                // still marks it — but the row no longer relies on a 16px
                // glyph to say which one is yours.
                .list_row(
                    &theme,
                    Bed::Card,
                    selected,
                    format!("appearance-row-{option}"),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set(option, cx)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_BODY))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(SharedString::from(title)),
                        )
                        .child(
                            div()
                                .mt(px(2.0))
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(description)),
                        ),
                )
                .child(theme_radio(&theme, selected))
        });

        div()
            .id("appearance-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(widgets::page_header(&theme, "Appearance", None))
                            .child(
                                widgets::page_subtitle(
                                    &theme,
                                    "How the app looks. Dark is the default; Light inverts \
                                     the palette for bright environments. The choice stays on \
                                     this device.",
                                )
                                .max_w(px(512.0))
                                .line_height(px(20.0)),
                            ),
                    )
                    .child(widgets::section_card(&theme).children(rows)),
            )
    }
}

/// The selection indicator: an empty ring, or a filled accent ring with a check
/// (comet's radio rows — `size-4 rounded-full border`).
fn theme_radio(theme: &Theme, selected: bool) -> gpui::Div {
    if selected {
        div()
            .flex_none()
            .size(px(18.0))
            // round-ok: a radio ring — square it and it reads as a checkbox
            .rounded_full()
            .bg(theme.accent)
            .flex()
            .items_center()
            .justify_center()
            .child(
                crate::icons::icon(crate::icons::CHECK)
                    .size(px(12.0))
                    .text_color(theme.bg),
            )
    } else {
        div()
            .flex_none()
            .size(px(18.0))
            // round-ok: the empty radio ring
            .rounded_full()
            .border_1()
            .border_color(theme.border_strong)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_options_are_distinct_and_described() {
        let mut seen = std::collections::HashSet::new();
        for option in ThemeChoice::ALL {
            assert!(seen.insert(option), "{option} listed twice");
            let (title, _) = option.describe();
            assert!(!title.is_empty());
        }
        assert_eq!(ThemeChoice::ALL.len(), 2, "dark + light");
    }
}
