//! Settings → Board stats (gh#143, rebuilt around spend in gh#179): what the
//! board did with the work it was given, and what that work would have cost.
//!
//! **Five cards in four rows, and the composition is the deliverable
//! (gh#252).** gh#225–gh#228 each built their element correctly and dropped it
//! into the container that was already here, and nobody restructured the page;
//! four accurate pull requests later the running app and
//! `Comet Stats Window.dc.html` were not the same screen. This module is now
//! built from the design file itself, read through `DesignSync`, and the
//! measurements above are its numbers rather than a description of them.
//!
//! What somebody opens this page for is *what would this work cost at list
//! price, and how far does the subscription carry it* — so that is the
//! headline, and everything under it is evidence:
//!
//! 1. **Spend** — list price, the multiple the subscription buys, and where
//!    inside that price the money went, as three cells of one band over a
//!    footer that says what the figures are not. The first two are never
//!    summed: one is the board's arithmetic, the other is a person's (gh#182).
//!    The card carries no title, because the figures are the page's.
//! 2. **Tokens and dispatches per day** — its own card, not a column of one
//!    (gh#252). Bar height is tokens, the dispatch count rides in the caption
//!    (gh#226), and a window where no day reported tokens draws **no band at
//!    all** rather than reserving one for a row of em dashes.
//! 3. **When you release work, and where** — one grid crossing the hour
//!    against the space.
//! 4. **Breakdown**, at `flex:3` — the same window cut by model, runtime,
//!    space, tracker or account, with tokens and money on every row (gh#227).
//! 5. **Where the work landed**, at `flex:2` beside it — one proportional bar
//!    over the four places a task ends up (gh#228).
//!
//! **Cards four and five are one row.** The page ends in a crossing of the two
//! ways of asking what came of the work, not in a stack that puts the only
//! numbers about wasted effort three blocks above the fold.
//!
//! **Two cards this page used to have are gone**, because the design has no
//! room for a question answered twice. *Work released* was a headline over the
//! day chart and the outcomes bar, which are cards two and five; its remaining
//! facts — duration, friction, who dispatched — ride under the outcomes they
//! qualify. *Tokens* was a per-model table, which is card four's first segment
//! drawn one card higher; its totals are the spend band's caption and its four
//! classes are the split's legend.
//!
//! **Every mark is ink, not accent.** A chart column, a heat cell, a meter
//! fill and a token-class chip are all [`StatsPage::mark`] — the text colour at
//! a fraction of itself. The four status hues appear once, in the outcomes bar,
//! where they name states (gh#173).
//!
//! **The four rates are priced apart and then read together (gh#225).** A
//! cache read is a tenth of fresh input and output is five times it, so the
//! window's price is nothing like its token counts — and the block that used
//! to give a whole card to one sentence now spends the room on the split: a
//! proportional bar over output, cache writes, cached input and uncached
//! input, with what each one cost and what it bought. Thirty-five million
//! cached tokens for a fifth of what a million output tokens cost is the whole
//! story of where the money goes, and no other view of these numbers tells it.
//!
//! **Card five draws its outcomes rather than listing them.** `Merged 18 of
//! 19` and `In review 1` were four definition-list rows that could not produce
//! the two facts worth having: a pull request somebody rejected, and an agent
//! that settled having produced nothing. Both hid inside "in review" — the one
//! word a queue of healthy work also reads as. They are the only numbers here
//! that say the board wasted its time, so the rejection has its own hue in one
//! bar and every category has a row in the legend, present even at zero
//! (gh#228).
//!
//! **The crossing is the point of card three.** "When do I release work" and
//! "which spaces" were two cards, and the interesting fact — that the evening
//! releases all go to one repo — was in neither of them, because a reader
//! cannot recover a crossing from two margins. So the grid is drawn, with the
//! per-space totals on its right edge where they cost nothing.
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
//! **Which board, said as a control rather than as a fact (gh#254).** The sweep
//! settles on whichever candidate answers first, and on a machine running Comet
//! beside the box that is the local one — §gh#195's "two boards and neither
//! knew", reached by a page instead of by the doctor. Read on the operator's
//! own Mac the headline figure was permanently blank, because the local board's
//! nineteen attempts all predate token capture while the box's twelve hold
//! 46.1M tokens, and nothing on screen said a second board existed.
//!
//! So two things changed. The sweep asks **every** candidate rather than
//! stopping at the first answer, and the boards it finds are a segmented
//! control in the header beside the window picker — the sweep's answer
//! selected, the choice remembered in `stats-prefs.json` (a page that had to be
//! re-pointed every launch is the same bug with a click in it). And an empty
//! board says *why*: `nothing metered to price` is true and useless, so
//! [`comet_proto::view::stats::other_boards_note`] writes both halves — what
//! this board holds, and what the others do. The page could always see the
//! other candidates; it just never mentioned them.
//!
//! The spend card carries its coverage in the footer, because a figure summed
//! from two of five attempts is not the window's spend, and a window that
//! reported nothing says so instead of drawing zeroes.
//!
//! **What is not here, and where it went instead.** The per-subscription table
//! that used to sit under the spend band is the Breakdown's `Account` segment
//! plus the Accounts page, and it cost this card three hundred pixels to say a
//! third time. Same rule as the two deleted cards: on a page whose whole
//! complaint was that it needed three screens, an answer already one toggle
//! away is not worth a second drawing of it.
//!
//! Descriptive, like the CLI it shares a source with: it reports what
//! happened. Nothing here grades the operator.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui::{AnyElement, Context, Entity, SharedString, Task, Window, div, prelude::*, px};
use serde::{Deserialize, Serialize};

use comet_proto::view::rates::human_usd;
use comet_proto::view::stats::{
    AgentSpend, AggregateBoardStats, BREAKDOWN_ROWS, BoardSpend, BoardStats, Breakdown,
    BreakdownRow, CostSplit, Dimension, HostBoard, LandingKind, WINDOWS, bar_fraction,
    day_captions_fit, day_columns, elsewhere_note, hour_grid, human_minutes, human_multiple,
    human_tokens, other_boards_note, peak_tokens, percent,
};
use comet_rpc::methods;

use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

// ---- the design's measurements (`Comet Stats Window.dc.html`) --------------
//
// Read off the design file rather than described from it (gh#252). The four
// pull requests before this one each built their element correctly and put it
// in the old container, because prose about a layout is not a layout.

/// How many spaces the crossed grid shows before folding. A row here is 24
/// cells wide, and a grid taller than it is legible stops being a shape.
const GRID_ROWS: usize = 5;

/// A card's interior: 20px in from the edges, 16 above and 14 below, with 12
/// between whatever is stacked inside it.
const CARD_PAD_X: f32 = 20.0;
const CARD_PAD_TOP: f32 = 16.0;
const CARD_PAD_BOTTOM: f32 = 14.0;
const CARD_GAP: f32 = 12.0;

/// Between the cards themselves, and between the two halves of the bottom row.
const PAGE_GAP: f32 = 12.0;

/// The day chart's own interior gap, two more than every other card's.
///
/// The canvas gives this one card 14 and the other three 12, and it is not an
/// accident of the drawing: the band is the only thing on the page with no box
/// around it and no baseline under it, so its captions need the extra two to
/// stop reading as a fifth row of the chart.
const CHART_GAP: f32 = 14.0;

/// The two fixed cells of the spend band, and their own deeper padding — this
/// is the card carrying the page's headline figure, and it is the one place
/// that buys room with it.
const SPEND_CELL: f32 = 238.0;
const SPEND_PAD_Y: f32 = 18.0;

/// The day chart's band. Short, and it is the *band* that is short: a window
/// with nothing in it draws no band at all rather than reserving this much
/// height for a row of em dashes (gh#252).
const CHART_HEIGHT: f32 = 96.0;

/// The gutter between day columns, shared by the bars and the captions under
/// them so a caption sits under its own bar.
const COLUMN_GAP: f32 = 8.0;

/// What a day that spent nothing draws instead of a bar (gh#226) — a rule on
/// the baseline, so the quiet days line up as one statement rather than as six
/// slightly different nothings.
const QUIET_RULE: f32 = 2.0;

/// A proportional bar: the cost split, and the outcomes. Height carries no
/// information in either, so it is the thickness of a rule and the width does
/// all the talking.
const BAR_HEIGHT: f32 = 8.0;

/// A breakdown row's track — thinner still, because it is one row of a table
/// and dozens of them stack.
const TRACK_HEIGHT: f32 = 6.0;

/// One cell of the crossed grid.
const HEAT_CELL: f32 = 20.0;

/// The corner on a drawn mark — a chart column, a meter fill, a heat cell, a
/// legend chip.
///
/// scale-ok: a mark is data, not a box on a surface. Its corner relates to its
/// own 4–8px of width, not to the card it sits in, so the three-radius scale
/// (gh#174) does not reach it — but it is still ONE number for every mark on
/// the page, which is the same rule one level down.
const MARK_RADIUS: f32 = 2.0;

/// The label gutters. The crossed grid's is narrower than the breakdown's
/// because it has to give 24 cells room to stay square.
const GRID_LABEL: f32 = 92.0;
const BREAKDOWN_LABEL: f32 = 132.0;

/// The numeric columns: a row total on the grid, and tokens and money on a
/// breakdown row.
///
/// The money column is wider than its figures need because it does not only
/// hold figures: a row nobody could price says `unpriced` there (gh#359), and
/// at 44px that word arrived as `unpric…` — a truncation that reads as a
/// rendering fault, which is the one thing the word was chosen to avoid.
const TOTAL_COL: f32 = 34.0;
const TOKENS_COL: f32 = 52.0;
const PRICE_COL: f32 = 58.0;

/// The floor under an outcome band, so one merge out of ninety is a sliver you
/// can see and point at rather than a rounding error. Bands grow from here,
/// which is also why the bar never overflows its track.
const LANDING_BAND_MIN: f32 = 6.0;

/// A legend's colour chip.
const SWATCH: f32 = 7.0;

/// The gutter a segmented track puts around and between its segments.
///
/// Two, not [`Theme::NEST_GUTTER`]'s four: a track is a chip bed with chips in
/// it, not a card with rows in it, and at four the wash reads as a bar with
/// buttons parked on it.
const TRACK_GUTTER: f32 = 2.0;

