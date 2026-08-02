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
//! tests can assert the child was reaped after a run.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify};

const SESSION_ID: &str = "ses_fake";

#[derive(Clone, Copy, PartialEq)]
enum Scenario {
    /// Emit a busy→delta turn (assistant message settles), close the SSE feed,
    /// and keep the process alive — a clean stream end on a live server.
    StreamEnd,
    /// Emit a busy→delta turn, then exit(1) mid-run — the server dies.
    Crash,
    /// Emit a full busy→delta→completed-message→idle turn and stay parked
    /// (persistent session); the harness reaps the process when the run ends.
    Happy,
    /// Emit a busy→delta→idle turn WITHOUT the assistant message ever settling
    /// (`message.updated` with `time.completed`) — a mid-stream stall that must
    /// surface as Errored, never a spurious Completed (gh#37).
    IdleMidTurn,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let _ = std::fs::write(
        std::env::current_dir()
            .unwrap_or_else(|_| ".".into())
            .join("fake-opencode.pid"),
        std::process::id().to_string(),
    );
    println!("opencode server listening on http://127.0.0.1:{port}");

    // The harness opens /event BEFORE sending the prompt, so the SSE handler
    // must wait until prompt_async arrives before playing its scenario.
    let prompt = Arc::new(Notify::new());
    let scenario = Arc::new(Mutex::new(None));
    loop {
        let (stream, _) = listener.accept().await?;
        let prompt = prompt.clone();
        let scenario = scenario.clone();
        tokio::spawn(async move {
            handle(stream, prompt, scenario).await;
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
    prompt: Arc<Notify>,
    scenario: Arc<Mutex<Option<Scenario>>>,
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
            } else if body.contains("scenario:crash") {
                Scenario::Crash
            } else if body.contains("scenario:idle-mid-turn") {
                Scenario::IdleMidTurn
            } else {
                Scenario::Happy
            };
            *scenario.lock().await = Some(sc);
            prompt.notify_waiters();
            respond(&mut stream, 200, "{}").await;
        }
        ("POST", path) if path.ends_with("/abort") => {
            respond(&mut stream, 200, "{}").await;
        }
        ("GET", "/event") => {
            serve_events(stream, prompt, scenario).await;
        }
        ("GET", "/config/providers") => {
            respond(&mut stream, 200, "{\"providers\":[],\"default\":{}}").await;
        }
        _ => respond(&mut stream, 404, "{}").await,
    }
}

/// Play the scenario on the SSE feed. Drops the stream at the end: closing it
/// delivers EOF to the harness's reader while this process stays alive (the
/// accept loop above keeps serving, and `shutdown_child`'s SIGTERM is what
/// actually ends the process).
async fn serve_events(
    mut stream: TcpStream,
    prompt: Arc<Notify>,
    scenario: Arc<Mutex<Option<Scenario>>>,
) {
    let preamble = "HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\n\
                    Connection: close\r\n\r\n";
    if stream.write_all(preamble.as_bytes()).await.is_err() {
        return;
    }
    prompt.notified().await;
    let sc = scenario.lock().await.unwrap_or(Scenario::Happy);
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
    match sc {
        // A settled turn's assistant message completes before the turn ends; a
        // mid-turn stall (IdleMidTurn) never does.
        Scenario::StreamEnd | Scenario::Happy => {
            sse(
                &mut stream,
                "message.updated",
                "{\"info\":{\"id\":\"msg_fake1\",\"role\":\"assistant\",\"time\":{\"created\":1,\"completed\":2}},\"sessionID\":\"ses_fake\"}",
            )
            .await;
        }
        Scenario::Crash | Scenario::IdleMidTurn => {}
    }
    match sc {
        Scenario::StreamEnd => {}
        Scenario::Crash => {
            let _ = stream.flush().await;
            std::process::exit(1);
        }
        Scenario::Happy | Scenario::IdleMidTurn => {
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
