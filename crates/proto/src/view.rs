//! Frontend-agnostic view logic: the derivations every viewport needs and none
//! of them should own — sort orders, staleness gating, sidebar grouping, the
//! boot gate, relative times.
//!
//! This lives in `proto` rather than in a viewport crate because comet-native
//! has two of them (the gpui app in `comet-ui`, the terminal app in
//! `comet-tui`) and a *divergent* sort order between them is a real bug: the
//! same workspace doc must produce the same row order on every surface. Both
//! crates re-export from here, so there is exactly one implementation and one
//! test suite per rule.
//!
//! Everything in this module is pure. `chat_indicator` (the status derivation
//! these gate on) is in [`crate::entities`].

use chrono::{DateTime, Utc};

use crate::{AuthState, Chat, ChatIndicator, EdgeHealth, Session, SessionStatus, Space};

pub mod board;
pub mod needs;
pub mod rates;
pub mod repos;
pub mod skills;
pub mod spaces;
pub mod stats;

// ---------------------------------------------------------------------------
// Connection + status
// ---------------------------------------------------------------------------

/// Viewport ⇄ engine connection lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting,
    Ready,
    Failed(String),
}

/// What a chat's status dot / working indicator should show right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Indicator {
    None,
    Working,
    AwaitingInput,
    Errored,
}

/// A `Working`/`AwaitingInput` session older than this is treated as dead — a
/// crashed backend must never show an eternal "Working" (feature-inventory
/// §1.12). Engines heartbeat sessions well inside this window.
pub const SESSION_STALE_MS: i64 = 45_000;

// ---------------------------------------------------------------------------
// Host presence (gh#126)
// ---------------------------------------------------------------------------

/// A device whose overlaid `lastSeenAt` is younger than this reads present.
///
/// The engine restamps that value `now` every 15s for every device it BELIEVES
/// connected (`comet_engine::presence`), so this is ~4 missed republishes — and
/// comfortably wider than the UI's 15s [`EdgeHealth`] poll cadence. It was ~4
/// missed 15s device heartbeats until gh#145 deleted those; the window is
/// unchanged, what it measures is now a belief rather than a beat.
pub const PRESENCE_STALE_MS: i64 = 70_000;

/// What a remote host's presence row may honestly claim.
///
/// The three states exist because "offline" is an ACCUSATION — it says the
/// HOST is gone — and the loudest signal in the sidebar must not make it on
/// evidence that equally fits "this viewer's own sync is down" (gh#126: eight
/// amber rows indicted a box that was up, when the edge was refusing every
/// connection to everyone, this viewer included).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostPresence {
    /// Fresh heartbeat (or the local device, or no evidence either way).
    Online,
    /// The heartbeat lapsed WHILE this viewer could hear — a room of ours is
    /// live, so absence of the beat is evidence about the host.
    Offline,
    /// The heartbeat lapsed while this viewer is deaf — its own engine holds
    /// no live sync room, so it cannot tell a dead host from its own dead
    /// pipe, and must indict the pipe (which is the thing it KNOWS is down).
    SyncDown,
}

/// Host-presence verdict for a remote device row. Pure.
///
/// `last_seen` is the device row's heartbeat-overlaid `lastSeenAt`; `edge` is
/// this viewer's OWN engine's census (`None` = not answered yet, treated as
/// able to hear so behavior degrades to the pre-gh#126 read).
pub fn host_presence(
    is_local_device: bool,
    last_seen: Option<DateTime<Utc>>,
    edge: Option<&EdgeHealth>,
    now: DateTime<Utc>,
) -> HostPresence {
    if is_local_device {
        return HostPresence::Online;
    }
    // No evidence at all (registry row still arriving): say nothing rather
    // than claim an outage nobody observed.
    let Some(last_seen) = last_seen else {
        return HostPresence::Online;
    };
    // A fresh beat outranks any health snapshot: heartbeats arriving IS
    // hearing, whatever a 15s-old census believed.
    if now.signed_duration_since(last_seen).num_milliseconds() <= PRESENCE_STALE_MS {
        return HostPresence::Online;
    }
    // Stale beat: whether that indicts the host depends on whether WE can
    // hear. Presence arrives on the workspace and org rooms; with neither
    // live (down, signed out, or local mode) the beat's absence is expected.
    let deaf = edge.is_some_and(|health| {
        health.workspace_room != Some(true) && health.org_registry != Some(true)
    });
    if deaf {
        HostPresence::SyncDown
    } else {
        HostPresence::Offline
    }
}