/// Which of the two segmented controls this is (gh#278).
///
/// The canvas draws them at different sizes in the same breath — the window
/// picker heads the page, the axis toggle heads a card — and the difference is
/// three properties, not one, so it is one named thing rather than three
/// arguments a call site can get half right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrackSize {
    /// In the page header: 12px segments, a `--line` hairline, radius 10.
    Page,
    /// In a card header: 11px segments, no hairline, and the radius its own
    /// gutter makes concentric with them.
    Card,
}

impl TrackSize {
    /// The outer radius. The page track closes at the row radius the rest of
    /// the page's furniture uses; the card track closes at exactly its
    /// segments' radius plus its gutter, which is the nesting rule (gh#174)
    /// evaluated at this size rather than assumed from the other one.
    ///
    /// scale-ok: not a fourth radius — it is `RADIUS_CHIP` plus the gutter
    /// around it, which is what makes the two curves concentric. Typing 8 here
    /// would be the literal the scale exists to prevent.
    fn radius(self) -> f32 {
        match self {
            Self::Page => Theme::RADIUS_ROW,
            Self::Card => Theme::RADIUS_CHIP + TRACK_GUTTER,
        }
    }
}

// ---- which board (gh#254) --------------------------------------------------

/// The board the operator picked, as it is written down.
///
/// `ThisDevice` rather than the local device's own id, so a pin survives the
/// device being re-registered — and so a pinned local host and an unpinned one
/// address the same engine through the same absent `targetDeviceId`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HostChoice {
    All,
    ThisDevice,
    Device(String),
}

impl HostChoice {
    /// What to remember for a candidate the sweep offers.
    fn of(target: Option<&str>, local: Option<&str>) -> Self {
        match target {
            Some(id) if Some(id) != local => Self::Device(id.to_string()),
            _ => Self::ThisDevice,
        }
    }

