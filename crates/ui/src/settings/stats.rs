//! Settings → Board stats (gh#143): what the board did with the work it was
//! given.
//!
//! herdr-board answered this on one text screen — dispatches, completion rate,
//! median duration, retries. Everything on it is still here, because those are
//! the numbers that say whether delegating is working at all; what a window
//! with real pixels adds is the shape around them. A rate is a number; a rate
//! next to thirty days of bars is a trend. So: the headline tiles, then when
//! the work happened, then where it actually landed, then what it cost in
//! friction, then who and what ran it.
//!
//! **Where the numbers come from.** `board.db` lives on whichever device hosts
//! the board, so this page sweeps [`comet_proto::view::board::host_candidates`]
//! for the one that answers `BoardStats` — the same contract the routing page
//! and the board panel use, and the reason a laptop can read the box's
//! throughput without an ssh account on it. Every derivation the *rendering*
//! needs (ordering a tally, scaling a bar, phrasing a duration) is in
//! `comet_proto::view::stats`, so the CLI, this page and anything after it
//! agree on the arithmetic.
//!
//! **Tokens (gh#151)** sit between the dispatch chart and where the work
//! landed: the headline processed, the four buckets it splits into, the
//! provider share, a day series, and a per-model table. Counts only — what a
//! token costs depends on a seat and a price table the board does not have,
//! and a number that looks like money is read as money. Every token card
//! carries its coverage as the aside, because a total summed from two of five
//! attempts is not the window's spend, and a window that reported nothing says
//! so instead of drawing zeroes.
//!
//! Descriptive, like the CLI it shares a source with: it reports what
//! happened. Nothing here grades the operator.

use gpui::{AnyElement, Context, Entity, SharedString, Task, Window, div, prelude::*, px};

use comet_proto::view::board;
use comet_proto::TokenUsage;
use comet_proto::view::stats::{
    BoardStats, Tally, TokenTally, WINDOWS, bar_fraction, human_minutes, human_tokens,
    peak_dispatches, peak_tokens, percent, ranked_top, ranked_tokens,
};
use comet_rpc::methods;

use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::{Bed, ListRow as _, Theme};

/// How many rows a tally card shows before folding the rest into `n others`.
const TALLY_ROWS: usize = 6;

/// Bar geometry for the throughput chart. Tall enough that a quiet day and a
/// busy one are different at a glance, short enough to sit above the fold with
/// the tiles.
const CHART_HEIGHT: f32 = 96.0;

/// The corner on a drawn mark — a chart column, a meter fill, a heat cell.
///
/// scale-ok: a mark is data, not a box on a surface. Its corner relates to its
/// own 4–6px width, not to the card it sits in, so the three-radius scale
/// (gh#174) does not reach it — but it is still ONE number for every mark on
/// the page, which is the same rule one level down.
const MARK_RADIUS: f32 = 3.0;

pub struct StatsPage {
    state: Entity<AppState>,
    /// The window in days; `None` is all time. Mirrors the CLI's
    /// `--since-days`, so the two surfaces are asking one question.
    since_days: Option<i64>,
    stats: Option<BoardStats>,
    /// The device that answered. `None` before the first reply, and on a board
    /// hosted right here.
    host: Option<String>,
    loaded: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
}

