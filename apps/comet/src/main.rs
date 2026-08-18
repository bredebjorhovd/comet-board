//! comet — headed by default; `comet headless` runs the engine alone. Auth is
//! decoupled from the daemon: `comet login` persists the session and exits, so a
//! service-managed `comet headless` only ever loads saved credentials.

mod auth_cli;
mod daemon;
mod update_cli;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "comet",
    version,
    about = "Multi-device controller for coding agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the engine without a UI (VPS / remote device mode).
    Headless,
    /// Sign in (paste-code flow), persist the session, and exit.
    Login,
    /// Remove the saved session.
    Logout,
    /// Show auth + engine status (exits nonzero when a sign-in is needed).
    Status,
    /// Manage `comet headless` as a background service (launchd / systemd --user).
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Check for a newer release and apply it (download → verify → swap →
    /// service restart). `--check` only reports (exits 1 when one is available).
    Update {
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCommand {
    /// Install, enable, and start the service (captures COMET_* env).
    Install,
    /// Stop and remove the service.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the service.
    Stop,
    /// Restart the service.
    Restart,
    /// Show the service manager's view of the daemon.
    Status,
}

/// Production edge (Cloudflare Worker + Durable Objects on the offhand.dev zone).
/// `COMET_EDGE_URL` overrides (local dev / self-hosting); `COMET_EDGE_URL=off`
/// disables the edge entirely (local mode — single-box deployments).
const DEFAULT_EDGE_URL: &str = "https://edge.comet.offhand.dev";

/// Production WorkOS AuthKit client id — public knowledge (it appears in every
/// authorize URL), so baking it in is safe. Overridden by `COMET_WORKOS_CLIENT_ID`;
/// set it to the empty string — or set a dev bearer via `COMET_EDGE_TOKEN` — to
/// force dev-mode auth instead.
const DEFAULT_WORKOS_CLIENT_ID: &str = "client_01KZ7CN5488CRQF7P0H7C12E8D";

fn edge_url_from_env() -> String {
    std::env::var("COMET_EDGE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_EDGE_URL.into())
}

/// WorkOS client id resolution: explicit env wins (empty string = dev mode);
/// otherwise a `COMET_EDGE_TOKEN` dev bearer keeps dev mode (smoke tests,
/// local wrangler); otherwise the baked production client id — so a bare
/// `comet headless` signs in against production with zero configuration.
fn workos_client_id_from_env(edge_token: &Option<String>) -> Option<String> {
    match std::env::var("COMET_WORKOS_CLIENT_ID") {
        Ok(v) if v.trim().is_empty() => None,
        Ok(v) => Some(v),
        Err(_) if edge_token.is_some() => None,
        Err(_) => Some(DEFAULT_WORKOS_CLIENT_ID.into()),
    }
}

/// mimalloc: system malloc (macOS libmalloc especially) never returns the
/// streaming churn's high-water pages, so transient allocation became
/// permanent RSS (gh#414).
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // Everything logs to stdout: long-running modes at info, one-shot CLI
    // commands at warn (RUST_LOG overrides either). This used to be skipped
    // for the TUI, which owned its own file tracing because a line on stdout
    // landed inside the alternate screen and corrupted it; with the TUI gone
    // (gh#416) no command needs the exemption.
    //
    // loro's internal block-encode diagnostics log at info and flood journald
    // on every snapshot export — enough to fill a disk on a long-running
    // headless host. Quiet them by default (RUST_LOG still overrides the whole
    // filter).
    let default_filter = match &cli.command {
        None | Some(Command::Headless) => "info,loro_internal=warn,loro=warn",
        Some(_) => "warn",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| default_filter.into()),
        )
        .init();

    match cli.command {
        Some(Command::Headless) => {
            comet_update::validate_running_managed_release()?;
            warn_on_stale_board_cli();
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(async {
                let engine = comet_engine::Engine::new(engine_config_from_env());
                engine.run().await
            })
        }
        Some(Command::Login) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::login(engine_config_from_env()))
        }
        Some(Command::Logout) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::logout(engine_config_from_env()))
        }
        Some(Command::Status) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(auth_cli::status(engine_config_from_env()))
        }
        Some(Command::Update { check }) => {
            let edge_url = edge_url_from_env();
            if comet_engine::edge_url_is_off(&edge_url) {
                anyhow::bail!(
                    "edge is disabled (COMET_EDGE_URL=off) — releases are served by the edge, \
                     so local-mode installs update from source or their own package flow"
                );
            }
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(update_cli::update(&edge_url, check))
        }
        Some(Command::Daemon { command }) => match command {
            DaemonCommand::Install => daemon::install(&engine_config_from_env().data_dir),
            DaemonCommand::Uninstall => daemon::uninstall(),
            DaemonCommand::Start => daemon::start(),
            DaemonCommand::Stop => daemon::stop(),
            DaemonCommand::Restart => daemon::restart(),
            DaemonCommand::Status => daemon::status(),
        },
        None => {
            let edge_token = std::env::var("COMET_EDGE_TOKEN").ok();
            // Headed: the UI probes COMET_IPC_PORT and connects to a running
            // daemon, or embeds the engine in-process (ARCHITECTURE §1).
            comet_ui::run_app(comet_ui::UiConfig {
                data_dir: std::env::var_os("COMET_DATA_DIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(dirs_data_dir),
                ipc_port: std::env::var("COMET_IPC_PORT")
                    .ok()
                    .and_then(|p| p.parse().ok())
                    .unwrap_or(27654),
                edge_url: edge_url_from_env(),
                workos_client_id: workos_client_id_from_env(&edge_token),
                edge_token,
                org_id: std::env::var("COMET_ORG_ID").ok(),
                default_harness: comet_ui::HarnessId::ClaudeCode,
            });
            Ok(())
        }
    }
}