/// Staleness-checked indicator for a session row. Pure.
pub fn effective_indicator(session: Option<&Session>, now: DateTime<Utc>) -> Indicator {
    let Some(session) = session else {
        return Indicator::None;
    };
    match session.status {
        SessionStatus::Idle => Indicator::None,
        SessionStatus::Errored => Indicator::Errored,
        SessionStatus::Working | SessionStatus::AwaitingInput => {
            let age_ms = now
                .signed_duration_since(session.updated_at)
                .num_milliseconds();
            if age_ms > SESSION_STALE_MS {
                Indicator::None
            } else if session.status == SessionStatus::Working {
                Indicator::Working
            } else {
                Indicator::AwaitingInput
            }
        }
    }
}

/// The full display status for a chat row / tab dot: live states win, then the
/// synced seen marker decides completed-vs-idle. Staleness gating rides on
/// [`effective_indicator`]; the derivation itself is [`crate::chat_indicator`].
pub fn display_status(chat: &Chat, session: Option<&Session>, now: DateTime<Utc>) -> ChatIndicator {
    let live = session.filter(|s| effective_indicator(Some(s), now) != Indicator::None);
    crate::chat_indicator(chat, live)
}

/// Attention bucket for the sidebar's Active list — lower is more urgent.
pub fn attention_rank(status: ChatIndicator) -> u8 {
    match status {
        ChatIndicator::AwaitingInput => 0,
        ChatIndicator::Errored => 1,
        ChatIndicator::Working => 2,
        ChatIndicator::Completed => 3,
        ChatIndicator::Idle => 4,
    }
}

// ---------------------------------------------------------------------------
// Sort orders
// ---------------------------------------------------------------------------

/// Active-list order: pure recency (`last_message_at` desc, `created_at`
/// fallback), id tiebreak so the sort is total. Deliberately NOT
/// attention-bucketed: status drives the DOT, never the position — bucketing
/// meant that merely OPENING a completed session (completed → seen → idle)
/// dropped its row under the pointer (user report: "their position in the
/// scrollbar changes"). Matches the old sidebar, which rendered chats in
/// recency order and let the dots carry urgency; [`attention_rank`] still
/// aggregates the space rows' urgency dot.
pub fn sort_active(rows: &mut Vec<(ChatIndicator, &Chat)>) {
    rows.sort_by(|(_, a), (_, b)| {
        let ka = a.last_message_at.unwrap_or(a.created_at);
        let kb = b.last_message_at.unwrap_or(b.created_at);
        kb.cmp(&ka).then_with(|| a.id.cmp(&b.id))
    });
}