impl StatsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            state,
            // A week: long enough to have a shape, short enough that today is
            // still visible in it.
            since_days: Some(7),
            stats: None,
            host: None,
            loaded: false,
            error: None,
            task: None,
        };
        page.reload(cx);
        page
    }

    /// Read the numbers, sweeping for the device that hosts the board.
    ///
    /// A candidate that errors has answered "I host no board" — the engine's
    /// contract for every board method — so the sweep moves on. When nobody
    /// answers, the last error is what the page shows: "board unavailable"
    /// from every device is a true and useful thing to read.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            self.loaded = true;
            return;
        };
        let (devices, local) = {
            let state = self.state.read(cx);
            (state.devices.clone(), state.local_device_id.clone())
        };
        let candidates = board::host_candidates(&devices, local.as_deref());
        let since_days = self.since_days;
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let mut last: Option<String> = None;
            for candidate in candidates {
                let mut params = serde_json::json!({ "sinceDays": since_days });
                if let (Some(host), Some(object)) = (candidate.as_deref(), params.as_object_mut()) {
                    object.insert("targetDeviceId".into(), serde_json::json!(host));
                }
                match engine.client().call(methods::BOARD_STATS, params).await {
                    Ok(value) => {
                        let parsed = serde_json::from_value::<BoardStats>(value);
                        let _ = this.update(cx, |page, cx| {
                            page.loaded = true;
                            match parsed {
                                Ok(stats) => {
                                    page.host = candidate;
                                    page.stats = Some(stats);
                                }
                                Err(err) => {
                                    page.error = Some(format!("Unreadable stats: {err}").into());
                                }
                            }
                            cx.notify();
                        });
                        return;
                    }
                    Err(err) => last = Some(err.to_string()),
                }
            }
            let _ = this.update(cx, |page, cx| {
                page.loaded = true;
                page.error = Some(
                    last.unwrap_or_else(|| "No device on this account hosts a board".into())
                        .into(),
                );
                cx.notify();
            });
        }));
    }

    fn set_window(&mut self, since_days: Option<i64>, cx: &mut Context<Self>) {
        if self.since_days == since_days {
            return;
        }
        self.since_days = since_days;
        // The old numbers stay on screen while the new ones land: blanking the
        // page on every window change makes a two-click comparison flicker.
        self.reload(cx);
        cx.notify();
    }

    /// The host's display name, for the subtitle.
    fn host_label(&self, cx: &gpui::App) -> SharedString {
        let Some(host) = self.host.as_deref() else {
            return "this device".into();
        };
        self.state
            .read(cx)
            .devices
            .iter()
            .find(|d| d.id == host)
            .map(|d| SharedString::from(d.name.clone()))
            .unwrap_or_else(|| SharedString::from(host.to_string()))
    }

    /// The window picker: a segmented row, current one filled.
    fn render_windows(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0))
            // The nesting rule (gh#174): segments at RADIUS_CHIP inside a track
            // at RADIUS_ROW want exactly one gutter between them, and then the
            // two curves are concentric instead of merely both round.
            .p(px(Theme::NEST_GUTTER))
            .rounded(px(Theme::RADIUS_ROW))
            .border_1()
            .border_color(theme.border)
            .bg(theme.white_alpha(0.02));
        for (days, label) in WINDOWS {
            let selected = self.since_days == *days;
            let days = *days;
            row = row.child(
                div()
                    .id(SharedString::from(format!("stats-window-{label}")))
                    .px(px(10.0))
                    .py(px(4.0))
                    .rounded(px(Theme::RADIUS_CHIP))
                    .text_size(px(Theme::TEXT_DENSE))
                    .font_weight(if selected {
                        gpui::FontWeight::MEDIUM
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .when(!selected, |el| el.cursor_pointer())
                    // A segmented control inside a settings card: in light
                    // mode the card is white, so the chosen window steps DOWN
                    // into it rather than trying to lift off it (gh#175).
                    .list_row(theme, Bed::Card, selected, format!("stats-window-{label}"))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_window(days, cx)))
                    .child(SharedString::from(*label)),
            );
        }
        row.into_any_element()
    }

    /// One headline number and what it is. The tiles carry the answer somebody
    /// opened the page for; everything below explains them.
    fn tile(theme: &Theme, value: impl Into<SharedString>, label: &str) -> AnyElement {
        div()
            .flex_1()
            .min_w_0()
            .px(px(14.0))
            .py(px(12.0))
            .rounded(px(Theme::RADIUS_CARD))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(
                div()
                    .text_size(px(Theme::TEXT_FIGURE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .truncate()
                    .child(value.into()),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_subtle)
                    .truncate()
                    .child(SharedString::from(label.to_string())),
            )
            .into_any_element()
    }

    /// Dispatches per day, with the share that ended `done` filled in.
    ///
    /// Two tones in one bar rather than two bars: the question is what
    /// proportion of a day's work landed, and side-by-side bars make that a
    /// subtraction the reader has to do.
    fn render_chart(&self, stats: &BoardStats, theme: &Theme) -> AnyElement {
        let peak = peak_dispatches(&stats.daily);
        if peak == 0 {
            return div()
                .px(px(20.0))
                .py(px(18.0))
                .text_size(px(Theme::TEXT_DENSE))
                .text_color(theme.text_subtle)
                .child(SharedString::from("Nothing was dispatched in this window."))
                .into_any_element();
        }
        let bars: Vec<AnyElement> = stats
            .daily
            .iter()
            .map(|day| {
                let total_h = CHART_HEIGHT * bar_fraction(day.dispatches, peak);
                let done_h = CHART_HEIGHT
                    * bar_fraction(day.done, peak).min(bar_fraction(day.dispatches, peak));
                div()
                    .flex_1()
                    .min_w(px(3.0))
                    .h(px(CHART_HEIGHT))
                    .flex()
                    .flex_col()
                    .justify_end()
                    .child(
                        // The column: everything dispatched, quiet…
                        div()
                            .w_full()
                            .h(px(total_h.max(if day.dispatches > 0 { 2.0 } else { 0.0 })))
                            .rounded(px(MARK_RADIUS))
                            .bg(theme.accent.opacity(0.22))
                            .flex()
                            .flex_col()
                            .justify_end()
                            // …and the part of it that landed, solid.
                            .child(
                                div()
                                    .w_full()
                                    .h(px(done_h))
                                    .rounded(px(MARK_RADIUS))
                                    .bg(theme.accent.opacity(0.85)),
                            ),
                    )
                    .into_any_element()
            })
            .collect();

        let first = stats
            .daily
            .first()
            .map(|d| d.date.clone())
            .unwrap_or_default();
        let last = stats
            .daily
            .last()
            .map(|d| d.date.clone())
            .unwrap_or_default();
        div()
            .px(px(20.0))
            .py(px(16.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(3.0))
                    .h(px(CHART_HEIGHT))
                    .children(bars),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(first))
                    .child(SharedString::from(format!("peak {peak}/day")))
                    .child(SharedString::from(last)),
            )
            .into_any_element()
    }

    /// Tokens per day, one tone.
    ///
    /// The dispatch chart above it splits its bars because "how much of a
    /// day's work landed" is a proportion; this one does not, because tokens
    /// have no such second number yet. What it does share is the day range —
    /// the two series are generated from one calendar, so a spike here sits
    /// under the day that caused it.
    fn render_token_chart(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let peak = peak_tokens(&stats.daily_tokens);
        if peak == 0 {
            return div()
                .px(px(20.0))
                .py(px(18.0))
                .text_size(px(Theme::TEXT_DENSE))
                .text_color(theme.text_subtle)
                .child(SharedString::from(
                    "No day in this window has usage to show.",
                ))
                .into_any_element();
        }
        let bars: Vec<AnyElement> = stats
            .daily_tokens
            .iter()
            .map(|day| {
                let total = day.usage.total();
                // Against the peak day and not the total, for the reason the
                // dispatch chart is: the question is which day was expensive.
                let fraction = if peak == 0 {
                    0.0
                } else {
                    (total as f32 / peak as f32).clamp(0.0, 1.0)
                };
                div()
                    .flex_1()
                    .min_w(px(3.0))
                    .h(px(CHART_HEIGHT))
                    .flex()
                    .flex_col()
                    .justify_end()
                    .child(
                        div()
                            .w_full()
                            .h(px(
                                CHART_HEIGHT * fraction + if total > 0 { 2.0 } else { 0.0 }
                            ))
                            .rounded(px(MARK_RADIUS))
                            .bg(theme.accent.opacity(0.7)),
                    )
                    .into_any_element()
            })
            .collect();
        let first = stats
            .daily_tokens
            .first()
            .map(|d| d.date.clone())
            .unwrap_or_default();
        let last = stats
            .daily_tokens
            .last()
            .map(|d| d.date.clone())
            .unwrap_or_default();
        div()
            .px(px(20.0))
            .py(px(16.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(3.0))
                    .h(px(CHART_HEIGHT))
                    .children(bars),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(first))
                    .child(SharedString::from(format!(
                        "peak {}/day",
                        human_tokens(peak)
                    )))
                    .child(SharedString::from(last)),
            )
            .into_any_element()
    }

    /// A token tally as label + share bar + total — the provider split, and
    /// anything else that divides the same pot.
    fn render_token_tally(theme: &Theme, rows: Vec<TokenTally>) -> AnyElement {
        let peak = rows.iter().map(|t| t.usage.total()).max().unwrap_or(0);
        let children: Vec<AnyElement> = rows
            .into_iter()
            .map(|row| {
                let total = row.usage.total();
                let fraction = if peak == 0 {
                    0.0
                } else {
                    (total as f32 / peak as f32).clamp(0.0, 1.0)
                };
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .w(px(150.0))
                            .flex_none()
                            .truncate()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text)
                            .child(SharedString::from(row.label)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(6.0))
                            .rounded(px(MARK_RADIUS))
                            .bg(theme.white_alpha(0.05))
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative(fraction))
                                    .rounded(px(MARK_RADIUS))
                                    .bg(theme.accent.opacity(0.6)),
                            ),
                    )
                    .child(
                        div()
                            .w(px(58.0))
                            .flex_none()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(human_tokens(total))),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .px(px(20.0))
            .py(px(14.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(children)
            .into_any_element()
    }

    /// The per-model table: where the tokens went, and into which bucket.
    ///
    /// A table rather than bars, because the interesting comparison here is
    /// across *columns* — a model whose input is almost all cache reads is a
    /// different fact from one whose input is fresh every turn, and a single
    /// bar per row cannot say which.
    fn render_token_table(theme: &Theme, rows: Vec<TokenTally>) -> AnyElement {
        let cell = |theme: &Theme, text: String, head: bool, lead: bool| {
            let base = div()
                .when(lead, |el| el.flex_1().min_w_0().truncate())
                .when(!lead, |el| el.w(px(72.0)).flex_none().text_right())
                .text_size(px(Theme::TEXT_DENSE));
            if head {
                base.text_color(theme.text_subtle)
                    .child(SharedString::from(text))
            } else {
                base.text_color(theme.text_muted)
                    .child(SharedString::from(text))
            }
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(cell(theme, "Model".into(), true, true))
            .child(cell(theme, "Uncached".into(), true, false))
            .child(cell(theme, "Cached".into(), true, false))
            .child(cell(theme, "Writes".into(), true, false))
            .child(cell(theme, "Output".into(), true, false))
            .child(cell(theme, "Total".into(), true, false));
        let body: Vec<AnyElement> = rows
            .into_iter()
            .map(|row| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text)
                            .child(SharedString::from(row.label)),
                    )
                    .child(cell(
                        theme,
                        human_tokens(row.usage.input_tokens),
                        false,
                        false,
                    ))
                    .child(cell(
                        theme,
                        human_tokens(row.usage.cache_read_tokens),
                        false,
                        false,
                    ))
                    .child(cell(
                        theme,
                        human_tokens(row.usage.cache_creation_tokens),
                        false,
                        false,
                    ))
                    .child(cell(
                        theme,
                        human_tokens(row.usage.output_tokens),
                        false,
                        false,
                    ))
                    .child(
                        div()
                            .w(px(72.0))
                            .flex_none()
                            .text_right()
                            .text_size(px(Theme::TEXT_DENSE))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(human_tokens(row.usage.total()))),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .px(px(20.0))
            .py(px(14.0))
            .flex()
            .flex_col()
            .gap(px(9.0))
            .child(header)
            .children(body)
            .into_any_element()
    }

    /// The coverage sentence — what share of the window's attempts the totals
    /// above it actually account for.
    ///
    /// It rides on every token card as the aside, not in a panel of its own,
    /// because a total read without it is a total read wrong: attempts that
    /// predate the recording, and harnesses that meter nothing, are simply
    /// absent from the sums. An honest "62% of attempts reported usage" is
    /// worth more than a figure that quietly under-reports.
    fn coverage_note(stats: &BoardStats) -> Option<String> {
        let share = percent(stats.token_coverage)?;
        Some(format!(
            "{share} of attempts reported usage ({} of {})",
            stats.attempts_with_tokens, stats.attempts
        ))
    }

    /// A ranked tally as label + bar + count.
    fn render_tally(theme: &Theme, rows: Vec<Tally>) -> AnyElement {
        let peak = rows.iter().map(|t| t.count).max().unwrap_or(0);
        let children: Vec<AnyElement> = rows
            .into_iter()
            .map(|row| {
                let fraction = bar_fraction(row.count, peak);
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .w(px(150.0))
                            .flex_none()
                            .truncate()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text)
                            .child(SharedString::from(row.label)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(6.0))
                            .rounded(px(MARK_RADIUS))
                            .bg(theme.white_alpha(0.05))
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative(fraction))
                                    .rounded(px(MARK_RADIUS))
                                    .bg(theme.accent.opacity(0.6)),
                            ),
                    )
                    .child(
                        div()
                            .w(px(34.0))
                            .flex_none()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("{}", row.count))),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .px(px(20.0))
            .py(px(14.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(children)
            .into_any_element()
    }

    /// Dispatches by hour of the box's local day — 24 slots, always all of
    /// them, so the quiet hours are as legible as the busy ones.
    fn render_hours(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let peak = stats.hour_of_day.iter().copied().max().unwrap_or(0);
        let cells: Vec<AnyElement> =
            stats
                .hour_of_day
                .iter()
                .enumerate()
                .map(|(hour, count)| {
                    let heat = bar_fraction(*count, peak);
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(4.0))
                        .child(div().w_full().h(px(26.0)).rounded(px(MARK_RADIUS)).bg(
                            if *count == 0 {
                                theme.white_alpha(0.03)
                            } else {
                                theme.accent.opacity(0.15 + 0.7 * f32::from(heat))
                            },
                        ))
                        // Every third hour is labelled: 24 numbers at this width
                        // is a smear, and the reader is looking for a shape.
                        .child(
                            div()
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(theme.text_subtle)
                                .child(SharedString::from(if hour % 3 == 0 {
                                    format!("{hour:02}")
                                } else {
                                    String::new()
                                })),
                        )
                        .into_any_element()
                })
                .collect();
        div()
            .px(px(20.0))
            .py(px(14.0))
            .flex()
            .flex_row()
            .gap(px(3.0))
            .children(cells)
            .into_any_element()
    }

    /// A plain `label — value` line inside a card.
    fn line(theme: &Theme, label: &str, value: impl Into<SharedString>) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(
                div()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(label.to_string())),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_DENSE))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(value.into()),
            )
            .into_any_element()
    }

    fn lines(_theme: &Theme, rows: Vec<AnyElement>) -> AnyElement {
        div()
            .px(px(20.0))
            .py(px(14.0))
            .flex()
            .flex_col()
            .gap(px(9.0))
            .children(rows)
            .into_any_element()
    }

    /// A card: heading, an optional aside, and a body.
    fn card(theme: &Theme, title: &str, aside: Option<String>, body: AnyElement) -> AnyElement {
        widgets::section_card(theme)
            // `section_card` carries the settings stack's own `mt(24)`. In a
            // grid that margin is wrong twice: it double-spaces the rows on top
            // of the column gap, and it drops a card 24px below the panel
            // beside it, so two halves of one row no longer start on the same
            // line. Spacing is the grid's job here.
            .mt(px(0.0))
            .h_full()
            .child(
                div()
                    .px(px(20.0))
                    .pt(px(14.0))
                    .flex()
                    .flex_row()
                    .items_baseline()
                    .justify_between()
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_BODY))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(title.to_string())),
                    )
                    .when_some(aside, |el, aside| {
                        el.child(
                            div()
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(theme.text_subtle)
                                .child(SharedString::from(aside)),
                        )
                    }),
            )
            .child(body)
            .into_any_element()
    }
    /// The answer, in the top-left corner where a reader starts.
    ///
    /// The page exists to answer "is delegating working", and before this it
    /// made you assemble that from five tiles and a scroll. So: the count, then
    /// the three facts that qualify it in one sentence each, then where the
    /// work went. Everything below this panel is evidence for it.
    fn render_answer(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let completion = percent(stats.completion_rate);
        let median = stats.median_minutes.map(human_minutes);
        // The qualifying line, assembled only from facts that exist. A board
        // with nothing ended has no rate and no median, and inventing an
        // em-dash for each would read as two failures rather than as a board
        // that has only just started.
        let mut facts: Vec<String> = Vec::new();
        if let Some(rate) = completion {
            facts.push(format!("{rate} ended in done"));
        }
        if stats.landing.merged > 0 {
            facts.push(format!("{} merged", stats.landing.merged));
        }
        if let Some(median) = median {
            facts.push(format!("{median} median"));
        }
        if stats.live > 0 {
            facts.push(format!("{} running now", stats.live));
        }

        let spaces = ranked_top(&stats.by_workspace, 4);
        let peak = spaces.first().map(|t| t.count).unwrap_or(0);
        let split: Vec<AnyElement> = spaces
            .into_iter()
            .map(|row| {
                let fraction = bar_fraction(row.count, peak);
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .w(px(120.0))
                            .flex_none()
                            .truncate()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(row.label)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(5.0))
                            .rounded(px(MARK_RADIUS))
                            .bg(theme.white_alpha(0.05))
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative(fraction))
                                    .rounded(px(MARK_RADIUS))
                                    .bg(theme.accent.opacity(0.55)),
                            ),
                    )
                    .child(
                        div()
                            .w(px(28.0))
                            .flex_none()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!("{}", row.count))),
                    )
                    .into_any_element()
            })
            .collect();

        widgets::section_card(theme)
            .mt(px(0.0))
            .h_full()
            .px(px(20.0))
            .py(px(18.0))
            .gap(px(14.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_FIGURE))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(format!(
                                "{} dispatch{}",
                                stats.attempts,
                                if stats.attempts == 1 { "" } else { "es" }
                            ))),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(if facts.is_empty() {
                                format!("across {} task(s)", stats.tasks_touched)
                            } else {
                                facts.join(" · ")
                            })),
                    ),
            )
            .when(!split.is_empty(), |el| {
                el.child(div().flex().flex_col().gap(px(7.0)).children(split))
            })
            .into_any_element()
    }

    /// The right-hand column of row three: the four facts that used to be four
    /// separate cards, each holding two numbers. A card per fact is what made
    /// the page a scroll; as label-and-value rows they read in one pass.
    fn render_glance(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let landing = stats.landing;
        let mut rows: Vec<AnyElement> = Vec::new();
        if landing.total() > 0 {
            rows.push(Self::line(
                theme,
                "Merged",
                format!("{} of {}", landing.merged, landing.total()),
            ));
            if landing.open > 0 {
                rows.push(Self::line(theme, "In review", format!("{}", landing.open)));
            }
            if landing.closed_unmerged > 0 {
                rows.push(Self::line(
                    theme,
                    "Closed unmerged",
                    format!("{}", landing.closed_unmerged),
                ));
            }
            if landing.no_pr > 0 {
                rows.push(Self::line(theme, "No pull request", format!("{}", landing.no_pr)));
            }
        }
        if let Some(coverage) = percent(stats.token_coverage) {
            rows.push(Self::line(
                theme,
                "Reported tokens",
                format!("{coverage} of attempts"),
            ));
        }
        rows.push(Self::line(
            theme,
            "Agent time",
            human_minutes(stats.total_minutes),
        ));
        if let Some(p90) = stats.p90_minutes {
            rows.push(Self::line(theme, "Nine in ten within", human_minutes(p90)));
        }
        // Friction earns a line when there is any, and one honest line when
        // there is none — four zeroes would be four things to read that all
        // say nothing happened.
        let friction = stats.friction;
        if friction.is_clean() {
            rows.push(Self::line(theme, "Friction", "none"));
        } else {
            if friction.retried_tasks > 0 {
                rows.push(Self::line(theme, "Retried", format!("{}", friction.retried_tasks)));
            }
            if friction.blocked_entries > 0 {
                rows.push(Self::line(theme, "Stopped to ask", format!("{}", friction.blocked_entries)));
            }
            if friction.early_settles > 0 {
                rows.push(Self::line(theme, "Closed while working", format!("{}", friction.early_settles)));
            }
            if friction.overruns > 0 {
                rows.push(Self::line(theme, "Past their cap", format!("{}", friction.overruns)));
            }
        }
        rows.push(Self::line(
            theme,
            "Released",
            if stats.agent_dispatched == stats.attempts {
                "all by agents".to_string()
            } else if stats.agent_dispatched == 0 {
                "all by you".to_string()
            } else {
                format!("{} by agents", stats.agent_dispatched)
            },
        ));
        Self::card(theme, "At a glance", None, Self::lines(theme, rows))
    }
}