/// Say once, at boot, when the `comet-board` on this box is not the one this
/// engine shipped with (gh#156).
///
/// Managed installs have already passed the synchronous, fail-closed sibling
/// check above (gh#496). This remaining warning covers an explicit override or
/// PATH copy that would answer instead in an unmanaged environment; managed
/// resolution is pinned to the validated sibling.
///
/// The one place the drift can be reported without anybody having gone looking
/// for it. `comet-board doctor` reports it too, but only from a CLI new enough
/// to carry the check — which the drifted ones are not, by definition. The
/// service restarts on every install, so this fires exactly when the gap opens,
/// into the journal somebody reads when the box misbehaves.
///
/// A warn and nothing more here: the managed payload cannot reach this point
/// incomplete, while an unmanaged source engine remains useful without a board
/// CLI installed beside it.
///
/// On its own thread for the same reason. The probe shells out to
/// `comet-board --version`, and the whole point is that the binary it runs is
/// one nobody vouches for — a copy that hangs instead of answering must not be
/// able to hold the engine's boot. Nothing waits on the result; it lands in the
/// log a few milliseconds after the engine is already up.
fn warn_on_stale_board_cli() {
    std::thread::spawn(|| {
        let engine = comet_update::current_version();
        let cli = comet_board::board_cli::probe();
        if cli.drifted(engine) {
            tracing::warn!("board CLI: {}", cli.line(engine).1);
        }
    });
}

/// The env-resolved engine configuration shared by `headless`, `login`,
/// `logout`, and `status` — one resolution so the CLI auth commands always
/// operate on the exact session the daemon will load.
fn engine_config_from_env() -> comet_engine::EngineConfig {
    // Dev-mode bearer (no WorkOS): an explicit token enables sync.
    let edge_token = std::env::var("COMET_EDGE_TOKEN").ok();
    comet_engine::EngineConfig {
        data_dir: std::env::var_os("COMET_DATA_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(dirs_data_dir),
        edge_url: edge_url_from_env(),
        ipc_port: std::env::var("COMET_IPC_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(27654),
        default_harness: harness_from_env(),
        // WorkOS mode: the signed-in session's org wins; COMET_ORG_ID (dev
        // default "dev-org") scopes the workspace room otherwise.
        org_id: std::env::var("COMET_ORG_ID").ok(),
        // Real auth against production by default; see
        // `workos_client_id_from_env` for the dev-mode escape hatches.
        workos_client_id: workos_client_id_from_env(&edge_token),
        edge_token,
        board: comet_engine::board_enabled_from_env(),
    }
}

/// `COMET_HARNESS` (kebab-case id) picks the default harness for chats without a
/// config row — `mock` powers the e2e smoke; default `claude-code`.
fn harness_from_env() -> comet_engine::HarnessId {
    match std::env::var("COMET_HARNESS").as_deref().map(str::trim) {
        Ok("mock") => comet_engine::HarnessId::Mock,
        Ok("codex") => comet_engine::HarnessId::Codex,
        Ok("cursor") => comet_engine::HarnessId::Cursor,
        Ok("opencode") => comet_engine::HarnessId::Opencode,
        _ => comet_engine::HarnessId::ClaudeCode,
    }
}

fn dirs_data_dir() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").expect("HOME not set");
    std::path::PathBuf::from(home).join(".comet-native")
}
