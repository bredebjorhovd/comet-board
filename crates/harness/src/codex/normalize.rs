//! Codex app-server notification/item → [`AgentEvent`] mapping, ported from
//! codex.ts's `mapItem`/notification switch.
//!
//! Tolerant by construction: both field spellings the app server has shipped
//! (`delta`/`textDelta`, `exitCode`/`exit_code`, camelCase/snake_case item
//! types) are accepted, and unknown item types map to nothing.

use comet_proto::{AgentEvent, TodoItem, ToolCall};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Started,
    Completed,
}

fn field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(*k))
}

fn str_field(v: &Value, keys: &[&str]) -> String {
    field(v, keys)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// Delta text under either spelling the app server has used
/// (`delta` on agentMessage, `textDelta` on some reasoning builds).
pub(crate) fn delta_text(params: &Value) -> Option<String> {
    field(params, &["delta", "textDelta"])
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

pub(crate) fn item_id(params: &Value) -> String {
    str_field(params, &["itemId", "item_id"])
}

/// `params.turn.id` on the turn/* lifecycle notifications.
pub(crate) fn turn_id(params: &Value) -> String {
    params
        .get("turn")
        .map(|t| str_field(t, &["id"]))
        .unwrap_or_default()
}

/// `params.turn.error.message` (turn/completed carries an optional error;
/// turn/failed always should).
pub(crate) fn turn_error_message(params: &Value) -> Option<String> {
    params
        .get("turn")
        .and_then(|t| t.get("error"))
        .filter(|e| !e.is_null())
        .map(|e| {
            let msg = str_field(e, &["message"]);
            if msg.is_empty() { e.to_string() } else { msg }
        })
}

/// `thread/tokenUsage/updated` → a [`AgentEvent::Usage`] snapshot of the LAST
/// turn's tokens (held by the session loop, emitted before `Done`).
///
/// Two things this notification is *not*, both of which would double-count a
/// long thread if taken at face value (gh#151):
///
/// - It is a **snapshot, not a delta.** Codex re-sends it as the turn runs,
///   each time with that turn's running total. The session loop keeps only the
///   latest in `pending_usage` and flushes one event at `turn/completed`, so
///   what reaches the journal is one figure per turn and summing over turns is
///   sound. We read `last` (the turn) and never `total` (the thread), which
///   would be added to itself once per turn.
/// - Its `inputTokens` **includes** `cachedInputTokens`, the reverse of the
///   Anthropic shape [`comet_proto::TokenUsage`] normalizes on. Subtracted
///   here, saturating, so the four buckets stay disjoint and addable.
pub(crate) fn usage_event(params: &Value) -> Option<AgentEvent> {
    let last = field(params, &["tokenUsage", "token_usage"])?.get("last")?;
    let count = |keys: &[&str]| {
        field(last, keys)
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    let cached = count(&["cachedInputTokens", "cached_input_tokens"]);
    Some(AgentEvent::Usage(comet_proto::TokenUsage {
        input_tokens: count(&["inputTokens", "input_tokens"]).saturating_sub(cached),
        output_tokens: count(&["outputTokens", "output_tokens"]),
        cache_read_tokens: cached,
        // Codex meters no cache *writes* — the field does not exist on its
        // wire. Zero is the honest reading: not "no cache", but "nothing this
        // provider will tell us about creating one".
        cache_creation_tokens: 0,
    }))
}

/// `thread/tokenUsage/updated` → how full the window is (gh#271).
///
/// The same notification [`usage_event`] reads, asked a different question —
/// and the reason the intermediate snapshots are worth more than the one that
/// survives to `turn/completed`. Codex volunteers `modelContextWindow`, so no
/// polling is needed on this harness at all.
///
/// **`last`, not `total`.** `total` is the thread's spend, which passes the
/// window many times over on a long session and would show as 400% full.
/// `last` is the most recent request: its `inputTokens` *is* the conversation
/// as the model saw it (prompt, tools, every prior turn), and the output
/// joins it in the next one — so input + output is what occupies the window
/// now. `totalTokens` is that sum when the server sends it.
///
/// `None` when the window is absent (`modelContextWindow` is nullable, and a
/// third-party provider may not state one): a fullness with no denominator is
/// not a reading, and the board renders the absence rather than a guess.
pub(crate) fn context_event(params: &Value) -> Option<comet_proto::ContextUsage> {
    let usage = field(params, &["tokenUsage", "token_usage"])?;
    let window = field(usage, &["modelContextWindow", "model_context_window"])
        .and_then(Value::as_u64)
        .filter(|w| *w > 0)?;
    let last = usage.get("last")?;
    let count = |keys: &[&str]| {
        field(last, keys)
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    let used = match count(&["totalTokens", "total_tokens"]) {
        0 => count(&["inputTokens", "input_tokens"]) + count(&["outputTokens", "output_tokens"]),
        total => total,
    };
    Some(comet_proto::ContextUsage {
        used_tokens: used,
        max_tokens: window,
        // Codex does not auto-compact on a threshold it will name; it fails
        // the turn with `contextWindowExceeded`. Reporting a made-up point to
        // count down to would be worse than reporting none.
        compact_at_tokens: None,
    })
}

/// Tool-shaped Codex items must always close the lifecycle they open: started
/// opens the ToolCall, completed refreshes its metadata and resolves the same
/// stable id (port of codex.ts `toolLifecycle`).
fn tool_lifecycle(phase: Phase, id: String, call: ToolCall, is_error: bool) -> Vec<AgentEvent> {
    match phase {
        Phase::Started => vec![AgentEvent::ToolCall { id, call }],
        Phase::Completed => vec![
            AgentEvent::ToolCall {
                id: id.clone(),
                call,
            },
            AgentEvent::ToolResult { id, is_error },
        ],
    }
}

/// A `fileChange` item's `changes` array reduced to the typed [`ToolCall`] the
/// UI renders: a lone `add` is a file write, a lone `update` an edit, anything
/// else (deletes, multi-file changes) a patch.
fn file_change_call(changes: &[(String, String)]) -> ToolCall {
    match changes {
        [(path, kind)] if kind == "add" => ToolCall::WriteFile {
            path: path.clone(),
            content: None,
        },
        [(path, kind)] if kind == "update" => ToolCall::EditFile {
            path: path.clone(),
            old_string: None,
            new_string: None,
        },
        [(path, _)] => ToolCall::ApplyPatch {
            path: Some(path.clone()),
        },
        _ => ToolCall::ApplyPatch { path: None },
    }
}

pub(crate) fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

/// Map one `item/started` or `item/completed` payload's item to events.
/// `agentMessage` and `reasoning` flow through their delta channels and are
/// handled by the session loop, not here.
pub(crate) fn map_item(phase: Phase, item: &Value) -> Vec<AgentEvent> {
    let id = str_field(item, &["id"]);
    let status = str_field(item, &["status"]);
    match item_type(item) {
        "commandExecution" | "command_execution" => match phase {
            Phase::Started => vec![AgentEvent::ToolCall {
                id,
                call: ToolCall::Exec {
                    command: str_field(item, &["command"]),
                },
            }],
            Phase::Completed => {
                let exit_code = field(item, &["exitCode", "exit_code"])
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                vec![AgentEvent::ToolResult {
                    id,
                    is_error: status == "failed" || exit_code != 0,
                }]
            }
        },
        "fileChange" | "file_change" => {
            let changes: Vec<(String, String)> = item
                .get("changes")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|c| {
                    // Unknown kinds degrade to "update", like codex.ts.
                    let kind = c
                        .get("kind")
                        .and_then(Value::as_str)
                        .filter(|k| matches!(*k, "add" | "delete" | "update"))
                        .unwrap_or("update");
                    (str_field(c, &["path"]), kind.to_owned())
                })
                .collect();
            tool_lifecycle(
                phase,
                id,
                file_change_call(&changes),
                status == "failed" || status == "declined",
            )
        }
        "mcpToolCall" | "mcp_tool_call" => match phase {
            Phase::Started => {
                let input = item.get("arguments").filter(|v| !v.is_null()).cloned();
                vec![AgentEvent::ToolCall {
                    id,
                    call: ToolCall::Mcp {
                        server: str_field(item, &["server"]),
                        tool: str_field(item, &["tool"]),
                        input,
                    },
                }]
            }
            Phase::Completed => vec![AgentEvent::ToolResult {
                id,
                is_error: status == "failed",
            }],
        },
        "webSearch" | "web_search" => tool_lifecycle(
            phase,
            id,
            ToolCall::WebSearch {
                query: str_field(item, &["query"]),
            },
            false,
        ),
        "todoList" | "todo_list" => {
            let items = item
                .get("items")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|t| TodoItem {
                    text: str_field(t, &["text"]),
                    done: field(t, &["completed", "done"]).and_then(Value::as_bool) == Some(true),
                })
                .collect();
            tool_lifecycle(phase, id, ToolCall::Todo { items }, false)
        }
        "error" => vec![AgentEvent::Error {
            message: str_field(item, &["message"]),
        }],
        // userMessage / reasoning / agentMessage flow through delta channels.
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn delta_accepts_both_spellings() {
        assert_eq!(delta_text(&json!({"delta": "a"})), Some("a".into()));
        assert_eq!(delta_text(&json!({"textDelta": "b"})), Some("b".into()));
        assert_eq!(delta_text(&json!({"delta": ""})), None);
        assert_eq!(delta_text(&json!({})), None);
    }

    #[test]
    fn command_execution_maps_exit_code_to_error() {
        let started = map_item(
            Phase::Started,
            &json!({"type": "commandExecution", "id": "c1", "command": "ls"}),
        );
        assert_eq!(
            started,
            vec![AgentEvent::ToolCall {
                id: "c1".into(),
                call: ToolCall::Exec {
                    command: "ls".into()
                },
            }]
        );
        let completed = map_item(
            Phase::Completed,
            &json!({"type": "command_execution", "id": "c1", "status": "completed", "exit_code": 2}),
        );
        assert_eq!(
            completed,
            vec![AgentEvent::ToolResult {
                id: "c1".into(),
                is_error: true,
            }]
        );
    }

    #[test]
    fn file_change_variants_map_to_typed_calls() {
        let add = map_item(
            Phase::Started,
            &json!({"type": "fileChange", "id": "f1", "changes": [{"path": "/a.rs", "kind": "add"}]}),
        );
        assert_eq!(
            add,
            vec![AgentEvent::ToolCall {
                id: "f1".into(),
                call: ToolCall::WriteFile {
                    path: "/a.rs".into(),
                    content: None
                },
            }]
        );
        let update = map_item(
            Phase::Completed,
            &json!({"type": "fileChange", "id": "f2", "status": "declined",
                    "changes": [{"path": "/b.rs", "kind": "update"}]}),
        );
        assert_eq!(
            update,
            vec![
                AgentEvent::ToolCall {
                    id: "f2".into(),
                    call: ToolCall::EditFile {
                        path: "/b.rs".into(),
                        old_string: None,
                        new_string: None
                    },
                },
                AgentEvent::ToolResult {
                    id: "f2".into(),
                    is_error: true
                },
            ]
        );
        let multi = map_item(
            Phase::Started,
            &json!({"type": "fileChange", "id": "f3",
                    "changes": [{"path": "/a"}, {"path": "/b", "kind": "delete"}]}),
        );
        assert_eq!(
            multi,
            vec![AgentEvent::ToolCall {
                id: "f3".into(),
                call: ToolCall::ApplyPatch { path: None },
            }]
        );
    }

    #[test]
    fn usage_reads_last_snapshot_under_both_spellings() {
        assert_eq!(
            usage_event(&json!({"tokenUsage": {"last": {"inputTokens": 42, "outputTokens": 7}}})),
            Some(AgentEvent::Usage(comet_proto::TokenUsage {
                input_tokens: 42,
                output_tokens: 7,
                ..Default::default()
            }))
        );
        assert_eq!(
            usage_event(&json!({"token_usage": {"last": {"input_tokens": 1, "output_tokens": 2}}})),
            Some(AgentEvent::Usage(comet_proto::TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                ..Default::default()
            }))
        );
        assert_eq!(usage_event(&json!({})), None);
    }

    /// Codex counts cached input *inside* `inputTokens`; we count it beside.
    /// Without the subtraction a cached thread's input is reported twice, and
    /// the four buckets stop being addable (gh#151).
    #[test]
    fn cached_input_is_taken_out_of_the_input_it_is_reported_inside() {
        let ev = usage_event(&json!({"tokenUsage": {"last": {
            "inputTokens": 50_000, "cachedInputTokens": 48_000, "outputTokens": 900,
        }}}));
        let Some(AgentEvent::Usage(usage)) = ev else {
            panic!("expected a usage event");
        };
        assert_eq!(usage.input_tokens, 2_000, "fresh input only");
        assert_eq!(usage.cache_read_tokens, 48_000);
        assert_eq!(usage.cache_creation_tokens, 0, "codex meters no writes");
        // The whole point of the split: the buckets add up to what codex said
        // the turn read, plus what it wrote — nothing counted twice.
        assert_eq!(usage.input_total(), 50_000);
        assert_eq!(usage.total(), 50_900);
    }

    /// A snapshot arriving with cached ≥ input (a rounding or ordering quirk
    /// on codex's side) must not wrap a u64 into a nonsense total.
    #[test]
    fn a_cached_count_larger_than_the_input_saturates_rather_than_wrapping() {
        let ev = usage_event(&json!({"tokenUsage": {"last": {
            "inputTokens": 10, "cachedInputTokens": 99, "outputTokens": 1,
        }}}));
        let Some(AgentEvent::Usage(usage)) = ev else {
            panic!("expected a usage event");
        };
        assert_eq!(usage.input_tokens, 0);
    }

    /// The same notification, asked how full the window is (gh#271). `last`
    /// is the reading: `total` is the thread's spend, which on a long session
    /// runs to several times the window.
    #[test]
    fn fullness_is_the_last_request_against_the_window_never_the_thread_total() {
        let ctx = context_event(&json!({"tokenUsage": {
            "modelContextWindow": 272_000,
            "last": {"inputTokens": 130_000, "cachedInputTokens": 120_000,
                     "outputTokens": 6_000, "totalTokens": 136_000},
            "total": {"inputTokens": 2_000_000, "outputTokens": 90_000,
                      "totalTokens": 2_090_000},
        }}))
        .expect("a reading");
        assert_eq!(ctx.used_tokens, 136_000);
        assert_eq!(ctx.max_tokens, 272_000);
        assert_eq!(ctx.percent(), Some(50));
        // Codex names no auto-compact point; it fails the turn instead.
        assert_eq!(ctx.compact_at_tokens, None);
        assert_eq!(ctx.until_compact(), None);
        // Snake case, and `totalTokens` absent: input + output is the sum.
        let ctx = context_event(&json!({"token_usage": {
            "model_context_window": 100,
            "last": {"input_tokens": 30, "output_tokens": 10},
        }}))
        .expect("a reading");
        assert_eq!(ctx.used_tokens, 40);
        assert_eq!(ctx.percent(), Some(40));
    }

    /// No window, no reading — never a percentage against a denominator
    /// nobody supplied. (`modelContextWindow` is nullable on the wire; a
    /// third-party provider may not state one.)
    #[test]
    fn a_thread_with_no_stated_window_reports_no_fullness() {
        assert_eq!(
            context_event(&json!({"tokenUsage": {"last": {"totalTokens": 500}}})),
            None
        );
        assert_eq!(
            context_event(&json!({"tokenUsage": {
                "modelContextWindow": null, "last": {"totalTokens": 500},
            }})),
            None
        );
        assert_eq!(context_event(&json!({})), None);
        // And the spend event is unaffected either way — the two readings are
        // taken off the same notification but answer different questions.
        assert!(usage_event(&json!({"tokenUsage": {"last": {"inputTokens": 5}}})).is_some());
    }

    #[test]
    fn turn_error_extraction() {
        assert_eq!(
            turn_error_message(&json!({"turn": {"id": "t", "error": {"message": "boom"}}})),
            Some("boom".into())
        );
        assert_eq!(turn_error_message(&json!({"turn": {"id": "t"}})), None);
        assert_eq!(turn_error_message(&json!({"turn": {"error": null}})), None);
    }
}
