//! The composer: a hand-rolled multiline text input (adapted from gpui's
//! `examples/input.rs`), the compact↔expanded flip, the Send/Steer/Stop morph,
//! optimistic send with failure recovery, per-chat drafts, and the question
//! wizard that replaces the composer while a run awaits input.
//!
//! Pure decision logic (flip, auto-grow math, button morph, wizard reducer,
//! pending-input detection) lives in free functions/structs with unit tests;
//! the gpui element only feeds them measurements.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, ClipboardEntry, ClipboardItem, Context, CursorStyle,
    ElementInputHandler, Entity, EntityInputHandler, EventEmitter, FocusHandle, Focusable,
    GlobalElementId, KeyBinding, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ObjectFit, PaintQuad, PathPromptOptions, Pixels, Point,
    SharedString, Style, StyledImage as _, Subscription, Task, TextRun, TextStyle, UTF16Selection,
    UnderlineStyle, Window, WrappedLine, actions, div, fill, img, point, prelude::*, px, relative,
    size,
};
use unicode_segmentation::UnicodeSegmentation;

use comet_doc::{
    MessagePart, MessageRole, QueueOp, QueuePause, QueueView, SessionCommandPayload,
    SessionMessageEntry,
};
use comet_proto::view::context as view_context;
use comet_proto::view::skills as view_skills;
use comet_proto::{
    ChatConfig, ContextMatch, ContextRef, ContextRefKind, ContextSearch, RunRequest, SandboxLevel,
    SkillDescriptor, UserInputAnswer, UserInputQuestion,
};
use comet_rpc::methods;

use crate::attachments::{self, StagedAttachment};
use crate::drafts::DraftStore;
use crate::motion;
use crate::pickers::Pickers;
use crate::state::{AppState, Indicator};
use crate::theme::{Bed, ListRow as _, Theme};

// ---------------------------------------------------------------------------
// Constants + pure decision logic
// ---------------------------------------------------------------------------

/// Expanded-mode textarea vertical padding: `pt-4 pb-1` (comet composer.tsx
/// line 578) = 16 + 4.
pub const TEXTAREA_PAD_V: f32 = 20.0;

