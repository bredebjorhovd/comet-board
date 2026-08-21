//! `comet login` / `comet logout` / `comet status` — the standalone auth surface.
//!
//! Sign-in used to live only inside `comet headless`, coupling authentication to
//! the long-running daemon. These commands work on the persisted session
//! (`{data_dir}/session.json`) and exit, so a service-managed `comet headless`
//! only ever *loads* credentials. While an engine is running it owns the session
//! (WorkOS refresh tokens are single-use and rotate on every refresh), so login
//! and logout take the same data-dir lock the engine holds and refuse politely
//! when it is busy.

use std::io::IsTerminal;

use comet_engine::{AuthState, Engine, EngineConfig, InstanceLock};

/// `comet login`: authenticate via the paste-code flow (and workspace
/// onboarding), persist `session.json`, and exit.
pub async fn login(config: EngineConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let auth = Engine::build_auth(&config).await;
    if !auth.workos_enabled() {
        println!("Auth is in dev mode (no WorkOS client id) — there is nothing to sign in to.");
        return Ok(());
    }
    if let AuthState::SignedIn { user, org_id } = auth.state() {
        println!(
            "Already signed in as {}{}.",
            user.email,
            org_id
                .map(|org| format!(" (workspace {org})"))
                .unwrap_or_default()
        );
        println!("Run `comet logout` first to switch accounts.");
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("comet login needs an interactive terminal");
    }
    let _lock = engine_lock(&config, "sign in")?;
    comet_engine::terminal_sign_in(&auth).await?;
    match auth.state() {
        AuthState::SignedIn { user, org_id } => {
            println!(
                "\nSigned in as {}{}.",
                user.email,
                org_id
                    .map(|org| format!(" (workspace {org})"))
                    .unwrap_or_default()
            );
            println!("Session saved — `comet headless` (and the daemon) will use it.");
        }
        // terminal_sign_in only returns Ok once signed in; keep an honest fallback.
        _ => println!("Sign-in did not complete."),
    }
    Ok(())
}

/// `comet logout`: remove the persisted session.
pub async fn logout(config: EngineConfig) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let auth = Engine::build_auth(&config).await;
    let _lock = engine_lock(&config, "sign out")?;
    if !auth.workos_enabled() {
        // Dev mode has no live session, but clear any stale session.json from a
        // previous WorkOS-mode run so the next real run starts signed out.
        auth.sign_out();
        println!("Auth is in dev mode — cleared any saved session.");
        return Ok(());
    }
    match auth.state() {
        AuthState::SignedOut => println!("No saved session."),
        state => {
            let email = state
                .user()
                .map(|u| u.email.clone())
                .unwrap_or_else(|| "<unknown>".into());
            auth.sign_out();
            println!(
                "Signed out {email} — removed {}.",
                config.data_dir.join("session.json").display()
            );
        }
    }
    Ok(())
}

/// How long to wait for a running engine to answer `EdgeHealth`. Short: a
/// status command must not hang on a wedged engine, and "it did not answer" is
/// itself a reportable state.
const HEALTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Ask a running engine which edge connections it actually holds (gh#116).
///
/// Over IPC rather than by inspecting local files, because the whole point is
/// live sockets — and over the local port rather than the relay, because an
/// engine whose relay is down is exactly the case this is for.
async fn edge_health(port: u16) -> anyhow::Result<comet_proto::EdgeHealth> {
    let ask = async {
        let client = comet_rpc::connect_ws(&format!("ws://127.0.0.1:{port}")).await?;
        let reply = client
            .call(comet_rpc::methods::EDGE_HEALTH, serde_json::json!({}))
            .await?;
        Ok::<_, anyhow::Error>(serde_json::from_value(reply)?)
    };
    tokio::time::timeout(HEALTH_TIMEOUT, ask)
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}s", HEALTH_TIMEOUT.as_secs()))?
}