/// Session-tab order for a space: creation order (activity never reorders
/// tabs), id tiebreak. Pure.
pub fn sort_tabs(chats: &mut [&Chat]) {
    chats.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// Spaces list order: creation order, id tiebreak — total and stable across
/// devices. Pure.
pub fn sort_spaces(spaces: &mut [Space]) {
    spaces.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
}

/// Sidebar order: `last_message_at` desc, falling back to `created_at`; ties
/// break by `created_at` desc then id so the sort is total and stable across
/// devices. Pure.
pub fn sort_chats(chats: &mut [Chat]) {
    chats.sort_by(|a, b| {
        let ka = a.last_message_at.unwrap_or(a.created_at);
        let kb = b.last_message_at.unwrap_or(b.created_at);
        kb.cmp(&ka)
            .then_with(|| b.created_at.cmp(&a.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
}

// ---------------------------------------------------------------------------
// Boot gate
// ---------------------------------------------------------------------------

/// The app gate (comet's App.tsx phases). Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatePhase {
    /// Booting / probing — splash covers this.
    Loading,
    /// Engine unreachable and embedding failed.
    Failed(String),
    /// Engine up, but signed out — show the sign-in card.
    SignIn,
    /// Signed in but no organization selected — "Create your workspace".
    OrgGate,
    /// Render the shell.
    Ready,
}

/// `auth = None` means "engine doesn't report auth yet" (dev mode) and gates
/// nothing.
pub fn gate_phase(connection: &ConnectionStatus, auth: Option<&AuthState>) -> GatePhase {
    match connection {
        ConnectionStatus::Connecting => GatePhase::Loading,
        ConnectionStatus::Failed(err) => GatePhase::Failed(err.clone()),
        ConnectionStatus::Ready => match auth {
            Some(AuthState::SignedOut) => GatePhase::SignIn,
            Some(AuthState::NeedsOrganization { .. }) => GatePhase::OrgGate,
            _ => GatePhase::Ready,
        },
    }
}

/// Parse an `AuthStatus` frame tolerantly. The engine currently serializes its
/// own enum (`{"_tag": "SignedIn", ...}`) while the proto type expects
/// `{"state": "signedIn", ...}` — accept both so either side can converge
/// without breaking a viewport.
pub fn parse_auth_state(value: &serde_json::Value) -> Option<AuthState> {
    if let Ok(state) = serde_json::from_value::<AuthState>(value.clone()) {
        return Some(state);
    }
    let tag = value.get("_tag").and_then(|t| t.as_str())?;
    let user = || -> Option<crate::UserProfile> {
        let u = value.get("user")?;
        Some(crate::UserProfile {
            id: u.get("id")?.as_str()?.to_string(),
            email: u.get("email")?.as_str()?.to_string(),
            name: u.get("name").and_then(|n| n.as_str()).map(str::to_string),
        })
    };
    match tag {
        "SignedOut" => Some(AuthState::SignedOut),
        "NeedsOrganization" => Some(AuthState::NeedsOrganization { user: user()? }),
        "SignedIn" => Some(AuthState::SignedIn {
            user: user()?,
            org_id: value
                .get("orgId")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Sidebar grouping
// ---------------------------------------------------------------------------

/// One grouped-by-project sidebar section.
#[derive(Debug, Clone, PartialEq)]
pub struct ChatGroup<'a> {
    pub label: String,
    pub chats: Vec<&'a Chat>,
}

/// Project label for a chat: the basename of its cwd, or "No project".
pub fn project_label(cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.map(str::trim).filter(|c| !c.is_empty()) else {
        return "No project".to_string();
    };
    std::path::Path::new(cwd.trim_end_matches(['/', '\\']))
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| cwd.to_string())
}

/// Group chats by project label, preserving the incoming (recency) order both
/// for groups (by their most recent chat) and rows within a group. Pure.
pub fn group_chats<'a>(chats: impl IntoIterator<Item = &'a Chat>) -> Vec<ChatGroup<'a>> {
    let mut groups: Vec<ChatGroup<'a>> = Vec::new();
    for chat in chats {
        let label = project_label(chat.cwd.as_deref());
        match groups.iter_mut().find(|g| g.label == label) {
            Some(group) => group.chats.push(chat),
            None => groups.push(ChatGroup {
                label,
                chats: vec![chat],
            }),
        }
    }
    groups
}

/// Compact relative time ("now", "5m", "3h", "2d", "1w", …) — no "ago" suffix;
/// port of comet's `formatTimeAgo`.
pub fn format_time_ago(then: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let s = now.signed_duration_since(then).num_seconds().max(0);
    // Under a minute reads as "now" — otherwise 45–59s floors to a bare "0m".
    if s < 60 {
        return "now".to_string();
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h");
    }
    let d = h / 24;
    if d < 7 {
        return format!("{d}d");
    }
    let w = d / 7;
    if w < 5 {
        return format!("{w}w");
    }
    let mo = d / 30;
    if mo < 12 {
        return format!("{mo}mo");
    }
    format!("{}y", d / 365)
}

/// Session-row sub-line, "project · branch" (comet `chatLocation`): the repo
/// checkout identity. Either part may be missing; empty when both are.
pub fn chat_location(chat: &Chat) -> Option<String> {
    let project = chat
        .cwd
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| project_label(Some(c)));
    let reference = chat
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());
    match (project, reference) {
        (Some(p), Some(r)) => Some(format!("{p} · {r}")),
        (Some(p), None) => Some(p),
        (None, Some(r)) => Some(r.to_string()),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// Tool summaries (pure)
// ---------------------------------------------------------------------------

/// Collapse model-generated text onto ONE line for single-line surfaces (tool
/// chips, titles, previews): newlines, tabs and runs of whitespace become
/// single spaces, trimmed.
///
/// Both viewports need this for the same reason from opposite directions — gpui
/// breaks on a literal `\n` before its ellipsis logic, and a terminal cell grid
/// would take an embedded newline as a cursor move.
pub fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

/// Per-kind chip label + one-line detail. Labels match comet's `describeTool`
/// (tool-chip.tsx) exactly, so the two viewports name a tool identically.
pub fn tool_chip_content(call: &crate::ToolCall) -> (&'static str, String) {
    let (label, detail) = tool_chip_content_raw(call);
    (label, single_line(&detail))
}

fn tool_chip_content_raw(call: &crate::ToolCall) -> (&'static str, String) {
    use crate::ToolCall;
    match call {
        ToolCall::Exec { command } => ("Run", command.clone()),
        ToolCall::ReadFile { path } => ("Read", path.clone()),
        ToolCall::WriteFile { path, .. } => ("Write", path.clone()),
        ToolCall::EditFile { path, .. } => ("Edit", path.clone()),
        ToolCall::ApplyPatch { path } => {
            ("Patch", path.clone().unwrap_or_else(|| "workspace".into()))
        }
        ToolCall::Search { pattern, path } => (
            "Search",
            match path {
                Some(path) => format!("{pattern} in {path}"),
                None => pattern.clone(),
            },
        ),
        ToolCall::Glob { pattern } => ("Glob", pattern.clone()),
        ToolCall::WebFetch { url, .. } => ("Fetch", url.clone()),
        ToolCall::WebSearch { query } => ("Web", query.clone()),
        ToolCall::Todo { items } => {
            let done = items.iter().filter(|i| i.done).count();
            ("Todo", format!("{done}/{} done", items.len()))
        }
        ToolCall::Mcp { server, tool, .. } => ("MCP", format!("{server} · {tool}")),
        // The name leads and carries the slash, because `/comet-board` is what
        // the operator and the agent both call it; the arguments are the
        // detail. Every other kind puts a verb in the label slot — a skill's
        // verb IS its name, so there is nothing generic to put in front of it.
        ToolCall::Skill { name, args } => (
            "Skill",
            match args.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
                Some(args) => format!("/{name} {args}"),
                None => format!("/{name}"),
            },
        ),
        ToolCall::Unknown { name, .. } => ("Tool", name.clone()),
    }
}

/// The ToolGroup summary line — "Ran 3 commands · edited 2 files".
///
/// Takes `(call, is_error)` pairs so each viewport can keep its own row model;
/// the summary itself is one implementation for both.
pub fn tool_group_summary(tools: &[(crate::ToolCall, bool)]) -> String {
    use crate::ToolCall;
    let mut commands = 0usize;
    let mut edited: Vec<&str> = Vec::new();
    let mut reads = 0usize;
    let mut searches = 0usize;
    let mut fetches = 0usize;
    let mut todos = 0usize;
    let mut other = 0usize;
    let mut failed = 0usize;
    for (call, is_error) in tools {
        if *is_error {
            failed += 1;
        }
        match call {
            ToolCall::Exec { .. } => commands += 1,
            ToolCall::WriteFile { path, .. } | ToolCall::EditFile { path, .. } => {
                if !edited.contains(&path.as_str()) {
                    edited.push(path);
                }
            }
            ToolCall::ApplyPatch { path } => {
                let p = path.as_deref().unwrap_or("patch");
                if !edited.contains(&p) {
                    edited.push(p);
                }
            }
            ToolCall::ReadFile { .. } => reads += 1,
            ToolCall::Search { .. } | ToolCall::Glob { .. } | ToolCall::WebSearch { .. } => {
                searches += 1
            }
            ToolCall::WebFetch { .. } => fetches += 1,
            ToolCall::Todo { .. } => todos += 1,
            // Defensive: both viewports break a skill out of the group into a
            // landmark of its own (gh#134), so one reaching here is a group
            // that was assembled without that split — count it rather than
            // dropping it from a summary that claims to cover the run.
            ToolCall::Mcp { .. } | ToolCall::Skill { .. } | ToolCall::Unknown { .. } => other += 1,
        }
    }
    let mut segments: Vec<String> = Vec::new();
    if commands > 0 {
        segments.push(format!("ran {}", plural(commands, "command", "commands")));
    }
    if !edited.is_empty() {
        segments.push(format!("edited {}", plural(edited.len(), "file", "files")));
    }
    if reads > 0 {
        segments.push(format!("read {}", plural(reads, "file", "files")));
    }
    if searches > 0 {
        segments.push(format!("searched {}", plural(searches, "time", "times")));
    }
    if fetches > 0 {
        segments.push(format!("fetched {}", plural(fetches, "page", "pages")));
    }
    if todos > 0 {
        segments.push("updated todos".to_string());
    }
    if other > 0 {
        segments.push(format!("called {}", plural(other, "tool", "tools")));
    }
    if segments.is_empty() {
        segments.push(plural(tools.len(), "tool", "tools"));
    }
    if failed > 0 {
        segments.push(format!("{failed} failed"));
    }
    let mut summary = segments.join(" · ");
    // Capitalize the first segment only (comet's style).
    if let Some(first) = summary.get(0..1) {
        let upper = first.to_uppercase();
        summary.replace_range(0..1, &upper);
    }
    summary
}

/// The status ramp: four hues at one lightness and one chroma (gh#173).
///
/// Colors live here rather than in either viewport because the *meaning* of a
/// hue must not differ between surfaces — a session that reads "running" in the
/// desktop app cannot read "error" in the terminal. Each frontend converts to
/// its own color type; `comet-ui` has the oklch→sRGB math, `comet-tui` pins the
/// converted values with a test.
///
/// The lightness is the point. Before the anchor the palette agreed on chroma
/// (≈0.19) and nothing else: amber sat at L 0.828 and indigo at L 0.673, so the
/// warning read twice as loud as the accent even where it meant less — which is
/// why a fifth hue (pink) had been introduced for "running", to escape an amber
/// that shouted. One lightness retires the pink.
pub mod status {
    /// The lightness every status hue is anchored to on a dark surface.
    pub const L: f32 = 0.74;
    /// The same anchor on a light surface — walked down so the hues keep
    /// contrast on near-white (~5:1).
    pub const L_LIGHT: f32 = 0.55;
    /// The chroma every status hue carries. The old palette already agreed on
    /// ≈0.19; 0.14 is that intent at the new lightness, where 0.19 would push
    /// amber and emerald out of sRGB.
    pub const C: f32 = 0.14;

    /// Blocked · failed · errored.
    pub const BLOCKED: f32 = 25.0;
    /// Working — an agent is running. Board pane AND sidebar.
    pub const WORKING: f32 = 75.0;
    /// Review · a question · links · focus.
    pub const REVIEW: f32 = 265.0;
    /// Settled · seen · online.
    pub const SETTLED: f32 = 160.0;

    /// A hue at the dark anchor, as an oklch triple (L, C, H°).
    pub const fn dark(hue: f32) -> (f32, f32, f32) {
        (L, C, hue)
    }

    /// A hue at the light anchor.
    pub const fn light(hue: f32) -> (f32, f32, f32) {
        (L_LIGHT, C, hue)
    }
}

/// The status-dot palette, as oklch triples (L, C, H°) — the [`status`] ramp
/// read through a chat's display status.
pub mod dot {
    use super::status;

    /// Running. Amber, at the anchor: running is routine, and the ramp is what
    /// keeps it from shouting.
    pub const WORKING: (f32, f32, f32) = status::dark(status::WORKING);
    /// Asking a question. The review hue — something wants your eyes, and it
    /// must read differently from "busy" at a glance.
    pub const AWAITING: (f32, f32, f32) = status::dark(status::REVIEW);
    /// Errored. The blocked hue.
    pub const ERRORED: (f32, f32, f32) = status::dark(status::BLOCKED);
    /// Finished but unseen. The settled hue — reads as "ready for you".
    pub const COMPLETED: (f32, f32, f32) = status::dark(status::SETTLED);
}

// ---------------------------------------------------------------------------
// Checkout selection (new sessions)
// ---------------------------------------------------------------------------

/// Where a new session runs (t3code's env-mode: `local | worktree`).
///
/// "Current worktree" is deliberately **not** a third mode — it is `Local` when
/// the picked ref already happens to be materialized as a worktree, in which
/// case the session reuses that checkout's path. Modelling it as three states
/// would let the UI hold a combination the engine cannot honour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckoutKind {
    /// The space's own folder — or the picked ref's existing worktree.
    #[default]
    Local,
    /// A fresh isolated worktree created off the picked base ref on send.
    NewWorktree,
}

/// The resolved on-send checkout action.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutPlan {
    /// Run in the space folder as-is.
    CurrentCheckout,
    /// Reuse the picked ref's existing worktree (a cwd override; no git).
    ReuseWorktree { path: String, branch: String },
    /// `CreateWorktree` off `base` on send (the engine mints a `comet/<name>`
    /// branch). `base: None` = refs never loaded — send falls back to the space
    /// folder rather than failing.
    NewWorktree { base: Option<String> },
}

/// Resolve the on-send action from the mode and the picked ref.
pub fn checkout_plan(kind: CheckoutKind, picked: Option<&crate::RepoRef>) -> CheckoutPlan {
    let name = picked.map(|r| r.name.clone());
    match kind {
        CheckoutKind::NewWorktree => CheckoutPlan::NewWorktree { base: name },
        CheckoutKind::Local => match picked.and_then(|r| r.worktree_path.clone()) {
            Some(path) => CheckoutPlan::ReuseWorktree {
                path,
                branch: name.unwrap_or_default(),
            },
            None => CheckoutPlan::CurrentCheckout,
        },
    }
}

/// Label of the checkout-kind trigger (t3code `resolveEnvModeLabel`).
pub fn checkout_label(kind: CheckoutKind, picked: Option<&crate::RepoRef>) -> &'static str {
    match kind {
        CheckoutKind::NewWorktree => "New worktree",
        CheckoutKind::Local => {
            if picked.is_some_and(|r| r.worktree_path.is_some()) {
                "Current worktree"
            } else {
                "Current checkout"
            }
        }
    }
}

