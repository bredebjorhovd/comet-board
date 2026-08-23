//! opencode `/event` SSE payload → session-loop signals, mirroring
//! `crates/harness/src/codex/normalize.rs`.
//!
//! The instance `/event` stream (established against `opencode serve`) emits
//! V1 events shaped `{ id, type, properties }`. The types a run produces
//! (observed against 1.18.x):
//!
//! - `message.part.delta`  — streaming text/reasoning (`properties.field` +
//!   `properties.delta`).
//! - `message.part.updated` — part lifecycle; tool parts carry a `state` whose
//!   `status` walks pending → running → completed, with the tool `input`
//!   populated once running.
//! - `message.updated` — message lifecycle; an assistant message's
//!   `info.time.completed` marks the steering boundary, and its
//!   `info.finish` names why the provider stream ended for that message
//!   (`"tool-calls"` mid-loop — the turn means to continue; `"stop"` at a
//!   real turn end).
//! - `session.status` / `session.idle` — busy/idle transitions (a turn ends
//!   on idle).
//! - `session.error` — a run error; `MessageAbortedError` means abort.
//! - `question.asked` / `permission.asked` — input bridges.

use comet_proto::{TodoItem, ToolCall, UserInputQuestion};
use serde_json::Value;

/// One parsed `/event` line, reduced to what the session loop acts on.
#[derive(Debug)]
pub(crate) enum Event {
    /// Streaming text or reasoning (`properties.field` picks the channel).
    Delta { field: String, delta: String },
    /// A tool part's state moved; the loop emits ToolCall once per part and a
    /// ToolResult when it settles.
    Tool {
        part_id: String,
        status: String,
        input: Option<Value>,
        exit: Option<i64>,
        title: String,
    },
    /// An assistant message was created or completed (`info.time.completed`).
    MessageUpdated { info: Value },
    /// The session turned busy (a turn is in flight).
    Busy,
    /// The session went idle (a turn finished).
    Idle,
    /// A session-level error; `aborted` ⇒ an abort/interrupt, else a run error.
    Error { message: String, aborted: bool },
    /// The server wants the user to answer questions.
    Question {
        request_id: String,
        questions: Vec<UserInputQuestion>,
    },
    /// The server wants a permission decision.
    Permission { request_id: String },
    /// Stream EOF or read failure.
    Eof,
}

