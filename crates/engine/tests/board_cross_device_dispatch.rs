//! gh#558: one board, two machines — a dispatch whose route names a space the
//! *other* device hosts.
//!
//! This is the proving step the consolidation ticket asked for, as a test
//! rather than as a throwaway ticket on the live board. The shape it proves is
//! the one the box is meant to run: a single board, hosted where nothing
//! sleeps, polling `comet-board` and sending every attempt on it into the Mac's
//! checkout — because comet-board has iOS and macOS targets that cannot build
//! on Linux.
//!
//! Most of that shape was already there. A route names a space, a space is a
//! device+folder pair, and the board's space lookup never filtered to the local
//! device; `DispatchSpec` has always carried `device_id`; `create_chat` stamps
//! the chat with `space.device_id`, so the conversation, its command ledger and
//! its runs belong to the space's own machine either way. **The checkout did
//! not.** `CometRuntime::dispatch` cut it with the local `Repos`, against a
//! path that exists on the other device — so the box would have refused every
//! comet dispatch with a git error about a directory it does not have, and the
//! ticket's "this is a config change, not a feature" was wrong by one call.
//!
//! Two facts, and the second is why this is a whole test binary rather than two
//! more cases in `device_routing.rs`:
//!
//! 1. The checkout is cut **on the space's device**. Provable only if the two
//!    engines have different worktree roots, and that root comes from a process
//!    variable (`COMET_WORKTREES_DIR`) read once, inside `Repos::new`. One test
//!    per binary means the two `set_var`s below sit between the two assembles
//!    with nothing else in the process reading them — the same argument
//!    `board_runtime_e2e.rs` makes for its single `set_var`.
//! 2. A board with no relay refuses such a dispatch **by name**, before cutting
//!    anything. A cross-device route on a box that cannot reach the other
//!    machine has to fail as a `RefusedBeforeCut` naming the device, not as a
//!    git error about a missing directory — the operator's next move is
//!    different in each case.
//!
//! The relay here is the route-only half of the device room. `device_routing.rs`
//! owns the authorization half (gh#66) and tests it against the same code;
//! repeating the org gate here would only be a second copy of it to keep true.

// The websocket handshake callback returns tungstenite's own `ErrorResponse` —
// its size is not ours to shrink. Same allow, same reason, as `device_routing.rs`.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::handshake::server::{
    Request as WsRequest, Response as WsResponse,
};

use comet_engine::{EngineCore, HarnessRegistry};
use comet_proto::HarnessId;
use comet_rpc::{
    DeviceFrameHeader, LinkCache, LinkCacheConfig, StaticToken, decode_device_frame,
    encode_device_frame,
};

const ORG: &str = "org-558";
const OWNER: &str = "alice";
const BOX_DEVICE: &str = "device-box";
const MAC_DEVICE: &str = "device-mac";

// ---------------------------------------------------------------------------
// A route-only device room: one host, many clients, frames moved between them.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RelayState {
    host: Option<mpsc::UnboundedSender<Vec<u8>>>,
    clients: HashMap<String, mpsc::UnboundedSender<Vec<u8>>>,
}

async fn fake_device_room() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
    let url = format!(
        "http://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let state = Arc::new(Mutex::new(RelayState::default()));
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let state = state.clone();
            tokio::spawn(async move {
                let mut uri = String::new();
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |req: &WsRequest, res: WsResponse| {
                        uri = req.uri().to_string();
                        Ok(res)
                    },
                )
                .await
                else {
                    return;
                };
                let query = uri.split_once('?').map(|(_, q)| q).unwrap_or("");
                let is_host = query.contains("role=host");
                let conn_id = query
                    .split('&')
                    .find_map(|kv| kv.strip_prefix("connId="))
                    .unwrap_or("anon")
                    .to_string();
                let (mut sink, mut ws_stream) = ws.split();
                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                {
                    let mut st = state.lock().expect("lock");
                    if is_host {
                        st.host = Some(tx);
                    } else {
                        st.clients.insert(conn_id.clone(), tx);
                    }
                }
                let writer = tokio::spawn(async move {
                    while let Some(bytes) = rx.recv().await {
                        if sink.send(WsMessage::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                });
                while let Some(Ok(message)) = ws_stream.next().await {
                    let WsMessage::Binary(bytes) = message else {
                        continue;
                    };
                    let Ok((header, payload)) = decode_device_frame(&bytes) else {
                        break;
                    };
                    let st = state.lock().expect("lock");
                    if is_host {
                        let Some(to) = header.to else { continue };
                        if let Some(client) = st.clients.get(&to) {
                            let stripped = DeviceFrameHeader::new(header.s, header.k);
                            let _ = client
                                .send(encode_device_frame(&stripped, &payload).expect("encode"));
                        }
                    } else if let Some(host) = &st.host {
                        let mut routed = DeviceFrameHeader::new(header.s, header.k);
                        routed.from = Some(conn_id.clone());
                        routed.u = Some(OWNER.to_string());
                        routed.o = Some(ORG.to_string());
                        let _ = host.send(encode_device_frame(&routed, &payload).expect("encode"));
                    }
                }
                writer.abort();
            });
        }
    });
    (url, task)
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn bearer() -> String {
    format!("{OWNER}@{ORG}")
}