    /// Does this pick name that candidate? The local device answers to both
    /// spellings — its own id, and the absent target the sweep asks it by.
    fn matches(&self, target: Option<&str>, local: Option<&str>) -> bool {
        match self {
            Self::All => false,
            Self::ThisDevice => target.is_none() || (target == local && local.is_some()),
            Self::Device(id) => {
                target == Some(id.as_str()) || (target.is_none() && local == Some(id.as_str()))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatsSelection {
    All,
    Board(usize),
}

/// Where the stats page's own preferences live.
const PREFS_FILE: &str = "stats-prefs.json";

/// The stats page's preference file, beside `composer-defaults.json`.
///
/// Its own file rather than a field on `ui-settings.json`: the shell saves that
/// one debounced from a copy it has held since boot, so a settings page writing
/// into it would have its write undone by the next sidebar drag.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct StatsPrefs {
    /// The board the page reads. Absent follows the sweep, which is the right
    /// answer on every install with one board on it.
    pub host: Option<HostChoice>,
}

impl StatsPrefs {
    /// Load from `{data_dir}/stats-prefs.json`; defaults on any failure.
    pub fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<StatsPrefs>(&text) {
                Ok(prefs) => prefs,
                Err(err) => {
                    tracing::warn!(error = %err, "stats-prefs corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write never
    /// corrupts. Synchronous: picking a host is a rare click, not a drag.
    pub fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(PREFS_FILE)
    }
}

/// Which of the answers the page shows.
///
/// The pin wins while the board it names is still answering; the sweep's own
/// answer is the default and the fallback. A pin that has not answered *yet*
/// holds the page rather than flashing somebody else's numbers: mid-sweep is
/// not evidence that the box is gone.
///
/// The sweep's answer follows the board pane's furniture rule (gh#125, taught
/// to this page by gh#434): the first candidate **with dispatch evidence**
/// wins, because a board somebody has released work from is the org's board
/// and one that only ever collected rows is furniture. Local answers first —
/// asking locally is free — so without this rule a leftover local `board.db`
/// outranks the box forever, and its stale numbers present as the board. A
/// furniture answer is held while the sweep keeps asking, and settles only
/// when every candidate has been asked and nobody with evidence answered —
/// so a lone device, and a genuinely fresh install, still see their own board.
fn resolve(
    answers: &[(Option<String>, BoardStats)],
    pinned: Option<&HostChoice>,
    local: Option<&str>,
    swept: bool,
) -> Option<StatsSelection> {
    if let Some(pin) = pinned {
        if *pin == HostChoice::All && !answers.is_empty() {
            return Some(StatsSelection::All);
        }
        if let Some(found) = answers
            .iter()
            .position(|(target, _)| pin.matches(target.as_deref(), local))
        {
            return Some(StatsSelection::Board(found));
        }
        if !swept {
            return None;
        }
    }
    if let Some(evidenced) = answers.iter().position(|(_, stats)| stats.dispatched) {
        return Some(StatsSelection::Board(evidenced));
    }
    (swept && !answers.is_empty()).then_some(StatsSelection::Board(0))
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
    /// **Every candidate that answered, in sweep order** (gh#254) — the host
    /// control's contents, the numbers of whichever one is selected, and what
    /// an empty board names instead of only its own emptiness. `None` in the
    /// target is this device, exactly as the sweep spells it.
    answers: Vec<(Option<String>, BoardStats)>,
    /// The shared all-board contract that supplied `answers` and the explicit
    /// aggregate choice. Retained whole for host completeness/audit notes.
    aggregate: Option<AggregateBoardStats>,
    /// The remembered pick. `None` follows the sweep.
    pinned: Option<HostChoice>,
    /// Every candidate has been asked. Until then, a pin with no answer yet —
    /// and a sweep holding only a furniture answer — is still worth waiting
    /// for.
    swept: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    /// Monotonic fence for aggregate reads. A slower request for an old
    /// window must never repaint beneath the newly selected window label.
    reload_generation: u64,
    updating: Option<String>,
    update_task: Option<Task<()>>,
}

impl StatsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let pinned = state
            .read(cx)
            .data_dir
            .as_deref()
            .and_then(|dir| StatsPrefs::load(dir).host);
        let mut page = Self {
            state,
            // A week: long enough to have a shape, short enough that today is
            // still visible in it.
            since_days: Some(7),
            // The axis where the answer is usually one row, and the one the
            // page could not ask about at all before gh#227.
            dimension: Dimension::Model,
            answers: Vec::new(),
            aggregate: None,
            pinned,
            swept: false,
            error: None,
            task: None,
            reload_generation: 0,
            updating: None,
            update_task: None,
        };
        page.reload(cx);
        page
    }

    /// Read the shared aggregate contract once. The engine owns the bounded
    /// concurrent fan-out; this page owns only selection and rendering.
    fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            // Nothing to sweep: the empty state must not read as mid-sweep.
            self.swept = true;
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        let since_days = self.since_days;
        self.reload_generation = self.reload_generation.wrapping_add(1);
        let generation = self.reload_generation;
        self.error = None;
        self.swept = false;
        self.task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::json!({});
            if let (Some(days), Some(object)) = (since_days, params.as_object_mut()) {
                object.insert("sinceDays".into(), serde_json::json!(days));
            }
            let result = engine
                .client()
                .call(methods::AGGREGATE_BOARD_STATS, params)
                .await
                .map_err(|error| error.to_string())
                .and_then(|value| {
                    serde_json::from_value::<AggregateBoardStats>(value)
                        .map_err(|error| format!("Unreadable aggregate stats: {error}"))
                });
            let _ = this.update(cx, |page, cx| {
                if page.reload_generation != generation {
                    return;
                }
                page.swept = true;
                match result {
                    Ok(aggregate) => {
                        let empty_note = aggregate
                            .completeness_note()
                            .unwrap_or_else(|| "No device on this account hosts a board".into());
                        page.apply_aggregate(aggregate, local.as_deref());
                        let answers = &page.answers;
                        page.error = if answers.is_empty() {
                            Some(empty_note.into())
                        } else {
                            None
                        };
                    }
                    Err(error) => page.error = Some(error.into()),
                }
                cx.notify();
            });
        }));
    }

    fn apply_aggregate(&mut self, aggregate: AggregateBoardStats, local: Option<&str>) {
        self.answers = aggregate
            .boards
            .iter()
            .map(|board| {
                let target = (Some(board.host.device_id.as_str()) != local)
                    .then(|| board.host.device_id.clone());
                (target, board.stats.clone())
            })
            .collect();
        self.aggregate = Some(aggregate);
    }

    /// This device's id, as the sweep and the pin both spell it.
    fn local_device(&self, cx: &gpui::App) -> Option<String> {
        self.state.read(cx).local_device_id.clone()
    }

    /// Which answer is on screen.
    fn resolved(&self, cx: &gpui::App) -> Option<StatsSelection> {
        resolve(
            &self.answers,
            self.pinned.as_ref(),
            self.local_device(cx).as_deref(),
            self.swept,
        )
    }

    /// Every board the sweep found, as the control and the empty states read
    /// them.
    fn boards(&self, cx: &gpui::App) -> Vec<HostBoard> {
        self.answers
            .iter()
            .map(|(target, stats)| {
                HostBoard::of(
                    target.clone(),
                    self.device_label(target.as_deref(), cx),
                    stats,
                )
            })
            .collect()
    }

    /// Read the board on that device instead, and remember it.
    ///
    /// No reload: the control only offers boards that already answered this
    /// window, so switching is a redraw over numbers the page is holding.
    fn set_host(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        let local = self.local_device(cx);
        let choice = HostChoice::of(target.as_deref(), local.as_deref());
        if self.pinned.as_ref() == Some(&choice) {
            return;
        }
        self.pinned = Some(choice.clone());
        if let Some(dir) = self.state.read(cx).data_dir.clone() {
            let prefs = StatsPrefs { host: Some(choice) };
            if let Err(err) = prefs.save(&dir) {
                // The pick still applies to this session; only the memory of
                // it was lost, and that is not worth a strip over the page.
                tracing::warn!(error = %err, "failed to persist the stats host");
            }
        }
        cx.notify();
    }

    fn set_all_boards(&mut self, cx: &mut Context<Self>) {
        let choice = HostChoice::All;
        if self.pinned.as_ref() == Some(&choice) {
            return;
        }
        self.pinned = Some(choice.clone());
        if let Some(dir) = self.state.read(cx).data_dir.clone() {
            let prefs = StatsPrefs { host: Some(choice) };
            if let Err(err) = prefs.save(&dir) {
                tracing::warn!(error = %err, "failed to persist the stats host");
            }
        }
        cx.notify();
    }

    /// Apply an old peer's managed update, then ride through the relay restart.
    /// A lost ApplyUpdate reply is expected: convergence is the peer answering
    /// the aggregate protocol, not the mutation socket surviving its restart.
    fn update_stats_host(&mut self, device_id: String, cx: &mut Context<Self>) {
        if self.updating.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.updating = Some(device_id.clone());
        self.error = None;
        let since_days = self.since_days;
        let timer = cx.background_executor().clone();
        self.update_task = Some(cx.spawn(async move |this, cx| {
            let apply = engine
                .client()
                .call(
                    methods::APPLY_UPDATE,
                    serde_json::json!({ "targetDeviceId": device_id }),
                )
                .await;
            if let Err(error) = apply
                && !apply_failure_may_be_restart(&error)
            {
                let _ = this.update(cx, |page, cx| {
                    page.updating = None;
                    page.error = Some(format!("Update refused: {error}").into());
                    cx.notify();
                });
                return;
            }
            let mut outcome = Err("The device did not return after its update".to_string());
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            while std::time::Instant::now() < deadline {
                timer.timer(Duration::from_secs(1)).await;
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                let value = tokio::time::timeout(
                    remaining,
                    engine.client().call(
                        methods::AGGREGATE_BOARD_STATS,
                        serde_json::json!({ "sinceDays": since_days }),
                    ),
                )
                .await;
                let Ok(Ok(value)) = value else { continue };
                let Ok(aggregate) = serde_json::from_value::<AggregateBoardStats>(value) else {
                    continue;
                };
                let converged = update_recovery_converged(&aggregate, &device_id);
                if converged {
                    outcome = Ok(aggregate);
                    break;
                }
            }
            let _ = this.update(cx, |page, cx| {
                page.updating = None;
                match outcome {
                    Ok(aggregate)
                        if recovery_matches_selected_window(since_days, page.since_days) =>
                    {
                        let local = page.local_device(cx);
                        page.apply_aggregate(aggregate, local.as_deref());
                    }
                    // A normal reload for the newly selected window owns the
                    // screen. The recovery probe used its captured window only
                    // to prove this exact peer had returned.
                    Ok(_) => {}
                    Err(message) => page.error = Some(message.into()),
                }
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

    /// What to call a candidate: the device's own name.
    ///
    /// The local board is named too, rather than called "this device" — a
    /// control whose segments read *this device* and *the box* makes the
    /// reader do the mapping, and the whole point of gh#254 is that the two
    /// were indistinguishable. The generic wording survives only where there
    /// is genuinely no name to use.
    fn device_label(&self, target: Option<&str>, cx: &gpui::App) -> String {
        let state = self.state.read(cx);
        let id = target.or(state.local_device_id.as_deref());
        id.and_then(|id| state.devices.iter().find(|d| d.id == id))
            .map(|d| d.name.clone())
            .unwrap_or_else(|| match target {
                Some(id) => id.to_string(),
                None => "this device".to_string(),
            })
    }

    /// The track a segmented control's choices sit in.
    ///
    /// One recipe in two sizes, used by the window picker and by the
    /// breakdown's dimension toggle (gh#227): two segmented controls on one
    /// page that were built twice would be two segmented controls that drift
    /// apart.
    ///
    /// **The bed is `--chip`** (gh#278). It was a private 2% wash, which is a
    /// third of what the canvas declares and had no token behind it at all
    /// until gh#274 gave `--chip` one. A chip bed is exactly what a track is:
    /// the wash the unchosen segments sit in, and the thing the chosen one is
    /// punched out of.
    ///
    /// The two sizes are the canvas's, and they differ in three ways at once
    /// because they are a page header's control and a card header's control.
    /// The `PAGE` track wears a hairline at [`Theme::RADIUS_ROW`]; the `CARD`
    /// one wears none and closes at its segments' radius plus its own gutter,
    /// which is what keeps the two curves concentric at the smaller size.
    fn track(theme: &Theme, size: TrackSize) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .gap(px(TRACK_GUTTER))
            .p(px(TRACK_GUTTER))
            .rounded(px(size.radius()))
            .when(size == TrackSize::Page, |el| {
                el.border_1().border_color(theme.border)
            })
            .bg(theme.chip)
    }

    /// One choice in that track.
    ///
    /// The padding follows the type size rather than being a second knob: the
    /// design gives 8×3 around an 11px segment and 10×4 around a 12px one,
    /// which is one rule stated twice.
    ///
    /// **The chosen one is the page's own ground, not a selected row**
    /// (gh#278). It used to go through [`crate::theme::Theme::row`] at
    /// [`crate::theme::Bed::Card`], which paints `--selcard` — and that is
    /// inverted in both themes here: the canvas fills the chosen segment with
    /// `--card`, which is `#070707` under a dark chip wash and `#ffffff` under
    /// a light one. So the segment steps DOWN in dark and UP in light, and
    /// `--lift` carries the light half where a change of tone alone would not
    /// read.
    ///
    /// That is not a contradiction of gh#175, which is about *rows in a list*:
    /// a row is one of many on a surface and lifts off it, a segment is a hole
    /// cut in a wash so the page shows through. Hover is still the shared wash,
    /// because that part of the grammar does not change with the shape.
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
            .text_color(if selected {
                theme.text
            } else {
                theme.text_muted
            })
            .when(selected, |el| el.bg(theme.bg).shadow(theme.lift_shadow()))
            .when(!selected, |el| {
                el.cursor_pointer()
                    .hover(|el| el.bg(theme.element_hover).text_color(theme.text))
            })
            .child(SharedString::from(label.to_string()))
    }

    /// The window picker: a segmented row, current one filled.
    fn render_windows(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut row = Self::track(theme, TrackSize::Page);
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

    /// **The host picker (gh#254/gh#461): All boards, then one segment per
    /// canonical board the aggregate found.**
    ///
    /// It is drawn only when there is a choice to make. One board on one
    /// candidate is every ordinary install, and a segmented control there is
    /// furniture. A second candidate still earns the aggregate segment even if
    /// it failed: selecting All boards is how the partial warning becomes
    /// visible rather than how the missing host becomes a zero.
    ///
    /// Individual segments are only boards that answered. Candidate failures
    /// stay in the All boards audit; offering one as a board would be offering
    /// an error.
    fn render_hosts(
        &self,
        boards: &[HostBoard],
        selection: Option<StatsSelection>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let has_multiple_candidates = self
            .aggregate
            .as_ref()
            .is_some_and(|aggregate| aggregate.hosts.len() > 1);
        if boards.len() < 2 && !has_multiple_candidates {
            return None;
        }
        let mut row = Self::track(theme, TrackSize::Page).child(
            Self::segment(
                theme,
                "stats-host-all".into(),
                "All boards",
                Theme::TEXT_DENSE,
                selection == Some(StatsSelection::All),
            )
            .on_click(cx.listener(|this, _, _, cx| this.set_all_boards(cx))),
        );
        for (index, host) in boards.iter().enumerate() {
            let target = host.device_id.clone();
            let selected = matches!(selection, Some(StatsSelection::Board(ix)) if ix == index);
            row = row.child(
                Self::segment(
                    theme,
                    format!("stats-host-{index}"),
                    &host.label,
                    Theme::TEXT_DENSE,
                    selected,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.set_host(target.clone(), cx))),
            );
        }
        Some(row.into_any_element())
    }

    // -- the shared furniture of a card --------------------------------------

    /// A card: a hairline, the card tone, one radius. The design draws every
    /// block of this page as the same box, so there is one function for it.
    fn card(theme: &Theme) -> gpui::Div {
        div()
            .rounded(px(Theme::RADIUS_CARD))
            .border_1()
            .border_color(theme.border)
            .bg(theme.card)
            .overflow_hidden()
            .flex()
            .flex_col()
    }

    /// The same card with the interior four of the five share.
    ///
    /// **The padding is the density fix** (gh#252). This page used to put a
    /// header at `pt(14)` above a body at `py(14)`, which is 28px of nothing
    /// between a title and the thing it titles, repeated five times. The design
    /// asks for one padded box with a 12px gap in it, and that alone is most of
    /// the two extra screens the page used to need.
    fn padded_card(theme: &Theme) -> gpui::Div {
        Self::card(theme)
            .px(px(CARD_PAD_X))
            .pt(px(CARD_PAD_TOP))
            .pb(px(CARD_PAD_BOTTOM))
            .gap(px(CARD_GAP))
    }

    /// The line a card opens with: what it is, and a caption qualifying it.
    fn card_head(theme: &Theme, title: &str, aside: Option<String>) -> gpui::Div {
        div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap(px(10.0))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(title.to_string())),
            )
            .when_some(aside, |el, aside| el.child(Self::caption(theme, aside)))
    }

    /// The caption beside a title, and the small subdued line a card ends with.
    fn caption(theme: &Theme, copy: String) -> AnyElement {
        div()
            .min_w_0()
            .text_size(px(Theme::TEXT_DENSE))
            .text_color(theme.text_subtle)
            .child(SharedString::from(copy))
            .into_any_element()
    }

    /// One step quieter than a caption — the day under a bar, the hour under a
    /// grid. Type that is there to be glanced past, not read.
    fn footnote(theme: &Theme, copy: String) -> AnyElement {
        div()
            .min_w_0()
            .truncate()
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(theme.text_faint)
            .child(SharedString::from(copy))
            .into_any_element()
    }

    /// **A drawn mark's tone: the theme's ink at a fraction of itself.**
    ///
    /// The design states every mark on this page as
    /// `color-mix(in srgb, var(--text) N%, transparent)` — a chart column, a
    /// heat cell, a meter fill, a legend chip for a token class — and
    /// [`Theme::white_alpha`] is that declaration already: soft-white over
    /// dark, cool ink over light, and unscaled, because a mark whose whole job
    /// is to be seen cannot be quietened the way a wash can.
    ///
    /// Two things fall out of it. The page needs no second palette to survive
    /// the theme flip, because near-white on near-black and near-black on white
    /// are the same statement. And the four status hues keep meaning *state*
    /// (gh#173): a token volume is not a state, so a bar of tokens is ink. The
    /// only hues on this page are in the outcomes bar, where they do name
    /// states.
    ///
    /// Deliberately not an alpha on a text token, which would be the same
    /// colour and a rule violation (gh#172, `tests/text_tones.rs`): text paints
    /// in four named tones and nothing between them. These are fills, and fills
    /// have their own primitive precisely so the two never get confused.
    fn mark(theme: &Theme, alpha: f32) -> gpui::Hsla {
        theme.white_alpha(alpha)
    }

    /// A hairline across a card's interior.
    fn seam(theme: &Theme) -> AnyElement {
        // `flex_none`, or the one-pixel rule is the first thing a crowded
        // column shrinks away and the seam silently disappears.
        div()
            .flex_none()
            .h(px(1.0))
            .bg(theme.border)
            .into_any_element()
    }

    /// A sentence inside a card — the honest notes, and the empty states.
    fn note(theme: &Theme, copy: impl Into<SharedString>) -> AnyElement {
        div()
            .text_size(px(Theme::TEXT_DENSE))
            .text_color(theme.text_subtle)
            .child(copy.into())
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

    // -- card 1: spend -------------------------------------------------------

    /// What the window would have cost at the meter, against what the plans
    /// behind it cost, and where inside that price the money went.
    ///
    /// **The card has no title, because the figures are the page's headline.**
    /// Two 238px cells and the split, hairline-separated inside one box, over a
    /// footer that says what the numbers are and are not. The three are one
    /// sentence read left to right — this cost that, the plan carried it this
    /// far, and it went mostly there.
    ///
    /// The first two are never added. One is the board's own arithmetic over
    /// tokens it counted; the other is a number a person typed beside a login
    /// (gh#178), possibly several people's on a box carrying several slots. The
    /// only honest thing to do with the pair is divide it, which is the
    /// multiple the second cell shows.
    fn render_spend(stats: &BoardStats, wrong_board: Option<String>, theme: &Theme) -> AnyElement {
        let Some(spend) = stats.spend.as_ref().filter(|s| s.has_price()) else {
            // The two ways there is no number, said apart — `spend_label` owns
            // which one this is, and neither of them is $0.00. No band and no
            // bar here: an empty state is prose, never a row of dashes over a
            // chart of zeroes.
            let mut card = Self::padded_card(theme).child(Self::note(
                theme,
                format!(
                    "No price for the {}: {}.",
                    stats.window_label(),
                    stats.spend_label()
                ),
            ));
            // **The third way there is no number: the wrong board (gh#254).**
            // `nothing metered to price` is true of a board whose attempts all
            // predate token capture, and says nothing about the one next to it
            // that holds 46 million tokens. This is the sentence that does.
            if let Some(note) = wrong_board {
                card = card.child(Self::note(theme, note));
            }
            if stats.spend.is_none() {
                card = card.child(Self::note(
                    theme,
                    "Model rates live in routing.toml under [defaults.rates]; the built-in \
                     list prices are used when it names none.",
                ));
            }
            if let Some(agent_usage) = Self::render_agent_usage(stats, theme) {
                card = card.child(agent_usage);
            }
            return card
                .child(Self::note(theme, Self::BILL_NOTE))
                .into_any_element();
        };

        let split = spend.cost_split();
        let window = stats.window_label();

        let price = Self::spend_cell(
            theme,
            human_usd(spend.list_price),
            "list-price API estimate for this work",
            Some(format!(
                "{} tokens over {window}",
                human_tokens(split.tokens)
            )),
        );

        let plans = match spend.subscriptions_in_window() {
            // The multiple is the figure and the two amounts are its note:
            // *how far does the subscription carry this* is the question, and
            // the dollars are how it was arrived at.
            Some(in_window) => match spend.subsidy() {
                Some(ratio) => Self::spend_cell(
                    theme,
                    human_multiple(ratio),
                    "subsidised by your subscription",
                    Some(format!(
                        "{} of {}",
                        human_usd(in_window),
                        Self::monthly_phrase(spend)
                    )),
                ),
                // Plans entered, and free: a ratio against zero is not a
                // number, and the amount is still worth showing.
                None => Self::spend_cell(
                    theme,
                    human_usd(in_window),
                    "in plans over the same window",
                    Some(format!(
                        "of {} — nothing to divide by",
                        Self::monthly_phrase(spend)
                    )),
                ),
            },
            // Two different "no", and collapsing them would be the mistake this
            // whole half of the page is built to avoid. An all-time window has
            // plans and no days to pro-rate them onto; a board with nothing
            // entered has no second figure at all — and unentered is not free.
            None => {
                let monthly = spend.monthly_subscriptions();
                if monthly.is_zero() {
                    Self::spend_cell(theme, "—", "no plan cost entered in Accounts", None)
                } else {
                    Self::spend_cell(
                        theme,
                        format!("{}/mo", human_usd(monthly)),
                        "in plans",
                        Some("all time has no window to pro-rate onto".into()),
                    )
                }
            }
        };

        let mut card = Self::card(theme).child(
            div()
                .flex()
                .flex_row()
                .items_stretch()
                .child(price)
                .child(Self::rule(theme))
                .child(plans)
                .child(Self::rule(theme))
                .child(Self::render_cost_split(&split, theme)),
        );
        if let Some(agent_usage) = Self::render_agent_usage(stats, theme) {
            card = card.child(agent_usage);
        }
        card.child(Self::spend_footer(stats, spend, theme))
            .into_any_element()
    }

    /// One cell of the spend band: a figure, what it is, and how it was arrived
    /// at.
    fn spend_cell(
        theme: &Theme,
        value: impl Into<SharedString>,
        caption: &str,
        note: Option<String>,
    ) -> AnyElement {
        div()
            .flex_none()
            .w(px(SPEND_CELL))
            .px(px(CARD_PAD_X))
            .py(px(SPEND_PAD_Y))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .child(
                div()
                    .text_size(px(Theme::TEXT_DISPLAY))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .truncate()
                    .child(value.into()),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(caption.to_string())),
            )
            .when_some(note, |el, note| {
                el.child(
                    div()
                        .mt(px(8.0))
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_muted)
                        .child(SharedString::from(note)),
                )
            })
            .into_any_element()
    }

    /// The full-height hairline between two cells of the spend band.
    fn rule(theme: &Theme) -> AnyElement {
        div()
            .flex_none()
            .w(px(1.0))
            .bg(theme.border)
            .into_any_element()
    }

    /// Where the list-price API estimate goes (gh#225): the four rates as a proportional
    /// bar, with what each one bought beneath it.
    ///
    /// This answers *which kind of token was expensive*, which no other view of
    /// these numbers can: a week that spends most of its price on a million
    /// output tokens while thirty-five million cached ones cost a fraction of
    /// that is a week where the cache is doing its job — and the same week read
    /// as one total is just a number.
    ///
    /// Both figures per class, always. Dollars alone hide that the cheap class
    /// is the enormous one; tokens alone hide that the small one is the bill.
    fn render_cost_split(split: &CostSplit, theme: &Theme) -> AnyElement {
        let cell = div()
            .flex_grow(1.0)
            .flex_basis(px(0.0))
            .min_w_0()
            .px(px(CARD_PAD_X))
            .py(px(SPEND_PAD_Y))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(Self::caption(
                theme,
                "Where the list-price API estimate goes".into(),
            ));

        // The bar earns its room only when there is a shape to draw. A window
        // whose whole price rounds to nothing says so instead of drawing four
        // empty segments.
        if split.is_empty() {
            return cell
                .child(Self::note(theme, "Nothing in this window carries a price."))
                .into_any_element();
        }

        let tone = |index: usize| Self::mark(theme, Self::CLASS_TONES[index.min(3)]);
        let bar = div()
            .flex()
            .flex_row()
            .gap(px(2.0))
            .w_full()
            .h(px(BAR_HEIGHT))
            .children(split.slices.iter().enumerate().map(|(index, slice)| {
                div()
                    // Proportional, and never invisible: a class that cost a
                    // fraction of a percent is still a class that was spent in.
                    .flex_grow(slice.share as f32)
                    .flex_basis(px(0.0))
                    .min_w(px(3.0))
                    .h_full()
                    .rounded(px(MARK_RADIUS))
                    .bg(tone(index))
                    .into_any_element()
            }));

        let legend = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap_x(px(18.0))
            .gap_y(px(6.0))
            .children(split.slices.iter().enumerate().map(|(index, slice)| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_none()
                            .size(px(SWATCH))
                            .rounded(px(MARK_RADIUS))
                            .bg(tone(index)),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!(
                                "{} {}",
                                slice.label(),
                                human_usd(slice.cost)
                            ))),
                    )
                    // What it bought, one tone back: the money is the fact and
                    // the tokens are its scale.
                    .child(Self::footnote(theme, human_tokens(slice.tokens)))
                    .into_any_element()
            }));

        cell.child(bar).child(legend).into_any_element()
    }

    /// The four tones of the cost bar, biggest class first.
    ///
    /// One ink at four weights rather than four hues: the status ramp (gh#173)
    /// means something on this page, and borrowing it to say `cache writes`
    /// would be a colour lying about what it names. Ranked slices in a ranked
    /// ramp also mean the bar reads dark-to-light left to right, which is the
    /// ordering said twice.
    const CLASS_TONES: [f32; 4] = [0.62, 0.44, 0.28, 0.16];

    /// Main and delegated work, per model, where Claude's journal exposed the
    /// assistant-step attribution (gh#426). It lives inside the spend card so
    /// its figures inherit the card's token-coverage qualification and cannot
    /// be mistaken for a second total.
    fn spend_agent_rows(stats: &BoardStats) -> Option<&[AgentSpend]> {
        (!stats.agent_usage.is_empty()).then_some(stats.agent_usage.as_slice())
    }

    fn render_agent_usage(stats: &BoardStats, theme: &Theme) -> Option<AnyElement> {
        let agent_usage = Self::spend_agent_rows(stats)?;
        let rows = div().flex().flex_col().gap(px(7.0)).children(
            agent_usage
                .iter()
                .take(BREAKDOWN_ROWS)
                .map(|row: &AgentSpend| {
                    let price = row
                        .price_label()
                        .unwrap_or_else(|| "rates not configured".to_string());
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .justify_between()
                        .gap(px(12.0))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(theme.text_muted)
                                .child(SharedString::from(row.label())),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(theme.text)
                                .child(SharedString::from(format!(
                                    "{} · {price}",
                                    human_tokens(row.usage.total())
                                ))),
                        )
                        .into_any_element()
                }),
        );
        Some(
            div()
                .border_t_1()
                .border_color(theme.border)
                .px(px(CARD_PAD_X))
                .py(px(12.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(Self::caption(
                    theme,
                    "By agent and model — list-price API estimates".into(),
                ))
                .child(rows)
                .child(Self::footnote(
                    theme,
                    format!(
                        "Agent detail from {} of {} attempts that reported usage.",
                        stats.attempts_with_agent_usage, stats.attempts_with_tokens
                    ) + &match agent_usage.len().saturating_sub(BREAKDOWN_ROWS) {
                        0 => String::new(),
                        remaining => format!(" Showing {BREAKDOWN_ROWS}; {remaining} more row(s)."),
                    },
                ))
                .into_any_element(),
        )
    }

    /// The sentence that keeps the card from being read as a bill — one line,
    /// because a footnote that runs to a paragraph is a paragraph nobody reads.
    const BILL_NOTE: &'static str = "List-price API estimate using the rates in \
         Settings → Agents — not a bill; subscription runs do not pay per token.";

    /// The one span of that sentence the canvas lifts a tone: the page the
    /// rates are on. Matched rather than re-spelled, so the two cannot drift.
    const RATES_PAGE: &'static str = "Settings → Agents";

    /// The footer under the spend band: what the figures are, what they cover,
    /// and everything they leave out.
    ///
    /// Every total carries its coverage here, because a figure summed from two
    /// of five attempts is not the window's spend. This is also where the
    /// per-model table's provenance column went (gh#252) — a rate read off a
    /// family or overridden in `routing.toml` is news about the whole card, not
    /// about one row of a table this page no longer has room for.
    fn spend_footer(stats: &BoardStats, spend: &BoardSpend, theme: &Theme) -> AnyElement {
        let mut lines: Vec<String> = Vec::new();
        // One sentence, like the design's. Where the rates came from and how
        // much of the window they cover are qualifications of the same claim,
        // and three stacked footnotes is the density this ticket is about.
        let mut first = format!(
            "{} Priced against {}.",
            Self::BILL_NOTE,
            spend.rates.provenance()
        );
        if let Some(coverage) = Self::coverage_note(stats) {
            first.push(' ');
            first.push_str(&coverage);
        }
        lines.push(first);
        if !spend.is_complete() {
            lines.push(format!(
                "Not in that total: {} token(s) on {} model(s) with no rate ({}).",
                human_tokens(spend.unpriced_tokens),
                spend.unpriced.len(),
                spend
                    .unpriced
                    .iter()
                    .map(|t| t.label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        div()
            .border_t_1()
            .border_color(theme.border)
            .px(px(CARD_PAD_X))
            .py(px(9.0))
            .flex()
            .flex_row()
            .items_start()
            .gap(px(8.0))
            .child(
                crate::icons::icon(crate::icons::INFO_CIRCLE)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_faint),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .children(lines.into_iter().map(|line| {
                        div()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_subtle)
                            .child(Self::footer_line(theme, line))
                            .into_any_element()
                    })),
            )
            .into_any_element()
    }

    /// One footer line, with the page it names lifted a tone (gh#278).
    ///
    /// The canvas writes the sentence in `--subtle` and sets `Settings →
    /// Agents` in `--muted`, which is the footnote pointing at somewhere you
    /// can go. Painted as two [`gpui::TextRun`]s over one shaped string rather
    /// than as two elements in a row: the sentence wraps, and a row of spans
    /// would wrap between them instead of inside them.
    ///
    /// A `TextRun` carries a colour and a font but no size, so this is exactly
    /// as far as inline styling reaches here — the same limit `window.md`'s D6
    /// records for inline code. Colour is all the canvas asks for.
    fn footer_line(theme: &Theme, line: String) -> AnyElement {
        let base = gpui::font(theme.font_sans.clone());
        let run = |len: usize, color: gpui::Hsla| gpui::TextRun {
            len,
            font: base.clone(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = match line.find(Self::RATES_PAGE) {
            Some(at) => vec![
                run(at, theme.text_subtle),
                run(Self::RATES_PAGE.len(), theme.text_muted),
                run(line.len() - at - Self::RATES_PAGE.len(), theme.text_subtle),
            ],
            None => vec![run(line.len(), theme.text_subtle)],
        };
        gpui::StyledText::new(SharedString::from(line))
            .with_runs(runs)
            .into_any_element()
    }

    /// What the plans cost per month, said so it survives a box carrying
    /// several teammates' slots (gh#59): one plan is *a* plan, several are
    /// counted rather than silently summed into somebody's imagined bill.
    fn monthly_phrase(spend: &BoardSpend) -> String {
        let monthly = human_usd(spend.monthly_subscriptions());
        match spend.accounts.iter().filter(|a| a.plan.is_some()).count() {
            0 | 1 => format!("a {monthly}/mo plan"),
            n => format!("{monthly}/mo across {n} plans"),
        }
    }

    // -- card 2: the day chart ----------------------------------------------

    /// **The day chart, as its own card** (gh#252): height is tokens, the
    /// dispatch count is in the caption (gh#226).
    ///
    /// It used to be the left three-fifths of a "Work released" card it shared
    /// with the outcomes bar, which is two questions in one box and neither of
    /// them where the design puts it. On its own it is what its title says.
    ///
    /// **An empty chart collapses rather than reserves.** A window where no day
    /// reported tokens draws no band at all — the aside says so and the day
    /// captions carry the dispatch counts, which is the whole of what there is
    /// to know. Reserving a full band for seven em dashes is the page claiming
    /// to have drawn something.
    ///
    /// Every day in a window that *did* report draws, including the quiet ones:
    /// a day that spent nothing is a 2px rule on the baseline, never a gap. A
    /// seven-day window on a board that worked one day is a shape; one lonely
    /// bar reads as six days the board failed to record.
    fn render_chart(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let columns = day_columns(&stats.daily, &stats.daily_tokens);
        if columns.is_empty() {
            return Self::padded_card(theme)
                .child(Self::card_head(theme, Self::CHART_TITLE, None))
                .child(Self::note(theme, "Nothing was dispatched in this window."))
                .into_any_element();
        }
        let peak = peak_tokens(&stats.daily_tokens);
        // A month of captions is a month of collisions; past the cap the
        // columns go bare and the axis carries the range instead.
        let captioned = day_captions_fit(columns.len());
        let mut card = Self::padded_card(theme)
            .gap(px(CHART_GAP))
            .child(Self::card_head(
                theme,
                Self::CHART_TITLE,
                Some(if peak > 0 {
                    format!("bars are tokens · peak {}", human_tokens(peak))
                } else {
                    "no day in this window reported tokens".to_string()
                }),
            ));

        if peak > 0 {
            card = card.child(
                div()
                    .flex()
                    .flex_row()
                    .items_end()
                    .gap(px(COLUMN_GAP))
                    .h(px(CHART_HEIGHT))
                    .children(columns.iter().map(|column| {
                        let quiet = column.is_quiet();
                        div()
                            .flex_1()
                            .min_w(px(3.0))
                            .h(px(if quiet {
                                QUIET_RULE
                            } else {
                                (CHART_HEIGHT * column.fraction).max(QUIET_RULE + 1.0)
                            }))
                            .rounded(px(MARK_RADIUS))
                            // A rule in the border's own weight where the bar
                            // would be: the day is present and it spent
                            // nothing, which is a different statement from a
                            // short bar somebody has to squint at.
                            .bg(Self::mark(
                                theme,
                                if quiet {
                                    0.12
                                } else if column.tokens == peak {
                                    0.55
                                } else {
                                    0.40
                                },
                            ))
                            .into_any_element()
                    })),
            );
        }

        if captioned {
            card = card.child(div().flex().flex_row().gap(px(COLUMN_GAP)).children(
                columns.iter().map(|column| {
                    let top = peak > 0 && column.tokens == peak;
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .truncate()
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(if column.is_quiet() {
                                    theme.text_faint
                                } else if top {
                                    theme.text
                                } else {
                                    theme.text_muted
                                })
                                .child(SharedString::from(column.value.clone())),
                        )
                        .child(
                            div()
                                .truncate()
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(if top {
                                    theme.text_subtle
                                } else {
                                    theme.text_faint
                                })
                                .child(SharedString::from(column.caption.clone())),
                        )
                        .into_any_element()
                }),
            ));
        } else {
            // Too many days to caption every column: the axis carries the range
            // and the peak instead.
            card = card.child(Self::axis(
                theme,
                columns.first().map(|c| c.date.clone()).unwrap_or_default(),
                format!("{} dispatch(es)", stats.attempts),
                columns.last().map(|c| c.date.clone()).unwrap_or_default(),
            ));
        }
        card.into_any_element()
    }

    /// The chart's title — the design's, and it names both series because both
    /// are in the card: the bars are tokens and the captions count dispatches.
    const CHART_TITLE: &'static str = "Tokens and dispatches per day";

    /// The caption under a day series too long to caption per column.
    fn axis(theme: &Theme, first: String, middle: String, last: String) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(theme.text_faint)
            .child(SharedString::from(first))
            .child(SharedString::from(middle))
            .child(SharedString::from(last))
            .into_any_element()
    }

    // -- card 3: when and where ----------------------------------------------

    /// The crossing (gh#179): every space against every hour of the box's
    /// local day, with the per-space totals kept on the right edge.
    ///
    /// These were two cards. A reader looking at an hour card and a space card
    /// can tell that the board runs late and that one repo takes most of the
    /// work, and cannot tell whether those are the same fact — which is the
    /// only thing either card was ever going to be used for.
    fn render_when_and_where(stats: &BoardStats, theme: &Theme) -> AnyElement {
        let grid = hour_grid(&stats.hours_by_workspace, GRID_ROWS);
        let head = Self::card_head(
            theme,
            "When you release work, and where",
            Some(format!(
                "dispatches by local hour · {}",
                stats.window_label()
            )),
        );
        if grid.is_empty() {
            // A board older than the crossing answers without it (the field is
            // defaulted on the wire). Its hour margin is still real, so the
            // card degrades to the histogram rather than to nothing.
            let margin: usize = stats.hour_of_day.iter().sum();
            if margin == 0 {
                return Self::padded_card(theme)
                    .child(head)
                    .child(Self::note(theme, "Nothing was dispatched in this window."))
                    .into_any_element();
            }
            return Self::padded_card(theme)
                .child(head)
                .child(Self::hour_margin(theme, &stats.hour_of_day))
                .child(Self::hour_labels(theme))
                .child(Self::note(
                    theme,
                    "This board reports hours without the space they went to, so the crossing \
                     is not drawn. Update the box to see it.",
                ))
                .into_any_element();
        }

        let peak = grid.peak;
        let rows: Vec<AnyElement> = grid
            .rows
            .iter()
            .map(|row| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .w(px(GRID_LABEL))
                            .flex_none()
                            .truncate()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(row.label.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .gap(px(2.0))
                            .children(row.hours.iter().map(|count| {
                                let heat = bar_fraction(*count, peak);
                                div()
                                    .flex_1()
                                    .min_w(px(4.0))
                                    .h(px(HEAT_CELL))
                                    .rounded(px(MARK_RADIUS))
                                    .bg(Self::mark(
                                        theme,
                                        if *count == 0 {
                                            0.04
                                        } else {
                                            0.12 + 0.56 * heat
                                        },
                                    ))
                                    .into_any_element()
                            })),
                    )
                    .child(Self::cell(theme, format!("{}", row.total), TOTAL_COL, true))
                    .into_any_element()
            })
            .collect();

        Self::padded_card(theme)
            .child(head)
            .child(div().flex().flex_col().gap(px(3.0)).children(rows))
            .child(Self::hour_labels(theme))
            .into_any_element()
    }

    /// The hour histogram, for the one board that cannot send the crossing.
    fn hour_margin(theme: &Theme, hours: &[usize]) -> AnyElement {
        let peak = hours.iter().copied().max().unwrap_or(0);
        div()
            .flex()
            .flex_row()
            .items_end()
            .gap(px(12.0))
            .child(
                div()
                    .w(px(GRID_LABEL))
                    .flex_none()
                    .truncate()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_muted)
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
                    .children(hours.iter().map(|count| {
                        let fraction = bar_fraction(*count, peak);
                        div()
                            .flex_1()
                            .min_w(px(4.0))
                            .h(px((HEAT_CELL * fraction).max(QUIET_RULE)))
                            .rounded(px(MARK_RADIUS))
                            .bg(Self::mark(theme, if *count == 0 { 0.04 } else { 0.45 }))
                            .into_any_element()
                    })),
            )
            .child(div().w(px(TOTAL_COL)).flex_none())
            .into_any_element()
    }

    /// The hour axis: four marks over 24 columns. Every third hour is a smear
    /// at this width, and the reader is looking for a shape, not a reading.
    fn hour_labels(theme: &Theme) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(12.0))
            .child(div().w(px(GRID_LABEL)).flex_none())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_row()
                    .gap(px(2.0))
                    .children(["00", "06", "12", "18"].into_iter().map(|hour| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(hour))
                            .into_any_element()
                    })),
            )
            .child(div().w(px(TOTAL_COL)).flex_none())
            .into_any_element()
    }

    // -- the bottom row: breakdown beside where the work landed --------------

    /// **The page ends in one two-column row** (gh#252), not in a stack: the
    /// breakdown at `flex:3` and the outcomes bar at `flex:2`, which is where
    /// the design puts the one number that says the board wasted its time.
    ///
    /// Either half can be absent — a window with nothing landed has no outcomes
    /// bar, a board that sent no cuts has no breakdown — and the survivor takes
    /// the row.
    fn render_bottom_row(
        &self,
        stats: &BoardStats,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let breakdown = self.render_breakdown(stats, theme, cx);
        let landing = Self::render_landing(stats, theme);
        if breakdown.is_none() && landing.is_none() {
            return None;
        }
        Some(
            div()
                .flex()
                .flex_row()
                .items_stretch()
                .gap(px(PAGE_GAP))
                .when_some(breakdown, |el, card| el.child(card))
                .when_some(landing, |el, card| el.child(card))
                .into_any_element(),
        )
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
    /// reads as. So the four categories are drawn as ONE shape, a rejected pull
    /// request has the blocked hue, and every category is in the legend even at
    /// zero: a reader has to be able to tell a window that lost nothing from a
    /// surface that does not count losses.
    ///
    /// The bar is over tasks that landed. Work still running is a caption under
    /// it and never a band, because a proportion of unfinished work is a
    /// proportion that moves while nothing lands.
    fn render_landing(stats: &BoardStats, theme: &Theme) -> Option<AnyElement> {
        let landing = stats.landing;
        if landing.touched() == 0 {
            return None;
        }
        let segments = landing.segments();
        // A window whose work is all still running has no shape yet: a blank
        // track over four zeroes says less than the caption underneath.
        let landed = landing.total() > 0;

        let mut card = Self::padded_card(theme)
            .flex_grow(2.0)
            .flex_basis(px(0.0))
            .min_w_0()
            .child(Self::card_head(
                theme,
                "Where the work landed",
                Some(format!(
                    "{} task{}",
                    landing.touched(),
                    if landing.touched() == 1 { "" } else { "s" }
                )),
            ));

        if landed {
            card = card
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(2.0))
                        .w_full()
                        .h(px(BAR_HEIGHT))
                        .children(segments.iter().filter(|s| s.count > 0).map(|s| {
                            div()
                                // Grow from a floor rather than sizing to the
                                // share, so a single-task band stays visible
                                // and four bands still fit the track exactly.
                                .flex_grow(s.fraction as f32)
                                .flex_basis(px(0.0))
                                .min_w(px(LANDING_BAND_MIN))
                                .h_full()
                                .rounded(px(MARK_RADIUS))
                                .bg(Self::landing_tone(theme, s.kind))
                                .into_any_element()
                        })),
                )
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(7.0))
                        .children(segments.iter().map(|s| {
                            let empty = s.count == 0;
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(
                                    div()
                                        .flex_none()
                                        .size(px(SWATCH))
                                        .rounded(px(MARK_RADIUS))
                                        // A category with nothing in it keeps
                                        // its row and gives up its hue: the
                                        // count is the fact, and four lit chips
                                        // over three real numbers is a bar that
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
                                        .text_color(if empty {
                                            theme.text_subtle
                                        } else {
                                            theme.text
                                        })
                                        .child(SharedString::from(format!("{}", s.count))),
                                )
                                .into_any_element()
                        })),
                );
        }

        // What the bar leaves out, said out loud rather than counted as an
        // agent that produced nothing.
        if let Some(note) = landing.in_flight_note() {
            card = card.child(Self::caption(theme, note));
        }

        // The throughput and friction facts (gh#252). The design gives them no
        // card of their own, and deleting them would take the only numbers on
        // this page that say what the work COST IN TIME rather than in money —
        // so they ride under the outcomes they qualify, in the room this half
        // of the row has spare beside a taller breakdown.
        let facts = Self::throughput_lines(stats);
        if !facts.is_empty() {
            card = card.child(Self::seam(theme));
            for fact in facts {
                card = card.child(Self::caption(theme, fact));
            }
        }
        Some(card.into_any_element())
    }

    /// A category's hue, off the status ramp both viewports paint from: merged
    /// is settled, an open pull request is review, a closed one is blocked.
    /// Nothing raised is ink — it is the absence of an outcome, not a state,
    /// and a fourth hue here would spend the ramp on a category that has none.
    fn landing_tone(theme: &Theme, kind: LandingKind) -> gpui::Hsla {
        match kind {
            LandingKind::Merged => theme.settled,
            LandingKind::Open => theme.accent,
            LandingKind::ClosedUnmerged => theme.danger,
            LandingKind::NoPr => Self::mark(theme, 0.18),
        }
    }

    /// What the work cost in time, and what it cost in friction — two lines,
    /// assembled only from facts that exist.
    ///
    /// A board with nothing ended has no rate and no median, and inventing an
    /// em dash for each would read as two failures rather than as a board that
    /// has only just started.
    fn throughput_lines(stats: &BoardStats) -> Vec<String> {
        let mut lines = Vec::new();

        let mut released: Vec<String> = vec![format!(
            "{} dispatch{}",
            stats.attempts,
            if stats.attempts == 1 { "" } else { "es" }
        )];
        if let Some(rate) = percent(stats.completion_rate) {
            released.push(format!("{rate} ended in done"));
        }
        if let Some(median) = stats.median_minutes.map(human_minutes) {
            released.push(format!("{median} median"));
        }
        if stats.live > 0 {
            released.push(format!("{} running now", stats.live));
        }
        released.push(match stats.agent_dispatched {
            n if n == stats.attempts => "all by agents".to_string(),
            0 => "all by you".to_string(),
            n => format!("{n} by agents"),
        });
        lines.push(released.join(" · "));

        let mut spent: Vec<String> = vec![format!(
            "{} of agent time",
            human_minutes(stats.total_minutes)
        )];
        if let Some(p90) = stats.p90_minutes {
            spent.push(format!("nine in ten within {}", human_minutes(p90)));
        }
        // Friction earns its words when there is any, and one honest phrase
        // when there is none — four zeroes would be four things to read that
        // all say nothing happened.
        let friction = stats.friction;
        if friction.is_clean() {
            spent.push("no friction".to_string());
        } else {
            if friction.retried_tasks > 0 {
                spent.push(format!("{} retried", friction.retried_tasks));
            }
            if friction.blocked_entries > 0 {
                spent.push(format!("{} stopped to ask", friction.blocked_entries));
            }
            if friction.early_settles > 0 {
                spent.push(format!("{} closed while working", friction.early_settles));
            }
            if friction.overruns > 0 {
                spent.push(format!("{} past their cap", friction.overruns));
            }
        }
        lines.push(spent.join(" · "));
        lines
    }

    /// The coverage sentence — what share of the window's attempts the totals
    /// above it actually account for.
    ///
    /// It rides in the spend card's footer, not in a panel of its own, because
    /// a total read without it is a total read wrong: attempts that predate the
    /// recording, and harnesses that meter nothing, are simply absent from the
    /// sums. An honest "62% of attempts reported usage" is worth more than a
    /// figure that quietly under-reports.
    fn coverage_note(stats: &BoardStats) -> Option<String> {
        // The fraction IS the share, so the percent beside it was the same
        // fact twice — and `18 of 18 (100%)` spent a whole line saying nothing
        // was missing.
        percent(stats.token_coverage)?;
        Some(format!(
            "From {} of {} attempts that reported usage.",
            stats.attempts_with_tokens, stats.attempts
        ))
    }

    /// The same window cut five ways, one axis at a time (gh#227).
    ///
    /// This was three fixed columns of dispatch counts, which answered the
    /// wrong question twice: a count per runtime says nothing about what the
    /// runtime cost, and the axis that matters most — the model — was not on
    /// the page at all. So the axis is a toggle, and every row carries the two
    /// numbers a reader is here for: what it spent and what that cost.
    ///
    /// Cut by model, this is also where the per-model table went (gh#252): the
    /// table was a second answer to the question this toggle's first segment
    /// already asks, drawn one card higher up the same page.
    ///
    /// Which is why a model nobody has a rate for keeps its row here (gh#359).
    /// This is the *usage* view: its tokens are a fact the board knows exactly,
    /// and the card that drops the biggest unpriced model is the card that
    /// hides the one worth noticing. Only the money is unknown, and only the
    /// money column says so.
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

        let head = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("Breakdown")),
            )
            // What the rows are ordered by, said out loud: the bar is drawn
            // against the same quantity, and on an unpriced window that is
            // tokens rather than money.
            .child(Self::caption(theme, cut.ranking.caption().to_string()))
            .child(div().flex_1())
            .child(Self::render_dimensions(stats, self.dimension, theme, cx));

        let mut card = Self::padded_card(theme)
            .flex_grow(3.0)
            .flex_basis(px(0.0))
            .min_w_0()
            .child(head)
            .child(
                div()
                    .flex()
                    .flex_col()
                    // Tighter than a card's own gap: these are rows of one
                    // table, not stacked facts.
                    .gap(px(9.0))
                    .children(
                        cut.rows
                            .iter()
                            .map(|row| Self::breakdown_row(theme, cut, row, priced)),
                    ),
            );
        if priced && unpriced > 0 {
            card = card.child(Self::caption(
                theme,
                format!(
                    "{} token(s) here ran on a model with no rate, and so are in no price above.",
                    human_tokens(unpriced)
                ),
            ));
        }
        Some(card.into_any_element())
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
        let mut row = Self::track(theme, TrackSize::Card);
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
                    .w(px(BREAKDOWN_LABEL))
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
                    .h(px(TRACK_HEIGHT))
                    .rounded(px(MARK_RADIUS))
                    .bg(Self::mark(theme, 0.07))
                    .child(
                        div()
                            .h_full()
                            .w(gpui::relative(fraction))
                            .rounded(px(MARK_RADIUS))
                            .bg(Self::mark(theme, 0.45)),
                    ),
            )
            .child(Self::cell(theme, Self::row_tokens(row), TOKENS_COL, false))
            .when(priced, |el| {
                el.child(
                    div()
                        .w(px(PRICE_COL))
                        .flex_none()
                        .text_right()
                        .truncate()
                        .text_size(px(Theme::TEXT_DENSE))
                        // The loudest thing on the row, because it is the one
                        // the card is opened for — except where the answer is
                        // a word about the absence of money (gh#359), which is
                        // said a tone back so a column of figures still reads
                        // as a column of figures.
                        .text_color(match row.is_unpriced() {
                            true => theme.text_subtle,
                            false => theme.text,
                        })
                        .child(SharedString::from(row.price_label())),
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
}

fn update_recovery_converged(aggregate: &AggregateBoardStats, device_id: &str) -> bool {
    aggregate.hosts.iter().any(|host| {
        host.device.device_id == device_id
            && matches!(
                host.status,
                comet_proto::view::stats::StatsHostStatus::Answered
                    | comet_proto::view::stats::StatsHostStatus::Duplicate
            )
    })
}

fn recovery_matches_selected_window(
    recovery_window: Option<i64>,
    selected_window: Option<i64>,
) -> bool {
    recovery_window == selected_window
}

fn apply_failure_may_be_restart(error: &comet_rpc::RpcError) -> bool {
    match error {
        comet_rpc::RpcError::Closed | comet_rpc::RpcError::Transport(_) => true,
        comet_rpc::RpcError::Failed(message) => {
            let message = message.to_ascii_lowercase();
            message.contains("connection closed")
                || message.contains("transport:")
                || message.contains("offline")
        }
        _ => false,
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
        // Which board, and which others there were (gh#254). Resolved once per
        // frame: the header, both empty states and the spend card are all
        // answering the same question, and three of them used to answer it by
        // not asking.
        let boards = self.boards(cx);
        let selection = self.resolved(cx);
        let all_boards = selection == Some(StatsSelection::All);
        let current: Option<String> = match selection {
            Some(StatsSelection::Board(ix)) => self.answers[ix].0.clone(),
            _ => None,
        };
        let stats = match selection {
            Some(StatsSelection::All) => self.aggregate.as_ref().map(|all| all.stats.clone()),
            Some(StatsSelection::Board(ix)) => Some(self.answers[ix].1.clone()),
            None => None,
        };
        let host = if all_boards {
            "all boards".to_string()
        } else {
            self.device_label(current.as_deref(), cx)
        };
        let picker = self.render_hosts(&boards, selection, &theme, cx);
        // The subtitle stops stating the host as a fact once a control names
        // it — the same sentence twice, one of them un-clickable, was half of
        // what made the wrong board so easy to believe.
        let subtitle = match (all_boards, picker.is_some()) {
            (true, _) => "What every reachable board did with the work it was given.".to_string(),
            (false, true) => {
                "What the board you are reading did with the work it was given.".to_string()
            }
            (false, false) => format!("What the board on {host} did with the work it was given."),
        };
        // The wide column the shared layer declares (gh#178) — this page is a
        // dashboard, and picking the width is a choice between two named ones
        // rather than a private copy of the column with a note attached.
        let mut column = widgets::dashboard_column().child(
            div()
                .flex()
                .flex_row()
                .items_end()
                .justify_between()
                .flex_wrap()
                .gap(px(16.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .min_w_0()
                        // The one dashboard headline in the app (gh#278): 20px
                        // over a 34px figure, where every other settings page
                        // is a section at 15.
                        .child(widgets::dashboard_header(&theme, "Board stats"))
                        .child(widgets::page_subtitle(&theme, subtitle)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .flex_wrap()
                        .justify_end()
                        .gap(px(8.0))
                        // Host first, then window: which board, then how much
                        // of it — the order the sentence under them reads in.
                        .when_some(picker, |el, picker| el.child(picker))
                        .child(self.render_windows(&theme, cx)),
                ),
        );

        if let Some(error) = self.error.clone() {
            column = column.child(widgets::error_strip(&theme, error));
        }

        if all_boards
            && let Some(note) = self
                .aggregate
                .as_ref()
                .and_then(AggregateBoardStats::completeness_note)
        {
            let upgrade = self.aggregate.as_ref().and_then(|aggregate| {
                aggregate.hosts.iter().find(|host| {
                    host.status == comet_proto::view::stats::StatsHostStatus::UpgradeRequired
                        && host
                            .upgrade
                            .as_ref()
                            .is_some_and(|upgrade| upgrade.can_apply)
                })
            });
            let mut strip = widgets::warning_strip(&theme, note);
            if let Some(host) = upgrade {
                let device_id = host.device.device_id.clone();
                let busy = self.updating.as_deref() == Some(device_id.as_str());
                strip = strip.child(
                    widgets::ghost_action(&theme)
                        .id("update-stats-host")
                        .ml_auto()
                        .when(busy, |el| el.opacity(0.5))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.update_stats_host(device_id.clone(), cx)
                        }))
                        .child(if busy { "Updating…" } else { "Update" }),
                );
            }
            column = column.child(strip);
        }

        // A pin whose board has stopped answering: the page falls back to the
        // sweep rather than showing nothing, and says which board it is
        // showing instead of quietly becoming the bug this ticket is about.
        let pinned_is_showing = match (&self.pinned, selection) {
            (Some(HostChoice::All), Some(StatsSelection::All)) => true,
            (Some(pin), Some(StatsSelection::Board(_))) => {
                pin.matches(current.as_deref(), self.local_device(cx).as_deref())
            }
            (None, _) => true,
            _ => false,
        };
        if self.swept && self.pinned.is_some() && !self.answers.is_empty() && !pinned_is_showing {
            column = column.child(widgets::warning_strip(
                &theme,
                format!("The board you picked did not answer. Showing the board on {host}."),
            ));
        }

        let Some(stats) = stats else {
            // Keyed on `swept`, because a furniture answer held by `resolve`
            // mid-sweep (gh#434) is the page still asking for something
            // better — which is "reading", not "no board answered".
            let note = if self.swept {
                "No board answered."
            } else {
                "Reading the board…"
            };
            return scroll_page(
                column.child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(note)),
                ),
            );
        };

        if stats.is_empty() {
            // A board with nothing in the window says so — and, when the sweep
            // found others, what they have (gh#254). Only the second half:
            // "nothing was dispatched here" is the line above it.
            let empty = div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .text_size(px(Theme::TEXT_BODY))
                .text_color(theme.text_subtle)
                .child(SharedString::from(if all_boards {
                    format!(
                        "No dispatches across any board in the {}. Release a task and this fills \
                         in.",
                        stats.window_label()
                    )
                } else {
                    format!(
                        "No dispatches on the board on {host} in the {}. Release a task and this \
                         fills in.",
                        stats.window_label()
                    )
                }))
                .when_some(
                    (!all_boards)
                        .then(|| elsewhere_note(&boards, current.as_deref()))
                        .flatten(),
                    |el, note| el.child(SharedString::from(note)),
                );
            return scroll_page(column.child(empty));
        }

        // The board that has attempts and no tokens on it — the state this
        // page read as "nothing metered to price" forever. The sentence names
        // the other candidates; the spend card is where a reader is standing
        // when they need it.
        let wrong_board = match (all_boards, stats.has_tokens()) {
            (true, _) => None,
            (false, true) => None,
            (false, false) => other_boards_note(&boards, current.as_deref()),
        };

        // Five cards in four rows, and the order is the argument: what it cost,
        // what that bought day by day, when and where it ran, and then — side
        // by side, because they are the two ways of asking what came of it —
        // the window cut whichever way the reader wants it, beside where the
        // work actually landed.
        column = column
            .child(Self::render_spend(&stats, wrong_board, &theme))
            .child(Self::render_chart(&stats, &theme))
            .child(Self::render_when_and_where(&stats, &theme));
        if let Some(row) = self.render_bottom_row(&stats, &theme, cx) {
            column = column.child(row);
        }

        scroll_page(column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recovery(status: comet_proto::view::stats::StatsHostStatus) -> AggregateBoardStats {
        AggregateBoardStats {
            since_days: Some(7),
            stats: BoardStats::empty(Some(7)),
            boards: vec![],
            hosts: vec![comet_proto::view::stats::StatsHost {
                device: comet_proto::view::stats::StatsDevice {
                    device_id: "peer".into(),
                    label: "Peer".into(),
                },
                status,
                board_id: None,
                error: None,
                upgrade: None,
            }],
            complete: false,
        }
    }

    #[test]
    fn update_recovery_waits_for_an_exact_new_protocol_answer() {
        use comet_proto::view::stats::StatsHostStatus::*;
        for transient in [NoBoard, Unreachable, Unreadable, UpgradeRequired] {
            assert!(!update_recovery_converged(&recovery(transient), "peer"));
        }
        assert!(!update_recovery_converged(&recovery(Answered), "other"));
        assert!(update_recovery_converged(&recovery(Answered), "peer"));
        assert!(update_recovery_converged(&recovery(Duplicate), "peer"));
    }

    #[test]
    fn old_window_recovery_cannot_overwrite_a_new_window_reload() {
        assert!(recovery_matches_selected_window(Some(7), Some(7)));
        assert!(!recovery_matches_selected_window(Some(7), Some(30)));
        assert!(!recovery_matches_selected_window(Some(7), None));
    }

    #[test]
    fn apply_refusal_is_immediate_but_a_lost_restart_reply_recovers() {
        assert!(!apply_failure_may_be_restart(
            &comet_rpc::RpcError::Refused("not update-managed".into())
        ));
        assert!(!apply_failure_may_be_restart(&comet_rpc::RpcError::Failed(
            "this install is not update-managed".into()
        )));
        assert!(apply_failure_may_be_restart(&comet_rpc::RpcError::Closed));
        assert!(apply_failure_may_be_restart(&comet_rpc::RpcError::Failed(
            "connection closed".into()
        )));
    }

    #[test]
    fn an_all_unpriced_spend_still_has_desktop_agent_rows() {
        let mut stats = BoardStats::empty(Some(7));
        stats.spend = Some(BoardSpend {
            rates: comet_proto::view::rates::builtin(),
            list_price: comet_proto::view::rates::Usd::ZERO,
            unpriced_tokens: 110,
            by_model: Vec::new(),
            unpriced: vec![comet_proto::view::stats::TokenTally {
                label: "unknown-model".into(),
                usage: comet_proto::TokenUsage {
                    input_tokens: 100,
                    output_tokens: 10,
                    ..Default::default()
                },
            }],
            accounts: Vec::new(),
        });
        stats.agent_usage = vec![AgentSpend {
            agent: comet_proto::AgentKind::Subagent,
            name: Some("Explore".into()),
            model: "unknown-model".into(),
            usage: comet_proto::TokenUsage {
                input_tokens: 100,
                output_tokens: 10,
                ..Default::default()
            },
            list_price_api_estimate: Some(comet_proto::view::rates::Usd::ZERO),
            unpriced_tokens: 110,
        }];
        assert!(!stats.spend.as_ref().expect("configured").has_price());
        assert_eq!(
            StatsPage::spend_agent_rows(&stats),
            Some(stats.agent_usage.as_slice())
        );
    }

    /// One candidate's answer. Only the attempt count separates them here —
    /// which board is which is the whole question. The evidence bit rides
    /// with the count: these tests' boards are either furniture or working,
    /// never merely idle this window.
    fn answer(target: Option<&str>, attempts: usize) -> (Option<String>, BoardStats) {
        let mut stats = BoardStats::empty(Some(7));
        stats.attempts = attempts;
        stats.dispatched = attempts > 0;
        (target.map(str::to_string), stats)
    }

    /// The default, and the behaviour every single-board install keeps: the
    /// sweep's own answer, which is the first candidate that replied with
    /// dispatch evidence.
    #[test]
    fn with_nothing_pinned_the_page_shows_the_sweep_s_answer() {
        let answers = vec![answer(None, 19), answer(Some("box"), 12)];
        assert_eq!(
            resolve(&answers, None, Some("mac"), true),
            Some(StatsSelection::Board(0))
        );
        // And nothing at all before anybody has answered.
        assert_eq!(resolve(&[], None, Some("mac"), true), None);
    }

    /// gh#434, in one assertion: a leftover local `board.db` that only ever
    /// collected rows answers first — local is first in sweep order — and
    /// loses anyway to the board the org actually works from.
    #[test]
    fn a_furniture_board_never_outranks_one_with_dispatch_evidence() {
        let answers = vec![answer(None, 0), answer(Some("box"), 12)];
        assert_eq!(
            resolve(&answers, None, Some("mac"), true),
            Some(StatsSelection::Board(1))
        );
        // Mid-sweep too: the box's answer paints the moment it lands.
        assert_eq!(
            resolve(&answers, None, Some("mac"), false),
            Some(StatsSelection::Board(1))
        );
    }

    /// A furniture answer is held while the sweep keeps asking, and settles
    /// only when every candidate has been asked — so a lone device, and a
    /// genuinely fresh install, still see their own board.
    #[test]
    fn a_furniture_answer_is_held_until_the_sweep_ends() {
        let answers = vec![answer(None, 0)];
        assert_eq!(resolve(&answers, None, Some("mac"), false), None);
        assert_eq!(
            resolve(&answers, None, Some("mac"), true),
            Some(StatsSelection::Board(0))
        );
    }

    /// The pin outranks the furniture rule: an operator who picked a board
    /// gets that board, whatever the evidence says. The rule decides only the
    /// sweep's own default.
    #[test]
    fn a_pin_beats_the_furniture_rule() {
        let answers = vec![answer(None, 0), answer(Some("box"), 12)];
        let here = HostChoice::ThisDevice;
        assert_eq!(
            resolve(&answers, Some(&here), Some("mac"), false),
            Some(StatsSelection::Board(0))
        );
    }

    #[test]
    fn all_boards_is_an_explicit_choice_not_the_sweeps_new_default() {
        let answers = vec![answer(None, 19), answer(Some("box"), 12)];
        assert_eq!(
            resolve(&answers, Some(&HostChoice::All), Some("mac"), true),
            Some(StatsSelection::All)
        );
        assert_eq!(
            resolve(&answers, None, Some("mac"), true),
            Some(StatsSelection::Board(0))
        );
    }

    /// The fix, in one assertion: the operator picked the box, so the box is
    /// what the page reads — even though the local board answers first and
    /// would otherwise win forever.
    #[test]
    fn a_pinned_board_beats_the_one_that_answered_first() {
        let answers = vec![answer(None, 19), answer(Some("box"), 12)];
        let pin = HostChoice::Device("box".into());
        assert_eq!(
            resolve(&answers, Some(&pin), Some("mac"), true),
            Some(StatsSelection::Board(1))
        );
    }

    /// A pin still being swept for holds the page rather than flashing the
    /// other board's numbers: mid-sweep is not evidence that the box is gone.
    #[test]
    fn a_pin_that_has_not_answered_yet_is_waited_for_and_then_given_up_on() {
        let answers = vec![answer(None, 19)];
        let pin = HostChoice::Device("box".into());
        assert_eq!(resolve(&answers, Some(&pin), Some("mac"), false), None);
        // Once every candidate has been asked, the sweep's answer is the best
        // there is — shown, and said out loud in a strip above it.
        assert_eq!(
            resolve(&answers, Some(&pin), Some("mac"), true),
            Some(StatsSelection::Board(0))
        );
    }

    /// The local board answers to both of its spellings — the absent target
    /// the sweep asks it by, and its own device id — so a pin written either
    /// way finds it.
    #[test]
    fn this_device_is_the_same_board_under_either_name() {
        let answers = vec![answer(Some("box"), 12), answer(None, 19)];
        let here = HostChoice::ThisDevice;
        assert_eq!(
            resolve(&answers, Some(&here), Some("mac"), true),
            Some(StatsSelection::Board(1))
        );
        let by_id = HostChoice::Device("mac".into());
        assert_eq!(
            resolve(&answers, Some(&by_id), Some("mac"), true),
            Some(StatsSelection::Board(1))
        );
        // And picking the local candidate is remembered as `ThisDevice`, so it
        // survives the device being registered again under a new id.
        assert_eq!(HostChoice::of(None, Some("mac")), HostChoice::ThisDevice);
        assert_eq!(
            HostChoice::of(Some("mac"), Some("mac")),
            HostChoice::ThisDevice
        );
        assert_eq!(
            HostChoice::of(Some("box"), Some("mac")),
            HostChoice::Device("box".into())
        );
    }

    /// The choice persists — the exit condition this ticket names, and the
    /// difference between a fix and a click to repeat every launch.
    #[test]
    fn the_pick_round_trips_through_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let prefs = StatsPrefs {
            host: Some(HostChoice::Device("box".into())),
        };
        prefs.save(dir.path()).unwrap();
        assert_eq!(StatsPrefs::load(dir.path()), prefs);
        // Never `ui-settings.json`: the shell owns that file and saves it from
        // a copy held since boot.
        assert!(StatsPrefs::path(dir.path()).ends_with("stats-prefs.json"));
    }

    /// A missing or corrupt file follows the sweep rather than refusing to
    /// draw. Nothing here is worth an error strip.
    #[test]
    fn a_missing_or_corrupt_file_falls_back_to_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(StatsPrefs::load(dir.path()), StatsPrefs::default());
        assert_eq!(StatsPrefs::load(dir.path()).host, None);
        std::fs::write(StatsPrefs::path(dir.path()), "{not json").unwrap();
        assert_eq!(StatsPrefs::load(dir.path()), StatsPrefs::default());
    }
}
