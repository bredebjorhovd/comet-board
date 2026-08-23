//! Fake `opencode serve` for comet-harness integration tests — a minimal
//! HTTP+SSE server that speaks just enough of opencode's REST/SSE protocol
//! for `crates/harness/tests/opencode.rs` to drive a real run against it
//! (no real `opencode` binary involved). Driven by the prompt text
//! (`scenario:…`), like the `fake-codex.sh`/`fake-claude.sh` fixtures.
//!
//! Endpoints served:
//!   GET  /global/health        — 200 (the harness polls readiness)
//!   POST /session              — `{"id":"ses_fake"}`
//!   GET  /event                — SSE; waits for the prompt, then plays the
//!                                scenario and either closes the feed while
//!                                staying alive, stays parked, or exits
//!   POST /session/{id}/prompt_async — picks the scenario from the prompt body
//!   POST /session/{id}/abort   — 200
//!   GET  /config/providers     — empty catalog (models fall back to static)
//!
//! The server writes its own pid to `fake-opencode.pid` in its current
//! working directory (the harness is pointed at a tempdir cwd by the tests) so
//! tests can assert the child was reaped after a run. Beside it goes
//! `fake-opencode.env` — the credential-shaped variables as this process
//! actually received them, which is how gh#233's question ("what does the
//! opencode child get?") is answered by a test rather than by reasoning about
//! it.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

const SESSION_ID: &str = "ses_fake";

#[derive(Clone, Copy, PartialEq)]
enum Scenario {
    /// Emit a busy→delta turn (assistant message settles), close the SSE feed,
    /// and keep the process alive — a clean stream end on a live server.
    StreamEnd,
    /// Emit a busy→delta turn, then exit(1) mid-run — the server dies.
    Crash,
    /// Die the way a crashing serve usually looks from the outside: the dying
    /// process's session goes `session.idle` without the assistant message ever
    /// settling, and the exit lands in the same instant (gh#79). The idle
    /// arrives first, so the stall path (gh#37) must still report the real exit
    /// status rather than "went idle".
    IdleThenCrash,
    /// Emit a full busy→delta→completed-message→idle turn and stay parked
    /// (persistent session); the harness reaps the process when the run ends.
    Happy,
    /// Emit a busy→delta→tool-call→tool-result→idle turn WITHOUT the assistant
    /// message ever settling (`message.updated` with `time.completed`) — a
    /// mid-stream stall exactly like the fjaerpenn#1 journal (text + real tool
    /// results, then idle, no completion) that must surface as Errored, never a
    /// spurious Completed (gh#37, gh#46).
    IdleMidTurn,
    /// Hold the SSE feed open for `SLOW_TURN_SLEEP` (streaming heartbeats, the
    /// way the real opencode `/event` handler does) BEFORE a normal settled
    /// turn — a slow-but-progressing run that outlives the harness's short
    /// total request deadline. Must still complete (gh#46: the stream is
    /// exempt from the total timeout).
    SlowTurn,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let _ = std::fs::write(
        cwd.join("fake-opencode.pid"),
        std::process::id().to_string(),
    );
    // What the process that runs the agent's tools was actually handed. The
    // server is the parent of every `git push` an opencode agent makes, so
    // this file is the environment those pushes inherit. The claude-shaped
    // variables ride along since gh#577: whether a foreign CLI's login reached
    // this child is answered by reading what it got, not by reasoning about it.
    let _ = std::fs::write(
        cwd.join("fake-opencode.env"),
        std::env::vars()
            .filter(|(k, _)| {
                k.starts_with("GIT_")
                    || k.starts_with("COMET_BOARD_")
                    || k == "OPENCODE_CONFIG_CONTENT"
                    || k == "CLAUDE_CONFIG_DIR"
                    || k == "CLAUDE_CODE_OAUTH_TOKEN"
            })
            .map(|(k, v)| format!("{k}={v}\n"))
            .collect::<String>(),
    );
    println!("opencode server listening on http://127.0.0.1:{port}");

    // The harness opens /event BEFORE sending the prompt, so the SSE handler
    // must wait until prompt_async arrives before playing its scenario.
    //
    // A latch, not a notification (gh#310): the prompt lands on a *different*
    // connection, so nothing orders it against the `/event` task reaching its
    // wait. `Notify::notify_waiters()` drops a wakeup that has no waiter yet,
    // and on a loaded CI runner the `/event` task is often still between its
    // preamble write and its await when the prompt arrives — the scenario was
    // then never played, the harness read an empty feed, and the test hung to
    // its deadline. A `watch` holds the value, so a receiver that arrives late
    // still sees it.
    let (scenario_tx, scenario_rx) = watch::channel(None);
    let scenario_tx = Arc::new(scenario_tx);
    loop {
        let (stream, _) = listener.accept().await?;
        let scenario_tx = scenario_tx.clone();
        let scenario_rx = scenario_rx.clone();
        tokio::spawn(async move {
            handle(stream, scenario_tx, scenario_rx).await;
        });
    }
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

async fn handle(
    mut stream: TcpStream,
    scenario_tx: Arc<watch::Sender<Option<Scenario>>>,
    scenario_rx: watch::Receiver<Option<Scenario>>,
) {
    let Some(request) = read_request(&mut stream).await else {
        return;
    };
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/global/health") => {
            respond(&mut stream, 200, "{\"ok\":true}").await;
        }
        ("POST", "/session") => {
            let body = format!("{{\"id\":\"{SESSION_ID}\"}}");
            respond(&mut stream, 200, &body).await;
        }
        ("POST", path) if path.ends_with("/prompt_async") => {
            let body = String::from_utf8_lossy(&request.body);
            let sc = if body.contains("scenario:stream-end") {
                Scenario::StreamEnd
            } else if body.contains("scenario:idle-then-crash") {
                Scenario::IdleThenCrash
            } else if body.contains("scenario:crash") {
                Scenario::Crash
            } else if body.contains("scenario:idle-mid-turn") {
                Scenario::IdleMidTurn
            } else if body.contains("scenario:slow-turn") {
                Scenario::SlowTurn
            } else {
                Scenario::Happy
            };
            // Latched before the 200 goes out: the harness may not send
            // anything else, so this value is all the `/event` handler ever
            // gets — and it must survive arriving before that handler waits.
            scenario_tx.send_replace(Some(sc));
            respond(&mut stream, 200, "{}").await;
        }
        ("POST", path) if path.ends_with("/abort") => {
            respond(&mut stream, 200, "{}").await;
        }
        ("GET", "/event") => {
            serve_events(stream, scenario_rx).await;
        }
        ("GET", "/config/providers") => {
            respond(&mut stream, 200, "{\"providers\":[],\"default\":{}}").await;
        }
        _ => respond(&mut stream, 404, "{}").await,
    }
}

