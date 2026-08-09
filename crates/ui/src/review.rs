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
//! the authoring session becomes the narrow column beside it. The shell owns
//! that half (`shell::Route::Review`) — what this module renders is the card
//! itself.
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
//! **Unclaimed changes are the product, and they are the only thing here that
//! shouts.** One hue on this screen means "look at this", and it is the ramp's
//! blocked hue (gh#173) — the verdict strip under the header wears it, the
//! remainder block wears it, and the inline `!` marks that flag a contradiction
//! wear it. Nothing else on the page is allowed to; a screen where three things
//! shout has nothing left to shout with.
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
    AnyElement, Context, Entity, ScrollHandle, SharedString, Task, Window, div, prelude::*, px,
};

use comet_board::claims::{
    AnchorKind, AttemptReview, ChangedFile, ClaimView, DiffSource, FindingKind, Tone, Verdict,
    anchor_kind,
};
use comet_proto::view::board;
use comet_rpc::methods;

use crate::motion;
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

/// One attempt's review, fetched and drawn.
pub struct ReviewPanel {
    state: Entity<AppState>,
    /// The board task this review is of. Set once; a different task is a
    /// different panel (the shell replaces it rather than re-pointing it, so a
    /// stale reply can never land on the wrong review).
    task_id: String,
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
}

impl ReviewPanel {
    pub fn new(
        state: Entity<AppState>,
        task_id: String,
        attempt: Option<i64>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut panel = Self {
            state,
            task_id,
            attempt,
            review: None,
            host: None,
            loaded: false,
            error: None,
            task: None,
            body_scroll: ScrollHandle::new(),
            brief_scroll: ScrollHandle::new(),
        };
        panel.reload(cx);
        panel
    }