#[cfg(test)]
mod presence_tests {
    use super::*;
    use chrono::TimeDelta;

    fn hearing() -> EdgeHealth {
        EdgeHealth {
            edge_url: Some("https://edge.example".into()),
            workspace_room: Some(true),
            org_registry: Some(true),
            ..EdgeHealth::default()
        }
    }

    fn deaf() -> EdgeHealth {
        EdgeHealth {
            edge_url: Some("https://edge.example".into()),
            workspace_room: Some(false),
            org_registry: Some(false),
            ..EdgeHealth::default()
        }
    }

    #[test]
    fn fresh_beats_and_missing_evidence_read_online() {
        let now = Utc::now();
        let fresh = Some(now - TimeDelta::seconds(20));
        assert_eq!(
            host_presence(false, fresh, Some(&hearing()), now),
            HostPresence::Online
        );
        // The local device is trivially online; no row yet says nothing.
        assert_eq!(host_presence(true, None, None, now), HostPresence::Online);
        assert_eq!(
            host_presence(false, None, Some(&deaf()), now),
            HostPresence::Online
        );
    }

    /// The window boundary the devices page has always used (70s).
    #[test]
    fn the_window_is_70s() {
        let now = Utc::now();
        let at = |s: i64| Some(now - TimeDelta::seconds(s));
        let hearing = hearing();
        assert_eq!(
            host_presence(false, at(70), Some(&hearing), now),
            HostPresence::Online
        );
        assert_eq!(
            host_presence(false, at(71), Some(&hearing), now),
            HostPresence::Offline
        );
    }

