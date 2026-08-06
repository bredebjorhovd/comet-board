//! gh#66 integration: what a SECOND person in the org sees.
//!
//! Two engines here are two different WorkOS users — the box (alice) and a
//! teammate's laptop (bob) — not two devices of one user. That distinction is
//! the whole issue: every sync surface was per-user, so a teammate signed in and
//! found an empty world.
//!
//! The in-memory bridges below stand in for the edge rooms, cross-importing Loro
//! updates exactly as `RoomClient` + the SessionRoom DO do over the wire. Which
//! docs are bridged is itself part of each test's claim: the org device registry
//! is shared, the per-user workspace docs are NOT, and a chat doc is shared only
//! because the board shared that chat.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;

use comet_doc::{
    CommandBasedOn, SessionCommandEntry, SessionCommandPayload, SessionCommandStatus, WorkspaceDoc,
};
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::{Harness, HarnessError, RunControls};
use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SandboxLevel,
    SteeringMode,
};

const ORG: &str = "org-team";

/// Mock harness that would answer instantly — so "nothing ran" in the tests
/// below is a decision, never a missing harness.
struct InstantHarness;

#[async_trait]
impl Harness for InstantHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Instant"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        _request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        Ok(futures::stream::iter([
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "instant-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-1".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-1".into()),
            }),
        ])
        .boxed())
    }
}

fn registry() -> Arc<HarnessRegistry> {
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(InstantHarness));
    Arc::new(registry)
}

/// An offline engine signed in as `user` of [`ORG`], with a fixed device id.
fn assemble(dir: &std::path::Path, device_id: &str, user: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    EngineCore::assemble_with_identity(dir, registry(), HarnessId::Mock, None, ORG, user)
        .expect("engine core assembles")
}

/// Cross-import two docs on a timer — the in-memory stand-in for one room.
fn bridge(a: Arc<WorkspaceDoc>, b: Arc<WorkspaceDoc>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        loop {
            tick.tick().await;
            for (from, to) in [(a.doc(), b.doc()), (b.doc(), a.doc())] {
                if let Ok(update) = from.export(loro::ExportMode::updates(&to.oplog_vv()))
                    && !update.is_empty()
                {
                    let _ = to.import(&update);
                }
            }
        }
    })
}

/// One-way import, `from` → `to`: the teammate receives the box's chat doc, and
/// their own writes stay put, so a test can assert on what the TEAMMATE's engine
/// did with them.
fn bridge_one_way(
    from: Arc<comet_doc::SessionDoc>,
    to: Arc<comet_doc::SessionDoc>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        loop {
            tick.tick().await;
            if let Ok(update) = from
                .doc()
                .export(loro::ExportMode::updates(&to.doc().oplog_vv()))
                && !update.is_empty()
            {
                let _ = to.doc().import(&update);
            }
        }
    })
}

async fn wait_for<F>(mut predicate: F, what: &str)
where
    F: FnMut() -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Gate 1: a teammate must be able to SEE the box. Devices are org-wide;
/// everything else stays private to the user who made it.
#[tokio::test]
async fn a_teammate_sees_the_orgs_devices_and_nothing_else() {
    let dir_box = tempfile::tempdir().unwrap();
    let dir_mate = tempfile::tempdir().unwrap();
    let the_box = assemble(dir_box.path(), "dev-box", "alice");
    let mate = assemble(dir_mate.path(), "dev-mate", "bob");

    // ONLY the org registry is bridged — the per-user workspace rooms are
    // different rooms and could never carry each other's rows.
    let link = bridge(
        the_box.workspace.org_devices().doc_arc(),
        mate.workspace.org_devices().doc_arc(),
    );

    // Alice's private world: a space and a chat on the box.
    the_box
        .workspace
        .create_space("space-box", "dev-box", "/tmp/box", None, false)
        .expect("space on the box");
    the_box
        .workspace
        .create_chat("chat-box", "space-box", None, None)
        .expect("chat on the box");

    // Asserted on the `WatchDevices` watch itself — the stream the device
    // switcher renders and the board's host sweep takes its candidates from.
    for core in [&the_box, &mate] {
        wait_for(
            || {
                let mut ids: Vec<String> = core
                    .workspace
                    .watch_devices()
                    .borrow()
                    .iter()
                    .map(|d| d.id.clone())
                    .collect();
                ids.sort();
                ids == ["dev-box", "dev-mate"]
            },
            "both device rows on the WatchDevices stream",
        )
        .await;
    }

    let devices = mate.workspace.watch_devices().borrow().clone();
    let local = mate.device_id.clone();
    let candidates = comet_proto::view::board::host_candidates(&devices, Some(&local));
    assert!(
        candidates.contains(&Some("dev-box".to_string())),
        "the board sweep must have the box to visit: {candidates:?}"
    );

    // …and nothing of alice's private index leaked with it.
    assert!(
        mate.workspace.read_spaces().unwrap().is_empty(),
        "spaces stay private to their owner"
    );
    assert!(
        mate.workspace.doc().read_chats().unwrap().is_empty(),
        "chats stay private to their owner"
    );

    link.abort();
    the_box.shutdown().await;
    mate.shutdown().await;
}

