//! Auth service tests: dev mode, and the WorkOS flows (headless paste-code exchange,
//! loopback callback, refresh rotation + revocation, org onboarding, member
//! invitations) against a stub edge HTTP server on a plain tokio TcpListener.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use comet_engine::{Auth, AuthConfig, AuthState};

// ---------------------------------------------------------------------------
// Fake JWTs
// ---------------------------------------------------------------------------

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// An unsigned JWT with the claims the engine reads (`exp`/`iat` for TTL, `org_id`).
fn fake_jwt(ttl_secs: i64, org_id: Option<&str>) -> String {
    let mut claims = serde_json::json!({ "sub": "user_1", "iat": 1_000, "exp": 1_000 + ttl_secs });
    if let Some(org) = org_id {
        claims["org_id"] = serde_json::json!(org);
    }
    format!("e30.{}.sig", base64url(claims.to_string().as_bytes()))
}

// ---------------------------------------------------------------------------
// Stub edge server
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StubState {
    exchanges: AtomicUsize,
    refreshes: AtomicUsize,
    /// Refresh tokens seen by /auth/refresh, in order.
    refresh_tokens: Mutex<Vec<String>>,
    /// TTL (seconds) for minted access tokens.
    token_ttl: AtomicUsize,
    /// org_id claim for exchange-minted tokens ("" = none).
    exchange_org: Mutex<String>,
    /// Emails posted to /auth/orgs/:id/invites, in order (gh#76).
    invited: Mutex<Vec<String>>,
    /// Invitation ids the engine asked to revoke.
    revoked: Mutex<Vec<String>>,
}

