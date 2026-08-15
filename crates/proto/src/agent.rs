//! Agent-side wire types: harness identity, run requests, streaming events, tool calls.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessId {
    ClaudeCode,
    Codex,
    Cursor,
    /// The opencode CLI (`opencode serve` — an HTTP/SSE headless server, the
    /// same interface the TUI/web frontends talk to).
    Opencode,
    /// Test harness; never shown in production pickers.
    Mock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningLevel {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
    Ultra,
    /// xhigh + harness-specific setting.
    Ultracode,
    /// Prompt-prefix driven (Claude).
    Ultrathink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxLevel {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl SandboxLevel {
    /// The wire word, for a surface that renders the level as text.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    /// How much this level permits, as a total order — the thing a comparison
    /// between a requested and an effective level needs and an enum does not
    /// give for free (gh#349).
    ///
    /// Deliberately not a `derive(PartialOrd)`: the variant order happens to
    /// agree today, and a level inserted in the middle later would silently
    /// re-rank every comparison in the workspace.
    pub fn permissiveness(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::WorkspaceWrite => 1,
            Self::DangerFullAccess => 2,
        }
    }

    /// Nothing between the agent and the box. The one level a reviewer has to
    /// be told about, whether it was asked for or arrived at.
    pub fn is_full_access(self) -> bool {
        matches!(self, Self::DangerFullAccess)
    }
}

/// What sandbox a run *actually* got, beside what was asked for (gh#349).
///
/// The board names a [`SandboxLevel`] on every dispatch and then never hears
/// what became of it. That gap is not theoretical: the Codex adapter used to
/// widen `workspace-write` to `danger-full-access` for a whole shape of
/// checkout, and said so only in a `WARN` nobody reads — so every board
/// dispatch ran unsandboxed and every surface still displayed the level that
/// had been requested. With `approval_policy` pinned to `"never"` on every
/// runtime, the sandbox is the only guardrail there is, and a guardrail whose
/// actual setting is unobservable is not one.
///
/// So the harness states it, once per run, as a fact the journal keeps: what
/// was asked for, what was applied, and — when they differ — why. A reviewer
/// reading an attempt afterwards can then be told "this agent had full access
/// to the box" instead of being shown the request and left to assume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxReport {
    /// The level the run was dispatched with.
    pub requested: SandboxLevel,
    /// The level the harness actually applied.
    pub effective: SandboxLevel,
    /// Why the two differ, in one sentence a reviewer can read. `None` when
    /// they do not differ — and required, by convention, when they do: an
    /// unexplained widening is the thing this type exists to stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl SandboxReport {
    /// A run that got exactly what it asked for.
    pub fn as_requested(level: SandboxLevel) -> Self {
        Self {
            requested: level,
            effective: level,
            reason: None,
        }
    }

    /// A run whose harness applied something other than what was asked.
    pub fn widened(
        requested: SandboxLevel,
        effective: SandboxLevel,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            requested,
            effective,
            reason: Some(reason.into()),
        }
    }

    /// Did the run end up with more than it asked for? The direction that
    /// matters — a harness that applied *less* has a run that fails loudly,
    /// while one that applied more has a run that succeeds quietly.
    pub fn widened_beyond_request(&self) -> bool {
        self.effective.permissiveness() > self.requested.permissiveness()
    }

    /// The sentence a surface leads with, or `None` when there is nothing a
    /// reviewer needs to be told: the run got what it asked for, and what it
    /// asked for was not full access.
    ///
    /// Full access is reported even when it was requested. "Nobody widened
    /// this" is not the reassurance it sounds like when the level was
    /// `danger-full-access` from the start.
    pub fn note(&self) -> Option<String> {
        match (
            self.widened_beyond_request(),
            self.effective.is_full_access(),
        ) {
            (false, false) => None,
            (false, true) => Some("this agent had full access to the box".into()),
            (true, full) => {
                let head = if full {
                    "this agent had full access to the box".to_string()
                } else {
                    format!("this agent ran at {}", self.effective.as_str())
                };
                Some(match &self.reason {
                    Some(why) => format!(
                        "{head} — {} was requested, and {why}",
                        self.requested.as_str()
                    ),
                    None => format!("{head} — {} was requested", self.requested.as_str()),
                })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SteeringMode {
    /// Steer delivered at the next step boundary within the live turn.
    StepBoundary,
    /// Steer delivered only between turns.
    TurnBoundary,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub label: String,
    /// Short tagline rendered under the name in the model picker (11px muted),
    /// mirroring the Electron app's `ModelInfo.description`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub reasoning_levels: Vec<ReasoningLevel>,
    #[serde(default)]
    pub options: Vec<ModelOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub id: String,
    pub label: String,
    pub choices: Vec<ModelOptionChoice>,
    pub default_choice: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOptionChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRequest {
    pub prompt: String,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    /// Harness-specific option selections (option id -> choice id), JSON round-tripped.
    #[serde(default)]
    pub model_options: serde_json::Map<String, serde_json::Value>,
    pub cwd: String,
    pub sandbox: SandboxLevel,
    #[serde(default)]
    pub auto_approve: bool,
    /// Harness-native session id to resume, if any.
    pub resume: Option<String>,
    /// Absolute paths of image attachments already staged on the run device
    /// (composer uploads: UploadChunk/UploadCommit → durable path). The same
    /// paths also ride the prompt text as `Attached images (local files …)`
    /// refs (comet's `withAttachments` transport — that's what persists in the
    /// doc); this field additionally lets a harness inline the bytes as image
    /// content blocks. Additive + serde-defaulted for wire compat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<String>,
}

/// A decoded tool invocation, reduced to the fields each kind renders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ToolCall {
    Exec {
        command: String,
    },
    ReadFile {
        path: String,
    },
    WriteFile {
        path: String,
        /// Full content; STRIPPED by the render-parts policy before entering the doc.
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
    },
    EditFile {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        old_string: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        new_string: Option<String>,
    },
    ApplyPatch {
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Search {
        pattern: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Glob {
        pattern: String,
    },
    WebFetch {
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    WebSearch {
        query: String,
    },
    Todo {
        #[serde(default)]
        items: Vec<TodoItem>,
    },
    Mcp {
        server: String,
        tool: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
    /// A skill / slash command the agent invoked (gh#134).
    ///
    /// Split out of [`ToolCall::Unknown`] because it is not a tool in the sense
    /// the rest of this enum means: the others are *how* an agent works, and
    /// this is *which playbook it decided to follow* — the thing somebody
    /// scrolling a long session is looking for. Every viewport renders it as a
    /// landmark rather than a row in a tool group.
    ///
    /// A viewport too old to know this variant degrades it to an empty part
    /// (`from_doc_part` maps an undecodable call to blank text) rather than
    /// failing the doc — one missing row until it updates, never a corrupt
    /// transcript.
    Skill {
        /// Bare name, no leading slash — `comet-board`, `vercel:deploy`.
        name: String,
        /// Whatever rode the invocation, if anything. Kept (unlike the other
        /// free-form inputs the render policy strips) because it is the
        /// secondary line of the chip: `/comet-board list --state ready` says
        /// what happened and `/comet-board` does not.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
    },
    /// Work the agent handed to a SUBAGENT (Claude's `Task` / `Agent` tool).
    ///
    /// Split out of [`ToolCall::Unknown`] for the reason gh#280 exists: a
    /// subagent's own transcript is deliberately not folded into its parent's
    /// (it runs concurrently with the parent's text and would split it around
    /// phantom calls), so this row is the ONLY account the reader gets of work
    /// that can run for minutes. `Tool  Task` with the input stripped said
    /// nothing at all — not what was delegated, not that anything was
    /// happening.
    ///
    /// A viewport too old to know this variant degrades it to an empty part,
    /// the same way [`ToolCall::Skill`] does.
    Task {
        /// The short label the caller gave the delegated work.
        description: String,
        /// Which agent type was asked for (`Explore`, `general-purpose`, …),
        /// when the call named one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        subagent_type: Option<String>,
        /// Steps the subagent has taken so far — one per tool call it makes.
        ///
        /// Progress rather than input, and it lives ON the call because the
        /// call is the whole doc-resident record of the delegation: counting
        /// here reaches every viewport through the part map they already
        /// decode, and an old one just ignores a field it does not know.
        #[serde(default, skip_serializing_if = "is_zero")]
        steps: u32,
    },
    Unknown {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    pub text: String,
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<String>,
    #[serde(default)]
    pub multi_select: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInputAnswer {
    pub question_id: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DoneStatus {
    Completed,
    Interrupted,
    Errored,
}

/// The tokens a turn spent, in the four buckets a provider actually meters.
///
/// **Normalized on the Anthropic convention**, which is the one that lets the
/// four numbers be *added*: [`input_tokens`](Self::input_tokens) is the input
/// that was NOT served from cache, and the two cache figures are counted apart
/// from it. Codex reports the opposite shape — its `inputTokens` includes its
/// `cachedInputTokens` — so its normalizer subtracts before emitting, and a
/// test pins that, because a total that double-counts the cached half of a
/// long session is wrong by more than the rest of the page put together.
///
/// **Per turn, never cumulative.** Both harnesses emit exactly one
/// [`AgentEvent::Usage`] per turn: the claude CLI's result frame carries the
/// turn's own totals, and the codex app-server's repeated
/// `thread/tokenUsage/updated` snapshots are held and flushed once at
/// `turn/completed`. So summing the events in a run journal is a sum over
/// turns, and that is what the board records against an attempt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TokenUsage {
    /// Fresh input — prompt tokens the provider had to read, cache aside.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Input served out of the prompt cache.
    pub cache_read_tokens: u64,
    /// Input written *into* the prompt cache.
    pub cache_creation_tokens: u64,
}

impl TokenUsage {
    /// Every token the provider handled — the headline number. A plain sum,
    /// which is only sound because the buckets are disjoint by construction.
    pub fn total(&self) -> u64 {
        self.input_total() + self.output_tokens
    }

    /// All input, cached and not.
    pub fn input_total(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_creation_tokens
    }

    /// Nothing was spent. Distinct from "nothing was *reported*", which is an
    /// `Option<TokenUsage>` being `None` — see [`crate::view::stats`].
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }

    /// Saturating, because a corrupt frame must not wrap a running total.
    pub fn merged(self, other: Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_read_tokens: self
                .cache_read_tokens
                .saturating_add(other.cache_read_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_add(other.cache_creation_tokens),
        }
    }

    pub fn add(&mut self, other: Self) {
        *self = self.merged(other);
    }
}

impl std::iter::Sum for TokenUsage {
    fn sum<I: Iterator<Item = TokenUsage>>(iter: I) -> Self {
        iter.fold(Self::default(), Self::merged)
    }
}

/// Which part of a harness run spent a token (gh#426).
///
/// Kept separate from the agent's display name: `main` is a stable role, and
/// `Explore` / `research` are harness-owned labels that may change between
/// calls. A consumer can therefore group all delegated work without parsing a
/// string, while still showing which kind of delegate did it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AgentKind {
    Main,
    Subagent,
}

/// One agent/model slice of token usage reported by a harness (gh#426).
///
/// This is attribution metadata beside [`AgentEvent::Usage`], never a second
/// total. The result event remains the authority for how many tokens the turn
/// spent; assistant-message usage says which agent and model spent a slice of
/// it. Older journals simply contain no values of this type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentTokenUsage {
    pub agent: AgentKind,
    /// Harness-owned subagent type (`Explore`, `research`, …). `None` for the
    /// main agent and for a delegate whose launch frame could not be matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub model: String,
    pub usage: TokenUsage,
}

/// One model's exact share of a turn total (gh#426).
///
/// Claude's result frame reports this independently of per-message agent
/// attribution. Keeping the two facts apart lets the board price a mixed-model
/// attempt exactly even when an older CLI omitted agent detail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTokenUsage {
    pub model: String,
    pub usage: TokenUsage,
}

/// How full the agent's context window is right now (gh#271).
///
/// **A different number from [`TokenUsage`], and it must never be confused
/// with one.** Tokens spent are a flow — they add up over a run, and a long
/// session's total can be many times the window. Context fullness is a level:
/// it says how much of the window the *next* request will carry, which is what
/// predicts the moment the harness compacts away the context the agent is
/// working from (or, worse, has already started thrashing against). Two
/// attempts with identical spend can sit at 12% and 94%.
///
/// **A snapshot, never cumulative and never summed.** The last one reported
/// wins; adding two of these together is meaningless. That is why it rides its
/// own [`AgentEvent::ContextUsage`] instead of widening `TokenUsage`, whose
/// whole contract is that its four buckets add up (see gh#151).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContextUsage {
    /// Tokens occupying the window — system prompt, tools, memory files and
    /// the conversation so far, as the harness accounts for them.
    pub used_tokens: u64,
    /// The window they occupy. **Zero means the harness did not say**, and
    /// every derived share below is then `None` rather than a made-up
    /// denominator.
    pub max_tokens: u64,
    /// The level at which this harness auto-compacts, when it has one and
    /// names it (Claude's `autoCompactThreshold`). `None` is "no such point
    /// was reported" — auto-compaction off, or a harness that never had it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_at_tokens: Option<u64>,
}

impl ContextUsage {
    /// Share of the window in use, `0.0..=1.0`. `None` when no window was
    /// reported — see [`max_tokens`](Self::max_tokens).
    pub fn fraction(&self) -> Option<f64> {
        (self.max_tokens > 0).then(|| (self.used_tokens as f64 / self.max_tokens as f64).min(1.0))
    }

    /// The same share as a rounded percentage, `0..=100`.
    pub fn percent(&self) -> Option<u8> {
        self.fraction().map(|f| (f * 100.0).round() as u8)
    }

    /// Tokens left before the harness compacts, when it named a threshold.
    /// Saturating: already past it reads as `0`, not as a wrapped headroom.
    pub fn until_compact(&self) -> Option<u64> {
        self.compact_at_tokens
            .map(|at| at.saturating_sub(self.used_tokens))
    }

    /// Is this attempt at the point where compaction is imminent — past the
    /// harness's own threshold, or (absent one) past `ratio` of the window?
    ///
    /// The threshold is preferred because it is the harness's own answer;
    /// `ratio` is the fallback for a harness that meters fullness but does not
    /// compact on a number it will state.
    pub fn is_near_compaction(&self, ratio: f64) -> bool {
        match self.compact_at_tokens {
            Some(at) if at > 0 => self.used_tokens >= at,
            _ => self.fraction().is_some_and(|f| f >= ratio),
        }
    }

    /// Nothing was measured. Distinct from a *reported* empty window, which
    /// cannot happen: a window always holds at least a system prompt.
    pub fn is_zero(&self) -> bool {
        self.used_tokens == 0 && self.max_tokens == 0
    }
}

/// The normalized streaming event every harness emits.
///
/// Mirrors comet's `AgentEvent` tagged enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AgentEvent {
    #[serde(rename_all = "camelCase")]
    SessionStarted {
        harness: HarnessId,
        model: String,
        #[serde(default)]
        tools: Vec<String>,
        cwd: String,
        /// Harness-native session id (used for resume).
        session_id: String,
        assistant_message_id: String,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    /// Backend-internal steering boundary marker.
    #[serde(rename_all = "camelCase")]
    AssistantMessageCompleted {
        assistant_message_id: String,
    },
    ToolCall {
        id: String,
        call: ToolCall,
    },
    #[serde(rename_all = "camelCase")]
    ToolResult {
        id: String,
        is_error: bool,
    },
    /// One step taken by a subagent, attributed to the [`ToolCall::Task`] that
    /// launched it (gh#280).
    ///
    /// A subagent's frames are not folded into the parent transcript — they
    /// would split the parent's contiguous text around calls it never made —
    /// so without this the busiest minutes of a run produce no events at all,
    /// and working is indistinguishable from wedged. This is the countable
    /// half of the signal (the subagent's token stream contributes only empty
    /// [`AgentEvent::ReasoningDelta`] heartbeats); it folds to a step count on
    /// the Task part.
    #[serde(rename_all = "camelCase")]
    SubagentActivity {
        /// The parent's tool_use id — the id of the Task part to count against.
        parent_tool_use_id: String,
    },
    /// What one *turn* spent, in [`TokenUsage`]'s four buckets. A harness
    /// passthrough (never persisted into docs); the run journal keeps it, and
    /// the board sums it onto its attempt rows (gh#151).
    ///
    /// Per turn, not per run and not cumulative — see [`TokenUsage`] for why
    /// that is the one property both harnesses had to be settled on before
    /// anything could be added up.
    Usage(TokenUsage),
    /// An assistant step's usage with the agent and model that spent it
    /// (gh#426). Additive metadata beside the turn-level [`Usage`](Self::Usage)
    /// total; journal readers must not add the two together.
    AgentUsage(AgentTokenUsage),
    /// The turn total split by model, as the harness reported it (gh#426).
    /// Empty/absent on harnesses that expose only one undifferentiated total.
    #[serde(rename_all = "camelCase")]
    ModelUsage {
        models: Vec<ModelTokenUsage>,
    },
    /// How full the context window is at this moment (gh#271) — a level, not a
    /// flow. See [`ContextUsage`] for why it is not folded into `Usage`.
    ///
    /// Emitted repeatedly *within* a turn (Claude polls its control channel,
    /// Codex derives it from the token-usage snapshots it already streams) and
    /// **never after that turn's [`Done`](AgentEvent::Done)**: the run journal
    /// reads a trailing non-`Done` event as a run that died mid-stream, and an
    /// event that arrives late must not make a finished attempt look live.
    ContextUsage(ContextUsage),
    /// What sandbox this run actually got (gh#349) — emitted once, before
    /// [`SessionStarted`](AgentEvent::SessionStarted), by every harness.
    ///
    /// Its own variant rather than a field on `SessionStarted` for the reason
    /// [`ContextUsage`] has one: it is a different *kind* of fact. The session
    /// event says what the run is; this says what the run was permitted, and it
    /// is the only event here that can contradict the request the engine sent.
    /// A reader that wants it can tag-filter one line out of a journal that is
    /// otherwise megabytes of text deltas.
    Sandbox(SandboxReport),
    Error {
        message: String,
    },
    #[serde(rename_all = "camelCase")]
    InputRequested {
        request_id: String,
        questions: Vec<UserInputQuestion>,
    },
    #[serde(rename_all = "camelCase")]
    InputResolved {
        request_id: String,
    },
    #[serde(rename_all = "camelCase")]
    Steered {
        assistant_message_id: Option<String>,
        next_assistant_message_id: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Done {
        status: DoneStatus,
        result: Option<String>,
        error: Option<String>,
        session_id: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_event_round_trips() {
        let ev = AgentEvent::ToolCall {
            id: "t1".into(),
            call: ToolCall::Exec {
                command: "cargo test".into(),
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);
    }

    /// The report only speaks when a reviewer needs it to, and full access is
    /// worth saying even when nobody widened anything.
    #[test]
    fn a_sandbox_report_says_something_exactly_when_a_reviewer_needs_it() {
        // Got what it asked for, and what it asked for was confined. Silent.
        assert_eq!(
            SandboxReport::as_requested(SandboxLevel::WorkspaceWrite).note(),
            None
        );
        assert_eq!(
            SandboxReport::as_requested(SandboxLevel::ReadOnly).note(),
            None
        );

        // Full access, unwidened. Still said: "nobody widened this" is not
        // reassurance when the level was full access from the start.
        let note = SandboxReport::as_requested(SandboxLevel::DangerFullAccess)
            .note()
            .expect("full access is always worth saying");
        assert_eq!(note, "this agent had full access to the box");

        // Widened, with the reason folded in and the request named.
        let report = SandboxReport::widened(
            SandboxLevel::WorkspaceWrite,
            SandboxLevel::DangerFullAccess,
            "this codex predates the worktree-mount fix",
        );
        assert!(report.widened_beyond_request());
        let note = report.note().unwrap();
        assert!(
            note.starts_with("this agent had full access to the box"),
            "{note}"
        );
        assert!(note.contains("workspace-write was requested"), "{note}");
        assert!(note.contains("predates the worktree-mount fix"), "{note}");

        // Widening that stops short of full access still reads as a widening.
        let note = SandboxReport::widened(
            SandboxLevel::ReadOnly,
            SandboxLevel::WorkspaceWrite,
            "a title run needs its scratch dir",
        )
        .note()
        .unwrap();
        assert!(note.contains("ran at workspace-write"), "{note}");
        assert!(note.contains("read-only was requested"), "{note}");
    }

    /// A harness that applies *less* than was asked has a run that fails
    /// loudly; only the other direction is a quiet broken promise.
    #[test]
    fn only_widening_counts_as_widening() {
        let narrowed = SandboxReport {
            requested: SandboxLevel::DangerFullAccess,
            effective: SandboxLevel::ReadOnly,
            reason: Some("no".into()),
        };
        assert!(!narrowed.widened_beyond_request());
        assert!(
            SandboxReport::widened(
                SandboxLevel::ReadOnly,
                SandboxLevel::DangerFullAccess,
                "why"
            )
            .widened_beyond_request()
        );
    }

    /// The event is journaled, and the journal is scanned by tag before it is
    /// parsed — so the tag has to be what the reader greps for.
    #[test]
    fn the_sandbox_event_round_trips_under_the_tag_the_journal_filters_on() {
        let ev = AgentEvent::Sandbox(SandboxReport::widened(
            SandboxLevel::WorkspaceWrite,
            SandboxLevel::DangerFullAccess,
            "because",
        ));
        let json = serde_json::to_string(&ev).unwrap();
        assert!(json.contains(r#""type":"sandbox""#), "{json}");
        assert_eq!(serde_json::from_str::<AgentEvent>(&json).unwrap(), ev);

        // An unwidened report drops the empty reason rather than writing null.
        let json = serde_json::to_value(AgentEvent::Sandbox(SandboxReport::as_requested(
            SandboxLevel::WorkspaceWrite,
        )))
        .unwrap();
        assert!(json.get("reason").is_none(), "{json}");
        assert_eq!(json["requested"], "workspace-write");
        assert_eq!(json["effective"], "workspace-write");
    }

    #[test]
    fn run_request_attachments_default_and_round_trip() {
        // Old-wire JSON without the field parses (additive compat)…
        let old = r#"{"prompt":"p","model":null,"reasoning":null,"cwd":".","sandbox":"workspace-write","resume":null}"#;
        let req: RunRequest = serde_json::from_str(old).unwrap();
        assert!(req.attachments.is_empty());
        // …and an empty list serializes away (old readers never see it).
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("attachments").is_none());
        // Populated lists round-trip.
        let req = RunRequest {
            attachments: vec!["/tmp/a.png".into()],
            ..req
        };
        let round: RunRequest =
            serde_json::from_value(serde_json::to_value(&req).unwrap()).unwrap();
        assert_eq!(round.attachments, vec!["/tmp/a.png".to_string()]);
    }

    /// The event is journaled to disk, and journals outlive a release. A
    /// two-field `usage` line written before gh#151 must still parse — as
    /// zeroes in the two buckets nobody recorded, which is what "we did not
    /// know" looks like when it is added to a total.
    #[test]
    fn a_usage_line_written_before_the_cache_fields_existed_still_parses() {
        let old = r#"{"type":"usage","inputTokens":10,"outputTokens":20}"#;
        let ev: AgentEvent = serde_json::from_str(old).unwrap();
        assert_eq!(
            ev,
            AgentEvent::Usage(TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                ..Default::default()
            })
        );
        // And the widened event keeps the same flat wire shape, so a reader
        // that only knows the old two fields still finds them.
        let json = serde_json::to_value(AgentEvent::Usage(TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_creation_tokens: 4,
        }))
        .unwrap();
        assert_eq!(json["type"], "usage");
        assert_eq!(json["inputTokens"], 1);
        assert_eq!(json["cacheReadTokens"], 3);
        assert_eq!(json["cacheCreationTokens"], 4);
    }

    #[test]
    fn the_buckets_add_up_without_counting_the_cache_twice() {
        let a = TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
            cache_read_tokens: 900,
            cache_creation_tokens: 50,
        };
        assert_eq!(a.input_total(), 1_050);
        assert_eq!(a.total(), 1_060);
        assert!(!a.is_zero());
        assert!(TokenUsage::default().is_zero());
        // Summing turns is how a run's total is built.
        let run: TokenUsage = [a, a].into_iter().sum();
        assert_eq!(run.total(), 2_120);
        // Saturating, so one corrupt frame cannot wrap a running total.
        let huge = TokenUsage {
            input_tokens: u64::MAX,
            ..Default::default()
        };
        assert_eq!(huge.merged(a).input_tokens, u64::MAX);
    }

    /// Fullness is a level: the wire keeps it flat and additive-free, and a
    /// reader that predates the event simply does not know the variant.
    #[test]
    fn context_usage_rides_its_own_event_with_a_flat_shape() {
        let ev = AgentEvent::ContextUsage(ContextUsage {
            used_tokens: 120_000,
            max_tokens: 200_000,
            compact_at_tokens: Some(167_000),
        });
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "contextUsage");
        assert_eq!(json["usedTokens"], 120_000);
        assert_eq!(json["maxTokens"], 200_000);
        assert_eq!(json["compactAtTokens"], 167_000);
        assert_eq!(serde_json::from_value::<AgentEvent>(json).unwrap(), ev);
        // A harness that meters fullness but names no compaction point leaves
        // the field off the wire entirely.
        let json = serde_json::to_value(AgentEvent::ContextUsage(ContextUsage {
            used_tokens: 1,
            max_tokens: 2,
            compact_at_tokens: None,
        }))
        .unwrap();
        assert!(json.get("compactAtTokens").is_none());
    }

    /// A window nobody reported is not a full one and not an empty one: every
    /// share is `None`, because the alternative is inventing a denominator.
    #[test]
    fn an_unreported_window_yields_no_percentage_at_all() {
        let unknown = ContextUsage {
            used_tokens: 40_000,
            max_tokens: 0,
            compact_at_tokens: None,
        };
        assert_eq!(unknown.fraction(), None);
        assert_eq!(unknown.percent(), None);
        assert!(!unknown.is_near_compaction(0.8));
        assert!(!unknown.is_zero(), "used tokens were reported");
        assert!(ContextUsage::default().is_zero());
    }

    #[test]
    fn fullness_reads_the_harnesss_own_threshold_before_a_ratio() {
        let ctx = ContextUsage {
            used_tokens: 170_000,
            max_tokens: 200_000,
            compact_at_tokens: Some(167_000),
        };
        assert_eq!(ctx.percent(), Some(85));
        // Past the threshold the harness stated, though short of any 90% rule.
        assert!(ctx.is_near_compaction(0.9));
        // Saturating: past the point is zero headroom, never a wrapped one.
        assert_eq!(ctx.until_compact(), Some(0));
        let ctx = ContextUsage {
            compact_at_tokens: None,
            ..ctx
        };
        assert!(ctx.is_near_compaction(0.8));
        assert!(!ctx.is_near_compaction(0.9));
        assert_eq!(ctx.until_compact(), None);
        // Over-full (a harness reporting past its own window) clamps at 100%.
        assert_eq!(
            ContextUsage {
                used_tokens: 300_000,
                max_tokens: 200_000,
                compact_at_tokens: None,
            }
            .percent(),
            Some(100)
        );
    }

    #[test]
    fn harness_id_uses_kebab_case() {
        assert_eq!(
            serde_json::to_string(&HarnessId::ClaudeCode).unwrap(),
            "\"claude-code\""
        );
        assert_eq!(
            serde_json::to_string(&HarnessId::Opencode).unwrap(),
            "\"opencode\""
        );
    }
}
