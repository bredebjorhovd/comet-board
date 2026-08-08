//! Frame → [`AgentEvent`] normalization, ported from claude.ts's `normalize`
//! (init dedupe, subagent filtering, tool decoding, error-code mapping).

use comet_proto::{AgentEvent, DoneStatus, HarnessId, TodoItem, ToolCall};
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
}

impl Normalizer {
    pub fn new() -> Self {
        Self {
            saw_init: false,
            assistant_message_id: new_message_id(),
            session_id: None,
            commands_seen: 0,
        }
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
            Frame::StreamEvent(f) => {
                if f.parent_tool_use_id.is_some() || f.event.kind != "content_block_delta" {
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
                if f.parent_tool_use_id.is_some() {
                    return Vec::new();
                }
                let mut out: Vec<AgentEvent> = f
                    .message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_use")
                    .map(|b| AgentEvent::ToolCall {
                        id: b.id.clone(),
                        call: decode_tool_use(&b.name, &b.input),
                    })
                    .collect();
                // A failed turn (usage limit, billing, auth, overloaded, …)
                // carries a terse `error` code here — often with empty content
                // and no `result` error — so surface it visibly.
                if let Some(code) = &f.error {
                    out.push(AgentEvent::Error {
                        message: assistant_error_text(code),
                    });
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
                if f.parent_tool_use_id.is_some() {
                    return Vec::new();
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
            // turn — make it visible; allowed/allowed_warning stay quiet.
            Frame::RateLimit(f) => {
                if f.rate_limit_info.status != "rejected" {
                    return Vec::new();
                }
                let window =
                    rate_window_label(f.rate_limit_info.rate_limit_type.as_deref().unwrap_or(""));
                vec![AgentEvent::Error {
                    message: format!(
                        "Claude {window} limit reached — the turn was blocked. Try again after it resets."
                    ),
                }]
            }

            Frame::Result(f) => {
                if let Some(id) = &f.session_id {
                    self.session_id = Some(id.clone());
                }
                // Straight across: the CLI already reports the four buckets in
                // the shape `TokenUsage` normalizes on (uncached input apart
                // from the two cache figures), so nothing is derived here.
                let usage = AgentEvent::Usage(comet_proto::TokenUsage {
                    input_tokens: f.usage.input_tokens,
                    output_tokens: f.usage.output_tokens,
                    cache_read_tokens: f.usage.cache_read_input_tokens,
                    cache_creation_tokens: f.usage.cache_creation_input_tokens,
                });
                let done = if f.subtype == "success" {
                    AgentEvent::Done {
                        status: if interrupted {
                            DoneStatus::Interrupted
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
                vec![usage, done]
            }

            // Control frames are handled by the run loop, not normalized.
            Frame::ControlRequest(_) | Frame::Other => Vec::new(),
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
}