struct StubEdge {
    port: u16,
    state: Arc<StubState>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for StubEdge {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl StubEdge {
    async fn start() -> StubEdge {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind stub");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(StubState::default());
        state.token_ttl.store(3600, Ordering::SeqCst);
        let handler_state = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(handle(stream, handler_state.clone()));
            }
        });
        StubEdge { port, state, task }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Option<(String, String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?.to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    Some((method, target, String::from_utf8_lossy(&body).into_owned()))
}

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn handle(mut stream: tokio::net::TcpStream, state: Arc<StubState>) {
    let Some((method, target, body)) = read_request(&mut stream).await else {
        return;
    };
    let path = target.split('?').next().unwrap_or("");
    let ttl = state.token_ttl.load(Ordering::SeqCst) as i64;
    match (method.as_str(), path) {
        ("GET", "/health") => {
            respond(&mut stream, "200 OK", r#"{"ok":true,"auth":"workos"}"#).await;
        }
        ("POST", "/auth/exchange") => {
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            if parsed.get("code").and_then(|v| v.as_str()).is_none() {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    r#"{"error":"missing code"}"#,
                )
                .await;
                return;
            }
            let n = state.exchanges.fetch_add(1, Ordering::SeqCst) + 1;
            let org = state.exchange_org.lock().expect("lock").clone();
            let token = fake_jwt(ttl, (!org.is_empty()).then_some(org.as_str()));
            let response = serde_json::json!({
                "user": { "id": "user_1", "email": "w@example.com",
                          "firstName": "Wing", "lastName": "Test" },
                "accessToken": token,
                "refreshToken": format!("refresh-{n}"),
            });
            respond(&mut stream, "200 OK", &response.to_string()).await;
        }
        ("POST", "/auth/refresh") => {
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let refresh_token = parsed
                .get("refreshToken")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            state
                .refresh_tokens
                .lock()
                .expect("lock")
                .push(refresh_token.to_string());
            if refresh_token == "dead" {
                respond(&mut stream, "401 Unauthorized", r#"{"error":"revoked"}"#).await;
                return;
            }
            let n = state.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
            let org = parsed.get("organizationId").and_then(|v| v.as_str());
            let response = serde_json::json!({
                "accessToken": fake_jwt(ttl, org),
                "refreshToken": format!("rotated-{n}"),
            });
            respond(&mut stream, "200 OK", &response.to_string()).await;
        }
        ("GET", "/auth/orgs") => {
            respond(
                &mut stream,
                "200 OK",
                r#"{"orgs":[{"id":"om_1","organizationId":"org_1","name":"Acme"}]}"#,
            )
            .await;
        }
        ("POST", "/auth/orgs") => {
            respond(&mut stream, "200 OK", r#"{"organizationId":"org_new"}"#).await;
        }
        // -- members and invitations (gh#76) --
        ("GET", "/auth/orgs/org_1/members") => {
            respond(
                &mut stream,
                "200 OK",
                r#"{"members":[{"id":"om_1","userId":"user_1","email":"w@example.com","role":"admin"}],"canInvite":true}"#,
            )
            .await;
        }
        ("GET", "/auth/orgs/org_1/invites") => {
            let invites: Vec<serde_json::Value> = state
                .invited
                .lock()
                .expect("lock")
                .iter()
                .enumerate()
                .map(|(ix, email)| {
                    serde_json::json!({ "id": format!("inv_{ix}"), "email": email,
                                        "state": "pending" })
                })
                .collect();
            let body = serde_json::json!({ "invites": invites });
            respond(&mut stream, "200 OK", &body.to_string()).await;
        }
        ("POST", "/auth/orgs/org_1/invites") => {
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let email = parsed
                .get("email")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if email == "taken@example.com" {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    r#"{"error":"already a member of this workspace"}"#,
                )
                .await;
                return;
            }
            state.invited.lock().expect("lock").push(email.clone());
            let body = serde_json::json!({
                "invitation": { "id": "inv_0", "email": email, "state": "pending",
                                "acceptUrl": "https://authkit.example/invite/tok" }
            });
            respond(&mut stream, "200 OK", &body.to_string()).await;
        }
        ("DELETE", path) if path.starts_with("/auth/orgs/org_1/invites/") => {
            let id = path.rsplit('/').next().unwrap_or_default().to_string();
            state.revoked.lock().expect("lock").push(id);
            respond(&mut stream, "200 OK", r#"{"ok":true}"#).await;
        }
        ("POST", "/auth/invites/accept") => {
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            match parsed.get("token").and_then(|v| v.as_str()) {
                Some("good") => {
                    respond(&mut stream, "200 OK", r#"{"organizationId":"org_2"}"#).await;
                }
                _ => {
                    respond(
                        &mut stream,
                        "403 Forbidden",
                        r#"{"error":"that invitation is for a different email address"}"#,
                    )
                    .await;
                }
            }
        }
        _ => respond(&mut stream, "404 Not Found", r#"{"error":"not_found"}"#).await,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn workos_config(edge_url: &str, data_dir: &std::path::Path) -> AuthConfig {
    let mut config = AuthConfig::new(edge_url, data_dir);
    config.workos_client_id = Some("client_test".into());
    config.workos_api_base = "https://authkit.example".into();
    config
}

fn query_param(url: &str, key: &str) -> Option<String> {
    url.split_once('?')?
        .1
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
}

async fn wait_for<T: Clone + PartialEq>(
    rx: &mut tokio::sync::watch::Receiver<T>,
    check: impl Fn(&T) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if check(&rx.borrow()) {
                return;
            }
            rx.changed().await.expect("state channel open");
        }
    })
    .await
    .expect("state reached in time");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dev_mode_is_signed_in_with_configured_bearer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = AuthConfig::new("http://127.0.0.1:1", dir.path());
    config.dev_user_id = "wing-dev".into();
    let auth = Auth::new(config);
    assert!(!auth.workos_enabled());
    assert!(matches!(auth.state(), AuthState::SignedIn { user, .. } if user.id == "wing-dev"));
    assert_eq!(auth.access_token().await.as_deref(), Some("wing-dev"));
    // Dev sign-in mirrors the TS service: a no-op URL, CompleteSignIn accepted.
    assert_eq!(auth.start_sign_in().await.expect("dev sign-in"), "");
    auth.complete_sign_in("whatever")
        .await
        .expect("dev complete is a no-op");
}

#[tokio::test]
async fn headless_flow_exchanges_pasted_code_and_gates_on_org() {
    let edge = StubEdge::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));
    assert!(auth.workos_enabled());
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert_eq!(auth.access_token().await, None, "signed out: no token");

    let url = auth.start_headless_sign_in();
    assert!(url.starts_with("https://authkit.example/user_management/authorize?"));
    assert_eq!(
        query_param(&url, "client_id").as_deref(),
        Some("client_test")
    );
    let redirect = query_param(&url, "redirect_uri").expect("redirect");
    assert!(
        redirect.contains("auth%2Fcli%2Fcallback"),
        "hosted paste-code page: {redirect}"
    );
    let state = query_param(&url, "state").expect("state param");

    // A code minted for someone else's flow (unknown state) is rejected — CSRF check.
    assert!(auth.complete_sign_in("bogus-state.code123").await.is_err());

    // The real paste: `state.code`. The exchange-minted token carries no org claim, so
    // the session lands in NeedsOrganization (the org gate).
    auth.complete_sign_in(&format!("{state}.code123"))
        .await
        .expect("paste-code sign-in");
    assert_eq!(edge.state.exchanges.load(Ordering::SeqCst), 1);
    assert!(
        matches!(auth.state(), AuthState::NeedsOrganization { user } if user.email == "w@example.com")
    );

    // Session persisted 0600 with the exchange's refresh token.
    let session_file = dir.path().join("session.json");
    let raw = std::fs::read_to_string(&session_file).expect("session persisted");
    assert!(raw.contains("refresh-1"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&session_file)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "session file must be private");
    }

