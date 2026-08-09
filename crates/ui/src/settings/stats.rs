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
//! 2. **Work released** — dispatches, where they landed, and the day chart:
//!    bar height is tokens, the dispatch count rides in the caption (gh#226).
//!    Where it landed is one proportional bar over four categories, not a
//!    merge count (gh#228).
//! 3. **Tokens** — what the price above was computed from, per model.
//! 4. **When and where** — one grid crossing the hour against the space.
//! 5. **Breakdown** — the same window cut by model, runtime, space, tracker or
//!    account, with tokens and money on every row (gh#227).
//!
//! **Block five is one card and a toggle, not three columns.** It shipped as
//! three fixed tallies of dispatch counts, which answered the wrong question
//! twice: a count per runtime says nothing about what the runtime cost, and the
//! axis that matters most — which model is the bill — was not on the page at
//! all. The axis a reader wants changes between visits, so it is a control; and
//! since the rows are about money, they are ordered and drawn against it.
//!
//! **Block two draws its outcomes rather than listing them.** `Merged 18 of
//! 19` and `In review 1` were four definition-list rows that could not produce
//! the two facts worth having: a pull request somebody rejected, and an agent
//! that settled having produced nothing. Both hid inside "in review" — the one
//! word a queue of healthy work also reads as. They are the only numbers here
//! that say the board wasted its time, so they are their own hues in one bar
//! and their own rows in its legend, present even at zero (gh#228).
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
    BoardSpend, BoardStats, Breakdown, BreakdownRow, Dimension, HOURS, LandingKind, TokenTally,
    WINDOWS, bar_fraction, day_captions_fit, day_columns, hour_grid, human_minutes, human_multiple,
    human_tokens, peak_tokens, percent, ranked_tokens,
};
use comet_rpc::methods;

use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::{Bed, ListRow as _, Theme};

/// How many rows the per-model table shows before folding the rest into
/// `n others`. The breakdown's own cap is the board's
/// ([`comet_proto::view::stats::BREAKDOWN_ROWS`]), because it folds before it
/// sends.
const TALLY_ROWS: usize = 6;

/// How many spaces the crossed grid shows before folding. Fewer than a tally:
/// a row here is 24 cells wide, and a grid taller than it is legible stops
/// being a shape.
const GRID_ROWS: usize = 5;

/// Bar geometry for the day chart. Tall enough that a quiet day and a busy one
/// are different at a glance, short enough to sit above the fold.
const CHART_HEIGHT: f32 = 92.0;

/// What a day that spent nothing draws instead of a bar (gh#226). One rule, so
/// the quiet days line up as a baseline rather than as six slightly different
/// nothings.
const HAIRLINE: f32 = 1.0;

/// The corner on a drawn mark — a chart column, a meter fill, a heat cell.
///
/// scale-ok: a mark is data, not a box on a surface. Its corner relates to its
/// own 4–6px width, not to the card it sits in, so the three-radius scale
/// (gh#174) does not reach it — but it is still ONE number for every mark on
/// the page, which is the same rule one level down.
const MARK_RADIUS: f32 = 3.0;

/// The label gutter shared by the crossed grid and the breakdown, so the two
/// blocks start their bars on the same vertical.
const LABEL_WIDTH: f32 = 132.0;

/// The landing bar's thickness (gh#228). Thicker than a tally's 6px rule: this
/// one is a proportion to be read, not a row's magnitude to be compared, and
/// four hues at 6px is a stripe nobody can name.
const LANDING_BAR: f32 = 12.0;

/// The floor under a band, so one merge out of ninety is a sliver you can see
/// and point at rather than a rounding error. Bands grow from here, which is
/// also why the bar never overflows its track.
const LANDING_BAND_MIN: f32 = 6.0;

