//! Frame → [`AgentEvent`] normalization, ported from claude.ts's `normalize`
//! (init dedupe, subagent filtering, tool decoding, error-code mapping).

use std::collections::{HashMap, HashSet};

use comet_proto::{
    AgentEvent, AgentKind, AgentTokenUsage, DoneStatus, HarnessId, ModelTokenUsage, StopReason,
    TodoItem, ToolCall,
};
use serde_json::Value;

use super::wire::{ContentBlock, Frame};

/// Human-readable text for the CLI's assistant-level error codes. These arrive
/// as a terse `error` field on an `assistant` frame — usually with NO text
/// content and NOT as a `result` error — so a usage-limited or otherwise failed
/// turn looks like the agent simply never replied unless we surface it.
fn assistant_error_text(code: &str) -> String {
    match code {
        "authentication_failed" => "Authentication failed — sign in to Claude again.".into(),
        "oauth_org_not_allowed" => "This organization isn't allowed to use Claude here.".into(),
        "billing_error" => "Billing error — check your Claude plan or payment method.".into(),
        "rate_limit" => "Claude usage limit reached — try again after the limit resets.".into(),
        "overloaded" => "Claude is overloaded right now — try again shortly.".into(),
        "invalid_request" => "The request was rejected as invalid.".into(),
        "model_not_found" => "The selected model isn't available.".into(),
        "server_error" => "Claude had a server error — try again.".into(),
        "max_output_tokens" => "The reply hit the maximum output length.".into(),
        "unknown" => "Claude returned an unspecified error.".into(),
        other => format!("Claude error: {other}"),
    }
}

/// Which of the CLI's assistant-level error codes is a *hard stop* (gh#545),
/// and what the board should call it.
///
/// A hard-stop code means the API step produced nothing and the turn cannot
/// go on — the class of failure whose transcript text used to be all there
/// was, and which the board then flattened into one dead-run sentence. The
/// classification rides the [`AgentEvent::Error`] itself so a reader can route
/// on it without parsing that sentence. `max_output_tokens` is deliberately
/// absent: its reply exists (up to the cap), so the turn did produce content
/// and must keep reading as a normal end.
fn hard_stop(code: &str) -> Option<StopReason> {
    match code {
        "rate_limit" => Some(StopReason::UsageLimit { window: None }),
        "billing_error" => Some(StopReason::Billing),
        "authentication_failed" | "oauth_org_not_allowed" => Some(StopReason::Auth),
        "overloaded" => Some(StopReason::Overloaded),
        "server_error" => Some(StopReason::Server),
        "invalid_request" | "model_not_found" | "unknown" => Some(StopReason::Other),
        _ => None,
    }
}

/// Which claude.ai usage window a `rate_limit_event` refers to.
fn rate_window_label(kind: &str) -> &'static str {
    match kind {
        "five_hour" => "5-hour",
        "seven_day" | "seven_day_overage_included" => "weekly",
        "seven_day_opus" => "weekly (Opus)",
        "seven_day_sonnet" => "weekly (Sonnet)",
        "overage" => "overage",
        _ => "usage",
    }
}

/// Fallback wording for a `result` error whose `errors` array is empty, so the
/// turn never ends with a blank (and therefore invisible) error.
fn result_error_text(subtype: &str) -> &'static str {
    match subtype {
        "error_max_turns" => "The run hit the maximum number of turns.",
        "error_max_budget_usd" => "The run hit its cost budget.",
        "error_max_structured_output_retries" => "The run exhausted its structured-output retries.",
        _ => "The run ended with an error.",
    }
}

/// The CLI seeds `result.errors` with internal `[ede_diagnostic]` breadcrumbs
/// for its error_during_execution telemetry ("turn aborted (…) stop_reason=…",
/// "result_type=… last_content_type=… stop_reason=…"). They're diagnostics
/// about the CLI's own turn accounting, not user-relevant errors — surfacing
/// them verbatim put raw `[ede_diagnostic] result_type=user …` boxes in the
/// transcript. They're debug-logged and dropped instead.
fn is_internal_diagnostic(message: &str) -> bool {
    message.contains("[ede_diagnostic]")
}

fn str_field(input: &Value, key: &str) -> String {
    input.get(key).and_then(Value::as_str).unwrap_or("").into()
}

fn opt_str_field(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Text between `<tag>` and `</tag>`, trimmed; `None` when the tag is absent.
fn tagged(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim().to_string())
}

/// The skill invocation an expanded slash command carries, if this text is one.
///
/// The CLI rewrites a `/name args` turn into `<command-name>` /
/// `<command-args>` markup before the model ever sees it, so the invocation is
/// legible on the stream without parsing the user's prose. Tolerant by
/// construction: anything that is not that markup returns `None` and the text
/// is left alone.
pub(crate) fn decode_command_markup(text: &str) -> Option<ToolCall> {
    let name = tagged(text, "command-name")?;
    let name = name.trim_start_matches('/').trim().to_string();
    if name.is_empty() {
        return None;
    }
    let args = tagged(text, "command-args").filter(|a| !a.is_empty());
    Some(ToolCall::Skill { name, args })
}