    // Org onboarding: list, then select — an org-scoped refresh; state follows the
    // returned token's org claim.
    let orgs = auth.list_orgs().await.expect("list orgs");
    assert_eq!(orgs.len(), 1);
    assert_eq!(orgs[0].organization_id, "org_1");
    auth.select_org("org_1").await.expect("select org");
    assert!(
        matches!(auth.state(), AuthState::SignedIn { org_id: Some(org), .. } if org == "org_1")
    );
    assert_eq!(
        edge.state
            .refresh_tokens
            .lock()
            .expect("lock")
            .first()
            .map(String::as_str),
        Some("refresh-1"),
        "org refresh presents the stored refresh token"
    );
    // Rotation persisted.
    let raw = std::fs::read_to_string(&session_file).expect("session persisted");
    assert!(
        raw.contains("rotated-1"),
        "rotated refresh token stored: {raw}"
    );

    // Sign-out clears state and removes the persisted session.
    auth.sign_out();
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(!session_file.exists());
}

/// gh#76: inviting a teammate used to be a manual membership creation in the
/// WorkOS dashboard. The roster, the invite, and the withdrawal all go through
/// the org the session is scoped to — the caller never names one.
#[tokio::test]
async fn members_and_invitations_go_through_the_scoped_org() {
    let edge = StubEdge::start().await;
    *edge.state.exchange_org.lock().expect("lock") = "org_1".into();
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));
    let url = auth.start_headless_sign_in();
    let state = query_param(&url, "state").expect("state");
    auth.complete_sign_in(&format!("{state}.code123"))
        .await
        .expect("sign-in");

    let roster = auth.list_members().await.expect("list members");
    assert_eq!(roster.members.len(), 1);
    assert_eq!(roster.members[0].email.as_deref(), Some("w@example.com"));
    assert_eq!(roster.members[0].role.as_deref(), Some("admin"));
    assert!(roster.can_invite);

    let invitation = auth
        .invite_member("new@example.com", None)
        .await
        .expect("invite");
    assert_eq!(invitation.email, "new@example.com");
    assert_eq!(
        invitation.accept_url.as_deref(),
        Some("https://authkit.example/invite/tok"),
        "the hosted link is carried through for teammates not on this desktop"
    );
    assert_eq!(
        edge.state.invited.lock().expect("lock").as_slice(),
        ["new@example.com"]
    );

    let pending = auth.list_invites().await.expect("list invites");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].email, "new@example.com");

    auth.revoke_invite("inv_0").await.expect("revoke");
    assert_eq!(
        edge.state.revoked.lock().expect("lock").as_slice(),
        ["inv_0"]
    );

    // A refusal keeps the edge's own words: "already a member of this
    // workspace" tells the user what to do; "workspace request failed (400)"
    // does not.
    let err = auth
        .invite_member("taken@example.com", None)
        .await
        .expect_err("duplicate refused");
    assert!(
        err.to_string().contains("already a member"),
        "edge message surfaced: {err}"
    );
}

/// The other half of gh#76: someone invited into an existing workspace signs in
/// with no org of their own, and joins with the code instead of creating one.
#[tokio::test]
async fn accepting_an_invitation_scopes_the_session() {
    let edge = StubEdge::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));
    let url = auth.start_headless_sign_in();
    let state = query_param(&url, "state").expect("state");
    auth.complete_sign_in(&format!("{state}.code123"))
        .await
        .expect("sign-in");
    assert!(matches!(auth.state(), AuthState::NeedsOrganization { .. }));

    // No org yet: member management has nothing to talk about, and says so
    // without a round trip.
    assert!(auth.list_members().await.is_err());

    // An empty code never reaches the edge.
    assert!(auth.accept_invite("   ").await.is_err());
    // A code for someone else is refused in the invitee's own words.
    let err = auth.accept_invite("stolen").await.expect_err("refused");
    assert!(
        err.to_string().contains("different email address"),
        "edge message surfaced: {err}"
    );
    assert!(
        matches!(auth.state(), AuthState::NeedsOrganization { .. }),
        "a refused code leaves the gate up"
    );

    // The real code lands the session in the workspace that invited us — the
    // same landing picking a membership gives, so the gate falls away.
    let org = auth.accept_invite("good").await.expect("accept");
    assert_eq!(org, "org_2");
    assert!(matches!(auth.state(), AuthState::SignedIn { org_id: Some(org), .. } if org == "org_2"));
}

