//! Always-on hardening (gh#57 — docs/PARITY.md §3.1–§3.3): the gaps that only
//! bite on a shared box nobody restarts.
//!
//! - the TIERED stall watchdog: a run that never produced anything is torn down
//!   with a visible error after `STALL`, while a run that HAS produced output
//!   and then went quiet is left strictly alone (a long tool call is not a
//!   crash — the reason a blanket silence-kills-it watchdog was rejected);
//! - boot-time warm-open of recent chats (14d / cap 30), which is what makes a
//!   command queued while the device was down execute at restart rather than
//!   waiting on a nudge that already fired into the void.
//!
//! Time-dependent tests run on a PAUSED clock: tokio auto-advances once every
//! task is idle, so "ten minutes of silence" costs no wall-clock and — more
//! importantly — is exact rather than racy.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_doc::{MessagePart, MessageRole, MessageStatus};
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel, SessionStatus,
    SteeringMode,
};

const CHAT: &str = "chat-hardening";
const SPACE: &str = "space-hardening";

/// Comfortably past the engine's 10-minute `STALL`.
const PAST_STALL: Duration = Duration::from_secs(11 * 60);

fn run_request(prompt: &str) -> RunRequest {
    RunRequest {
        prompt: prompt.into(),
        model: None,
        reasoning: None,
        model_options: Default::default(),
        cwd: "/tmp".into(),
        sandbox: SandboxLevel::WorkspaceWrite,
        auto_approve: true,
        attachments: Vec::new(),
        resume: None,
    }
}

/// A harness that comes up and then says nothing, ever. `preamble` events are
/// emitted first; after them the stream is pending forever, so nothing but the
/// watchdog can end the run.
///
/// With an empty preamble this is the tier-1 wedge (a CLI hung at startup —
/// waiting on a credential prompt no one can answer on a headless box). With a
/// preamble it is the tier-2 shape: a healthy agent that has spoken and is now
/// deep inside one long silent tool call.
struct SilentHarness {
    preamble: Vec<AgentEvent>,
}

#[async_trait]
impl Harness for SilentHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Silent"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[ReasoningLevel::Medium]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        let preamble = futures::stream::iter(
            self.preamble
                .clone()
                .into_iter()
                .map(Ok::<AgentEvent, HarnessError>)
                .collect::<Vec<_>>(),
        );
        Ok(preamble.chain(futures::stream::pending()).boxed())
    }
}

fn assemble(dir: &std::path::Path, harness: SilentHarness) -> EngineCore {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(harness));
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine core assembles")
}

/// A titled chat row, so the auto-titler never launches a run of its own.
fn seed_chat(core: &EngineCore, chat_id: &str) {
    core.workspace
        .create_space(SPACE, &core.device_id, "/tmp", None, false)
        .expect("create space row");
    core.workspace
        .create_chat(chat_id, SPACE, None, None)
        .expect("create chat row");
    core.workspace
        .rename_chat(chat_id, "Pre-titled")
        .expect("rename chat");
}

fn assistant_entries(core: &EngineCore, chat_id: &str) -> Vec<comet_doc::SessionMessageEntry> {
    core.doc_host
        .open(chat_id)
        .ok()
        .and_then(|h| h.doc().read_entries().ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.role == MessageRole::Assistant)
        .collect()
}

// ── §3.2 stall watchdog ─────────────────────────────────────────────────────

/// Tier 1: a run that produces NOTHING — not even `SessionStarted` — is ended
/// after `STALL` with a visible error, and the session leaves `Working`. This
/// is the whole point on an unattended box: the child is released, and the row
/// stops lying about being busy.
#[tokio::test(start_paused = true)]
async fn startup_stall_ends_the_run_with_a_visible_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(dir.path(), SilentHarness { preamble: vec![] });
    seed_chat(&core, CHAT);

    core.sessions
        .dispatch(CHAT, HarnessId::Mock, run_request("hello?"), None)
        .await
        .expect("dispatch");
    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Working),
        "a freshly dispatched run is Working"
    );

    tokio::time::sleep(PAST_STALL).await;

    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Errored),
        "the wedged run must not stay Working forever"
    );
    let entries = assistant_entries(&core, CHAT);
    let errors: Vec<&String> = entries
        .iter()
        .flat_map(|e| &e.parts)
        .filter_map(|p| match p {
            MessagePart::Error { message, .. } => Some(message),
            _ => None,
        })
        .collect();
    assert!(
        errors
            .iter()
            .any(|m| m.contains("no output for 10 minutes")),
        "the transcript must say WHY the turn ended, got {errors:?}"
    );
    assert!(
        entries
            .iter()
            .all(|e| e.status != Some(MessageStatus::Streaming)),
        "no entry may be left streaming"
    );
    core.shutdown().await;
}