/// How long `SlowTurn` holds the feed open before its turn — comfortably past
/// the harness's (test-shortened) total request deadline, well inside the read
/// timeout the stream actually carries.
const SLOW_TURN_SLEEP: Duration = Duration::from_secs(2);

/// Play the scenario on the SSE feed. Drops the stream at the end: closing it
/// delivers EOF to the harness's reader while this process stays alive (the
/// accept loop above keeps serving, and `shutdown_child`'s SIGTERM is what
/// actually ends the process).
async fn serve_events(mut stream: TcpStream, mut scenario: watch::Receiver<Option<Scenario>>) {
    let preamble = "HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\n\
                    Connection: close\r\n\r\n";
    if stream.write_all(preamble.as_bytes()).await.is_err() {
        return;
    }
    // Waits for the prompt, or returns straight away if it already arrived —
    // `wait_for` tests the value it is given before it ever sleeps, which is
    // the whole point of the latch (gh#310).
    let sc = match scenario.wait_for(Option::is_some).await {
        Ok(seen) => (*seen).unwrap_or(Scenario::Happy),
        // Every sender dropped: the server is going away, nothing left to play.
        Err(_) => return,
    };
    if sc == Scenario::SlowTurn {
        // A slow-but-progressing run: stream heartbeats (the real `/event`
        // handler emits one every 10s) past the total request deadline, then
        // play the settled turn. The harness must not read this as a dropped
        // feed / stall (gh#46).
        let deadline = tokio::time::Instant::now() + SLOW_TURN_SLEEP;
        while tokio::time::Instant::now() < deadline {
            sse(&mut stream, "server.heartbeat", "{}").await;
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    sse(
        &mut stream,
        "session.status",
        "{\"status\":{\"type\":\"busy\"},\"sessionID\":\"ses_fake\"}",
    )
    .await;
    sse(
        &mut stream,
        "message.part.delta",
        "{\"field\":\"text\",\"delta\":\"hello\",\"sessionID\":\"ses_fake\"}",
    )
    .await;
    // A mid-turn stall works real tools before going idle (the fjaerpenn#1
    // journal shape: deltas + tool calls + idle, no settled message).
    if sc == Scenario::IdleMidTurn {
        sse(
            &mut stream,
            "message.part.updated",
            "{\"part\":{\"id\":\"prt_tool1\",\"title\":\"ls\",\"state\":{\"status\":\"running\",\"input\":{\"command\":\"ls\"}}},\"sessionID\":\"ses_fake\"}",
        )
        .await;
        sse(
            &mut stream,
            "message.part.updated",
            "{\"part\":{\"id\":\"prt_tool1\",\"title\":\"ls\",\"state\":{\"status\":\"completed\",\"input\":{\"command\":\"ls\"},\"metadata\":{\"exit\":0}}},\"sessionID\":\"ses_fake\"}",
        )
        .await;
    }
    match sc {
        // A settled turn's assistant message completes before the turn ends; a
        // mid-turn stall (IdleMidTurn) never does.
        Scenario::StreamEnd | Scenario::Happy | Scenario::SlowTurn => {
            sse(
                &mut stream,
                "message.updated",
                "{\"info\":{\"id\":\"msg_fake1\",\"role\":\"assistant\",\"time\":{\"created\":1,\"completed\":2}},\"sessionID\":\"ses_fake\"}",
            )
            .await;
        }
        Scenario::Crash | Scenario::IdleThenCrash | Scenario::IdleMidTurn => {}
    }
    match sc {
        Scenario::StreamEnd => {}
        Scenario::Crash => {
            let _ = stream.flush().await;
            std::process::exit(1);
        }
        Scenario::IdleThenCrash => {
            sse(&mut stream, "session.idle", "{\"sessionID\":\"ses_fake\"}").await;
            std::process::exit(1);
        }
        Scenario::Happy | Scenario::IdleMidTurn | Scenario::SlowTurn => {
            sse(&mut stream, "session.idle", "{\"sessionID\":\"ses_fake\"}").await;
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
    }
}

async fn sse(stream: &mut TcpStream, type_: &str, props: &str) {
    let line = format!("data: {{\"type\":\"{type_}\",\"properties\":{props}}}\n\n");
    if stream.write_all(line.as_bytes()).await.is_err() {
        return;
    }
    let _ = stream.flush().await;
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.flush().await;
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 1 << 20 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let mut parts = lines.next()?.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut tmp).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = buf[header_end..header_end + content_length].to_vec();
    Some(Request { method, path, body })
}