/// The page's own column, wider than [`widgets::page_column`]'s 768px.
///
/// That width is the settings *form* rhythm — right for Devices and Accounts,
/// wrong here: a dashboard read as a 768px ribbon in a 1400px window is the
/// whole of "hard to get an overview without scrolling" (operator, 2026-08-08).
/// Still capped, because a chart stretched across an ultrawide is not a better
/// chart.
fn dashboard_column() -> gpui::Div {
    div()
        .w_full()
        .max_w(px(1160.0))
        .mx_auto()
        .px(px(24.0))
        .pt(px(32.0))
        .pb(px(64.0))
        .flex()
        .flex_col()
        .gap(px(14.0))
}

/// Two panels side by side, weighted, that stack when the window is too narrow
/// to hold them — gpui has no media queries, so `flex_wrap` plus a min width
/// per panel is the responsive rule.
fn split_row(left: AnyElement, left_grow: f32, right: AnyElement, right_grow: f32) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .flex_wrap()
        .gap(px(12.0))
        .child(
            div()
                .flex_grow(left_grow)
                .min_w(px(320.0))
                .flex_basis(px(0.0))
                .child(left),
        )
        .child(
            div()
                .flex_grow(right_grow)
                .min_w(px(320.0))
                .flex_basis(px(0.0))
                .child(right),
        )
        .into_any_element()
}