fn assemble(dir: &std::path::Path, device_id: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    let registry = Arc::new(HarnessRegistry::new());
    let core =
        EngineCore::assemble(dir, registry, HarnessId::Mock, None).expect("engine assembles");
    let mut auth = comet_engine::AuthConfig::new("http://localhost:27640", dir.join("auth"));
    auth.dev_user_id = bearer();
    core.set_auth(comet_engine::Auth::new(auth));
    core
}

fn links(relay_url: &str) -> Arc<LinkCache> {
    let mut config = LinkCacheConfig::new(relay_url.to_string(), Arc::new(StaticToken(bearer())));
    config.probe_timeout = Duration::from_secs(5);
    config.cooldown_base = Duration::from_millis(50);
    config.cooldown_max = Duration::from_millis(200);
    LinkCache::new(config)
}

/// The board on the box, with the runtime kept so the relay can be handed to it
/// the way `EngineRuntime::assemble` does in production.
fn board_on(
    core: &EngineCore,
    paths: comet_board::config::Paths,
) -> (comet_engine::BoardService, Arc<comet_engine::CometRuntime>) {
    let runtime = Arc::new(comet_engine::CometRuntime::new(
        core.repos.clone(),
        core.workspace.clone(),
        core.doc_host.clone(),
        core.workspace
            .merged_sessions_watch(core.sessions.watch_sessions()),
        core.sessions.journal(),
        core.agent_accounts.clone(),
        tokio::runtime::Handle::current(),
    ));
    let board = comet_engine::BoardService::spawn_at(
        paths,
        core.workspace
            .merged_sessions_watch(core.sessions.watch_sessions()),
        runtime.clone(),
        core.workspace.watch_spaces(),
        tokio::runtime::Handle::current(),
    )
    .expect("board service");
    (board, runtime)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dispatch_whose_space_is_on_another_device_cuts_its_checkout_there() {
    use comet_board::db::{Db, UpsertTask};
    use comet_board::model::{Source, UpstreamState};

    let dirs = tempfile::tempdir().expect("tempdir");
    let (relay_url, _relay) = fake_device_room().await;

    // The repo the route points at. On the real fleet this path exists only on
    // the Mac; here one filesystem holds both engines, so what separates them
    // is where each one puts the checkouts it cuts — see the module comment.
    let origin = dirs.path().join("origin");
    std::fs::create_dir_all(&origin).expect("origin dir");
    git(&origin, &["init", "-b", "main"]);
    git(&origin, &["config", "user.email", "t@t"]);
    git(&origin, &["config", "user.name", "t"]);
    git(&origin, &["commit", "--allow-empty", "-m", "root"]);
    let repo = dirs.path().join("comet-board");
    git(
        dirs.path(),
        &["clone", &origin.to_string_lossy(), &repo.to_string_lossy()],
    );

    // The Mac: the device that holds the space, and the only one that should
    // ever cut a checkout for it.
    let mac_worktrees = dirs.path().join("mac-worktrees");
    unsafe { std::env::set_var("COMET_WORKTREES_DIR", &mac_worktrees) };
    let core_mac = assemble(&dirs.path().join("mac"), MAC_DEVICE);
    let _mac_host = core_mac.start_host_relay(&relay_url);

    // The box: hosts the board, holds no checkout of this repo, and must not
    // grow one. Anything it cuts lands here, where the assertion can see it.
    let box_worktrees = dirs.path().join("box-worktrees");
    unsafe { std::env::set_var("COMET_WORKTREES_DIR", &box_worktrees) };
    let core_box = assemble(&dirs.path().join("box"), BOX_DEVICE);

    let paths = comet_board::config::Paths::under(&dirs.path().join("board")).expect("board dirs");
    {
        let db = Db::open(&paths.db()).expect("board store");
        db.upsert_task(&UpsertTask {
            // Deliberately not a `gh:owner/repo#n` id, and matched by label:
            // the board's credential preflight (gh#440) probes GitHub before it
            // cuts anything, and this test is about which machine cuts. A task
            // that names no GitHub repo, in a clone whose `origin` is a local
            // path, resolves `push_repo` to `None` and skips the probe — the
            // preflight has tests of its own that are actually about it.
            id: "board-558".into(),
            source: Source::Github,
            source_id: "558".into(),
            identifier: "gh#558".into(),
            title: "consolidate to one board".into(),
            body: None,
            url: "https://github.com/o/comet-board/issues/558".into(),
            labels: vec!["cross-device".into()],
            source_state: Some("open".into()),
            upstream: UpstreamState::Unstarted,
            updated_at: "2026-08-22T09:00:00Z".into(),
        })
        .expect("seed task");
    }
    // `base = "HEAD"` is the documented opt-out from the origin fetch (gh#67):
    // this test is about which machine cuts the branch, not about where the
    // branch starts, and a fetch would only add a second thing that can fail.
    std::fs::write(
        paths.routing(),
        format!(
            "[[route]]\nmatch = {{ label = \"cross-device\" }}\n\
             workspace = \"comet-board-laptop\"\nrepo = \"{}\"\n\
             runtime = \"mock\"\nbase = \"HEAD\"\n",
            repo.display()
        ),
    )
    .expect("write routing.toml");

    // The space the route names — on the Mac, in the box's own workspace doc,
    // which is exactly what the synced doc gives the box in production.
    core_box
        .workspace
        .create_space(
            "space-mac",
            MAC_DEVICE,
            &repo.to_string_lossy(),
            Some("comet-board-laptop".into()),
            true,
        )
        .expect("create the Mac's space in the box's doc");

    let (board, runtime) = board_on(&core_box, paths);
    core_box.set_board(Arc::new(board));

    // ---- 1. no relay: refused by name, and nothing is cut ------------------
    let refused = core_box
        .board()
        .expect("board")
        .dispatch_task(
            "board-558",
            comet_board::dispatch::DispatchOrigin::default(),
            Default::default(),
        )
        .await
        .expect_err("a board with no relay cannot reach the device holding the space");
    let refused = format!("{refused:#}");
    assert!(
        refused.contains(MAC_DEVICE),
        "the refusal names the device it could not reach: {refused}"
    );
    assert!(
        refused.contains("relay"),
        "…and why, so the repair is 'sign in / bring the edge up' rather than \
         'make a directory': {refused}"
    );
    assert!(
        !box_worktrees.exists(),
        "a refused dispatch cuts nothing anywhere, least of all here"
    );

    // ---- 2. with the relay: the Mac cuts it --------------------------------
    let cache = links(&relay_url);
    core_box.set_links(cache.clone());
    runtime.set_links(cache);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let dispatched = loop {
        match core_box
            .board()
            .expect("board")
            .dispatch_task(
                "board-558",
                comet_board::dispatch::DispatchOrigin::default(),
                Default::default(),
            )
            .await
        {
            Ok(d) => break d,
            Err(e) => {
                let text = format!("{e:#}");
                // The host relay dials with backoff; ride over it the way the
                // forwarding tests do.
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the cross-device dispatch never went through: {text}"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };

    assert!(
        dispatched
            .cwd
            .starts_with(&*mac_worktrees.to_string_lossy()),
        "the attempt's checkout is cut by the device that holds the space: {}",
        dispatched.cwd
    );
    assert!(
        !box_worktrees.exists(),
        "and never by the device that holds the board — {} should not exist",
        box_worktrees.display()
    );
    assert!(
        std::path::Path::new(&dispatched.cwd).join(".git").exists(),
        "the cut is a real checkout, not a path the reply invented: {}",
        dispatched.cwd
    );

    // The chat belongs to the Mac too — it always did (`create_chat` stamps
    // `space.device_id`), and it is what makes the run, its journal and its
    // transcript happen where the checkout is rather than where the board is.
    let chat = core_box
        .workspace
        .doc()
        .chat(&dispatched.chat_id)
        .expect("read the chat")
        .expect("the dispatch made one");
    assert_eq!(
        chat.device_id, MAC_DEVICE,
        "the attempt's conversation is hosted where its checkout is"
    );
    assert_eq!(chat.cwd.as_deref(), Some(dispatched.cwd.as_str()));

    core_mac.shutdown().await;
    core_box.shutdown().await;
}
