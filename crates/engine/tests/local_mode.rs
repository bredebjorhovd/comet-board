//! H9: edge-less (local) mode. `COMET_EDGE_URL=off` is a stated configuration,
//! not graceful failure: no edge transport may be built — no room joins, no
//! peer link cache / device room, no release poller — and auth is forced to
//! dev mode even when a WorkOS client id (or dev bearer) is configured.

use comet_engine::{Engine, EngineConfig, HarnessId, edge_url_is_off};

fn local_config(data_dir: &std::path::Path) -> EngineConfig {
    EngineConfig {
        data_dir: data_dir.to_path_buf(),
        edge_url: "off".into(),
        // A dev bearer would normally flip the engine online — local mode wins.
        edge_token: Some("dev-user".into()),
        ipc_port: 0,
        default_harness: HarnessId::Mock,
        org_id: None,
        // A configured client id would normally probe {edge}/health and gate
        // on sign-in — local mode forces dev auth instead.
        workos_client_id: Some("client_test".into()),
        // The board polls trackers over its own HTTP, not the edge — H9 is
        // about edge transports, so keep it out of this test's world.
        board: false,
    }
}

#[test]
fn off_sentinel_matching() {
    assert!(edge_url_is_off("off"));
    assert!(edge_url_is_off(" OFF "));
    assert!(!edge_url_is_off("https://edge.example"));
    assert!(!edge_url_is_off(""));
}

#[tokio::test]
async fn edge_off_builds_no_edge_transports() {
    let dir = tempfile::tempdir().unwrap();
    let config = local_config(dir.path());
    assert!(!config.edge_enabled());

    let auth = Engine::build_auth(&config).await;
    assert!(!auth.workos_enabled(), "local mode must force dev auth");

    let runtime = Engine::assemble_runtime(&config, auth).await.unwrap();
    let core = runtime.core();
    assert!(core.links().is_none(), "no peer link cache in local mode");
    assert!(core.updater().is_none(), "no release poller in local mode");

    // Opening a chat must not spawn a room join (the pre-H9 behavior logged a
    // "session room join failed" warning per open); the handle stays local.
    let handle = core.doc_host.open("chat-local").unwrap();
    assert!(!handle.connected());

    runtime.shutdown().await;
}