/// The legend's colour chip.
const SWATCH: f32 = 8.0;

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
    /// Which axis the breakdown is cut along (gh#227). A view state, not a
    /// query: every cut arrives in the one reply, so the toggle costs nothing
    /// but a redraw.
    dimension: Dimension,
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
            // The axis where the answer is usually one row, and the one the
            // page could not ask about at all before gh#227.
            dimension: Dimension::Model,
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

    /// Switch the axis the breakdown is cut along. A redraw, never a reload —
    /// all five cuts came down in the same reply.
    fn set_dimension(&mut self, dimension: Dimension, cx: &mut Context<Self>) {
        if self.dimension == dimension {
            return;
        }
        self.dimension = dimension;
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

    /// The track a segmented control's choices sit in.
    ///
    /// One recipe, used by the window picker and by the breakdown's dimension
    /// toggle (gh#227): two segmented controls on one page that were built
    /// twice would be two segmented controls that drift apart.
    fn track(theme: &Theme) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .gap(px(2.0))
            // The nesting rule (gh#174): segments at RADIUS_CHIP inside a track
            // at RADIUS_ROW want exactly one gutter between them, and then the
            // two curves are concentric instead of merely both round.
            .p(px(Theme::NEST_GUTTER))
            .rounded(px(Theme::RADIUS_ROW))
            .border_1()
            .border_color(theme.border)
            .bg(theme.white_alpha(0.02))
    }

    /// One choice in that track.
    ///
    /// The padding follows the type size rather than being a second knob: the
    /// design gives 8×3 around an 11px segment and 10×4 around a 12px one,
    /// which is one rule stated twice.
    fn segment(
        theme: &Theme,
        id: String,
        label: &str,
        size: f32,
        selected: bool,
    ) -> gpui::Stateful<gpui::Div> {
        let (pad_x, pad_y) = match size <= Theme::TEXT_CAPTION {
            true => (8.0, 3.0),
            false => (10.0, 4.0),
        };
        div()
            .id(SharedString::from(id.clone()))
            .px(px(pad_x))
            .py(px(pad_y))
            .rounded(px(Theme::RADIUS_CHIP))
            .text_size(px(size))
            .font_weight(if selected {
                gpui::FontWeight::MEDIUM
            } else {
                gpui::FontWeight::NORMAL
            })
            .when(!selected, |el| el.cursor_pointer())
            // A segmented control inside a settings card: in light mode the
            // card is white, so the chosen segment steps DOWN into it rather
            // than trying to lift off it (gh#175).
            .list_row(theme, Bed::Card, selected, id)
            .child(SharedString::from(label.to_string()))
    }

    /// The window picker: a segmented row, current one filled.
    fn render_windows(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut row = Self::track(theme);
        for (days, label) in WINDOWS {
            let selected = self.since_days == *days;
            let days = *days;
            row = row.child(
                Self::segment(
                    theme,
                    format!("stats-window-{label}"),
                    label,
                    Theme::TEXT_DENSE,
                    selected,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_window(days, cx))),
            );
        }
        row.into_any_element()
    }

    // -- the shared furniture of a block ------------------------------------

    /// A block: heading, an optional aside, and a body.
    fn block(theme: &Theme, title: &str, aside: Option<String>, body: AnyElement) -> AnyElement {
        Self::block_with(theme, title, aside, None, body)
    }

    /// The same block with a control in its header (gh#227).
    ///
    /// The aside moves to the left, under the title's baseline, when there is
    /// one: a caption and a segmented control both pinned to the right edge
    /// would read as one crowded object rather than as a label and a switch.
    fn block_with(
        theme: &Theme,
        title: &str,
        aside: Option<String>,
        control: Option<AnyElement>,
        body: AnyElement,
    ) -> AnyElement {
        let (beside_title, at_the_edge) = match control.is_some() {
            true => (aside, None),
            false => (None, aside),
        };
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
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap(px(8.0))
                            .min_w_0()
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text)
                                    .child(SharedString::from(title.to_string())),
                            )
                            .when_some(beside_title, |el, aside| {
                                el.child(Self::caption(theme, aside))
                            }),
                    )
                    .when_some(at_the_edge, |el, aside| {
                        el.child(Self::caption(theme, aside))
                    })
                    .when_some(control, |el, control| el.child(control)),
            )
            .child(body)
            .into_any_element()
    }

    /// The small subdued line a block heads or ends with.
    fn caption(theme: &Theme, copy: String) -> AnyElement {
        div()
            .min_w_0()
            .truncate()
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(theme.text_subtle)
            .child(SharedString::from(copy))
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
    /// shape of the days it went in — which is a shape in tokens, not in
    /// dispatch counts (gh#226).
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
                    .when_some(Self::render_landing(stats, theme), |el, landing| {
                        el.child(landing).child(Self::seam(theme))
                    })
                    .children(Self::glance_lines(stats, theme)),
            );

        Self::block(
            theme,
            "Work released",
            // The legend only when there are bars to legend — and it says what
            // the height is, because that is the thing a bar chart otherwise
            // lets a reader assume (gh#226).
            (!stats.daily.is_empty()).then(|| "bar height = tokens that day".to_string()),
            Self::body(14.0).child(head).child(split).into_any_element(),
        )
    }

    /// The day chart: **height is tokens, the dispatch count is in the caption**
    /// (gh#226).
    ///
    /// It plotted dispatches, split solid-and-quiet for the share that ended
    /// `done`. Dispatches per day is nearly flat and nearly meaningless — two
    /// of them can differ by twenty times in what they cost — so the shape said
    /// almost nothing, and the one number that does vary was drawn as an
    /// unlabelled strip in another block. Token volume is also what the spend
    /// block above is computed from, so the two now read as one argument. The
    /// completion share it gave up is a sentence in the headline above
    /// (`82% ended in done`), where a proportion belongs.
    ///
    /// Every day in the window draws, including the ones that spent nothing: a
    /// quiet day is a hairline under a dash, never a gap. A seven-day window on
    /// a board that worked one day is a shape; one lonely bar reads as six days
    /// the board failed to record.
    fn render_chart(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let columns = day_columns(&stats.daily, &stats.daily_tokens);
        if columns.is_empty() {
            return Self::note(theme, "Nothing was dispatched in this window.");
        }
        let peak = peak_tokens(&stats.daily_tokens);
        // A month of captions is a month of collisions; past the cap the
        // columns go bare and the axis carries the range instead.
        let captioned = day_captions_fit(columns.len());
        let label = |theme: &Theme, text: String, strong: bool| {
            div()
                .w_full()
                .min_w_0()
                .truncate()
                .text_center()
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(if strong {
                    theme.text_muted
                } else {
                    theme.text_subtle
                })
                .child(SharedString::from(text))
        };
        let bars: Vec<AnyElement> = columns
            .iter()
            .map(|column| {
                // A hairline where the bar would be, in the border's own tone:
                // the day is present and it spent nothing, which is a different
                // statement from a short accent bar somebody has to squint at.
                let height = if column.is_quiet() {
                    HAIRLINE
                } else {
                    (CHART_HEIGHT * column.fraction).max(2.0)
                };
                div()
                    .flex_1()
                    .min_w(px(3.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.0))
                    .when(captioned, |el| {
                        el.child(label(theme, column.value.clone(), !column.is_quiet()))
                    })
                    .child(
                        div()
                            .w_full()
                            .h(px(CHART_HEIGHT))
                            .flex()
                            .flex_col()
                            .justify_end()
                            .child(div().w_full().h(px(height)).rounded(px(MARK_RADIUS)).bg(
                                if column.is_quiet() {
                                    theme.border
                                } else {
                                    theme.accent.opacity(0.85)
                                },
                            )),
                    )
                    .when(captioned, |el| {
                        el.child(label(theme, column.caption.clone(), false))
                    })
                    .into_any_element()
            })
            .collect();

        // The peak reads in tokens, like the bars. With the captions on, the
        // range is already under every column and the axis is that one fact.
        let middle = if peak > 0 {
            format!("peak {}/day", human_tokens(peak))
        } else {
            "no day in this window reported tokens".to_string()
        };
        let (first, last) = if captioned {
            (String::new(), String::new())
        } else {
            (
                columns.first().map(|c| c.date.clone()).unwrap_or_default(),
                columns.last().map(|c| c.date.clone()).unwrap_or_default(),
            )
        };
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
                    .children(bars),
            )
            .child(Self::axis(theme, first, middle, last))
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

    /// **Where the work landed** (gh#228): one proportional bar over the four
    /// places a task ends up, with a legend that names all four and counts
    /// them.
    ///
    /// It was a definition list — `Merged 18 of 19`, `In review 1` — and the
    /// two rows that mattered most were the two it could not produce. A pull
    /// request somebody rejected and an agent that came back with nothing are
    /// the only numbers on this page that say the board wasted its time, and
    /// both were folded into the same "in review" that a queue of healthy work
    /// reads as. So the four categories are drawn as ONE shape, the two losses
    /// have their own hues off the status ramp, and every category is in the
    /// legend even at zero: a reader has to be able to tell a window that lost
    /// nothing from a surface that does not count losses.
    ///
    /// The bar is over tasks that landed. Work still running is a caption
    /// under it and never a band, because a proportion of unfinished work is a
    /// proportion that moves while nothing lands.
    fn render_landing(stats: &BoardStats, theme: &Theme) -> Option<AnyElement> {
        let landing = stats.landing;
        let segments = landing.segments();
        if landing.touched() == 0 {
            return None;
        }

        // A window whose work is all still running has no shape yet: a blank
        // track over four zeroes says less than the caption underneath, which
        // says the whole of it.
        let landed = landing.total() > 0;
        let bar = landed.then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(2.0))
                .w_full()
                .h(px(LANDING_BAR))
                .children(segments.iter().filter(|s| s.count > 0).map(|s| {
                    div()
                        // Grow from the floor rather than sizing to the share,
                        // so a single-task band stays visible and four bands
                        // still fit the track exactly.
                        .flex_grow(s.fraction as f32)
                        .flex_basis(px(0.0))
                        .min_w(px(LANDING_BAND_MIN))
                        .h_full()
                        // scale-ok: a drawn band's own cap — 12px tall, and
                        // its corner belongs to that, not to the card it sits
                        // in (the chart columns beside it are capped the same).
                        .rounded(px(MARK_RADIUS))
                        .bg(Self::landing_tone(theme, s.kind))
                        .into_any_element()
                }))
        });

        // Every category, including the empty ones — see above. Nothing at all
        // landed is the one case with no legend, because four zeroes is not a
        // list of categories, it is the caption written four times.
        let shown = if landed { segments.as_slice() } else { &[] };
        let legend: Vec<AnyElement> = shown
            .iter()
            .map(|s| {
                let empty = s.count == 0;
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(SWATCH))
                            .h(px(SWATCH))
                            // scale-ok: a legend chip is the bar's own mark at 8px;
                            // its corner belongs to those 8px, not to the card.
                            .rounded(px(MARK_RADIUS))
                            // A category with nothing in it keeps its row and
                            // gives up its hue: the count is the fact, and four
                            // lit chips over three real numbers is a bar that
                            // disagrees with its own legend.
                            .bg(if empty {
                                theme.border
                            } else {
                                Self::landing_tone(theme, s.kind)
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(if empty {
                                theme.text_subtle
                            } else {
                                theme.text_muted
                            })
                            .child(SharedString::from(s.label.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(Theme::TEXT_DENSE))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(if empty { theme.text_subtle } else { theme.text })
                            .child(SharedString::from(format!("{}", s.count))),
                    )
                    .into_any_element()
            })
            .collect();

        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(9.0))
                .child(Self::caption(theme, "Where the work landed".into()))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_FIGURE))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.text)
                        .child(SharedString::from(landing.headline())),
                )
                .when_some(bar, |el, bar| el.child(bar))
                .children(legend)
                // What the bar leaves out, said out loud rather than counted
                // as an agent that produced nothing.
                .when_some(landing.in_flight_note(), |el, note| {
                    el.child(Self::caption(theme, note))
                })
                .into_any_element(),
        )
    }

    /// A category's hue, off the status ramp both viewports paint from: merged
    /// is settled, an open pull request is review, a closed one is blocked,
    /// and nothing raised is the working amber. The two losses are the two
    /// loud colours on the page, which is the point of drawing them at all.
    fn landing_tone(theme: &Theme, kind: LandingKind) -> gpui::Hsla {
        match kind {
            LandingKind::Merged => theme.settled,
            LandingKind::Open => theme.accent,
            LandingKind::ClosedUnmerged => theme.danger,
            LandingKind::NoPr => theme.warning,
        }
    }

    /// The facts that qualify the headline — what the work cost in time, and
    /// what it cost in friction. Four cards' worth of numbers as
    /// label-and-value rows: a card per fact is what made this page a scroll.
    /// Where it *landed* is the bar above these (gh#228), not a row among them.
    fn glance_lines(stats: &BoardStats, theme: &Theme) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = vec![Self::line(
            theme,
            "Agent time",
            human_minutes(stats.total_minutes),
        )];
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

        // No day strip here any more (gh#226): the day chart in the block above
        // *is* the token series now, and drawing it twice on one page — once
        // captioned, once as an unlabelled strip — is two answers to one
        // question. What belongs under this headline is what it was spent on.
        Self::block(
            theme,
            "Tokens",
            coverage,
            Self::body(14.0)
                .child(head)
                .child(Self::seam(theme))
                .child(Self::render_model_table(stats, theme))
                .into_any_element(),
        )
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

    // -- block 5: the breakdown ----------------------------------------------

    /// The same window cut five ways, one axis at a time (gh#227).
    ///
    /// This was three fixed columns of dispatch counts, which answered the
    /// wrong question twice: a count per runtime says nothing about what the
    /// runtime cost, and the axis that matters most — the model — was not on
    /// the page at all. So the axis is a toggle, and every row carries the two
    /// numbers a reader is here for: what it spent and what that cost.
    ///
    /// The toggle is built from the cuts the board actually sent, so a
    /// dimension this window has nothing under is absent rather than a segment
    /// that opens onto an empty card.
    fn render_breakdown(
        &self,
        stats: &BoardStats,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        // A window that dropped the chosen axis falls back to the first one it
        // does have, WITHOUT moving the selection: switching to 24 hours and
        // back should return to the axis the reader picked, not to whatever
        // the quiet window happened to offer.
        let cut = stats
            .cut(self.dimension)
            .or_else(|| stats.breakdown.first())?;
        let priced = cut.is_priced();
        let unpriced: u64 = cut.rows.iter().map(|r| r.unpriced_tokens).sum();
        let rows: Vec<AnyElement> = cut
            .rows
            .iter()
            .map(|row| Self::breakdown_row(theme, cut, row, priced))
            .collect();
        let mut body = Self::body(14.0).child(
            div()
                .flex()
                .flex_col()
                // Tighter than a block's own gap: these are rows of one table,
                // not stacked facts.
                .gap(px(9.0))
                .children(rows),
        );
        if priced && unpriced > 0 {
            body = body.child(Self::caption(
                theme,
                format!(
                    "{} token(s) here ran on a model with no rate, and so are in no price above.",
                    human_tokens(unpriced)
                ),
            ));
        }
        Some(Self::block_with(
            theme,
            "Breakdown",
            // What the rows are ordered by, said out loud: the bar is drawn
            // against the same quantity, and on an unpriced window that is
            // tokens rather than money.
            Some(cut.ranking.caption().to_string()),
            Some(Self::render_dimensions(stats, self.dimension, theme, cx)),
            body.into_any_element(),
        ))
    }

    /// The axis toggle: one segment per cut the board sent.
    fn render_dimensions(
        stats: &BoardStats,
        current: Dimension,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let offered: Vec<Dimension> = stats.breakdown.iter().map(|b| b.dimension).collect();
        // The fallback above chooses a cut; the toggle has to agree with it, or
        // a window without the chosen axis would fill one card and highlight a
        // different segment.
        let shown = offered
            .iter()
            .copied()
            .find(|d| *d == current)
            .or_else(|| offered.first().copied());
        let mut row = Self::track(theme);
        for dimension in offered {
            row = row.child(
                Self::segment(
                    theme,
                    format!("stats-dimension-{}", dimension.label()),
                    dimension.label(),
                    Theme::TEXT_CAPTION,
                    Some(dimension) == shown,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_dimension(dimension, cx))),
            );
        }
        row.into_any_element()
    }

    /// One row: what it is, how big a share of the window it is, what it spent,
    /// and what that cost.
    fn breakdown_row(
        theme: &Theme,
        cut: &Breakdown,
        row: &BreakdownRow,
        priced: bool,
    ) -> AnyElement {
        let fraction = cut.share(row);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(
                div()
                    // The gutter the crossed grid uses, so the two blocks start
                    // their bars on the same vertical.
                    .w(px(LABEL_WIDTH))
                    .flex_none()
                    .truncate()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(row.label.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(40.0))
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
            .child(Self::cell(theme, Self::row_tokens(row), 52.0, false))
            .when(priced, |el| {
                el.child(
                    div()
                        .w(px(44.0))
                        .flex_none()
                        .text_right()
                        .truncate()
                        .text_size(px(Theme::TEXT_DENSE))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        // The loudest thing on the row, because it is the one
                        // the card is opened for.
                        .text_color(theme.text)
                        .child(SharedString::from(Self::row_price(row))),
                )
            })
            .into_any_element()
    }

    /// A row's tokens — a dash where it reported none, never a zero that would
    /// read as work that was free.
    fn row_tokens(row: &BreakdownRow) -> String {
        match row.usage.is_zero() {
            true => "—".into(),
            false => human_tokens(row.usage.total()),
        }
    }

    /// A row's money, with the two ways there is none said as a dash: nothing
    /// metered here, and nothing here the rate table could price.
    fn row_price(row: &BreakdownRow) -> String {
        match row.cost {
            Some(cost) if !row.usage.is_zero() && (!cost.is_zero() || row.unpriced_tokens == 0) => {
                human_usd(cost)
            }
            _ => "—".into(),
        }
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
        // ran, what it spent that on, when and where, and the same window cut
        // whichever way the reader wants it.
        column = column
            .child(Self::render_spend(&stats, &theme))
            .child(Self::render_delivery(&stats, &theme))
            .child(Self::render_tokens(&stats, &theme))
            .child(Self::render_when_and_where(&stats, &theme));
        if let Some(breakdown) = self.render_breakdown(&stats, &theme, cx) {
            column = column.child(breakdown);
        }

        scroll_page(column)
    }
}
