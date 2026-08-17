//! The review surface (§gh#180): what changed, not which lines.
//!
//! The board dispatched the brief, so only the board can put what you asked for
//! next to what the agent says it did next to what actually changed. That is
//! the one question GitHub cannot answer about a run, and it is the only
//! question this screen asks. It is deliberately **not** a diff viewer: a diff
//! viewer here would be a worse GitHub, and it would be built around an
//! activity that stops scaling the moment a fleet generates more code than one
//! person can read.
//!
//! ## Four ideas, and the third is the product
//!
//! **The layout is inverted, and the inversion is the argument.** Everywhere
//! else in this app the chat is the content and the diff is the reference in a
//! dock. During review that is backwards: the review takes the main card and
//! the authoring session becomes the narrow column beside it — in the dock's
//! own slot on the right (gh#276), because the reference is what goes there
//! and during a review the session is the reference. The shell owns that half
//! (`shell::Route::Review`) — what this module renders is the card itself, and
//! `docs/design/review.md` is the list of claims it is checked against.
//!
//! **Prose alone marks its own homework.** A summary written by the model that
//! wrote the code inherits its blind spots: a misunderstanding comes back
//! described fluently and confidently. So no claim on this screen is ever drawn
//! alone. Every one of them carries, underneath it, the diff facts for the
//! files it anchored — a status letter and a line count that came from git and
//! not from the agent — and the run's own evidence block says what was executed
//! and how it exited. Where a claim's anchors match nothing, that is drawn too,
//! because work described that did not happen is as interesting as work nobody
//! described.
//!
//! **The effects row comes before the claims, and that order is the argument**
//! (§gh#236). A reader who has already been told a fluent story reads numbers
//! underneath it as confirmation of the story; the same numbers read first are
//! what the story then has to agree with. Every chip on that row —
//! `Tests 41 → 47, all passing`, `Public API unchanged`, `1 dependency added` —
//! is derived by [`comet_board::effects`] from the checkout and the run
//! journal, and a fact the board could not establish wears the unknown ground
//! rather than the reassuring one. The same goes for a claim: `✓` is reserved
//! for one something the agent did not author stands behind, and a claim
//! nothing checks says `no test covers this` and drops to a dot.
//!
//! **Unclaimed changes are the product, and they are the only thing here that
//! shouts.** One hue on this screen means "look at this", and it is the ramp's
//! blocked hue (gh#173) — the verdict strip under the header wears it, the
//! remainder block wears it, and the inline `!` marks that flag a contradiction
//! wear it. Nothing else on the page is allowed to; a screen where three things
//! shout has nothing left to shout with. The single carve-out is the deletion
//! count in the diff strip (§gh#238), where the hue belongs to the *minus sign*
//! rather than to the screen: `−33` is arithmetic, every diff in this app and
//! every other one paints it red, and a grey minus would be the only one that
//! is not.
//!
//! ## Two ways out, and neither draws a diff
//!
//! The screen is not a diff viewer and it will not become one, but the diff has
//! to be one click away or the refusal is just a missing feature (§gh#238). So
//! the body closes with a strip that says *how much* moved — `3 files changed`,
//! `+117`, `−33`, summed off the same `changed` list the remainder is derived
//! from — beside a `Read the diff` chip that hands the reader to
//! [`crate::changes`], which is the module that does draw diffs. The chip
//! leaves by event ([`ReviewEvent`]) rather than reaching for the route: a card
//! that navigated the shell would be a card that owns the window.
//!
//! And the header says whose turn it is. A review that is waiting for a human
//! and one that has already been answered are otherwise the same screen — same
//! verdict strip, same claims, same bar — which was survivable while the
//! verdict had to be written on GitHub and stopped being so the moment it could
//! be sent from here (§gh#239). The pill is the difference.
//!
//! **The reading is not this module's.** Whether a review is alarming, and the
//! sentence that says so, come from [`comet_board::claims`] —
//! [`AttemptReview::verdict`] and [`AttemptReview::findings`] — which the
//! `comet-board review` terminal output reads from as well. Two surfaces that
//! phrased the same attempt for themselves would eventually disagree about it,
//! and the one place that must never happen is the surface whose whole job is
//! to be trusted about what a run did.
//!
//! ## Where the numbers come from
//!
//! `board.db` lives on whichever device hosts the board, so this panel sweeps
//! [`comet_proto::view::board::host_candidates`] for the device that answers
//! `ReadAttemptReview` — the same contract the stats page and the routing page
//! use, and the reason a laptop can review the box's work without an ssh
//! account on it. The diff is read on that host, from the attempt's checkout or
//! from the snapshot the board took while the run was live; a review with
//! neither says why rather than rendering an empty diff.

use gpui::{
    AnyElement, Context, Entity, ScrollHandle, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};

use comet_board::claims::{
    AnchorKind, AttemptReview, ChangedFile, ClaimMark, ClaimView, DiffSource, FindingKind, Tone,
    Verdict, anchor_kind,
};
use comet_board::effects::{Chip, Ground};
use comet_board::verdict::{self, VerdictKind, VerdictReceipt};
use comet_proto::view::board;
use comet_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::loaders;
use crate::motion;
use crate::popover;
use crate::state::AppState;
use crate::theme::Theme;

/// How tall the brief is allowed to grow before it scrolls inside itself.
///
/// The brief is the question and it belongs at the top, but an issue body is
/// unbounded prose and this one is 60 lines. Capped rather than folded by
/// default: a reviewer who cannot see what was asked cannot judge what was
/// answered, so the first screenful is always there.
const BRIEF_MAX_H: f32 = 220.0;

/// The gutter between the status letter and the path in every file row, and
/// between the path and its line counts. One number, so the claim rows and the
/// unclaimed rows line up as one table even though they are drawn by different
/// functions.
const FILE_GUTTER: f32 = 10.0;

/// How tall the delivery preview is allowed to be before it fades out
/// (§gh#239, review.md I6). Enough for the header, a verdict, a sentence and a
/// couple of unclaimed lines — the shape of the payload rather than all of it,
/// which is what the fade says.
const PREVIEW_MAX_H: f32 = 196.0;

/// Where the preview starts fading, as the design gives it: a mask from 72% of
/// the card's height to its bottom edge. Expressed as the band
/// [`crate::edge_fade`] wants, so the number that moves is the design's one.
const PREVIEW_FADE_AT: f32 = 0.72;

/// The payload's own leading, in the mono 11/17 the design specifies.
const PREVIEW_LINE_H: f32 = 17.0;

/// The three verdicts, in the order the bar draws them: quietest first, so the
/// one that interrupts an agent is not the one under the cursor by default.
const VERDICTS: [(VerdictKind, &str); 3] = [
    (VerdictKind::Comment, "Comment"),
    (VerdictKind::Approve, "Approve"),
    (VerdictKind::ChangesRequested, "Request changes"),
];

/// One chip on the effects row, and under a claim (§gh#236). A row of these is
/// the densest thing on the screen, so the numbers are the design's: 22 high,
/// 8 of side padding, a 5px dot.
const CHIP_H: f32 = 22.0;
const CHIP_PAD: f32 = 8.0;
const CHIP_DOT: f32 = 5.0;

/// How hard a tinted chip's ground sits under its text. The design's 12%, and
/// the same weight the verdict strip's tint uses — a chip that shouted louder
/// than the verdict would be arguing with it.
const CHIP_TINT: f32 = 0.12;

/// The turn pill in the header (§gh#238), as the design gives it: `1px 8px` of
/// padding on a 14% tint of its own hue. Two off the chip row's 12%, because
/// this one has no dot to carry the colour and the text is all there is.
const PILL_PX: f32 = 8.0;
const PILL_PY: f32 = 1.0;
const PILL_TINT: f32 = 0.14;

/// The diff strip that closes the body (§gh#238): `9px 12px` inside a hairline
/// row. Tighter vertically than a section, because it is a signpost and not a
/// section — the reading is over by the time a reader reaches it. The unclaimed
/// block's rows take the same measures, so the two read as one table.
const STRIP_PX: f32 = 12.0;
const STRIP_PY: f32 = 9.0;

/// The label column every band shares (review.md C2/D2): "Asked for",
/// "Effects", "Evidence". One width, so the three bodies start on one line —
/// which is what makes three different kinds of fact read as one column.
const LABEL_W: f32 = 62.0;

/// A band's own vertical padding, on the card's [`Theme::SPACE_LG`] sides.
const BAND_PY: f32 = 11.0;

/// The canvas's one gutter that is not on the spacing scale: between a band's
/// label and its body, between the facts in the header, and between the cells
/// of a row. Named rather than repeated, because it is one measure.
const BAND_GAP: f32 = 10.0;

/// A claim's chip (review.md E7): one step under the effects row's, and with
/// no dot. The claims are a column of cards and the chips inside them are
/// annotations on a sentence; at the effects row's weight they would compete
/// with the sentence they annotate.
const CLAIM_CHIP_H: f32 = 20.0;
const CLAIM_CHIP_PAD: f32 = 7.0;

/// The claim card's glyph column, and how far its glyph sits below the cap
/// height of the sentence beside it.
const GLYPH_W: f32 = 14.0;
const GLYPH_NUDGE: f32 = 2.0;

/// The unclaimed block's three weights of its own hue (review.md F1/F2): the
/// border, the header's bed, and the hairline under the header. One hue at
/// three strengths rather than three colours.
const ALARM_BORDER: f32 = 0.32;
const ALARM_BED: f32 = 0.09;
const ALARM_RULE: f32 = 0.22;

/// How loud the header's aside is against the header it sits in.
const ALARM_ASIDE: f32 = 0.75;

/// A header control (review.md B2): the links out, at the titlebar's own
/// icon-button size.
const ICON_BTN: f32 = 24.0;
const ICON_GLYPH: f32 = 14.0;

/// A fact's glyph on the header's second row (review.md B5).
const FACT_GLYPH: f32 = 12.0;

/// A verdict control in the bar (review.md H4).
const VERDICT_H: f32 = 28.0;
const VERDICT_PX: f32 = 11.0;

/// One cell of the loading wave (gh#341). Eight puts the row at 56px across —
/// the boot splash's mark is 64 tall, and this is the same gesture one step
/// quieter, because a card that is about to fill is not a window that is empty.
const LOADER_CELL: f32 = 8.0;

/// What the card says while it is still reading. One sentence, kept from before
/// the wave was under it: the shimmer says a wait is happening and this says
/// whose.
const LOADING_LINE: &str = "Reading the review…";