pub(crate) fn parse_event(type_: &str, props: &Value) -> Option<Event> {
    let str_field = |k: &str| props.get(k).and_then(Value::as_str).unwrap_or("");
    match type_ {
        "message.part.delta" => {
            let field = str_field("field");
            let delta = str_field("delta");
            if delta.is_empty() {
                return None;
            }
            Some(Event::Delta {
                field: field.to_owned(),
                delta: delta.to_owned(),
            })
        }
        "message.part.updated" => {
            let part = props.get("part")?;
            let status = part
                .get("state")
                .and_then(|s| s.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let input = part
                .get("state")
                .and_then(|s| s.get("input"))
                .filter(|v| !v.is_null());
            let exit = part
                .get("state")
                .and_then(|s| s.get("metadata"))
                .and_then(|m| m.get("exit"))
                .and_then(Value::as_i64);
            Some(Event::Tool {
                part_id: part
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
                status,
                input: input.cloned(),
                exit,
                title: part
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            })
        }
        "message.updated" => {
            let info = props.get("info").cloned()?;
            if info.get("role").and_then(Value::as_str) == Some("assistant") {
                Some(Event::MessageUpdated { info })
            } else {
                None
            }
        }
        "session.status" => {
            let type_ = props
                .get("status")
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("");
            match type_ {
                "busy" => Some(Event::Busy),
                "idle" => Some(Event::Idle),
                _ => None,
            }
        }
        "session.idle" => Some(Event::Idle),
        "session.error" => {
            let error = props.get("error").cloned().unwrap_or_else(|| Value::Null);
            let message = error
                .get("data")
                .and_then(|d| d.get("message"))
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("opencode error")
                })
                .to_owned();
            let aborted = error
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| n.contains("Aborted"));
            Some(Event::Error { message, aborted })
        }
        "question.asked" => {
            let request_id = props.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
            let questions: Vec<UserInputQuestion> = props
                .get("questions")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .enumerate()
                        .map(|(i, q)| question_from_info(request_id.as_str(), i, q))
                        .collect()
                })
                .unwrap_or_default();
            if questions.is_empty() {
                return None;
            }
            Some(Event::Question { request_id, questions })
        }
        "permission.asked" => {
            let request_id = props.get("id").and_then(Value::as_str).unwrap_or("").to_owned();
            Some(Event::Permission { request_id })
        }
        "session.next.prompted" => {
            // The steer was admitted into the live turn (delivery "steer").
            if props.get("delivery").and_then(Value::as_str) == Some("steer") {
                Some(Event::Busy)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// The V1 `question.asked` carries a `que_` request id; each [`UserInputQuestion`]
/// gets a stable sub-id (`{requestId}:{i}`) the answers map back by.
fn question_from_info(request_id: &str, index: usize, q: &Value) -> UserInputQuestion {
    let options: Vec<String> = q
        .get("options")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|o| o.get("label").and_then(Value::as_str)).map(str::to_owned).collect())
        .unwrap_or_default();
    UserInputQuestion {
        id: format!("{request_id}:{index}"),
        header: q
            .get("header")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("Question")
            .to_owned(),
        question: q
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        options,
        multi_select: q.get("multiple").and_then(Value::as_bool) == Some(true),
    }
}

/// Map a tool part's `input` object to the typed [`ToolCall`] the UI renders.
/// opencode's tool inputs carry the tool's own argument schema; we infer the
/// kind from the dominant key (command/filePath/pattern/…), falling back to an
/// `Unknown` named by the part's title. `WriteFile` content is deliberately
/// dropped (the render-parts policy strips it anyway).
pub(crate) fn tool_call_from_input(_part_id: &str, input: &Value, title: &str) -> ToolCall {
    let str_of = |k: &str| input.get(k).and_then(Value::as_str).map(str::to_owned);
    if str_of("command").is_some() {
        return ToolCall::Exec {
            command: str_of("command").unwrap_or_default(),
        };
    }
    if let Some(file_path) = str_of("filePath") {
        if str_of("content").is_some() {
            return ToolCall::WriteFile {
                path: file_path,
                content: None,
            };
        }
        if str_of("oldString").is_some() || str_of("newString").is_some() {
            return ToolCall::EditFile {
                path: file_path,
                old_string: str_of("oldString"),
                new_string: str_of("newString"),
            };
        }
        return ToolCall::ReadFile { path: file_path };
    }
    if str_of("pattern").is_some() {
        let pattern = str_of("pattern").unwrap_or_default();
        let path = str_of("path");
        return if path.is_some() {
            ToolCall::Search { pattern, path }
        } else {
            ToolCall::Glob { pattern }
        };
    }
    if str_of("url").is_some() {
        return ToolCall::WebFetch {
            url: str_of("url").unwrap_or_default(),
            prompt: str_of("prompt"),
        };
    }
    if str_of("query").is_some() {
        return ToolCall::WebSearch {
            query: str_of("query").unwrap_or_default(),
        };
    }
    if let Some(items) = input.get("items").and_then(Value::as_array) {
        return ToolCall::Todo {
            items: items
                .iter()
                .map(|t| TodoItem {
                    text: t
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    done: t.get("completed").and_then(Value::as_bool) == Some(true),
                })
                .collect(),
        };
    }
    if let (Some(server), Some(tool)) = (str_of("server"), str_of("tool")) {
        return ToolCall::Mcp {
            server,
            tool,
            input: if input.is_object() && !input.as_object().is_some_and(|m| m.is_empty()) {
                Some(input.clone())
            } else {
                None
            },
        };
    }
    ToolCall::Unknown {
        name: if title.is_empty() { "tool".into() } else { title.into() },
        input: if input.is_object() && !input.as_object().is_some_and(|m| m.is_empty()) {
            Some(input.clone())
        } else {
            None
        },
    }
}