fn draft_mutate_params(
    materialized: bool,
    chat_id: &str,
    space_id: &str,
    cwd: &str,
    branch: Option<&str>,
    config: &ChatConfig,
) -> serde_json::Value {
    let mut params = serde_json::json!({
        "op": if materialized { "finalizeChatDraft" } else { "createChat" },
        "chatId": chat_id,
        "spaceId": space_id,
        "cwd": cwd,
        "config": config,
    });
    if let (Some(branch), Some(object)) = (branch, params.as_object_mut()) {
        object.insert("branch".into(), serde_json::Value::String(branch.into()));
    }
    params
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextSelection {
    path: String,
    kind: comet_proto::ContextRefKind,
    checkout_id: String,
    token_start: usize,
    token_end: usize,
}

fn exact_token_at(text: &str, token: &str, start: usize) -> bool {
    let end = start.saturating_add(token.len());
    text.get(start..end) == Some(token)
        && text[..start]
            .chars()
            .next_back()
            .is_none_or(char::is_whitespace)
        && text[end..].chars().next().is_none_or(char::is_whitespace)
}

fn exact_token_occurrences<'a>(text: &'a str, token: &'a str) -> impl Iterator<Item = usize> + 'a {
    text.match_indices(token)
        .map(|(start, _)| start)
        .filter(|start| exact_token_at(text, token, *start))
}

fn selected_context_refs(
    text: &str,
    selections: Option<&Vec<ContextSelection>>,
) -> Vec<ContextRef> {
    let mut selected: Vec<_> = selections
        .into_iter()
        .flatten()
        .filter_map(|selection| {
            let expected = ContextRef {
                path: selection.path.clone(),
                kind: selection.kind,
                checkout_id: None,
            }
            .token();
            (selection.token_end == selection.token_start + expected.len()
                && exact_token_at(text, &expected, selection.token_start))
            .then(|| selection.clone())
        })
        .collect();
    selected.sort_by_key(|selection| selection.token_start);
    selected
        .into_iter()
        .map(|selection| ContextRef {
            path: selection.path,
            kind: selection.kind,
            checkout_id: Some(selection.checkout_id),
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerTextEdit {
    replaced: Range<usize>,
    inserted_len: usize,
}

fn apply_context_edit(selections: &mut Vec<ContextSelection>, edit: &ComposerTextEdit) {
    let removed_len = edit.replaced.end.saturating_sub(edit.replaced.start);
    let delta = edit.inserted_len as isize - removed_len as isize;
    selections.retain_mut(|selection| {
        if edit.replaced.end < selection.token_start {
            selection.token_start = selection.token_start.saturating_add_signed(delta);
            selection.token_end = selection.token_end.saturating_add_signed(delta);
            true
        } else {
            edit.replaced.start > selection.token_end
        }
    });
}

fn queued_context_selections(prompt: &str, context: &[ContextRef]) -> Vec<ContextSelection> {
    context
        .iter()
        .filter_map(|reference| {
            let checkout_id = reference.checkout_id.as_deref()?.trim();
            if checkout_id.is_empty()
                || context
                    .iter()
                    .filter(|other| other.path == reference.path)
                    .count()
                    != 1
            {
                return None;
            }
            let token = reference.token();
            let mut occurrences = exact_token_occurrences(prompt, &token);
            let token_start = occurrences.next()?;
            if occurrences.next().is_some() {
                return None;
            }
            Some(ContextSelection {
                path: reference.path.clone(),
                kind: reference.kind,
                checkout_id: checkout_id.to_string(),
                token_start,
                token_end: token_start + token.len(),
            })
        })
        .collect()
}

fn replace_with_queued_context(
    all: &mut HashMap<String, Vec<ContextSelection>>,
    draft_key: &str,
    prompt: &str,
    context: &[ContextRef],
) {
    let selections = queued_context_selections(prompt, context);
    if selections.is_empty() {
        all.remove(draft_key);
    } else {
        all.insert(draft_key.to_string(), selections);
    }
}

fn clear_draft_provenance(
    drafts: &mut DraftStore,
    context: &mut HashMap<String, Vec<ContextSelection>>,
    draft_key: &str,
) {
    drafts.remove(draft_key);
    context.remove(draft_key);
}

/// The key a draft (and its attachments, and its reference provenance) is
/// filed under: the chat, or — on the new-chat canvas, which has no chat yet —
/// the SPACE the canvas belongs to.
///
/// Space-scoped rather than one shared `""`, because the canvas is per space:
/// a key that could not tell two spaces apart showed the prompt typed for one
/// repo sitting in another repo's composer, one keypress from being sent
/// there. (`ensure_skills` keys its own answers the same way, for the same
/// reason.) A chat id is a uuid, so the `new:` prefix can never collide.
fn draft_key(selected_chat: Option<&str>, selected_space: Option<&str>) -> String {
    match (selected_chat, selected_space) {
        (Some(chat), _) => chat.to_string(),
        (None, Some(space)) => format!("new:{space}"),
        (None, None) => String::new(),
    }
}

fn take_context_selections(
    all: &mut HashMap<String, Vec<ContextSelection>>,
    draft_key: &str,
) -> Option<Vec<ContextSelection>> {
    all.remove(draft_key)
}

fn queue_add_gesture_key(chat_id: &str, prompt: &str, context: &[ContextRef]) -> String {
    serde_json::to_string(&(chat_id, prompt, context))
        .unwrap_or_else(|_| format!("{chat_id}:{prompt}"))
}
/// The expanded textarea BOX (content + padding) is clamped by the original's
/// auto-grow effect: `ta.style.height = Math.min(Math.max(scrollHeight, 76),
/// 260)` (comet composer.tsx line 235). The 76px floor applies even when
/// empty — it's what makes the always-expanded new-chat composer tall.
pub const TEXTAREA_MIN: f32 = 76.0;
pub const TEXTAREA_MAX: f32 = 260.0;
/// Expanded actions row: `pt-1` (4) + h-8 picker chips (32 — the tallest
/// children; composer/styles.tsx pickerChip) + `pb-2.5` (10) — comet
/// composer-actions.tsx line 60.
pub const ACTIONS_ROW_HEIGHT: f32 = 46.0;
/// The pill's 1px hairline, top + bottom (`rounded-[26px] border`).
pub const PILL_BORDER_V: f32 = 2.0;
/// Expanded composer bounds, border-box: 76 + 46 + 2 = 124 when empty (the
/// new-chat canvas), 260 + 46 + 2 = 308 at the content cap.
pub const COMPOSER_MIN_HEIGHT: f32 = TEXTAREA_MIN + ACTIONS_ROW_HEIGHT + PILL_BORDER_V;
pub const COMPOSER_MAX_HEIGHT: f32 = TEXTAREA_MAX + ACTIONS_ROW_HEIGHT + PILL_BORDER_V;
/// Compact pill, border-box: one-line textarea `py-3` (24) + one 22.75px line
/// (scrollHeight rounds to 47 in the original) + the 2px hairline = 49. The
/// compact cluster (`py-1.5` + h-8 = 44) is shorter, so the textarea wins.
pub const COMPACT_TOTAL_HEIGHT: f32 = 49.0;
/// Below this pill input width the composer always expands.
pub const MIN_COMPACT_INPUT_WIDTH: f32 = 200.0;
/// Input text metrics: `leading-relaxed` = 14 × 1.625 = 22.75. The composer is
/// prose — what you type is set at the size it is read back at (gh#174).
pub const INPUT_LINE_HEIGHT: f32 = 22.75;
pub const INPUT_TEXT_SIZE: f32 = Theme::TEXT_PROSE;
/// How long the `@` picker waits after a keystroke before asking the host
/// (gh#424). Long enough that a burst of typing is one search, short enough
/// that a menu still feels like it is tracking the caret.
const CONTEXT_SEARCH_DEBOUNCE_MS: u64 = 90;

/// How many characters of a queued follow-up the tray shows on its row.
const QUEUE_ROW_CHARS: usize = 120;

/// Single-select questions auto-advance after this long.
pub const AUTO_ADVANCE_MS: u64 = 220;

/// Hysteresis slack for the expanded→compact flip: once expanded, the composer
/// only collapses when the text is comfortably narrower than the compact
/// capacity — expanding and collapsing share no boundary, so a width right at
/// the flip threshold can't oscillate between the two layouts.
pub const COLLAPSE_HYSTERESIS: f32 = 32.0;
/// During an interactive window resize the current mode is frozen until the
/// measured widths have been stable this long.
pub const RESIZE_SETTLE_MS: u64 = 150;

/// Compact↔expanded flip with hysteresis. `capacity` is the *compact-mode*
/// input capacity (a layout-stable width: measured while compact, tracked by
/// container-width deltas while expanded — never the post-flip measured width,
/// which differs per mode and would feed back into the decision):
/// - a newline always expands;
/// - while `resizing`, the current mode is kept (no flip until sizes settle);
/// - a too-narrow pill (`capacity < MIN_COMPACT_INPUT_WIDTH`) always expands;
/// - compact expands only when `text_width > capacity`; expanded collapses
///   only when `text_width < capacity - COLLAPSE_HYSTERESIS`.
pub fn composer_flip(
    expanded: bool,
    text_width: f32,
    capacity: f32,
    has_newline: bool,
    resizing: bool,
) -> bool {
    if has_newline {
        return true;
    }
    if resizing {
        return expanded;
    }
    if capacity < MIN_COMPACT_INPUT_WIDTH {
        return true;
    }
    if expanded {
        text_width >= capacity - COLLAPSE_HYSTERESIS
    } else {
        text_width > capacity
    }
}

/// Caret blink half-period (standard textarea cadence: ~500ms on / 500ms off).
pub const CARET_BLINK_MS: u64 = 500;

/// Caret blink phase for a time since the last keystroke/caret move: solid
/// through the first half-period (typing bursts never blink — each keystroke
/// resets the phase), then alternating.
pub fn caret_visible(ms_since_activity: u64) -> bool {
    (ms_since_activity / CARET_BLINK_MS) % 2 == 0
}

/// Auto-grow: content height for a wrapped-line count.
pub fn input_content_height(wrapped_lines: usize) -> f32 {
    wrapped_lines.max(1) as f32 * INPUT_LINE_HEIGHT
}

/// Total expanded composer height (border-box) for a content height: the
/// textarea BOX (content + `pt-4 pb-1`) clamps to 76–260 exactly like the
/// original's auto-grow effect, then the 46px actions row and the hairline
/// ride on top. Range 124–308.
pub fn composer_total_height(content_height: f32) -> f32 {
    (content_height + TEXTAREA_PAD_V).clamp(TEXTAREA_MIN, TEXTAREA_MAX)
        + ACTIONS_ROW_HEIGHT
        + PILL_BORDER_V
}

/// Staged-attachment strip metrics (comet attachment-ui.tsx AttachmentStrip:
/// `flex flex-wrap gap-2 px-4 pt-3`, `size-14` thumbs).
pub const STRIP_THUMB: f32 = 56.0;
pub const STRIP_GAP: f32 = 8.0;
pub const STRIP_PAD_TOP: f32 = 12.0;
pub const STRIP_PAD_X: f32 = 16.0;

/// Height the wrap strip adds to the pill for `count` staged thumbnails at an
/// `inner_width` pill content width (0 when empty). Mirrors flex-wrap: as many
/// 56px thumbs per row as fit with 8px gaps inside the 16px side insets.
pub fn attachment_strip_height(count: usize, inner_width: f32) -> f32 {
    if count == 0 {
        return 0.0;
    }
    let usable = (inner_width - 2.0 * STRIP_PAD_X).max(STRIP_THUMB);
    let per_row = (((usable + STRIP_GAP) / (STRIP_THUMB + STRIP_GAP)).floor() as usize).max(1);
    let rows = count.div_ceil(per_row);
    STRIP_PAD_TOP + rows as f32 * STRIP_THUMB + (rows - 1) as f32 * STRIP_GAP
}

/// Compact↔expanded flip morph (round 9): the flip used to snap between the
/// two pill layouts. The original has no height transition (its shell carries
/// only `transition-colors`), so this is a native nicety: ONE committed flip
/// starts exactly one 180ms ease-out morph ([`motion::COLLAPSE`], the same
/// manual-drive pattern as shell.rs `WidthTween` — never `with_animation`,
/// whose element-id keying replays tweens on remount, round-6 §1–3).
///
/// The morph animates the pill's COMMITTED height: the flip commits its final
/// layout immediately (the input entity never remounts — the caret survives,
/// exactly as before) while the pill clips toward the live target. The pill's
/// bottom edge is stationary on screen, so the controls stay pinned to it
/// (constant screen-y; see the anchoring helpers below) and only the text
/// glides with the sweeping top edge. [`composer_flip`]'s hysteresis already
/// guarantees no oscillation at the boundary, and [`flip_morph_step`] never
/// restarts a morph while the committed mode holds. Reduced motion snaps: no
/// morph is ever created.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlipMorph {
    /// Rendered height when the flip committed — the animation's start point.
    pub from: f32,
    /// Commit time in ms on the caller's monotonic clock.
    pub start_ms: f32,
}

impl FlipMorph {
    /// Raw timeline position 0..1 over [`motion::COLLAPSE`]'s 180ms.
    fn raw(&self, now_ms: f32) -> f32 {
        let total = motion::COLLAPSE.total().as_secs_f32() * 1000.0;
        ((now_ms - self.start_ms) / total).clamp(0.0, 1.0)
    }

    /// Eased progress 0..1 (ease-out) — also drives the actions fade.
    pub fn progress(&self, now_ms: f32) -> f32 {
        motion::COLLAPSE.progress(self.raw(now_ms))
    }

    pub fn done(&self, now_ms: f32) -> bool {
        self.raw(now_ms) >= 1.0
    }

    /// Committed-height evaluation: eased lerp from the flip-time height to
    /// the LIVE target (auto-grow may move the target mid-morph — the morph
    /// tracks it instead of finishing on a stale height).
    pub fn height(&self, target: f32, now_ms: f32) -> f32 {
        motion::lerp(self.from, target, self.progress(now_ms))
    }
}

// -- morph anchoring (round-9 follow-up) ------------------------------------
// The pill sits at the BOTTOM of the shell column: growing it moves its TOP
// edge; the bottom edge is stationary on screen. The first morph cut anchored
// the pill's inner content to the top, so the actions/cluster (laid out at
// the inner bottom) rode the animating height up and down. The controls are
// therefore pinned to the stationary bottom edge (absolute bottom row when
// expanded, a bottom-justified row when compact) and only the TEXT glides
// with the sweeping top edge. The helpers below are the pure math.

/// Send/attach center sits 27px above the pill's outer bottom in expanded
/// mode (`pb-2.5` 10 + half the 32px content zone + 1px hairline) but 24.5px
/// in compact (centered in the 47px row) — an inherent 2.5px delta between
/// the two SOURCE geometries. The morph glides it instead of snapping.
pub const CLUSTER_Y_DELTA: f32 = 2.5;

/// The cluster's INTERNAL spacing is mode-independent in the source — it is
/// ONE element (`clusterRef`: `gap-1` chips + `ml-1` attach) reused by both
/// layouts, so inter-button distances never change across the flip (round 9:
/// branch-specific gaps read as a horizontal compression pulse mid-morph).
/// Only the wrapper's right inset differs: `pr-2` (8) compact vs `px-3` (12)
/// expanded — a whole-cluster 4px shift that glides with the morph.
pub const CLUSTER_X_DELTA: f32 = 4.0;

/// The right inset for the in-flight morph: eases from the OLD mode's resting
/// inset to the committed mode's (compact 8 ↔ expanded 12) — pairwise button
/// distances stay constant; the cluster glides as one.
pub fn morph_cluster_inset(expanded: bool, progress: f32) -> f32 {
    let (from, to) = if expanded {
        (8.0, 8.0 + CLUSTER_X_DELTA)
    } else {
        (8.0 + CLUSTER_X_DELTA, 8.0)
    };
    motion::lerp(from, to, progress)
}

/// Expanded text top padding across the morph: starts at the compact resting
/// inset (12 ≈ `py-3`) and eases to `pt-4` (16) — the first line glides with
/// the rising top edge instead of jumping at the commit.
pub fn morph_text_pad(progress: f32) -> f32 {
    motion::lerp(12.0, 16.0, progress)
}

/// Collapse-morph text glide: the committed compact row is bottom-anchored
/// (text resting top = 36px above the pill's outer bottom: 49 − 1 hairline −
/// 12 centering inset), while at the commit instant the text sat 17px below
/// the expanded pill's top (1 hairline + 16 `pt-4`) — i.e. `from − 17` above
/// the bottom. The decaying relative offset walks it down smoothly.
pub fn collapse_text_glide(from: f32, progress: f32) -> f32 {
    (from - 53.0).max(0.0) * (1.0 - progress)
}

/// The decaying [`CLUSTER_Y_DELTA`] offset for the in-flight morph.
/// The whole control cluster — chips AND attach/send — rides the stationary
/// bottom anchor at FULL alpha throughout (round-9 follow-up: any fade on the
/// picker chips read as flicker; their screen position is near-stationary
/// across the flip, so nothing needs to be hidden).
pub fn morph_cluster_dy(progress: f32) -> f32 {
    CLUSTER_Y_DELTA * (1.0 - progress)
}

/// Session/route changes SNAP the composer (same rule as the header inset
/// tween, round 6: route swaps remount in the original — zero motion). The
/// nav-driven flip doesn't commit on the first render after a switch (the
/// draft swap has to be laid out and re-measured first), so a plain reset at
/// the nav instant leaks: `last_rendered_height` is repopulated before the
/// flip lands and the session change morphs 49↔124. Instead, every flip
/// committed within this wall-clock window of a navigation snaps. User-driven
/// flips need typing and can't land this fast after a switch.
pub const ROUTE_SNAP_MS: u64 = 250;

/// Advance the flip morph across one render pass. While the committed mode
/// holds, the morph is kept (a finished one clears) — same-mode renders can
/// NEVER restart the animation. A committed mode change starts one morph from
/// the last rendered height, which mid-flight is the CURRENT animated height,
/// so a reverse flip hands off seamlessly instead of popping to an endpoint.
/// Reduced motion (or a first paint with no measured height yet) snaps, and
/// `route_snap` (a session/route change within [`ROUTE_SNAP_MS`]) both blocks
/// arming AND kills anything in flight — navigation never animates the pill.
pub fn flip_morph_step(
    morph: Option<FlipMorph>,
    mode_changed: bool,
    last_height: f32,
    now_ms: f32,
    reduced_motion: bool,
    route_snap: bool,
) -> Option<FlipMorph> {
    if route_snap {
        return None;
    }
    if !mode_changed {
        return morph.filter(|m| !m.done(now_ms));
    }
    if reduced_motion || last_height <= 0.0 {
        return None;
    }
    Some(FlipMorph {
        from: last_height,
        start_ms: now_ms,
    })
}

/// What the send button is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendButtonMode {
    /// No live run: plain send.
    Send,
    /// Live steerable run with text typed: "Send (steers the current run)".
    Steer,
    /// Live run, nothing typed: red stop square.
    Stop,
}

pub fn send_button_mode(run_live: bool, has_text: bool) -> SendButtonMode {
    match (run_live, has_text) {
        (false, _) => SendButtonMode::Send,
        (true, true) => SendButtonMode::Steer,
        (true, false) => SendButtonMode::Stop,
    }
}

/// What pressing send actually does (gh#424).
///
/// Four outcomes, and the product requirement is that they stay four — that a
/// person can say which one they are about to get before they press anything:
///
/// | state                 | verb    | what it does                                |
/// |-----------------------|---------|---------------------------------------------|
/// | idle                  | Send    | starts a turn now                            |
/// | running, Enter        | Steer   | joins the LIVE turn; a newer steer supersedes|
/// | running, ⌘/Ctrl-Enter | Queue   | waits for the turn, in a visible ordered plan|
/// | running, no text      | Stop    | ends the turn — and holds the plan           |
///
/// Steer stays the default because it is the existing behaviour and the common
/// intent mid-turn ("no, not that"); queueing is the deliberate second gesture,
/// because "do this next" is a thing you decide, not a thing you fall into.
/// Idle never queues: with nothing running there is nothing to wait for, and a
/// follow-up that ran instantly would be a send with extra steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendIntent {
    Send,
    Steer,
    Queue,
    Stop,
}

pub fn send_intent(run_live: bool, has_text: bool, queue_modifier: bool) -> SendIntent {
    match (run_live, has_text) {
        (false, _) => SendIntent::Send,
        (true, false) => SendIntent::Stop,
        (true, true) if queue_modifier => SendIntent::Queue,
        (true, true) => SendIntent::Steer,
    }
}

/// A multi-line prompt as one tray row: newlines collapsed, elided at `max`
/// characters. Collapsed rather than clipped by the layout because a queued
/// follow-up is often a paragraph, and the first line of one says less about it
/// than the first `max` characters of the whole.
pub fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

/// Find the unresolved input request the panel should serve, if any: an
/// unresolved input part on the LAST assistant entry — regardless of the
/// entry's run status. The question stays answerable until the user actually
/// answers it (user requirement): a run that died under its question (engine
/// restart reaping it) leaves an aborted entry whose answer the engine
/// delivers as a resumed turn (`RespondInput`'s dead-run fallback). A newer
/// assistant entry supersedes an unanswered question. Assistant-entry-scoped,
/// not last-entry: a steer prompt sent while the agent waits appends a USER
/// entry after the streaming assistant entry, and a last-entry-only read made
/// the QuestionPanel vanish exactly when the user typed (earlier forensics;
/// matches the original composer.tsx, which reads the live-assistant fold —
/// rebuilt from replay even after the run died).
pub fn pending_input_request(
    transcript: &[SessionMessageEntry],
) -> Option<(String, Vec<UserInputQuestion>)> {
    transcript
        .iter()
        .rev()
        .find(|entry| entry.role == MessageRole::Assistant)
        .and_then(|entry| {
            entry.parts.iter().find_map(|part| match part {
                MessagePart::Input {
                    request_id,
                    questions,
                    resolved: false,
                    ..
                } => Some((request_id.clone(), questions.clone())),
                _ => None,
            })
        })
}

/// Whether the transcript shows `request_id` explicitly resolved (here or on
/// another device) — the wizard latch's release condition.
pub fn input_request_resolved(transcript: &[SessionMessageEntry], request_id: &str) -> bool {
    transcript.iter().any(|entry| {
        entry.parts.iter().any(|part| {
            matches!(
                part,
                MessagePart::Input {
                    request_id: rid,
                    resolved: true,
                    ..
                } if rid == request_id
            )
        })
    })
}

// ---------------------------------------------------------------------------
// Question wizard (pure reducer)
// ---------------------------------------------------------------------------

/// Reducer outcome of a wizard interaction.
#[derive(Debug, Clone, PartialEq)]
pub enum WizardStep {
    Stay,
    /// Single-select landed — advance after [`AUTO_ADVANCE_MS`].
    AutoAdvance,
    /// All pages answered — submit these answers.
    Done(Vec<UserInputAnswer>),
}

/// Paged question state ("1/3"): single-select auto-advances, multi-select and
/// typed answers advance explicitly, number keys 1-9 select, Back pages back.
#[derive(Debug, Clone)]
pub struct Wizard {
    pub request_id: String,
    pub questions: Vec<UserInputQuestion>,
    pub page: usize,
    picked: Vec<Vec<usize>>,
    typed: Vec<String>,
}

impl Wizard {
    pub fn new(request_id: String, questions: Vec<UserInputQuestion>) -> Self {
        let n = questions.len();
        Self {
            request_id,
            questions,
            page: 0,
            picked: vec![Vec::new(); n],
            typed: vec![String::new(); n],
        }
    }

    pub fn counter(&self) -> String {
        format!("{}/{}", self.page + 1, self.questions.len().max(1))
    }

    pub fn current(&self) -> Option<&UserInputQuestion> {
        self.questions.get(self.page)
    }

    pub fn is_picked(&self, option_ix: usize) -> bool {
        self.picked
            .get(self.page)
            .is_some_and(|p| p.contains(&option_ix))
    }

    /// Whether the current page has any picked option.
    pub fn page_has_pick(&self) -> bool {
        self.picked.get(self.page).is_some_and(|p| !p.is_empty())
    }

    /// Click/tap an option.
    pub fn select(&mut self, option_ix: usize) -> WizardStep {
        let Some(question) = self.questions.get(self.page) else {
            return WizardStep::Stay;
        };
        if option_ix >= question.options.len() {
            return WizardStep::Stay;
        }
        let multi = question.multi_select;
        let Some(picked) = self.picked.get_mut(self.page) else {
            return WizardStep::Stay;
        };
        if multi {
            match picked.iter().position(|&p| p == option_ix) {
                Some(at) => {
                    picked.remove(at);
                }
                None => picked.push(option_ix),
            }
            WizardStep::Stay
        } else {
            *picked = vec![option_ix];
            WizardStep::AutoAdvance
        }
    }

    /// Number key 1-9.
    pub fn press_number(&mut self, number: usize) -> WizardStep {
        if number == 0 {
            return WizardStep::Stay;
        }
        self.select(number - 1)
    }

    pub fn set_typed(&mut self, text: String) {
        if let Some(slot) = self.typed.get_mut(self.page) {
            *slot = text;
        }
    }

    /// Explicit submit / auto-advance landing.
    pub fn advance(&mut self) -> WizardStep {
        if self.page + 1 < self.questions.len() {
            self.page += 1;
            WizardStep::Stay
        } else {
            WizardStep::Done(self.answers())
        }
    }

    /// Page back; false when already on the first page.
    pub fn back(&mut self) -> bool {
        if self.page > 0 {
            self.page -= 1;
            true
        } else {
            false
        }
    }

    /// Answers per question: free text overrides picked labels.
    pub fn answers(&self) -> Vec<UserInputAnswer> {
        self.questions
            .iter()
            .enumerate()
            .map(|(ix, q)| {
                let typed = self.typed.get(ix).map(|s| s.trim()).unwrap_or("");
                let labels = if !typed.is_empty() {
                    vec![typed.to_string()]
                } else {
                    self.picked
                        .get(ix)
                        .map(|picked| {
                            picked
                                .iter()
                                .filter_map(|&p| q.options.get(p).cloned())
                                .collect()
                        })
                        .unwrap_or_default()
                };
                UserInputAnswer {
                    question_id: q.id.clone(),
                    labels,
                }
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Multiline text input (adapted from gpui examples/input.rs)
// ---------------------------------------------------------------------------

actions!(
    composer,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        WordLeft,
        WordRight,
        Copy,
        Cut,
        Paste,
        Newline,
        Submit,
        /// ⌘/Ctrl-Enter: queue the text as a follow-up instead of steering the
        /// live turn (gh#424).
        QueueSubmit,
    ]
);

/// Bind the composer keymap. Call once at app boot.
pub fn init(cx: &mut App) {
    let ctx = Some("Composer");
    let mut bindings = vec![
        KeyBinding::new("enter", Submit, ctx),
        KeyBinding::new("shift-enter", Newline, ctx),
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
    ];
    // Word navigation and clipboard: bind both modifier conventions so the same
    // map works across platforms.
    for prefix in ["ctrl", "alt"] {
        bindings.push(KeyBinding::new(&format!("{prefix}-left"), WordLeft, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-right"), WordRight, ctx));
    }
    for prefix in ["cmd", "ctrl"] {
        bindings.push(KeyBinding::new(&format!("{prefix}-a"), SelectAll, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-c"), Copy, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-x"), Cut, ctx));
        bindings.push(KeyBinding::new(&format!("{prefix}-v"), Paste, ctx));
        // Both conventions, like the clipboard above: the same gesture must
        // queue a follow-up on a Mac and on the Linux box people mosh into.
        bindings.push(KeyBinding::new(
            &format!("{prefix}-enter"),
            QueueSubmit,
            ctx,
        ));
    }
    // Palette-search context: TEXT-EDITING keys only. gpui dispatches matched
    // keybindings BEFORE raw key listeners (window.rs `dispatch_key_event`),
    // so anything bound here can never reach a palette's `on_key_down` —
    // navigation keys (up/down/left/right/enter) are deliberately unbound and
    // bubble to the palette frame instead.
    let palette = Some("PaletteSearch");
    let mut palette_bindings = vec![
        KeyBinding::new("backspace", Backspace, palette),
        KeyBinding::new("delete", Delete, palette),
        KeyBinding::new("home", Home, palette),
        KeyBinding::new("end", End, palette),
        KeyBinding::new("shift-left", SelectLeft, palette),
        KeyBinding::new("shift-right", SelectRight, palette),
    ];
    for prefix in ["cmd", "ctrl"] {
        palette_bindings.push(KeyBinding::new(&format!("{prefix}-a"), SelectAll, palette));
        palette_bindings.push(KeyBinding::new(&format!("{prefix}-c"), Copy, palette));
        palette_bindings.push(KeyBinding::new(&format!("{prefix}-x"), Cut, palette));
        palette_bindings.push(KeyBinding::new(&format!("{prefix}-v"), Paste, palette));
    }
    cx.bind_keys(palette_bindings);
    cx.bind_keys(bindings);
}

/// Where a keypress moves a completion menu the input is hosting (gh#134).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuNav {
    Up,
    Down,
    /// Enter with the menu up: complete the highlighted row, do not send.
    Accept,
}

/// Events the composer wrapper listens for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerInputEvent {
    Submitted,
    /// ⌘/Ctrl-Enter — send as a queued follow-up (gh#424).
    QueueSubmitted,
    Edited(Option<ComposerTextEdit>),
    /// A navigation key arrived while a completion menu was up — the wrapper
    /// owns the menu, so the input only reports the intent.
    Menu(MenuNav),
    /// Images pasted from the clipboard (screenshots / copied image data) —
    /// the wrapper stages them as attachments (use-attachments.ts onPaste).
    PastedImages(Vec<gpui::Image>),
    /// File paths pasted from the clipboard (a file manager "Copy").
    PastedPaths(Vec<PathBuf>),
}

/// Multiline input entity: content + selection + IME marked text + measured
/// layout (wrapped lines) for mouse mapping and auto-grow.
pub struct ComposerInput {
    /// Key context for the binding map ("Composer", or "PaletteSearch" for
    /// palette filters whose navigation keys must bubble).
    key_context: &'static str,
    focus_handle: FocusHandle,
    content: String,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    is_selecting: bool,
    /// A completion menu is up (the wrapper's `/` picker, gh#134): up/down/
    /// Enter belong to it, not to the caret. Set by the wrapper, because the
    /// menu's contents — and therefore whether there is one — depend on state
    /// the input has no business holding.
    menu_active: bool,
    /// Vertical scroll inside the input once content exceeds the max height.
    scroll_top: f32,
    // -- measured state (written during layout/paint) --
    last_lines: Vec<WrappedLine>,
    line_starts: Vec<usize>,
    last_bounds: Option<Bounds<Pixels>>,
    line_height: Pixels,
    content_height: f32,
    max_line_width: f32,
    last_width: f32,
    /// Bumped once per `layout_text` pass — the flip logic uses it to apply at
    /// most one compact↔expanded flip per layout (a flip is only re-evaluated
    /// after the input has been measured in the new mode).
    layout_epoch: u64,
    display_is_placeholder: bool,
    /// Caret blink anchor: reset on every keystroke/caret move so the caret is
    /// solid while typing and blinks at [`CARET_BLINK_MS`] when idle.
    blink_anchor: Instant,
    /// Half-period repaint driver, alive only while the input is focused.
    blink_task: Option<Task<()>>,
}

impl ComposerInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self::with_context(placeholder, "Composer", cx)
    }

    /// An input in a custom KEY context — palettes use `"PaletteSearch"`,
    /// whose keymap binds only text-editing keys so navigation keys bubble to
    /// the surrounding frame (see `init`).
    pub fn with_context(
        placeholder: impl Into<SharedString>,
        key_context: &'static str,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            key_context,
            focus_handle: cx.focus_handle(),
            content: String::new(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            is_selecting: false,
            menu_active: false,
            scroll_top: 0.0,
            last_lines: Vec::new(),
            line_starts: vec![0],
            last_bounds: None,
            line_height: px(INPUT_LINE_HEIGHT),
            content_height: INPUT_LINE_HEIGHT,
            max_line_width: 0.0,
            last_width: 0.0,
            layout_epoch: 0,
            display_is_placeholder: true,
            blink_anchor: Instant::now(),
            blink_task: None,
        }
    }

    /// Reset the caret blink phase (solid again) — called on every edit and
    /// caret move, matching textarea behavior.
    fn reset_blink(&mut self) {
        self.blink_anchor = Instant::now();
    }

    /// Caret paint gate: focused input in an active window, in the "on" blink
    /// phase. Also (re)arms the half-period repaint driver while focused, and
    /// drops it on blur so an unfocused input schedules no frames.
    fn caret_shown(&mut self, window: &Window, cx: &mut Context<Self>) -> bool {
        let focused = self.focus_handle.is_focused(window);
        if !focused || !window.is_window_active() {
            self.blink_task = None;
            return false;
        }
        if self.blink_task.is_none() {
            self.blink_task = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(CARET_BLINK_MS))
                        .await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
        caret_visible(self.blink_anchor.elapsed().as_millis() as u64)
    }

    pub fn text(&self) -> &str {
        &self.content
    }

    /// Caret byte offset (the selection's active end).
    pub fn caret(&self) -> usize {
        self.cursor_offset()
    }

    /// Tell the input a completion menu is up; see [`ComposerInput::menu_active`].
    pub fn set_menu_active(&mut self, active: bool) {
        self.menu_active = active;
    }

    /// Replace the buffer AND place the caret — the completion path, which
    /// (unlike [`ComposerInput::set_text`]) has somewhere specific to leave it.
    pub fn set_text_with_caret(
        &mut self,
        text: impl Into<String>,
        caret: usize,
        cx: &mut Context<Self>,
    ) {
        self.content = text.into();
        let caret = caret.min(self.content.len());
        self.selected_range = caret..caret;
        self.selection_reversed = false;
        self.marked_range = None;
        self.reset_blink();
        cx.emit(ComposerInputEvent::Edited(None));
        cx.notify();
    }

    pub fn is_empty(&self) -> bool {
        self.content.is_empty()
    }

    pub fn has_newline(&self) -> bool {
        self.content.contains('\n')
    }

    /// Unwrapped width of the widest line — feeds the compact/expanded flip.
    pub fn measured_text_width(&self) -> f32 {
        self.max_line_width
    }

    pub fn measured_content_height(&self) -> f32 {
        self.content_height
    }

    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.scroll_top = 0.0;
        self.reset_blink();
        cx.emit(ComposerInputEvent::Edited(None));
        cx.notify();
    }

    // ---- editing ops ----

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.reset_blink();
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.reset_blink();
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(ix, _)| (ix < offset).then_some(ix))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(ix, _)| (ix > offset).then_some(ix))
            .unwrap_or(self.content.len())
    }

    fn previous_word_boundary(&self, offset: usize) -> usize {
        self.content
            .split_word_bound_indices()
            .rev()
            .find_map(|(ix, word)| (ix < offset && !word.trim().is_empty()).then_some(ix))
            .unwrap_or(0)
    }

    fn next_word_boundary(&self, offset: usize) -> usize {
        self.content
            .split_word_bound_indices()
            .find_map(|(ix, word)| {
                let end = ix + word.len();
                (end > offset && !word.trim().is_empty()).then_some(end)
            })
            .unwrap_or(self.content.len())
    }

    /// Byte range of the logical line containing `offset`.
    fn line_range_at(&self, offset: usize) -> Range<usize> {
        let start = self.content[..offset]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = self.content[offset..]
            .find('\n')
            .map(|i| offset + i)
            .unwrap_or(self.content.len());
        start..end
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                return;
            }
            self.select_to(prev, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                return;
            }
            self.select_to(next, cx);
        }
        self.replace_text_in_range(None, "", window, cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            self.move_to(prev, cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.selected_range.end);
            self.move_to(next, cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }

    fn up(&mut self, _: &Up, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu_active {
            cx.emit(ComposerInputEvent::Menu(MenuNav::Up));
            return;
        }
        self.move_vertical(-1.0, cx);
    }

    fn down(&mut self, _: &Down, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu_active {
            cx.emit(ComposerInputEvent::Menu(MenuNav::Down));
            return;
        }
        self.move_vertical(1.0, cx);
    }

    fn move_vertical(&mut self, dir: f32, cx: &mut Context<Self>) {
        let Some(current) = self.point_for_index(self.cursor_offset()) else {
            return;
        };
        let target_y = f32::from(current.y) + dir * f32::from(self.line_height);
        if target_y < 0.0 {
            self.move_to(0, cx);
            return;
        }
        if target_y >= self.content_height {
            self.move_to(self.content.len(), cx);
            return;
        }
        let ix = self.index_for_point(point(current.x, px(target_y)));
        self.move_to(ix, cx);
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_range_at(self.cursor_offset());
        self.move_to(line.start, cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let line = self.line_range_at(self.cursor_offset());
        self.move_to(line.end, cx);
    }

    fn word_left(&mut self, _: &WordLeft, _: &mut Window, cx: &mut Context<Self>) {
        let prev = self.previous_word_boundary(self.cursor_offset());
        self.move_to(prev, cx);
    }

    fn word_right(&mut self, _: &WordRight, _: &mut Window, cx: &mut Context<Self>) {
        let next = self.next_word_boundary(self.cursor_offset());
        self.move_to(next, cx);
    }

    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        } else if let Some(text) = crate::markdown::selection::selected_text() {
            // The composer keeps focus while the user reads the transcript —
            // Cmd+C with no input selection copies the markdown selection.
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_text_in_range(None, "", window, cx);
        }
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        // Image data (or copied files) beats text — the original composer's
        // onPaste prevents the default text insert when `clipboardData.files`
        // is non-empty and stages the images instead.
        let mut images: Vec<gpui::Image> = Vec::new();
        let mut paths: Vec<PathBuf> = Vec::new();
        for entry in &item.entries {
            match entry {
                ClipboardEntry::Image(image) => images.push(image.clone()),
                ClipboardEntry::ExternalPaths(files) => {
                    paths.extend(files.paths().iter().cloned());
                }
                ClipboardEntry::String(_) => {}
            }
        }
        if !images.is_empty() {
            cx.emit(ComposerInputEvent::PastedImages(images));
            return;
        }
        if !paths.is_empty() {
            cx.emit(ComposerInputEvent::PastedPaths(paths));
            return;
        }
        if let Some(text) = item.text() {
            // Multiline input: newlines are welcome (unlike the single-line example).
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "\n", window, cx);
    }

    fn submit(&mut self, _: &Submit, _: &mut Window, cx: &mut Context<Self>) {
        // With a completion menu up, Enter completes rather than sends. gpui
        // dispatches bound actions before raw key listeners, so this is the
        // only place that decision can be made — a listener on the composer
        // frame would never see the key.
        if self.menu_active {
            cx.emit(ComposerInputEvent::Menu(MenuNav::Accept));
            return;
        }
        cx.emit(ComposerInputEvent::Submitted);
    }

    /// ⌘/Ctrl-Enter (gh#424). A completion menu takes it too — accepting the
    /// highlighted row is what the person looking at an open menu means by any
    /// flavour of Enter, and queueing a half-typed `@src/comp` would be the
    /// least useful reading of the keystroke available.
    fn queue_submit(&mut self, _: &QueueSubmit, _: &mut Window, cx: &mut Context<Self>) {
        if self.menu_active {
            cx.emit(ComposerInputEvent::Menu(MenuNav::Accept));
            return;
        }
        cx.emit(ComposerInputEvent::QueueSubmitted);
    }

    // ---- geometry ----

    /// Content-local point for a byte index (y grows down from content top).
    fn point_for_index(&self, index: usize) -> Option<Point<Pixels>> {
        for (line_ix, line) in self.last_lines.iter().enumerate() {
            let line_start = *self.line_starts.get(line_ix)?;
            let line_len = line.len();
            if index < line_start {
                continue;
            }
            if index <= line_start + line_len {
                let local = line.position_for_index(index - line_start, self.line_height)?;
                let y_offset: f32 = self
                    .last_lines
                    .iter()
                    .take(line_ix)
                    .map(|l| f32::from(l.size(self.line_height).height))
                    .sum();
                return Some(point(local.x, local.y + px(y_offset)));
            }
        }
        None
    }

    /// Byte index closest to a content-local point.
    fn index_for_point(&self, position: Point<Pixels>) -> usize {
        if self.display_is_placeholder {
            return 0;
        }
        let mut y = f32::from(position.y);
        if y < 0.0 {
            return 0;
        }
        for (line_ix, line) in self.last_lines.iter().enumerate() {
            let height = f32::from(line.size(self.line_height).height);
            let line_start = self.line_starts.get(line_ix).copied().unwrap_or(0);
            if y < height || line_ix + 1 == self.last_lines.len() {
                let local = point(position.x, px(y.min(height - 1.0).max(0.0)));
                let ix = line
                    .closest_index_for_position(local, self.line_height)
                    .unwrap_or_else(|ix| ix);
                return (line_start + ix).min(self.content.len());
            }
            y -= height;
        }
        self.content.len()
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let Some(bounds) = self.last_bounds else {
            return 0;
        };
        let local = point(
            position.x - bounds.left(),
            position.y - bounds.top() + px(self.scroll_top),
        );
        self.index_for_point(local)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        self.is_selecting = true;
        let index = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(index, cx);
        } else {
            self.move_to(index, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    // ---- utf16 mapping (IME) ----

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    /// Shape the text at a width; store measured layout; return content height.
    /// Called from the element's measured-layout closure.
    fn layout_text(&mut self, width: Pixels, style: &TextStyle, window: &mut Window) -> f32 {
        let (display, is_placeholder) = if self.content.is_empty() {
            (self.placeholder.clone(), true)
        } else {
            (SharedString::from(self.content.clone()), false)
        };
        let font_size = style.font_size.to_pixels(window.rem_size());
        self.line_height = px(INPUT_LINE_HEIGHT);

        let run_for = |len: usize, underline: bool| TextRun {
            len,
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: underline.then_some(UnderlineStyle {
                color: Some(style.color),
                thickness: px(1.0),
                wavy: false,
            }),
            strikethrough: None,
        };
        let runs: Vec<TextRun> = match self.marked_range.as_ref() {
            Some(marked) if !is_placeholder => vec![
                run_for(marked.start, false),
                run_for(marked.len(), true),
                run_for(display.len() - marked.end, false),
            ]
            .into_iter()
            .filter(|r| r.len > 0)
            .collect(),
            _ => vec![run_for(display.len(), false)],
        };

        let lines = window
            .text_system()
            .shape_text(display, font_size, &runs, Some(width), None)
            .map(|small| small.into_vec())
            .unwrap_or_default();

        // Logical line byte offsets (each shaped line covers one \n-split line).
        let mut line_starts = Vec::with_capacity(lines.len());
        let mut at = 0usize;
        for line in &lines {
            line_starts.push(at);
            at += line.len() + 1; // + '\n'
        }
        if line_starts.is_empty() {
            line_starts.push(0);
        }

        let content_height: f32 = lines
            .iter()
            .map(|l| f32::from(l.size(self.line_height).height))
            .sum();
        let max_line_width: f32 = lines
            .iter()
            .map(|l| f32::from(l.unwrapped_layout.width))
            .fold(0.0, f32::max);

        self.display_is_placeholder = is_placeholder;
        self.last_lines = lines;
        self.line_starts = line_starts;
        self.content_height = content_height.max(INPUT_LINE_HEIGHT);
        self.max_line_width = if is_placeholder { 0.0 } else { max_line_width };
        self.last_width = f32::from(width);
        self.layout_epoch += 1;
        self.content_height
    }

    /// Keep the cursor visible when content exceeds the element height.
    fn clamp_scroll(&mut self, element_height: f32) {
        let max_scroll = (self.content_height - element_height).max(0.0);
        if let Some(cursor) = self.point_for_index(self.cursor_offset()) {
            let cursor_top = f32::from(cursor.y);
            let cursor_bottom = cursor_top + f32::from(self.line_height);
            if cursor_top < self.scroll_top {
                self.scroll_top = cursor_top;
            } else if cursor_bottom > self.scroll_top + element_height {
                self.scroll_top = cursor_bottom - element_height;
            }
        }
        self.scroll_top = self.scroll_top.clamp(0.0, max_scroll);
    }
}

impl EventEmitter<ComposerInputEvent> for ComposerInput {}

impl Focusable for ComposerInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EntityInputHandler for ComposerInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content.get(range)?.to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let edit = ComposerTextEdit {
            replaced: range.clone(),
            inserted_len: new_text.len(),
        };
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        let cursor = range.start + new_text.len();
        self.selected_range = cursor..cursor;
        self.marked_range.take();
        self.reset_blink();
        cx.emit(ComposerInputEvent::Edited(Some(edit)));
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        let edit = ComposerTextEdit {
            replaced: range.clone(),
            inserted_len: new_text.len(),
        };
        self.content =
            self.content[0..range.start].to_owned() + new_text + &self.content[range.end..];
        if new_text.is_empty() {
            self.marked_range = None;
        } else {
            self.marked_range = Some(range.start..range.start + new_text.len());
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .map(|new_range| new_range.start + range.start..new_range.end + range.start)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());
        self.reset_blink();
        cx.emit(ComposerInputEvent::Edited(Some(edit)));
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.range_from_utf16(&range_utf16);
        let start = self.point_for_index(range.start)?;
        let origin = point(
            bounds.left() + start.x,
            bounds.top() + start.y - px(self.scroll_top),
        );
        Some(Bounds::new(origin, size(px(2.0), self.line_height)))
    }

    fn character_index_for_point(
        &mut self,
        point_in_window: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let index = self.index_for_mouse_position(point_in_window);
        Some(self.offset_to_utf16(index))
    }
}

