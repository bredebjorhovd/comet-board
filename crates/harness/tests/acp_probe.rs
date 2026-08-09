//! ACP evaluation probe (gh#147) — the two measurements the decision in
//! `docs/research/acp.md` rests on, kept runnable so they can be re-taken
//! against a newer adapter instead of re-argued.
//!
//! Both are `#[ignore]`: they fetch the real org adapters through `npx -y` and
//! spawn them. They are not part of `cargo test` and never run in CI.
//!
//! ```text
//! cargo test -p comet-harness --test acp_probe -- --ignored --nocapture
//! ```
//!
//! What each one settles, and why the answer is load-bearing:
//!
//! - [`config_dir_env_still_picks_the_login_through_the_acp_adapter`] — the
//!   account slots (gh#59) work by pointing a child at a config dir of its
//!   own, and nothing else. Under ACP the child is the *adapter*, one process
//!   further out than the CLI that reads the variable. If the variable stopped
//!   carrying that far, every per-teammate login on the board would silently
//!   collapse onto whichever account the box itself is signed into — a dispatch
//!   would still run, and bill the wrong person. Costs nothing to check: no
//!   turn is taken, so no tokens are spent.
//! - [`prompt_responses_carry_all_four_token_buckets`] — the board prices
//!   attempts per account (gh#151, `comet_board::prices`) out of
//!   [`comet_proto::TokenUsage`]'s four disjoint buckets, and cache reads are
//!   most of a Claude turn. Upstream's ACP mapping reads `inputTokens` and
//!   `outputTokens` only; whether that is a lossy *wire* or merely a lossy
//!   *mapping* decides whether billing survives a conversion. This one takes a
//!   real turn on whatever login the box holds — a few tokens, and a live
//!   `claude`/`codex` session is required.

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;

use comet_harness::jsonrpc::{Incoming, RpcClient};

const CLAUDE_ADAPTER: &str = "@agentclientprotocol/claude-agent-acp@0.66.0";
const CODEX_ADAPTER: &str = "@agentclientprotocol/codex-acp@1.1.14";

/// A cold `npx -y` may have to fetch the adapter before it says anything.
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(180);
/// A real turn, on a real model.
const TURN_DEADLINE: Duration = Duration::from_secs(240);

/// One adapter under `npx -y`, with `env` stamped on the child, driven far
/// enough to answer `initialize` and attempt `session/new`.
struct Adapter {
    child: tokio::process::Child,
    client: RpcClient,
    drain: tokio::task::JoinHandle<()>,
}

impl Adapter {
    async fn spawn(package: &str, cwd: &std::path::Path, env: &[(&str, &std::path::Path)]) -> Self {
        let mut cmd = Command::new("npx");
        cmd.arg("-y").arg(package).current_dir(cwd);
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("npx must be on PATH to run the ACP probe");
        let (client, incoming) = RpcClient::new(
            child.stdin.take().expect("stdin"),
            child.stdout.take().expect("stdout"),
        );
        // The incoming channel has to be drained for the whole run, not just
        // read at the end: the reader task gives up the moment a send fails,
        // and a dropped receiver takes every pending *response* down with it.
        // Draining is also where permission requests get answered — an
        // unanswered one parks the turn until the deadline, which is exactly
        // the auto-accept the shared harness does in unattended mode.
        let drain = tokio::spawn(drain_incoming(client.clone(), incoming));
        Self {
            child,
            client,
            drain,
        }
    }

    /// `initialize` with fs/terminal capabilities declined — the handshake the
    /// shared harness would send.
    async fn initialize(&self) -> Value {
        let params = json!({
            "protocolVersion": 1,
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false,
            },
        });
        tokio::time::timeout(HANDSHAKE_DEADLINE, self.client.request("initialize", params))
            .await
            .expect("initialize timed out")
            .expect("initialize failed")
    }

    async fn new_session(&self, cwd: &std::path::Path) -> Result<Value, String> {
        let params = json!({ "cwd": cwd.display().to_string(), "mcpServers": [] });
        tokio::time::timeout(HANDSHAKE_DEADLINE, self.client.request("session/new", params))
            .await
            .expect("session/new timed out")
            .map_err(|e| e.to_string())
    }

    async fn prompt(&self, session_id: &str, text: &str) -> Value {
        let params = json!({
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": text }],
        });
        tokio::time::timeout(TURN_DEADLINE, self.client.request("session/prompt", params))
            .await
            .expect("session/prompt timed out")
            .expect("session/prompt failed")
    }

    async fn shutdown(mut self) {
        let _ = self.child.kill().await;
        self.drain.abort();
    }
}

/// Discard session updates; auto-accept permission requests with the agent's
/// preferred allow option (`allow_always` > `allow_once` > first offered).
async fn drain_incoming(client: RpcClient, mut incoming: tokio::sync::mpsc::Receiver<Incoming>) {
    while let Some(message) = incoming.recv().await {
        match message {
            Incoming::Request { id, method, params } if method == "session/request_permission" => {
                let options = params
                    .get("options")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let pick = |kind: &str| {
                    options
                        .iter()
                        .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
                };
                let chosen = pick("allow_always")
                    .or_else(|| pick("allow_once"))
                    .or_else(|| options.first())
                    .and_then(|o| o.get("optionId"))
                    .cloned();
                match chosen {
                    Some(option_id) => client.respond(
                        &id,
                        json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
                    ),
                    None => client.respond(&id, json!({ "outcome": { "outcome": "cancelled" } })),
                }
            }
            // Anything else the adapter asks of a client we declined at
            // initialize (fs, terminal): refuse by method-not-found rather
            // than leave it hanging.
            Incoming::Request { id, .. } => {
                client.respond_error(&id, -32601, "probe declines client capabilities");
            }
            Incoming::Notification { .. } => {}
            Incoming::Eof => return,
        }
    }
}

