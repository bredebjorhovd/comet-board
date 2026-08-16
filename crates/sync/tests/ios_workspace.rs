//! Cross-language workspace invariants that must run without an iOS simulator.
//!
//! The Swift sync spec exercises the Codable round trip when a simulator is
//! available. CI does not have one, so this source contract prevents a future
//! iOS projection/config edit from silently dropping the board-owned GitHub
//! push tuple again (gh#440).

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root must exist")
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn ios_round_trips_and_preserves_the_board_push_tuple() {
    let entities = read("apps/ios/Comet/Models/Entities.swift");
    let workspace = read("apps/ios/Comet/Sync/WorkspaceStore.swift");
    let spec = read("apps/ios/Comet/App/SyncSpecRunner.swift");

    assert!(
        entities.contains("var pushRepo: String? = nil")
            && entities.contains("var pushContract: GithubPushContract? = nil")
            && entities.contains("struct GithubPushContract: Hashable, Codable"),
        "iOS ChatConfig must encode both halves of the board push tuple"
    );
    assert!(
        workspace.contains("let shadow = m[\"boardPush\"]?.mapValue")
            && workspace.contains("pushRepo = shadow?[\"repo\"]?.stringValue")
            && workspace.contains("pushContract = shadowContract"),
        "the iOS projection must recover an omitted tuple from boardPush"
    );
    assert!(
        workspace.contains("chatConfig.pushRepo = existing.pushRepo")
            && workspace.contains("chatConfig.pushContract = existing.pushContract"),
        "a model/reasoning edit must preserve both tuple halves"
    );
    assert!(
        spec.contains("githubPushTupleRoundTrips()")
            && spec.contains("expect(decoded.pushRepo, config.pushRepo")
            && spec.contains("expect(decoded.pushContract, config.pushContract"),
        "the simulator spec must exercise both Codable fields"
    );
}