    /// The gh#126 rule: a lapsed beat only indicts the HOST while this viewer
    /// can hear. Deaf (both sync rooms down — the edge outage shape, or signed
    /// out, or local mode) indicts the pipe instead.
    #[test]
    fn a_deaf_viewer_never_claims_the_host_is_offline() {
        let now = Utc::now();
        let stale = Some(now - TimeDelta::hours(8));
        assert_eq!(
            host_presence(false, stale, Some(&deaf()), now),
            HostPresence::SyncDown
        );
        // One live room is enough to hear on: back to a real offline verdict.
        let org_only = EdgeHealth {
            workspace_room: Some(false),
            org_registry: Some(true),
            ..hearing()
        };
        assert_eq!(
            host_presence(false, stale, Some(&org_only), now),
            HostPresence::Offline
        );
        // No census at all (engine predates it / first poll pending): the
        // pre-gh#126 read, so nothing regresses while the answer loads.
        assert_eq!(
            host_presence(false, stale, None, now),
            HostPresence::Offline
        );
        // A fresh beat outranks a deaf census — hearing it IS hearing.
        let fresh = Some(now - TimeDelta::seconds(10));
        assert_eq!(
            host_presence(false, fresh, Some(&deaf()), now),
            HostPresence::Online
        );
    }
}