/// Gate 3: a teammate opening a chat the board dispatched reads it — and does
/// NOT execute its commands. Their per-user workspace has no row for it, and
/// "no row" used to mean "unclaimed, so mine to run": the teammate's laptop
/// would have run the box's work a second time, in a folder that only exists on
/// the box. The session doc's own `hostDeviceId` is what settles it.
#[tokio::test]
async fn a_teammate_opening_a_shared_chat_never_executes_it() {
    let dir_box = tempfile::tempdir().unwrap();
    let dir_mate = tempfile::tempdir().unwrap();
    let the_box = assemble(dir_box.path(), "dev-box", "alice");
    let mate = assemble(dir_mate.path(), "dev-mate", "bob");

    // The box dispatches: a chat row of its own, then the doc.
    the_box
        .workspace
        .create_space("space-box", "dev-box", "/tmp/box", None, false)
        .expect("space on the box");
    the_box
        .workspace
        .create_chat("chat-dispatched", "space-box", None, None)
        .expect("dispatched chat row");
    let box_handle = the_box
        .doc_host
        .open("chat-dispatched")
        .expect("open on the box");
    assert_eq!(
        box_handle.doc().host_device_id().as_deref(),
        Some("dev-box"),
        "the host stamps itself into the doc it hosts"
    );

    // The teammate opens the same chat (from the board panel) and syncs it.
    let mate_handle = mate
        .doc_host
        .open("chat-dispatched")
        .expect("open on the teammate's laptop");
    assert!(
        mate.workspace
            .doc()
            .chat("chat-dispatched")
            .unwrap()
            .is_none(),
        "the teammate has no workspace row for it — that is the whole hazard"
    );
    let link = bridge_one_way(box_handle.doc_arc(), mate_handle.doc_arc());
    wait_for(
        || mate_handle.doc().host_device_id().as_deref() == Some("dev-box"),
        "the host stamp reaches the teammate",
    )
    .await;

    // The teammate steers: a run command lands in their copy of the doc.
    let now = chrono::Utc::now().timestamp_millis();
    mate_handle
        .doc()
        .queue_command(&SessionCommandEntry {
            id: "cmd-from-the-teammate".into(),
            payload: SessionCommandPayload::Run {
                request: RunRequest {
                    prompt: "carry on".into(),
                    model: None,
                    reasoning: None,
                    model_options: Default::default(),
                    cwd: "/tmp/box".into(),
                    sandbox: SandboxLevel::WorkspaceWrite,
                    auto_approve: true,
                    attachments: Vec::new(),
                    resume: None,
                },
                message_id: "m-mate-1".into(),
            },
            issued_by: "dev-mate".into(),
            issued_at: now,
            based_on: None::<CommandBasedOn>,
            expires_at: None,
            status: SessionCommandStatus::Pending,
            resolution: None,
        })
        .expect("queue command");

    tokio::time::sleep(Duration::from_millis(400)).await;
    let commands = mate_handle.doc().read_commands().expect("read commands");
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].status,
        SessionCommandStatus::Pending,
        "the command waits for the box, which is where the work is"
    );
    assert!(
        mate_handle.doc().read_entries().unwrap().is_empty(),
        "the teammate's engine must write no transcript entries"
    );
    assert!(
        mate.sessions.session_status("chat-dispatched").is_none(),
        "and must start no run"
    );

    link.abort();
    the_box.shutdown().await;
    mate.shutdown().await;
}