/// Dev/measurement knob (`COMET_REVIEW_HOLD_MS`, default unset): holds the
/// review's read for this many milliseconds before it is issued, so the loading
/// state can be photographed rather than raced — the same job
/// [`motion::speed_scale`] does for the tweens. Read once; never set in
/// production. Capped at ten seconds so a typo cannot wedge the card.
fn loading_hold() -> Option<std::time::Duration> {
    static HOLD: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    (*HOLD.get_or_init(|| {
        std::env::var("COMET_REVIEW_HOLD_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(|ms| ms.min(10_000))
    }))
    .map(std::time::Duration::from_millis)
}

/// What the review card asks the shell to do, because it cannot do it itself.
///
/// The same shape [`crate::board::BoardEvent`] has, and for the same reason: a
/// panel that reached over and set the route would be a dock that owns the
/// window.
#[derive(Debug, Clone)]
pub enum ReviewEvent {
    /// The `Read the diff` chip (§gh#238). The card does not draw a diff and
    /// will not learn to; this is the hand-off to [`crate::changes`], which
    /// reads the live checkout of whichever chat is selected — so the chat to
    /// select travels with the request rather than being guessed at the far
    /// end.
    ReadTheDiff { chat_id: String },
}

/// The review's name in the tab strip (`docs/design/window.md` B10):
/// `Review · gh#138`.
///
/// The brief is the source of the identifier, but the tab exists from the first
/// frame of the route and the brief arrives an RPC later — so a review still
/// loading names itself off the task id the route carries rather than showing a
/// nameless tab and then a titled one. Pure, and the fallback is exact:
/// `gh:owner/repo#138` is `gh#138`, `gh:owner/repo!212` is `gh!212`, and
/// `linear:LIN-142` is `LIN-142`.
pub(crate) fn review_tab_title(identifier: Option<&str>, task_id: &str) -> String {
    let name = match identifier.filter(|id| !id.is_empty()) {
        Some(id) => id.to_string(),
        None => task_identifier(task_id),
    };
    format!("Review · {name}")
}

/// Whether the merge key is drawn at all (gh#408): there is a pull request to
/// merge, and the work has not already landed.
///
/// Deliberately not gated on [`board::landing`] being `Ready`: the board's
/// reservation rides *inside* the confirmation instead, because GitHub
/// evaluates the rules at execution time and a board that hid the key on a
/// stale poll would be refusing a merge GitHub takes. Done is different — a
/// merged pull request has nothing left to press.
pub(crate) fn merge_available(review: &AttemptReview) -> bool {
    review.pr_url.is_some() && review.state != board::BoardState::Done.as_str()
}

/// The identifier a task id spells — `<source>:<scope><sep><number>` keeps the
/// source and the number, and anything this shape cannot parse is returned as
/// it came (a made-up identifier would be worse than a literal id).
fn task_identifier(task_id: &str) -> String {
    let Some((source, rest)) = task_id.split_once(':') else {
        return task_id.to_string();
    };
    match rest.find(['#', '!']) {
        Some(ix) if ix + 1 < rest.len() => format!("{source}{}", &rest[ix..]),
        _ => rest.to_string(),
    }
}

/// One attempt's review, fetched and drawn.
pub struct ReviewPanel {
    state: Entity<AppState>,
    /// The board task this review is of. Set once; a different task is a
    /// different panel (the shell replaces it rather than re-pointing it, so a
    /// stale reply can never land on the wrong review).
    task_id: String,
    /// The chat that authored this attempt, where there still is one. Held for
    /// one purpose: [`ReviewEvent::ReadTheDiff`] needs a session to select, and
    /// a review outlives its chat — so a review with none has no diff to open
    /// and says so by drawing the counts without the chip.
    chat_id: Option<String>,
    /// Which attempt, or the task's latest. Held so a later "previous attempt"
    /// affordance has somewhere to write.
    attempt: Option<i64>,
    review: Option<AttemptReview>,
    /// The device that answered. `None` before the first reply, and on a board
    /// hosted right here.
    host: Option<String>,
    loaded: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    body_scroll: ScrollHandle,
    brief_scroll: ScrollHandle,
    // -- the verdict bar (§gh#239) -------------------------------------------
    /// What the reviewer is writing.
    comment: Entity<ComposerInput>,
    /// Which verdict the bar is armed with. Held rather than decided at the
    /// click, because the preview has to show the payload *before* submit and
    /// a payload that did not know its own verdict would be a preview of
    /// something else.
    kind: VerdictKind,
    submitting: bool,
    /// What the last submission did — where it went, and what it could not
    /// reach. Kept on screen: "recorded, and the chat is gone" and "recorded,
    /// and GitHub would not take it" (gh#365) are outcomes a reviewer has to
    /// know about, and neither is an error.
    receipt: Option<VerdictReceipt>,
    submit_error: Option<SharedString>,
    submit_task: Option<Task<()>>,
    // -- the merge key (gh#408) ----------------------------------------------
    /// The confirm is up. The one control on this screen that interposes a
    /// dialog, because it is the one action that cannot be undone — and for a
    /// layer of a stack the dialog names every open layer that merges along.
    merge_confirming: bool,
    merging: bool,
    /// What the merge answered — the engine's sentence, kept on screen the way
    /// the verdict's receipt is: `o/r#87 merged` and `o/r#87 is in the merge
    /// queue` are both outcomes a reviewer has to know about, and only one of
    /// them moves the row.
    merge_line: Option<SharedString>,
    merge_error: Option<SharedString>,
    merge_task: Option<Task<()>>,
    /// The comment box's events: every edit, so the preview follows the
    /// typing, and Enter, which submits here as it does everywhere else in
    /// this app (shift-Enter is the newline).
    _edits: Subscription,
}

impl ReviewPanel {
    pub fn new(
        state: Entity<AppState>,
        task_id: String,
        chat_id: Option<String>,
        attempt: Option<i64>,
        cx: &mut Context<Self>,
    ) -> Self {
        let comment = cx.new(|cx| {
            ComposerInput::new(
                "What is wrong, or what is right — the agent reads this.",
                cx,
            )
        });
        // The preview promises "this is what will be sent", which is only true
        // if it moves with the sentence being written.
        let edits = cx.subscribe(
            &comment,
            |panel: &mut Self, _, event: &ComposerInputEvent, cx| match event {
                ComposerInputEvent::Edited(_) => cx.notify(),
                ComposerInputEvent::Submitted => panel.submit(cx),
                _ => {}
            },
        );
        let mut panel = Self {
            state,
            task_id,
            chat_id,
            attempt,
            review: None,
            host: None,
            loaded: false,
            error: None,
            task: None,
            body_scroll: ScrollHandle::new(),
            brief_scroll: ScrollHandle::new(),
            comment,
            kind: VerdictKind::Comment,
            submitting: false,
            receipt: None,
            submit_error: None,
            submit_task: None,
            merge_confirming: false,
            merging: false,
            merge_line: None,
            merge_error: None,
            merge_task: None,
            _edits: edits,
        };
        panel.reload(cx);
        panel
    }

    /// Which task this panel is showing — the shell's check before it decides
    /// whether a navigation is a re-point or a new panel.
    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// What the tab strip calls this review (review.md A7): `Review · gh#138`,
    /// off the same identifier the card's own header leads with — the tab and
    /// the card have to be naming one thing.
    pub fn tab_title(&self) -> SharedString {
        SharedString::from(review_tab_title(
            self.review.as_ref().map(|r| r.brief.identifier.as_str()),
            &self.task_id,
        ))
    }

    /// Read the review, sweeping for the device that hosts the board.
    ///
    /// A candidate that errors has answered "I host no board" — the engine's
    /// contract for every board method — so the sweep moves on. When nobody
    /// answers, the last error is what the panel shows.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
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
        let task_id = self.task_id.clone();
        let attempt = self.attempt;
        self.error = None;
        let hold = loading_hold();
        self.task = Some(cx.spawn(async move |this, cx| {
            if let Some(hold) = hold {
                cx.background_executor().timer(hold).await;
            }
            let mut last: Option<String> = None;
            for candidate in candidates {
                let mut params = serde_json::json!({ "taskId": task_id });
                if let Some(object) = params.as_object_mut() {
                    if let Some(attempt) = attempt {
                        object.insert("attempt".into(), serde_json::json!(attempt));
                    }
                    if let Some(host) = candidate.as_deref() {
                        object.insert("targetDeviceId".into(), serde_json::json!(host));
                    }
                }
                match engine
                    .client()
                    .call(methods::READ_ATTEMPT_REVIEW, params)
                    .await
                {
                    Ok(value) => {
                        let parsed = serde_json::from_value::<AttemptReview>(value);
                        let _ = this.update(cx, |panel, cx| {
                            panel.loaded = true;
                            match parsed {
                                Ok(review) => {
                                    panel.host = candidate;
                                    panel.review = Some(review);
                                }
                                Err(err) => {
                                    panel.error = Some(format!("Unreadable review: {err}").into());
                                }
                            }
                            cx.notify();
                        });
                        return;
                    }
                    Err(err) => last = Some(err.to_string()),
                }
            }
            let _ = this.update(cx, |panel, cx| {
                panel.loaded = true;
                panel.error = Some(
                    last.unwrap_or_else(|| "No device on this account hosts a board".into())
                        .into(),
                );
                cx.notify();
            });
        }));
    }

    /// Submit the armed verdict (§gh#239): the verdict recorded, one prompt
    /// into the checkout the agent is still in, and one review projected onto
    /// the pull request — in that order (gh#365).
    ///
    /// Sent to the device that answered the read, not swept for again: this is
    /// a write, and a write that wandered to a second board would be a verdict
    /// posted about somebody else's row. The unclaimed changes are not sent —
    /// they are derived on that host from the diff, so the payload cannot
    /// disagree with the screen that promised it.
    fn submit(&mut self, cx: &mut Context<Self>) {
        if self.submitting {
            return;
        }
        let comment = self.comment.read(cx).text().trim().to_string();
        if self.kind.needs_comment() && comment.is_empty() {
            self.submit_error = Some("Write something first — an empty verdict is not one.".into());
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.submit_error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let mut params = serde_json::json!({
            "taskId": self.task_id,
            "kind": self.kind.as_str(),
            "comment": comment,
        });
        if let Some(object) = params.as_object_mut() {
            if let Some(review) = &self.review {
                object.insert("attempt".into(), serde_json::json!(review.attempt));
            }
            if let Some(host) = self.host.as_deref() {
                object.insert("targetDeviceId".into(), serde_json::json!(host));
            }
        }
        self.submitting = true;
        self.submit_error = None;
        self.receipt = None;
        cx.notify();
        self.submit_task = Some(cx.spawn(async move |this, cx| {
            let reply = engine.client().call(methods::SUBMIT_VERDICT, params).await;
            let _ = this.update(cx, |panel, cx| {
                panel.submitting = false;
                match reply.map(serde_json::from_value::<VerdictReceipt>) {
                    Ok(Ok(receipt)) => {
                        // The verdict is sent; the box is for the next one. A
                        // resend would be refused as the same submission
                        // anyway, but leaving it there invites the click.
                        //
                        // Unless GitHub would not take it (gh#365). The verdict
                        // stands and the agent has it, but nobody reading the
                        // pull request does — so the words stay where the
                        // reviewer can retry them or paste them there by hand.
                        // The retry is the same submission and posts nothing
                        // twice; it finishes the projection.
                        if !verdict::keeps_the_comment(&receipt) {
                            panel
                                .comment
                                .update(cx, |input, cx| input.set_text(String::new(), cx));
                        }
                        panel.receipt = Some(receipt);
                    }
                    Ok(Err(err)) => {
                        panel.submit_error = Some(format!("Unreadable receipt: {err}").into());
                    }
                    Err(err) => panel.submit_error = Some(err.to_string().into()),
                }
                cx.notify();
            });
        }));
    }

    /// The one key that cannot be undone (gh#408): merge the pull request, on
    /// the device that answered the read — a write, like the verdict's, and a
    /// merge that wandered to a second board would land somebody else's row.
    ///
    /// Only ever reached through the confirm: the sentence the reader agreed
    /// to is [`board::merge_confirmation`]'s, which for a layer of a stack has
    /// already named every open layer below as cargo. The engine adds no
    /// second question.
    fn merge(&mut self, cx: &mut Context<Self>) {
        if self.merging {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.merge_error = Some("Engine not connected".into());
            self.merge_confirming = false;
            cx.notify();
            return;
        };
        let mut params = serde_json::json!({ "taskId": self.task_id });
        if let Some(object) = params.as_object_mut()
            && let Some(host) = self.host.as_deref()
        {
            object.insert("targetDeviceId".into(), serde_json::json!(host));
        }
        self.merging = true;
        self.merge_confirming = false;
        self.merge_error = None;
        self.merge_line = None;
        cx.notify();
        self.merge_task = Some(cx.spawn(async move |this, cx| {
            let reply = engine.client().call(methods::MERGE_TASK, params).await;
            let _ = this.update(cx, |panel, cx| {
                panel.merging = false;
                match reply {
                    Ok(value) => {
                        let line = value
                            .get("line")
                            .and_then(|v| v.as_str())
                            .unwrap_or("merged")
                            .to_string();
                        panel.merge_line = Some(line.into());
                        // A merge that landed moved the row's state and its
                        // stack's shape; re-read rather than guess which of
                        // the screen's facts moved with it.
                        panel.reload(cx);
                    }
                    Err(err) => panel.merge_error = Some(err.to_string().into()),
                }
                cx.notify();
            });
        }));
    }

    // -- paint --------------------------------------------------------------

    /// The one hue that means "look at this" on this screen (gh#173's blocked
    /// hue), and the quiet tones for everything that does not.
    fn tone_color(tone: Tone, theme: &Theme) -> gpui::Hsla {
        match tone {
            Tone::Alarm => theme.danger,
            Tone::Settled => theme.settled,
            Tone::Unknown => theme.text_subtle,
        }
    }

    /// `·` for a fact, `!` for a contradiction. Two glyphs, and the second one
    /// is only ever painted in [`Tone::Alarm`]'s colour.
    fn mark(loud: bool, theme: &Theme) -> AnyElement {
        div()
            .flex_none()
            .w(px(10.0))
            .font_family(theme.font_mono.clone())
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(if loud { theme.danger } else { theme.text_faint })
            .child(SharedString::from(if loud { "!" } else { "·" }))
            .into_any_element()
    }

    /// A claim's own glyph (§gh#236): `!`, `✓` or `·`.
    ///
    /// The third state is the one that had to exist. A claim the diff supports
    /// and nothing corroborates is neither contradicted nor verified, and
    /// letting it borrow the tick would be this screen doing the flattening it
    /// was built to undo — so the tick is reserved for a claim something the
    /// agent did not author stands behind, and everything else drops to a dot
    /// in [`Theme::text_subtle`].
    fn claim_glyph(mark: ClaimMark, theme: &Theme) -> AnyElement {
        div()
            .flex_none()
            .w(px(GLYPH_W))
            .pt(px(GLYPH_NUDGE))
            .flex()
            .justify_center()
            .font_family(theme.font_mono.clone())
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(Self::claim_color(mark, theme))
            .child(SharedString::from(mark.glyph()))
            .into_any_element()
    }

    /// What colour a claim's glyph is. Only the contradicted one is allowed
    /// the screen's loud hue.
    fn claim_color(mark: ClaimMark, theme: &Theme) -> gpui::Hsla {
        match mark {
            ClaimMark::Contradicted => theme.danger,
            ClaimMark::Corroborated => theme.settled,
            ClaimMark::Bare => theme.text_subtle,
        }
    }

    /// The two colours one chip is painted in: its dot, and its text.
    ///
    /// The grounds come from [`comet_board::effects`], which is where the
    /// meaning is decided; this only says which of the theme's existing hues
    /// each one wears. None of them is [`Theme::danger`] — the review's one
    /// loud hue belongs to the unclaimed set, and a chip row competing with it
    /// would leave the screen with nothing to shout with.
    fn chip_colors(ground: Ground, theme: &Theme) -> (gpui::Hsla, gpui::Hsla) {
        match ground {
            // A checked fact that is not news: a settled dot, so the row reads
            // at a glance as "the board looked", with quiet text.
            Ground::Neutral => (theme.settled, theme.text_muted),
            Ground::Settled => (theme.settled, theme.settled),
            Ground::Working => (theme.warning, theme.warning),
            // The one a reader must not mistake for either: no status colour at
            // all, because there is no status.
            Ground::Unknown => (theme.text_faint, theme.text_subtle),
        }
    }

    /// One chip: a 5px dot, and the sentence the board phrased.
    ///
    /// A tinted ground for the two chips that carry a status, the flat chip
    /// wash for the rest — so a row of five reads as one row with one thing in
    /// it, rather than five things of equal weight.
    fn chip(chip: &Chip, theme: &Theme) -> AnyElement {
        let (dot, text) = Self::chip_colors(chip.ground, theme);
        let tinted = matches!(chip.ground, Ground::Settled | Ground::Working);
        div()
            .flex_none()
            .h(px(CHIP_H))
            .px(px(CHIP_PAD))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .rounded(px(Theme::RADIUS_CHIP))
            .bg(if tinted {
                dot.opacity(CHIP_TINT)
            } else {
                theme.wash(0.08)
            })
            // round-ok: a status dot — the design's 5px, one per chip.
            .child(div().flex_none().size(px(CHIP_DOT)).rounded_full().bg(dot))
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(text)
                    .child(SharedString::from(chip.text.clone())),
            )
            .into_any_element()
    }

    /// A row of chips that wraps, because the effects row is five sentences and
    /// the card is not always wide. The canvas's `6px 8px`: tighter between
    /// wrapped lines than along one, so a wrapped row still reads as a row.
    fn chip_row(chips: &[Chip], theme: &Theme) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_y(px(6.0))
            .gap_x(px(Theme::SPACE_SM))
            .children(chips.iter().map(|chip| Self::chip(chip, theme)))
            .into_any_element()
    }

    /// A chip under a claim (review.md E7/E8): 20 high, no dot.
    ///
    /// The same two grounds the effects row uses, said with the fill alone —
    /// a dot on a chip this size is a speck, and the row it sits in is already
    /// indented under a glyph that carries the claim's own state.
    fn claim_chip(chip: &Chip, theme: &Theme) -> AnyElement {
        let (hue, text) = Self::chip_colors(chip.ground, theme);
        let tinted = matches!(chip.ground, Ground::Settled | Ground::Working);
        div()
            .flex_none()
            .h(px(CLAIM_CHIP_H))
            .px(px(CLAIM_CHIP_PAD))
            .flex()
            .items_center()
            .rounded(px(Theme::RADIUS_CHIP))
            .bg(if tinted {
                hue.opacity(CHIP_TINT)
            } else {
                theme.wash(0.08)
            })
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(text)
            .child(SharedString::from(chip.text.clone()))
            .into_any_element()
    }

    /// One anchor, as the claim named it (review.md E8): the `--chip` wash,
    /// mono, at the claim chip's measures so the two kinds line up as one row.
    fn anchor_chip(anchor: &str, theme: &Theme) -> AnyElement {
        div()
            .flex_none()
            .h(px(CLAIM_CHIP_H))
            .px(px(CLAIM_CHIP_PAD))
            .flex()
            .items_center()
            .rounded(px(Theme::RADIUS_CHIP))
            .bg(theme.wash(0.08))
            .font_family(theme.font_mono.clone())
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(theme.text_subtle)
            .child(SharedString::from(anchor.to_string()))
            .into_any_element()
    }

    /// A full-bleed band (review.md C/D): a label in a fixed column, and a
    /// body that starts where every other band's body starts.
    ///
    /// The card's top half is bands and its bottom half is inset blocks, and
    /// that is the whole of its rhythm. A band is told from its neighbour by a
    /// hairline and — for the brief — by tone; never by a heading, which is
    /// what the uppercase labels used to be.
    fn band(
        theme: &Theme,
        label: &'static str,
        nudge: f32,
        raised: bool,
        body: AnyElement,
    ) -> AnyElement {
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(BAND_GAP))
            .px(px(Theme::SPACE_LG))
            .py(px(BAND_PY))
            .border_b_1()
            .border_color(theme.border)
            .when(raised, |el| el.bg(theme.surface_raised))
            .child(
                div()
                    .flex_none()
                    .w(px(LABEL_W))
                    .pt(px(nudge))
                    .text_size(px(Theme::TEXT_CAPTION))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(label)),
            )
            .child(div().flex_1().min_w_0().child(body))
            .into_any_element()
    }

    /// One changed file, as git reports it: the status letter, the path, and
    /// what moved. The row a claim's anchors and the unclaimed set share, so
    /// the evidence under a claim and the remainder read as one table.
    fn file_row(file: &ChangedFile, theme: &Theme, loud: bool) -> AnyElement {
        let path_color = if loud { theme.text } else { theme.text_muted };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(FILE_GUTTER))
            .font_family(theme.font_mono.clone())
            .text_size(px(Theme::TEXT_CAPTION))
            .child(
                div()
                    .flex_none()
                    .w(px(9.0))
                    .text_color(if loud { theme.danger } else { theme.text_faint })
                    .child(SharedString::from(file.status.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(path_color)
                    .child(SharedString::from(file.path.clone())),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(theme.text_faint)
                    .child(SharedString::from(file.counts())),
            )
            .into_any_element()
    }

    /// The evidence under one claim: the changed files its anchors reached,
    /// spelled exactly as the remainder spells them.
    ///
    /// `changed` is the review's own diff, looked up by path rather than
    /// carried on the claim: `matched` is a list of paths, and a reviewer needs
    /// the counts beside them or the "evidence" is the agent's own words again
    /// with a filename attached.
    fn claim_row(
        review: &AttemptReview,
        claim: &ClaimView,
        changed: &[ChangedFile],
        theme: &Theme,
    ) -> AnyElement {
        // A claim nothing in the diff supports is the screen's second-loudest
        // row, after the unclaimed set itself.
        let unsupported = !claim.anchored();
        let mark = review.claim_mark(claim);
        let chips = review.claim_chips(claim);
        let matched: Vec<AnyElement> = claim
            .matched
            .iter()
            .filter_map(|path| changed.iter().find(|f| &f.path == path))
            .map(|file| Self::file_row(file, theme, false))
            .collect();
        // Everything the claim carries, in one column beside the glyph: the
        // sentence, then what stands behind it. A card rather than a row
        // (review.md E3) — the sentence and its evidence are one unit, and a
        // hairline around them is what says so on a screen of five sections.
        let body = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(7.0))
            .child(
                div()
                    .min_w_0()
                    .text_size(px(Theme::TEXT_BODY))
                    .line_height(px(19.0))
                    .text_color(if unsupported {
                        theme.text_muted
                    } else {
                        theme.text
                    })
                    .child(SharedString::from(claim.text.clone())),
            )
            // The evidence for this one claim, and the anchors it named, on one
            // wrapping row: what checks it and what it is about are the same
            // annotation, and two rows of chips under one sentence read as two
            // claims.
            .when(
                !chips.is_empty() || !claim.files.is_empty() || !claim.symbols.is_empty(),
                |el| {
                    el.child(
                        div()
                            .flex()
                            .flex_row()
                            .flex_wrap()
                            .gap_y(px(5.0))
                            .gap_x(px(7.0))
                            .children(chips.iter().map(|chip| Self::claim_chip(chip, theme)))
                            .children(
                                claim
                                    .files
                                    .iter()
                                    .chain(&claim.symbols)
                                    .map(|anchor| Self::anchor_chip(anchor, theme)),
                            ),
                    )
                },
            )
            .when(!matched.is_empty(), |el| {
                el.child(div().flex().flex_col().gap(px(2.0)).children(matched))
            })
            // An anchor the diff refuses, labelled by what kind it was
            // (§gh#235): "unchanged" is what a reviewer goes and checks about a
            // path, and it is the wrong instruction entirely for a symbol no
            // changed line names.
            .children(claim.unmatched.iter().map(|anchor| {
                let label = match anchor_kind(anchor) {
                    AnchorKind::Path => format!("unchanged  {anchor}"),
                    AnchorKind::Symbol => format!("not in the diff  {anchor}"),
                };
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(FILE_GUTTER))
                    .font_family(theme.font_mono.clone())
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.danger)
                    .child(SharedString::from(label))
            }));
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(BAND_GAP))
            .px(px(Theme::SPACE_MD))
            .py(px(BAND_GAP))
            .rounded(px(Theme::RADIUS_ROW))
            .border_1()
            .border_color(theme.border)
            .child(Self::claim_glyph(mark, theme))
            .child(body)
            .into_any_element()
    }

    /// The verdict strip: one line under the header, in the tone the reading
    /// gave it. Never absent — a review with nothing to report still says what
    /// it checked, or a quiet screen reads as a screen that failed to load.
    ///
    /// A band like the three under it (review.md C/D) rather than a tinted
    /// card of its own, and quiet even when the reading is loud: it says the
    /// same thing the unclaimed block's header says, and two blocks shouting
    /// the same number leaves the screen with nothing to shout with. The hue
    /// is carried by the glyph and the copy, and the bed stays the card's.
    fn render_verdict(verdict: &Verdict, theme: &Theme) -> AnyElement {
        let color = Self::tone_color(verdict.tone, theme);
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(BAND_GAP))
            .px(px(Theme::SPACE_LG))
            .py(px(BAND_PY))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_none()
                    .w(px(GLYPH_W))
                    .flex()
                    .justify_center()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(color)
                    .child(SharedString::from(if verdict.tone.loud() {
                        "!"
                    } else {
                        "·"
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(Theme::TEXT_BODY))
                    .font_weight(if verdict.tone.loud() {
                        gpui::FontWeight::MEDIUM
                    } else {
                        gpui::FontWeight::NORMAL
                    })
                    .text_color(if verdict.tone.loud() {
                        color
                    } else {
                        theme.text_muted
                    })
                    .child(SharedString::from(verdict.text.clone())),
            )
            .into_any_element()
    }

    /// The brief: what was asked, rendered with the transcript's own markdown
    /// pipeline so an issue reads here the way an agent's reply reads there.
    fn render_brief(
        &mut self,
        review: &AttemptReview,
        theme: &Theme,
        window: &mut Window,
    ) -> AnyElement {
        let body: AnyElement = match review.brief.body.as_deref() {
            Some(text) => {
                let tree = crate::markdown::parser::parse_full(text);
                crate::markdown::render::render_tree(
                    &tree,
                    &crate::markdown::render::RenderOptions::settled(SharedString::from(format!(
                        "review-brief-{}",
                        review.task_id
                    ))),
                    theme,
                    window,
                    &|_| None,
                )
            }
            None => div()
                .text_size(px(Theme::TEXT_CAPTION))
                .text_color(theme.text_faint)
                .child(SharedString::from(board::NO_BODY))
                .into_any_element(),
        };
        // The band's label says what this is; the issue's URL is not repeated
        // beside it, because the header's own control opens the issue and a
        // link nobody can click is decoration (review.md C4, B2).
        Self::band(
            theme,
            "Asked for",
            GLYPH_NUDGE,
            true,
            div()
                .id("review-brief")
                .max_h(px(BRIEF_MAX_H))
                .overflow_y_scroll()
                .track_scroll(&self.brief_scroll)
                .flex()
                .flex_col()
                .child(body)
                .into_any_element(),
        )
    }

    /// The claims section's own heading (review.md E1): what it is, how many
    /// of them, and — on the right — where the evidence under them came from.
    ///
    /// The aside is the section's whole argument in six words, and it is worth
    /// the line: everything else on this screen is the agent's account of
    /// itself, and this is the part that is not.
    fn claims_heading(theme: &Theme, count: Option<String>, aside: &'static str) -> AnyElement {
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(BAND_GAP))
            .px(px(Theme::SPACE_LG))
            .pt(px(BAND_PY))
            .pb(px(Theme::SPACE_XS))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_DENSE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from("What it says it did")),
            )
            .children(count.map(|count| {
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(count))
            }))
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_faint)
                    .child(SharedString::from(aside)),
            )
            .into_any_element()
    }

    /// One line where a claim card would be: the section's three empty states,
    /// inset to the cards' own column so the section keeps its shape.
    fn claims_line(color: gpui::Hsla, text: &'static str) -> AnyElement {
        div()
            .flex_none()
            .px(px(Theme::SPACE_LG))
            .text_size(px(Theme::TEXT_BODY))
            .text_color(color)
            .child(SharedString::from(text))
            .into_any_element()
    }

    /// The claims, each carrying the diff facts for what it anchored.
    fn render_claims(review: &AttemptReview, theme: &Theme) -> AnyElement {
        // A block that was written and could not be read (§gh#235). Ahead of
        // the never-answered copy below and in the danger colour, because this
        // attempt did describe its work and the description is the thing that
        // went missing — the refusal is printed whole, since it names the line.
        if let Some(err) = &review.claims_error {
            return div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(Theme::SPACE_XS))
                .child(Self::claims_heading(theme, None, "unreadable"))
                .child(Self::claims_line(
                    theme.text,
                    "This attempt wrote a claims block the board could not parse, \
                     so nothing was recorded from it.",
                ))
                .child(
                    div()
                        .px(px(Theme::SPACE_LG))
                        .font_family(theme.font_mono.clone())
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.danger)
                        .child(SharedString::from(err.clone())),
                )
                .into_any_element();
        }
        // Never asked and claimed nothing are different facts, and this is the
        // last place they could be flattened into one. So is never *having been
        // asked*: an agent that was told the contract and said nothing and a
        // pull request no agent was ever given are not the same silence
        // (§gh#344).
        if !review.claimed() {
            return div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(Theme::SPACE_XS))
                .child(Self::claims_heading(
                    theme,
                    None,
                    "evidence gathered by the board, not the agent",
                ))
                .child(Self::claims_line(
                    theme.text_subtle,
                    if review.undispatched() {
                        "Nothing dispatched this pull request, so nobody was ever told the \
                         claim contract. Nothing here is asserted, and nothing is checked."
                    } else {
                        "This attempt never answered the claim contract. \
                         Nothing here is asserted, and nothing is checked."
                    },
                ))
                .into_any_element();
        }
        let claims: Vec<AnyElement> = review
            .remainder
            .claims
            .iter()
            .map(|claim| Self::claim_row(review, claim, &review.changed, theme))
            .collect();
        let count = match claims.len() {
            1 => "1 claim".to_string(),
            n => format!("{n} claims"),
        };
        div()
            .flex_none()
            .flex()
            .flex_col()
            .child(Self::claims_heading(
                theme,
                Some(count),
                "evidence gathered by the board, not the agent",
            ))
            .when(claims.is_empty(), |el| {
                el.child(Self::claims_line(
                    theme.text_subtle,
                    "Asked, and claimed nothing at all.",
                ))
            })
            .child(
                div()
                    .px(px(Theme::SPACE_LG))
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_SM))
                    .children(claims),
            )
            .into_any_element()
    }

    /// The effects row (§gh#236): what the board read off the branch itself.
    ///
    /// Above the claims, deliberately. A reader who has already been told a
    /// fluent story about the work reads the numbers underneath it as
    /// confirmation of the story; the same numbers read first are what the
    /// story then has to agree with. That ordering is the whole of why this row
    /// exists, and it is the design's own.
    fn render_effects(review: &AttemptReview, theme: &Theme) -> AnyElement {
        let chips = review.effect_chips();
        Self::band(
            theme,
            "Effects",
            GLYPH_NUDGE + 1.0,
            false,
            Self::chip_row(&chips, theme),
        )
    }

    /// What the board saw for itself: the commands the run executed, and how
    /// they exited. Never the agent's account of them — this comes off the run
    /// journal, which every harness writes without being asked.
    fn render_evidence(review: &AttemptReview, theme: &Theme) -> AnyElement {
        let evidence = &review.evidence;
        if evidence.commands == 0 {
            return Self::band(
                theme,
                "Evidence",
                GLYPH_NUDGE,
                false,
                div()
                    .text_size(px(Theme::TEXT_BODY))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(
                        "The board recorded no commands for this run.",
                    ))
                    .into_any_element(),
            );
        }
        let aside = format!(
            "{} commands · {} exited non-zero",
            evidence.commands, evidence.failed
        );
        let rows: Vec<AnyElement> = evidence
            .checks
            .iter()
            .map(|check| {
                let loud = !check.ever_passed();
                let tail = match (check.runs, check.failed) {
                    (1, 0) => String::new(),
                    (runs, 0) => format!("×{runs}"),
                    (runs, failed) => format!("×{runs}, {failed} failed"),
                };
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(Self::mark(loud, theme))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(if loud { theme.text } else { theme.text_muted })
                            .child(SharedString::from(check.command.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(if loud { theme.danger } else { theme.text_faint })
                            .child(SharedString::from(tail)),
                    )
                    .into_any_element()
            })
            .collect();
        Self::band(
            theme,
            "Evidence",
            GLYPH_NUDGE,
            false,
            div()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .when(rows.is_empty(), |el| {
                    el.child(
                        div()
                            .text_size(px(Theme::TEXT_BODY))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from("Nothing that verifies anything ran.")),
                    )
                })
                .children(rows)
                .when(evidence.truncated, |el| {
                    el.child(
                        div()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_faint)
                            .child(SharedString::from("…and more; the list is capped.")),
                    )
                })
                // The denominator, under the list rather than beside the label:
                // "0 checks among 214 commands" is the finding the list itself
                // cannot make, and it is the last thing to read, not the first.
                .child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(aside)),
                )
                .into_any_element(),
        )
    }

    /// The remainder — the only block on this screen that is allowed to shout.
    ///
    /// Drawn as a bordered, tinted card in the blocked hue when it is non-empty
    /// and as a plain quiet line when it is not, because "four files nobody
    /// mentioned" and "everything is accounted for" should not be two readings
    /// of the same shape.
    fn render_remainder(review: &AttemptReview, theme: &Theme) -> AnyElement {
        if let DiffSource::Unavailable { reason } = &review.diff {
            return Self::quiet_block(
                theme.text_subtle,
                format!("Unclaimed unknown — there is no diff to check against: {reason}"),
            );
        }
        let loud = !review.remainder.complete();
        let notes: Vec<AnyElement> = review
            .findings()
            .into_iter()
            .filter(|f| matches!(f.kind, FindingKind::Uncommitted | FindingKind::NeverClaimed))
            .map(|finding| {
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(6.0))
                    .child(Self::mark(finding.kind.tone().loud(), theme))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(Self::tone_color(finding.kind.tone(), theme))
                            .child(SharedString::from(finding.text)),
                    )
                    .into_any_element()
            })
            .collect();

        // Everything that is not the block itself: the notes, and where the
        // diff came from. Quiet lines under it, inset to its column.
        let footnotes = div()
            .flex_none()
            .px(px(Theme::SPACE_LG))
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_XS))
            .children(notes)
            .when(matches!(review.diff, DiffSource::Recorded), |el| {
                el.child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(
                            "From the diff the board recorded while the run was live; \
                                 the checkout is gone.",
                        )),
                )
            })
            // Where the numbers came from when nothing local ever held this
            // branch (§gh#344). Said for the same reason the line above is: a
            // count is only as good as its provenance.
            .when(matches!(review.diff, DiffSource::PullRequest), |el| {
                el.child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(
                            "From GitHub's file list for the pull request; \
                                 nothing ran here to read a checkout of.",
                        )),
                )
            });

        // Nothing unaccounted for: one quiet line, never the block. "Four files
        // nobody mentioned" and "everything is accounted for" must not be two
        // readings of the same shape.
        if !loud {
            return div()
                .flex_none()
                .flex()
                .flex_col()
                .gap(px(Theme::SPACE_XS))
                .when(review.claimed(), |el| {
                    el.child(Self::quiet_block(
                        theme.settled,
                        "Nothing unclaimed — every changed file is accounted for.".to_string(),
                    ))
                })
                .child(footnotes)
                .into_any_element();
        }

        // The one thing on this screen that shouts (review.md F): its own
        // hue at three strengths, and the count said once, in the header.
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(STRIP_PX))
            .py(px(STRIP_PY))
            .bg(theme.danger.opacity(ALARM_BED))
            .border_b_1()
            .border_color(theme.danger.opacity(ALARM_RULE))
            .child(
                icon(icons::DANGER_TRIANGLE)
                    .size(px(ICON_GLYPH))
                    .text_color(theme.danger),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_DENSE))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.danger)
                    .child(SharedString::from(match review.remainder.unclaimed.len() {
                        1 => "1 change no claim accounts for".to_string(),
                        n => format!("{n} changes no claim accounts for"),
                    })),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_CAPTION))
                    // Not one of the four text tones on purpose: a grey aside
                    // inside the alarm block reads as unrelated to it, and the
                    // whole block is the hue.
                    // theme-opacity-ok: the block's own hue, one step quieter (review.md F3)
                    .text_color(theme.danger.opacity(ALARM_ASIDE))
                    .child(SharedString::from("this is where drift hides")),
            );
        let rows = review
            .remainder
            .unclaimed
            .iter()
            .enumerate()
            .map(|(ix, file)| Self::unclaimed_row(file, ix > 0, theme));
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_XS))
            .child(
                div()
                    .flex_none()
                    .mx(px(Theme::SPACE_LG))
                    .mt(px(GLYPH_NUDGE + Theme::SPACE_MD))
                    .rounded(px(Theme::RADIUS_ROW))
                    .overflow_hidden()
                    .border_1()
                    .border_color(theme.danger.opacity(ALARM_BORDER))
                    .child(header)
                    .children(rows),
            )
            .child(footnotes)
            .into_any_element()
    }

    /// One unclaimed change (review.md F4): the path, what happened to it, and
    /// nothing else.
    ///
    /// The counts are spelled as a sentence rather than as the file table's
    /// `+6 −4`, because inside this block they are the *finding* — "modified,
    /// and no claim mentions it" is the row's whole content, and a bare pair of
    /// numbers reads as bookkeeping.
    fn unclaimed_row(file: &ChangedFile, ruled: bool, theme: &Theme) -> AnyElement {
        let counts = file.counts();
        let what = match file.status.as_str() {
            "A" => format!("added, {counts}"),
            "D" => "deleted, and no claim mentions it".to_string(),
            _ => format!("{counts}, unmentioned"),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(BAND_GAP))
            .px(px(STRIP_PX))
            .py(px(STRIP_PY))
            .when(ruled, |el| el.border_t_1().border_color(theme.border))
            .text_size(px(Theme::TEXT_DENSE))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(theme.font_mono.clone())
                    .text_color(theme.text_muted)
                    .child(SharedString::from(file.path.clone())),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(what)),
            )
            .into_any_element()
    }

    /// The terms this attempt ran on, when they are worth saying (§gh#349) —
    /// a band under the verdict, in the verdict's own shape and none of its
    /// volume.
    ///
    /// Under the verdict rather than in it, and never loud: `danger-full-access`
    /// is a standing condition of most runs on this board (two of the three
    /// runtimes apply no sandbox at all), so a band that shouted it would
    /// shout on nearly every review — see [`AttemptReview::sandbox_note`]. It
    /// is here at all because it qualifies everything below it: a reviewer
    /// weighing the effects and the evidence is entitled to know whether the
    /// agent that produced them was confined to its own checkout.
    fn render_sandbox(review: &AttemptReview, theme: &Theme) -> Option<AnyElement> {
        let note = review.sandbox_note()?;
        Some(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(BAND_GAP))
                .px(px(Theme::SPACE_LG))
                .py(px(BAND_PY))
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex_none()
                        .w(px(GLYPH_W))
                        .flex()
                        .justify_center()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("?")),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(note)),
                )
                .into_any_element(),
        )
    }

    /// A section that is one sentence: inset to the blocks' column, with the
    /// air above them.
    fn quiet_block(color: gpui::Hsla, text: String) -> AnyElement {
        div()
            .flex_none()
            .mx(px(Theme::SPACE_LG))
            .mt(px(GLYPH_NUDGE + Theme::SPACE_MD))
            .text_size(px(Theme::TEXT_BODY))
            .text_color(color)
            .child(SharedString::from(text))
            .into_any_element()
    }

    /// Whose turn it is, as the header pill says it (§gh#238).
    ///
    /// Two readings and an absence. A task the board has parked in `review` is
    /// waiting for a human, and one `blocked` on a question is waiting for the
    /// same human about something else — both are your turn, so both wear the
    /// review hue and the label says which. An attempt still running, or one
    /// long since done, gets no pill: the facts line beside it already says
    /// which, and a badge on every review is not a signal.
    ///
    /// `answered` is this session's receipt rather than anything durable, and
    /// it wins over the board's state because it is newer — the state moves
    /// when the sync loop next reads the pull request, which is some seconds
    /// after the button was pressed and not before.
    ///
    /// Never [`Theme::danger`], including for `blocked`: that hue is the
    /// unclaimed set's, and a pill in the header wearing it would be the
    /// loudest thing on the screen saying something the remainder block is
    /// there to say.
    fn turn_pill(state: &str, answered: bool, theme: &Theme) -> Option<(&'static str, gpui::Hsla)> {
        let review_hue = crate::board::state_color(board::BoardState::Review, theme);
        if answered {
            return Some(("Answered", theme.settled));
        }
        match board::BoardState::parse(state) {
            Some(board::BoardState::Review) => Some(("Waiting on you", review_hue)),
            Some(board::BoardState::Blocked) => Some(("Blocked on you", review_hue)),
            _ => None,
        }
    }

    /// The pill itself: the design's `1px 8px` on a 14% tint of its own hue.
    fn render_pill(label: &'static str, color: gpui::Hsla) -> AnyElement {
        div()
            .flex_none()
            .px(px(PILL_PX))
            .py(px(PILL_PY))
            .rounded(px(Theme::RADIUS_CHIP))
            .bg(color.opacity(PILL_TINT))
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(color)
            .child(SharedString::from(label))
            .into_any_element()
    }

    /// One fact on the header's second row (review.md B5): a 12px glyph, and
    /// what it is about.
    fn fact(
        theme: &Theme,
        glyph: Option<&'static str>,
        text: String,
        tint: Option<gpui::Hsla>,
    ) -> AnyElement {
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .children(glyph.map(|path| {
                icon(path)
                    .size(px(FACT_GLYPH))
                    // Explicitly, always: gpui tints an svg from its OWN text
                    // colour, which the row's does not reach.
                    .text_color(tint.unwrap_or(theme.text_subtle))
            }))
            .child(SharedString::from(text))
            .into_any_element()
    }

    /// The `·` between two facts, in the quietest tone on the row.
    fn fact_sep(theme: &Theme) -> AnyElement {
        div()
            .flex_none()
            .text_color(theme.text_faint)
            .child(SharedString::from("·"))
            .into_any_element()
    }

    /// The pull request's number, for the header's first fact. Off the URL,
    /// which is the only place the review carries it.
    fn pr_number(url: &str) -> String {
        match url.rsplit('/').next().filter(|n| !n.is_empty()) {
            Some(number) => format!("PR #{number}"),
            None => "Pull request".to_string(),
        }
    }

    /// Where one layer of the stack lives on GitHub, derived off this review's
    /// own pull request URL (gh#389).
    ///
    /// A stack is scoped to one repository — that is the key the board groups
    /// on — so every layer is this URL with a different number on the end. The
    /// map is otherwise a row of numbers a reader has to go and find by hand,
    /// which is half of what "nothing links to them" meant.
    ///
    /// `None` unless the URL ends in a number, so nothing is derived off a
    /// shape this did not recognise.
    fn layer_url(pr_url: &str, number: i64) -> Option<String> {
        let (base, tail) = pr_url.rsplit_once('/')?;
        tail.parse::<i64>().ok()?;
        Some(format!("{base}/{number}"))
    }

    /// The stack map, bottom layer first, this layer marked (gh#389).
    ///
    /// The same chips the board's peek panel draws — one hue per layer, saying
    /// what the review already knows about it: the layer you are on is
    /// accented, one GitHub objects to wears the failed colour, one that has
    /// already landed is muted, because it is history in the chain rather than
    /// something still in the way. The arrow points the way the chain merges,
    /// so the row cannot be read as a set.
    ///
    /// Each chip is a door to that layer's pull request. The board's peek opens
    /// the *sibling row*; there is no board here to open one on, and the
    /// pull request is the thing a reviewer wants next anyway.
    fn stack_ladder(review: &AttemptReview, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let layers = board::stack_map(review.stacked());
        let here = review.task_id.clone();
        div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(4.0))
            .children(layers.iter().enumerate().map(|(ix, layer)| {
                let current = layer.id == here;
                let stuck = layer
                    .mergeable
                    .as_deref()
                    .is_some_and(|state| state != "clean");
                let color = if current {
                    theme.accent
                } else if !layer.open {
                    theme.text_faint
                } else if stuck || layer.changes_requested {
                    crate::board::state_color(board::BoardState::Failed, theme)
                } else {
                    theme.text_muted
                };
                let url = review
                    .pr_url
                    .as_deref()
                    .zip(layer.pr_number)
                    .and_then(|(pr, n)| Self::layer_url(pr, n));
                div()
                    .id(SharedString::from(format!(
                        "review-stack-layer-{}",
                        layer.id
                    )))
                    .flex_none()
                    .px(px(6.0))
                    .h(px(18.0))
                    .flex()
                    .items_center()
                    .rounded(px(Theme::RADIUS_CHIP))
                    .bg(theme.wash(if current { 0.16 } else { 0.08 }))
                    .font_family(theme.font_mono.clone())
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(color)
                    .when(url.is_some(), |el| {
                        el.cursor_pointer().hover(|s| s.bg(theme.wash(0.18)))
                    })
                    .child(SharedString::from(match ix {
                        0 => board::layer_label(layer),
                        _ => format!("↑ {}", board::layer_label(layer)),
                    }))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        // A layer with no pull request number — the board holds
                        // the row, GitHub has not been given the branch — is
                        // not a door. Nothing happens rather than a tab opening
                        // on a guess.
                        if let Some(url) = url.as_deref() {
                            cx.open_url(url);
                        }
                    }))
            }))
            .into_any_element()
    }

    /// The stack band (gh#389): that this pull request is a layer at all, which
    /// layers are under it, and which way the chain merges.
    ///
    /// Absent on every pull request that is not one, which is nearly all of
    /// them — and the reason it is pinned above the scroll with the verdict
    /// rather than filed in the body when it is present. A reviewer who has not
    /// been told this is layer 2 of 3 is reading an ordinary pull request, and
    /// the mistake that follows is not a misreading of the diff: it is
    /// approving one that is about to be replayed onto a branch that moved, or
    /// merging out of an order GitHub will not let them take back.
    ///
    /// Three lines, and each is a different question. Where this sits and where
    /// the chain lands ([`board::stack_line`]); the map, with a door on every
    /// layer ([`Self::stack_ladder`]); and the order
    /// ([`board::merge_order`]) — the one fact no per-pull-request field
    /// carries, because it is not about this pull request at all.
    fn render_stack(
        review: &AttemptReview,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let line = board::stack_line(review.stacked())?;
        let body = div()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_SM))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(line)),
            )
            .child(Self::stack_ladder(review, theme, cx))
            .children(board::merge_order(review.stacked()).map(|order| {
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(order))
            }))
            .into_any_element();
        Some(Self::band(theme, "Stack", GLYPH_NUDGE, false, body))
    }

    /// The card's own header (review.md B): what this is, whose turn it is,
    /// and the ways out — over a line of facts about the run.
    fn render_header(
        &self,
        review: &AttemptReview,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The way out to the upstream. Two of them where there are two: a
        // review is read on a device that may not hold the checkout, so
        // dropping one would lose a route.
        let links: Vec<(&'static str, &'static str, String)> = [
            review
                .pr_url
                .clone()
                .map(|url| ("review-open-pr", icons::GITHUB_MARK, url)),
            (!review.brief.url.is_empty()).then(|| {
                (
                    "review-open-issue",
                    icons::DOCUMENT,
                    review.brief.url.clone(),
                )
            }),
        ]
        .into_iter()
        .flatten()
        .collect();
        // Beside the title rather than out by the links: whose turn it is is a
        // fact about this review, not an action you can take on it.
        let pill = Self::turn_pill(&review.state, self.receipt.is_some(), theme)
            .map(|(label, color)| Self::render_pill(label, color));

        let mut facts: Vec<AnyElement> = Vec::new();
        if let Some(url) = review.pr_url.as_deref() {
            facts.push(Self::fact(
                theme,
                Some(icons::GITHUB_MARK),
                Self::pr_number(url),
                None,
            ));
        }
        if let Some(branch) = &review.branch {
            facts.push(Self::fact(theme, Some(icons::GIT_BRANCH), branch.clone(), None));
        }
        // Whether it can land, with the layers underneath taken into account
        // (gh#283, gh#389) — never the flat `mergeable_state`, which mid-stack
        // says `clean` about a pull request that cannot go anywhere. Silent
        // until GitHub has been asked, which is the ordinary state of a fresh
        // row: a blank is honest and an invented verdict is not.
        if let Some(note) = board::landing_note(review.stacked()) {
            facts.push(Self::fact(theme, None, note, None));
        }
        // What the board knows about the run itself. The canvas names the model
        // and how long it took; neither is on the wire (see review.md), and the
        // attempt and its outcome are what the board does know. When there was
        // no run at all (§gh#344) this is the fact that says so — "attempt 1 ·
        // still running" about a pull request nobody dispatched would be two
        // inventions in four words.
        facts.push(Self::fact(
            theme,
            None,
            if review.undispatched() {
                "no attempt · opened outside the board".to_string()
            } else {
                format!(
                    "attempt {} · {}",
                    review.attempt_number.max(1),
                    match &review.outcome {
                        Some(outcome) => outcome.as_str(),
                        None => "still running",
                    }
                )
            },
            None,
        ));
        let mut row: Vec<AnyElement> = Vec::new();
        for (ix, fact) in facts.into_iter().enumerate() {
            if ix > 0 {
                row.push(Self::fact_sep(theme));
            }
            row.push(fact);
        }

        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .px(px(Theme::SPACE_LG))
            .pt(px(Theme::SPACE_MD))
            .pb(px(BAND_GAP))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(9.0))
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(review.brief.identifier.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(Theme::TEXT_PROSE))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(SharedString::from(review.brief.title.clone())),
                    )
                    .children(pill)
                    .children(links.into_iter().map(|(id, glyph, url)| {
                        div()
                            .id(id)
                            .flex_none()
                            .size(px(ICON_BTN))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Theme::RADIUS_CHIP))
                            .text_color(theme.text_subtle)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.wash(0.08)))
                            .child(
                                icon(glyph).size(px(ICON_GLYPH)).text_color(theme.text_subtle),
                            )
                            .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
                    })),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .flex_wrap()
                    .gap(px(BAND_GAP))
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_subtle)
                    .children(row),
            )
            .into_any_element()
    }

    /// What the diff strip says on its left: how many files moved, and by how
    /// much (§gh#238).
    ///
    /// Summed off `changed` — the same list the remainder is computed against
    /// — so the strip is a restatement of the denominator and not a second
    /// reading of the branch. A binary file counts as a file and contributes no
    /// lines, which is what git says about it and what
    /// [`ChangedFile::counts`] already spells one row at a time.
    fn diff_totals(changed: &[ChangedFile]) -> (String, u32, u32) {
        let added: u32 = changed.iter().map(|file| file.added).sum();
        let removed: u32 = changed.iter().map(|file| file.removed).sum();
        let label = match changed.len() {
            1 => "1 file changed".to_string(),
            n => format!("{n} files changed"),
        };
        (label, added, removed)
    }

    /// Whether the `Read the diff` chip has anywhere to hand the reader.
    ///
    /// It needs both halves, because [`crate::changes`] reads the **live**
    /// checkout of the selected chat: a session to select, and a checkout still
    /// on disk. A [`DiffSource::Recorded`] review is a snapshot of a worktree
    /// `gc` has since reclaimed (gh#72) — its counts are true and there is
    /// nothing left to open — and a review whose chat is gone has nothing to
    /// select. Either way the counts are still drawn: the numbers are the fact,
    /// and the chip is only the route to the rest of it.
    fn diff_openable(review: &AttemptReview, chat_id: Option<&str>) -> bool {
        chat_id.is_some() && matches!(review.diff, DiffSource::Checkout)
    }

    /// The strip that closes the body (§gh#238): what moved, and the way out.
    ///
    /// This is the whole of the screen's answer to "but where is the diff". It
    /// draws none: the chip emits [`ReviewEvent::ReadTheDiff`] and the shell
    /// takes the reader to [`crate::changes`], which is the module whose job
    /// that is. Absent entirely when nothing changed — there is no diff to
    /// point at, and `0 files changed` beside a dead chip would be a signpost
    /// to an empty room.
    fn render_diff_strip(
        &self,
        review: &AttemptReview,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if review.changed.is_empty() {
            return None;
        }
        let (label, added, removed) = Self::diff_totals(&review.changed);
        let chat = Self::diff_openable(review, self.chat_id.as_deref())
            .then(|| self.chat_id.clone())
            .flatten();
        let counts = |text: String, color: gpui::Hsla| {
            div()
                .flex_none()
                .font_family(theme.font_mono.clone())
                .text_size(px(Theme::TEXT_DENSE))
                .text_color(color)
                .child(SharedString::from(text))
        };
        Some(
            div()
                .flex_none()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(BAND_GAP))
                .mx(px(Theme::SPACE_LG))
                .mt(px(Theme::SPACE_MD))
                .px(px(STRIP_PX))
                .py(px(STRIP_PY))
                .rounded(px(Theme::RADIUS_ROW))
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .flex_none()
                        .text_size(px(Theme::TEXT_DENSE))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(label)),
                )
                .child(counts(format!("+{added}"), theme.settled))
                // The one place on this screen the blocked hue is not an alarm:
                // it belongs to the minus sign, as it does in every diff ever
                // printed, and a grey `−33` would be the only one that is not.
                .child(counts(format!("−{removed}"), theme.danger))
                .child(div().flex_1())
                .children(chat.map(|chat_id| {
                    div()
                        .id("review-read-the-diff")
                        .flex_none()
                        .h(px(CHIP_H))
                        .px(px(CHIP_PAD))
                        .flex()
                        .items_center()
                        .rounded(px(Theme::RADIUS_CHIP))
                        .bg(theme.wash(0.08))
                        .text_size(px(Theme::TEXT_DENSE))
                        .text_color(theme.text)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.wash(0.14)))
                        .child(SharedString::from("Read the diff"))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(ReviewEvent::ReadTheDiff {
                                chat_id: chat_id.clone(),
                            })
                        }))
                }))
                .into_any_element(),
        )
    }

    /// What the submission did, in one line. Not an error even when nothing
    /// was delivered or nothing was posted: a verdict that reached the agent
    /// and not the pull request is a real outcome, and the reviewer is the
    /// person who needs to know which one happened.
    ///
    /// The sentence itself is [`verdict::receipt_line`] — the board's, so the
    /// phone and the CLI say the same thing about the same receipt.
    fn receipt_line(receipt: &VerdictReceipt) -> String {
        verdict::receipt_line(receipt)
    }

    /// The payload, before it is sent.
    ///
    /// Rendered from [`comet_board::verdict::compose`] — the same function the
    /// board sends with, called here rather than reimplemented, because the
    /// one promise this card makes is that it is showing what will be sent.
    /// Mono 11/17 in a dashed card, faded out at the bottom: it is the shape of
    /// the message, not a document to read.
    ///
    /// It lives at the bottom of the authoring column (review.md I4), not in
    /// the review card — the payload is a message that will arrive *over
    /// there*, and the column it will arrive in is where the canvas shows it
    /// waiting. The shell asks for it through [`Self::delivery_preview`].
    fn render_preview(&self, review: &AttemptReview, comment: &str, theme: &Theme) -> AnyElement {
        let payload = verdict::compose(review, self.kind, comment);
        let lines: Vec<AnyElement> = payload
            .lines()
            .map(|line| {
                div()
                    .flex_none()
                    .h(px(PREVIEW_LINE_H))
                    .min_w_0()
                    .truncate()
                    .text_color(if line.starts_with("[unclaimed]") {
                        // The one thing in the payload the reviewer did not
                        // type, and the reason it is worth previewing.
                        theme.danger
                    } else if line.starts_with('[') || line.starts_with("comet-board:") {
                        theme.text_muted
                    } else {
                        theme.text_subtle
                    })
                    .child(SharedString::from(line.to_string()))
                    .into_any_element()
            })
            .collect();
        // Whether the payload runs past the card. Counted rather than measured
        // — a long comment wraps and this does not know it — so the fade is
        // sometimes absent from a payload that just overflows, and never
        // present on one that plainly does not.
        let overflows = lines.len() as f32 * PREVIEW_LINE_H > PREVIEW_MAX_H;
        let body = div()
            .flex()
            .flex_col()
            .font_family(theme.font_mono.clone())
            .text_size(px(Theme::TEXT_CAPTION))
            .line_height(px(PREVIEW_LINE_H))
            .children(lines);
        div()
            .flex_none()
            .mx(px(Theme::SPACE_MD))
            .mb(px(Theme::SPACE_MD))
            .rounded(px(Theme::RADIUS_ROW))
            .overflow_hidden()
            .border_1()
            .border_dashed()
            .border_color(theme.border_strong)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .px(px(Theme::SPACE_MD))
                    .py(px(Theme::SPACE_SM))
                    .bg(theme.surface_raised)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        icon(icons::INFO_CIRCLE)
                            .size(px(13.0))
                            .text_color(theme.text_subtle),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::TEXT_DENSE))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Will be delivered on submit")),
                    ),
            )
            .child(
                div()
                    .max_h(px(PREVIEW_MAX_H))
                    .overflow_hidden()
                    .px(px(Theme::SPACE_MD))
                    .py(px(BAND_GAP))
                    .child(crate::edge_fade::edge_faded(
                        PREVIEW_MAX_H * (1.0 - PREVIEW_FADE_AT),
                        false,
                        overflows,
                        body,
                    )),
            )
            .into_any_element()
    }

    /// The payload preview, for the column it will be delivered into.
    ///
    /// The shell draws it there (review.md I4); this is the review's own
    /// state, so the review composes it — the card and the column must not be
    /// able to disagree about what is about to be sent.
    pub fn delivery_preview(&self, theme: &Theme, cx: &gpui::App) -> Option<AnyElement> {
        let review = self.review.as_ref()?;
        // No pull request, no payload — the same guard the bar takes.
        review.pr_url.as_ref()?;
        let comment = self.comment.read(cx).text().trim().to_string();
        Some(self.render_preview(review, &comment, theme))
    }

    /// The verdict bar: the comment, the three verdicts, and the sentence that
    /// says where it goes.
    ///
    /// Absent when there is no pull request to review — the whole bar is about
    /// posting one, and a row with no PR has nowhere to post it.
    fn render_verdict_bar(
        &mut self,
        review: &AttemptReview,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        review.pr_url.as_ref()?;
        let comment = self.comment.read(cx).text().trim().to_string();
        let armed = self.kind;
        let submitting = self.submitting;
        let ready = !(armed.needs_comment() && comment.is_empty());
        // What the screen promises. Delivery into the chat is the default and
        // the point; the two ways it does not happen are a verdict not worth
        // interrupting anybody with (a bare approval), and an author who has
        // moved — which only the board can know, and which the receipt says
        // afterwards rather than the bar guessing at beforehand.
        let contract = verdict::contract_line(review, verdict::worth_delivering(armed, &comment));
        // The three verdicts, bare (review.md H4): each one its own meaning's
        // colour, and no bed under any of them — the armed one takes H5's
        // filled treatment instead, so the bar has exactly one solid control
        // and it is the one that is about to happen.
        let picker = div()
            .flex()
            .flex_row()
            .gap(px(Theme::SPACE_XS))
            .children(VERDICTS.map(|(kind, label)| {
                let on = kind == armed;
                let quiet = match kind {
                    VerdictKind::Approve => theme.settled,
                    _ => theme.text_muted,
                };
                div()
                    .id(SharedString::from(format!("verdict-{}", kind.as_str())))
                    .flex_none()
                    .h(px(VERDICT_H))
                    .px(px(if on { Theme::SPACE_MD } else { VERDICT_PX }))
                    .flex()
                    .items_center()
                    .rounded(px(Theme::RADIUS_CHIP))
                    .when(on, |el| {
                        el.bg(theme.text)
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.bg)
                    })
                    .when(!on, |el| {
                        el.text_color(quiet).hover(|s| s.bg(theme.wash(0.08)))
                    })
                    .text_size(px(Theme::TEXT_DENSE))
                    .cursor_pointer()
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |panel, _, _, cx| {
                        panel.kind = kind;
                        panel.submit_error = None;
                        cx.notify();
                    }))
            }));
        // The merge key (gh#408), leftmost and quiet: landing the row is the
        // bar's other act, and the one solid control stays the verdict's
        // Submit. The ellipsis is the promise that a confirmation follows —
        // this click arms the dialog, never the merge.
        let merge = merge_available(review).then(|| {
            let merging = self.merging;
            popover::btn_ghost(
                theme,
                if merging { "Merging…" } else { "Merge…" },
                "review-merge",
            )
            .id("review-merge")
            .h(px(VERDICT_H))
            .flex()
            .items_center()
            .when(merging, |el| el.opacity(0.5))
            .on_click(cx.listener(|panel, _, _, cx| {
                if panel.merging {
                    return;
                }
                panel.merge_confirming = true;
                cx.notify();
            }))
        });
        let bar = div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(BAND_GAP))
            .px(px(Theme::SPACE_LG))
            .py(px(Theme::SPACE_MD))
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.surface_raised)
            .child(
                // Darker than the bar it sits in, not lighter: an input is a
                // hole in a raised surface (review.md H2).
                div()
                    .min_h(px(42.0))
                    .px(px(Theme::SPACE_MD))
                    .py(px(9.0))
                    .rounded(px(Theme::RADIUS_ROW))
                    .border_1()
                    .border_color(theme.border_strong)
                    .bg(theme.bg)
                    .text_size(px(Theme::TEXT_BODY))
                    .child(self.comment.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(BAND_GAP))
                    .children(merge)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(Theme::TEXT_DENSE))
                            .line_height(px(17.0))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(contract)),
                    )
                    .child(picker)
                    .child(
                        popover::btn_primary(
                            theme,
                            if submitting {
                                "Submitting…"
                            } else {
                                "Submit"
                            },
                        )
                        .id("verdict-submit")
                        .h(px(VERDICT_H))
                        .flex()
                        .items_center()
                        .when(submitting || !ready, |el| el.opacity(0.5))
                        .on_click(cx.listener(|panel, _, _, cx| panel.submit(cx))),
                    ),
            )
            .when_some(self.submit_error.clone(), |el, error| {
                el.child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.warning)
                        .child(error),
                )
            })
            .when_some(self.receipt.as_ref(), |el, receipt| {
                let told = receipt.delivered;
                el.child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(if told {
                            theme.settled
                        } else {
                            theme.text_muted
                        })
                        .child(SharedString::from(Self::receipt_line(receipt))),
                )
            })
            .when_some(self.merge_error.clone(), |el, error| {
                el.child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.warning)
                        .child(error),
                )
            })
            .when_some(self.merge_line.clone(), |el, line| {
                // Settled green only for the merge that landed: `in the merge
                // queue` and `still merging` are answers, not arrivals — the
                // poll that sees them land is what moves the row (gh#290).
                let landed = line.ends_with("merged");
                el.child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(if landed {
                            theme.settled
                        } else {
                            theme.text_muted
                        })
                        .child(line),
                )
            });
        Some(bar.into_any_element())
    }

    /// The quiet line under everything: which device answered.
    fn render_host(&self, theme: &Theme, cx: &gpui::App) -> AnyElement {
        let label = match self.host.as_deref() {
            None => "this device".to_string(),
            Some(host) => self
                .state
                .read(cx)
                .devices
                .iter()
                .find(|d| d.id == host)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| host.to_string()),
        };
        div()
            .flex_none()
            .mx(px(Theme::SPACE_LG))
            .mt(px(Theme::SPACE_MD))
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(theme.text_faint)
            .child(SharedString::from(format!(
                "Read from the board on {label}."
            )))
            .into_any_element()
    }

    /// The card while the review is still being assembled (gh#341).
    ///
    /// A review is read off the attempt's checkout and three GitHub endpoints,
    /// so this is a real wait and not a frame's flicker — and a real wait in
    /// this app is said in this app's own vocabulary. That is
    /// [`loaders::comet_loader`], the wave the boot splash and the terminal
    /// viewport already pulse; the review route was the one surface that opened
    /// with a bare sentence instead.
    ///
    /// The sentence stays under it. A loader alone says something is happening
    /// but not what, and "reading" is the honest verb here — the board is doing
    /// the reading, and the reader is being told whose work they are waiting on.
    fn render_loading(theme: &Theme, view: gpui::EntityId, cx: &mut gpui::App) -> AnyElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(Theme::SPACE_MD))
            .child(loaders::comet_loader(theme, LOADER_CELL, view, cx))
            .child(
                div()
                    .text_size(px(Theme::TEXT_DENSE))
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(LOADING_LINE)),
            )
            .into_any_element()
    }
}

