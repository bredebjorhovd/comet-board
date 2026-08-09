//! Settings → Board stats (gh#143, rebuilt around spend in gh#179): what the
//! board did with the work it was given, and what that work would have cost.
//!
//! **Five blocks, in the order the questions get asked.** This page was twelve
//! cards down a column, each holding one number and answering nothing in
//! particular; you had to assemble the question yourself out of five tiles and
//! a scroll. What somebody actually opens it for is *what would this work cost
//! at list price, and how far does the subscription carry it* — so that is the
//! headline, and everything under it is evidence:
//!
//! 1. **Spend** — list price against what the plans behind it cost. Never
//!    summed: one figure is the board's, the other is a person's (gh#182).
//! 2. **Work released** — dispatches, the day shape, and where they landed.
//! 3. **Tokens** — what the price above was computed from, per model.
//! 4. **When and where** — one grid crossing the hour against the space.
//! 5. **Who ran it** — the runtime, the tracker, the subscription.
//!
//! **The crossing is the point of block four.** "When do I release work" and
//! "which spaces" were two cards, and the interesting fact — that the evening
//! releases all go to one repo — was in neither of them, because a reader
//! cannot recover a crossing from two margins. So the grid is drawn and the
//! two margins are kept on its edges, where they cost nothing.
//!
//! **Comet never sees your bill.** The rates are list prices (a dated table,
//! `comet_proto::view::rates`), and what a subscription costs is a number the
//! operator entered beside the login in Accounts (gh#178). The page says which
//! is which rather than implying it knows what anybody pays.
//!
//! **Where the numbers come from.** `board.db` lives on whichever device hosts
//! the board, so this page sweeps [`comet_proto::view::board::host_candidates`]
//! for the one that answers `BoardStats` — the same contract the routing page
//! and the board panel use, and the reason a laptop can read the box's
//! throughput without an ssh account on it. Every derivation the *rendering*
//! needs (ordering a tally, scaling a bar, folding a crossing, phrasing a
//! duration or a multiple) is in `comet_proto::view::stats`, so the CLI, this
//! page and anything after it agree on the arithmetic.
//!
//! Every total carries its coverage as the aside, because a figure summed from
//! two of five attempts is not the window's spend, and a window that reported
//! nothing says so instead of drawing zeroes.
//!
//! Descriptive, like the CLI it shares a source with: it reports what
//! happened. Nothing here grades the operator.

use gpui::{AnyElement, Context, Entity, SharedString, Task, Window, div, prelude::*, px};

use comet_proto::TokenUsage;
use comet_proto::view::board;
use comet_proto::view::rates::{Usd, human_usd};
use comet_proto::view::stats::{
    BoardSpend, BoardStats, HOURS, Tally, TokenTally, WINDOWS, bar_fraction, hour_grid,
    human_minutes, human_multiple, human_tokens, peak_dispatches, peak_tokens, percent,
    ranked_tokens, ranked_top,
};
use comet_rpc::methods;

use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::{Bed, ListRow as _, Theme};

/// How many rows a tally shows before folding the rest into `n others`.
const TALLY_ROWS: usize = 6;

/// How many spaces the crossed grid shows before folding. Fewer than a tally:
/// a row here is 24 cells wide, and a grid taller than it is legible stops
/// being a shape.
const GRID_ROWS: usize = 5;

/// Bar geometry for the throughput chart. Tall enough that a quiet day and a
/// busy one are different at a glance, short enough to sit above the fold.
const CHART_HEIGHT: f32 = 92.0;

/// The token series, drawn as a strip inside the tokens block rather than as a
/// card of its own — it qualifies the total above it.
const SPARK_HEIGHT: f32 = 40.0;

/// The corner on a drawn mark — a chart column, a meter fill, a heat cell.
///
/// scale-ok: a mark is data, not a box on a surface. Its corner relates to its
/// own 4–6px width, not to the card it sits in, so the three-radius scale
/// (gh#174) does not reach it — but it is still ONE number for every mark on
/// the page, which is the same rule one level down.
const MARK_RADIUS: f32 = 3.0;

