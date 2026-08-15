//! The first consumer of the dispatch MCP seam (gh#273): a small stdio server
//! over the board CLI's existing typed RPC client.
//!
//! The agent receives board rows and dispatch receipts as structured tool
//! results. It no longer has to shell out, parse terminal JSON, or manually
//! round-trip `COMET_BOARD_CHAT_ID`; the MCP server inherits that identity once
//! and uses it for current/sibling/child relationships and dispatch provenance.

use anyhow::{Context, Result, anyhow};
use comet_board::rows::TaskRow;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::ops::{self, Board, DispatchOpts};

const FALLBACK_PROTOCOL_VERSION: &str = "2024-11-05";

/// Serve newline-delimited JSON-RPC on stdin/stdout until the MCP client closes
/// the pipe. Stdout is protocol-only; diagnostics travel as tool errors.
pub async fn serve(port: u16, device: Option<&str>) -> Result<()> {
    let board = ops::attach(port, device).await?;
    let chat_id = std::env::var(ops::CHAT_ID_ENV)
        .ok()
        .filter(|id| !id.is_empty());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::stdout();
    while let Some(line) = lines.next_line().await.context("reading MCP stdin")? {
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(message) => handle(&board, chat_id.as_deref(), &message).await,
            Err(err) => Some(rpc_error(
                Value::Null,
                -32700,
                format!("invalid JSON: {err}"),
            )),
        };
        if let Some(response) = response {
            let mut encoded = serde_json::to_vec(&response)?;
            encoded.push(b'\n');
            stdout
                .write_all(&encoded)
                .await
                .context("writing MCP stdout")?;
            stdout.flush().await.context("flushing MCP stdout")?;
        }
    }
    Ok(())
}

async fn handle(board: &Board, chat_id: Option<&str>, message: &Value) -> Option<Value> {
    let method = message.get("method").and_then(Value::as_str)?;

    // Notifications have no response. `initialized` is the only one required
    // for this server, but accepting the cancellation/progress family keeps a
    // newer client from receiving a method error it cannot use.
    let id = message.get("id").cloned()?;
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    match method {
        "initialize" => {
            let version = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(FALLBACK_PROTOCOL_VERSION);
            Some(rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "comet-board",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            ))
        }
        "ping" => Some(rpc_result(id, json!({}))),
        "tools/list" => Some(rpc_result(id, json!({ "tools": tool_definitions() }))),
        "tools/call" => {
            let called = call_tool(board, chat_id, params).await;
            Some(rpc_result(
                id,
                match called {
                    Ok(value) => tool_result(value, false),
                    Err(err) => tool_result(json!({ "error": format!("{err:#}") }), true),
                },
            ))
        }
        _ => Some(rpc_error(
            id,
            -32601,
            format!("method `{method}` is not implemented"),
        )),
    }
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// MCP tool results keep a text fallback for older clients and carry the same
/// JSON as `structuredContent` for clients that can keep it structured.
fn tool_result(value: Value, is_error: bool) -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
        }],
        "structuredContent": value,
        "isError": is_error
    })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "task_status",
            "description": "Read one task's current board row. With no task, read the task for this dispatched chat.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Canonical board id or an unambiguous identifier, for example gh:owner/repo#14 or gh#14. Omit for this chat's task."
                    }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "related_attempts",
            "description": "Read this chat's task, sibling attempts released by the same parent chat, and tasks this chat released.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "dispatch_task",
            "description": "Release a ready board task. This starts a real coding agent; use only when the human explicitly authorized dispatch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string" },
                    "runtime": { "type": "string" },
                    "model": { "type": "string" },
                    "account": { "type": "string" },
                    "stack": { "type": "boolean", "default": false },
                    "decompose": { "type": "boolean", "default": false },
                    "onto": { "type": "string" },
                    "base": { "type": "string" }
                },
                "required": ["task"],
                "additionalProperties": false
            }
        }),
    ]
}

