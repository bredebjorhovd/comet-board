//! Thin HTTP client over a spawned `opencode serve` — the counterpart of
//! `crates/harness/src/codex/rpc.rs`, but for opencode's HTTP+SSE protocol
//! (research: docs/ARCHITECTURE.md "opencode harness"; verified against
//! 1.18.x — the instance `/event` SSE stream is the event source, the
//! `/session` REST endpoints drive it).
//!
//! Every request is against `http://127.0.0.1:{port}` (the child is spawned
//! with `--hostname 127.0.0.1`), so no auth is needed. The reader task owns the
//! SSE stream and pumps parsed [`crate::opencode::normalize::Event`]s into a
//! channel the session loop drains.

use std::time::Duration;

use comet_proto::Model;
use serde_json::{Value, json};

use crate::HarnessError;

/// Total deadline for the short JSON requests (create/prompt/abort/reply/
/// health) — the response body is read to completion quickly, so a total
/// timeout is fine there.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The long-lived `/event` SSE stream is exempt from the total request
/// deadline: a total timeout closes a busy, mid-run stream when the deadline
/// fires — even while events are actively streaming (reqwest wraps the body in
/// `TotalTimeoutBody`, gh#46). A read timeout resets on every event instead:
/// opencode serves a `server.heartbeat` every 10s, so a live stream never
/// trips it while a genuinely dead connection is still caught.
pub(crate) const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(crate) struct Client {
    base: String,
    /// Short JSON requests — total deadline `REQUEST_TIMEOUT`.
    http: reqwest::Client,
    /// The `/event` SSE stream — no total timeout, only `STREAM_READ_TIMEOUT`.
    stream: reqwest::Client,
}

/// A model reference on the opencode wire: `(providerID, modelID)`, with an
/// optional reasoning-effort `variant`. `None` model = the server's default.
#[derive(Debug, Clone)]
pub(crate) struct ModelRef {
    pub model: Option<(String, String)>,
    pub variant: Option<String>,
}

fn protocol_error(context: &str, err: impl std::fmt::Display) -> HarnessError {
    HarnessError::Protocol(format!("opencode {context}: {err}"))
}

impl Client {
    pub(crate) fn new(base: String) -> Self {
        Self::with_timeouts(base, REQUEST_TIMEOUT, STREAM_READ_TIMEOUT)
    }