/// The label gutter shared by the crossed grid and the tallies, so the two
/// blocks start their bars on the same vertical.
const LABEL_WIDTH: f32 = 132.0;

/// One row of the per-model table: what ran, what it spent, and — when the
/// board could price it — what that would have cost.
///
/// `cost: None` is *unpriced*, never zero. It is the same rule the money type
/// itself is built on (gh#182): a model the rate table has never heard of
/// carries its tokens through to the table and reads as a dash, so the rows
/// still add up to the totals on the same block.
struct ModelRow {
    label: String,
    /// The provenance a reader who distrusts the figure asks for first: the
    /// family it priced off, or the config that overrode the built-in rate.
    note: Option<String>,
    usage: TokenUsage,
    cost: Option<Usd>,
}

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

    // -- the shared furniture of a block ------------------------------------

    /// A block: heading, an optional aside, and a body.
    fn block(theme: &Theme, title: &str, aside: Option<String>, body: AnyElement) -> AnyElement {
        widgets::section_card(theme)
            // `section_card` carries the settings stack's own `mt(24)`, which
            // in this column double-spaces on top of `dashboard_column`'s gap.
            // Spacing is the column's job here.
            .mt(px(0.0))
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
                                .min_w_0()
                                .truncate()
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(theme.text_subtle)
                                .child(SharedString::from(aside)),
                        )
                    }),
            )
            .child(body)
            .into_any_element()
    }

    /// The padded interior every block body shares.
    fn body(gap: f32) -> gpui::Div {
        div()
            .px(px(20.0))
            .py(px(14.0))
            .flex()
            .flex_col()
            .gap(px(gap))
    }

    /// A hairline between two halves of one block — the seam that used to be a
    /// second card.
    fn seam(theme: &Theme) -> AnyElement {
        // `flex_none`, or the one-pixel rule is the first thing a crowded
        // column shrinks away and the seam silently disappears.
        div()
            .flex_none()
            .h(px(1.0))
            .bg(theme.border)
            .into_any_element()
    }

    /// One headline number and what it is, unboxed: inside a block, a border
    /// around a figure is a card inside a card.
    fn figure(theme: &Theme, value: impl Into<SharedString>, caption: &str) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(3.0))
            .min_w_0()
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
                    .child(SharedString::from(caption.to_string())),
            )
            .into_any_element()
    }

    /// A sentence inside a block — the honest notes, and the empty states.
    fn note(theme: &Theme, copy: impl Into<SharedString>) -> AnyElement {
        div()
            .text_size(px(Theme::TEXT_DENSE))
            .text_color(theme.text_subtle)
            .child(copy.into())
            .into_any_element()
    }

    /// A plain `label — value` line inside a block.
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

    /// A right-aligned fixed-width table cell.
    fn cell(theme: &Theme, text: String, width: f32, head: bool) -> AnyElement {
        div()
            .w(px(width))
            .flex_none()
            .text_right()
            .truncate()
            .text_size(px(Theme::TEXT_DENSE))
            .text_color(if head {
                theme.text_subtle
            } else {
                theme.text_muted
            })
            .child(SharedString::from(text))
            .into_any_element()
    }

    // -- block 1: spend ------------------------------------------------------

    /// What the window would have cost at the meter, against what the plans
    /// behind it cost — the question the page is opened for.
    ///
    /// The two figures are never added. One is the board's own arithmetic over
    /// tokens it counted; the other is a number a person typed beside a login
    /// (gh#178), possibly several people's on a box carrying several slots. The
    /// only honest thing to do with the pair is divide it, which is the
    /// multiple beside them.
    fn render_spend(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let priced = stats.spend.as_ref().filter(|s| s.has_price());
        let aside = stats.spend.as_ref().map(|s| s.rates.provenance());
        let Some(spend) = priced else {
            // The two ways there is no number, said apart — `spend_label` owns
            // which one this is, and neither of them is $0.00.
            let mut body = Self::body(8.0).child(Self::note(
                theme,
                format!(
                    "No price for the {}: {}.",
                    stats.window_label(),
                    stats.spend_label()
                ),
            ));
            if stats.spend.is_none() {
                body = body.child(Self::note(
                    theme,
                    "Model rates live in routing.toml under [defaults.rates]; the built-in \
                     list prices are used when it names none.",
                ));
            }
            return Self::block(
                theme,
                "Spend",
                aside,
                body.child(Self::note(theme, Self::BILL_NOTE))
                    .into_any_element(),
            );
        };

        let window = stats.window_label();
        let mut figures = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .gap(px(32.0))
            .child(Self::figure(
                theme,
                human_usd(spend.list_price),
                &format!("at list price, {window}"),
            ));
        match spend.subscriptions_in_window() {
            Some(plans) => {
                figures = figures.child(Self::figure(
                    theme,
                    human_usd(plans),
                    &format!(
                        "of subscription over the same {window} ({}/mo)",
                        human_usd(spend.monthly_subscriptions())
                    ),
                ));
                if let Some(ratio) = spend.subsidy() {
                    figures = figures.child(Self::figure(
                        theme,
                        human_multiple(ratio),
                        "list price per plan dollar",
                    ));
                }
            }
            // Two different "no", and collapsing them would be the mistake
            // this whole half of the page is built to avoid. An all-time
            // window has plans and no days to pro-rate them onto; a board with
            // nothing entered has no second figure at all — and unentered is
            // not free.
            None => {
                let monthly = spend.monthly_subscriptions();
                figures = figures.child(if monthly.is_zero() {
                    Self::figure(theme, "—", "no plan cost entered in Accounts")
                } else {
                    Self::figure(
                        theme,
                        format!("{}/mo", human_usd(monthly)),
                        "of subscription — all time has no window to pro-rate onto",
                    )
                });
            }
        }

        let mut body = Self::body(14.0)
            .child(figures)
            .child(Self::note(theme, Self::BILL_NOTE));
        if !spend.is_complete() {
            body = body.child(Self::note(
                theme,
                format!(
                    "Not in that total: {} token(s) on {} model(s) with no rate ({}).",
                    human_tokens(spend.unpriced_tokens),
                    spend.unpriced.len(),
                    spend
                        .unpriced
                        .iter()
                        .map(|t| t.label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        if !spend.accounts.is_empty() {
            body = body
                .child(Self::seam(theme))
                .child(Self::render_accounts(spend, theme));
        }
        Self::block(theme, "Spend", aside, body.into_any_element())
    }

    /// The sentence that keeps the block from being read as a bill.
    const BILL_NOTE: &'static str = "Comet never sees your bill. The list price is what these tokens cost at the meter; \
         the subscription is what you entered beside each login in Accounts.";

    /// Per subscription: what it ran, what that was worth at the meter, and
    /// what the plan behind it costs.
    fn render_accounts(spend: &BoardSpend, theme: &Theme) -> AnyElement {
        let name = |theme: &Theme, text: String, head: bool| {
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(Theme::TEXT_DENSE))
                .text_color(if head { theme.text_subtle } else { theme.text })
                .child(SharedString::from(text))
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(name(theme, "Subscription".into(), true))
            .child(Self::cell(theme, "Dispatches".into(), 72.0, true))
            .child(Self::cell(theme, "Tokens".into(), 68.0, true))
            .child(Self::cell(theme, "List price".into(), 78.0, true))
            .child(Self::cell(theme, "Plan".into(), 92.0, true))
            .child(Self::cell(theme, "Multiple".into(), 64.0, true));
        let rows: Vec<AnyElement> = spend
            .accounts
            .iter()
            .map(|account| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(name(theme, account.label.clone(), false))
                    .child(Self::cell(
                        theme,
                        format!("{}", account.attempts),
                        72.0,
                        false,
                    ))
                    .child(Self::cell(
                        theme,
                        human_tokens(account.usage.total()),
                        68.0,
                        false,
                    ))
                    .child(
                        div()
                            .w(px(78.0))
                            .flex_none()
                            .text_right()
                            .text_size(px(Theme::TEXT_DENSE))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(human_usd(account.list_price))),
                    )
                    .child(Self::cell(
                        theme,
                        // A plan nobody entered says so. `$0.00/mo` would be a
                        // claim about somebody's bill that comet cannot make.
                        match &account.plan {
                            Some(plan) => format!("{}/mo", human_usd(plan.monthly)),
                            None => "not entered".into(),
                        },
                        92.0,
                        false,
                    ))
                    .child(Self::cell(
                        theme,
                        account
                            .subsidy()
                            .map(human_multiple)
                            .unwrap_or_else(|| "—".into()),
                        64.0,
                        false,
                    ))
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(9.0))
            .child(header)
            .children(rows)
            .into_any_element()
    }

    // -- block 2: work released ----------------------------------------------

    /// The throughput answer: how much was released, how it went, and the
    /// shape of the days it went in.
    fn render_delivery(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let completion = percent(stats.completion_rate);
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
        if let Some(median) = stats.median_minutes.map(human_minutes) {
            facts.push(format!("{median} median"));
        }
        if stats.live > 0 {
            facts.push(format!("{} running now", stats.live));
        }

        let head = div()
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
            );

        let split = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_start()
            .gap(px(24.0))
            .child(
                div()
                    .flex_grow(3.0)
                    .flex_basis(px(0.0))
                    .min_w(px(320.0))
                    .child(Self::render_chart(stats, theme)),
            )
            .child(
                div()
                    .flex_grow(2.0)
                    .flex_basis(px(0.0))
                    .min_w(px(260.0))
                    .flex()
                    .flex_col()
                    .gap(px(9.0))
                    .children(Self::glance_lines(stats, theme)),
            );

        Self::block(
            theme,
            "Work released",
            // The legend only when there are bars to legend.
            (peak_dispatches(&stats.daily) > 0).then(|| "solid bar = ended in done".to_string()),
            Self::body(14.0).child(head).child(split).into_any_element(),
        )
    }

    /// Dispatches per day, with the share that ended `done` filled in.
    ///
    /// Two tones in one bar rather than two bars: the question is what
    /// proportion of a day's work landed, and side-by-side bars make that a
    /// subtraction the reader has to do.
    fn render_chart(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let peak = peak_dispatches(&stats.daily);
        if peak == 0 {
            return Self::note(theme, "Nothing was dispatched in this window.");
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
            .child(Self::axis(theme, first, format!("peak {peak}/day"), last))
            .into_any_element()
    }

    /// The caption under a day series: where it starts, its peak, where it ends.
    fn axis(theme: &Theme, first: String, middle: String, last: String) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(theme.text_subtle)
            .child(SharedString::from(first))
            .child(SharedString::from(middle))
            .child(SharedString::from(last))
            .into_any_element()
    }

    /// The facts that qualify the headline — where the work landed, what it
    /// cost in friction, and who released it. Four cards' worth of numbers as
    /// label-and-value rows: a card per fact is what made this page a scroll.
    fn glance_lines(stats: &BoardStats, theme: &Theme) -> Vec<AnyElement> {
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
                rows.push(Self::line(
                    theme,
                    "No pull request",
                    format!("{}", landing.no_pr),
                ));
            }
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
                rows.push(Self::line(
                    theme,
                    "Retried",
                    format!("{}", friction.retried_tasks),
                ));
            }
            if friction.blocked_entries > 0 {
                rows.push(Self::line(
                    theme,
                    "Stopped to ask",
                    format!("{}", friction.blocked_entries),
                ));
            }
            if friction.early_settles > 0 {
                rows.push(Self::line(
                    theme,
                    "Closed while working",
                    format!("{}", friction.early_settles),
                ));
            }
            if friction.overruns > 0 {
                rows.push(Self::line(
                    theme,
                    "Past their cap",
                    format!("{}", friction.overruns),
                ));
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
        rows
    }

    // -- block 3: tokens -----------------------------------------------------

    /// What the price was computed from: the totals, the day shape, and the
    /// per-model table with a cost column when the board could price it.
    fn render_tokens(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let coverage = Self::coverage_note(stats);
        if !stats.has_tokens() {
            // Never a wall of zeroes: nothing reported is a fact about the
            // board's records, not about what the work cost.
            return Self::block(
                theme,
                "Tokens",
                coverage,
                Self::body(8.0)
                    .child(Self::note(
                        theme,
                        "No attempt in this window reported token usage. Attempts from before \
                         the board recorded it stay blank rather than reading as free.",
                    ))
                    .into_any_element(),
            );
        }
        let tokens: TokenUsage = stats.tokens;
        let head = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_end()
            .justify_between()
            .gap(px(16.0))
            .child(Self::figure(
                theme,
                human_tokens(tokens.total()),
                "tokens processed",
            ))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_baseline()
                    .gap(px(14.0))
                    .children(
                        [
                            ("uncached", tokens.input_tokens),
                            ("cached", tokens.cache_read_tokens),
                            ("writes", tokens.cache_creation_tokens),
                            ("output", tokens.output_tokens),
                        ]
                        .into_iter()
                        .map(|(label, count)| {
                            div()
                                .flex()
                                .flex_row()
                                .items_baseline()
                                .gap(px(5.0))
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_DENSE))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text)
                                        .child(SharedString::from(human_tokens(count))),
                                )
                                .child(
                                    div()
                                        .text_size(px(Theme::TEXT_CAPTION))
                                        .text_color(theme.text_subtle)
                                        .child(SharedString::from(label)),
                                )
                                .into_any_element()
                        })
                        .collect::<Vec<_>>(),
                    ),
            );

        Self::block(
            theme,
            "Tokens",
            coverage,
            Self::body(14.0)
                .child(head)
                .child(Self::render_spark(stats, theme))
                .child(Self::seam(theme))
                .child(Self::render_model_table(stats, theme))
                .into_any_element(),
        )
    }

    /// Tokens per day, one tone.
    ///
    /// The dispatch chart in the block above splits its bars because "how much
    /// of a day's work landed" is a proportion; this one does not, because
    /// tokens have no such second number. What it does share is the day range —
    /// the two series are generated from one calendar, so a spike here sits
    /// under the day that caused it.
    fn render_spark(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let peak = peak_tokens(&stats.daily_tokens);
        if peak == 0 {
            return Self::note(theme, "No day in this window has usage to show.");
        }
        let bars: Vec<AnyElement> = stats
            .daily_tokens
            .iter()
            .map(|day| {
                let total = day.usage.total();
                // Against the peak day and not the total, for the reason the
                // dispatch chart is: the question is which day was expensive.
                let fraction = (total as f32 / peak as f32).clamp(0.0, 1.0);
                div()
                    .flex_1()
                    .min_w(px(3.0))
                    .h(px(SPARK_HEIGHT))
                    .flex()
                    .flex_col()
                    .justify_end()
                    .child(
                        div()
                            .w_full()
                            .h(px(
                                SPARK_HEIGHT * fraction + if total > 0 { 2.0 } else { 0.0 }
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
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(3.0))
                    .h(px(SPARK_HEIGHT))
                    .children(bars),
            )
            .child(Self::axis(
                theme,
                first,
                format!("peak {}/day", human_tokens(peak)),
                last,
            ))
            .into_any_element()
    }

    /// The per-model rows, priced where the board could price them.
    ///
    /// Ordered by cost when there is one, because this table now sits under a
    /// money headline and the row a reader is looking for is the expensive one.
    /// Unpriced models are one honest row at the bottom rather than being
    /// dropped: a breakdown that silently omitted them would not add up to the
    /// token total directly above it.
    fn model_rows(stats: &BoardStats) -> Vec<ModelRow> {
        let Some(spend) = stats.spend.as_ref().filter(|s| s.has_price()) else {
            return ranked_tokens(&stats.tokens_by_model, TALLY_ROWS)
                .into_iter()
                .map(|row| ModelRow {
                    label: row.label,
                    note: None,
                    usage: row.usage,
                    cost: None,
                })
                .collect();
        };
        let mut rows: Vec<ModelRow> = spend
            .by_model
            .iter()
            .take(TALLY_ROWS)
            .map(|model| ModelRow {
                label: model.label.clone(),
                // Provenance, and only when it is news: an exact hit off the
                // built-in table is the default and says nothing.
                note: match (
                    model.rate_key != model.label,
                    model.source == comet_proto::view::rates::RateSource::Config,
                ) {
                    (_, true) => Some(format!("rate from routing.toml ({})", model.rate_key)),
                    (true, false) => Some(format!("priced as {}", model.rate_key)),
                    (false, false) => None,
                },
                usage: model.usage,
                cost: Some(model.cost),
            })
            .collect();
        if spend.by_model.len() > TALLY_ROWS {
            let tail = &spend.by_model[TALLY_ROWS..];
            rows.push(ModelRow {
                label: format!("{} others", tail.len()),
                note: None,
                usage: tail.iter().map(|m| m.usage).sum(),
                cost: Some(tail.iter().map(|m| m.cost).sum()),
            });
        }
        if !spend.unpriced.is_empty() {
            let folded: Vec<&TokenTally> = spend.unpriced.iter().collect();
            rows.push(ModelRow {
                label: match folded.as_slice() {
                    [only] => only.label.clone(),
                    many => format!("{} models with no rate", many.len()),
                },
                note: Some("no rate, so not in the total above".into()),
                usage: folded.iter().map(|t| t.usage).sum(),
                cost: None,
            });
        }
        rows
    }

    /// The per-model table: where the tokens went, into which bucket, and what
    /// that cost.
    ///
    /// A table rather than bars, because the interesting comparison here is
    /// across *columns* — a model whose input is almost all cache reads is a
    /// different fact from one whose input is fresh every turn, and it is the
    /// whole reason the four buckets are priced apart.
    fn render_model_table(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let rows = Self::model_rows(stats);
        let priced = rows.iter().any(|r| r.cost.is_some());
        let header = div()
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
                    .text_color(theme.text_subtle)
                    .child(SharedString::from("Model")),
            )
            .child(Self::cell(theme, "Uncached".into(), 68.0, true))
            .child(Self::cell(theme, "Cached".into(), 68.0, true))
            .child(Self::cell(theme, "Writes".into(), 62.0, true))
            .child(Self::cell(theme, "Output".into(), 62.0, true))
            .child(Self::cell(theme, "Tokens".into(), 68.0, true))
            .when(priced, |el| {
                el.child(Self::cell(theme, "Cost".into(), 76.0, true))
            });
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
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(Theme::TEXT_DENSE))
                                    .text_color(theme.text)
                                    .child(SharedString::from(row.label)),
                            )
                            .when_some(row.note, |el, note| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(px(Theme::TEXT_CAPTION))
                                        .text_color(theme.text_subtle)
                                        .child(SharedString::from(note)),
                                )
                            }),
                    )
                    .child(Self::cell(
                        theme,
                        human_tokens(row.usage.input_tokens),
                        68.0,
                        false,
                    ))
                    .child(Self::cell(
                        theme,
                        human_tokens(row.usage.cache_read_tokens),
                        68.0,
                        false,
                    ))
                    .child(Self::cell(
                        theme,
                        human_tokens(row.usage.cache_creation_tokens),
                        62.0,
                        false,
                    ))
                    .child(Self::cell(
                        theme,
                        human_tokens(row.usage.output_tokens),
                        62.0,
                        false,
                    ))
                    .child(
                        div()
                            .w(px(68.0))
                            .flex_none()
                            .text_right()
                            .text_size(px(Theme::TEXT_DENSE))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(human_tokens(row.usage.total()))),
                    )
                    .when(priced, |el| {
                        el.child(
                            div()
                                .w(px(76.0))
                                .flex_none()
                                .text_right()
                                .text_size(px(Theme::TEXT_DENSE))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                // A dash, never $0.00 — the one thing the money
                                // half of this page exists to avoid printing.
                                .child(SharedString::from(
                                    row.cost.map(human_usd).unwrap_or_else(|| "—".into()),
                                )),
                        )
                    })
                    .into_any_element()
            })
            .collect();
        div()
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
    /// It rides on the token and spend blocks as the aside, not in a panel of
    /// its own, because a total read without it is a total read wrong: attempts
    /// that predate the recording, and harnesses that meter nothing, are simply
    /// absent from the sums. An honest "62% of attempts reported usage" is
    /// worth more than a figure that quietly under-reports.
    fn coverage_note(stats: &BoardStats) -> Option<String> {
        let share = percent(stats.token_coverage)?;
        Some(format!(
            "{share} of attempts reported usage ({} of {})",
            stats.attempts_with_tokens, stats.attempts
        ))
    }

    // -- block 4: when and where ---------------------------------------------

    /// The crossing (gh#179): every space against every hour of the box's
    /// local day, with the two histograms it replaced kept as its margins.
    ///
    /// These were two cards. A reader looking at an hour card and a space card
    /// can tell that the board runs late and that one repo takes most of the
    /// work, and cannot tell whether those are the same fact — which is the
    /// only thing either card was ever going to be used for.
    fn render_when_and_where(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let grid = hour_grid(&stats.hours_by_workspace, GRID_ROWS);
        let body = if grid.is_empty() {
            // A board older than the crossing answers without it (the field is
            // defaulted on the wire). Its hour margin is still real, so the
            // block degrades to the histogram rather than to nothing.
            let margin: usize = stats.hour_of_day.iter().sum();
            if margin == 0 {
                Self::body(8.0)
                    .child(Self::note(theme, "Nothing was dispatched in this window."))
                    .into_any_element()
            } else {
                Self::body(10.0)
                    .child(Self::hour_margin(theme, &stats.hour_of_day, None))
                    .child(Self::hour_labels(theme))
                    .child(Self::note(
                        theme,
                        "This board reports hours without the space they went to, so the \
                         crossing is not drawn. Update the box to see it.",
                    ))
                    .into_any_element()
            }
        } else {
            let peak = grid.peak;
            let rows: Vec<AnyElement> = grid
                .rows
                .iter()
                .map(|row| {
                    let cells: Vec<AnyElement> = row
                        .hours
                        .iter()
                        .map(|count| {
                            let heat = bar_fraction(*count, peak);
                            div()
                                .flex_1()
                                .min_w(px(6.0))
                                .h(px(20.0))
                                .rounded(px(MARK_RADIUS))
                                .bg(if *count == 0 {
                                    theme.white_alpha(0.03)
                                } else {
                                    theme.accent.opacity(0.15 + 0.7 * heat)
                                })
                                .into_any_element()
                        })
                        .collect();
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(10.0))
                        .child(
                            div()
                                .w(px(LABEL_WIDTH))
                                .flex_none()
                                .truncate()
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(theme.text)
                                .child(SharedString::from(row.label.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_row()
                                .gap(px(2.0))
                                .children(cells),
                        )
                        .child(Self::cell(theme, format!("{}", row.total), 34.0, false))
                        .into_any_element()
                })
                .collect();
            Self::body(10.0)
                .child(div().flex().flex_col().gap(px(3.0)).children(rows))
                .child(Self::hour_margin(theme, &grid.columns, Some(grid.total)))
                .child(Self::hour_labels(theme))
                .into_any_element()
        };
        Self::block(
            theme,
            "When and where",
            Some("dispatches, local time".into()),
            body,
        )
    }

    /// The bottom margin: dispatches per hour, summed across every space —
    /// the old histogram, in the one place it is still worth having.
    fn hour_margin(theme: &Theme, hours: &[usize], total: Option<usize>) -> AnyElement {
        let peak = hours.iter().copied().max().unwrap_or(0);
        let bars: Vec<AnyElement> = hours
            .iter()
            .map(|count| {
                let fraction = bar_fraction(*count, peak);
                div()
                    .flex_1()
                    .min_w(px(6.0))
                    .h(px(18.0))
                    .flex()
                    .flex_col()
                    .justify_end()
                    .child(
                        div()
                            .w_full()
                            .h(px(18.0 * fraction + if *count > 0 { 2.0 } else { 0.0 }))
                            .rounded(px(MARK_RADIUS))
                            .bg(theme.accent.opacity(0.55)),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(10.0))
            .child(
                div()
                    .w(px(LABEL_WIDTH))
                    .flex_none()
                    .truncate()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from("every space")),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(2.0))
                    .children(bars),
            )
            .child(Self::cell(
                theme,
                total.map(|t| format!("{t}")).unwrap_or_default(),
                34.0,
                false,
            ))
            .into_any_element()
    }

    /// The hour axis. Every third hour is labelled: 24 numbers at this width is
    /// a smear, and the reader is looking for a shape.
    fn hour_labels(theme: &Theme) -> AnyElement {
        let labels: Vec<AnyElement> = (0..HOURS)
            .map(|hour| {
                div()
                    .flex_1()
                    .min_w(px(6.0))
                    .text_center()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(if hour % 3 == 0 {
                        format!("{hour:02}")
                    } else {
                        String::new()
                    }))
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(div().w(px(LABEL_WIDTH)).flex_none())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .gap(px(2.0))
                    .children(labels),
            )
            .child(div().w(px(34.0)).flex_none())
            .into_any_element()
    }

    // -- block 5: who ran it -------------------------------------------------

    /// The three remaining single-axis tallies, side by side in one block:
    /// which harness, which tracker, whose subscription.
    fn render_who(stats: &BoardStats, theme: &Theme) -> Option<AnyElement> {
        let columns: Vec<AnyElement> = [
            ("Runtime", &stats.by_runtime),
            ("Tracker", &stats.by_source),
            ("Subscription", &stats.by_account),
        ]
        .into_iter()
        .filter(|(_, tally)| !tally.is_empty())
        .map(|(heading, tally)| Self::tally_column(theme, heading, ranked_top(tally, TALLY_ROWS)))
        .collect();
        if columns.is_empty() {
            return None;
        }
        Some(Self::block(
            theme,
            "Who ran it",
            None,
            Self::body(14.0)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .items_start()
                        .gap(px(24.0))
                        .children(columns),
                )
                .into_any_element(),
        ))
    }

    /// One tally as a headed column of label + bar + count.
    fn tally_column(theme: &Theme, heading: &str, rows: Vec<Tally>) -> AnyElement {
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
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text)
                            .child(SharedString::from(row.label)),
                    )
                    .child(
                        div()
                            .w(px(64.0))
                            .flex_none()
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
                    .child(Self::cell(theme, format!("{}", row.count), 28.0, false))
                    .into_any_element()
            })
            .collect();
        div()
            .flex_grow(1.0)
            .flex_basis(px(0.0))
            .min_w(px(240.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(heading)),
            )
            .children(children)
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
        // The wide column the shared layer declares (gh#178) — this page is a
        // dashboard, and picking the width is a choice between two named ones
        // rather than a private copy of the column with a note attached.
        let mut column = widgets::dashboard_column().child(
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
            column = column.child(
                div()
                    .mt(px(20.0))
                    .child(widgets::error_strip(&theme, error)),
            );
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

        // Five blocks, and the order is the argument: what it cost, what it
        // ran, what it spent that on, when and where, and who.
        column = column
            .child(Self::render_spend(&stats, &theme))
            .child(Self::render_delivery(&stats, &theme))
            .child(Self::render_tokens(&stats, &theme))
            .child(Self::render_when_and_where(&stats, &theme));
        if let Some(who) = Self::render_who(&stats, &theme) {
            column = column.child(who);
        }

        scroll_page(column)
    }
}