/// gh#59 survives the extra hop: `CLAUDE_CONFIG_DIR` / `CODEX_HOME` still
/// relocate the whole config dir when the variable is read by an adapter's
/// child rather than by the adapter itself.
///
/// Asserted two different ways, because the two CLIs fail differently on an
/// empty home and only one of them is a clean signal:
///
/// - claude materializes its config into the dir it was pointed at, so the
///   proof is that `.claude.json` appears *there* and not under `~/.claude`.
/// - codex refuses `session/new` outright ("Authentication required"), which
///   is the stronger proof: a fresh `CODEX_HOME` has no login, so the refusal
///   is the adapter reading exactly the dir we named. It is also better than
///   what the app-server path gives us today, where a signed-out home surfaces
///   as a failed turn rather than a failed session.
#[tokio::test]
#[ignore = "spawns the real ACP adapters via npx"]
async fn config_dir_env_still_picks_the_login_through_the_acp_adapter() {
    let work = tempfile::tempdir().expect("tempdir");

    let claude_home = work.path().join("claude-config");
    std::fs::create_dir_all(&claude_home).unwrap();
    let claude = Adapter::spawn(
        CLAUDE_ADAPTER,
        work.path(),
        &[("CLAUDE_CONFIG_DIR", claude_home.as_path())],
    )
    .await;
    claude.initialize().await;
    // Session creation is what makes the CLI touch its config dir.
    let _ = claude.new_session(work.path()).await;
    claude.shutdown().await;
    assert!(
        claude_home.join(".claude.json").exists(),
        "claude-agent-acp must write into CLAUDE_CONFIG_DIR ({}), or the \
         account slots point at nothing: {:?}",
        claude_home.display(),
        std::fs::read_dir(&claude_home)
            .map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
            .unwrap_or_default(),
    );

    let codex_home = work.path().join("codex-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    let codex = Adapter::spawn(
        CODEX_ADAPTER,
        work.path(),
        &[("CODEX_HOME", codex_home.as_path())],
    )
    .await;
    codex.initialize().await;
    let refused = codex.new_session(work.path()).await;
    codex.shutdown().await;
    let error = refused.expect_err("a fresh CODEX_HOME holds no login, so session/new must refuse");
    assert!(
        error.to_lowercase().contains("auth"),
        "codex-acp must refuse an unauthenticated CODEX_HOME by name, got: {error}"
    );
}

/// The four buckets `comet_proto::TokenUsage` bills on all arrive on the
/// settled `session/prompt` response — so a conversion would keep the board's
/// per-account spend intact, and upstream's two-field mapping is a lossy
/// *reading* of a lossless wire rather than a lost capability.
///
/// Spelling differs from the CLI paths (`cachedReadTokens` /
/// `cachedWriteTokens`, camel), and codex reports no cache-write bucket at all
/// — it never writes one, matching the app-server path we normalize today. The
/// buckets are disjoint on both adapters: they sum to the reported
/// `totalTokens`, which is the property `TokenUsage::total` already assumes.
#[tokio::test]
#[ignore = "takes a real turn on the box's own login"]
async fn prompt_responses_carry_all_four_token_buckets() {
    let work = tempfile::tempdir().expect("tempdir");

    for (package, cache_write_expected) in [(CLAUDE_ADAPTER, true), (CODEX_ADAPTER, false)] {
        let adapter = Adapter::spawn(package, work.path(), &[]).await;
        adapter.initialize().await;
        let session = adapter
            .new_session(work.path())
            .await
            .unwrap_or_else(|e| panic!("{package}: session/new failed ({e}) — is it signed in?"));
        let session_id = session["sessionId"].as_str().expect("sessionId").to_owned();

        let settled = adapter.prompt(&session_id, "Reply with exactly: ok").await;
        adapter.shutdown().await;

        let usage = settled
            .get("usage")
            .unwrap_or_else(|| panic!("{package}: settled prompt response carries no usage"));
        let count = |key: &str| usage.get(key).and_then(Value::as_u64);

        let input = count("inputTokens").unwrap_or_else(|| panic!("{package}: no inputTokens"));
        let output = count("outputTokens").unwrap_or_else(|| panic!("{package}: no outputTokens"));
        let cache_read = count("cachedReadTokens")
            .unwrap_or_else(|| panic!("{package}: no cachedReadTokens — billing would lose the \
                                       bucket that dominates a coding turn"));
        let cache_write = count("cachedWriteTokens").unwrap_or(0);
        if cache_write_expected {
            assert!(
                count("cachedWriteTokens").is_some(),
                "{package}: no cachedWriteTokens"
            );
        }

        // Disjoint, so summing them is sound — `TokenUsage::total` does.
        if let Some(total) = count("totalTokens") {
            assert_eq!(
                input + output + cache_read + cache_write,
                total,
                "{package}: buckets must be disjoint for TokenUsage::total to hold"
            );
        }
    }
}