/// Decode a Claude `tool_use` block (name + input) into a typed [`ToolCall`].
pub(crate) fn decode_tool_use(name: &str, input: &Value) -> ToolCall {
    match name {
        // The Skill tool: `{skill, args}` in the CLI's own shape, with the
        // older `command`/`name` spellings accepted so a version skew costs a
        // plainer chip rather than an anonymous `Tool  Skill` row.
        "Skill" | "SlashCommand" => {
            let name = ["skill", "command", "name"]
                .into_iter()
                .find_map(|key| input.get(key).and_then(Value::as_str))
                .unwrap_or_default()
                .trim_start_matches('/')
                .trim()
                .to_string();
            let args = ["args", "arguments"]
                .into_iter()
                .find_map(|key| input.get(key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(str::to_owned);
            // A Skill call that names no skill is not a landmark — it is a
            // malformed tool call, and pretending otherwise would put a
            // nameless chip in the transcript.
            if name.is_empty() {
                return ToolCall::Unknown {
                    name: "Skill".into(),
                    input: (!input.is_null()).then(|| input.clone()),
                };
            }
            ToolCall::Skill { name, args }
        }
        "Bash" => ToolCall::Exec {
            command: str_field(input, "command"),
        },
        "Read" => ToolCall::ReadFile {
            path: str_field(input, "file_path"),
        },
        "Write" => ToolCall::WriteFile {
            path: str_field(input, "file_path"),
            content: opt_str_field(input, "content"),
        },
        "Edit" => ToolCall::EditFile {
            path: str_field(input, "file_path"),
            old_string: opt_str_field(input, "old_string"),
            new_string: opt_str_field(input, "new_string"),
        },
        "Grep" => ToolCall::Search {
            pattern: str_field(input, "pattern"),
            path: opt_str_field(input, "path"),
        },
        "Glob" => ToolCall::Glob {
            pattern: str_field(input, "pattern"),
        },
        "WebFetch" => ToolCall::WebFetch {
            url: str_field(input, "url"),
            prompt: opt_str_field(input, "prompt"),
        },
        "WebSearch" => ToolCall::WebSearch {
            query: str_field(input, "query"),
        },
        // Delegation. `Task` is the long-standing spelling; `Agent` is the
        // newer one — same tool, and a version skew must not cost the row its
        // identity (an anonymous `Tool  Agent` is exactly the blind spot
        // gh#280 is about). The `prompt` is deliberately not decoded: it is
        // the whole brief, often thousands of words, and the description is
        // what a chip can say.
        "Task" | "Agent" => ToolCall::Task {
            description: str_field(input, "description"),
            subagent_type: ["subagent_type", "agentType"]
                .into_iter()
                .find_map(|key| input.get(key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned),
            steps: 0,
        },
        "TodoWrite" => ToolCall::Todo {
            items: input
                .get("todos")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|t| TodoItem {
                    text: str_field(t, "content"),
                    done: t.get("status").and_then(Value::as_str) == Some("completed"),
                })
                .collect(),
        },
        // MCP tools arrive as `mcp__<server>__<tool>`.
        _ => match name.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
            Some((server, tool)) => ToolCall::Mcp {
                server: server.into(),
                tool: tool.into(),
                input: (!input.is_null()).then(|| input.clone()),
            },
            None => ToolCall::Unknown {
                name: name.into(),
                input: (!input.is_null()).then(|| input.clone()),
            },
        },
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Per-run normalization state.
///
/// `saw_init` dedupes `system:init` — the CLI re-emits it every time the model
/// is re-invoked WITHIN one session (a background-task notification, a
/// scheduled wakeup), not just at start. Downstream, `SessionStarted` is the
/// fold's run boundary (it resets accumulated parts), so one run ⇒ one
/// `SessionStarted`.
pub(crate) struct Normalizer {
    saw_init: bool,
    /// Rotates at each assistant-frame close and at each steer; SessionStarted
    /// carries the first value so folds can attribute deltas from the start.
    assistant_message_id: String,
    /// Last session id seen (init or result) — used for synthetic Dones.
    pub session_id: Option<String>,
    /// Counter behind the synthetic ids expanded slash commands get. Monotonic
    /// per run rather than random so a replayed stream folds to the same parts
    /// (`fold_event_into_parts` keys tool parts on the id).
    commands_seen: u64,
    /// A complete assistant message may be repeated for every tool in one API
    /// step. Its usage is the step's usage, so the stable message id is the
    /// dedupe key (Claude Agent SDK cost-tracking contract).
    usage_messages: HashSet<String>,
    /// Parent Task/Agent tool id -> harness-owned subagent type. The nested
    /// assistant frames carry only the id, so the launch frame is where the
    /// human name has to be remembered.
    subagents: HashMap<String, Option<String>>,
    /// A hard stop seen since the last real content (gh#545). The CLI's
    /// failed turns usually end with a `result` that still says `success` —
    /// the stream completed, the turn did not — so the stop is held here and
    /// folded into the run's `Done`, which is what flips a usage-limited
    /// turn from reading as a clean end to reading as the error it was.
    /// Cleared by any later assistant content (the CLI retried and got
    /// through) and by every `Done`.
    pending_stop: Option<StopReason>,
}

impl Normalizer {
    pub fn new() -> Self {
        Self {
            saw_init: false,
            assistant_message_id: new_message_id(),
            session_id: None,
            commands_seen: 0,
            usage_messages: HashSet::new(),
            subagents: HashMap::new(),
            pending_stop: None,
        }
    }

    fn agent_usage(&mut self, frame: &super::wire::MessageFrame) -> Option<AgentEvent> {
        let message = &frame.message;
        let usage = message.usage.normalized();
        if message.id.is_empty()
            || message.model.is_empty()
            || usage.is_zero()
            || !self.usage_messages.insert(message.id.clone())
        {
            return None;
        }
        let (agent, name) = match frame.parent_tool_use_id.as_deref() {
            Some(parent) => (
                AgentKind::Subagent,
                self.subagents.get(parent).cloned().flatten(),
            ),
            None => (AgentKind::Main, None),
        };
        Some(AgentEvent::AgentUsage(AgentTokenUsage {
            agent,
            name,
            model: message.model.clone(),
            usage,
        }))
    }

    fn next_command_id(&mut self) -> String {
        self.commands_seen += 1;
        format!("cmd-{}", self.commands_seen)
    }

    /// Rotate the assistant message id for a steer boundary; returns
    /// (previous, next) for the `Steered` event.
    pub fn rotate_for_steer(&mut self) -> (String, String) {
        let prev = std::mem::replace(&mut self.assistant_message_id, new_message_id());
        (prev, self.assistant_message_id.clone())
    }

    /// Normalize one stdout frame into 0+ unified events. `interrupted` folds
    /// a post-interrupt `result` into `Done { status: Interrupted }`.
    pub fn normalize(&mut self, frame: Frame, interrupted: bool) -> Vec<AgentEvent> {
        match frame {
            Frame::System(f) => {
                if f.subtype != "init" || self.saw_init {
                    return Vec::new();
                }
                self.saw_init = true;
                self.session_id = Some(f.session_id.clone());
                // A new run: any stop a previous turn ended on says nothing
                // about this one.
                self.pending_stop = None;
                vec![AgentEvent::SessionStarted {
                    harness: HarnessId::ClaudeCode,
                    model: f.model,
                    tools: f.tools,
                    cwd: f.cwd,
                    session_id: f.session_id,
                    assistant_message_id: self.assistant_message_id.clone(),
                }]
            }

            // Frames with `parent_tool_use_id` set belong to a SUBAGENT's
            // nested transcript; a background Task runs concurrently with the
            // parent's text stream, so folding them in would split a contiguous
            // text block around a phantom tool call. Only null-parent frames
            // are this turn's own content.
            //
            // They are not, however, nothing. Dropping them outright made the
            // busiest stretch of a run — an Explore subagent working for
            // minutes across dozens of tool calls — emit not one event, so the
            // chat could not be told apart from a stopped one (gh#280). Each
            // kind of subagent frame is answered below with the smallest
            // signal that keeps the parent transcript intact: the token stream
            // becomes pure liveness, and the discrete steps get counted.
            Frame::StreamEvent(f) => {
                // The subagent's own tokens. Nothing here belongs in the
                // parent's text, but "characters are arriving from somewhere"
                // is exactly what an empty reasoning delta means — the engine
                // takes those as liveness and never journals or renders them.
                if f.parent_tool_use_id.is_some() {
                    return vec![AgentEvent::ReasoningDelta {
                        text: String::new(),
                    }];
                }
                if f.event.kind != "content_block_delta" {
                    return Vec::new();
                }
                match f.event.delta.kind.as_str() {
                    "text_delta" => vec![AgentEvent::TextDelta {
                        text: f.event.delta.text,
                    }],
                    "thinking_delta" => vec![AgentEvent::ReasoningDelta {
                        text: f.event.delta.thinking,
                    }],
                    // A big tool input (a 90-line Write) streams as a long run
                    // of input_json_delta frames with nothing else — minutes of
                    // apparent silence that reads as a stalled run. Surface
                    // them as empty reasoning deltas: the engine treats those
                    // as pure liveness heartbeats (never journaled/rendered).
                    "input_json_delta" => vec![AgentEvent::ReasoningDelta {
                        text: String::new(),
                    }],
                    _ => Vec::new(),
                }
            }

            Frame::Assistant(f) => {
                let attributed = self.agent_usage(&f);
                // A subagent's assistant frame: one step of delegated work.
                // Its tool calls are the countable unit — they are what the
                // subagent spends its minutes on — so each becomes an activity
                // event against the Task row that launched it. A frame that
                // only carries text (the subagent's final answer to its
                // parent) still beats as liveness.
                if let Some(parent) = f.parent_tool_use_id {
                    let mut steps = Vec::new();
                    for block in f
                        .message
                        .blocks()
                        .filter(|b: &ContentBlock| b.kind == "tool_use")
                    {
                        // Delegates may delegate again. Their calls stay out
                        // of the parent transcript, but the launch id is what
                        // lets nested usage say `Plan`, not only `Subagent`.
                        if let ToolCall::Task { subagent_type, .. } =
                            decode_tool_use(&block.name, &block.input)
                        {
                            self.subagents.insert(block.id, subagent_type);
                        }
                        steps.push(AgentEvent::SubagentActivity {
                            parent_tool_use_id: parent.clone(),
                        });
                    }
                    if steps.is_empty() {
                        return attributed
                            .into_iter()
                            .chain([AgentEvent::ReasoningDelta {
                                text: String::new(),
                            }])
                            .collect();
                    }
                    return attributed.into_iter().chain(steps).collect();
                }
                let mut out: Vec<AgentEvent> = attributed.into_iter().collect();
                // Any block — streamed text included — is the model having
                // gotten through, which is what clears a held stop below.
                let content = f.message.blocks().next().is_some();
                for block in f
                    .message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_use")
                {
                    let call = decode_tool_use(&block.name, &block.input);
                    if let ToolCall::Task { subagent_type, .. } = &call {
                        self.subagents
                            .insert(block.id.clone(), subagent_type.clone());
                    }
                    out.push(AgentEvent::ToolCall { id: block.id, call });
                }
                // A failed turn (usage limit, billing, auth, overloaded, …)
                // carries a terse `error` code here — often with empty content
                // and no `result` error — so surface it visibly, classified
                // for the board (gh#545). The classification is also *held*:
                // the result frame usually still says success, and it must
                // not get the last word on how this run ended.
                match &f.error {
                    Some(code) => {
                        let stop = hard_stop(code);
                        out.push(AgentEvent::Error {
                            message: assistant_error_text(code),
                            stop,
                        });
                        self.pending_stop = hard_stop(code);
                    }
                    // Content after a held stop is the CLI's own retry having
                    // gotten through — the run recovered, so the stop no
                    // longer describes its ending.
                    None if content => self.pending_stop = None,
                    None => {}
                }
                // The enclosing assistant frame closes the streamed message
                // item; rotate so post-boundary deltas get a fresh id.
                let (prev, _next) = self.rotate_for_steer();
                out.push(AgentEvent::AssistantMessageCompleted {
                    assistant_message_id: prev,
                });
                out
            }

            Frame::User(f) => {
                // A subagent's tool results. Liveness only — the call they
                // answer was already counted as the step, and counting both
                // ends would double every subagent's progress.
                if f.parent_tool_use_id.is_some() {
                    return vec![AgentEvent::ReasoningDelta {
                        text: String::new(),
                    }];
                }
                let mut out: Vec<AgentEvent> = Vec::new();
                for block in f.message.blocks() {
                    match block.kind.as_str() {
                        "tool_result" => out.push(AgentEvent::ToolResult {
                            id: block.tool_use_id.clone(),
                            is_error: block.is_error.unwrap_or(false),
                        }),
                        // A slash command the CLI expanded on its way in. It
                        // is an invocation with no tool_use id and no result
                        // of its own, so it gets a synthetic id and is
                        // resolved on the spot — an eternally-pending chip
                        // would read as a skill that never returned.
                        "text" => {
                            if let Some(call) = decode_command_markup(&block.text) {
                                let id = self.next_command_id();
                                out.push(AgentEvent::ToolCall {
                                    id: id.clone(),
                                    call,
                                });
                                out.push(AgentEvent::ToolResult {
                                    id,
                                    is_error: false,
                                });
                            }
                        }
                        _ => {}
                    }
                }
                out
            }

            // A claude.ai plan window was hit. A hard `rejected` blocks the
            // turn — make it visible, classified, and held for the result
            // frame (gh#545); allowed/allowed_warning stay quiet.
            Frame::RateLimit(f) => {
                if f.rate_limit_info.status != "rejected" {
                    return Vec::new();
                }
                let window =
                    rate_window_label(f.rate_limit_info.rate_limit_type.as_deref().unwrap_or(""));
                let stop = StopReason::UsageLimit {
                    window: Some(window.into()),
                };
                self.pending_stop = Some(stop.clone());
                vec![AgentEvent::Error {
                    message: format!(
                        "Claude {window} limit reached — the turn was blocked. Try again after it resets."
                    ),
                    stop: Some(stop),
                }]
            }

            Frame::Result(f) => {
                if let Some(id) = &f.session_id {
                    self.session_id = Some(id.clone());
                }
                // Straight across: the CLI already reports the four buckets in
                // the shape `TokenUsage` normalizes on (uncached input apart
                // from the two cache figures), so nothing is derived here.
                let usage = AgentEvent::Usage(f.usage.normalized());
                let model_usage = (!f.model_usage.is_empty()).then(|| AgentEvent::ModelUsage {
                    models: f
                        .model_usage
                        .iter()
                        .map(|(model, usage)| ModelTokenUsage {
                            model: model.clone(),
                            usage: usage.normalized(),
                        })
                        .collect(),
                });
                let done = if f.subtype == "success" {
                    // gh#545: the result frame's `success` means the *stream*
                    // completed. When a hard stop was held since the last
                    // real content, the turn itself failed — the CLI just
                    // does not say so — and the run must end the way the
                    // board can see it: `Errored`, not a clean end that
                    // leaves the attempt reading idle. The error text stays
                    // off the Done; the Error event already put it in the
                    // transcript, and a second box would say it twice.
                    let stopped = !interrupted && self.pending_stop.take().is_some();
                    AgentEvent::Done {
                        status: if interrupted {
                            DoneStatus::Interrupted
                        } else if stopped {
                            DoneStatus::Errored
                        } else {
                            DoneStatus::Completed
                        },
                        result: f.result,
                        error: None,
                        session_id: f.session_id,
                    }
                } else {
                    // Split the CLI's internal `[ede_diagnostic]` breadcrumbs
                    // off the real errors: diagnostics are debug-logged, never
                    // surfaced as transcript error parts.
                    let (diagnostics, errors): (Vec<String>, Vec<String>) = f
                        .errors
                        .iter()
                        .map(|e| match e {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .partition(|m| is_internal_diagnostic(m));
                    for diagnostic in &diagnostics {
                        tracing::debug!(
                            target: "comet_harness::claude",
                            "internal CLI diagnostic (not surfaced): {diagnostic}"
                        );
                    }
                    let error = if !errors.is_empty() {
                        // Real user-relevant errors — surface verbatim.
                        Some(errors.join("; "))
                    } else {
                        match f.subtype.as_str() {
                            // Known run-failure subtypes stay visible with
                            // their mapped human wording (never blank — a
                            // blank error folds to no part and the failed
                            // turn reads as a silent non-reply).
                            "error_max_turns"
                            | "error_max_budget_usd"
                            | "error_max_structured_output_retries" => {
                                Some(result_error_text(&f.subtype).to_owned())
                            }
                            // Diagnostic-only ends (the CLI's turn-accounting
                            // telemetry, typically `error_during_execution`
                            // after an abort): nothing user-relevant to show.
                            _ if !diagnostics.is_empty() => None,
                            _ => Some(result_error_text(&f.subtype).to_owned()),
                        }
                    };
                    AgentEvent::Done {
                        status: if interrupted {
                            DoneStatus::Interrupted
                        } else {
                            DoneStatus::Errored
                        },
                        result: None,
                        error,
                        session_id: f.session_id,
                    }
                };
                // The turn is over either way; a held stop must not outlive
                // it into the next one.
                self.pending_stop = None;
                model_usage.into_iter().chain([usage, done]).collect()
            }

            // Control frames are handled by the run loop, not normalized: it
            // owns the request ids it issued, and a reply is only meaningful
            // beside the question (gh#271).
            Frame::ControlRequest(_) | Frame::ControlResponse(_) | Frame::Other => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_typed_tools() {
        assert_eq!(
            decode_tool_use("Bash", &json!({"command": "ls -la"})),
            ToolCall::Exec {
                command: "ls -la".into()
            }
        );
        assert_eq!(
            decode_tool_use(
                "Edit",
                &json!({"file_path": "/a", "old_string": "x", "new_string": "y"})
            ),
            ToolCall::EditFile {
                path: "/a".into(),
                old_string: Some("x".into()),
                new_string: Some("y".into())
            }
        );
        assert_eq!(
            decode_tool_use(
                "TodoWrite",
                &json!({"todos": [{"content": "t", "status": "completed"}]})
            ),
            ToolCall::Todo {
                items: vec![TodoItem {
                    text: "t".into(),
                    done: true
                }]
            }
        );
        assert_eq!(
            decode_tool_use("mcp__linear__search", &json!({"q": "bug"})),
            ToolCall::Mcp {
                server: "linear".into(),
                tool: "search".into(),
                input: Some(json!({"q": "bug"}))
            }
        );
        assert!(matches!(
            decode_tool_use("Mystery", &json!({})),
            ToolCall::Unknown { .. }
        ));
    }

    #[test]
    fn skill_calls_decode_to_a_named_landmark() {
        assert_eq!(
            decode_tool_use("Skill", &json!({"skill": "comet-board", "args": "list"})),
            ToolCall::Skill {
                name: "comet-board".into(),
                args: Some("list".into())
            }
        );
        // A leading slash is how a human writes it; the wire form has none.
        assert_eq!(
            decode_tool_use("SlashCommand", &json!({"command": "/comet-board"})),
            ToolCall::Skill {
                name: "comet-board".into(),
                args: None
            }
        );
        // Blank args are no args — the chip must not read `/comet-board  `.
        assert_eq!(
            decode_tool_use("Skill", &json!({"skill": "debug", "args": "   "})),
            ToolCall::Skill {
                name: "debug".into(),
                args: None
            }
        );
        // Nameless: stays an unknown tool rather than becoming a blank chip.
        assert!(matches!(
            decode_tool_use("Skill", &json!({"args": "x"})),
            ToolCall::Unknown { .. }
        ));
    }

    #[test]
    fn expanded_slash_commands_become_resolved_invocations() {
        let frame = crate::claude::wire::parse_frame(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<command-message>comet-board is running…</command-message>\n<command-name>/comet-board</command-name>\n<command-args>list --state ready</command-args>"}]}}"#,
        )
        .expect("frame parses");
        let events = Normalizer::new().normalize(frame, false);
        assert_eq!(
            events,
            vec![
                AgentEvent::ToolCall {
                    id: "cmd-1".into(),
                    call: ToolCall::Skill {
                        name: "comet-board".into(),
                        args: Some("list --state ready".into())
                    }
                },
                // Resolved immediately: the markup is the whole record of the
                // invocation, so there is no later result to wait for.
                AgentEvent::ToolResult {
                    id: "cmd-1".into(),
                    is_error: false
                },
            ]
        );

        // Ordinary user prose is left entirely alone.
        let frame = crate::claude::wire::parse_frame(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"look at src/main.rs"}]}}"#,
        )
        .expect("frame parses");
        assert!(Normalizer::new().normalize(frame, false).is_empty());
    }

    #[test]
    fn command_markup_parses_tolerantly() {
        // Args are optional.
        assert_eq!(
            decode_command_markup("<command-name>/debug</command-name>"),
            Some(ToolCall::Skill {
                name: "debug".into(),
                args: None
            })
        );
        // Empty args read as none.
        assert_eq!(
            decode_command_markup(
                "<command-name>debug</command-name><command-args></command-args>"
            ),
            Some(ToolCall::Skill {
                name: "debug".into(),
                args: None
            })
        );
        // Anything that is not the markup is not an invocation.
        assert_eq!(decode_command_markup("just some prose"), None);
        assert_eq!(decode_command_markup("<command-name></command-name>"), None);
        assert_eq!(decode_command_markup("<command-name>/debug"), None);
    }

    fn result_done(raw: &str) -> AgentEvent {
        let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
        let events = Normalizer::new().normalize(frame, false);
        assert_eq!(events.len(), 2, "usage + done");
        events.into_iter().nth(1).expect("done event")
    }

    #[test]
    fn stream_deltas_map_to_text_reasoning_and_heartbeats() {
        let normalize = |raw: &str| {
            let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
            Normalizer::new().normalize(frame, false)
        };
        // Real thinking text streams as a reasoning delta.
        let ev = normalize(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#,
        );
        assert_eq!(ev, vec![AgentEvent::ReasoningDelta { text: "hmm".into() }]);
        // Redacted thinking (estimated_tokens only) yields the empty
        // heartbeat shape the engine filters.
        let ev = normalize(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"","estimated_tokens":50}}}"#,
        );
        assert_eq!(
            ev,
            vec![AgentEvent::ReasoningDelta {
                text: String::new()
            }]
        );
        // A tool input being generated (input_json_delta) is a liveness
        // heartbeat, not silence — minutes of a big Write must not read as
        // a stalled run.
        let ev = normalize(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"file_"}}}"#,
        );
        assert_eq!(
            ev,
            vec![AgentEvent::ReasoningDelta {
                text: String::new()
            }]
        );
        // Signature deltas stay dropped.
        let ev = normalize(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"abc"}}}"#,
        );
        assert!(ev.is_empty());
    }

    #[test]
    fn delegation_decodes_to_a_named_task() {
        assert_eq!(
            decode_tool_use(
                "Task",
                &json!({
                    "description": "find the normalizer",
                    "subagent_type": "Explore",
                    "prompt": "a very long brief…"
                })
            ),
            ToolCall::Task {
                description: "find the normalizer".into(),
                subagent_type: Some("Explore".into()),
                steps: 0
            }
        );
        // The newer spelling of the same tool.
        assert!(matches!(
            decode_tool_use("Agent", &json!({"description": "x"})),
            ToolCall::Task { .. }
        ));
        // No agent named ⇒ no agent shown (never a blank ` · ` in the chip).
        assert_eq!(
            decode_tool_use("Task", &json!({"description": "x", "subagent_type": " "})),
            ToolCall::Task {
                description: "x".into(),
                subagent_type: None,
                steps: 0
            }
        );
    }

    #[test]
    fn complete_messages_are_attributed_once_to_the_agent_and_model_that_ran_them() {
        let mut normalizer = Normalizer::new();
        let main = crate::claude::wire::parse_frame(
            r#"{"type":"assistant","message":{"id":"msg-main","model":"claude-opus-5","usage":{"input_tokens":10,"output_tokens":2},"content":[{"type":"tool_use","id":"task-1","name":"Task","input":{"description":"research","subagent_type":"Explore"}}]}}"#,
        )
        .expect("main frame parses");
        let main_events = normalizer.normalize(main, false);
        assert!(
            main_events.contains(&AgentEvent::AgentUsage(AgentTokenUsage {
                agent: AgentKind::Main,
                name: None,
                model: "claude-opus-5".into(),
                usage: comet_proto::TokenUsage {
                    input_tokens: 10,
                    output_tokens: 2,
                    ..Default::default()
                },
            }))
        );

        let subagent_raw = r#"{"type":"assistant","parent_tool_use_id":"task-1","message":{"id":"msg-sub","model":"claude-haiku-4-5","usage":{"input_tokens":20,"output_tokens":4},"content":[{"type":"tool_use","id":"task-2","name":"Task","input":{"description":"plan","subagent_type":"Plan"}}]}}"#;
        let subagent = crate::claude::wire::parse_frame(subagent_raw).expect("subagent parses");
        let subagent_events = normalizer.normalize(subagent, false);
        assert!(
            subagent_events.contains(&AgentEvent::AgentUsage(AgentTokenUsage {
                agent: AgentKind::Subagent,
                name: Some("Explore".into()),
                model: "claude-haiku-4-5".into(),
                usage: comet_proto::TokenUsage {
                    input_tokens: 20,
                    output_tokens: 4,
                    ..Default::default()
                },
            }))
        );

        let nested = crate::claude::wire::parse_frame(
            r#"{"type":"assistant","parent_tool_use_id":"task-2","message":{"id":"msg-nested","model":"claude-sonnet-5","usage":{"input_tokens":30,"output_tokens":6},"content":[{"type":"text","text":"planned"}]}}"#,
        )
        .expect("nested subagent parses");
        assert!(
            normalizer
                .normalize(nested, false)
                .contains(&AgentEvent::AgentUsage(AgentTokenUsage {
                    agent: AgentKind::Subagent,
                    name: Some("Plan".into()),
                    model: "claude-sonnet-5".into(),
                    usage: comet_proto::TokenUsage {
                        input_tokens: 30,
                        output_tokens: 6,
                        ..Default::default()
                    },
                }))
        );

        // Claude may repeat a complete assistant frame for one API step. Its
        // stable message id makes that one charge, not two.
        let repeated = crate::claude::wire::parse_frame(subagent_raw).expect("repeat parses");
        assert!(
            normalizer
                .normalize(repeated, false)
                .iter()
                .all(|event| !matches!(event, AgentEvent::AgentUsage(_)))
        );
    }

    #[test]
    fn subagent_frames_are_liveness_and_counted_steps() {
        let normalize = |raw: &str| {
            let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
            Normalizer::new().normalize(frame, false)
        };
        let beat = vec![AgentEvent::ReasoningDelta {
            text: String::new(),
        }];

        // The subagent's token stream: liveness, never parent transcript text.
        assert_eq!(
            normalize(
                r#"{"type":"stream_event","parent_tool_use_id":"toolu_1","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"subagent prose"}}}"#,
            ),
            beat
        );
        // Its tool calls are the countable steps, one event each, attributed
        // to the Task row that launched it.
        assert_eq!(
            normalize(
                r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"content":[
                    {"type":"tool_use","id":"s1","name":"Read","input":{"file_path":"/a"}},
                    {"type":"tool_use","id":"s2","name":"Grep","input":{"pattern":"x"}}]}}"#,
            ),
            vec![
                AgentEvent::SubagentActivity {
                    parent_tool_use_id: "toolu_1".into()
                },
                AgentEvent::SubagentActivity {
                    parent_tool_use_id: "toolu_1".into()
                },
            ]
        );
        // A text-only subagent message is progress, not a step.
        assert_eq!(
            normalize(
                r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"content":[{"type":"text","text":"here is what I found"}]}}"#,
            ),
            beat
        );
        // Its tool RESULTS beat but do not count — the call was the step.
        assert_eq!(
            normalize(
                r#"{"type":"user","parent_tool_use_id":"toolu_1","message":{"content":[{"type":"tool_result","tool_use_id":"s1"}]}}"#,
            ),
            beat
        );
        // And none of it disturbs the parent's own message accounting: no
        // AssistantMessageCompleted, no ToolCall, no ToolResult above.
    }

    #[test]
    fn ede_diagnostics_never_surface_as_errors() {
        // The CLI's internal turn-accounting breadcrumbs must not become
        // transcript error parts (they showed up as raw red boxes).
        let done = result_done(
            r#"{"type":"result","subtype":"error_during_execution","errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"]}"#,
        );
        match done {
            AgentEvent::Done { status, error, .. } => {
                assert_eq!(status, DoneStatus::Errored);
                assert_eq!(error, None, "diagnostic-only failure surfaces no text");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn real_errors_survive_diagnostic_filtering() {
        let done = result_done(
            r#"{"type":"result","subtype":"error_during_execution","errors":["[ede_diagnostic] turn aborted (x) stop_reason=null","Something real broke"]}"#,
        );
        match done {
            AgentEvent::Done { error, .. } => {
                assert_eq!(error.as_deref(), Some("Something real broke"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn known_failure_subtypes_keep_mapped_wording() {
        // A known run-failure subtype stays visible with human wording even
        // when its errors array is all diagnostics (or empty).
        let done = result_done(
            r#"{"type":"result","subtype":"error_max_turns","errors":["[ede_diagnostic] turn aborted (max) stop_reason=null"]}"#,
        );
        match done {
            AgentEvent::Done { error, .. } => {
                assert_eq!(
                    error.as_deref(),
                    Some("The run hit the maximum number of turns.")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let done = result_done(r#"{"type":"result","subtype":"error_max_turns","errors":[]}"#);
        match done {
            AgentEvent::Done { error, .. } => {
                assert_eq!(
                    error.as_deref(),
                    Some("The run hit the maximum number of turns.")
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    // ---- classified hard stops (gh#545) -----------------------------------

    const RATE_LIMITED_TURN: &[&str] = &[
        // The failed step: an assistant frame with a terse error code, no
        // content, and — this is the trap — a `result` that still says
        // `success` afterwards.
        r#"{"type":"assistant","message":{"id":"msg-1","model":"claude-opus-5","content":[]},"error":"rate_limit"}"#,
        r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":0}}"#,
    ];

    fn normalize_all(raws: &[&str], interrupted: bool) -> Vec<AgentEvent> {
        let mut n = Normalizer::new();
        let mut out = Vec::new();
        for raw in raws {
            let frame = crate::claude::wire::parse_frame(raw).expect("frame parses");
            out.extend(n.normalize(frame, interrupted));
        }
        out
    }

    /// The shape gh#545 is about: the turn ends `success` after the error
    /// frame. The transcript keeps its one error box (classified for the
    /// board), and the run ends **Errored** — never a clean end that leaves
    /// a usage-limited attempt reading idle.
    #[test]
    fn a_rate_limited_turn_does_not_end_clean() {
        let events = normalize_all(RATE_LIMITED_TURN, false);
        let error = events
            .iter()
            .find(|e| matches!(e, AgentEvent::Error { .. }))
            .expect("the failure is surfaced");
        assert!(
            matches!(
                error,
                AgentEvent::Error {
                    stop: Some(StopReason::UsageLimit { window: None }),
                    ..
                }
            ),
            "classified as a usage limit: {error:?}"
        );
        match events.last().expect("done") {
            AgentEvent::Done {
                status: DoneStatus::Errored,
                ..
            } => {}
            other => panic!("a limited run must end errored: {other:?}"),
        }
    }

    /// The CLI's own retry getting through clears the held stop: content
    /// after the error means the run recovered, and it ends clean.
    #[test]
    fn a_recovered_step_clears_the_held_stop() {
        let events = normalize_all(
            &[
                RATE_LIMITED_TURN[0],
                r#"{"type":"assistant","message":{"id":"msg-2","model":"claude-opus-5","content":[{"type":"text","text":"back online"}]}}"#,
                r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":1}}"#,
            ],
            false,
        );
        // (The recovered step's own content streams via deltas and usage, not
        // as a parent TextDelta — what matters here is how the run *ended*.)
        match events.last().expect("done") {
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            } => {}
            other => panic!("a recovered run ends completed: {other:?}"),
        }
    }

    /// A hard `rate_limit_event` names the window; the classification keeps
    /// it, so the board can say *which* wall was hit.
    #[test]
    fn a_rejected_plan_window_is_classified_with_its_window() {
        let events = normalize_all(
            &[
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour"}}"#,
                r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":0}}"#,
            ],
            false,
        );
        assert!(matches!(
            events
                .iter()
                .find(|e| matches!(e, AgentEvent::Error { .. }))
                .expect("surfaced"),
            AgentEvent::Error {
                stop: Some(StopReason::UsageLimit {
                    window: Some(w)
                }),
                ..
            } if w == "5-hour"
        ));
        match events.last().expect("done") {
            AgentEvent::Done {
                status: DoneStatus::Errored,
                ..
            } => {}
            other => panic!("a blocked window must end the run errored: {other:?}"),
        }
    }

    /// An interrupt is still an interrupt: the human chose to stop, and the
    /// board's Interrupted handling outranks a stop that happened to precede
    /// it.
    #[test]
    fn an_interrupted_run_stays_interrupted_even_after_a_hard_stop() {
        let events = normalize_all(RATE_LIMITED_TURN, true);
        match events.last().expect("done") {
            AgentEvent::Done {
                status: DoneStatus::Interrupted,
                ..
            } => {}
            other => panic!("{other:?}"),
        }
    }

    /// A reply capped at its output limit produced content — the turn worked.
    /// It is not a hard stop, and must not flip a success end.
    #[test]
    fn max_output_tokens_is_not_a_hard_stop() {
        let events = normalize_all(
            &[
                r#"{"type":"assistant","message":{"id":"msg-1","model":"claude-opus-5","content":[{"type":"text","text":"half a repl"}]},"error":"max_output_tokens"}"#,
                r#"{"type":"result","subtype":"success","usage":{"input_tokens":1,"output_tokens":64}}"#,
            ],
            false,
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Error { .. })),
            "still surfaced in the transcript"
        );
        match events.last().expect("done") {
            AgentEvent::Done {
                status: DoneStatus::Completed,
                ..
            } => {}
            other => panic!("{other:?}"),
        }
    }

    /// Other recognised causes classify too, each into the kind that names
    /// what a reader must fix.
    #[test]
    fn billing_and_auth_stops_classify_as_themselves() {
        for (code, expected) in [
            ("billing_error", StopReason::Billing),
            ("authentication_failed", StopReason::Auth),
            ("overloaded", StopReason::Overloaded),
            ("server_error", StopReason::Server),
            ("model_not_found", StopReason::Other),
        ] {
            let raw = format!(
                r#"{{"type":"assistant","message":{{"id":"m","model":"claude-opus-5","content":[]}},"error":"{code}"}}"#,
            );
            let frame = crate::claude::wire::parse_frame(&raw).unwrap();
            let events = Normalizer::new().normalize(frame, false);
            // First, before the frame-closing rotation event.
            assert_eq!(
                events.first(),
                Some(&AgentEvent::Error {
                    message: assistant_error_text(code),
                    stop: Some(expected),
                }),
                "{code}"
            );
        }
    }

    /// A later clean turn supersedes the previous one's stop: the normalizer
    /// is per-session, not per-run, so a held stop must not leak across runs.
    #[test]
    fn a_held_stop_does_not_leak_into_the_next_run() {
        let mut n = Normalizer::new();
        let feed = |n: &mut Normalizer, raw: &str| {
            let frame = crate::claude::wire::parse_frame(raw).unwrap();
            n.normalize(frame, false)
        };
        feed(&mut n, RATE_LIMITED_TURN[0]);
        feed(&mut n, RATE_LIMITED_TURN[1]);
        // Next run, clean end.
        let events = feed(
            &mut n,
            r#"{"type":"result","subtype":"success","session_id":"s2"}"#,
        );
        match events.last().expect("done") {
            AgentEvent::Done {
                status: DoneStatus::Completed,
                session_id: Some(s),
                ..
            } => assert_eq!(s, "s2"),
            other => panic!("{other:?}"),
        }
    }
}
