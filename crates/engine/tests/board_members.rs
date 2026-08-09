//! gh#162: the `[users]` map, written over the RPC instead of by hand.
//!
//! What the map *means* — which spellings of a GitHub identity are accepted,
//! what the value must parse as, how a repeat write behaves — is unit-tested in
//! `comet_board::members` and `comet_board::routes`. What needs an engine is
//! the wiring: `WriteBoardConfig` has to recognise the op at all, serve it off
//! the board's own paths, refuse a bad one with the board's reason, and answer
//! with a fresh read of the file that landed.
//!
//! Locally rather than over the relay, which `device_routing.rs` covers: this
//! is about the handler, and a test that has to stand a device room up first
//! fails for reasons that are not the handler's.
//!
//! `--github` is an address throughout. A bare login is resolved by asking
//! GitHub, deliberately on the *board's* credential (the laptop running
//! `member add` usually has none), and a test must not depend on the network to
//! prove the writer works.

use std::sync::Arc;

use comet_board::config::Paths;
use comet_engine::{EngineCore, HarnessRegistry};
use comet_harness::mock::MockHarness;
use comet_proto::HarnessId;
use comet_rpc::methods;

/// Board paths under `dir` — the same derivation the engine makes from its own
/// data dir, and since gh#190 a pure one: the board's two directory variables
/// are set in the environment of every board-dispatched agent, and
/// [`Paths::under`] no longer lets them overrule the directory it was handed.
/// `board_env_isolation.rs` is the test that keeps it that way.
fn board_paths(dir: &std::path::Path) -> Paths {
    Paths::under(dir).expect("board dirs")
}

/// An engine with a board service of its own, on a tempdir nobody else shares.
fn box_with_a_board(dir: &std::path::Path) -> (EngineCore, Paths) {
    let data = dir.join("engine");
    std::fs::create_dir_all(&data).expect("data dir");
    std::fs::write(data.join("device-id"), "device-box").expect("device id");
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(MockHarness { script: Vec::new() }));
    let core = EngineCore::assemble(&data, Arc::new(registry), HarnessId::Mock, None)
        .expect("engine assembles");

    let paths = board_paths(dir);
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
        paths.clone(),
        core.workspace
            .merged_sessions_watch(core.sessions.watch_sessions()),
        runtime,
        core.workspace.watch_spaces(),
        tokio::runtime::Handle::current(),
    )
    .expect("board service");
    core.set_board(Arc::new(board));
    (core, paths)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn member_add_writes_the_users_map_and_member_remove_takes_it_back_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (core, paths) = box_with_a_board(dir.path());
    let client = comet_rpc::memory_client(core.rpc_service());

    // A box with routes and no map: the single-operator default, and the state
    // in which every teammate's dispatch commits as the box.
    let text = "[[route]]\nmatch = { gh_repo = \"o/r\" }\nworkspace = \"box\"\n\
                repo = \"/tmp\"\nruntime = \"claude-code\"\n";
    client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({ "op": "text", "text": text }),
        )
        .await
        .expect("first config");

    let wrote = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "member", "user": "ana@example.com",
                "github": "22494697+ana@users.noreply.github.com"
            }),
        )
        .await
        .expect("member add");
    assert_eq!(
        wrote["routing"]["config"]["users"]["ana@example.com"].as_str(),
        Some("22494697+ana@users.noreply.github.com"),
        "the reply is a fresh read of what landed: {wrote}"
    );
    // On the board's own paths, which is what the dispatch reads at release
    // time — not on the engine's data dir, and not somewhere only this reply
    // knows about.
    let on_disk = std::fs::read_to_string(paths.routing()).expect("routing.toml");
    assert!(on_disk.contains("[users]"), "got: {on_disk}");
    assert!(
        on_disk.contains(text),
        "the routes are untouched: {on_disk}"
    );
    assert!(
        comet_board::adopt::backup_path(&paths.routing()).exists(),
        "a write leaves the previous contents beside it"
    );

    // `--name` is the one thing an address cannot carry when it was a login.
    let named = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "member", "user": "ana@example.com",
                "github": "22494697+ana@users.noreply.github.com", "name": "Ana Ruiz"
            }),
        )
        .await
        .expect("member add --name");
    assert_eq!(
        named["routing"]["config"]["users"]["ana@example.com"].as_str(),
        Some("Ana Ruiz <22494697+ana@users.noreply.github.com>"),
        "got: {named}"
    );
    assert_eq!(
        named["routing"]["config"]["users"]
            .as_object()
            .map(|m| m.len()),
        Some(1),
        "one person, one entry: {named}"
    );

    // The refusal, with the board's own words and the file left alone. A value
    // that is not an address would land on GIT_AUTHOR_EMAIL and produce the
    // unattributable commits the map exists to prevent.
    let before = std::fs::read_to_string(paths.routing()).expect("routing.toml");
    let refused = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({ "op": "member", "user": "sam@example.com", "github": "Sam Ito" }),
        )
        .await
        .expect_err("a name is not a GitHub identity");
    assert!(
        refused.to_string().contains("GitHub login"),
        "the refusal says what to type instead: {refused}"
    );
    // And the other half: a key that is not a sign-in email writes an entry no
    // dispatch could ever match.
    let bad_key = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({
                "op": "member", "user": "ana",
                "github": "22494697+ana@users.noreply.github.com"
            }),
        )
        .await
        .expect_err("a login is not a sign-in email");
    assert!(
        bad_key.to_string().contains("sign-in email"),
        "got: {bad_key}"
    );
    assert_eq!(
        std::fs::read_to_string(paths.routing()).expect("routing.toml"),
        before,
        "a refused write leaves the config the board is running on untouched"
    );

    // Offboarding: the primitive op, with a null value.
    let removed = client
        .call(
            methods::WRITE_BOARD_CONFIG,
            serde_json::json!({ "op": "user", "user": "ANA@Example.com", "value": null }),
        )
        .await
        .expect("member remove");
    assert!(
        removed["routing"]["config"]["users"]
            .as_object()
            .is_some_and(|m| m.is_empty()),
        "removal is case-insensitive, like every read of the map: {removed}"
    );
    assert!(
        removed["routing"]["problems"]
            .as_array()
            .is_some_and(|p| p.is_empty()),
        "got: {removed}"
    );

    core.shutdown().await;
}