impl gpui::EventEmitter<ReviewEvent> for ReviewPanel {}

impl Render for ReviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // The card is the shell's; what this renders is its contents, and every
        // band inside it brings its own padding — the top half is full-bleed
        // bands and the bottom half is inset blocks (review.md A6).
        let card = div().size_full().flex().flex_col();
        // The two states with nothing to compose: one message, centred in the
        // card's own gutters rather than in a band that has no label.
        let message = |color: gpui::Hsla, text: SharedString| {
            div()
                .size_full()
                .flex()
                .flex_col()
                .px(px(Theme::SPACE_LG))
                .py(px(Theme::SPACE_MD))
                .child(div().text_size(px(Theme::TEXT_BODY)).text_color(color).child(text))
                .into_any_element()
        };

        if let Some(error) = self.error.clone() {
            return message(theme.warning, error);
        }
        let Some(review) = self.review.clone() else {
            // Two different nothings, and only one of them is a wait. An answer
            // that came back empty is a fact, and facts are sentences on this
            // screen; a review that has not arrived yet is a wait, and a wait
            // gets the app's own wave (gh#341).
            return if self.loaded {
                message(
                    theme.text_subtle,
                    SharedString::from("No review for this task."),
                )
            } else {
                Self::render_loading(&theme, cx.entity_id(), cx)
            };
        };

        let header = self.render_header(&review, &theme, cx);
        let verdict = Self::render_verdict(&review.verdict(), &theme);
        let sandbox = Self::render_sandbox(&review, &theme);
        let stack = Self::render_stack(&review, &theme, cx);
        let brief = self.render_brief(&review, &theme, window);
        let effects = Self::render_effects(&review, &theme);
        let claims = Self::render_claims(&review, &theme);
        let evidence = Self::render_evidence(&review, &theme);
        let remainder = Self::render_remainder(&review, &theme);
        let strip = self.render_diff_strip(&review, &theme, cx);
        let host = self.render_host(&theme, cx);
        let bar = self.render_verdict_bar(&review, &theme, cx);
        // The merge confirm (gh#408), over everything. The sentence is the
        // row's own ([`board::merge_confirmation`]): for a layer of a stack it
        // names every open layer that merges along, and the board's own
        // reservation rides last — the reader most in need of it is the one
        // confirming a merge the board does not think can land.
        let confirm = (self.merge_confirming && merge_available(&review)).then(|| {
            let sentence = board::merge_confirmation(review.merge_subject());
            let dialog = popover::dialog_card(&theme)
                .child(popover::dialog_title(&theme, "Merge?"))
                .child(
                    div()
                        .mt(px(6.0))
                        .child(popover::dialog_body(&theme, sentence)),
                )
                .child(
                    div()
                        .mt(px(16.0))
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            popover::btn_ghost(&theme, "Cancel", "merge-cancel")
                                .id("merge-cancel")
                                .on_click(cx.listener(|panel, _, _, cx| {
                                    panel.merge_confirming = false;
                                    cx.notify();
                                })),
                        )
                        .child(
                            popover::btn_primary(&theme, "Merge")
                                .id("merge-confirm")
                                .on_click(cx.listener(|panel, _, _, cx| panel.merge(cx))),
                        ),
                )
                .into_any_element();
            popover::modal("merge-confirm-dialog", window.viewport_size(), dialog)
        });

        // The question in order — what was asked, what the branch did, what the
        // board saw the run do, what the agent says it did, and what nobody
        // accounted for — with the verdict pinned above the scroll so the
        // loudest fact cannot be pushed under the fold by a long issue body.
        //
        // Evidence sits with Effects rather than after the claims (review.md
        // deviations): both are what the board read for itself, and the whole
        // argument for the effects row's position is that numbers read *before*
        // a fluent story are what the story then has to agree with.
        let body = div()
            .id("review-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.body_scroll)
            .flex()
            .flex_col()
            .pb(px(Theme::SPACE_LG))
            .child(brief)
            .child(effects)
            .child(evidence)
            .child(claims)
            .child(remainder)
            // Last, and after the remainder on purpose: "here is how much moved"
            // is the question a reader has once they have been told what nobody
            // accounted for, and never before.
            .children(strip)
            .child(host);

        // The verdict bar is pinned under the scroll for the same reason the
        // verdict strip is pinned above it: the thing you came to do must not
        // be reachable only by scrolling past a long issue body.
        motion::fade_quick(
            SharedString::from(format!("review-in-{}", review.task_id)),
            card.child(header)
                .child(verdict)
                .children(sandbox)
                // Above the scroll, with the verdict, and for the same reason:
                // what a reviewer is looking at is a layer of a stack, and that
                // must not be reachable only by scrolling back past a long
                // issue body (gh#389).
                .children(stack)
                .child(body)
                .children(bar)
                .children(confirm),
        )
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::state_color;
    use comet_proto::view::board::BoardState;

    /// The tab is on screen before the brief is, and a tab that renamed itself
    /// mid-read would be a tab you had to re-find. The loading name is the
    /// loaded one wherever the id spells the identifier — which is everywhere
    /// the board mints ids.
    #[test]
    fn the_review_tab_is_named_before_the_brief_lands() {
        // Loaded: the brief's identifier, verbatim — the card's own header
        // leads with this exact string.
        assert_eq!(
            review_tab_title(Some("gh#138"), "gh:o/r#138"),
            "Review · gh#138"
        );
        // Still loading: parsed off the id, and identical to the loaded name.
        assert_eq!(review_tab_title(None, "gh:o/r#138"), "Review · gh#138");
        // A pull-request id keeps its `!` — `gh!212` and `gh#212` are two rows.
        assert_eq!(review_tab_title(None, "gh:o/r!212"), "Review · gh!212");
        // Linear spells its own identifier after the colon.
        assert_eq!(review_tab_title(None, "linear:LIN-142"), "Review · LIN-142");
        // Unparseable ids are shown as they came rather than invented.
        assert_eq!(review_tab_title(None, "weird-id"), "Review · weird-id");
        assert_eq!(review_tab_title(Some(""), "gh:o/r#7"), "Review · gh#7");
    }

    /// One hue on this screen means "look at this", and it is the ramp's
    /// blocked hue — the same red the board paints a blocked or failed row in
    /// (gh#173). An unclaimed change is not a new kind of bad news needing a
    /// new colour; it is the board's existing "something is wrong here", said
    /// about a diff.
    #[test]
    fn only_an_alarming_review_wears_the_loud_hue() {
        for theme in [Theme::dark(), Theme::light()] {
            let alarm = ReviewPanel::tone_color(Tone::Alarm, &theme);
            assert_eq!(alarm, state_color(BoardState::Blocked, &theme));
            assert_eq!(alarm, state_color(BoardState::Failed, &theme));
            // The other two tones must be able to sit on the same screen
            // without competing with it — and must differ from each other, or
            // "nothing is wrong" and "nothing is known" would read alike.
            let settled = ReviewPanel::tone_color(Tone::Settled, &theme);
            let unknown = ReviewPanel::tone_color(Tone::Unknown, &theme);
            assert_ne!(alarm, settled);
            assert_ne!(alarm, unknown);
            assert_ne!(settled, unknown);
        }
    }

    /// A submission is not an error, and the ways it lands are not the same
    /// sentence. "Recorded and delivered" and "recorded, and the author is
    /// gone" are the difference between a review that arrived and one nobody
    /// has read — and the projection is a third fact on top of both (gh#365).
    #[test]
    fn the_receipt_says_what_reached_the_chat_and_what_did_not() {
        let base = VerdictReceipt {
            task_id: "gh:o/r#138".into(),
            attempt: 7,
            kind: VerdictKind::ChangesRequested,
            review_id: 900,
            recorded: true,
            chat_id: Some("chat-1".into()),
            delivered: true,
            not_delivered: None,
            projection: verdict::Projection::Posted,
            refused: None,
            posted_as: None,
            unclaimed: 2,
            payload: String::new(),
        };
        assert_eq!(
            ReviewPanel::receipt_line(&base),
            "Recorded, and delivered into the chat once. It is on the pull request."
        );
        // Cast under the reviewer's own credential (gh#369), the line says
        // whose name is on it — the difference between a bot having spoken and
        // a person having reviewed.
        let as_person = VerdictReceipt {
            posted_as: Some("bredebjorhovd".into()),
            ..base.clone()
        };
        assert_eq!(
            ReviewPanel::receipt_line(&as_person),
            "Recorded, and delivered into the chat once. It is on the pull request, \
             as @bredebjorhovd."
        );
        // The idempotent path says so rather than pretending it just happened.
        let retried = VerdictReceipt {
            recorded: false,
            ..base.clone()
        };
        assert!(
            ReviewPanel::receipt_line(&retried).starts_with("Already recorded"),
            "{}",
            ReviewPanel::receipt_line(&retried)
        );
        let undelivered = VerdictReceipt {
            delivered: false,
            not_delivered: Some("chat chat-1 no longer holds the agent".into()),
            ..base.clone()
        };
        let line = ReviewPanel::receipt_line(&undelivered);
        assert!(line.contains("Nothing was delivered"), "{line}");
        assert!(line.contains("no longer holds the agent"), "{line}");

        // A verdict GitHub would not take still stands, and the line says both
        // halves without the reader having to know GitHub's rules.
        let unposted = VerdictReceipt {
            projection: verdict::Projection::Unposted,
            refused: Some("github HTTP 422: Can not approve your own pull request".into()),
            ..base
        };
        let line = ReviewPanel::receipt_line(&unposted);
        assert!(line.starts_with("Recorded, and delivered"), "{line}");
        assert!(line.contains("not on the pull request"), "{line}");
        assert!(line.contains("Can not approve"), "{line}");
    }

    /// The preview is the payload, not a paraphrase of it: it is composed by
    /// the same function the board sends with, so the card cannot promise one
    /// message and the chat receive another — including the unclaimed lines,
    /// which are the part nobody typed.
    /// A finished attempt with one change nobody claimed — the shape the
    /// preview is about.
    fn reviewed() -> AttemptReview {
        let unclaimed = ChangedFile {
            path: "Cargo.toml".into(),
            status: "A".into(),
            added: 1,
            removed: 0,
            binary: false,
            symbols: vec![],
        };
        AttemptReview {
            // gh#236's field, defaulted: read = false, so this fixture claims
            // nothing about effects it does not exercise.
            effects: Default::default(),
            task_id: "gh:o/r#138".into(),
            attempt: 7,
            attempt_number: 1,
            state: "review".into(),
            outcome: Some("done".into()),
            branch: Some("board/gh-138".into()),
            worktree: Some("/wt/gh-138-1".into()),
            pr_url: Some("https://github.com/o/r/pull/212".into()),
            pr_base_ref: None,
            pr_mergeable: None,
            changes_below: None,
            stack: None,
            brief: comet_board::claims::Brief {
                identifier: "gh#138".into(),
                title: "Active owns a chat's row while its session is live".into(),
                url: "https://github.com/o/r/issues/138".into(),
                body: None,
            },
            claimed_at: Some("2026-08-09T10:00:00Z".into()),
            claims_error: None,
            sandbox: None,
            remainder: comet_board::claims::Remainder {
                unclaimed: vec![unclaimed.clone()],
                claimed: 0,
                ..Default::default()
            },
            changed: vec![unclaimed],
            diff: DiffSource::Checkout,
            uncommitted: Some(0),
            evidence: Default::default(),
        }
    }

    #[test]
    fn the_preview_is_the_payload_the_board_will_send() {
        let review = reviewed();
        let payload = verdict::compose(
            &review,
            VerdictKind::ChangesRequested,
            "Why does this need itertools?",
        );
        assert!(
            payload.contains("[review · changes requested]"),
            "{payload}"
        );
        assert!(payload.contains("[unclaimed] Cargo.toml"), "{payload}");
        // And the card fades where the design says it does, at 72% of its own
        // height — the band is the remainder of it.
        assert!((PREVIEW_MAX_H * (1.0 - PREVIEW_FADE_AT) - 196.0 * 0.28).abs() < 0.01);
    }

    /// The strip restates the denominator, and the denominator is the same
    /// `changed` list the remainder is subtracted from (§gh#238). A strip that
    /// counted the branch for itself could disagree with the block above it
    /// about how many files there are, on one screen.
    #[test]
    fn the_strip_counts_the_same_files_the_remainder_does() {
        let mut review = reviewed();
        let (label, added, removed) = ReviewPanel::diff_totals(&review.changed);
        assert_eq!((label.as_str(), added, removed), ("1 file changed", 1, 0));
        // A second file, a binary one, and the plural: binary contributes a
        // file and no lines, because that is all git says about it.
        review.changed.push(ChangedFile {
            path: "docs/a.png".into(),
            status: "A".into(),
            added: 0,
            removed: 0,
            binary: true,
            symbols: vec![],
        });
        review.changed.push(ChangedFile {
            path: "src/lib.rs".into(),
            status: "M".into(),
            added: 116,
            removed: 33,
            binary: false,
            symbols: vec![],
        });
        let (label, added, removed) = ReviewPanel::diff_totals(&review.changed);
        assert_eq!(
            (label.as_str(), added, removed),
            ("3 files changed", 117, 33)
        );
    }

    /// The chip is a route, so it needs somewhere to route to: a chat to
    /// select, and a checkout still on disk for [`crate::changes`] to read. A
    /// chip that opened an empty diff pane would be worse than no chip — the
    /// counts beside it are true either way, which is why only the chip goes.
    #[test]
    fn the_chip_is_only_drawn_when_there_is_a_diff_to_open() {
        let review = reviewed();
        assert!(ReviewPanel::diff_openable(&review, Some("chat-1")));
        // A review outlives its chat (gh#183), and the pane resolves its diff
        // from the selected one.
        assert!(!ReviewPanel::diff_openable(&review, None));
        // The snapshot the board kept after `gc` reclaimed the worktree
        // (gh#72): the numbers survive, the checkout does not.
        let recorded = AttemptReview {
            diff: DiffSource::Recorded,
            ..reviewed()
        };
        assert!(!ReviewPanel::diff_openable(&recorded, Some("chat-1")));
        let gone = AttemptReview {
            diff: DiffSource::Unavailable {
                reason: "no checkout".into(),
            },
            ..reviewed()
        };
        assert!(!ReviewPanel::diff_openable(&gone, Some("chat-1")));
        // A pull request nobody dispatched (§gh#344): the counts are GitHub's
        // and there is no checkout here to open. The numbers are still drawn;
        // the chip is only the route to the rest of it.
        let undispatched = AttemptReview {
            diff: DiffSource::PullRequest,
            ..reviewed()
        };
        assert!(!ReviewPanel::diff_openable(&undispatched, Some("chat-1")));
    }

    /// The review that was three unrelated pull requests (§gh#389).
    ///
    /// Every sentence the band draws is the board's own, so this asserts the
    /// screen calls them with the right facts rather than re-checking the
    /// words. The one thing that is this module's is the door on each chip.
    #[test]
    fn a_stacked_review_says_which_layer_it_is_and_links_to_the_rest() {
        let layer = |n: i64, position: i64| comet_proto::view::board::StackLayer {
            id: format!("gh:o/r!{n}"),
            identifier: format!("gh!{n}"),
            pr_number: Some(n),
            position: Some(position),
            open: true,
            mergeable: Some("clean".into()),
            changes_requested: false,
        };
        let review = AttemptReview {
            task_id: "gh:o/r!48".into(),
            pr_url: Some("https://github.com/o/r/pull/48".into()),
            pr_base_ref: Some("board/gh-44-packages".into()),
            pr_mergeable: Some("clean".into()),
            stack: Some(comet_proto::view::board::RowStack {
                number: 49,
                position: Some(2),
                size: Some(3),
                base_ref: Some("main".into()),
                layers: vec![layer(47, 1), layer(48, 2), layer(50, 3)],
            }),
            ..reviewed()
        };
        assert_eq!(
            board::stack_line(review.stacked()).as_deref(),
            Some("stack 2 of 3 · onto board/gh-44-packages · lands on main"),
        );
        assert_eq!(board::stack_map(review.stacked()).len(), 3);
        assert_eq!(
            board::merge_order(review.stacked()).as_deref(),
            Some("bottom-up: #47 lands before this one, #50 after"),
        );
        // The header's fact, and the whole reason the band is worth drawing:
        // GitHub says `clean` about this pull request either way.
        assert_eq!(
            board::landing_note(review.stacked()).as_deref(),
            Some("ready to land with 1 below"),
        );

        // Each chip is a door: a stack is scoped to one repository, so a
        // sibling is this URL with a different number on the end.
        assert_eq!(
            ReviewPanel::layer_url("https://github.com/o/r/pull/48", 47).as_deref(),
            Some("https://github.com/o/r/pull/47"),
        );
        // Nothing is derived off a shape this did not recognise.
        assert_eq!(ReviewPanel::layer_url("https://github.com/o/r", 47), None);

        // And an ordinary pull request draws none of it.
        let plain = reviewed();
        assert_eq!(board::stack_line(plain.stacked()), None);
        assert!(board::stack_map(plain.stacked()).is_empty());
        assert_eq!(board::merge_order(plain.stacked()), None);
        assert_eq!(
            board::landing_note(plain.stacked()),
            None,
            "nobody has asked GitHub, and a blank is honest",
        );
    }

    /// The merge key (gh#408): drawn while there is a pull request still to
    /// land, and worded by the board's own confirmation — the same sentence
    /// the row's confirm carries, off the review's facts alone.
    #[test]
    fn the_merge_key_is_drawn_only_while_there_is_something_to_press() {
        assert!(merge_available(&reviewed()));
        // A merged pull request has nothing left to press…
        let done = AttemptReview {
            state: "done".into(),
            ..reviewed()
        };
        assert!(!merge_available(&done));
        // …and a row with no pull request has nothing to merge.
        let no_pr = AttemptReview {
            pr_url: None,
            ..reviewed()
        };
        assert!(!merge_available(&no_pr));
        // Deliberately not gated on `landing`: nobody has asked GitHub about
        // this fixture, the key is drawn anyway, and the confirmation carries
        // the blank honestly — named row, addressed pull request, no verdict
        // invented.
        assert_eq!(
            board::merge_confirmation(reviewed().merge_subject()),
            "merge gh#138 (PR #212) into its base",
        );
    }

    /// The pill separates a review that wants a human from one already
    /// answered (§gh#238) — which, since the verdict can be sent from this
    /// screen (§gh#239), are otherwise the same screen exactly.
    #[test]
    fn the_pill_says_whose_turn_it_is() {
        for theme in [Theme::dark(), Theme::light()] {
            let waiting = ReviewPanel::turn_pill("review", false, &theme);
            assert_eq!(waiting.map(|(label, _)| label), Some("Waiting on you"));
            // Answered wins over the state, because it is newer: the board's
            // state moves when the sync loop next reads the pull request.
            let answered = ReviewPanel::turn_pill("review", true, &theme);
            assert_eq!(answered.map(|(label, _)| label), Some("Answered"));
            assert_ne!(waiting, answered, "the two must not paint alike");
            // A question is your turn too, and says which kind.
            assert_eq!(
                ReviewPanel::turn_pill("blocked", false, &theme).map(|(label, _)| label),
                Some("Blocked on you")
            );
            // Nobody is waiting on you while it runs, and nobody is once it is
            // over: no pill, rather than a badge on every review.
            for state in ["working", "ready", "done", "failed", "nonsense"] {
                assert!(
                    ReviewPanel::turn_pill(state, false, &theme).is_none(),
                    "{state}"
                );
            }
            // …and none of them borrows the screen's one loud hue, `blocked`
            // included: that hue belongs to the unclaimed set.
            for (state, answered) in [("review", false), ("blocked", false), ("review", true)] {
                let (_, color) = ReviewPanel::turn_pill(state, answered, &theme).unwrap();
                assert_ne!(color, theme.danger, "{state}");
            }
            // The waiting pill is the board's own review hue — the colour the
            // row it was opened from is painted in.
            assert_eq!(
                waiting.map(|(_, color)| color),
                Some(state_color(BoardState::Review, &theme))
            );
        }
    }

    /// The tone the surface paints is the reading's, never the renderer's —
    /// which is what stops this card and `comet-board review` from disagreeing
    /// about the same attempt.
    #[test]
    fn loudness_is_the_readings_answer_and_not_the_renderers() {
        assert!(Tone::Alarm.loud());
        assert!(!Tone::Settled.loud());
        assert!(!Tone::Unknown.loud());
        // Never-claimed and no-diff are quiet on purpose: there is nothing on
        // screen to point at, and an absence of evidence must not be painted
        // as evidence.
        assert!(!FindingKind::NeverClaimed.tone().loud());
        assert!(!FindingKind::NoDiff.tone().loud());
        assert!(FindingKind::Unclaimed.tone().loud());
        assert!(FindingKind::Uncommitted.tone().loud());
    }

    /// The effects row must not be able to shout (§gh#236). One hue on this
    /// screen means "look at this" and it belongs to the unclaimed set; a chip
    /// that borrowed it would leave the block that is actually the product
    /// with nothing of its own.
    #[test]
    fn no_chip_on_the_effects_row_wears_the_screens_one_loud_hue() {
        for theme in [Theme::dark(), Theme::light()] {
            for ground in [
                Ground::Neutral,
                Ground::Settled,
                Ground::Working,
                Ground::Unknown,
            ] {
                let (dot, text) = ReviewPanel::chip_colors(ground, &theme);
                assert_ne!(dot, theme.danger, "{ground:?} dot");
                assert_ne!(text, theme.danger, "{ground:?} text");
            }
            // …and an unknown chip carries no status colour at all, because
            // there is no status: "the board looked and nothing moved" and
            // "the board could not look" must not paint alike.
            let (unknown, _) = ReviewPanel::chip_colors(Ground::Unknown, &theme);
            let (neutral, _) = ReviewPanel::chip_colors(Ground::Neutral, &theme);
            assert_ne!(unknown, neutral);
            assert_ne!(unknown, theme.settled);
            assert_ne!(unknown, theme.warning);
        }
    }

    /// Three glyphs for three states, and the middle one is the point: a claim
    /// the diff supports and nothing corroborates must not be able to borrow
    /// the tick.
    #[test]
    fn a_claim_earns_its_tick_and_cannot_borrow_it() {
        assert_eq!(ClaimMark::Corroborated.glyph(), "✓");
        assert_eq!(ClaimMark::Bare.glyph(), "·");
        assert_eq!(ClaimMark::Contradicted.glyph(), "!");
        for theme in [Theme::dark(), Theme::light()] {
            // Only a contradicted claim is allowed the loud hue: a claim
            // nothing checks is quiet, not alarming.
            assert_eq!(
                ReviewPanel::claim_color(ClaimMark::Contradicted, &theme),
                theme.danger
            );
            assert_ne!(
                ReviewPanel::claim_color(ClaimMark::Bare, &theme),
                theme.danger
            );
            assert_ne!(
                ReviewPanel::claim_color(ClaimMark::Corroborated, &theme),
                theme.danger
            );
            // …and the three are three, or the tick and the dot would be one
            // state wearing two glyphs.
            assert_ne!(
                ReviewPanel::claim_color(ClaimMark::Bare, &theme),
                ReviewPanel::claim_color(ClaimMark::Corroborated, &theme)
            );
        }
    }
}