/// The custom element: measured auto-grow layout + shaped-line painting.
struct ComposerTextElement {
    input: Entity<ComposerInput>,
    /// Max content height before internal scrolling kicks in.
    max_content_height: f32,
}

struct ComposerTextPrepaint {
    cursor: Option<PaintQuad>,
    selection_quads: Vec<PaintQuad>,
}

impl IntoElement for ComposerTextElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl gpui::Element for ComposerTextElement {
    type RequestLayoutState = ();
    type PrepaintState = ComposerTextPrepaint;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        let input = self.input.clone();
        let text_style = window.text_style();
        let max_content = self.max_content_height;
        let layout_id =
            window.request_measured_layout(style, move |known, available, window, cx| {
                let width = known.width.unwrap_or(match available.width {
                    gpui::AvailableSpace::Definite(width) => width,
                    _ => px(320.0),
                });
                let content_height =
                    input.update(cx, |input, _| input.layout_text(width, &text_style, window));
                size(width, px(content_height.min(max_content)))
            });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        _window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.input.update(cx, |input, _| {
            input.clamp_scroll(f32::from(bounds.size.height));
            input.last_bounds = Some(bounds);
        });
        let input = self.input.read(cx);
        let scroll = px(input.scroll_top);
        let origin = point(bounds.left(), bounds.top() - scroll);
        let selection_color = gpui::hsla(0.66, 0.6, 0.55, 0.35);

        let mut selection_quads = Vec::new();
        let mut cursor = None;
        if input.selected_range.is_empty() || input.display_is_placeholder {
            if let Some(p) = input.point_for_index(input.cursor_offset()) {
                cursor = Some(fill(
                    Bounds::new(
                        point(origin.x + p.x, origin.y + p.y),
                        size(px(2.0), input.line_height),
                    ),
                    gpui::hsla(0.66, 0.7, 0.7, 1.0),
                ));
            } else if input.display_is_placeholder {
                cursor = Some(fill(
                    Bounds::new(origin, size(px(2.0), input.line_height)),
                    gpui::hsla(0.66, 0.7, 0.7, 1.0),
                ));
            }
        } else if let (Some(start), Some(end)) = (
            input.point_for_index(input.selected_range.start),
            input.point_for_index(input.selected_range.end),
        ) {
            let lh = input.line_height;
            if start.y == end.y {
                selection_quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x + start.x, origin.y + start.y),
                        point(origin.x + end.x, origin.y + start.y + lh),
                    ),
                    selection_color,
                ));
            } else {
                // First visual row, full middle rows, last visual row.
                selection_quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x + start.x, origin.y + start.y),
                        point(bounds.right(), origin.y + start.y + lh),
                    ),
                    selection_color,
                ));
                if end.y > start.y + lh {
                    selection_quads.push(fill(
                        Bounds::from_corners(
                            point(origin.x, origin.y + start.y + lh),
                            point(bounds.right(), origin.y + end.y),
                        ),
                        selection_color,
                    ));
                }
                selection_quads.push(fill(
                    Bounds::from_corners(
                        point(origin.x, origin.y + end.y),
                        point(origin.x + end.x, origin.y + end.y + lh),
                    ),
                    selection_color,
                ));
            }
        }
        ComposerTextPrepaint {
            cursor,
            selection_quads,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        // WrappedLine isn't Clone — temporarily take the shaped lines out of the
        // entity for painting, then put them back for mouse mapping.
        let (lines, line_height, scroll) = self.input.update(cx, |input, _| {
            (
                std::mem::take(&mut input.last_lines),
                input.line_height,
                input.scroll_top,
            )
        });

        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for quad in prepaint.selection_quads.drain(..) {
                window.paint_quad(quad);
            }
            let mut y = bounds.top() - px(scroll);
            for line in &lines {
                let height = line.size(line_height).height;
                let _ = line.paint(
                    point(bounds.left(), y),
                    line_height,
                    gpui::TextAlign::Left,
                    Some(bounds),
                    window,
                    cx,
                );
                y += height;
            }
            // Caret only when this input is actually focused in an active
            // window (Electron hides it on window deactivation too), and only
            // in the "on" blink phase — solid while typing, ~500ms blink idle.
            if self
                .input
                .update(cx, |input, cx| input.caret_shown(window, cx))
                && let Some(cursor) = prepaint.cursor.take()
            {
                window.paint_quad(cursor);
            }
        });
        self.input.update(cx, |input, _| {
            input.last_lines = lines;
        });
    }
}

impl Render for ComposerInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx);
        let text_color = if self.content.is_empty() {
            theme.text_faint
        } else {
            theme.text
        };
        div()
            .key_context(self.key_context)
            .track_focus(&self.focus_handle)
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::word_left))
            .on_action(cx.listener(Self::word_right))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::submit))
            .on_action(cx.listener(Self::queue_submit))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .text_size(px(INPUT_TEXT_SIZE))
            .line_height(px(INPUT_LINE_HEIGHT))
            .text_color(text_color)
            .font_family(theme.font_sans.clone())
            .child(ComposerTextElement {
                input: cx.entity(),
                // Internal scrolling once content exceeds the 260px textarea
                // box minus its `pt-4 pb-1` padding.
                max_content_height: TEXTAREA_MAX - TEXTAREA_PAD_V,
            })
    }
}

// ---------------------------------------------------------------------------
// Composer wrapper
// ---------------------------------------------------------------------------

/// Events the shell listens for.
#[derive(Debug, Clone)]
pub enum ComposerEvent {
    /// A prompt was sent (optimistically) — re-engage the transcript pin.
    Sent { chat_id: String },
}