    /// Which task this panel is showing — the shell's check before it decides
    /// whether a navigation is a re-point or a new panel.
    pub fn task_id(&self) -> &str {
        &self.task_id
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
        self.task = Some(cx.spawn(async move |this, cx| {
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

    /// A section heading: the label, and the aside that qualifies it.
    fn heading(theme: &Theme, title: &str, aside: Option<String>) -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_baseline()
            .gap(px(Theme::SPACE_SM))
            .child(
                div()
                    .flex_none()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text_subtle)
                    .child(SharedString::from(title.to_string())),
            )
            .when_some(aside, |el, aside| {
                el.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(aside)),
                )
            })
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
    fn claim_row(claim: &ClaimView, changed: &[ChangedFile], theme: &Theme) -> AnyElement {
        // A claim nothing in the diff supports is the screen's second-loudest
        // row, after the unclaimed set itself.
        let unsupported = !claim.anchored();
        let matched: Vec<AnyElement> = claim
            .matched
            .iter()
            .filter_map(|path| changed.iter().find(|f| &f.path == path))
            .map(|file| Self::file_row(file, theme, false))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_XS))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(6.0))
                    .child(Self::mark(unsupported, theme))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(Theme::TEXT_BODY))
                            .text_color(if unsupported {
                                theme.text_muted
                            } else {
                                theme.text
                            })
                            .child(SharedString::from(claim.text.clone())),
                    ),
            )
            .when(!matched.is_empty(), |el| {
                el.child(
                    div()
                        .pl(px(16.0))
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .children(matched),
                )
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
                    .pl(px(16.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(FILE_GUTTER))
                    .font_family(theme.font_mono.clone())
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.danger)
                    .child(SharedString::from(label))
            }))
            .into_any_element()
    }

    /// The verdict strip: one line under the header, in the tone the reading
    /// gave it. Never absent — a review with nothing to report still says what
    /// it checked, or a quiet screen reads as a screen that failed to load.
    fn render_verdict(verdict: &Verdict, theme: &Theme) -> AnyElement {
        let color = Self::tone_color(verdict.tone, theme);
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_MD))
            .py(px(Theme::SPACE_SM))
            .rounded(px(Theme::RADIUS_ROW))
            .border_1()
            .border_color(if verdict.tone.loud() {
                color.opacity(0.45)
            } else {
                theme.border
            })
            .bg(if verdict.tone.loud() {
                color.opacity(0.10)
            } else {
                theme.wash(0.03)
            })
            .child(
                div()
                    .flex_none()
                    .font_family(theme.font_mono.clone())
                    .text_size(px(Theme::TEXT_BODY))
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
                        theme.text
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
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_SM))
            .child(Self::heading(
                theme,
                "THE BRIEF",
                Some(review.brief.url.clone()),
            ))
            .child(
                div()
                    .id("review-brief")
                    .max_h(px(BRIEF_MAX_H))
                    .overflow_y_scroll()
                    .track_scroll(&self.brief_scroll)
                    .flex()
                    .flex_col()
                    .child(body),
            )
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
                .flex()
                .flex_col()
                .gap(px(Theme::SPACE_SM))
                .child(Self::heading(theme, "CLAIMS", Some("unreadable".into())))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text)
                        .child(SharedString::from(
                            "This attempt wrote a claims block the board could not parse, \
                             so nothing was recorded from it.",
                        )),
                )
                .child(
                    div()
                        .font_family(theme.font_mono.clone())
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.danger)
                        .child(SharedString::from(err.clone())),
                )
                .into_any_element();
        }
        // Never asked and claimed nothing are different facts, and this is the
        // last place they could be flattened into one.
        if !review.claimed() {
            return div()
                .flex()
                .flex_col()
                .gap(px(Theme::SPACE_SM))
                .child(Self::heading(theme, "CLAIMS", None))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(
                            "This attempt never answered the claim contract. \
                             Nothing here is asserted, and nothing is checked.",
                        )),
                )
                .into_any_element();
        }
        // `claimed()` is `claimed_at.is_some()`, so past the guard above there
        // is always a timestamp to name.
        let aside = review
            .claimed_at
            .as_deref()
            .map(|at| format!("submitted {at}"));
        let claims: Vec<AnyElement> = review
            .remainder
            .claims
            .iter()
            .map(|claim| Self::claim_row(claim, &review.changed, theme))
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_SM))
            .child(Self::heading(theme, "CLAIMS", aside))
            .when(claims.is_empty(), |el| {
                el.child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from("Asked, and claimed nothing at all.")),
                )
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::SPACE_MD))
                    .children(claims),
            )
            .into_any_element()
    }

    /// What the board saw for itself: the commands the run executed, and how
    /// they exited. Never the agent's account of them — this comes off the run
    /// journal, which every harness writes without being asked.
    fn render_evidence(review: &AttemptReview, theme: &Theme) -> AnyElement {
        let evidence = &review.evidence;
        if evidence.commands == 0 {
            return div()
                .flex()
                .flex_col()
                .gap(px(Theme::SPACE_SM))
                .child(Self::heading(theme, "EVIDENCE", None))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(
                            "The board recorded no commands for this run.",
                        )),
                )
                .into_any_element();
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
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_SM))
            .child(Self::heading(theme, "EVIDENCE", Some(aside)))
            .when(rows.is_empty(), |el| {
                el.child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from("Nothing that verifies anything ran.")),
                )
            })
            .child(div().flex().flex_col().gap(px(3.0)).children(rows))
            .when(evidence.truncated, |el| {
                el.child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_faint)
                        .child(SharedString::from("…and more; the list is capped.")),
                )
            })
            .into_any_element()
    }

    /// The remainder — the only block on this screen that is allowed to shout.
    ///
    /// Drawn as a bordered, tinted card in the blocked hue when it is non-empty
    /// and as a plain quiet line when it is not, because "four files nobody
    /// mentioned" and "everything is accounted for" should not be two readings
    /// of the same shape.
    fn render_remainder(review: &AttemptReview, theme: &Theme) -> AnyElement {
        if let DiffSource::Unavailable { reason } = &review.diff {
            return div()
                .flex()
                .flex_col()
                .gap(px(Theme::SPACE_SM))
                .child(Self::heading(theme, "UNCLAIMED", None))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(format!(
                            "Unknown — there is no diff to check against: {reason}"
                        ))),
                )
                .into_any_element();
        }
        let loud = !review.remainder.complete();
        let total = review.changed.len();
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

        let body = div()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_SM))
            .when(loud, |el| {
                el.child(
                    // The count as a figure (gh#174's one off-ramp size): this
                    // is the number the screen exists to produce.
                    div()
                        .flex()
                        .flex_row()
                        .items_baseline()
                        .gap(px(Theme::SPACE_SM))
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(Theme::TEXT_FIGURE))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.danger)
                                .child(SharedString::from(
                                    review.remainder.unclaimed.len().to_string(),
                                )),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(Theme::TEXT_BODY))
                                .text_color(theme.text)
                                .child(SharedString::from(format!(
                                    "of {total} changed files are claimed by nobody"
                                ))),
                        ),
                )
                .child(
                    div().flex().flex_col().gap(px(2.0)).children(
                        review
                            .remainder
                            .unclaimed
                            .iter()
                            .map(|file| Self::file_row(file, theme, true)),
                    ),
                )
            })
            // No count here: the verdict strip at the top of the card already
            // carries it, and the same number said twice on one screen is the
            // reader wondering which of the two is the answer.
            .when(!loud && review.claimed(), |el| {
                el.child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.settled)
                        .child(SharedString::from(
                            "Nothing — every changed file is accounted for.",
                        )),
                )
            })
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
            });

        div()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_SM))
            .child(Self::heading(theme, "UNCLAIMED", None))
            .child(body.when(loud, |el| {
                el.p(px(Theme::SPACE_MD))
                    .rounded(px(Theme::RADIUS_ROW))
                    .border_1()
                    .border_color(theme.danger.opacity(0.45))
                    .bg(theme.danger.opacity(0.08))
            }))
            .into_any_element()
    }

    /// The card's own header: which attempt this is, and the links out.
    fn render_header(
        &self,
        review: &AttemptReview,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut facts = vec![format!("attempt {}", review.attempt_number.max(1))];
        if let Some(branch) = &review.branch {
            facts.push(branch.clone());
        }
        facts.push(match &review.outcome {
            Some(outcome) => outcome.clone(),
            None => "still running".to_string(),
        });
        let links: Vec<(SharedString, String)> = [
            review.pr_url.clone().map(|url| ("Open PR", url)),
            (!review.brief.url.is_empty()).then(|| ("Open issue", review.brief.url.clone())),
        ]
        .into_iter()
        .flatten()
        .map(|(label, url)| (SharedString::from(label), url))
        .collect();
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_XS))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::SPACE_SM))
                    .child(
                        div()
                            .flex_none()
                            .font_family(theme.font_mono.clone())
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(review.brief.identifier.clone())),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_faint)
                            .child(SharedString::from(facts.join(" · "))),
                    )
                    .children(links.into_iter().map(|(label, url)| {
                        div()
                            .id(SharedString::from(format!("review-link-{label}")))
                            .flex_none()
                            .h(px(20.0))
                            .px(px(8.0))
                            .flex()
                            .items_center()
                            .rounded(px(Theme::RADIUS_CHIP))
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.accent)
                            .hover(|s| s.bg(theme.wash(0.12)))
                            .child(label)
                            .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
                    })),
            )
            .child(
                div()
                    .text_size(px(Theme::TEXT_TITLE))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(review.brief.title.clone())),
            )
            .into_any_element()
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
            .text_size(px(Theme::TEXT_CAPTION))
            .text_color(theme.text_faint)
            .child(SharedString::from(format!(
                "Read from the board on {label}."
            )))
            .into_any_element()
    }
}

