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
//! Descriptive, like the CLI it shares a source with: it reports what
//! happened. Nothing here grades the operator.

use gpui::{AnyElement, Context, Entity, SharedString, Task, Window, div, prelude::*, px};

use comet_proto::view::board;
use comet_proto::view::stats::{
    BoardStats, Tally, WINDOWS, bar_fraction, human_minutes, peak_dispatches, percent, ranked_top,
};
use comet_rpc::methods;

use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

/// How many rows a tally card shows before folding the rest into `n others`.
const TALLY_ROWS: usize = 6;

/// Bar geometry for the throughput chart. Tall enough that a quiet day and a
/// busy one are different at a glance, short enough to sit above the fold with
/// the tiles.
const CHART_HEIGHT: f32 = 96.0;

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
            .p(px(2.0))
            .rounded(px(9.0))
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
                    .rounded(px(7.0))
                    .text_size(px(12.0))
                    .font_weight(if selected {
                        gpui::FontWeight::MEDIUM
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .text_color(if selected {
                        theme.text
                    } else {
                        theme.text_muted
                    })
                    .when(selected, |el| el.bg(theme.element_active))
                    .when(!selected, |el| {
                        el.hover(|s| s.bg(theme.element_hover)).cursor_pointer()
                    })
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
            .rounded(px(11.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.surface)
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(
                div()
                    .text_size(px(21.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .truncate()
                    .child(value.into()),
            )
            .child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text_muted.opacity(0.75))
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
                .text_size(px(12.5))
                .text_color(theme.text_faint)
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
                            .rounded(px(3.0))
                            .bg(theme.accent.opacity(0.22))
                            .flex()
                            .flex_col()
                            .justify_end()
                            // …and the part of it that landed, solid.
                            .child(
                                div()
                                    .w_full()
                                    .h(px(done_h))
                                    .rounded(px(3.0))
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
                    .text_size(px(10.5))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(first))
                    .child(SharedString::from(format!("peak {peak}/day")))
                    .child(SharedString::from(last)),
            )
            .into_any_element()
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
                            .text_size(px(12.5))
                            .text_color(theme.text)
                            .child(SharedString::from(row.label)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h(px(6.0))
                            .rounded(px(3.0))
                            .bg(theme.white_alpha(0.05))
                            .child(
                                div()
                                    .h_full()
                                    .w(gpui::relative(fraction))
                                    .rounded(px(3.0))
                                    .bg(theme.accent.opacity(0.6)),
                            ),
                    )
                    .child(
                        div()
                            .w(px(34.0))
                            .flex_none()
                            .text_size(px(12.0))
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
        let cells: Vec<AnyElement> = stats
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
                    .child(
                        div()
                            .w_full()
                            .h(px(26.0))
                            .rounded(px(4.0))
                            .bg(if *count == 0 {
                                theme.white_alpha(0.03)
                            } else {
                                theme.accent.opacity(0.15 + 0.7 * f32::from(heat))
                            }),
                    )
                    // Every third hour is labelled: 24 numbers at this width
                    // is a smear, and the reader is looking for a shape.
                    .child(div().text_size(px(9.0)).text_color(theme.text_faint).child(
                        SharedString::from(if hour % 3 == 0 {
                            format!("{hour:02}")
                        } else {
                            String::new()
                        }),
                    ))
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
                    .text_size(px(12.5))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(label.to_string())),
            )
            .child(
                div()
                    .text_size(px(12.5))
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
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(title.to_string())),
                    )
                    .when_some(aside, |el, aside| {
                        el.child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(aside)),
                        )
                    }),
            )
            .child(body)
            .into_any_element()
    }
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
        let mut column = widgets::page_column().child(
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
            return scroll_page(column.child(
                div()
                    .mt(px(24.0))
                    .text_size(px(13.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(note)),
            ));
        };

        if stats.is_empty() {
            return scroll_page(column.child(
                div()
                    .mt(px(24.0))
                    .text_size(px(13.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(format!(
                        "No dispatches in the {}. Release a task and this fills in.",
                        stats.window_label()
                    ))),
            ));
        }

        // ── the headline ────────────────────────────────────────────────────
        // Completion has no number until something has ended, and "—" is the
        // honest glyph for that: a 0% on a board whose first agent is still
        // running would be a lie about the board rather than about the work.
        let completion = percent(stats.completion_rate).unwrap_or_else(|| "—".into());
        let median = stats
            .median_minutes
            .map(human_minutes)
            .unwrap_or_else(|| "—".into());
        column = column.child(
            div()
                .mt(px(20.0))
                .flex()
                .flex_row()
                .gap(px(10.0))
                .child(Self::tile(
                    &theme,
                    format!("{}", stats.attempts),
                    "dispatches",
                ))
                .child(Self::tile(
                    &theme,
                    format!("{}", stats.tasks_touched),
                    "tasks touched",
                ))
                .child(Self::tile(&theme, completion, "ended in done"))
                .child(Self::tile(&theme, median, "median run"))
                .child(Self::tile(&theme, format!("{}", stats.live), "running now")),
        );

        // ── when ────────────────────────────────────────────────────────────
        column = column.child(Self::card(
            &theme,
            "Dispatches",
            Some("solid = ended in done".into()),
            self.render_chart(&stats, &theme),
        ));

        // ── where it landed ─────────────────────────────────────────────────
        // The question a completion rate only half-answers: an attempt can end
        // `done` and leave a pull request nobody merged.
        let landing = stats.landing;
        if landing.total() > 0 {
            column = column.child(Self::card(
                &theme,
                "Where the work landed",
                Some(format!("{} task(s)", landing.total())),
                Self::lines(
                    &theme,
                    vec![
                        Self::line(&theme, "Merged", format!("{}", landing.merged)),
                        Self::line(
                            &theme,
                            "Pull request still open",
                            format!("{}", landing.open),
                        ),
                        Self::line(
                            &theme,
                            "Closed without merging",
                            format!("{}", landing.closed_unmerged),
                        ),
                        Self::line(&theme, "No pull request", format!("{}", landing.no_pr)),
                    ],
                ),
            ));
        }

        // ── how long ────────────────────────────────────────────────────────
        let mut duration_rows = vec![Self::line(
            &theme,
            "Agent time in this window",
            human_minutes(stats.total_minutes),
        )];
        if let Some(p90) = stats.p90_minutes {
            duration_rows.push(Self::line(
                &theme,
                "Nine in ten finished within",
                human_minutes(p90),
            ));
        }
        if let Some(longest) = stats.longest_minutes {
            duration_rows.push(Self::line(&theme, "Longest", human_minutes(longest)));
        }
        column = column.child(Self::card(
            &theme,
            "How long they take",
            stats
                .median_minutes
                .map(|m| format!("{} median", human_minutes(m))),
            Self::lines(&theme, duration_rows),
        ));

        // ── friction ────────────────────────────────────────────────────────
        let friction = stats.friction;
        let friction_body = if friction.is_clean() {
            div()
                .px(px(20.0))
                .py(px(14.0))
                .text_size(px(12.5))
                .text_color(theme.text_faint)
                .child(SharedString::from(
                    "Nothing was retried, reopened, blocked or capped.",
                ))
                .into_any_element()
        } else {
            Self::lines(
                &theme,
                vec![
                    Self::line(
                        &theme,
                        "Tasks that needed more than one go",
                        format!("{}", friction.retried_tasks),
                    ),
                    Self::line(
                        &theme,
                        "Times an agent stopped to ask, or died",
                        format!("{}", friction.blocked_entries),
                    ),
                    Self::line(
                        &theme,
                        "Closed while still working (the board's own misjudgement)",
                        format!("{}", friction.early_settles),
                    ),
                    Self::line(
                        &theme,
                        "Ran past their route's cap",
                        format!("{}", friction.overruns),
                    ),
                ],
            )
        };
        column = column.child(Self::card(&theme, "Friction", None, friction_body));

        // ── when in the day ─────────────────────────────────────────────────
        column = column.child(Self::card(
            &theme,
            "When you release work",
            Some("local time".into()),
            Self::render_hours(&stats, &theme),
        ));

        // ── who and what ────────────────────────────────────────────────────
        for (title, tally) in [
            ("By space", &stats.by_workspace),
            ("By runtime", &stats.by_runtime),
            ("By tracker", &stats.by_source),
            ("Whose subscription", &stats.by_account),
        ] {
            if tally.is_empty() {
                continue;
            }
            column = column.child(Self::card(
                &theme,
                title,
                None,
                Self::render_tally(&theme, ranked_top(tally, TALLY_ROWS)),
            ));
        }

        // The number that says whether the herd is releasing its own work.
        column = column.child(Self::card(
            &theme,
            "Released by",
            None,
            Self::lines(
                &theme,
                vec![
                    Self::line(&theme, "An agent", format!("{}", stats.agent_dispatched)),
                    Self::line(
                        &theme,
                        "You",
                        format!("{}", stats.attempts.saturating_sub(stats.agent_dispatched)),
                    ),
                ],
            ),
        ));

        scroll_page(column)
    }
}