pub struct Composer {
    state: Entity<AppState>,
    input: Entity<ComposerInput>,
    /// Composer actions row: repo/branch/harness-model/traits (§1.7).
    pickers: Entity<Pickers>,
    /// Unsent text per draft key ([`draft_key`]) — the composer's contents are
    /// a per-chat draft, not per-view transient state, so every navigation
    /// files the current text here and unfiles the destination's (gh#536).
    /// Persisted device-locally: a draft outlives a quit, and never syncs.
    drafts: DraftStore,
    /// Where [`Self::drafts`] is written; `None` in tests that don't care.
    data_dir: Option<PathBuf>,
    /// In-flight debounced draft write.
    draft_save_task: Option<Task<()>>,
    /// Staged-but-unsent attachments per chat key (use-attachments.ts `stash`):
    /// navigating away and back restores them; memory-only, like the original.
    attachments: HashMap<String, Vec<StagedAttachment>>,
    /// The staged attachment being viewed full-size (click a thumbnail).
    preview: Option<attachments::PreviewImage>,
    /// In-flight file-picker prompt (paperclip).
    picker_task: Option<Task<()>>,
    /// What the composer is currently filed under — see [`draft_key`].
    current_key: String,
    /// Skills invocable by a run started from here (gh#134), as the chat's HOST
    /// answered — never this device's own `~/.claude`, which for a chat on the
    /// box, or one naming an agent account, is a different list entirely.
    skills: Vec<SkillDescriptor>,
    /// Which composer key `skills` describes; `None` until the first answer.
    skills_key: Option<String>,
    skills_task: Option<Task<()>>,
    /// Highlighted row of the `/` picker, and the first row drawn — kept
    /// across keystrokes so arrowing back up retraces the same rows.
    skill_selected: usize,
    skill_first: usize,
    /// Offset of a `/` token Escape closed the picker on; see
    /// [`Composer::dismiss_skill_menu`].
    skill_dismissed: Option<usize>,
    // ---- the `@` context picker (gh#424) ----
    /// Rows the HOST answered for the current query — paths in the chat's own
    /// checkout, never this device's disk.
    context_rows: Vec<ContextMatch>,
    /// Did the host's walk hit its budget? The menu says so rather than
    /// presenting a capped list as the whole tree.
    context_truncated: bool,
    context_checkout_id: Option<String>,
    /// Checkout stamps captured when a picker row was completed, per draft.
    /// Search chrome may close or refresh; provenance belongs to the inserted
    /// reference and must survive until that reference/draft is removed.
    context_ref_checkouts: HashMap<String, Vec<ContextSelection>>,
    /// `(chat key, query)` the rows answer. Both halves matter: the same query
    /// means different files in a different checkout.
    context_key: Option<(String, String)>,
    context_task: Option<Task<()>>,
    context_selected: usize,
    context_first: usize,
    context_dismissed: Option<usize>,
    // ---- the follow-up queue (gh#424) ----
    /// The plan for the selected chat, as the host folds it out of the ledger.
    queue: QueueView,
    /// Which chat `queue` is about (a stale pump must not paint another chat's
    /// plan under this one's composer).
    queue_chat: Option<String>,
    queue_task: Option<Task<()>>,
    queue_action_task: Option<Task<()>>,
    /// Client-owned ids retained while an append's acknowledgement is unknown.
    /// The serialized intent is the key: editing it creates a new gesture;
    /// retrying it after response loss reuses the old id.
    pending_queue_gestures: HashMap<String, String>,
    pending_queue_message_ids: HashMap<String, String>,
    /// The queued row the composer is editing, if any. Sending commits an edit
    /// against this id instead of queueing a new row.
    editing: Option<String>,
    /// What the composer held before a queued-row edit borrowed it — the
    /// draft and its reference provenance, handed back when the edit ends
    /// (gh#536). Editing a follow-up is neither sending nor deleting, so it is
    /// not one of the two things allowed to clear a prompt in progress.
    edit_stash: Option<(crate::drafts::Draft, Option<Vec<ContextSelection>>)>,
    sending: bool,
    failure: Option<SharedString>,
    wizard: Option<Wizard>,
    wizard_focus: FocusHandle,
    /// Requests already answered locally (suppresses the panel until the doc
    /// frame marks them resolved).
    answered_requests: HashSet<String>,
    advance_task: Option<Task<()>>,
    send_task: Option<Task<()>>,
    // -- compact/expanded flip state (hysteresis; see `composer_flip`) --
    /// Current layout mode (persisted across frames — never derived fresh).
    expanded_mode: bool,
    /// `layout_epoch` of the measurement that caused the last flip: the flip is
    /// re-evaluated only after the input has been laid out in the new mode, so
    /// at most one flip can happen per layout pass.
    flip_epoch: u64,
    /// Compact-mode input capacity, learned while compact (layout-stable).
    compact_capacity: f32,
    /// Input width first measured after expanding — container-width deltas
    /// while expanded shift `compact_capacity` by the same amount.
    expanded_anchor: f32,
    /// Last input width seen in the current mode (resize detection).
    last_seen_width: f32,
    /// Set while an interactive resize is in flight; mode is frozen until
    /// widths have settled for [`RESIZE_SETTLE_MS`].
    width_changed_at: Option<Instant>,
    settle_task: Option<Task<()>>,
    /// In-flight compact↔expanded morph (one per committed flip; manual
    /// drive — see [`FlipMorph`]).
    flip_morph: Option<FlipMorph>,
    /// Pill height actually rendered last frame — a committed flip morphs
    /// from here, so mid-flight reversals hand off without a jump.
    last_rendered_height: f32,
    /// Monotonic clock anchor for the morph timeline.
    morph_clock: Instant,
    /// Set on every session/route change: flips committed before this instant
    /// SNAP instead of morphing (see [`ROUTE_SNAP_MS`]).
    route_snap_until: Option<Instant>,
    _observe: Subscription,
    _pickers_observe: Subscription,
    _input_events: Subscription,
    /// Flush the drafts on the way out — the debounced write is a task, and a
    /// task does not survive the app it was scheduled in (gh#536).
    _quit: Subscription,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    /// `data_dir` is where drafts are persisted (gh#536) — `None` keeps them
    /// in memory only, which is what a test that never quits wants.
    pub fn new(state: Entity<AppState>, data_dir: Option<PathBuf>, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| ComposerInput::new("Do anything…", cx));
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        // The footer toolbar (checkout kind + ref picker) is rendered INLINE
        // by the composer from picker state — a pickers-side notify (refs
        // loaded, popover toggled, pick made) must repaint the composer too.
        let pickers_observe = cx.observe(&pickers, |_, _, cx| cx.notify());
        let observe = cx.observe(&state, |this: &mut Self, _, cx| this.on_state_changed(cx));
        let input_events = cx.subscribe(&input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.on_submit(false, cx),
            ComposerInputEvent::QueueSubmitted => this.on_submit(true, cx),
            // Recomputed on every edit rather than only on render: the input
            // asks `menu_active` while dispatching the NEXT keystroke, so a
            // flag that lagged a frame would send the prompt on the Enter that
            // was meant to complete it.
            ComposerInputEvent::Edited(edit) => {
                if let Some(edit) = edit {
                    this.apply_context_edit(edit);
                }
                // Every keystroke updates the chat's draft. Filing it here
                // rather than only on navigation is what makes a draft survive
                // the ways of leaving that are not navigations at all — ⌘W,
                // ⌘Q, a crash (gh#536).
                this.record_draft(cx);
                this.sync_menus(cx);
                cx.notify();
            }
            // Whichever menu is open owns the keys. They cannot both be: a `/`
            // token stops at a slash and an `@` token starts at whitespace, so
            // the caret is in at most one of them.
            ComposerInputEvent::Menu(nav) => match (this.context_menu(cx).is_some(), nav) {
                (true, MenuNav::Up) => this.step_context(-1, cx),
                (true, MenuNav::Down) => this.step_context(1, cx),
                (true, MenuNav::Accept) => this.accept_context(cx),
                (false, MenuNav::Up) => this.step_skill(-1, cx),
                (false, MenuNav::Down) => this.step_skill(1, cx),
                (false, MenuNav::Accept) => this.accept_skill(cx),
            },
            ComposerInputEvent::PastedImages(images) => {
                let staged = images
                    .iter()
                    .map(|image| attachments::stage_clipboard_image(image.clone()))
                    .collect();
                this.add_staged(staged, cx);
            }
            ComposerInputEvent::PastedPaths(paths) => this.add_paths(paths.clone(), cx),
        });
        let current_key = {
            let s = state.read(cx);
            draft_key(s.selected_chat.as_deref(), s.selected_space.as_deref())
        };
        // Drafts come off disk before the first paint, so the composer opens
        // holding whatever was left in it — including across a full quit.
        let drafts = data_dir
            .as_deref()
            .map(DraftStore::load)
            .unwrap_or_default();
        let restored = drafts.get(&current_key).cloned();
        // ⌘Q inside the debounce window is still a quit with typing in it.
        let quit = cx.on_app_quit(|composer: &mut Self, cx| {
            composer.flush_drafts(cx);
            async {}
        });
        let mut composer = Self {
            state,
            input,
            pickers,
            drafts,
            data_dir,
            draft_save_task: None,
            attachments: HashMap::new(),
            preview: None,
            picker_task: None,
            current_key,
            skills: Vec::new(),
            skills_key: None,
            skills_task: None,
            skill_selected: 0,
            skill_first: 0,
            skill_dismissed: None,
            context_rows: Vec::new(),
            context_truncated: false,
            context_checkout_id: None,
            context_ref_checkouts: HashMap::new(),
            context_key: None,
            context_task: None,
            context_selected: 0,
            context_first: 0,
            context_dismissed: None,
            queue: QueueView::default(),
            queue_chat: None,
            queue_task: None,
            queue_action_task: None,
            pending_queue_gestures: HashMap::new(),
            pending_queue_message_ids: HashMap::new(),
            editing: None,
            edit_stash: None,
            sending: false,
            failure: None,
            wizard: None,
            wizard_focus: cx.focus_handle(),
            answered_requests: HashSet::new(),
            advance_task: None,
            send_task: None,
            expanded_mode: false,
            flip_epoch: 0,
            compact_capacity: 0.0,
            expanded_anchor: 0.0,
            last_seen_width: 0.0,
            width_changed_at: None,
            settle_task: None,
            flip_morph: None,
            last_rendered_height: 0.0,
            morph_clock: Instant::now(),
            route_snap_until: None,
            _observe: observe,
            _pickers_observe: pickers_observe,
            _input_events: input_events,
            _quit: quit,
        };
        if let Some(draft) = restored {
            composer.input.update(cx, |input, cx| {
                input.set_text_with_caret(draft.text, draft.caret, cx)
            });
        }
        // Dev knob: pre-stage attachments (drop/paste can't be synthesized on
        // a rig) — `COMET_ATTACH=/path/a.png[,/path/b.png]`, and
        // `COMET_ATTACH_PREVIEW=1` boots with the first one's lightbox open.
        if let Ok(spec) = std::env::var("COMET_ATTACH") {
            let staged: Vec<StagedAttachment> = spec
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .filter_map(|path| {
                    match attachments::stage_file(std::path::Path::new(path.trim())) {
                        Ok(att) => Some(att),
                        Err(err) => {
                            tracing::warn!(%path, error = %err, "COMET_ATTACH stage failed");
                            None
                        }
                    }
                })
                .collect();
            if std::env::var("COMET_ATTACH_PREVIEW").is_ok_and(|v| v == "1")
                && let Some(first) = staged.first()
            {
                composer.preview = Some(attachments::PreviewImage {
                    name: first.name.clone().into(),
                    image: first.image.clone(),
                });
            }
            if !staged.is_empty() {
                composer
                    .attachments
                    .entry(composer.current_key.clone())
                    .or_default()
                    .extend(staged);
            }
        }
        composer
    }

    #[cfg(test)]
    pub(crate) fn send_for_interaction_test(
        &mut self,
        text: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.send(text.into(), false, cx);
    }

    /// Capture-knob passthrough (`COMET_OPEN_DIALOG=model`): open the
    /// combined harness/model menu.
    pub fn debug_open_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pickers
            .update(cx, |pickers, cx| pickers.open_model_menu(window, cx));
    }

    pub fn is_sending(&self) -> bool {
        self.sending
    }

    // ---- attachment staging (use-attachments.ts) ----

    /// Staged attachments for the chat the composer is showing.
    fn staged(&self) -> &[StagedAttachment] {
        self.attachments
            .get(&self.current_key)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    fn add_staged(&mut self, staged: Vec<StagedAttachment>, cx: &mut Context<Self>) {
        if staged.is_empty() {
            return;
        }
        self.attachments
            .entry(self.current_key.clone())
            .or_default()
            .extend(staged);
        cx.notify();
    }

    /// Stage image files (picker / drop / pasted paths). Non-images are
    /// skipped silently (matching the original's `image/*` filter); read
    /// failures and oversize files surface in the failure notice.
    pub(crate) fn add_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut staged = Vec::new();
        for path in &paths {
            if attachments::format_by_extension(path).is_none() {
                continue;
            }
            match attachments::stage_file(path) {
                Ok(att) => staged.push(att),
                Err(message) => {
                    self.failure = Some(message.into());
                    cx.notify();
                }
            }
        }
        self.add_staged(staged, cx);
    }

    fn remove_attachment(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(list) = self.attachments.get_mut(&self.current_key) {
            list.retain(|a| a.id != id);
            if list.is_empty() {
                self.attachments.remove(&self.current_key);
            }
        }
        cx.notify();
    }

    /// Drop a deleted chat's per-chat composer state — staged attachments hold
    /// raw image bytes, and a deleted chat's stage could never be sent again.
    ///
    /// Deleting the chat IS the user deleting the draft: one of the exactly
    /// two things allowed to clear one (gh#536).
    pub fn purge_chat(&mut self, chat_id: &str, cx: &mut Context<Self>) {
        self.attachments.remove(chat_id);
        if self.drafts.remove(chat_id) {
            self.schedule_draft_save(cx);
        }
        // The shell deselects a deleted chat before it purges, so the input is
        // normally already showing something else. If it is not, the text on
        // screen belongs to a chat that no longer exists — leaving it would
        // re-file it under the next key the composer takes.
        if self.current_key == chat_id {
            self.input.update(cx, |input, cx| input.set_text("", cx));
        }
    }

    // ---- drafts (gh#536) ----

    /// File the input's current text under the current key, and schedule the
    /// write. Called on every edit and before every navigation, so the store
    /// is never behind what is on screen by more than a keystroke.
    fn record_draft(&mut self, cx: &mut Context<Self>) {
        // The wizard's free-text override and a queued-row edit both borrow
        // the same input for text that is not this chat's prompt; neither may
        // overwrite the draft the user had going. Both hand it back when they
        // are done.
        if self.wizard.is_some() || self.editing.is_some() {
            return;
        }
        let (text, caret) = {
            let input = self.input.read(cx);
            (input.text().to_string(), input.caret())
        };
        let key = self.current_key.clone();
        if self
            .drafts
            .set(&key, &text, caret, chrono::Utc::now().timestamp_millis())
        {
            self.schedule_draft_save(cx);
        }
    }

    /// Hand back the draft a queued-row edit borrowed the composer from. With
    /// nothing stashed this is the plain clear the edit paths used to do.
    fn restore_stashed_draft(&mut self, cx: &mut Context<Self>) {
        let Some((draft, provenance)) = self.edit_stash.take() else {
            self.clear_input_and_provenance(cx);
            return;
        };
        // The edit's own reference provenance goes with the edit.
        self.context_ref_checkouts.remove(&self.current_key);
        if let Some(provenance) = provenance {
            self.context_ref_checkouts
                .insert(self.current_key.clone(), provenance);
        }
        let caret = draft.caret;
        self.input.update(cx, |input, cx| {
            input.set_text_with_caret(draft.text, caret, cx)
        });
    }

    /// Debounced write of the whole store (it is a few kilobytes of text; the
    /// debounce is about not touching the disk on every keystroke, not about
    /// the cost of the write itself). Re-scheduling cancels the pending timer.
    fn schedule_draft_save(&mut self, cx: &mut Context<Self>) {
        let Some(dir) = self.data_dir.clone() else {
            return;
        };
        self.draft_save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(crate::drafts::SAVE_DEBOUNCE_MS))
                .await;
            let Ok(snapshot) = this.update(cx, |composer, _| composer.drafts.clone()) else {
                return;
            };
            cx.background_executor()
                .spawn(async move {
                    if let Err(err) = snapshot.save(&dir) {
                        tracing::warn!(error = %err, "failed to persist composer drafts");
                    }
                })
                .await;
        }));
    }

    /// Write the drafts NOW, on the calling thread. The debounce above is a
    /// task, and a task does not outlive the app that scheduled it: ⌘Q inside
    /// the debounce window would otherwise drop the last thing typed.
    pub fn flush_drafts(&mut self, cx: &mut Context<Self>) {
        self.record_draft(cx);
        self.draft_save_task = None;
        let Some(dir) = self.data_dir.as_deref() else {
            return;
        };
        if let Err(err) = self.drafts.save(dir) {
            tracing::warn!(error = %err, "failed to persist composer drafts");
        }
    }

    /// The staged-thumbnail strip (attachment-ui.tsx AttachmentStrip):
    /// `flex flex-wrap gap-2 px-4 pt-3`, 56px rounded thumbs, a remove button
    /// revealed on hover, click opens the full-size preview.
    fn render_attachment_strip(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let staged = self.staged();
        if staged.is_empty() {
            return None;
        }
        let mut strip = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(STRIP_GAP))
            .px(px(STRIP_PAD_X))
            .pt(px(STRIP_PAD_TOP));
        for (ix, att) in staged.iter().enumerate() {
            let group: SharedString = format!("composer-att-{}", att.id).into();
            let preview = attachments::PreviewImage {
                name: att.name.clone().into(),
                image: att.image.clone(),
            };
            let remove_id = att.id.clone();
            strip = strip.child(
                div()
                    .group(group.clone())
                    .relative()
                    .child(
                        div()
                            .id(("composer-att-thumb", ix))
                            .size(px(STRIP_THUMB))
                            .rounded(px(Theme::RADIUS_ROW))
                            .overflow_hidden()
                            .border_1()
                            .border_color(theme.white_alpha(0.10))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.preview = Some(preview.clone());
                                cx.notify();
                            }))
                            .child(
                                img(att.image.clone())
                                    .size_full()
                                    .object_fit(ObjectFit::Cover),
                            ),
                    )
                    .child(
                        div()
                            .id(("composer-att-remove", ix))
                            .absolute()
                            .top(px(-6.0))
                            .right(px(-6.0))
                            .size(px(18.0))
                            .rounded(px(Theme::RADIUS_CHIP))
                            .bg(theme.bg)
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .shadow_sm()
                            .opacity(0.0)
                            .group_hover(group, |s| s.opacity(1.0))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_attachment(&remove_id, cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::CLOSE_CIRCLE)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            ),
                    ),
            );
        }
        Some(strip)
    }

    /// Paperclip: the native image picker (the original's hidden
    /// `<input type=file accept=image/* multiple>`).
    fn open_file_picker(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        self.picker_task = Some(cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update(cx, |composer, cx| composer.add_paths(paths, cx))
                    .ok();
            }
        }));
    }

    fn on_state_changed(&mut self, cx: &mut Context<Self>) {
        let (key, pending) = {
            let s = self.state.read(cx);
            (
                draft_key(s.selected_chat.as_deref(), s.selected_space.as_deref()),
                pending_input_request(&s.transcript),
            )
        };

        // Draft swap on chat navigation — the input entity itself survives.
        if key != self.current_key {
            // A queued-row edit belongs to the chat whose plan the row is in:
            // leaving ends it, handing that chat's own draft back before the
            // swap files it. (Carried across, the next Enter would have
            // committed an edit against another chat's queue.)
            self.stop_editing(cx);
            self.record_draft(cx);
            let draft = self.drafts.get(&key).cloned().unwrap_or_default();
            self.current_key = key;
            self.failure = None;
            self.wizard = None;
            // Attachments stay stashed under their chat key (the map swap IS
            // the navigation); only the transient chrome resets.
            self.preview = None;
            // Route changes snap (round 5/6): a mode difference between the
            // old and new session's composer must not glide across
            // navigation. Killing the in-flight morph here isn't enough —
            // the nav-driven flip only commits AFTER the swapped draft has
            // been re-measured, one or two renders later, so the whole
            // window snaps (see ROUTE_SNAP_MS).
            self.flip_morph = None;
            self.last_rendered_height = 0.0;
            self.route_snap_until = Some(Instant::now() + Duration::from_millis(ROUTE_SNAP_MS));
            // Text AND caret: a draft comes back where it was left, not with
            // the caret dumped at its end.
            self.input.update(cx, |input, cx| {
                input.set_text_with_caret(draft.text, draft.caret, cx)
            });
        }

        // Question panel lifecycle (wizard state cached per request id).
        match pending {
            Some((request_id, questions)) if !self.answered_requests.contains(&request_id) => {
                let same = self
                    .wizard
                    .as_ref()
                    .is_some_and(|w| w.request_id == request_id);
                if !same {
                    self.wizard = Some(Wizard::new(request_id, questions));
                    self.advance_task = None;
                    // The shared input becomes the panel's free-text override.
                    self.input.update(cx, |input, cx| {
                        input.set_placeholder("Type your own answer, or pick an option above", cx)
                    });
                }
            }
            _ => {
                if let Some(wizard) = self.wizard.as_ref() {
                    // LATCH (original composer.tsx `inputLatch`): a transient
                    // fold/sync blip — or a steer appended behind the
                    // streaming entry — must not unmount the panel and lose
                    // the user's picks. Release only on explicit resolution
                    // (here or on another device) or when a NON-EMPTY
                    // transcript shows the question superseded (a newer
                    // assistant entry took over). Never on run death: the
                    // question stays answerable until answered — the engine
                    // delivers a dead run's answer as a resumed turn.
                    let transcript = self.state.read(cx).transcript.clone();
                    let released = input_request_resolved(&transcript, &wizard.request_id)
                        || (!transcript.is_empty()
                            && !self.answered_requests.contains(&wizard.request_id));
                    if released {
                        self.wizard = None;
                        self.advance_task = None;
                        self.input
                            .update(cx, |input, cx| input.set_placeholder("Do anything…", cx));
                    }
                }
            }
        }
        self.ensure_skills(cx);
        self.ensure_queue_watch(cx);
        cx.notify();
    }

    // ---- the `/` skill picker (gh#134) ----

    /// Load the skills invocable from this composer, once per chat.
    ///
    /// Prefetched on navigation rather than fetched when `/` is first typed:
    /// the round trip may cross the relay to the box, and a picker that
    /// appeared a beat after the slash — or worse, after the name was already
    /// typed — would train people not to wait for it.
    fn ensure_skills(&mut self, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let chat = state.selected_chat_row().cloned();
        // The chat's host answers about the chat; the new-chat canvas has only
        // the space, so it asks about the space's folder on the space's device.
        let (chat_id, cwd, device) = match &chat {
            Some(chat) => (Some(chat.id.clone()), None, Some(chat.device_id.clone())),
            None => match state.selected_space_row() {
                Some(space) => (
                    None,
                    Some(space.path.clone()),
                    Some(space.device_id.clone()),
                ),
                None => (None, None, None),
            },
        };
        // Keyed on what the answer is ABOUT, not on the composer's draft key:
        // on the new-chat canvas the draft key is `""` for every space, and a
        // key that could not tell two spaces apart would keep offering the
        // first one's repo-level skills after you switched.
        let key = match (&chat_id, &cwd) {
            (Some(chat), _) => format!("chat:{chat}"),
            (None, Some(cwd)) => format!("cwd:{cwd}"),
            // Nothing to ask about yet. Left unclaimed so the first space or
            // chat to arrive triggers the ask.
            (None, None) => return,
        };
        if self.skills_key.as_deref() == Some(key.as_str()) {
            return;
        }
        let Some(engine) = state.engine().cloned() else {
            return;
        };
        let target = device.filter(|d| state.local_device_id.as_deref() != Some(d.as_str()));
        let harness = self.pickers.read(cx).harness(cx);
        // Claim the key before the call so a re-entrant render cannot queue a
        // second load for the same chat.
        self.skills_key = Some(key.clone());
        self.skills = Vec::new();
        self.skill_selected = 0;
        self.skill_first = 0;
        self.skill_dismissed = None;

        self.skills_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            let mut put = |k: &str, v: Option<String>| {
                if let Some(v) = v {
                    params.insert(k.into(), serde_json::Value::String(v));
                }
            };
            put("chatId", chat_id);
            put("cwd", cwd);
            put("targetDeviceId", target);
            if let Some(harness) = harness {
                params.insert(
                    "harness".into(),
                    serde_json::to_value(harness).unwrap_or(serde_json::Value::Null),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_SKILLS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |composer, cx| {
                // A reply for a chat the composer has since navigated away
                // from is stale — dropping it is what keeps the menu about the
                // chat you are looking at.
                if composer.skills_key.as_deref() != Some(key.as_str()) {
                    return;
                }
                match result {
                    Ok(value) => match serde_json::from_value::<Vec<SkillDescriptor>>(value) {
                        Ok(skills) => composer.skills = skills,
                        // Never surfaced: an empty picker and a picker that
                        // could not load look the same to somebody typing, and
                        // a notice under the composer for it would be noise on
                        // a device with no skills installed at all.
                        Err(err) => tracing::warn!(error = %err, "ListSkills decode failed"),
                    },
                    Err(err) => tracing::warn!(error = %err, "ListSkills failed"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The `/…` token under the caret and the rows matching it — `None`
    /// whenever the picker should be closed, including when nothing matches
    /// (an empty menu hanging under a composer says nothing and eats Enter).
    fn skill_menu(&self, cx: &App) -> Option<(view_skills::SlashToken, Vec<SkillDescriptor>)> {
        if self.wizard.is_some() || self.skills.is_empty() {
            return None;
        }
        let input = self.input.read(cx);
        let token = view_skills::slash_token(input.text(), input.caret())?;
        if self.skill_dismissed == Some(token.start) {
            return None;
        }
        let rows = view_skills::filter(&token.query, &self.skills);
        (!rows.is_empty()).then_some((token, rows))
    }

    /// Re-derive both pickers' open state after an edit: clamp the selections to
    /// the new row sets, fetch context rows for a changed `@` query, and tell
    /// the input whose keys the arrows are.
    fn sync_menus(&mut self, cx: &mut Context<Self>) {
        // Forget a dismissal once the caret has left the token it was about,
        // so the next `/` — even one typed at the same offset after clearing
        // the line — opens a menu again.
        if self.skill_dismissed.is_some() {
            let input = self.input.read(cx);
            let inside = view_skills::slash_token(input.text(), input.caret())
                .is_some_and(|t| Some(t.start) == self.skill_dismissed);
            if !inside {
                self.skill_dismissed = None;
            }
        }
        if self.context_dismissed.is_some() {
            let input = self.input.read(cx);
            let inside = view_context::at_token(input.text(), input.caret())
                .is_some_and(|t| Some(t.start) == self.context_dismissed);
            if !inside {
                self.context_dismissed = None;
            }
        }
        self.ensure_context_rows(cx);
        let skills = self.skill_menu(cx).map_or(0, |(_, rows)| rows.len());
        if self.skill_selected >= skills {
            self.skill_selected = 0;
            self.skill_first = 0;
        }
        let context = self.context_menu(cx).map_or(0, |(_, rows)| rows.len());
        if self.context_selected >= context {
            self.context_selected = 0;
            self.context_first = 0;
        }
        self.input
            .update(cx, |input, _| input.set_menu_active(skills + context > 0));
    }

    // ---- the `@` context picker (gh#424) ----

    /// Ask the chat's HOST for paths matching the `@` token under the caret.
    ///
    /// Per query rather than once per chat (which is what the `/` picker does):
    /// a skill list is tens of rows and fixed, a checkout is a tree that the
    /// agent itself is editing, and shipping it to the composer to filter
    /// locally would mean shipping a repo listing over a relay to a phone. The
    /// host walks it under a budget and answers with the top rows.
    ///
    /// Keyed on `(chat, query)`: a reply for a query the caret has moved past —
    /// or for the chat you have since navigated away from — is dropped, which is
    /// what keeps the menu about what is on screen.
    fn ensure_context_rows(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.at_token(cx) else {
            // Do not retain a filesystem answer after the token closes. The
            // agent may create, delete, or rename the same path before the
            // user types the same query again.
            self.context_key = None;
            self.context_checkout_id = None;
            self.context_rows.clear();
            self.context_task.take();
            return;
        };
        let state = self.state.read(cx);
        let chat = state.selected_chat_row().cloned();
        let (chat_id, space_id, device, scope) = match &chat {
            Some(chat) => (
                Some(chat.id.clone()),
                None,
                Some(chat.device_id.clone()),
                format!(
                    "chat:{}:{}:{}",
                    chat.id,
                    chat.cwd.as_deref().unwrap_or(""),
                    chat.checkout_id.as_deref().unwrap_or("")
                ),
            ),
            None => match state.selected_space_row() {
                Some(space) => (
                    None,
                    Some(space.id.clone()),
                    Some(space.device_id.clone()),
                    format!(
                        "space:{}:{}:{}",
                        space.id,
                        space.path,
                        space.checkout_id.as_deref().unwrap_or("")
                    ),
                ),
                None => return,
            },
        };
        let key = (scope, token.query.clone());
        if self.context_key.as_ref() == Some(&key) {
            return;
        }
        let Some(engine) = state.engine().cloned() else {
            return;
        };
        let target = device.filter(|d| state.local_device_id.as_deref() != Some(d.as_str()));
        // Claim the key before the call so a re-entrant render cannot queue a
        // second search for the same keystroke.
        self.context_key = Some(key.clone());
        let query = token.query.clone();

        self.context_task = Some(cx.spawn(async move |this, cx| {
            // A keystroke's worth of grace before crossing the relay: typing
            // `@crates/ui/src/comp` would otherwise be nineteen searches of a
            // box's disk, eighteen of whose answers are discarded on arrival.
            cx.background_executor()
                .timer(Duration::from_millis(CONTEXT_SEARCH_DEBOUNCE_MS))
                .await;
            let alive = this.update(cx, |composer, _| {
                composer.context_key.as_ref() == Some(&key)
            });
            if !matches!(alive, Ok(true)) {
                return;
            }
            let mut params = serde_json::Map::new();
            let mut put = |k: &str, v: Option<String>| {
                if let Some(v) = v {
                    params.insert(k.into(), serde_json::Value::String(v));
                }
            };
            put("chatId", chat_id);
            put("spaceId", space_id);
            put("targetDeviceId", target);
            params.insert("query".into(), serde_json::Value::String(query));
            let result = engine
                .client()
                .call(
                    methods::SEARCH_CONTEXT_FILES,
                    serde_json::Value::Object(params),
                )
                .await;
            this.update(cx, |composer, cx| {
                if composer.context_key.as_ref() != Some(&key) {
                    return; // the caret moved on; this answer is about the past
                }
                match result {
                    Ok(value) => match serde_json::from_value::<ContextSearch>(value) {
                        Ok(found) => {
                            composer.context_rows = found.matches;
                            composer.context_truncated = found.truncated;
                            composer.context_checkout_id = found.checkout_id;
                        }
                        // Never surfaced, for the same reason the skill picker
                        // stays quiet: an empty menu and a menu that could not
                        // load look identical to somebody typing, and a red
                        // notice under the composer for it would be noise.
                        Err(err) => {
                            tracing::warn!(error = %err, "SearchContextFiles decode failed")
                        }
                    },
                    Err(err) => tracing::warn!(error = %err, "SearchContextFiles failed"),
                }
                composer.context_selected = 0;
                composer.context_first = 0;
                composer.sync_menus(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// The `@…` token under the caret, if the picker should be looking at one.
    fn at_token(&self, cx: &App) -> Option<view_context::AtToken> {
        if self.wizard.is_some() {
            return None;
        }
        let input = self.input.read(cx);
        let token = view_context::at_token(input.text(), input.caret())?;
        (self.context_dismissed != Some(token.start)).then_some(token)
    }

    /// The token and the rows matching it — `None` whenever the menu should be
    /// closed, including when nothing matches.
    fn context_menu(&self, cx: &App) -> Option<(view_context::AtToken, Vec<ContextMatch>)> {
        let token = self.at_token(cx)?;
        // Filtered again locally against the host's rows: the host answered a
        // query that may be one keystroke stale, and the same pure ranking
        // (`view::context::filter`) narrows it without waiting for a round trip.
        let rows = view_context::filter(&token.query, &self.context_rows);
        (!rows.is_empty()).then_some((token, rows))
    }

    fn step_context(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some((_, rows)) = self.context_menu(cx) else {
            return;
        };
        self.context_selected = view_skills::step(self.context_selected, delta, rows.len());
        let (first, _) = view_skills::window(
            self.context_first,
            self.context_selected,
            rows.len(),
            view_skills::PICKER_MAX_ROWS,
        );
        self.context_first = first;
        cx.notify();
    }

    /// Complete the highlighted row into the buffer as a reference token.
    fn accept_context(&mut self, cx: &mut Context<Self>) {
        let Some((token, rows)) = self.context_menu(cx) else {
            return;
        };
        let Some(row) = rows.get(self.context_selected) else {
            return;
        };
        let reference = row.to_ref();
        let inserted = reference.token();
        let (text, caret) = view_context::complete(self.input.read(cx).text(), &token, &reference);
        self.apply_context_edit(&ComposerTextEdit {
            replaced: token.start..token.end,
            inserted_len: inserted.len() + usize::from(reference.kind == ContextRefKind::File),
        });
        if let Some(checkout_id) = self.context_checkout_id.clone() {
            self.context_ref_checkouts
                .entry(self.current_key.clone())
                .or_default()
                .push(ContextSelection {
                    path: row.path.clone(),
                    kind: row.kind,
                    checkout_id,
                    token_start: token.start,
                    token_end: token.start + inserted.len(),
                });
        }
        self.input
            .update(cx, |input, cx| input.set_text_with_caret(text, caret, cx));
        self.context_selected = 0;
        self.context_first = 0;
        self.sync_menus(cx);
        cx.notify();
    }

    /// Escape closes the `@` picker and leaves the text alone (same contract as
    /// the `/` one, keyed on the dismissed token's offset).
    fn dismiss_context_menu(&mut self, cx: &mut Context<Self>) {
        let input = self.input.read(cx);
        let Some(token) = view_context::at_token(input.text(), input.caret()) else {
            return;
        };
        self.context_dismissed = Some(token.start);
        self.sync_menus(cx);
        cx.notify();
    }

    fn step_skill(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some((_, rows)) = self.skill_menu(cx) else {
            return;
        };
        self.skill_selected = view_skills::step(self.skill_selected, delta, rows.len());
        let (first, _) = view_skills::window(
            self.skill_first,
            self.skill_selected,
            rows.len(),
            view_skills::PICKER_MAX_ROWS,
        );
        self.skill_first = first;
        cx.notify();
    }

    /// Complete the highlighted row into the buffer.
    fn accept_skill(&mut self, cx: &mut Context<Self>) {
        let Some((token, rows)) = self.skill_menu(cx) else {
            return;
        };
        let Some(skill) = rows.get(self.skill_selected) else {
            return;
        };
        let (text, caret) = view_skills::complete(self.input.read(cx).text(), &token, &skill.name);
        self.apply_context_edit(&ComposerTextEdit {
            replaced: token.start..token.end,
            inserted_len: 1 + skill.name.len() + 1,
        });
        self.input
            .update(cx, |input, cx| input.set_text_with_caret(text, caret, cx));
        self.skill_selected = 0;
        self.skill_first = 0;
        // The completion left a trailing space, so the token is gone and the
        // menu closes — but say so explicitly rather than relying on the
        // `Edited` event's ordering with the next keystroke.
        self.sync_menus(cx);
        cx.notify();
    }

    /// Escape closes the picker and leaves the text alone — somebody who typed
    /// `/tmp/x` and does not want a menu about it must not have their prompt
    /// rewritten to make the menu go away.
    ///
    /// Remembered as the dismissed token's offset rather than a bare flag: the
    /// menu is derived from the buffer, so a flag would have to be cleared from
    /// every edit path to avoid disagreeing with it. Keyed this way, typing on
    /// inside the same token keeps it shut and the next `/` elsewhere opens
    /// afresh, with no bookkeeping anywhere else.
    fn dismiss_skill_menu(&mut self, cx: &mut Context<Self>) {
        let input = self.input.read(cx);
        let Some(token) = view_skills::slash_token(input.text(), input.caret()) else {
            return;
        };
        self.skill_dismissed = Some(token.start);
        self.sync_menus(cx);
        cx.notify();
    }

    fn run_live(&self, cx: &App) -> bool {
        let s = self.state.read(cx);
        let Some(chat_id) = s.selected_chat.as_deref() else {
            return false;
        };
        matches!(
            s.indicator_for(chat_id, chrono::Utc::now()),
            Indicator::Working | Indicator::AwaitingInput
        )
    }

    fn button_mode(&self, cx: &App) -> SendButtonMode {
        // A staged image counts as content: image-only sends are legal
        // (the prompt body becomes "See the attached image(s).").
        let has_text = !self.input.read(cx).text().trim().is_empty() || !self.staged().is_empty();
        send_button_mode(self.run_live(cx), has_text)
    }

    fn on_submit(&mut self, queue_modifier: bool, cx: &mut Context<Self>) {
        if self.wizard.is_some() {
            // Enter inside the panel's free-text input submits the page.
            let typed = self.input.read(cx).text().trim().to_string();
            if let Some(w) = self.wizard.as_mut() {
                w.set_typed(typed);
            }
            self.wizard_advance(cx);
            return;
        }
        let text = self.input.read(cx).text().trim().to_string();
        // Editing a queued row is its own submit: the text goes back to the row
        // it came from, not into the conversation.
        if self.editing.is_some() {
            if text.is_empty() {
                return;
            }
            self.commit_edit(text, cx);
            return;
        }
        let has_content = !text.is_empty() || !self.staged().is_empty();
        match send_intent(self.run_live(cx), has_content, queue_modifier) {
            SendIntent::Stop => self.interrupt(cx),
            _ if !has_content => {}
            SendIntent::Send => self.send(text, false, cx),
            SendIntent::Steer => self.send(text, true, cx),
            SendIntent::Queue => self.queue_follow_up(text, cx),
        }
    }

    fn context_tokens(&self, text: &str) -> Vec<ContextRef> {
        selected_context_refs(text, self.context_ref_checkouts.get(&self.current_key))
    }

    fn clear_input_and_provenance(&mut self, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| input.set_text("", cx));
        clear_draft_provenance(
            &mut self.drafts,
            &mut self.context_ref_checkouts,
            &self.current_key,
        );
        self.schedule_draft_save(cx);
    }

    fn apply_context_edit(&mut self, edit: &ComposerTextEdit) {
        let Some(selections) = self.context_ref_checkouts.get_mut(&self.current_key) else {
            return;
        };
        apply_context_edit(selections, edit);
        if selections.is_empty() {
            self.context_ref_checkouts.remove(&self.current_key);
        }
    }

    fn queue_gesture_id(
        &mut self,
        chat_id: &str,
        payload: &SessionCommandPayload,
        context: &[ContextRef],
    ) -> (String, String) {
        let key = serde_json::to_string(&(chat_id, payload, context))
            .unwrap_or_else(|_| format!("{chat_id}:{payload:?}"));
        let id = self
            .pending_queue_gestures
            .entry(key.clone())
            .or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone();
        (key, id)
    }

    fn queue_add_ids(
        &mut self,
        chat_id: &str,
        prompt: &str,
        context: &[ContextRef],
    ) -> (String, String, String) {
        let key = queue_add_gesture_key(chat_id, prompt, context);
        let command_id = self
            .pending_queue_gestures
            .entry(key.clone())
            .or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone();
        let message_id = self
            .pending_queue_message_ids
            .entry(key.clone())
            .or_insert_with(|| uuid::Uuid::new_v4().to_string())
            .clone();
        (key, command_id, message_id)
    }

    // ---- the follow-up queue (gh#424) ----

    /// Follow the selected chat's plan. Re-subscribed on navigation, dropped
    /// with the composer — the stream ends when the host releases the chat, and
    /// re-opens with it.
    fn ensure_queue_watch(&mut self, cx: &mut Context<Self>) {
        let chat_id = self.state.read(cx).selected_chat.clone();
        if self.queue_chat == chat_id {
            return;
        }
        self.queue_chat = chat_id.clone();
        self.queue = QueueView::default();
        self.editing = None;
        self.queue_task = None;
        let (Some(chat_id), Some(engine)) = (chat_id, self.state.read(cx).engine().cloned()) else {
            return;
        };
        // Watched locally, not forwarded: the plan lives in the chat's SESSION
        // doc, which this device holds a synced replica of — the same reason the
        // transcript watch is local. (The file search is not, because files are
        // not in any doc.)
        self.queue_task = Some(cx.spawn(async move |this, cx| {
            let params = serde_json::json!({ "chatId": chat_id });
            let mut rx = match engine
                .client()
                .subscribe(methods::WATCH_DOC_QUEUE, params)
                .await
            {
                Ok(rx) => rx,
                Err(err) => {
                    tracing::warn!(%chat_id, error = %err, "queue watch failed");
                    return;
                }
            };
            while let Some(value) = rx.recv().await {
                let view: QueueView = match serde_json::from_value(value) {
                    Ok(view) => view,
                    Err(err) => {
                        tracing::warn!(error = %err, "dropping malformed queue frame");
                        continue;
                    }
                };
                let alive = this.update(cx, |composer, cx| {
                    if composer.queue_chat.as_deref() != Some(chat_id.as_str()) {
                        return;
                    }
                    // An edit whose row the plan no longer has (it ran, or
                    // somebody else removed it) releases the composer rather
                    // than leaving it editing a ghost.
                    if let Some(editing) = composer.editing.clone()
                        && view.position(&editing).is_none()
                    {
                        composer.stop_editing(cx);
                    }
                    composer.queue = view;
                    cx.notify();
                });
                if alive.is_err() {
                    return;
                }
            }
        }));
    }

    /// Append a follow-up to the plan.
    ///
    /// No optimistic row: the tray is fed by the host's fold of the ledger, and
    /// an optimistic row would be a second source of truth for the one thing
    /// this design exists to have exactly one of. The round trip is local (the
    /// doc commits on this device and syncs), so the row appears immediately
    /// anyway — and if it does not, that is the truth about a queue the engine
    /// did not accept.
    fn queue_follow_up(&mut self, text: String, cx: &mut Context<Self>) {
        if !self.staged().is_empty() {
            // Until queued rows own a host-staged upload lifecycle, refusing is
            // safer than silently dropping images. Keep both draft and staged
            // files exactly where the user left them.
            self.failure = Some(
                "Images can’t be queued yet — steer them now or wait for the turn to finish."
                    .into(),
            );
            cx.notify();
            return;
        }
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let context = self.context_tokens(&text);
        let (gesture_key, command_id, message_id) = self.queue_add_ids(&chat_id, &text, &context);
        let payload = SessionCommandPayload::Queue {
            prompt: text.clone(),
            message_id,
            attachments: Vec::new(),
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.failure = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let params = serde_json::json!({
            "chatId": chat_id,
            "commandId": command_id,
            "command": payload,
            "context": context,
        });
        self.sending = true;
        self.failure = None;
        self.queue_action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::QUEUE_COMMAND, params).await;
            this.update(cx, |composer, cx| {
                composer.sending = false;
                match result {
                    Ok(_) => {
                        composer.pending_queue_gestures.remove(&gesture_key);
                        composer.pending_queue_message_ids.remove(&gesture_key);
                        // Do not erase keystrokes entered while the durable
                        // append was in flight.
                        if composer.input.read(cx).text().trim() == text {
                            composer.clear_input_and_provenance(cx);
                        }
                    }
                    Err(err) => composer.failure = Some(format!("Queue failed: {err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Load a queued row back into the composer for editing.
    fn start_editing(&mut self, row_id: &str, cx: &mut Context<Self>) {
        let Some(row) = self.queue.rows.iter().find(|r| r.id == row_id) else {
            return;
        };
        let prompt = row.prompt.clone();
        // Park the prompt in progress (and whose checkout its references came
        // from) for the length of the edit.
        self.edit_stash = Some((
            self.drafts
                .get(&self.current_key)
                .cloned()
                .unwrap_or_default(),
            self.context_ref_checkouts.remove(&self.current_key),
        ));
        replace_with_queued_context(
            &mut self.context_ref_checkouts,
            &self.current_key,
            &prompt,
            &row.context,
        );
        self.editing = Some(row_id.to_string());
        self.input
            .update(cx, |input, cx| input.set_text(prompt, cx));
        self.input.update(cx, |input, cx| {
            input.set_placeholder("Editing a queued follow-up — Enter saves, Esc cancels", cx)
        });
        cx.notify();
    }

    fn stop_editing(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_none() {
            return;
        }
        self.restore_stashed_draft(cx);
        self.input
            .update(cx, |input, cx| input.set_placeholder("Do anything…", cx));
        cx.notify();
    }

    fn commit_edit(&mut self, text: String, cx: &mut Context<Self>) {
        let (Some(chat_id), Some(target)) = (
            self.state.read(cx).selected_chat.clone(),
            self.editing.clone(),
        ) else {
            return;
        };
        let context = self.context_tokens(&text);
        let op = QueueOp::Edit {
            target,
            prompt: text.clone(),
            context,
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.failure = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let payload = SessionCommandPayload::QueueControl { op };
        let (gesture_key, command_id) = self.queue_gesture_id(&chat_id, &payload, &[]);
        let params = serde_json::json!({
            "chatId": chat_id,
            "commandId": command_id,
            "command": payload,
        });
        self.sending = true;
        self.queue_action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::QUEUE_COMMAND, params).await;
            this.update(cx, |composer, cx| {
                composer.sending = false;
                match result {
                    Ok(_) => {
                        composer.pending_queue_gestures.remove(&gesture_key);
                        composer.stop_editing(cx)
                    }
                    Err(err) => {
                        composer.failure = Some(format!("Queue update failed: {err}").into());
                        // editing id and draft remain intact for retry.
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Move a row one place up or down: expressed as an anchor
    /// ("after this neighbour"), never an index, so two clients holding
    /// different-length views of the plan converge on the same order.
    fn nudge_row(&mut self, row_id: &str, delta: isize, cx: &mut Context<Self>) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let Some(ix) = self.queue.position(row_id) else {
            return;
        };
        let after = match delta {
            d if d < 0 => match ix {
                0 => return, // already the head
                1 => None,   // to the head
                _ => Some(self.queue.rows[ix - 2].id.clone()),
            },
            _ => match self.queue.rows.get(ix + 1) {
                Some(next) => Some(next.id.clone()),
                None => return, // already the tail
            },
        };
        self.queue_control(
            &chat_id,
            QueueOp::Move {
                target: row_id.to_string(),
                after,
            },
            cx,
        );
    }

    fn remove_row(&mut self, row_id: &str, cx: &mut Context<Self>) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        if self.editing.as_deref() == Some(row_id) {
            self.stop_editing(cx);
        }
        self.queue_control(
            &chat_id,
            QueueOp::Remove {
                target: row_id.to_string(),
            },
            cx,
        );
    }

    fn run_next(&mut self, row_id: &str, cx: &mut Context<Self>) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        self.queue_control(
            &chat_id,
            QueueOp::RunNext {
                target: row_id.to_string(),
            },
            cx,
        );
    }

    fn set_paused(&mut self, paused: bool, cx: &mut Context<Self>) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let op = match paused {
            true => QueueOp::Pause {},
            false => QueueOp::Resume {},
        };
        self.queue_control(&chat_id, op, cx);
    }

    fn queue_control(&mut self, chat_id: &str, op: QueueOp, cx: &mut Context<Self>) {
        self.queue_command(
            chat_id,
            SessionCommandPayload::QueueControl { op },
            Vec::new(),
            "Queue update failed",
            cx,
        );
    }

    /// One durable command, one failure notice. Every plan mutation goes through
    /// here so there is exactly one path from a tray button to the ledger.
    fn queue_command(
        &mut self,
        chat_id: &str,
        payload: SessionCommandPayload,
        context: Vec<ContextRef>,
        failure: &'static str,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.failure = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        let command = match serde_json::to_value(&payload) {
            Ok(command) => command,
            Err(err) => {
                self.failure = Some(format!("{failure}: {err}").into());
                cx.notify();
                return;
            }
        };
        let (gesture_key, command_id) = self.queue_gesture_id(chat_id, &payload, &context);
        let params = serde_json::json!({
            "chatId": chat_id,
            "commandId": command_id,
            "command": command,
            "context": context,
        });
        self.failure = None;
        cx.notify();
        self.queue_action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::QUEUE_COMMAND, params).await;
            this.update(cx, |composer, cx| {
                match result {
                    Ok(_) => {
                        composer.pending_queue_gestures.remove(&gesture_key);
                    }
                    Err(err) => composer.failure = Some(format!("{failure}: {err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Queue a Run (or Steer) doc command with an optimistic echo. New chats
    /// thread the picked config in: worktree creation (when the isolated toggle
    /// is on), `Mutate createChat` with the `ChatConfig` + cwd, and the model /
    /// reasoning / options on the Run request itself (§1.7).
    fn send(&mut self, text: String, steer: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.failure = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        // Chat id: existing selection, or client-minted for the new-chat canvas
        // (the chat then appears from the doc host once the doc materializes).
        let draft_key = self.current_key.clone();
        let configurable = self.state.read(cx).selected_chat_is_draft();
        let selected_chat = self.state.read(cx).selected_chat.clone();
        let materialized_draft = selected_chat.is_some() && configurable;
        let (chat_id, is_new) = match selected_chat {
            Some(id) => (id, configurable),
            None => (uuid::Uuid::new_v4().to_string(), true),
        };
        // Where the new session runs (Current checkout / reuse an existing
        // worktree / fresh worktree off the picked base) — resolved NOW so
        // the async block needs no picker access.
        let plan = self.pickers.read(cx).checkout_plan();
        // Fully-resolved model/reasoning/options — concrete values (chat config
        // or defaults), so the engine never has to guess a "default".
        let resolved = self.pickers.read(cx).resolved(cx);
        let existing_cwd = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.cwd.clone());
        // The SPACE fixes the new chat's device + base folder — this is the
        // behavioral core of spaces: sessions are minted onto the space's
        // device, not necessarily this one.
        let space = self.state.read(cx).selected_space_row().cloned();
        if is_new && space.is_none() {
            self.failure = Some("Add a space first".into());
            cx.notify();
            return;
        }
        let local_device_id = self.state.read(cx).local_device_id.clone();
        let device_id = if is_new {
            space
                .as_ref()
                .map(|s| s.device_id.clone())
                .unwrap_or_else(|| "local".to_string())
        } else {
            self.state
                .read(cx)
                .selected_chat_row()
                .map(|c| c.device_id.clone())
                .or_else(|| local_device_id.clone())
                .unwrap_or_else(|| "local".to_string())
        };
        // Uploads/read-backs target the chat's HOST device (forwardable RPCs);
        // for a new chat that's the space's device (None when it's local).
        let host_device_id = if is_new {
            space
                .as_ref()
                .map(|s| s.device_id.clone())
                .filter(|id| local_device_id.as_deref() != Some(id.as_str()))
        } else {
            self.state
                .read(cx)
                .selected_chat_row()
                .map(|c| c.device_id.clone())
        };
        let space_id = space.as_ref().map(|s| s.id.clone());
        let space_path = space.as_ref().map(|s| s.path.clone());
        let space_remote = space
            .as_ref()
            .is_some_and(|s| local_device_id.as_deref() != Some(s.device_id.as_str()));
        // Snapshot-and-clear NOW (use-attachments.ts takeAttachments): the
        // strip empties the instant you hit send; a failure hands the files
        // back into the chat's stash.
        let staged = self
            .attachments
            .remove(&self.current_key)
            .unwrap_or_default();
        self.preview = None;
        let message_id = uuid::Uuid::new_v4().to_string();
        let created_at = chrono::Utc::now().timestamp_millis();

        // Image-only sends echo the same body `with_attachments` will use, so
        // the bubble never renders empty (refs are upserted in post-upload).
        let echo_text = if text.is_empty() && !staged.is_empty() {
            attachments::ATTACHMENT_ONLY_TEXT.to_string()
        } else {
            text.clone()
        };
        // Snapshot reference provenance with the draft. The async upload may
        // outlive navigation to another chat, and the picker identity is the
        // one captured at completion—not whichever checkout is current later.
        let picked_context = self.context_tokens(&echo_text);
        let picked_provenance =
            take_context_selections(&mut self.context_ref_checkouts, &draft_key);

        // Optimistic echo (client-minted id doubles as the persisted message id,
        // so the doc frame dedups it away).
        let echo = SessionMessageEntry {
            id: message_id.clone(),
            role: comet_doc::MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t0".into(),
                text: echo_text.clone(),
            }],
            created_at,
            device_id: "local".into(),
            status: None,
            continuation_of: None,
        };
        self.state.update(cx, |s, cx| {
            if is_new {
                s.select_chat(Some(chat_id.clone()), cx);
            }
            s.push_echo(&chat_id, echo);
            cx.notify();
        });

        // Sending it is one of the two things that may clear a draft.
        self.input.update(cx, |input, cx| input.set_text("", cx));
        self.drafts.remove(&draft_key);
        self.schedule_draft_save(cx);
        self.failure = None;
        self.sending = true;
        cx.emit(ComposerEvent::Sent {
            chat_id: chat_id.clone(),
        });
        cx.notify();

        let steer_cmd = steer && !is_new;
        let restore_text = text.clone();
        let err_chat_id = chat_id.clone();
        let err_message_id = message_id.clone();
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result: Result<(), String> = async {
                // Resolve the working directory: existing chats keep theirs;
                // new chats run per the checkout plan (t3code env-mode): the
                // space's folder as-is, an EXISTING worktree of the picked ref
                // (a plain cwd override — multiple sessions share one
                // worktree), or a fresh isolated worktree created off the
                // picked base ref (CreateWorktree on send, targeted at the
                // space's device; the RPC relay-forwards).
                let mut cwd = if is_new {
                    space_path.clone()
                } else {
                    existing_cwd
                }
                .unwrap_or_else(|| ".".to_string());
                let mut worktree_cwd: Option<String> = None;
                // The picked ref rides createChat so the session footer names
                // it from the first frame (it read "Select ref" until the
                // host's diff reconciler got around to stamping the branch).
                let mut chat_branch: Option<String> = None;
                if is_new {
                    match &plan {
                        crate::pickers::CheckoutPlan::CurrentCheckout => {}
                        crate::pickers::CheckoutPlan::ReuseWorktree { path, branch } => {
                            cwd = path.clone();
                            worktree_cwd = Some(path.clone());
                            chat_branch = Some(branch.clone());
                        }
                        crate::pickers::CheckoutPlan::NewWorktree { base } => {
                            chat_branch = base.clone();
                            if let (Some(repo_path), Some(base)) = (&space_path, base) {
                                let mut params = serde_json::json!({
                                    "repoPath": repo_path,
                                    "branch": base,
                                });
                                if space_remote
                                    && let Some(object) = params.as_object_mut()
                                {
                                    object.insert(
                                        "targetDeviceId".into(),
                                        serde_json::Value::String(device_id.clone()),
                                    );
                                }
                                let value = engine
                                    .client()
                                    .call(methods::CREATE_WORKTREE, params)
                                    .await
                                    .map_err(|e| format!("Worktree failed: {e}"))?;
                                let worktree: comet_proto::Worktree = serde_json::from_value(value)
                                    .map_err(|e| format!("Worktree reply malformed: {e}"))?;
                                cwd = worktree.path.clone();
                                worktree_cwd = Some(worktree.path);
                            }
                        }
                    }
                }

                // Best-effort Mutate createChat with the picked config: the
                // engine resolves device + cwd from the SPACE row (idempotent;
                // the doc host would materialize the chat on first command
                // anyway, so failures are non-fatal).
                if is_new && let Some(space_id) = &space_id {
                    let config = resolved.chat_config().ok_or_else(|| {
                        "Send failed: choose an available harness before starting the session."
                            .to_string()
                    })?;
                    let mutate = draft_mutate_params(
                        materialized_draft,
                        &chat_id,
                        space_id,
                        &cwd,
                        chat_branch.as_deref(),
                        &config,
                    );
                    if let Err(err) = engine.client().call(methods::MUTATE, mutate).await {
                        // A freshly cut worktree belongs to this transaction.
                        // Roll it back only after a definitive engine refusal;
                        // transport loss is ambiguous and may have committed.
                        if !matches!(err, comet_rpc::RpcError::Closed | comet_rpc::RpcError::Transport(_))
                            && matches!(plan, crate::pickers::CheckoutPlan::NewWorktree { .. })
                            && let (Some(repo_path), Some(path)) = (&space_path, &worktree_cwd)
                        {
                            let _ = engine.client().call(methods::DELETE_WORKTREE, serde_json::json!({
                                "repoPath": repo_path, "worktreePath": path,
                            })).await;
                        }
                        return Err(format!("Send failed while finalizing the session: {err}"));
                    }
                }

                // Stage every attachment on the host device (sequential — the
                // chunks share one channel), then thread the refs into the
                // prompt text (`with_attachments`, the persisted transport)
                // and the paths onto the Run request (inline image blocks).
                let mut content = text.clone();
                let mut attachment_paths: Vec<String> = Vec::new();
                if !staged.is_empty() {
                    for att in &staged {
                        match attachments::upload_attachment(
                            &engine,
                            cx.background_executor(),
                            host_device_id.as_deref(),
                            att,
                        )
                        .await
                        {
                            Ok(path) => attachment_paths.push(path),
                            Err(err) => {
                                tracing::warn!(name = %att.name, error = %err, "attachment upload failed");
                                return Err(
                                    "Couldn't upload the attachment — the device may be offline."
                                        .to_string(),
                                );
                            }
                        }
                    }
                    // Seed the transcript cache from local bytes so the sent
                    // bubble's thumbnails never round-trip (seedTranscript-
                    // Attachment in the original send path).
                    let seed_device = host_device_id.clone().unwrap_or_else(|| device_id.clone());
                    for (path, att) in attachment_paths.iter().zip(&staged) {
                        attachments::seed_attachment(&seed_device, path, &att.name, att.image.clone());
                        if seed_device != device_id {
                            attachments::seed_attachment(&device_id, path, &att.name, att.image.clone());
                        }
                    }
                    content = attachments::with_attachments(&text, &attachment_paths);
                    // Refresh the echo in place with the attachment refs
                    // (same id, same clock — the bubble grows its thumbnails
                    // without flickering).
                    let refreshed = SessionMessageEntry {
                        id: message_id.clone(),
                        role: comet_doc::MessageRole::User,
                        parts: vec![MessagePart::Text {
                            id: "t0".into(),
                            text: content.clone(),
                        }],
                        created_at,
                        device_id: "local".into(),
                        status: None,
                        continuation_of: None,
                    };
                    let echo_chat_id = chat_id.clone();
                    this.update(cx, |composer, cx| {
                        composer.state.update(cx, |s, cx| {
                            s.remove_echo(&echo_chat_id, &message_id);
                            s.push_echo(&echo_chat_id, refreshed);
                            cx.notify();
                        });
                    })
                    .ok();
                }

                // Typed references were snapshotted with the draft before the
                // async upload/navigation boundary. The host resolves them;
                // nothing here touches the filesystem.
                let context = picked_context.clone();
                let command = if steer_cmd {
                    SessionCommandPayload::Steer {
                        prompt: content.clone(),
                        message_id: Some(message_id.clone()),
                    }
                } else {
                    SessionCommandPayload::Run {
                        request: RunRequest {
                            prompt: content.clone(),
                            model: resolved.model.clone(),
                            reasoning: resolved.reasoning,
                            model_options: resolved.model_options.clone(),
                            cwd,
                            sandbox: SandboxLevel::WorkspaceWrite,
                            auto_approve: false,
                            resume: None,
                            attachments: attachment_paths,
                        },
                        message_id: message_id.clone(),
                    }
                };
                let command = serde_json::to_value(&command)
                    .map_err(|e| format!("Send failed: {e}"))?;
                let params = serde_json::json!({
                    "chatId": chat_id,
                    "commandId": uuid::Uuid::new_v4().to_string(),
                    "command": command,
                    "context": context,
                });
                engine
                    .client()
                    .call(methods::QUEUE_COMMAND, params)
                    .await
                    .map_err(|e| format!("Send failed: {e}"))?;
                Ok(())
            }
            .await;
            this.update(cx, |composer, cx| {
                composer.sending = false;
                if let Err(message) = result {
                    // Failure: red banner, echo removed, prompt back in the
                    // draft, staged files back in the chat's stash.
                    composer.failure = Some(message.into());
                    composer.state.update(cx, |s, cx| {
                        s.remove_echo(&err_chat_id, &err_message_id);
                        cx.notify();
                    });
                    // The prompt goes back to the chat it was written for, not
                    // to whatever is on screen when the failure lands: a send
                    // can outlive the navigation away from it, and handing one
                    // chat's prompt to another chat's composer is how a draft
                    // gets sent somewhere it was never meant to go. If that
                    // chat IS on screen the input shows it again, as before.
                    let caret = restore_text.len();
                    composer.drafts.set(
                        &err_chat_id,
                        &restore_text,
                        caret,
                        chrono::Utc::now().timestamp_millis(),
                    );
                    composer.schedule_draft_save(cx);
                    if composer.current_key == err_chat_id {
                        composer
                            .input
                            .update(cx, |input, cx| input.set_text(restore_text, cx));
                    }
                    if let Some(provenance) = picked_provenance {
                        composer
                            .context_ref_checkouts
                            .insert(err_chat_id.clone(), provenance);
                    }
                    if !staged.is_empty() {
                        // Merge by id (stashAttachments): files the user staged
                        // while the send was in flight survive the hand-back.
                        let slot = composer.attachments.entry(err_chat_id.clone()).or_default();
                        let mut merged = staged.clone();
                        merged.extend(
                            slot.drain(..)
                                .filter(|e| !staged.iter().any(|f| f.id == e.id)),
                        );
                        *slot = merged;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn interrupt(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let params = serde_json::json!({
            "chatId": chat_id,
            "commandId": uuid::Uuid::new_v4().to_string(),
            "command": { "kind": "interrupt" },
        });
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::QUEUE_COMMAND, params).await;
            if let Err(err) = result {
                this.update(cx, |composer, cx| {
                    composer.failure = Some(format!("Stop failed: {err}").into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    // ---- wizard glue ----

    fn wizard_select(&mut self, option_ix: usize, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        let step = wizard.select(option_ix);
        let has_pick = wizard.page_has_pick();
        self.input.update(cx, |input, cx| {
            input.set_placeholder(
                if has_pick {
                    "Type your own answer, or leave this blank to use the selected option"
                } else {
                    "Type your own answer, or pick an option above"
                },
                cx,
            )
        });
        match step {
            WizardStep::AutoAdvance => self.schedule_auto_advance(cx),
            WizardStep::Done(answers) => self.wizard_finish(answers, cx),
            WizardStep::Stay => {}
        }
        cx.notify();
    }

    fn schedule_auto_advance(&mut self, cx: &mut Context<Self>) {
        self.advance_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(AUTO_ADVANCE_MS))
                .await;
            this.update(cx, |composer, cx| composer.wizard_advance(cx))
                .ok();
        }));
    }

    fn wizard_advance(&mut self, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.as_mut() else {
            return;
        };
        match wizard.advance() {
            WizardStep::Done(answers) => self.wizard_finish(answers, cx),
            _ => {
                // Moving on: clear the shared free-text input for the next page.
                self.clear_input_and_provenance(cx);
                cx.notify();
            }
        }
    }

    fn wizard_back(&mut self, cx: &mut Context<Self>) {
        if let Some(wizard) = self.wizard.as_mut() {
            wizard.back();
            cx.notify();
        }
    }

    /// Submit RespondInput and retire the panel.
    fn wizard_finish(&mut self, answers: Vec<UserInputAnswer>, cx: &mut Context<Self>) {
        let Some(wizard) = self.wizard.take() else {
            return;
        };
        self.advance_task = None;
        self.answered_requests.insert(wizard.request_id.clone());
        self.clear_input_and_provenance(cx);
        self.input.update(cx, |input, cx| {
            // The panel borrowed the composer input; hand back its identity.
            input.set_placeholder("Do anything…", cx);
        });
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let request_id = wizard.request_id.clone();
        let command = SessionCommandPayload::RespondInput {
            request_id: request_id.clone(),
            answers,
        };
        let params = match serde_json::to_value(&command) {
            Ok(value) => serde_json::json!({ "chatId": chat_id, "command": value }),
            Err(_) => return,
        };
        self.send_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::QUEUE_COMMAND, params).await;
            if let Err(err) = result {
                this.update(cx, |composer, cx| {
                    composer.failure = Some(format!("Answer failed: {err}").into());
                    // The answer never left this device — put the panel back.
                    composer.answered_requests.remove(&request_id);
                    cx.notify();
                })
                .ok();
                return;
            }
            // Safety net against a dead-looking session: the command queued,
            // but the host may still REJECT it (e.g. the run's resolver is
            // gone). If the very same request is still the live pending input
            // once the host has had ample time to execute and the resolved
            // flag to sync back, the answer demonstrably didn't take —
            // un-hide the panel instead of leaving the question unanswerable.
            cx.background_executor().timer(Duration::from_secs(2)).await;
            this.update(cx, |composer, cx| {
                let transcript = composer.state.read(cx).transcript.clone();
                let still_pending = pending_input_request(&transcript)
                    .is_some_and(|(pending_id, _)| pending_id == request_id);
                if still_pending && composer.answered_requests.remove(&request_id) {
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn on_wizard_key(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        // Keys bubbling out of the free-text input must not double-handle:
        // digits select options only while the input is empty, and Enter is the
        // input's own Submit action when it has focus.
        let input_focused = self.input.read(cx).focus_handle.is_focused(window);
        let input_empty = self.input.read(cx).is_empty();
        let key = event.keystroke.key.as_str();
        if let Ok(digit) = key.parse::<usize>()
            && (1..=9).contains(&digit)
        {
            if !input_focused || input_empty {
                self.wizard_select(digit - 1, cx);
                // Consumed as a selection: stop the platform from also
                // inserting the digit into the focused free-text input.
                cx.stop_propagation();
            }
        } else if key == "enter" {
            if !input_focused {
                self.wizard_advance(cx);
                cx.stop_propagation();
            }
        } else if key == "escape" && (!input_focused || input_empty) {
            self.wizard_back(cx);
            cx.stop_propagation();
        }
    }

    // ---- render pieces ----

    /// The agent-asked-a-question panel (comet question-panel.tsx), rendered in
    /// place of the composer: the same floating-pill chrome (`rounded-[26px]
    /// border-white/[0.08] bg-white/[0.03] shadow-xl`), uppercase header +
    /// "1/3" counter chip, option rows with number kbd chips, a free-text
    /// override over a hairline, and Back / Next-Submit footer.
    fn render_wizard(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(wizard) = self.wizard.clone() else {
            return gpui::Empty.into_any_element();
        };
        let counter = wizard.counter();
        let Some(question) = wizard.current().cloned() else {
            return gpui::Empty.into_any_element();
        };
        let page = wizard.page;
        let last = page + 1 >= wizard.questions.len();
        let typed_empty = self.input.read(cx).is_empty();
        let can_advance = wizard.page_has_pick() || !typed_empty;

        let options = question.options.iter().enumerate().map(|(ix, label)| {
            // Selection reads on the row only while no typed override exists
            // (typed answers win — comet question-panel.tsx `isSel`).
            let picked = wizard.is_picked(ix) && typed_empty;
            div()
                .id(("wizard-option", ix))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(12.0))
                .px(px(14.0))
                .py(px(10.0))
                .rounded(px(Theme::RADIUS_ROW))
                .border_1()
                .border_color(if picked {
                    theme.white_alpha(0.16)
                } else {
                    gpui::transparent_black()
                })
                // comet question-panel.tsx option rows: `transition-colors`.
                .bg(if picked {
                    theme.white_alpha(0.09)
                } else {
                    motion::hover_blend(
                        &format!("wizard-option-{ix}"),
                        theme.white_alpha(0.025),
                        theme.white_alpha(0.06),
                    )
                })
                .on_hover(motion::hover_listener(format!("wizard-option-{ix}")))
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| this.wizard_select(ix, cx)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(Theme::TEXT_BODY))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(if picked { theme.text } else { theme.text_muted })
                        .child(SharedString::from(label.clone())),
                )
                .when(ix < 9, |el| {
                    el.child(
                        // Number kbd chip: `size-[22px] rounded-md text-[11px]`.
                        div()
                            .flex_none()
                            .size(px(22.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(Theme::RADIUS_CHIP))
                            .bg(if picked {
                                theme.white_alpha(0.16)
                            } else {
                                theme.white_alpha(0.05)
                            })
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(if picked {
                                theme.text
                            } else {
                                theme.text_subtle
                            })
                            .child(SharedString::from(format!("{}", ix + 1))),
                    )
                })
        });

        div()
            .id("question-panel")
            .track_focus(&self.wizard_focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_wizard_key(event, window, cx)
            }))
            .rounded(px(Theme::RADIUS_CARD))
            .border_1()
            .border_color(theme.border)
            .bg(if theme.light {
                theme.surface_raised
            } else {
                theme.white_alpha(0.03)
            })
            .when(theme.light, |el| el.shadow_sm())
            .when(!theme.light, |el| el.shadow_lg())
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(16.0))
                    .pt(px(16.0))
                    .flex()
                    .flex_col()
                    // Header: tracked uppercase + counter chip when paged.
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(theme.text_subtle)
                                    .child(SharedString::from(crate::popover::tracked_upper(
                                        &question.header,
                                    ))),
                            )
                            .when(wizard.questions.len() > 1, |el| {
                                el.child(
                                    div()
                                        .h(px(20.0))
                                        .px(px(6.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(Theme::RADIUS_CHIP))
                                        .bg(theme.white_alpha(0.06))
                                        .text_size(px(Theme::TEXT_CAPTION))
                                        .font_weight(gpui::FontWeight::MEDIUM)
                                        .text_color(theme.text_subtle)
                                        .child(SharedString::from(counter)),
                                )
                            }),
                    )
                    .child(
                        div()
                            .mt(px(6.0))
                            .text_size(px(Theme::TEXT_TITLE))
                            .line_height(px(20.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(question.question.clone())),
                    )
                    .when(question.multi_select, |el| {
                        el.child(
                            div()
                                .mt(px(4.0))
                                .text_size(px(Theme::TEXT_DENSE))
                                .text_color(theme.text_subtle)
                                .child(SharedString::from("Select one or more options.")),
                        )
                    })
                    .child(
                        div()
                            .mt(px(12.0))
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .children(options),
                    )
                    // Free-text override over a hairline (shares the composer
                    // input entity).
                    .child(
                        div()
                            .mt(px(12.0))
                            .border_t_1()
                            .border_color(theme.white_alpha(0.06))
                            .pt(px(12.0))
                            .pb(px(4.0))
                            .px(px(4.0))
                            .child(self.input.clone()),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_between()
                    .items_center()
                    .px(px(16.0))
                    .pb(px(16.0))
                    .pt(px(4.0))
                    .child(if page > 0 {
                        crate::popover::btn_ghost(&theme, "Back", "wizard-back")
                            .id("wizard-back")
                            .on_click(cx.listener(|this, _, _, cx| this.wizard_back(cx)))
                            .into_any_element()
                    } else {
                        gpui::Empty.into_any_element()
                    })
                    .child(
                        crate::popover::btn_primary(&theme, if last { "Submit" } else { "Next" })
                            .id("wizard-submit")
                            .px(px(16.0))
                            .when(!can_advance, |el| el.opacity(0.4))
                            .on_click(cx.listener(|this, _, _, cx| this.wizard_advance(cx))),
                    ),
            )
            .into_any_element()
    }

    /// The `/` picker: a floating list above the pill, sharing the composer's
    /// chrome so it reads as part of the same control rather than a dialog.
    ///
    /// Rows are name-first with the description trailing, because the name is
    /// what you are typing toward and the description is what confirms you
    /// picked the right one. The source tag sits on the right edge, quiet, and
    /// exists to answer "why is this offered here" — a repo-level skill and a
    /// personal one behave identically until you switch chats.
    fn render_skill_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (_, rows) = self.skill_menu(cx)?;
        let theme = Theme::of(cx).clone();
        let (first, visible) = view_skills::window(
            self.skill_first,
            self.skill_selected,
            rows.len(),
            view_skills::PICKER_MAX_ROWS,
        );
        let hidden = rows.len().saturating_sub(visible);
        let selected = self.skill_selected;

        let list = div()
            .flex()
            .flex_col()
            // The nesting rule (gh#174): rows at RADIUS_ROW one gutter inside
            // the RADIUS_CARD popup, so the list reads as part of the card.
            .p(px(Theme::NEST_GUTTER))
            .children(
                rows.iter()
                    .enumerate()
                    .skip(first)
                    .take(visible)
                    .map(|(ix, skill)| {
                        let active = ix == selected;
                        let name: SharedString = format!("/{}", skill.name).into();
                        div()
                            .id(SharedString::from(format!("skill-row-{ix}")))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .rounded(px(Theme::RADIUS_ROW))
                            .px(px(8.0))
                            .py(px(6.0))
                            .cursor_pointer()
                            .list_row(&theme, Bed::Shell, active, format!("skill-row-{ix}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.skill_selected = ix;
                                this.accept_skill(cx);
                            }))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(if active { theme.text } else { theme.text_muted })
                                    .child(name),
                            )
                            .when_some(skill.description.clone(), |el, description| {
                                el.child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .truncate()
                                        .text_size(px(Theme::TEXT_DENSE))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(description)),
                                )
                            })
                            .child(
                                div()
                                    .flex_none()
                                    .ml_auto()
                                    .text_size(px(Theme::TEXT_CAPTION))
                                    .text_color(theme.text_subtle)
                                    .child(SharedString::from(skill.source.label())),
                            )
                    }),
            )
            // Never silently truncate: a menu that showed 7 of 30 without
            // saying so reads as "these are all the skills there are".
            .when(hidden > 0, |el| {
                el.child(
                    div()
                        .px(px(8.0))
                        .py(px(4.0))
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(format!("{hidden} more — keep typing"))),
                )
            });

        Some(
            div()
                .mx(px(4.0))
                .rounded(px(Theme::RADIUS_CARD))
                .border_1()
                .border_color(theme.white_alpha(0.08))
                .bg(theme.surface_raised)
                .shadow(theme.float_shadow())
                .child(list)
                .into_any_element(),
        )
    }

    /// The `@` picker (gh#424) — same card, same window arithmetic and same
    /// "N more" honesty as the `/` one, two columns instead of three: the name
    /// somebody is typing at, and the directory that disambiguates it.
    fn render_context_menu(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (_, rows) = self.context_menu(cx)?;
        let theme = Theme::of(cx).clone();
        let (first, visible) = view_skills::window(
            self.context_first,
            self.context_selected,
            rows.len(),
            view_skills::PICKER_MAX_ROWS,
        );
        let hidden = rows.len().saturating_sub(visible);
        let selected = self.context_selected;
        let truncated = self.context_truncated;

        let list = div()
            .flex()
            .flex_col()
            .p(px(Theme::NEST_GUTTER))
            .children(
                rows.iter()
                    .enumerate()
                    .skip(first)
                    .take(visible)
                    .map(|(ix, row)| {
                        let active = ix == selected;
                        let name: SharedString = match row.kind.is_dir() {
                            true => format!("{}/", row.name()).into(),
                            false => row.name().to_string().into(),
                        };
                        let parent: SharedString = row.parent().to_string().into();
                        div()
                            .id(SharedString::from(format!("context-row-{ix}")))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .rounded(px(Theme::RADIUS_ROW))
                            .px(px(8.0))
                            .py(px(6.0))
                            .cursor_pointer()
                            .list_row(&theme, Bed::Shell, active, format!("context-row-{ix}"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.context_selected = ix;
                                this.accept_context(cx);
                            }))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(Theme::TEXT_BODY))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(if active { theme.text } else { theme.text_muted })
                                    .child(name),
                            )
                            .when(!parent.is_empty(), |el| {
                                el.child(
                                    div()
                                        .min_w_0()
                                        .flex_1()
                                        .truncate()
                                        .text_size(px(Theme::TEXT_DENSE))
                                        .text_color(theme.text_subtle)
                                        .child(parent),
                                )
                            })
                    }),
            )
            // Two different "there is more": more matches than fit the window,
            // and a host walk that stopped at its budget. Both are said, because
            // both mean the list on screen is not the whole answer.
            .when(hidden > 0 || truncated, |el| {
                el.child(
                    div()
                        .px(px(8.0))
                        .py(px(4.0))
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(match (hidden, truncated) {
                            (0, _) => "more in the checkout — keep typing".to_string(),
                            (n, false) => format!("{n} more — keep typing"),
                            (n, true) => format!("{n} more, and more in the checkout"),
                        })),
                )
            });

        Some(
            div()
                .mx(px(4.0))
                .rounded(px(Theme::RADIUS_CARD))
                .border_1()
                .border_color(theme.white_alpha(0.08))
                .bg(theme.surface_raised)
                .shadow(theme.float_shadow())
                .child(list)
                .into_any_element(),
        )
    }

    /// The follow-up tray (gh#424): the plan, above the composer, in the order
    /// it will run.
    ///
    /// Ordered rows rather than a count badge, because the whole point of the
    /// slice is that a queued instruction is *inspectable* — a number would tell
    /// you that three things are going to happen and not which three, which is
    /// the state the composer was already in.
    fn render_queue_tray(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.queue.rows.is_empty() {
            return None;
        }
        let theme = Theme::of(cx).clone();
        let paused = self.queue.paused;
        let editing = self.editing.clone();
        let count = self.queue.rows.len();
        let last = count.saturating_sub(1);

        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .py(px(4.0))
            .child(
                div()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(match count {
                        1 => "1 queued after this turn".to_string(),
                        n => format!("{n} queued after this turn"),
                    })),
            )
            .when_some(paused, |el, cause| {
                el.child(
                    div()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_subtle)
                        .child(SharedString::from(match cause {
                            QueuePause::Stopped => "· held by Stop",
                            QueuePause::User => "· paused",
                        })),
                )
            })
            .child(
                div()
                    .id("queue-pause")
                    .ml_auto()
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(px(Theme::RADIUS_CHIP))
                    .cursor_pointer()
                    .text_size(px(Theme::TEXT_CAPTION))
                    .text_color(theme.text_muted)
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_paused(paused.is_none(), cx);
                    }))
                    .child(SharedString::from(match paused {
                        Some(_) => "Resume",
                        None => "Pause",
                    })),
            );

        let rows = self.queue.rows.clone();
        let list = div()
            .flex()
            .flex_col()
            .children(rows.iter().enumerate().map(|(ix, row)| {
                let id = row.id.clone();
                let being_edited = editing.as_deref() == Some(id.as_str());
                let text: SharedString = one_line(&row.prompt, QUEUE_ROW_CHARS).into();
                let button = |key: &'static str, label: &'static str| {
                    div()
                        .id(SharedString::from(format!("queue-{key}-{ix}")))
                        .flex_none()
                        .px(px(4.0))
                        .cursor_pointer()
                        .text_size(px(Theme::TEXT_CAPTION))
                        .text_color(theme.text_subtle)
                        .hover(|s| s.text_color(theme.text))
                        .child(label)
                };
                div()
                    .id(SharedString::from(format!("queue-row-{ix}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(8.0))
                    .py(px(5.0))
                    .rounded(px(Theme::RADIUS_ROW))
                    .list_row(&theme, Bed::Shell, being_edited, format!("queue-row-{ix}"))
                    .child(
                        div()
                            .flex_none()
                            .w(px(14.0))
                            .text_size(px(Theme::TEXT_CAPTION))
                            .text_color(theme.text_subtle)
                            .child(SharedString::from(format!("{}", ix + 1))),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .cursor_pointer()
                            .text_size(px(Theme::TEXT_DENSE))
                            .text_color(match being_edited {
                                true => theme.text,
                                false => theme.text_muted,
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener({
                                    let id = id.clone();
                                    move |this, _, _, cx| this.start_editing(&id, cx)
                                }),
                            )
                            .child(text),
                    )
                    .when(row.edited, |el| {
                        el.child(
                            div()
                                .flex_none()
                                .text_size(px(Theme::TEXT_CAPTION))
                                .text_color(theme.text_subtle)
                                .child("edited"),
                        )
                    })
                    .when(ix > 0, |el| {
                        el.child(button("up", "↑").on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, _, cx| this.nudge_row(&id, -1, cx)
                        })))
                    })
                    .when(ix < last, |el| {
                        el.child(button("down", "↓").on_click(cx.listener({
                            let id = id.clone();
                            move |this, _, _, cx| this.nudge_row(&id, 1, cx)
                        })))
                    })
                    .child(button("run-next", "▶").on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| this.run_next(&id, cx)
                    })))
                    .child(button("remove", "✕").on_click(cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| this.remove_row(&id, cx)
                    })))
            }));

        Some(
            div()
                .mx(px(4.0))
                .mb(px(4.0))
                .rounded(px(Theme::RADIUS_CARD))
                .border_1()
                .border_color(theme.white_alpha(0.08))
                .bg(theme.surface_raised)
                .child(div().p(px(Theme::NEST_GUTTER)).child(header).child(list))
                .into_any_element(),
        )
    }

    fn render_send_button(
        &mut self,
        mode: SendButtonMode,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let theme = Theme::of(cx);
        // Comet composer-actions.tsx: a size-7 filled circle — up-arrow to
        // send/steer, a dark rounded square on the same light circle to stop.
        match mode {
            SendButtonMode::Stop => div()
                .id("composer-stop")
                .size(px(28.0))
                .flex_none()
                // round-ok: the stop button — the send button, mid-run
                .rounded_full()
                .bg(theme.text)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_click(cx.listener(|this, _, _, cx| this.interrupt(cx)))
                // scale-ok: the stop square is a drawn glyph inside the button,
                // not a box on the surface — it has no corner to relate to.
                .child(div().size(px(11.0)).rounded(px(3.0)).bg(theme.bg))
                .into_any_element(),
            SendButtonMode::Send | SendButtonMode::Steer => div()
                .id("composer-send")
                .size(px(28.0))
                .flex_none()
                // round-ok: the send button. The one round thing you press
                .rounded_full()
                .bg(theme.text)
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.opacity(0.85))
                .on_click(cx.listener(|this, _, _, cx| this.on_submit(false, cx)))
                .child(
                    crate::icons::icon(crate::icons::ARROW_UP)
                        .size(px(14.0))
                        .text_color(theme.bg),
                )
                .into_any_element(),
        }
    }
}

/// Focus lands on the prompt input (window-level focus fallbacks — e.g. after
/// the focused terminal panel is hidden — route here).
impl Focusable for Composer {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let wizard_active = self.wizard.is_some();
        let mode = self.button_mode(cx);
        let (text_width, has_newline, content_height, last_width, epoch) = {
            let input = self.input.read(cx);
            (
                input.measured_text_width(),
                input.has_newline(),
                input.measured_content_height(),
                input.last_width,
                input.layout_epoch,
            )
        };
        let now = Instant::now();
        // Only measurements taken *after* the last flip may drive the next one
        // (at most one flip per layout pass — a flip invalidates the widths).
        let measured_since_flip = epoch > self.flip_epoch && last_width > 0.0;
        if measured_since_flip {
            // A same-mode width change is an interactive window/pane resize:
            // freeze the mode until sizes settle for RESIZE_SETTLE_MS.
            if self.last_seen_width > 0.0 && (last_width - self.last_seen_width).abs() > 0.5 {
                self.width_changed_at = Some(now);
            }
            self.last_seen_width = last_width;
            if self.expanded_mode {
                if self.expanded_anchor <= 0.0 {
                    self.expanded_anchor = last_width;
                }
            } else {
                // The compact pill's content box is the layout-stable capacity
                // both thresholds measure against.
                self.compact_capacity = last_width - 8.0;
            }
        }
        let resizing = self
            .width_changed_at
            .is_some_and(|t| now.duration_since(t) < Duration::from_millis(RESIZE_SETTLE_MS));
        if resizing && self.settle_task.is_none() {
            // Re-evaluate once the settle window has passed.
            self.settle_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(RESIZE_SETTLE_MS + 20))
                    .await;
                this.update(cx, |composer, cx| {
                    composer.settle_task = None;
                    cx.notify();
                })
                .ok();
            }));
        }
        // Layout-stable compact capacity: measured directly while compact;
        // while expanded, the learned value shifted by any container resize
        // (the expanded input width tracks the container 1:1).
        let capacity = if !self.expanded_mode {
            if last_width > 0.0 {
                last_width - 8.0
            } else {
                f32::MAX // before first measure default to compact
            }
        } else if self.compact_capacity > 0.0 {
            if self.expanded_anchor > 0.0 && last_width > 0.0 {
                self.compact_capacity + (last_width - self.expanded_anchor)
            } else {
                self.compact_capacity
            }
        } else {
            f32::MAX
        };
        let next = composer_flip(
            self.expanded_mode,
            text_width,
            capacity,
            has_newline,
            resizing,
        );
        let committed_flip = next != self.expanded_mode && measured_since_flip;
        if committed_flip {
            self.expanded_mode = next;
            self.flip_epoch = epoch;
            self.expanded_anchor = 0.0;
            // The mode change moves the input width; don't read that jump as
            // an interactive resize.
            self.last_seen_width = 0.0;
        }
        // New chats render expanded regardless of `expanded_mode` (see below),
        // so a mode flip there changes nothing visible — never morph it.
        let new_chat = self.state.read(cx).selected_chat.is_none();
        // Morph clock in ms; dividing by the measurement knob stretches the
        // timeline exactly like shell.rs eval_tween's scaled duration.
        let now_ms = self.morph_clock.elapsed().as_secs_f32() * 1000.0 / motion::speed_scale();
        let route_snap = self
            .route_snap_until
            .is_some_and(|until| Instant::now() < until);
        self.flip_morph = flip_morph_step(
            self.flip_morph,
            committed_flip && !new_chat,
            self.last_rendered_height,
            now_ms,
            motion::reduced_motion(cx),
            route_snap,
        );
        let expanded = self.expanded_mode;

        let failure = self.failure.clone();
        // Centered composer column (comet `mx-auto w-full max-w-3xl`).
        let container = div()
            .w_full()
            .max_w(px(768.0))
            .mx_auto()
            .flex()
            .flex_col()
            .gap(px(Theme::SPACE_SM))
            .px(px(Theme::SPACE_LG))
            .pb(px(Theme::SPACE_LG))
            .when_some(failure, |el, message| {
                // comet composer.tsx `Notice` (matches the transcript
                // ErrorChip palette): `flex items-start gap-2 rounded-xl
                // border px-3 py-2 text-[12px] leading-snug` with a 14px
                // DangerTriangle — a subtle tinted wash, not a bare red
                // stroke. Amber for the offline-ish case (engine not
                // connected), red for send/run failures. Click dismisses.
                let offline = message.as_ref() == "Engine not connected";
                let (border_c, wash, text_c) = if offline {
                    let amber = theme.warning; // amber-400
                    (
                        amber.opacity(0.16),
                        amber.opacity(0.05),
                        theme.warning_text(),
                    )
                } else {
                    let danger = theme.danger; // red-400
                    (
                        danger.opacity(0.16),
                        danger.opacity(0.05),
                        theme.danger_text(),
                    )
                };
                el.child(
                    div()
                        .id("composer-failure")
                        .mx(px(4.0))
                        .mt(px(6.0))
                        .flex()
                        .items_start()
                        .gap(px(8.0))
                        .rounded(px(Theme::RADIUS_CARD))
                        .border_1()
                        .border_color(border_c)
                        .bg(wash)
                        .px(px(12.0))
                        .py(px(8.0))
                        .text_size(px(Theme::TEXT_DENSE))
                        .line_height(px(16.0))
                        .text_color(text_c)
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.failure = None;
                            cx.notify();
                        }))
                        .child(
                            crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                                .size(px(14.0))
                                .mt(px(2.0))
                                .text_color(text_c),
                        )
                        .child(div().min_w_0().child(message)),
                )
            });

        if wizard_active {
            let wizard = self.render_wizard(cx);
            return container.child(motion::fade_quick("composer-wizard", div().child(wizard)));
        }

        // New chats always use the expanded layout: the repo/branch pickers
        // need the full-width actions row (comet composer-actions.tsx
        // `mustExpand = isNew || …`).
        let expanded = expanded || new_chat;

        // Committed-height morph: the layout below is already the NEW mode's;
        // only the pill's height (and the entrance fade/text glide driven by
        // `morph_t`) animates. Steady state renders exactly the target.
        // Staged attachments add the wrap strip's height to the pill in BOTH
        // modes (attachment-ui.tsx AttachmentStrip sits above the input row).
        let staged_count = self.staged().len();
        let strip_width_hint = if last_width > 0.0 { last_width } else { 720.0 };
        let strip_h = attachment_strip_height(staged_count, strip_width_hint);
        let base_height = if expanded {
            composer_total_height(content_height)
        } else {
            COMPACT_TOTAL_HEIGHT
        };
        let target_height = base_height + strip_h;
        let (pill_height, morph_t, morphing) = match self.flip_morph {
            Some(m) if !m.done(now_ms) => {
                (m.height(target_height, now_ms), m.progress(now_ms), true)
            }
            _ => (target_height, 1.0, false),
        };
        if !morphing {
            self.flip_morph = None;
        } else {
            // Manual tween drive: keep frames coming (shell.rs motion_active).
            window.request_animation_frame();
        }
        self.last_rendered_height = pill_height;

        let send_button = self.render_send_button(mode, cx);
        // Attach button — opens the native image picker (the original's hidden
        // `<input type=file accept="image/*" multiple>`); paste/drop also feed
        // the same strip. `ml-1` per the source cluster — chips→attach reads
        // 8px (4 gap + 4 margin) in BOTH modes.
        let attach = div()
            .id("composer-attach")
            .ml(px(4.0))
            .size(px(28.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(Theme::RADIUS_CHIP))
            .cursor_pointer()
            // comet composer-actions.tsx attach: `transition-colors`.
            .bg(motion::hover_blend(
                "composer-attach",
                gpui::transparent_black(),
                theme.white_alpha(0.10),
            ))
            .on_hover(motion::hover_listener("composer-attach"))
            .on_click(cx.listener(|this, _, _, cx| this.open_file_picker(cx)))
            .child(
                crate::icons::icon(crate::icons::PAPERCLIP)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            );
        // Staged-thumbnail strip (attachment-ui.tsx AttachmentStrip), above
        // the input inside the pill in both modes.
        let strip = self.render_attachment_strip(&theme, cx);

        // The pill chrome (comet composer.tsx): `rounded-[26px] border
        // border-white/[0.08] bg-white/[0.03] shadow-xl` — a floating pill with
        // a hairline over a faint wash, never a solid grey box. Picker chips,
        // attach, and the send circle all live INSIDE the pill.
        //
        // Light mode inverts the wash: a 3% black tint over the near-white
        // panel reads as a smudge under the heavy shadow, so the pill goes
        // SOLID (`surface_raised`) with a lighter shadow — a crisp raised well
        // with the same hairline.
        let pill_bg = if theme.light {
            theme.surface_raised
        } else {
            theme.white_alpha(0.03)
        };
        let pill = div()
            .rounded(px(Theme::RADIUS_CARD))
            .bg(pill_bg)
            .border_1()
            .border_color(theme.border)
            .when(theme.light, |el| el.shadow_sm())
            .when(!theme.light, |el| el.shadow_lg());
        // The pill's bottom edge is stationary on screen (the composer sits at
        // the bottom of the shell column; growth moves the TOP edge), so the
        // controls pin to the bottom and only the text glides with the reveal
        // (round-9 follow-up: the send/attach/chips must not ride the height,
        // and none of them fade — the full cluster stays visible throughout).
        let cluster_dy = morph_cluster_dy(morph_t);
        let body = if expanded {
            // Expanded: textarea on top (`px-4 pb-1 pt-4`), actions row
            // (`px-3 pb-2.5 pt-1`, h-8 chips → 46px) ABSOLUTE at the pill's
            // stationary bottom — constant screen-y through the morph, with
            // the 2.5px compact↔expanded centering delta gliding out. The
            // text container is laid out at TARGET size (committed layout
            // never reflows mid-tween — the caret can't jump); its top pad
            // eases 12→16 so the first line glides from its compact resting
            // place. The whole control cluster stays at full alpha — chips,
            // attach and send are all (near-)stationary on the bottom anchor.
            let text_pt = morph_text_pad(morph_t);
            pill.h(px(pill_height))
                .overflow_hidden()
                .relative()
                .flex()
                .flex_col()
                .children(strip)
                .child(
                    div()
                        .h(px(
                            (base_height - PILL_BORDER_V - ACTIONS_ROW_HEIGHT).max(0.0)
                        ))
                        .px(px(16.0))
                        .pt(px(text_pt))
                        .pb(px(4.0))
                        .child(self.input.clone()),
                )
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(px(-cluster_dy))
                        .h(px(ACTIONS_ROW_HEIGHT))
                        .flex()
                        .flex_row()
                        .items_center()
                        // Shared cluster metrics (see CLUSTER_X_DELTA): gap-1
                        // internals identical to compact; only the right
                        // inset (`px-3` 12) differs, and it GLIDES in from
                        // the compact 8 so the buttons never step sideways.
                        .gap(px(4.0))
                        .pl(px(12.0))
                        .pr(px(morph_cluster_inset(true, morph_t)))
                        .pt(px(4.0))
                        .pb(px(10.0))
                        .child(div().flex_1().min_w_0().child(self.pickers.clone()))
                        .child(attach)
                        .child(send_button),
                )
        } else {
            // Compact pill: input and the actions cluster on one 47px line
            // (`py-3 pl-4 pr-2` textarea, `gap-2 py-1.5 pl-1 pr-2` cluster;
            // the 22.75px line centers to the same 12px inset as `py-3`).
            // The row is BOTTOM-justified: during the collapse morph the pill
            // top sweeps down over a stationary row, the text walks down from
            // its expanded resting place via a decaying relative offset, and
            // the whole inline cluster (chips + attach/send) holds its spot at
            // full alpha (2.5px centering delta gliding in).
            let text_glide = match self.flip_morph {
                Some(m) if morphing => collapse_text_glide(m.from, morph_t),
                _ => 0.0,
            };
            pill.h(px(pill_height))
                .overflow_hidden()
                .flex()
                .flex_col()
                .justify_end()
                .children(strip)
                .child(
                    div()
                        .h(px(COMPACT_TOTAL_HEIGHT - PILL_BORDER_V))
                        .flex()
                        .flex_row()
                        .items_center()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .pl(px(16.0))
                                .pr(px(8.0))
                                .relative()
                                .top(px(-text_glide))
                                .child(self.input.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .flex_row()
                                .items_center()
                                // Shared cluster metrics (`gap-1 pl-1 pr-2`,
                                // comet composer-actions.tsx): identical
                                // internals to expanded; the right inset
                                // glides 12→8 on collapse.
                                .gap(px(4.0))
                                .pl(px(4.0))
                                .pr(px(morph_cluster_inset(false, morph_t)))
                                .relative()
                                .top(px(-cluster_dy))
                                .child(div().flex_none().child(self.pickers.clone()))
                                .child(attach)
                                .child(send_button),
                        ),
                )
        };
        // The `/` picker (gh#134) floats directly above the pill and shares
        // its chrome. Rendered before the input in the column so it opens
        // upward, away from the caret — a menu covering the line being typed
        // is a menu you close to see what you wrote.
        // The plan sits ABOVE both menus (gh#424): it is standing state about
        // what happens next, not a transient completion, so a menu must open
        // between it and the caret rather than on top of it.
        let container = match self.render_queue_tray(cx) {
            Some(tray) => container.child(motion::fade_quick("composer-queue", div().child(tray))),
            None => container,
        };
        let container = match self.render_skill_menu(cx) {
            Some(menu) => container.child(motion::fade_quick("composer-skills", div().child(menu))),
            None => container,
        };
        // The `@` picker shares the slot and the reasoning: above the pill,
        // opening upward, away from the caret.
        let container = match self.render_context_menu(cx) {
            Some(menu) => {
                container.child(motion::fade_quick("composer-context", div().child(menu)))
            }
            None => container,
        };
        // The file dropzone lives in the shell (the whole conversation column,
        // not just the pill — shell.rs `chat-dropzone`); drops land back here
        // via `add_paths`.
        let container = container
            // Escape and Tab are unbound in the Composer keymap, so they
            // arrive here as raw keys — which is exactly where they belong:
            // both are about the menu, and the menu is the wrapper's.
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let key = event.keystroke.key.as_str();
                if this.context_menu(cx).is_some() {
                    match key {
                        "escape" => {
                            this.dismiss_context_menu(cx);
                            cx.stop_propagation();
                        }
                        "tab" => {
                            this.accept_context(cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                    return;
                }
                if this.skill_menu(cx).is_some() {
                    match key {
                        "escape" => {
                            this.dismiss_skill_menu(cx);
                            cx.stop_propagation();
                        }
                        "tab" => {
                            this.accept_skill(cx);
                            cx.stop_propagation();
                        }
                        _ => {}
                    }
                    return;
                }
                // With no menu up, Escape leaves an edit of a queued row
                // (gh#424) — the tray said "Esc cancels", and the row it was
                // about is still there, untouched.
                if key == "escape" && this.editing.is_some() {
                    this.stop_editing(cx);
                    cx.stop_propagation();
                }
            }))
            .child(motion::fade_quick("composer-input", body));
        // Branch/worktree toolbar under the pill (t3code BranchToolbar): the
        // checkout-kind selector + ref picker for new sessions, read-only
        // labels once the session exists. Git spaces only.
        let footer = self
            .pickers
            .update(cx, |pickers, cx| pickers.render_footer(cx));
        let container = match footer {
            Some(footer) => container.child(footer),
            None => container,
        };
        // Full-size preview of a staged thumbnail (AttachmentPreviewDialog).
        if let Some(preview) = self.preview.clone() {
            let weak = cx.weak_entity();
            return container.child(attachments::lightbox(
                window.viewport_size(),
                &preview,
                move |_, cx| {
                    weak.update(cx, |this, cx| {
                        this.preview = None;
                        cx.notify();
                    })
                    .ok();
                },
            ));
        }
        container
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An input in a real window — the clipboard verbs need a `Window`, so
    /// these run under gpui's test app rather than as pure functions.
    fn input(
        cx: &mut gpui::TestAppContext,
    ) -> (gpui::Entity<ComposerInput>, &mut gpui::VisualTestContext) {
        let cx = cx.add_empty_window();
        let input = cx.new(|cx| ComposerInput::new("Message", cx));
        (input, cx)
    }

    /// gh#534, acceptance line 4: "paste 50 lines into a composer without
    /// loss". Every line, every blank, every trailing newline — a composer that
    /// drops any of them is a composer you cannot move a diff or a log through.
    #[gpui::test]
    async fn a_fifty_line_paste_lands_verbatim(cx: &mut gpui::TestAppContext) {
        let (input, cx) = input(cx);
        // Blank lines and a CRLF included on purpose: both are what real pasted
        // logs carry, and both are what a line-wise "sanitizer" eats.
        let mut pasted = (1..=48)
            .map(|n| if n % 7 == 0 { String::new() } else { format!("line {n}") })
            .collect::<Vec<_>>()
            .join("\n");
        pasted.push_str("\n\tindented\r\ntrailing\n");
        assert_eq!(pasted.lines().count(), 50);
        cx.update(|_, cx| cx.write_to_clipboard(ClipboardItem::new_string(pasted.clone())));
        cx.update(|window, cx| {
            input.update(cx, |this, cx| this.paste(&Paste, window, cx));
        });
        assert_eq!(cx.read(|cx| input.read(cx).content.clone()), pasted);
    }

    /// A paste with a selection replaces it rather than appending, and the
    /// caret lands after the inserted text (so the next keystroke continues it).
    #[gpui::test]
    async fn a_paste_replaces_the_selection(cx: &mut gpui::TestAppContext) {
        let (input, cx) = input(cx);
        cx.update(|window, cx| {
            input.update(cx, |this, cx| {
                this.replace_text_in_range(None, "keep DROP keep", window, cx);
                this.selected_range = 5..9;
            });
            cx.write_to_clipboard(ClipboardItem::new_string("one\ntwo".to_string()));
        });
        cx.update(|window, cx| {
            input.update(cx, |this, cx| this.paste(&Paste, window, cx));
        });
        cx.read(|cx| {
            let this = input.read(cx);
            assert_eq!(this.content, "keep one\ntwo keep");
            assert_eq!(this.selected_range, 12..12);
        });
    }

    /// ⌘C with no input selection copies the TRANSCRIPT's selection: the
    /// composer keeps focus while a person reads back through the replies, so
    /// it is the only element on the focus path when the copy arrives (gh#534).
    #[gpui::test]
    async fn copy_falls_through_to_the_transcript_selection(cx: &mut gpui::TestAppContext) {
        // The markdown selection is one static for the whole app; hold the
        // same lock selection.rs's own drag tests hold, or they interleave.
        let _selection = crate::markdown::selection::test_lock();
        let (input, cx) = input(cx);
        cx.update(|window, cx| {
            input.update(cx, |this, cx| {
                this.replace_text_in_range(None, "draft", window, cx);
            });
        });
        crate::markdown::selection::begin_with_span("p1", "the error string", 4..16);
        cx.update(|window, cx| {
            input.update(cx, |this, cx| this.copy(&Copy, window, cx));
        });
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("error string".to_string()),
        );
        // An input selection still wins — the composer's own text is nearer.
        cx.update(|window, cx| {
            input.update(cx, |this, cx| {
                this.selected_range = 0..5;
                this.copy(&Copy, window, cx);
            });
        });
        assert_eq!(
            cx.update(|_, cx| cx.read_from_clipboard().and_then(|item| item.text())),
            Some("draft".to_string()),
        );
    }

    fn test_chat_config() -> ChatConfig {
        serde_json::from_value(serde_json::json!({
            "harness": "codex", "model": null, "reasoning": null,
            "modelOptions": {}, "sandbox": "workspace-write", "account": null,
            "pushRepo": null, "pushContract": null, "gitAuthor": null,
            "turnLimits": {}, "mcpServers": []
        }))
        .unwrap()
    }

    #[test]
    fn first_turn_checkout_plans_always_carry_an_exact_cwd() {
        let config = test_chat_config();
        let cases = [
            ("/repo", None),
            ("/existing-worktree", Some("existing")),
            ("/fresh-worktree", Some("feature")),
        ];
        for (cwd, branch) in cases {
            let params = draft_mutate_params(true, "chat", "space", cwd, branch, &config);
            assert_eq!(params["op"], "finalizeChatDraft");
            assert_eq!(params["cwd"], cwd);
            assert_eq!(params["branch"].as_str(), branch);
            assert_eq!(params["config"]["harness"], "codex");
        }
    }

    fn question(id: &str, options: &[&str], multi: bool) -> UserInputQuestion {
        UserInputQuestion {
            id: id.into(),
            header: "Header".into(),
            question: format!("Question {id}"),
            options: options.iter().map(|s| s.to_string()).collect(),
            multi_select: multi,
        }
    }

    #[test]
    fn flip_decision() {
        // Fits in the pill → compact stays compact.
        assert!(!composer_flip(false, 150.0, 300.0, false, false));
        // Overflow → expand.
        assert!(composer_flip(false, 320.0, 300.0, false, false));
        // Newline always expands (either mode, even mid-resize).
        assert!(composer_flip(false, 10.0, 300.0, true, false));
        assert!(composer_flip(true, 10.0, 300.0, true, true));
        // Narrow column (< MIN_COMPACT_INPUT_WIDTH) always expands.
        assert!(composer_flip(false, 10.0, 199.0, false, false));
        assert!(!composer_flip(false, 10.0, 200.0, false, false));
    }

    #[test]
    fn flip_hysteresis_band_prevents_oscillation() {
        let cap = 300.0;
        // Text just over capacity expands…
        assert!(composer_flip(false, cap + 1.0, cap, false, false));
        // …and the SAME width, now expanded, does NOT collapse back — the
        // collapse threshold sits COLLAPSE_HYSTERESIS below the expand one.
        assert!(composer_flip(true, cap + 1.0, cap, false, false));
        // Anywhere inside the band the two modes are both stable (no width in
        // (cap - 32, cap] flips in either direction).
        let in_band = cap - COLLAPSE_HYSTERESIS + 1.0;
        assert!(!composer_flip(false, in_band, cap, false, false));
        assert!(composer_flip(true, in_band, cap, false, false));
        // Comfortably under the band → collapses.
        assert!(!composer_flip(
            true,
            cap - COLLAPSE_HYSTERESIS - 1.0,
            cap,
            false,
            false
        ));
    }

    #[test]
    fn flip_frozen_during_interactive_resize() {
        // While resizing, both modes hold even across their thresholds…
        assert!(!composer_flip(false, 500.0, 300.0, false, true));
        assert!(composer_flip(true, 0.0, 300.0, false, true));
        // …including the narrow-column force-expand.
        assert!(!composer_flip(false, 10.0, 150.0, false, true));
        // Once settled, the same inputs flip.
        assert!(composer_flip(false, 500.0, 300.0, false, false));
        assert!(!composer_flip(true, 0.0, 300.0, false, false));
        assert!(composer_flip(false, 10.0, 150.0, false, false));
    }

    #[test]
    fn caret_blink_phase() {
        // Solid through the first half-period (typing burst never blinks).
        assert!(caret_visible(0));
        assert!(caret_visible(CARET_BLINK_MS - 1));
        // Off for the second half-period, back on for the third.
        assert!(!caret_visible(CARET_BLINK_MS));
        assert!(!caret_visible(2 * CARET_BLINK_MS - 1));
        assert!(caret_visible(2 * CARET_BLINK_MS));
    }

    #[test]
    fn auto_grow_math() {
        // The source heights (comet composer.tsx line 235 clamp, composer-
        // actions.tsx row, 1px hairlines): 76+46+2 empty … 260+46+2 capped.
        assert_eq!(COMPOSER_MIN_HEIGHT, 124.0);
        assert_eq!(COMPOSER_MAX_HEIGHT, 308.0);
        // One line sits at the floor: the textarea BOX (content + `pt-4 pb-1`)
        // clamps UP to 76 exactly like `Math.max(scrollHeight, 76)` — this is
        // what makes the always-expanded new-chat composer 124px tall.
        assert_eq!(
            composer_total_height(input_content_height(1)),
            COMPOSER_MIN_HEIGHT
        );
        // Growth is linear once the textarea box exceeds its 76px floor.
        let h4 = composer_total_height(input_content_height(4));
        assert_eq!(
            h4,
            4.0 * INPUT_LINE_HEIGHT + TEXTAREA_PAD_V + ACTIONS_ROW_HEIGHT + PILL_BORDER_V
        );
        // Caps at a 260px textarea box (comet max-h-[260px] / the JS clamp).
        assert_eq!(
            composer_total_height(input_content_height(100)),
            COMPOSER_MAX_HEIGHT
        );
        // Zero lines still measures one.
        assert_eq!(input_content_height(0), INPUT_LINE_HEIGHT);
    }

    /// One frame short of the full morph timeline (never rounds up to done).
    const ALMOST: f32 = 179.0;

    #[test]
    fn flip_morph_starts_once_per_committed_flip() {
        // No committed flip → no morph.
        assert_eq!(flip_morph_step(None, false, 49.0, 0.0, false, false), None);
        // A committed flip starts one, from the last rendered height…
        let m = flip_morph_step(None, true, 49.0, 100.0, false, false).unwrap();
        assert_eq!(m.from, 49.0);
        assert_eq!(m.start_ms, 100.0);
        // …and same-mode renders keep it UNCHANGED (no restart at the
        // boundary, whatever the heights are doing).
        assert_eq!(
            flip_morph_step(Some(m), false, 80.0, 150.0, false, false),
            Some(m)
        );
        // A finished morph clears on the next same-mode render.
        assert_eq!(
            flip_morph_step(Some(m), false, 124.0, 100.0 + ALMOST, false, false),
            Some(m)
        );
        assert_eq!(
            flip_morph_step(Some(m), false, 124.0, 300.0, false, false),
            None
        );
    }

    #[test]
    fn flip_morph_height_ramps_monotonically_to_target() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        // Starts exactly at the committed height…
        let mut prev = m.height(124.0, 0.0);
        assert_eq!(prev, 49.0);
        // …ramps without ever moving backwards…
        for step in 1..=18 {
            let h = m.height(124.0, step as f32 * 10.0);
            assert!(h >= prev, "height regressed at {step}: {h} < {prev}");
            prev = h;
        }
        // …and lands exactly on the target when done (and stays there).
        assert_eq!(m.height(124.0, 180.0), 124.0);
        assert!(m.done(180.0));
        assert_eq!(m.height(124.0, 500.0), 124.0);
        // Collapse runs the same ramp downward.
        assert!(m.height(124.0, 90.0) > 49.0);
        let down = FlipMorph {
            from: 124.0,
            start_ms: 0.0,
        };
        assert!(down.height(49.0, 90.0) < 124.0);
        assert!(down.height(49.0, 90.0) > 49.0);
    }

    #[test]
    fn flip_morph_reverse_hands_off_from_current_height() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        let mid = m.height(124.0, 90.0);
        assert!(mid > 49.0 && mid < 124.0);
        // A reverse flip mid-flight commits a new morph FROM the animated
        // height — continuous at the handoff, no pop to an endpoint.
        let rev = flip_morph_step(Some(m), true, mid, 90.0, false, false).unwrap();
        assert_eq!(rev.from, mid);
        assert_eq!(rev.height(49.0, 90.0), mid);
    }

    #[test]
    fn flip_morph_snaps_for_reduced_motion_and_first_paint() {
        // Reduced motion never creates a morph (the flip just snaps)…
        assert_eq!(flip_morph_step(None, true, 49.0, 0.0, true, false), None);
        // …and neither does a flip before anything was ever rendered.
        assert_eq!(flip_morph_step(None, true, 0.0, 0.0, false, false), None);
    }

    #[test]
    fn route_change_never_arms_the_morph() {
        // A flip committed inside the route-snap window must NOT animate —
        // switching sessions (chat↔chat or chat↔new-session) snaps the
        // composer straight to the target mode, like the header (round 6).
        assert_eq!(flip_morph_step(None, true, 49.0, 0.0, false, true), None);
        // The route change also kills anything already in flight…
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        assert_eq!(
            flip_morph_step(Some(m), false, 80.0, 50.0, false, true),
            None
        );
        assert_eq!(
            flip_morph_step(Some(m), true, 80.0, 50.0, false, true),
            None
        );
        // …while outside the window the same flip animates as usual.
        let armed = flip_morph_step(None, true, 49.0, 300.0, false, false).unwrap();
        assert_eq!(armed.from, 49.0);
    }

    #[test]
    fn morph_anchoring_holds_controls_and_glides_text() {
        // Steady state (progress 1): no offsets, everything at rest.
        assert_eq!(morph_cluster_dy(1.0), 0.0);
        assert_eq!(morph_text_pad(1.0), 16.0);
        assert_eq!(collapse_text_glide(124.0, 1.0), 0.0);
        // At the commit instant the pieces start from the OLD mode's resting
        // geometry: text pad at the compact 12px inset, cluster displaced by
        // exactly the 2.5px centering delta.
        assert_eq!(morph_text_pad(0.0), 12.0);
        assert_eq!(morph_cluster_dy(0.0), CLUSTER_Y_DELTA);
        // Collapse glide: starts where the expanded text sat (17px below the
        // committed pill top → `from − 53` above the compact resting spot)…
        assert_eq!(collapse_text_glide(124.0, 0.0), 71.0);
        // …decays monotonically to zero…
        let mut prev = collapse_text_glide(124.0, 0.0);
        for step in 1..=10 {
            let g = collapse_text_glide(124.0, step as f32 / 10.0);
            assert!(g <= prev, "glide regressed at {step}");
            prev = g;
        }
        // …and can't go negative on shallow mid-flight reversals.
        assert_eq!(collapse_text_glide(50.0, 0.0), 0.0);
    }

    #[test]
    fn cluster_inset_glides_between_the_source_endpoints() {
        // The morph starts from the OLD mode's resting inset (no sideways
        // step at the commit) and eases to the committed mode's…
        assert_eq!(morph_cluster_inset(true, 0.0), 8.0); // expand: from compact pr-2
        assert_eq!(morph_cluster_inset(true, 1.0), 12.0); // …to expanded px-3
        assert_eq!(morph_cluster_inset(false, 0.0), 12.0); // collapse: from px-3
        assert_eq!(morph_cluster_inset(false, 1.0), 8.0); // …to pr-2
        // …monotonically, bounded by the 4px source delta.
        let mut prev = morph_cluster_inset(true, 0.0);
        for step in 1..=10 {
            let v = morph_cluster_inset(true, step as f32 / 10.0);
            assert!(v >= prev && v <= 8.0 + CLUSTER_X_DELTA);
            prev = v;
        }
        // Internal spacing is SHARED between modes (one cluster in the
        // source) — only this wrapper inset may differ across the flip.
    }

    #[test]
    fn flip_morph_tracks_live_target_and_drives_fade() {
        let m = FlipMorph {
            from: 49.0,
            start_ms: 0.0,
        };
        // Auto-grow can move the target mid-morph: evaluation tracks the
        // live value instead of finishing on a stale height.
        assert!(m.height(159.0, 90.0) > m.height(124.0, 90.0));
        // The eased progress is the actions-row fade: 0 at commit, 1 at rest.
        assert_eq!(m.progress(0.0), 0.0);
        assert_eq!(m.progress(180.0), 1.0);
        let mid = m.progress(90.0);
        assert!(mid > 0.0 && mid < 1.0);
    }

    #[test]
    fn send_button_morph() {
        assert_eq!(send_button_mode(false, false), SendButtonMode::Send);
        assert_eq!(send_button_mode(false, true), SendButtonMode::Send);
        assert_eq!(send_button_mode(true, true), SendButtonMode::Steer);
        assert_eq!(send_button_mode(true, false), SendButtonMode::Stop);
    }

    #[test]
    fn completed_context_keeps_the_checkout_it_was_picked_from() {
        let input = "inspect @src/f";
        let token = view_context::at_token(input, input.len()).expect("active picker token");
        let row = ContextMatch {
            path: "src/foo.rs".into(),
            kind: comet_proto::ContextRefKind::File,
        };
        let (completed, _) = view_context::complete(input, &token, &row.to_ref());
        assert!(completed.ends_with(' '), "completion closes the picker");

        let refs = selected_context_refs(
            &completed,
            Some(&vec![ContextSelection {
                path: "src/foo.rs".into(),
                kind: comet_proto::ContextRefKind::File,
                checkout_id: "checkout-before".into(),
                token_start: token.start,
                token_end: token.start + "@src/foo.rs".len(),
            }]),
        );
        assert_eq!(refs[0].checkout_id.as_deref(), Some("checkout-before"));
        assert_ne!(refs[0].checkout_id.as_deref(), Some("checkout-after"));
    }

    #[test]
    fn hand_typed_context_syntax_remains_plain_prompt_text() {
        assert!(selected_context_refs("inspect @src/forged.rs", None).is_empty());
        assert!(selected_context_refs("inspect @src/forged.rs", Some(&Vec::new())).is_empty());
    }

    #[test]
    fn successful_send_drops_selection_authority_before_the_same_path_is_typed_again() {
        let text = "inspect @src/foo.rs";
        let selections = vec![ContextSelection {
            path: "src/foo.rs".into(),
            kind: comet_proto::ContextRefKind::File,
            checkout_id: "host-checkout".into(),
            token_start: 8,
            token_end: text.len(),
        }];
        assert_eq!(selected_context_refs(text, Some(&selections)).len(), 1);

        // Successful Send/Steer consumes the draft provenance. Identical text
        // in the next draft is only text until the picker selects it again.
        let mut drafts = HashMap::from([("chat-a".into(), selections)]);
        take_context_selections(&mut drafts, "chat-a");
        assert!(selected_context_refs(text, drafts.get("chat-a")).is_empty());
    }

    #[test]
    fn deleting_a_selected_token_revokes_authority_before_retyping_it() {
        let mut selections = vec![ContextSelection {
            path: "src/foo.rs".into(),
            kind: comet_proto::ContextRefKind::File,
            checkout_id: "host-checkout".into(),
            token_start: 8,
            token_end: 19,
        }];

        apply_context_edit(
            &mut selections,
            &ComposerTextEdit {
                replaced: 8..19,
                inserted_len: 0,
            },
        );
        assert!(selections.is_empty(), "deleting the exact token revokes it");
        assert!(selected_context_refs("inspect @src/foo.rs", Some(&selections)).is_empty());
    }

    #[test]
    fn one_picker_occurrence_never_authorizes_a_typed_duplicate() {
        let selected_text = "@src/foo.rs ";
        let mut selections = vec![ContextSelection {
            path: "src/foo.rs".into(),
            kind: comet_proto::ContextRefKind::File,
            checkout_id: "host-checkout".into(),
            token_start: 0,
            token_end: 11,
        }];
        let typed = "and @src/foo.rs";
        apply_context_edit(
            &mut selections,
            &ComposerTextEdit {
                replaced: selected_text.len()..selected_text.len(),
                inserted_len: typed.len(),
            },
        );
        let text = format!("{selected_text}{typed}");
        let refs = selected_context_refs(&text, Some(&selections));
        assert_eq!(refs.len(), 1, "only the picker-selected span is typed");
    }

    #[test]
    fn editor_paste_over_identical_selected_bytes_revokes_the_occurrence() {
        let text = "inspect @src/foo.rs";
        let mut selections = vec![ContextSelection {
            path: "src/foo.rs".into(),
            kind: comet_proto::ContextRefKind::File,
            checkout_id: "host-checkout".into(),
            token_start: 8,
            token_end: text.len(),
        }];
        apply_context_edit(
            &mut selections,
            &ComposerTextEdit {
                replaced: 8..text.len(),
                inserted_len: "@src/foo.rs".len(),
            },
        );
        assert!(
            selected_context_refs(text, Some(&selections)).is_empty(),
            "an indistinguishable editor replacement fails closed"
        );
    }

    #[test]
    fn extending_a_selected_token_at_its_endpoint_revokes_it() {
        let selected = "@src/foo.rs";
        let mut selections = vec![ContextSelection {
            path: "src/foo.rs".into(),
            kind: ContextRefKind::File,
            checkout_id: "host-checkout".into(),
            token_start: 0,
            token_end: selected.len(),
        }];
        apply_context_edit(
            &mut selections,
            &ComposerTextEdit {
                replaced: selected.len()..selected.len(),
                inserted_len: 1,
            },
        );
        assert!(selected_context_refs("@src/foo.rsx ", Some(&selections)).is_empty());
    }

    #[test]
    fn removing_the_preceding_delimiter_revokes_the_selected_token() {
        let mut selections = vec![ContextSelection {
            path: "foo".into(),
            kind: ContextRefKind::File,
            checkout_id: "host-checkout".into(),
            token_start: 2,
            token_end: 6,
        }];
        apply_context_edit(
            &mut selections,
            &ComposerTextEdit {
                replaced: 1..2,
                inserted_len: 0,
            },
        );
        assert!(selected_context_refs("x@foo ", Some(&selections)).is_empty());
    }

    fn stamped_file(path: &str) -> ContextRef {
        ContextRef {
            path: path.into(),
            kind: ContextRefKind::File,
            checkout_id: Some("host-checkout".into()),
        }
    }

    #[test]
    fn starting_queue_edit_replaces_prior_draft_provenance() {
        let mut all = HashMap::from([(
            "chat-a".into(),
            vec![ContextSelection {
                path: "old.rs".into(),
                kind: ContextRefKind::File,
                checkout_id: "host-checkout".into(),
                token_start: 0,
                token_end: 7,
            }],
        )]);
        replace_with_queued_context(
            &mut all,
            "chat-a",
            "edit @new.rs",
            &[stamped_file("new.rs")],
        );
        let selections = all.get("chat-a").unwrap();
        assert_eq!(selections.len(), 1);
        assert_eq!(selections[0].path, "new.rs");
    }

    #[test]
    fn ambiguous_duplicate_queue_context_requires_reselection() {
        let prompt = "typed @same.rs then selected @same.rs";
        assert!(queued_context_selections(prompt, &[stamped_file("same.rs")]).is_empty());
    }

    #[test]
    fn queue_context_never_rehydrates_a_prefix_inside_a_longer_token() {
        assert!(queued_context_selections("inspect @foobar", &[stamped_file("foo")]).is_empty());
    }

    #[test]
    fn queue_context_never_rehydrates_a_midword_at_sign() {
        assert!(queued_context_selections("inspect x@foo", &[stamped_file("foo")]).is_empty());
    }

    #[test]
    fn clearing_an_edit_or_wizard_draft_also_revokes_provenance() {
        let text = "edit @same.rs";
        let selection = ContextSelection {
            path: "same.rs".into(),
            kind: ContextRefKind::File,
            checkout_id: "host-checkout".into(),
            token_start: 5,
            token_end: text.len(),
        };
        let mut drafts = DraftStore::default();
        drafts.set("chat-a", text, text.len(), 0);
        let mut context = HashMap::from([("chat-a".into(), vec![selection])]);
        clear_draft_provenance(&mut drafts, &mut context, "chat-a");
        assert!(drafts.get("chat-a").is_none());
        assert!(selected_context_refs(text, context.get("chat-a")).is_empty());
    }

    #[test]
    fn programmatic_completion_shifts_later_selected_occurrences() {
        let before = "/x @src/foo.rs";
        let mut selections = vec![ContextSelection {
            path: "src/foo.rs".into(),
            kind: ContextRefKind::File,
            checkout_id: "host-checkout".into(),
            token_start: 3,
            token_end: before.len(),
        }];
        apply_context_edit(
            &mut selections,
            &ComposerTextEdit {
                replaced: 0..2,
                inserted_len: "/longer-skill ".len(),
            },
        );
        let after = "/longer-skill  @src/foo.rs";
        assert_eq!(selected_context_refs(after, Some(&selections)).len(), 1);
    }

    #[test]
    fn queued_add_retry_key_is_stable_and_chat_scoped() {
        let context = vec![ContextRef::file("src/foo.rs")];
        let first = queue_add_gesture_key("chat-a", "follow up", &context);
        assert_eq!(
            first,
            queue_add_gesture_key("chat-a", "follow up", &context),
            "a response-loss retry finds the same retained command/message ids"
        );
        assert_ne!(
            first,
            queue_add_gesture_key("chat-b", "follow up", &context)
        );
        assert_ne!(first, queue_add_gesture_key("chat-a", "changed", &context));
    }

    #[test]
    fn wizard_single_select_auto_advances_and_completes() {
        let mut w = Wizard::new(
            "req".into(),
            vec![
                question("q1", &["a", "b"], false),
                question("q2", &["x"], false),
            ],
        );
        assert_eq!(w.counter(), "1/2");
        assert_eq!(w.select(1), WizardStep::AutoAdvance);
        assert!(w.is_picked(1));
        assert_eq!(w.advance(), WizardStep::Stay);
        assert_eq!(w.counter(), "2/2");
        assert_eq!(w.select(0), WizardStep::AutoAdvance);
        let WizardStep::Done(answers) = w.advance() else {
            panic!("expected Done")
        };
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].labels, vec!["b"]);
        assert_eq!(answers[1].labels, vec!["x"]);
    }

    #[test]
    fn wizard_multi_select_toggles_and_stays() {
        let mut w = Wizard::new("req".into(), vec![question("q", &["a", "b", "c"], true)]);
        assert_eq!(w.select(0), WizardStep::Stay);
        assert_eq!(w.select(2), WizardStep::Stay);
        assert!(w.is_picked(0) && w.is_picked(2));
        // Toggle off.
        assert_eq!(w.select(0), WizardStep::Stay);
        assert!(!w.is_picked(0));
        let WizardStep::Done(answers) = w.advance() else {
            panic!()
        };
        assert_eq!(answers[0].labels, vec!["c"]);
    }

    #[test]
    fn wizard_number_keys_and_bounds() {
        let mut w = Wizard::new("req".into(), vec![question("q", &["a", "b"], false)]);
        assert_eq!(w.press_number(9), WizardStep::Stay, "out of range ignored");
        assert_eq!(w.press_number(0), WizardStep::Stay);
        assert_eq!(w.press_number(2), WizardStep::AutoAdvance);
        assert!(w.is_picked(1));
        assert_eq!(w.select(5), WizardStep::Stay, "bad option ix ignored");
    }

    #[test]
    fn wizard_typed_answer_overrides_and_back_pages() {
        let mut w = Wizard::new(
            "req".into(),
            vec![
                question("q1", &["a"], false),
                question("q2", &["x", "y"], false),
            ],
        );
        w.select(0);
        w.advance();
        assert_eq!(w.page, 1);
        assert!(w.back());
        assert_eq!(w.page, 0);
        assert!(!w.back(), "already at first page");
        w.advance();
        w.set_typed("  custom answer  ".into());
        let WizardStep::Done(answers) = w.advance() else {
            panic!()
        };
        assert_eq!(answers[0].labels, vec!["a"]);
        assert_eq!(
            answers[1].labels,
            vec!["custom answer"],
            "typed overrides picked, trimmed"
        );
    }

    #[test]
    fn pending_input_detection() {
        use comet_doc::MessageStatus;
        let input_part = MessagePart::Input {
            id: "in-r1".into(),
            request_id: "r1".into(),
            questions: vec![question("q", &["a"], false)],
            resolved: false,
        };
        let entry = |status: Option<MessageStatus>, parts: Vec<MessagePart>| SessionMessageEntry {
            id: "m".into(),
            role: MessageRole::Assistant,
            parts,
            created_at: 0,
            device_id: "d".into(),
            status,
            continuation_of: None,
        };
        // Streaming entry with unresolved input → panel.
        let t = vec![entry(
            Some(MessageStatus::Streaming),
            vec![input_part.clone()],
        )];
        assert_eq!(
            pending_input_request(&t).map(|(id, _)| id),
            Some("r1".into())
        );
        // DEAD entry with an unresolved input STILL gets the panel: the
        // question stays answerable until answered (the engine delivers the
        // answer as a resumed turn), so a run reaped under its question —
        // engine restart — must not orphan it (user report).
        let t = vec![entry(
            Some(MessageStatus::Aborted),
            vec![input_part.clone()],
        )];
        assert_eq!(
            pending_input_request(&t).map(|(id, _)| id),
            Some("r1".into())
        );
        // A NEWER assistant entry supersedes an unanswered question.
        let t = vec![
            entry(Some(MessageStatus::Aborted), vec![input_part.clone()]),
            SessionMessageEntry {
                id: "m2".into(),
                role: MessageRole::Assistant,
                parts: vec![MessagePart::Text {
                    id: "t2".into(),
                    text: "moved on".into(),
                }],
                created_at: 2,
                device_id: "d".into(),
                status: Some(MessageStatus::Complete),
                continuation_of: None,
            },
        ];
        assert!(pending_input_request(&t).is_none());
        // Resolved part → no panel.
        let resolved = MessagePart::Input {
            id: "in-r1".into(),
            request_id: "r1".into(),
            questions: vec![],
            resolved: true,
        };
        let t = vec![entry(
            Some(MessageStatus::Streaming),
            vec![resolved.clone()],
        )];
        assert!(pending_input_request(&t).is_none());
        assert!(pending_input_request(&[]).is_none());

        // Regression (user forensics): a steer prompt appends a USER entry
        // AFTER the streaming assistant entry — the question must still be
        // found (a last-entry-only read vanished the panel exactly when the
        // user typed, bricking the answer flow).
        let user_echo = SessionMessageEntry {
            id: "u2".into(),
            role: MessageRole::User,
            parts: vec![MessagePart::Text {
                id: "t".into(),
                text: "I answered".into(),
            }],
            created_at: 1,
            device_id: "d".into(),
            status: Some(MessageStatus::Complete),
            continuation_of: None,
        };
        let t = vec![
            entry(Some(MessageStatus::Streaming), vec![input_part.clone()]),
            user_echo,
        ];
        assert_eq!(
            pending_input_request(&t).map(|(id, _)| id),
            Some("r1".into()),
            "question survives entries appended behind the streaming entry"
        );

        // Latch release: only an explicitly resolved matching part releases.
        assert!(!input_request_resolved(&t, "r1"));
        let t = vec![entry(Some(MessageStatus::Streaming), vec![resolved])];
        assert!(input_request_resolved(&t, "r1"));
        assert!(!input_request_resolved(&t, "other"));
    }

    // ---- drafts (gh#536) -------------------------------------------------
    //
    // The composer's contents belong to the chat, not to the view: these tests
    // are the four acceptance cases from the ticket, minus the phone.

    fn draft_chat(id: &str, space: &str) -> comet_proto::Chat {
        comet_proto::Chat {
            id: id.into(),
            device_id: "dev".into(),
            title: None,
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            creation_state: comet_proto::ChatCreationState::Ready,
            last_message_preview: None,
            last_message_at: None,
            created_at: chrono::Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: Some(space.into()),
            last_seen_at: None,
            forked_from: None,
        }
    }

    /// A composer over a temp data dir, with two chats in one space to
    /// navigate between.
    fn draft_fixture(
        cx: &mut gpui::TestAppContext,
        dir: &std::path::Path,
    ) -> (Entity<AppState>, Entity<Composer>) {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.chats = vec![
                draft_chat("chat-a", "space-1"),
                draft_chat("chat-b", "space-1"),
            ];
            state.selected_space = Some("space-1".into());
            state
        });
        let composer = cx.new(|cx| Composer::new(state.clone(), Some(dir.to_path_buf()), cx));
        (state, composer)
    }

    fn type_into(composer: &Entity<Composer>, text: &str, cx: &mut gpui::TestAppContext) {
        composer.update(cx, |composer, cx| {
            let input = composer.input.clone();
            input.update(cx, |input, cx| input.set_text(text, cx));
        });
        cx.run_until_parked();
    }

    fn select(state: &Entity<AppState>, chat: Option<&str>, cx: &mut gpui::TestAppContext) {
        state.update(cx, |s, cx| {
            s.select_chat(chat.map(str::to_string), cx);
            cx.notify();
        });
        cx.run_until_parked();
    }

    fn queued_row(id: &str, prompt: &str) -> comet_doc::QueueRow {
        comet_doc::QueueRow {
            id: id.into(),
            prompt: prompt.into(),
            context: Vec::new(),
            attachments: Vec::new(),
            message_id: "m1".into(),
            issued_by: "dev".into(),
            issued_at: 0,
            edited: false,
        }
    }

    fn shown(composer: &Entity<Composer>, cx: &mut gpui::TestAppContext) -> String {
        composer.read_with(cx, |composer, cx| {
            composer.input.read(cx).text().to_string()
        })
    }

    #[gpui::test]
    async fn a_draft_survives_navigation_and_comes_back_with_its_caret(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (state, composer) = draft_fixture(cx, dir.path());
        select(&state, Some("chat-a"), cx);
        type_into(&composer, "three\nlines\nhere", cx);
        // Park the caret in the middle: leaving is not an excuse to move it.
        composer.update(cx, |composer, cx| {
            let input = composer.input.clone();
            input.update(cx, |input, cx| {
                input.set_text_with_caret("three\nlines\nhere", 6, cx)
            });
        });
        cx.run_until_parked();

        select(&state, Some("chat-b"), cx);
        assert_eq!(shown(&composer, cx), "", "chat-b has no draft of its own");
        type_into(&composer, "b's own words", cx);

        select(&state, Some("chat-a"), cx);
        assert_eq!(shown(&composer, cx), "three\nlines\nhere");
        assert_eq!(
            composer.read_with(cx, |composer, cx| composer.input.read(cx).caret()),
            6,
            "the caret comes back where it was left"
        );
        // Two chats, two drafts, each still its own.
        select(&state, Some("chat-b"), cx);
        assert_eq!(shown(&composer, cx), "b's own words");
    }

    #[gpui::test]
    async fn a_draft_survives_a_quit(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        {
            let (state, composer) = draft_fixture(cx, dir.path());
            select(&state, Some("chat-a"), cx);
            type_into(&composer, "half a thought", cx);
            // What ⌘Q runs.
            composer.update(cx, |composer, cx| composer.flush_drafts(cx));
        }
        // A fresh app over the same data dir: the composer opens holding it.
        let (state, composer) = draft_fixture(cx, dir.path());
        select(&state, Some("chat-a"), cx);
        assert_eq!(shown(&composer, cx), "half a thought");
    }

    #[gpui::test]
    async fn the_two_things_that_clear_a_draft(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, composer) = draft_fixture(cx, dir.path());
        select(&state, Some("chat-a"), cx);
        type_into(&composer, "typed then deleted", cx);
        // (1) The user empties it.
        type_into(&composer, "", cx);
        select(&state, Some("chat-b"), cx);
        select(&state, Some("chat-a"), cx);
        assert_eq!(shown(&composer, cx), "");
        // (2) Sending it — `send` needs an engine, so this is the clear the
        // send path performs, which is also the queue-edit/wizard clear.
        type_into(&composer, "sent", cx);
        composer.update(cx, |composer, cx| composer.clear_input_and_provenance(cx));
        cx.run_until_parked();
        select(&state, Some("chat-b"), cx);
        select(&state, Some("chat-a"), cx);
        assert_eq!(shown(&composer, cx), "");
        // Deleting the chat takes its draft with it.
        type_into(&composer, "about to be deleted", cx);
        composer.update(cx, |composer, cx| composer.purge_chat("chat-a", cx));
        select(&state, Some("chat-b"), cx);
        select(&state, Some("chat-a"), cx);
        assert_eq!(shown(&composer, cx), "");
    }

    #[gpui::test]
    async fn each_space_canvas_keeps_its_own_new_chat_draft(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, composer) = draft_fixture(cx, dir.path());
        // The new-chat canvas of space-1.
        select(&state, None, cx);
        type_into(&composer, "for the first repo", cx);
        // The same canvas in another space is another canvas.
        state.update(cx, |s, cx| {
            s.selected_space = Some("space-2".into());
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(
            shown(&composer, cx),
            "",
            "one space's prompt must not turn up in another space's composer"
        );
        type_into(&composer, "for the second repo", cx);
        state.update(cx, |s, cx| {
            s.selected_space = Some("space-1".into());
            cx.notify();
        });
        cx.run_until_parked();
        assert_eq!(shown(&composer, cx), "for the first repo");
    }

    #[gpui::test]
    async fn a_queued_row_edit_hands_the_draft_back(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let (state, composer) = draft_fixture(cx, dir.path());
        select(&state, Some("chat-a"), cx);
        type_into(&composer, "half a thought", cx);

        composer.update(cx, |composer, cx| {
            composer.queue = QueueView {
                rows: vec![queued_row("row-1", "the queued follow-up")],
                ..QueueView::default()
            };
            composer.start_editing("row-1", cx);
        });
        cx.run_until_parked();
        assert_eq!(shown(&composer, cx), "the queued follow-up");

        // Cancelling the edit is neither sending nor deleting, so the prompt
        // in progress comes back rather than being cleared.
        composer.update(cx, |composer, cx| composer.stop_editing(cx));
        cx.run_until_parked();
        assert_eq!(shown(&composer, cx), "half a thought");
    }

    #[gpui::test]
    async fn leaving_mid_edit_ends_the_edit_in_the_chat_it_started_in(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (state, composer) = draft_fixture(cx, dir.path());
        select(&state, Some("chat-a"), cx);
        type_into(&composer, "half a thought", cx);
        composer.update(cx, |composer, cx| {
            composer.queue = QueueView {
                rows: vec![queued_row("row-1", "the queued follow-up")],
                ..QueueView::default()
            };
            composer.start_editing("row-1", cx);
        });
        cx.run_until_parked();

        select(&state, Some("chat-b"), cx);
        assert!(
            composer.read_with(cx, |composer, _| composer.editing.is_none()),
            "the edit does not follow you into another chat's queue"
        );
        assert_eq!(shown(&composer, cx), "", "chat-b's own composer is empty");
        select(&state, Some("chat-a"), cx);
        assert_eq!(
            shown(&composer, cx),
            "half a thought",
            "and chat-a kept its draft"
        );
    }
}