/// The scroll container every settings page wraps its column in — and the one
/// this page shipped without, which on the longest page in the app meant
/// everything below the fold was simply unreachable (operator, 2026-08-08).
/// It has to wrap EVERY return path, including the two empty states, or the
/// bug comes back the first time somebody adds a third.
fn scroll_page(column: gpui::Div) -> gpui::Stateful<gpui::Div> {
    div()
        .id("stats-page")
        .size_full()
        .overflow_y_scroll()
        .child(column)
}

impl Render for StatsPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let host = self.host_label(cx);
        let mut column = dashboard_column().child(
            div()
                .flex()
                .flex_row()
                .items_start()
                .justify_between()
                .gap(px(16.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .child(widgets::page_header(&theme, "Board stats", None))
                        .child(widgets::page_subtitle(
                            &theme,
                            format!("What the board on {host} did with the work it was given."),
                        )),
                )
                .child(self.render_windows(&theme, cx)),
        );

        if let Some(error) = self.error.clone() {
            column = column.child(div().mt(px(20.0)).child(widgets::error_strip(error)));
        }

        let Some(stats) = self.stats.clone() else {
            let note = if self.loaded {
                "No board answered."
            } else {
                "Reading the board…"
            };
            return scroll_page(
                column.child(
                    div()
                        .mt(px(24.0))
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(note)),
                ),
            );
        };

        if stats.is_empty() {
            return scroll_page(
                column.child(
                    div()
                        .mt(px(24.0))
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(format!(
                            "No dispatches in the {}. Release a task and this fills in.",
                            stats.window_label()
                        ))),
                ),
            );
        }

        // ── row 1: the answer, and the shape of it ──────────────────────────
        column = column.child(split_row(
            Self::render_answer(&stats, &theme),
            2.0,
            Self::card(
                &theme,
                "Dispatches per day",
                Some("solid = ended in done".into()),
                self.render_chart(&stats, &theme),
            ),
            3.0,
        ));

        // ── row 2: what it spent (gh#151) ───────────────────────────────────
        // Counts only. What a token costs depends on a seat, a plan and a
        // price table none of which the board knows, and a number that looks
        // like money is read as money.
        let coverage = Self::coverage_note(&stats);
        if !stats.has_tokens() {
            // Never a wall of zeroes: nothing reported is a fact about the
            // board's records, not about what the work cost.
            column = column.child(Self::card(
                &theme,
                "Tokens",
                coverage.clone(),
                div()
                    .px(px(20.0))
                    .py(px(18.0))
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(
                        "No attempt in this window reported token usage. \
                         Attempts from before the board recorded it stay blank \
                         rather than reading as free.",
                    ))
                    .into_any_element(),
            ));
        } else {
            let tokens: TokenUsage = stats.tokens;
            column = column.child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap(px(10.0))
                    .child(Self::tile(
                        &theme,
                        human_tokens(tokens.total()),
                        "tokens processed",
                    ))
                    .child(Self::tile(
                        &theme,
                        human_tokens(tokens.input_tokens),
                        "uncached input",
                    ))
                    .child(Self::tile(
                        &theme,
                        human_tokens(tokens.cache_read_tokens),
                        "cached input",
                    ))
                    .child(Self::tile(
                        &theme,
                        human_tokens(tokens.cache_creation_tokens),
                        "cache writes",
                    ))
                    .child(Self::tile(
                        &theme,
                        human_tokens(tokens.output_tokens),
                        "output",
                    )),
            );

            // ── row 3: the breakdown, beside the facts that qualify it ──────
            column = column.child(split_row(
                Self::card(
                    &theme,
                    "By model",
                    coverage.clone(),
                    Self::render_token_table(
                        &theme,
                        ranked_tokens(&stats.tokens_by_model, TALLY_ROWS),
                    ),
                ),
                3.0,
                Self::render_glance(&stats, &theme),
                2.0,
            ));

            column = column.child(split_row(
                Self::card(
                    &theme,
                    "Tokens per day",
                    Some("tokens processed".into()),
                    Self::render_token_chart(&stats, &theme),
                ),
                3.0,
                Self::card(
                    &theme,
                    "By provider",
                    coverage,
                    Self::render_token_tally(
                        &theme,
                        ranked_tokens(&stats.tokens_by_runtime, TALLY_ROWS),
                    ),
                ),
                2.0,
            ));
        }

        // With no tokens to break down, the glance panel still has to land —
        // it carries where the work went, which is not a token fact.
        if !stats.has_tokens() {
            column = column.child(split_row(
                Self::render_glance(&stats, &theme),
                2.0,
                Self::card(
                    &theme,
                    "When you release work",
                    Some("local time".into()),
                    Self::render_hours(&stats, &theme),
                ),
                3.0,
            ));
        } else {
            column = column.child(Self::card(
                &theme,
                "When you release work",
                Some("local time".into()),
                Self::render_hours(&stats, &theme),
            ));
        }

        // ── the tallies, two up ─────────────────────────────────────────────
        let tallies: Vec<AnyElement> = [
            ("By space", &stats.by_workspace),
            ("By runtime", &stats.by_runtime),
            ("By tracker", &stats.by_source),
            ("Whose subscription", &stats.by_account),
        ]
        .into_iter()
        .filter(|(_, tally)| !tally.is_empty())
        .map(|(title, tally)| {
            Self::card(
                &theme,
                title,
                None,
                Self::render_tally(&theme, ranked_top(tally, TALLY_ROWS)),
            )
        })
        .collect();
        let mut pairs = tallies.into_iter();
        while let Some(left) = pairs.next() {
            match pairs.next() {
                Some(right) => column = column.child(split_row(left, 1.0, right, 1.0)),
                None => column = column.child(left),
            }
        }

        scroll_page(column)
    }
}