/// The `message.updated` assistant info's completed timestamp — the steering
/// boundary marker.
pub(crate) fn assistant_message_id(info: &Value) -> String {
    info.get("id").and_then(Value::as_str).unwrap_or("").to_owned()
}

pub(crate) fn assistant_message_completed(info: &Value) -> bool {
    info.get("time")
        .and_then(|t| t.get("completed"))
        .and_then(Value::as_i64)
        .is_some()
}

/// The assistant message's provider finish reason (`info.finish`), when the
/// update carries one: `"tool-calls"` while the agentic loop means to run
/// tools and continue, `"stop"` at a message that ends its turn. Absent on
/// older servers — callers must treat `None` as "unknown", never as either.
pub(crate) fn assistant_message_finish(info: &Value) -> Option<String> {
    info.get("finish")
        .and_then(Value::as_str)
        .filter(|f| !f.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn delta_parses_text_and_reasoning() {
        let ev = parse_event(
            "message.part.delta",
            &json!({"sessionID": "ses_x", "partID": "prt_x", "field": "text", "delta": "hi"}),
        )
        .unwrap();
        assert!(matches!(ev, Event::Delta { field, delta } if field == "text" && delta == "hi"));
        let ev = parse_event(
            "message.part.delta",
            &json!({"field": "reasoning", "delta": "think"}),
        )
        .unwrap();
        assert!(matches!(ev, Event::Delta { field, .. } if field == "reasoning"));
        assert!(parse_event("message.part.delta", &json!({"field": "text", "delta": ""})).is_none());
    }

    #[test]
    fn tool_part_walks_states() {
        let running = parse_event(
            "message.part.updated",
            &json!({"part": {"id": "prt_c", "state": {"status": "running", "input": {"command": "ls"}}}}),
        )
        .unwrap();
        match running {
            Event::Tool { part_id, status, input, .. } => {
                assert_eq!(part_id, "prt_c");
                assert_eq!(status, "running");
                assert_eq!(input.unwrap().get("command").unwrap(), "ls");
            }
            other => panic!("expected Tool, got {other:?}"),
        }
        let done = parse_event(
            "message.part.updated",
            &json!({"part": {"id": "prt_c", "title": "ls",
                    "state": {"status": "completed", "input": {"command": "ls"},
                              "metadata": {"exit": 2, "output": ""}}}}),
        )
        .unwrap();
        match done {
            Event::Tool { status, exit, .. } => {
                assert_eq!(status, "completed");
                assert_eq!(exit, Some(2));
            }
            other => panic!("expected Tool, got {other:?}"),
        }
    }

    #[test]
    fn tool_input_maps_to_typed_calls() {
        assert_eq!(
            tool_call_from_input("x", &json!({"command": "cargo test"}), "run"),
            ToolCall::Exec { command: "cargo test".into() }
        );
        assert_eq!(
            tool_call_from_input("x", &json!({"filePath": "/a.rs", "content": "code"}), "write"),
            ToolCall::WriteFile { path: "/a.rs".into(), content: None }
        );
        assert_eq!(
            tool_call_from_input(
                "x",
                &json!({"filePath": "/a.rs", "oldString": "a", "newString": "b"}),
                "edit"
            ),
            ToolCall::EditFile {
                path: "/a.rs".into(),
                old_string: Some("a".into()),
                new_string: Some("b".into())
            }
        );
        assert_eq!(
            tool_call_from_input("x", &json!({"filePath": "/a.rs"}), "read"),
            ToolCall::ReadFile { path: "/a.rs".into() }
        );
        assert_eq!(
            tool_call_from_input("x", &json!({"pattern": "foo", "path": "/a"}), "grep"),
            ToolCall::Search { pattern: "foo".into(), path: Some("/a".into()) }
        );
        assert_eq!(
            tool_call_from_input("x", &json!({"pattern": "*.rs"}), "glob"),
            ToolCall::Glob { pattern: "*.rs".into() }
        );
        assert_eq!(
            tool_call_from_input("x", &json!({"url": "https://x.dev", "prompt": "p"}), "fetch"),
            ToolCall::WebFetch { url: "https://x.dev".into(), prompt: Some("p".into()) }
        );
        let unk = tool_call_from_input("x", &json!({"description": "d"}), "mytool");
        assert!(matches!(unk, ToolCall::Unknown { name, .. } if name == "mytool"));
    }

    #[test]
    fn question_and_permission_map() {
        let ev = parse_event(
            "question.asked",
            &json!({"id": "que_1", "sessionID": "ses_x", "questions": [
                {"question": "Go?", "header": "Approve", "options": [{"label": "Yes"}], "multiple": false}
            ]}),
        )
        .unwrap();
        match ev {
            Event::Question { request_id, questions } => {
                assert_eq!(request_id, "que_1");
                assert_eq!(questions[0].question, "Go?");
                assert_eq!(questions[0].options, vec!["Yes".to_string()]);
            }
            other => panic!("expected Question, got {other:?}"),
        }
        let ev = parse_event(
            "permission.asked",
            &json!({"id": "per_1", "sessionID": "ses_x", "permission": "bash",
                    "patterns": ["**"], "metadata": {}}),
        )
        .unwrap();
        match ev {
            Event::Permission { request_id } => {
                assert_eq!(request_id, "per_1");
            }
            other => panic!("expected Permission, got {other:?}"),
        }
    }

    #[test]
    fn session_lifecycle_and_abort_map() {
        assert!(matches!(parse_event("session.status", &json!({"status": {"type": "busy"}})), Some(Event::Busy)));
        assert!(matches!(parse_event("session.status", &json!({"status": {"type": "idle"}})), Some(Event::Idle)));
        assert!(matches!(parse_event("session.idle", &json!({})), Some(Event::Idle)));
        let aborted = parse_event(
            "session.error",
            &json!({"sessionID": "ses_x", "error": {"name": "MessageAbortedError", "data": {"message": "Aborted"}}}),
        )
        .unwrap();
        assert!(matches!(aborted, Event::Error { aborted: true, .. }));
        let failed = parse_event(
            "session.error",
            &json!({"error": {"name": "UnknownError", "data": {"message": "boom"}}}),
        )
        .unwrap();
        assert!(matches!(failed, Event::Error { aborted: false, message, .. } if message == "boom"));
    }

    #[test]
    fn assistant_completion_detection() {
        assert!(!assistant_message_completed(&json!({"id": "m1", "time": {"created": 1}})));
        assert!(assistant_message_completed(&json!({"id": "m1", "time": {"created": 1, "completed": 2}})));
        assert_eq!(assistant_message_id(&json!({"id": "m1"})), "m1");
    }

    #[test]
    fn assistant_finish_reason_is_read_when_carried_and_absent_means_unknown() {
        assert_eq!(
            assistant_message_finish(&json!({"id": "m1", "finish": "tool-calls"})),
            Some("tool-calls".to_string())
        );
        assert_eq!(
            assistant_message_finish(&json!({"id": "m1", "finish": "stop"})),
            Some("stop".to_string())
        );
        // Older servers carry no finish at all, and an empty string is no
        // reason either — both must read as unknown, not as a value.
        assert_eq!(assistant_message_finish(&json!({"id": "m1"})), None);
        assert_eq!(assistant_message_finish(&json!({"id": "m1", "finish": ""})), None);
    }
}