/// `comet status`: report auth + engine liveness. Exits nonzero when a sign-in
/// is needed, so scripts (and service health checks) can gate on it.
pub async fn status(config: EngineConfig) -> anyhow::Result<()> {
    let auth = Engine::build_auth(&config).await;
    println!("Data dir: {}", config.data_dir.display());
    if config.edge_enabled() {
        println!("Edge:     {}", config.edge_url);
    } else {
        println!("Edge:     disabled (local mode)");
    }
    let signed_in = match (auth.workos_enabled(), auth.state()) {
        (false, _) => {
            println!("Auth:     dev mode (bearer = user id)");
            true
        }
        (true, AuthState::SignedIn { user, org_id }) => {
            println!(
                "Auth:     signed in as {}{}",
                user.email,
                org_id
                    .map(|org| format!(" (workspace {org})"))
                    .unwrap_or_default()
            );
            true
        }
        (true, AuthState::NeedsOrganization { user }) => {
            println!(
                "Auth:     signed in as {} but no workspace selected — run `comet login`",
                user.email
            );
            false
        }
        (true, AuthState::SignedOut) => {
            println!("Auth:     signed out — run `comet login`");
            false
        }
    };
    match InstanceLock::holder(&config.data_dir) {
        Some(pid) => println!("Engine:   running (pid {pid})"),
        None => println!("Engine:   not running"),
    }
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], config.ipc_port));
    let ipc = std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500));
    println!(
        "IPC:      {} 127.0.0.1:{}",
        if ipc.is_ok() {
            "listening on"
        } else {
            "not listening on"
        },
        config.ipc_port
    );
    // The gh#116 line. An engine can be running, signed in and answering IPC
    // while holding not one live edge socket — locally perfect, remotely
    // nonexistent — and until now the only way to see that was journald.
    if ipc.is_ok() {
        match edge_health(config.ipc_port).await {
            Ok(health) => {
                println!("Rooms:    {}", health.summary());
                if health.dark() {
                    println!(
                        "          Remote viewers cannot see this device. It should recover \
                         on its own within a few minutes; if it does not, restart the engine \
                         (`comet daemon restart`)."
                    );
                }
                // The gh#527 line. The summary above already counts the rooms
                // that keep dying; this says what a person is to DO about it,
                // because the reading that precedes it ("N of N live") is the
                // one that talked an operator out of looking for a whole
                // evening.
                if health.churning() {
                    println!(
                        "          These rooms are joining and then dying, which is the edge \
                         failing mid-session rather than refusing. Replies will not arrive \
                         while it lasts. Check the room's own account (`/stats` now reports \
                         which sockets vanished and whether it aborted itself) and the \
                         Workers plan's duration cap."
                    );
                }
            }
            Err(err) => println!("Rooms:    could not ask the engine ({err:#})"),
        }
    }
    // The gh#156 line, and it has to be printed from here rather than from
    // `comet-board doctor`: a CLI old enough to have drifted is old enough not
    // to carry the check that would say so, which is exactly how the box ran
    // three weeks behind in silence. This binary is the one the release
    // upgrades, so this is the one that can tell.
    //
    // Not an exit code, unlike the auth gate above: the engine is fine, and a
    // service health check must not start failing over a CLI that is merely
    // stale. Saying it plainly, where somebody is already looking, is the fix.
    let (_, cli_line) = comet_board::board_cli::probe().line(comet_update::current_version());
    println!("Board CLI: {cli_line}");

    if !signed_in {
        std::process::exit(1);
    }
    Ok(())
}

/// The same exclusive data-dir lock the engine holds for its lifetime: taken for
/// the whole login/logout mutation so we never rotate or delete a session out
/// from under a running engine (whose in-memory copy would fight back — the next
/// token refresh re-persists it).
fn engine_lock(config: &EngineConfig, verb: &str) -> anyhow::Result<InstanceLock> {
    InstanceLock::acquire(&config.data_dir).map_err(|err| {
        anyhow::anyhow!(
            "{err}\nCannot {verb} while an engine is running — stop it first \
             (`comet daemon stop`, or quit the Comet app), or use the running UI instead."
        )
    })
}
