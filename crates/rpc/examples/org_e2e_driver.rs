//! Two-USER e2e smoke driver (`scripts/e2e-org-smoke.sh` runs it) — gh#66.
//!
//! The sibling of `e2e_driver`, which drives two devices of ONE user. Here the
//! two engines are two different WorkOS users of one org: A is the always-on box
//! (alice), B is a teammate's laptop (bob). Everything the sharing layer does is
//! per-user, so this is the flow that was impossible before gh#66 — B signed in
//! and found an empty world.
//!
//! Against a real edge, in the order the three gates depend on each other:
//!
//! 1. **B sees the box** — A's device row reaches B's `WatchDevices` through the
//!    org device registry room (their workspace docs are different rooms and
//!    never converge), and the board pane's host sweep has a candidate to visit.
//! 2. **B relays to the box** — a device-addressed RPC (`targetDeviceId`) is
//!    answered by A through A's device room, which now admits the org.
//! 3. **B opens and steers a chat the box shared** — A creates the chat and runs
//!    its first turn (what a board dispatch does), the chat is shared with the
//!    org, and then B reads the transcript and queues a turn of its own that A —
//!    not B — executes.
//!
//! Also asserts the negative that makes 3 safe: B never writes the transcript.
//! B has no workspace row for the chat, and before the session doc carried its
//! host device that read as "unclaimed, so mine to run".
//!
//! Prints `PASS`/`FAIL` lines; exits nonzero on failure.

use std::time::{Duration, Instant};

use comet_rpc::{RpcClient, connect_ws, methods};

const STEP_TIMEOUT: Duration = Duration::from_secs(90);
const MOCK_TEXT: &str = "Mock harness reporting in.";

fn fail(message: &str) -> ! {
    eprintln!("FAIL: {message}");
    std::process::exit(1);
}

fn pass(message: &str) {
    println!("PASS: {message}");
}

async fn device_id(client: &RpcClient, label: &str) -> String {
    match client
        .call(methods::LOCAL_DEVICE, serde_json::json!({}))
        .await
    {
        Ok(value) => value
            .get("deviceId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| fail(&format!("{label}: LocalDevice reply missing deviceId"))),
        Err(err) => fail(&format!("{label}: LocalDevice call failed: {err}")),
    }
}

/// Subscribe to a watch-stream and poll items until `predicate` returns `Some`.
async fn wait_stream<T>(
    client: &RpcClient,
    method: &str,
    params: serde_json::Value,
    what: &str,
    predicate: impl Fn(&serde_json::Value) -> Option<T>,
) -> T {
    let mut rx = match client.subscribe(method, params).await {
        Ok(rx) => rx,
        Err(err) => fail(&format!("{what}: subscribe {method} failed: {err}")),
    };
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            fail(&format!(
                "{what}: timed out after {}s",
                STEP_TIMEOUT.as_secs()
            ));
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(item)) => {
                if let Some(found) = predicate(&item) {
                    return found;
                }
            }
            Ok(None) => fail(&format!("{what}: stream ended early")),
            Err(_) => fail(&format!(
                "{what}: timed out after {}s",
                STEP_TIMEOUT.as_secs()
            )),
        }
    }
}