#[cfg(test)]
mod checkout_tests {
    use super::*;
    use crate::RepoRef;

    fn plain(name: &str) -> RepoRef {
        RepoRef {
            name: name.into(),
            current: false,
            worktree_path: None,
        }
    }

    fn materialized(name: &str, path: &str) -> RepoRef {
        RepoRef {
            name: name.into(),
            current: false,
            worktree_path: Some(path.into()),
        }
    }

    #[test]
    fn local_resolves_by_whether_the_ref_has_a_worktree() {
        // The same mode means two different things depending on the ref — which
        // is exactly why "current worktree" is not its own state.
        assert_eq!(
            checkout_plan(CheckoutKind::Local, Some(&plain("main"))),
            CheckoutPlan::CurrentCheckout
        );
        assert_eq!(
            checkout_plan(CheckoutKind::Local, Some(&materialized("feat", "/wt/feat"))),
            CheckoutPlan::ReuseWorktree {
                path: "/wt/feat".into(),
                branch: "feat".into()
            }
        );
        // No ref picked at all is still the space folder.
        assert_eq!(
            checkout_plan(CheckoutKind::Local, None),
            CheckoutPlan::CurrentCheckout
        );
    }

    #[test]
    fn new_worktree_carries_its_base_and_tolerates_none() {
        assert_eq!(
            checkout_plan(CheckoutKind::NewWorktree, Some(&plain("main"))),
            CheckoutPlan::NewWorktree {
                base: Some("main".into())
            }
        );
        // Refs never loaded: send falls back to the space folder rather than
        // failing, so the base is allowed to be absent.
        assert_eq!(
            checkout_plan(CheckoutKind::NewWorktree, None),
            CheckoutPlan::NewWorktree { base: None }
        );
    }

    #[test]
    fn labels_say_which_of_the_three_outcomes_you_will_get() {
        assert_eq!(
            checkout_label(CheckoutKind::Local, Some(&plain("main"))),
            "Current checkout"
        );
        assert_eq!(
            checkout_label(CheckoutKind::Local, Some(&materialized("f", "/wt/f"))),
            "Current worktree"
        );
        assert_eq!(
            checkout_label(CheckoutKind::NewWorktree, Some(&plain("main"))),
            "New worktree"
        );
    }
}