async fn call_tool(board: &Board, chat_id: Option<&str>, params: Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("tools/call requires a tool name"))?;
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match name {
        "task_status" => task_status(board, chat_id, arguments).await,
        "related_attempts" => related_attempts(board, chat_id, arguments).await,
        "dispatch_task" => dispatch_task(board, chat_id, arguments).await,
        _ => Err(anyhow!("unknown comet-board tool `{name}`")),
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct StatusArgs {
    task: Option<String>,
}

async fn task_status(board: &Board, chat_id: Option<&str>, arguments: Value) -> Result<Value> {
    let args: StatusArgs = serde_json::from_value(arguments).context("task_status arguments")?;
    let rows = ops::board_rows(board).await?;
    let row = match args.task.as_deref() {
        Some(task) => Some(ops::task_row_by_reference(&rows, task)?),
        None => {
            let chat = chat_id.ok_or_else(|| {
                anyhow!("this is not a board-dispatched chat; pass `task` explicitly")
            })?;
            rows.iter().find(|row| row_belongs_to_chat(row, chat))
        }
    }
    .ok_or_else(|| anyhow!("task is not on the board"))?;
    serde_json::to_value(row).context("serializing task status")
}

async fn related_attempts(board: &Board, chat_id: Option<&str>, arguments: Value) -> Result<Value> {
    let _: EmptyArgs = serde_json::from_value(arguments).context("related_attempts arguments")?;
    let chat = chat_id.ok_or_else(|| anyhow!("this MCP server has no dispatched chat identity"))?;
    let rows = ops::board_rows(board).await?;
    Ok(related_rows(&rows, chat))
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct EmptyArgs {}

fn related_rows(rows: &[TaskRow], chat_id: &str) -> Value {
    let current = rows.iter().find(|row| row_belongs_to_chat(row, chat_id));
    let parent = current.and_then(|row| row.dispatched_by_chat.as_deref());
    let siblings: Vec<&TaskRow> = parent
        .map(|parent| {
            rows.iter()
                .filter(|row| {
                    !row_belongs_to_chat(row, chat_id)
                        && row.dispatched_by_chat.as_deref() == Some(parent)
                })
                .collect()
        })
        .unwrap_or_default();
    let children: Vec<&TaskRow> = rows
        .iter()
        .filter(|row| row.dispatched_by_chat.as_deref() == Some(chat_id))
        .collect();
    json!({ "current": current, "siblings": siblings, "children": children })
}

/// A settled attempt moves its chat address from the live slot to the review
/// slot. The MCP configuration lives on that same chat and review follow-ups
/// must still resolve the task they are reviewing.
fn row_belongs_to_chat(row: &TaskRow, chat_id: &str) -> bool {
    chat_address_belongs(
        row.chat_id.as_deref(),
        row.review_chat_id.as_deref(),
        chat_id,
    )
}

fn chat_address_belongs(live: Option<&str>, review: Option<&str>, chat_id: &str) -> bool {
    live == Some(chat_id) || review == Some(chat_id)
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DispatchArgs {
    task: String,
    runtime: Option<String>,
    model: Option<String>,
    account: Option<String>,
    stack: bool,
    decompose: bool,
    onto: Option<String>,
    base: Option<String>,
}

async fn dispatch_task(board: &Board, chat_id: Option<&str>, arguments: Value) -> Result<Value> {
    let args: DispatchArgs =
        serde_json::from_value(arguments).context("dispatch_task arguments")?;
    if args.task.trim().is_empty() {
        return Err(anyhow!("dispatch_task requires a non-empty task"));
    }
    if args.stack && args.decompose {
        return Err(anyhow!(
            "a dispatch cannot request both stack and decompose"
        ));
    }
    let chat_id = chat_id.ok_or_else(|| {
        anyhow!("dispatch_task is only available inside a board-dispatched agent chat")
    })?;
    let rows = ops::board_rows(board).await?;
    let current = rows
        .iter()
        .find(|row| row_belongs_to_chat(row, chat_id))
        .ok_or_else(|| anyhow!("this chat no longer belongs to a task on the board"))?;
    let target = ops::task_row_by_reference(&rows, &args.task)?;
    let opts = DispatchOpts {
        via: Some(chat_id),
        runtime: args.runtime.as_deref(),
        model: args.model.as_deref(),
        account: args.account.as_deref(),
        // An MCP call is released by the parent agent, not by the host's
        // signed-in frontend user. Keep its chat provenance and carry no human
        // attribution or payer acknowledgement the model could manufacture.
        bill: None,
        via_user: None,
        onto: args.onto.as_deref(),
        base: args.base.as_deref(),
        replace: false,
        stack: args.stack,
        decompose: args.decompose,
    };
    if let Some(warning) = ops::cross_billing_warning_for_row(
        board,
        target,
        opts,
        current.billed_to.as_deref(),
    )
    .await
    {
        return Err(anyhow!(
            "{warning}; an agent cannot acknowledge a different payer — ask a human to dispatch this task from the board, desktop, or CLI"
        ));
    }
    let dispatched = ops::dispatch_checked(board, &target.id, opts).await?;
    Ok(json!({ "dispatch": dispatched }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_catalog_is_the_three_board_operations() {
        let tools = tool_definitions();
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert_eq!(names, ["task_status", "related_attempts", "dispatch_task"]);
        assert_eq!(tools[2]["inputSchema"]["required"], json!(["task"]));
    }

    #[test]
    fn structured_results_keep_a_text_fallback() {
        let result = tool_result(json!({"state": "working"}), false);
        assert_eq!(result["structuredContent"]["state"], "working");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("working"))
        );
        assert_eq!(result["isError"], false);
    }

    #[test]
    fn review_chat_still_belongs_to_its_task() {
        assert!(chat_address_belongs(
            None,
            Some("review-chat"),
            "review-chat"
        ));
        assert!(!chat_address_belongs(
            None,
            Some("review-chat"),
            "somebody-else"
        ));
    }
}
