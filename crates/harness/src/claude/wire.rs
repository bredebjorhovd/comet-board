//! Claude CLI stream-json wire frames (stdout JSONL + stdin lines).
//!
//! Tolerant by construction: every field defaults, unknown frame types map to
//! [`Frame::Other`], so a newer CLI never breaks parsing — we only read the
//! fields the normalizer needs (spec: docs/research/harness.md).

use serde::Deserialize;
use serde_json::{Value, json};

/// One parsed stdout line.
#[derive(Debug)]
pub(crate) enum Frame {
    System(SystemFrame),
    StreamEvent(StreamEventFrame),
    Assistant(MessageFrame),
    User(MessageFrame),
    RateLimit(RateLimitFrame),
    Result(ResultFrame),
    ControlRequest(ControlRequestFrame),
    /// The CLI's reply to a request *we* sent (gh#271: `get_context_usage`).
    ControlResponse(ControlResponseFrame),
    /// control_cancel_request / anything unknown.
    Other,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct SystemFrame {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub session_id: String,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StreamEventFrame {
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub event: StreamEventBody,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct StreamEventBody {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub delta: Delta,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct Delta {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub thinking: String,
}

/// An `assistant` or `user` frame (an Anthropic API message envelope).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MessageFrame {
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    #[serde(default)]
    pub message: MessageBody,
    /// Terse assistant-level error code (`rate_limit`, `billing_error`, …).
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct MessageBody {
    /// Either a plain string or an array of content blocks.
    #[serde(default)]
    pub content: Value,
}

impl MessageBody {
    pub fn blocks(&self) -> impl Iterator<Item = ContentBlock> + '_ {
        self.content
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .filter_map(|b| serde_json::from_value(b.clone()).ok())
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ContentBlock {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub input: Value,
    /// `text` blocks only. Read for the `<command-name>` markup the CLI wraps
    /// an expanded slash command in (gh#134) — nothing else in a user frame's
    /// prose is normalized.
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub tool_use_id: String,
    #[serde(default)]
    pub is_error: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RateLimitFrame {
    #[serde(default)]
    pub rate_limit_info: RateLimitInfo,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RateLimitInfo {
    #[serde(default)]
    pub status: String,
    #[serde(rename = "rateLimitType", default)]
    pub rate_limit_type: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ResultFrame {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub result: Option<String>,
    #[serde(default)]
    pub errors: Vec<Value>,
    #[serde(default)]
    pub usage: UsageBody,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// The result frame's `usage` block. `input_tokens` here is the *uncached*
/// input only — the cached halves are the two fields below it, and on a long
/// session they are the overwhelming majority of what the turn read. Reading
/// only the first two (which is what shipped) under-reported a run by an order
/// of magnitude; gh#151.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct UsageBody {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    /// Input written into the prompt cache by this turn.
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Input this turn read back out of it.
    #[serde(default)]
    pub cache_read_input_tokens: u64,
}

/// A CLI→client control request (`can_use_tool` is the one we act on).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ControlRequestFrame {
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub request: ControlRequestBody,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ControlRequestBody {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub input: Value,
}

/// A client→CLI request's reply. `response.response` is the payload on
/// success; `subtype: "error"` carries `error` instead and no payload.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ControlResponseFrame {
    #[serde(default)]
    pub response: ControlResponseBody,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct ControlResponseBody {
    #[serde(default)]
    pub subtype: String,
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub response: Value,
    #[serde(default)]
    pub error: Option<String>,
}

/// The `get_context_usage` payload, reduced to the four numbers a fullness
/// signal is made of. The CLI's own reply is much larger — a per-category
/// breakdown, an MCP tool inventory, a render grid for `/context` — and none
/// of that is a level; see [`comet_proto::ContextUsage`].
///
/// `maxTokens` is the *usable* window (an auto-compact buffer can hold it
/// under `rawMaxTokens`), which is the denominator the CLI itself divides by,
/// so it is the one we read — falling back to `rawMaxTokens` on a CLI that
/// only sends the latter.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ContextUsageBody {
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub max_tokens: u64,
    #[serde(default)]
    pub raw_max_tokens: u64,
    #[serde(default)]
    pub auto_compact_threshold: Option<u64>,
    #[serde(default)]
    pub is_auto_compact_enabled: Option<bool>,
}

impl ContextUsageBody {
    /// `None` when the reply carried no usable window at all: a fullness with
    /// no denominator is not a reading, and inventing one would put a
    /// percentage on the board that no harness ever said.
    pub fn to_usage(&self) -> Option<comet_proto::ContextUsage> {
        let max_tokens = if self.max_tokens > 0 {
            self.max_tokens
        } else {
            self.raw_max_tokens
        };
        if max_tokens == 0 && self.total_tokens == 0 {
            return None;
        }
        Some(comet_proto::ContextUsage {
            used_tokens: self.total_tokens,
            max_tokens,
            // A threshold the CLI will not act on is not a threshold. Absent
            // `isAutoCompactEnabled` (older CLI) the number is taken at its
            // word, which is what it meant before the flag existed.
            compact_at_tokens: self
                .auto_compact_threshold
                .filter(|_| self.is_auto_compact_enabled.unwrap_or(true))
                .filter(|at| *at > 0),
        })
    }
}

/// Decode a successful `get_context_usage` reply.
pub(crate) fn parse_context_usage(payload: &Value) -> Option<comet_proto::ContextUsage> {
    serde_json::from_value::<ContextUsageBody>(payload.clone())
        .ok()?
        .to_usage()
}

/// Parse one stdout JSONL line. `Err` = not JSON; unknown types = `Other`.
pub(crate) fn parse_frame(line: &str) -> Result<Frame, serde_json::Error> {
    let value: Value = serde_json::from_str(line)?;
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
    let frame = match kind {
        "system" => Frame::System(serde_json::from_value(value)?),
        "stream_event" => Frame::StreamEvent(serde_json::from_value(value)?),
        "assistant" => Frame::Assistant(serde_json::from_value(value)?),
        "user" => Frame::User(serde_json::from_value(value)?),
        "rate_limit_event" => Frame::RateLimit(serde_json::from_value(value)?),
        "result" => Frame::Result(serde_json::from_value(value)?),
        "control_request" => Frame::ControlRequest(serde_json::from_value(value)?),
        "control_response" => Frame::ControlResponse(serde_json::from_value(value)?),
        _ => Frame::Other,
    };
    Ok(frame)
}

/// A stdin user turn: `{"type":"user","message":{...},"parent_tool_use_id":null}`.
/// Steering = another such line mid-run (consumed at a step boundary).
pub(crate) fn user_message_line(text: &str) -> String {
    json!({
        "type": "user",
        "message": { "role": "user", "content": text },
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// One inline image for a stdin user turn (Anthropic base64 image source).
pub(crate) struct ImageBlock {
    /// One of the API-supported media types (png/jpeg/gif/webp).
    pub media_type: String,
    /// Raw base64 (no data-URL prefix).
    pub data: String,
}

/// A stdin user turn whose content is an array of blocks: the attached images
/// first, then the text — the standard Anthropic image+text message shape
/// (verified against the real CLI: `--input-format stream-json` accepts image
/// content blocks in user frames). Empty `images` degrades to the plain line.
pub(crate) fn user_message_line_with_images(text: &str, images: &[ImageBlock]) -> String {
    if images.is_empty() {
        return user_message_line(text);
    }
    let mut blocks: Vec<Value> = images
        .iter()
        .map(|img| {
            json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": img.media_type,
                    "data": img.data,
                },
            })
        })
        .collect();
    blocks.push(json!({ "type": "text", "text": text }));
    json!({
        "type": "user",
        "message": { "role": "user", "content": blocks },
        "parent_tool_use_id": null,
    })
    .to_string()
}

/// Success reply to a CLI control request (`can_use_tool` allow/deny payloads).
pub(crate) fn control_response_line(request_id: &str, response: Value) -> String {
    json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": request_id,
            "response": response,
        },
    })
    .to_string()
}

/// `can_use_tool` allow payload with the (possibly updated) tool input.
pub(crate) fn allow_response(updated_input: Value) -> Value {
    json!({ "behavior": "allow", "updatedInput": updated_input })
}

/// Client→CLI interrupt control request.
pub(crate) fn interrupt_request_line(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "interrupt" },
    })
    .to_string()
}

/// Client→CLI context-usage control request (gh#271). Its reply arrives as a
/// `control_response` carrying the same `request_id`, which is how the session
/// loop tells our polls apart from the interrupt's reply.
pub(crate) fn context_usage_request_line(request_id: &str) -> String {
    json!({
        "type": "control_request",
        "request_id": request_id,
        "request": { "subtype": "get_context_usage" },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_and_unknown_frames() {
        let init = r#"{"type":"system","subtype":"init","model":"m","tools":["Bash"],"cwd":"/x","session_id":"s1"}"#;
        match parse_frame(init).expect("parses") {
            Frame::System(f) => {
                assert_eq!(f.subtype, "init");
                assert_eq!(f.session_id, "s1");
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        assert!(matches!(
            parse_frame(r#"{"type":"mystery_frame"}"#).expect("parses"),
            Frame::Other
        ));
        assert!(parse_frame("not json").is_err());
    }

    /// Verbatim (bar the trimmed grid/inventory fields) from claude 2.1.227
    /// answering `get_context_usage` over a live stdio control channel. The
    /// numbers the board renders come out of *these* four keys.
    #[test]
    fn context_usage_reply_decodes_to_a_level() {
        let line = r#"{"type":"control_response","response":{"subtype":"success","request_id":"ctx_1",
            "response":{"categories":[{"name":"System prompt","tokens":6441}],"totalTokens":28294,
            "maxTokens":200000,"rawMaxTokens":200000,"percentage":14,"autocompactSource":"auto",
            "model":"claude-haiku-4-5-20251001","autoCompactThreshold":167000,
            "isAutoCompactEnabled":true}}}"#;
        let Frame::ControlResponse(frame) = parse_frame(line).expect("parses") else {
            panic!("expected a control_response frame");
        };
        assert_eq!(frame.response.request_id, "ctx_1");
        assert_eq!(frame.response.subtype, "success");
        let usage = parse_context_usage(&frame.response.response).expect("a reading");
        assert_eq!(usage.used_tokens, 28_294);
        assert_eq!(usage.max_tokens, 200_000);
        assert_eq!(usage.compact_at_tokens, Some(167_000));
        assert_eq!(usage.percent(), Some(14), "matches the CLI's own rounding");
    }

    /// Auto-compaction off means there is no point to count down to — the
    /// window is still the window, and the threshold must not be reported as
    /// one the CLI will act on.
    #[test]
    fn a_disabled_autocompact_threshold_is_not_a_threshold() {
        let payload = serde_json::json!({
            "totalTokens": 10, "maxTokens": 100,
            "autoCompactThreshold": 80, "isAutoCompactEnabled": false,
        });
        let usage = parse_context_usage(&payload).expect("a reading");
        assert_eq!(usage.compact_at_tokens, None);
        assert_eq!(usage.percent(), Some(10));
        // And a reply with nothing in it at all is not a reading of zero.
        assert!(parse_context_usage(&serde_json::json!({})).is_none());
    }

    #[test]
    fn context_usage_request_names_the_control_subtype() {
        let v: Value = serde_json::from_str(&context_usage_request_line("ctx_7")).expect("json");
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["request_id"], "ctx_7");
        assert_eq!(v["request"]["subtype"], "get_context_usage");
    }

    #[test]
    fn user_line_shape_matches_protocol() {
        let line = user_message_line("hi");
        let v: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["content"], "hi");
        assert!(v["parent_tool_use_id"].is_null());
    }

    #[test]
    fn user_line_with_images_is_blocks_then_text() {
        let line = user_message_line_with_images(
            "what is this?",
            &[ImageBlock {
                media_type: "image/png".into(),
                data: "QUJD".into(),
            }],
        );
        let v: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(v["type"], "user");
        let content = v["message"]["content"].as_array().expect("array content");
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "image");
        assert_eq!(content[0]["source"]["type"], "base64");
        assert_eq!(content[0]["source"]["media_type"], "image/png");
        assert_eq!(content[0]["source"]["data"], "QUJD");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "what is this?");
        // No images ⇒ identical to the plain string line.
        assert_eq!(
            user_message_line_with_images("hi", &[]),
            user_message_line("hi")
        );
    }
}