#[tokio::test]
async fn short_lived_tokens_refresh_on_demand() {
    let edge = StubEdge::start().await;
    // Tokens live 20s < the 30s slack → every access_token() call refreshes.
    edge.state.token_ttl.store(20, Ordering::SeqCst);
    *edge.state.exchange_org.lock().expect("lock") = "org_1".into();
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));

    let url = auth.start_headless_sign_in();
    let state = query_param(&url, "state").expect("state");
    auth.complete_sign_in(&format!("{state}.codeX"))
        .await
        .expect("sign in");
    assert!(auth.state().is_signed_in());

    let first = auth.access_token().await.expect("token after refresh");
    assert_eq!(
        edge.state.refreshes.load(Ordering::SeqCst),
        1,
        "stale exchange token refreshed"
    );
    let second = auth.access_token().await.expect("token again");
    assert_eq!(
        edge.state.refreshes.load(Ordering::SeqCst),
        2,
        "still under slack → refreshed"
    );
    assert_eq!(first, second, "same claims → same fake token bytes");
    // Rotated refresh tokens are chained: refresh N presents rotation N-1's token.
    let seen = edge.state.refresh_tokens.lock().expect("lock").clone();
    assert_eq!(seen, vec!["refresh-1".to_string(), "rotated-1".to_string()]);
}

#[tokio::test]
async fn revoked_refresh_token_signs_out() {
    let edge = StubEdge::start().await;
    let dir = tempfile::tempdir().expect("tempdir");
    // A persisted session whose refresh token the edge rejects with a definitive 4xx.
    std::fs::write(
        dir.path().join("session.json"),
        r#"{"refreshToken":"dead","user":{"id":"user_1","email":"w@example.com"},"orgId":"org_1"}"#,
    )
    .expect("seed session");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));
    assert!(
        auth.state().is_signed_in(),
        "boots from the persisted session"
    );

    // The refresh is doomed → the session degrades to SignedOut and the file is gone.
    assert_eq!(auth.access_token().await, None);
    assert_eq!(auth.state(), AuthState::SignedOut);
    assert!(!dir.path().join("session.json").exists());
}

#[tokio::test]
async fn loopback_callback_completes_headed_sign_in() {
    let edge = StubEdge::start().await;
    *edge.state.exchange_org.lock().expect("lock") = "org_1".into();
    let dir = tempfile::tempdir().expect("tempdir");
    let auth = Auth::new(workos_config(&edge.url(), dir.path()));

    let url = auth.start_sign_in().await.expect("authorize url");
    let redirect = query_param(&url, "redirect_uri").expect("redirect");
    assert!(
        redirect.starts_with("http%3A%2F%2F127.0.0.1%3A"),
        "loopback redirect: {redirect}"
    );
    let state = query_param(&url, "state").expect("state");
    let callback: String = redirect.replace("%3A", ":").replace("%2F", "/");

    // A wrong/expired state is rejected without touching the exchange endpoint.
    let bad = reqwest::get(format!("{callback}?code=abc&state=wrong"))
        .await
        .expect("bad cb");
    assert_eq!(bad.status().as_u16(), 400);
    assert_eq!(edge.state.exchanges.load(Ordering::SeqCst), 0);

    // The browser hits the loopback callback → the engine exchanges the code with the
    // edge and the session lands org-scoped.
    let ok = reqwest::get(format!("{callback}?code=abc&state={state}"))
        .await
        .expect("cb");
    assert_eq!(ok.status().as_u16(), 200);
    let mut state_rx = auth.watch_state();
    wait_for(&mut state_rx, |s| s.is_signed_in()).await;
    assert_eq!(edge.state.exchanges.load(Ordering::SeqCst), 1);
    assert!(
        matches!(auth.state(), AuthState::SignedIn { org_id: Some(org), user } if org == "org_1" && user.name.as_deref() == Some("Wing Test"))
    );
}

#[tokio::test]
async fn detect_probes_edge_dev_mode() {
    // A stub that reports auth:"dev" forces dev mode even with a client id configured.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            if read_request(&mut stream).await.is_some() {
                respond(&mut stream, "200 OK", r#"{"ok":true,"auth":"dev"}"#).await;
            }
        }
    });
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = workos_config(&format!("http://127.0.0.1:{port}"), dir.path());
    config.dev_user_id = "dev-w".into();
    let auth = Auth::detect(config).await;
    assert!(!auth.workos_enabled(), "edge dev mode wins");
    assert_eq!(auth.access_token().await.as_deref(), Some("dev-w"));
    task.abort();
}