/// Tier 2: a run that HAS produced output and then goes quiet is left running.
/// An agent can sit silently inside one `Exec` for far longer than any timeout,
/// and a live child that has already spoken is the working signal — killing it
/// here is exactly the regression the watchdog must not reintroduce.
#[tokio::test(start_paused = true)]
async fn silence_after_real_output_never_kills_the_run() {
    let dir = tempfile::tempdir().expect("tempdir");
    let core = assemble(
        dir.path(),
        SilentHarness {
            preamble: vec![
                AgentEvent::SessionStarted {
                    harness: HarnessId::Mock,
                    model: "mock-1".into(),
                    tools: vec![],
                    cwd: "/tmp".into(),
                    session_id: "sess-1".into(),
                    assistant_message_id: "a-1".into(),
                },
                AgentEvent::TextDelta {
                    text: "running the suite…".into(),
                },
            ],
        },
    );
    seed_chat(&core, CHAT);

    core.sessions
        .dispatch(CHAT, HarnessId::Mock, run_request("run the tests"), None)
        .await
        .expect("dispatch");

    // Two full stall windows: the advisory tier re-arms, so if it were ever
    // going to escalate it would have done so by now.
    tokio::time::sleep(PAST_STALL * 2).await;

    assert_eq!(
        core.sessions.session_status(CHAT).map(|s| s.status),
        Some(SessionStatus::Working),
        "a long silent tool call is not a crash"
    );
    let has_stall_error = assistant_entries(&core, CHAT)
        .iter()
        .flat_map(|e| &e.parts)
        .any(|p| matches!(p, MessagePart::Error { .. }));
    assert!(
        !has_stall_error,
        "no error may be written for tier-2 silence"
    );
    core.shutdown().await;
}

// ── §3.3 boot warm-open ─────────────────────────────────────────────────────

/// Only chats this device hosts, unarchived, and active inside the 14-day
/// window are warm-opened at boot.
#[tokio::test]
async fn boot_warm_opens_only_recent_local_unarchived_chats() {
    let dir = tempfile::tempdir().expect("tempdir");
    let day_ms = 24 * 60 * 60 * 1000i64;
    let now_ms = chrono::Utc::now().timestamp_millis();
    {
        let core = assemble(dir.path(), SilentHarness { preamble: vec![] });
        core.workspace
            .create_space(SPACE, &core.device_id, "/tmp", None, false)
            .expect("create space row");
        for id in ["recent", "old", "archived", "remote", "never-ran"] {
            core.workspace
                .create_chat(id, SPACE, None, None)
                .expect("create chat row");
        }
        core.workspace
            .set_chat_activity("recent", Some(now_ms - day_ms), None)
            .expect("backdate recent");
        core.workspace
            .set_chat_activity("old", Some(now_ms - 20 * day_ms), None)
            .expect("backdate old");
        core.workspace
            .set_chat_activity("archived", Some(now_ms - day_ms), None)
            .expect("backdate archived");
        core.workspace
            .set_chat_archived("archived", true)
            .expect("archive");
        core.workspace
            .set_chat_activity("remote", Some(now_ms - day_ms), None)
            .expect("backdate remote");
        core.workspace
            .set_chat_host("remote", "some-other-device")
            .expect("re-home");
        // `never-ran` keeps its null lastMessageAt: a chat created (possibly on
        // another device) with a run queued and no turn yet is exactly the cold
        // case warm-open exists for, so createdAt has to carry it.
        core.shutdown().await;
    }

    let core = assemble(dir.path(), SilentHarness { preamble: vec![] });
    let open = core.doc_host.open_chats();
    assert!(
        open.contains(&"recent".to_string()),
        "recent chat: {open:?}"
    );
    assert!(
        open.contains(&"never-ran".to_string()),
        "a chat with no turn yet falls back to createdAt: {open:?}"
    );
    assert!(!open.contains(&"old".to_string()), "outside 14d: {open:?}");
    assert!(
        !open.contains(&"archived".to_string()),
        "archived: {open:?}"
    );
    assert!(
        !open.contains(&"remote".to_string()),
        "hosted elsewhere: {open:?}"
    );
    core.shutdown().await;
}

/// The cap holds, and it keeps the NEWEST chats — a device hosting hundreds
/// must not open every one of them (a room socket and a live doc each).
#[tokio::test]
async fn boot_warm_open_caps_at_thirty_newest_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let now_ms = chrono::Utc::now().timestamp_millis();
    {
        let core = assemble(dir.path(), SilentHarness { preamble: vec![] });
        core.workspace
            .create_space(SPACE, &core.device_id, "/tmp", None, false)
            .expect("create space row");
        // 40 chats one minute apart: chat-39 newest, chat-00 oldest.
        for i in 0..40 {
            let id = format!("chat-{i:02}");
            core.workspace
                .create_chat(&id, SPACE, None, None)
                .expect("create chat row");
            core.workspace
                .set_chat_activity(&id, Some(now_ms - (40 - i as i64) * 60_000), None)
                .expect("backdate");
        }
        core.shutdown().await;
    }

    let core = assemble(dir.path(), SilentHarness { preamble: vec![] });
    let open = core.doc_host.open_chats();
    assert_eq!(open.len(), 30, "capped at 30, got {}", open.len());
    assert!(open.contains(&"chat-39".to_string()), "newest kept");
    assert!(open.contains(&"chat-10".to_string()), "30th-newest kept");
    assert!(
        !open.contains(&"chat-09".to_string()),
        "31st-newest dropped"
    );
    core.shutdown().await;
}
