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
    Unknown {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        input: Option<serde_json::Value>,
    },
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
            cache_read_tokens: self.cache_read_tokens.saturating_add(other.cache_read_tokens),
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
    /// What one *turn* spent, in [`TokenUsage`]'s four buckets. A harness
    /// passthrough (never persisted into docs); the run journal keeps it, and
    /// the board sums it onto its attempt rows (gh#151).
    ///
    /// Per turn, not per run and not cumulative — see [`TokenUsage`] for why
    /// that is the one property both harnesses had to be settled on before
    /// anything could be added up.
    Usage(TokenUsage),
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