/// Retry a device-addressed call until the host relay has finished joining.
async fn call_until(
    client: &RpcClient,
    method: &str,
    params: serde_json::Value,
    what: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + STEP_TIMEOUT;
    loop {
        match client.call(method, params.clone()).await {
            Ok(value) => return value,
            Err(err) => {
                if Instant::now() >= deadline {
                    fail(&format!("{what}: {err}"));
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

fn run_command(prompt: &str, message_id: &str) -> serde_json::Value {
    serde_json::json!({
        "kind": "run",
        "messageId": message_id,
        "request": {
            "prompt": prompt,
            "model": null,
            "reasoning": null,
            "cwd": "/tmp",
            "sandbox": "workspace-write",
            "autoApprove": true,
            "resume": null,
        },
    })
}

/// Count the completed assistant entries in a transcript frame.
fn completed_assistants(item: &serde_json::Value) -> Vec<(String, String)> {
    item.as_array()
        .map(|entries| {
            entries
                .iter()
                .filter(|entry| {
                    entry.get("role").and_then(|v| v.as_str()) == Some("assistant")
                        && entry.get("status").and_then(|v| v.as_str()) == Some("complete")
                })
                .filter_map(|entry| {
                    Some((
                        entry.get("deviceId")?.as_str()?.to_string(),
                        entry.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let a_port: u16 = args
        .next()
        .unwrap_or_else(|| "27801".into())
        .parse()
        .expect("A port");
    let b_port: u16 = args
        .next()
        .unwrap_or_else(|| "27802".into())
        .parse()
        .expect("B port");
    let edge_url = args
        .next()
        .unwrap_or_else(|| "http://localhost:27640".into());
    // The box's bearer — the identity that owns the chat and therefore the only
    // one the edge lets share it.
    let box_token = args.next().unwrap_or_else(|| "alice@org1".into());

    let a = connect_ws(&format!("ws://127.0.0.1:{a_port}"))
        .await
        .unwrap_or_else(|err| fail(&format!("connect the box's ipc :{a_port}: {err}")));
    let b = connect_ws(&format!("ws://127.0.0.1:{b_port}"))
        .await
        .unwrap_or_else(|err| fail(&format!("connect the teammate's ipc :{b_port}: {err}")));

    let a_dev = device_id(&a, "box").await;
    let b_dev = device_id(&b, "teammate").await;
    if a_dev == b_dev {
        fail("both engines report the same deviceId — data dirs are shared");
    }

    // ── gate 1: the teammate can SEE the box ──────────────────────────────
    wait_stream(
        &b,
        methods::WATCH_DEVICES,
        serde_json::json!({}),
        "the box in the teammate's device list",
        |item| {
            item.as_array()?
                .iter()
                .find(|device| device.get("id").and_then(|v| v.as_str()) == Some(a_dev.as_str()))
                .map(|_| ())
        },
    )
    .await;
    pass("gate 1: the box appears in the teammate's WatchDevices (org registry)");

    // ── gate 2: the teammate can RELAY to the box ─────────────────────────
    let harnesses = call_until(
        &b,
        methods::LIST_HARNESSES,
        serde_json::json!({ "targetDeviceId": a_dev }),
        "a device-addressed RPC must reach the box",
    )
    .await;
    if !harnesses.is_array() {
        fail(&format!("the box answered with a non-list: {harnesses}"));
    }
    pass("gate 2: a targetDeviceId RPC is answered by the box over the relay");

    // ── the box dispatches (what CometRuntime::dispatch does) ─────────────
    let space_id = uuid::Uuid::new_v4().to_string();
    a.call(
        methods::MUTATE,
        serde_json::json!({
            "op": "createSpace",
            "spaceId": space_id,
            "deviceId": a_dev,
            "path": "/tmp",
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("createSpace on the box: {err}")));
    let chat_id = uuid::Uuid::new_v4().to_string();
    a.call(
        methods::MUTATE,
        serde_json::json!({
            "op": "createChat",
            "chatId": chat_id,
            "spaceId": space_id,
            "config": {
                "harness": "mock",
                "model": null,
                "reasoning": null,
                "sandbox": "workspace-write",
            },
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("createChat on the box: {err}")));
    a.call(
        methods::QUEUE_COMMAND,
        serde_json::json!({
            "chatId": chat_id,
            "command": run_command("e2e org smoke: the brief", &uuid::Uuid::new_v4().to_string()),
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("the brief on the box: {err}")));
    wait_stream(
        &a,
        methods::WATCH_DOC_MESSAGES,
        serde_json::json!({ "chatId": chat_id }),
        "the box runs the brief",
        |item| (!completed_assistants(item).is_empty()).then_some(()),
    )
    .await;
    pass("the box created a chat and ran its first turn");

    // The board shares every chat it dispatches — the same request
    // `DocHost::share_chat` makes, issued here because this smoke has no board.
    let share = reqwest::Client::new()
        .post(format!(
            "{}/share/{}",
            edge_url.trim_end_matches('/'),
            chat_id
        ))
        .bearer_auth(&box_token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap_or_else(|err| fail(&format!("POST /share: {err}")));
    if !share.status().is_success() {
        fail(&format!("POST /share answered {}", share.status()));
    }
    pass("the chat is shared with the org");

    // ── gate 3: the teammate READS the shared chat ────────────────────────
    let (wrote, text) = wait_stream(
        &b,
        methods::WATCH_DOC_MESSAGES,
        serde_json::json!({ "chatId": chat_id }),
        "the shared transcript on the teammate's laptop",
        |item| completed_assistants(item).into_iter().next(),
    )
    .await;
    if wrote != a_dev {
        fail(&format!(
            "the brief's answer was written by {wrote}, expected the box {a_dev}"
        ));
    }
    if !text.contains(MOCK_TEXT) {
        fail(&format!("the transcript lacks the mock text: {text}"));
    }
    pass("gate 3: the teammate opened the board-dispatched chat and read it");

    // ── …and steers it, WITHOUT running it ────────────────────────────────
    b.call(
        methods::QUEUE_COMMAND,
        serde_json::json!({
            "chatId": chat_id,
            "command": run_command(
                "e2e org smoke: the teammate's turn",
                &uuid::Uuid::new_v4().to_string(),
            ),
        }),
    )
    .await
    .unwrap_or_else(|err| fail(&format!("QueueCommand on the teammate's laptop: {err}")));
    let answers = wait_stream(
        &b,
        methods::WATCH_DOC_MESSAGES,
        serde_json::json!({ "chatId": chat_id }),
        "the teammate's turn answered",
        |item| {
            let done = completed_assistants(item);
            (done.len() >= 2).then_some(done)
        },
    )
    .await;
    for (device, entry) in &answers {
        if device != &a_dev {
            fail(&format!(
                "an assistant entry was written by {device}: the chat must only ever run on the box ({a_dev}) — {entry}"
            ));
        }
    }
    pass("the teammate's turn ran on the box, never on their own laptop");

    println!("PASS: two-user org e2e smoke complete");
}