    /// Build with explicit timeouts so tests can shrink the total deadline
    /// below a turn's length and prove the stream ignores it (gh#46).
    pub(crate) fn with_timeouts(
        base: String,
        request_timeout: Duration,
        stream_read_timeout: Duration,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(request_timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let stream = reqwest::Client::builder()
            .read_timeout(stream_read_timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { base, http, stream }
    }

    pub(crate) async fn health(&self) -> bool {
        self.http
            .get(format!("{}/global/health", self.base))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    }

    pub(crate) async fn create_session(&self) -> Result<String, HarnessError> {
        let res = self
            .http
            .post(format!("{}/session", self.base))
            .json(&json!({}))
            .send()
            .await
            .map_err(|e| protocol_error("session create request", e))?;
        if !res.status().is_success() {
            return Err(HarnessError::Protocol(format!(
                "opencode session create failed: {}",
                res.status()
            )));
        }
        let body: Value = res
            .json()
            .await
            .map_err(|e| protocol_error("session create response", e))?;
        body.get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| HarnessError::Protocol("opencode session create returned no id".into()))
    }

    /// Send a prompt without waiting (the SSE stream reports progress).
    pub(crate) async fn prompt_async(
        &self,
        session_id: &str,
        text: &str,
        model: &ModelRef,
    ) -> Result<(), HarnessError> {
        let mut body = serde_json::Map::new();
        body.insert("parts".into(), json!([{ "type": "text", "text": text }]));
        if let Some((provider_id, model_id)) = &model.model {
            let mut model_ref = serde_json::Map::new();
            model_ref.insert("providerID".into(), json!(provider_id));
            model_ref.insert("modelID".into(), json!(model_id));
            if let Some(variant) = &model.variant {
                model_ref.insert("variant".into(), json!(variant));
            }
            body.insert("model".into(), Value::Object(model_ref));
        }
        let res = self
            .http
            .post(format!("{}/session/{session_id}/prompt_async", self.base))
            .json(&Value::Object(body))
            .send()
            .await
            .map_err(|e| protocol_error("prompt request", e))?;
        if !res.status().is_success() {
            return Err(HarnessError::Protocol(format!(
                "opencode prompt failed: {}",
                res.status()
            )));
        }
        Ok(())
    }

    pub(crate) async fn abort(&self, session_id: &str) -> Result<(), HarnessError> {
        let res = self
            .http
            .post(format!("{}/session/{session_id}/abort", self.base))
            .send()
            .await
            .map_err(|e| protocol_error("abort request", e))?;
        if !res.status().is_success() {
            return Err(HarnessError::Protocol(format!(
                "opencode abort failed: {}",
                res.status()
            )));
        }
        Ok(())
    }

    /// Answer a `question.asked` request: `{ answers: [[label, …], …] }` — one
    /// array of selected labels per question, in order.
    pub(crate) async fn reply_question(
        &self,
        request_id: &str,
        answers: Vec<Vec<String>>,
    ) -> Result<(), HarnessError> {
        let res = self
            .http
            .post(format!("{}/question/{request_id}/reply", self.base))
            .json(&json!({ "answers": answers }))
            .send()
            .await
            .map_err(|e| protocol_error("question reply", e))?;
        if !res.status().is_success() {
            return Err(HarnessError::Protocol(format!(
                "opencode question reply failed: {}",
                res.status()
            )));
        }
        Ok(())
    }

    /// Answer a `permission.asked` request (`once`/`always`/`reject`).
    pub(crate) async fn reply_permission(
        &self,
        request_id: &str,
        reply: &str,
    ) -> Result<(), HarnessError> {
        let res = self
            .http
            .post(format!("{}/permission/{request_id}/reply", self.base))
            .json(&json!({ "reply": reply }))
            .send()
            .await
            .map_err(|e| protocol_error("permission reply", e))?;
        if !res.status().is_success() {
            return Err(HarnessError::Protocol(format!(
                "opencode permission reply failed: {}",
                res.status()
            )));
        }
        Ok(())
    }

    /// Open the instance `/event` SSE stream; the caller owns reading it. The
    /// stream client carries no total deadline — a total timeout would close a
    /// busy mid-run feed (gh#46); only the read timeout (reset by each
    /// heartbeat) applies.
    pub(crate) async fn event_stream(&self) -> Result<reqwest::Response, HarnessError> {
        let res = self
            .stream
            .get(format!("{}/event", self.base))
            .send()
            .await
            .map_err(|e| protocol_error("event stream", e))?;
        if !res.status().is_success() {
            return Err(HarnessError::Protocol(format!(
                "opencode event stream failed: {}",
                res.status()
            )));
        }
        Ok(res)
    }

    /// Live model catalog: `GET /config/providers` → connected providers'
    /// models. Returns `None` (not an error) when the endpoint is unreachable
    /// or yields nothing usable — the caller falls back to the static catalog.
    pub(crate) async fn models(&self) -> Option<Vec<Model>> {
        let res = self
            .http
            .get(format!("{}/config/providers", self.base))
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?;
        let body: Value = res.json().await.ok()?;
        let providers = body.get("providers")?.as_array()?;
        let connected: Vec<&str> = body
            .get("default")
            .and_then(Value::as_object)
            .map(|d| d.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let mut out = Vec::new();
        for provider in providers {
            let pid = provider.get("id").and_then(Value::as_str)?;
            // Skip providers with no credentials configured.
            if !connected.is_empty() && !connected.contains(&pid) {
                continue;
            }
            let models = provider.get("models").and_then(Value::as_object)?;
            let mut entries: Vec<Value> = models.values().cloned().collect();
            entries.sort_by_key(|m| {
                m.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            });
            for entry in entries {
                if let Some(model) = crate::opencode::catalog::model_from_entry(pid, &entry) {
                    out.push(model);
                }
            }
        }
        // Bound the picker surface — a long tail of niche/media models is
        // noise; text-capable coding models sort first by the label.
        out.truncate(120);
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }
}