impl Render for ReviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let card = div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_MD))
            .px(px(Theme::SPACE_LG))
            .py(px(Theme::SPACE_MD));

        if let Some(error) = self.error.clone() {
            return card
                .child(Self::heading(&theme, "REVIEW", None))
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.warning)
                        .child(error),
                )
                .into_any_element();
        }
        let Some(review) = self.review.clone() else {
            return card
                .child(
                    div()
                        .text_size(px(Theme::TEXT_BODY))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(if self.loaded {
                            "No review for this task."
                        } else {
                            "Reading the review…"
                        })),
                )
                .into_any_element();
        };

        let header = self.render_header(&review, &theme, cx);
        let verdict = Self::render_verdict(&review.verdict(), &theme);
        let brief = self.render_brief(&review, &theme, window);
        let claims = Self::render_claims(&review, &theme);
        let evidence = Self::render_evidence(&review, &theme);
        let remainder = Self::render_remainder(&review, &theme);
        let host = self.render_host(&theme, cx);

        // The question in order — what was asked, what the agent says it did,
        // what the board saw for itself, and what nobody accounted for — with
        // the verdict pinned above the scroll so the loudest fact cannot be
        // pushed under the fold by a long issue body.
        let body = div()
            .id("review-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.body_scroll)
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_LG))
            .pb(px(Theme::SPACE_LG))
            .child(brief)
            .child(claims)
            .child(evidence)
            .child(remainder)
            .child(host);

        motion::fade_quick(
            SharedString::from(format!("review-in-{}", review.task_id)),
            card.child(header).child(verdict).child(body),
        )
        .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::state_color;
    use comet_proto::view::board::BoardState;

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
}
