//! `comet-board` — the board's command line (docs/BOARD.md §H8, growing into
//! §H6's full surface).
//!
//! A thin binary over `crates/board`, attaching to the local engine exactly as
//! `comet-tui` does: the engine owns the board loop and the workspace doc, and
//! this asks it over the localhost IPC WebSocket for the one thing the library
//! cannot know — which spaces exist on this device. Everything else (config,
//! detection, the routing.toml writer, the report) is library code with tests.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use comet_board::adopt;
use comet_board::config::Paths;
use comet_board::doctor::EngineStatus;
use comet_proto::Space;
use comet_rpc::{RpcClient, connect_ws, methods};
use std::path::PathBuf;
use std::time::Duration;

/// Same default as `apps/comet` and `comet-tui`.
const DEFAULT_IPC_PORT: u16 = 27654;

/// The engine answers a snapshot immediately; anything slower than this is a
/// listener that is not the engine.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser)]
#[command(
    name = "comet-board",
    about = "Task board over comet — Linear/GitHub issues in, coding-agent chats out"
)]
struct Cli {
    /// Localhost IPC port the engine serves (env: COMET_IPC_PORT).
    #[arg(long, global = true)]
    port: Option<u16>,
    /// Engine data directory (env: COMET_DATA_DIR).
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check the environment: keys, engine, routes, repos. Exits non-zero on
    /// any failing check.
    Doctor,
    /// Generate a starter routing.toml from the spaces on this device.
    Init {
        /// Overwrite an existing routing.toml.
        #[arg(long)]
        force: bool,
    },
    /// Offer git-detected spaces the board is not watching; adopt one by slug.
    ///
    /// With no slug: list what could be adopted. With one: write the missing
    /// `[[route]]` / `[github] repos` halves (validated, with a .bak beside).
    Adopt {
        /// `owner/repo` from the list `comet-board adopt` prints.
        slug: Option<String>,
        /// Poll only issues carrying one of these labels (comma-separated) —
        /// writes a `[[github.repo]]` filter so a roadmap-sized backlog does
        /// not land on the board whole.
        #[arg(long, value_delimiter = ',')]
        labels: Option<Vec<String>>,
        /// Poll every open issue, said out loud (writes `labels = []`,
        /// overriding a narrower global filter).
        #[arg(long, conflicts_with = "labels")]
        all_issues: bool,
        /// Stop offering this repo — you are only reading it.
        #[arg(long, requires = "slug")]
        ignore: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let port = cli
        .port
        .or_else(|| {
            std::env::var("COMET_IPC_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
        })
        .unwrap_or(DEFAULT_IPC_PORT);
    let paths = match &cli.data_dir {
        Some(dir) => Paths::under(dir)?,
        None => Paths::discover()?,
    };

    // One-shot commands on a current-thread runtime; the only async work is
    // the IPC round-trip.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    match cli.command {
        Command::Doctor => {
            let (engine, spaces) = match runtime.block_on(fetch_spaces(port)) {
                Ok((device, spaces)) => (
                    EngineStatus {
                        reachable: true,
                        detail: format!(
                            "listening on 127.0.0.1:{port} (device {device}, {} space(s) here)",
                            spaces.len()
                        ),
                    },
                    Some(spaces),
                ),
                Err(e) => (
                    EngineStatus {
                        reachable: false,
                        detail: format!(
                            "not reachable on 127.0.0.1:{port} ({e:#}) — start `comet` or \
                             `comet headless`"
                        ),
                    },
                    None,
                ),
            };
            let checks = comet_board::doctor::doctor(&paths, &engine, spaces.as_deref())?;
            if !comet_board::doctor::print_doctor(&checks) {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Init { force } => {
            let (_, spaces) = runtime.block_on(fetch_spaces(port)).with_context(|| {
                format!(
                    "listing spaces from the engine on 127.0.0.1:{port} — \
                     start `comet` or `comet headless` first"
                )
            })?;
            comet_board::init::init(&paths, &spaces, adopt::probe, force)
        }
        Command::Adopt {
            slug,
            labels,
            all_issues,
            ignore,
        } => {
            let routing = paths.routing();
            if let (Some(slug), true) = (&slug, ignore) {
                adopt::ignore(&routing, slug)?;
                println!("{slug} will not be offered again (see [adopt] ignore)");
                return Ok(());
            }

            let (_, spaces) = runtime.block_on(fetch_spaces(port)).with_context(|| {
                format!(
                    "listing spaces from the engine on 127.0.0.1:{port} — \
                     start `comet` or `comet headless` first"
                )
            })?;
            // Unvalidated on purpose, same as doctor: adoption must stay
            // usable while some unrelated route is broken.
            let cfg = comet_board::config::RoutingConfig::load_unvalidated(&routing).with_context(
                || {
                    format!(
                        "reading {} — run `comet-board init` if it does not exist yet",
                        routing.display()
                    )
                },
            )?;
            let found = adopt::detect(&spaces, &cfg, adopt::probe);

            let Some(slug) = slug else {
                if found.is_empty() {
                    println!("every space with a GitHub remote is on the board");
                } else {
                    for u in &found {
                        println!("{:<40} {}{}", u.slug, u.label, u.missing.note());
                    }
                    println!(
                        "\nadopt one:   comet-board adopt <owner/repo> [--labels a,b | --all-issues]\
                         \nstop offers: comet-board adopt <owner/repo> --ignore"
                    );
                }
                return Ok(());
            };

            let Some(u) = found.iter().find(|u| u.slug.eq_ignore_ascii_case(&slug)) else {
                bail!(
                    "`{slug}` is not on the unadopted list — run `comet-board adopt` to see it; \
                     already-adopted and ignored repos are not offered"
                );
            };

            // What this is about to pull, before it pulls it. Best-effort: no
            // token or no network degrades to adopting without the numbers.
            let preview = comet_board::sources::github::HttpRest::new(
                comet_board::config::github_token(&paths),
            )
            .ok()
            .map(comet_board::sources::github::Github::new)
            .and_then(|gh| adopt::preview(&gh, &u.slug).ok());
            if let Some(p) = &preview {
                println!("{}: {}", u.slug, p.count_phrase());
                for (label, n) in p.labels.iter().take(8) {
                    println!("  {n:>4}  {label}");
                }
            }

            let labels: Option<Vec<String>> = if all_issues { Some(Vec::new()) } else { labels };
            let done = adopt::adopt_with(&routing, u, labels.as_deref())?;

            let mut wrote = Vec::new();
            if done.wrote_route {
                wrote.push("a [[route]]".to_string());
            }
            if done.wrote_repo {
                wrote.push("[github] repos".to_string());
            }
            if let Some(l) = &done.labels {
                wrote.push(if l.is_empty() {
                    "a [[github.repo]] polling every open issue".to_string()
                } else {
                    format!("a [[github.repo]] filter: {}", l.join(", "))
                });
            }
            println!("adopted {} → wrote {}", u.slug, wrote.join(" + "));
            if labels.is_none()
                && let Some(p) = &preview
                && p.open_issues > 20
            {
                println!(
                    "note: the global [github] labels filter applies — {} would arrive \
                     unfiltered; re-run with --labels to narrow what this repo contributes",
                    p.count_phrase()
                );
            }
            println!(
                "a commented `label = \"{}\"` route was left for Linear issues — edit it if \
                 that guess is wrong",
                done.suggested_label
            );
            Ok(())
        }
    }
}

/// This device's spaces, from the engine: `LocalDevice` for the device id, the
/// first `WatchSpaces` snapshot for the rows, filtered to spaces this device
/// owns — a route's `repo =` is a local path, and another device's folders are
/// not on this disk.
async fn fetch_spaces(port: u16) -> Result<(String, Vec<Space>)> {
    let fetch = async {
        let client: RpcClient = connect_ws(&format!("ws://127.0.0.1:{port}")).await?;
        let device = client
            .call(methods::LOCAL_DEVICE, serde_json::json!({}))
            .await?
            .get("deviceId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("LocalDevice reply missing deviceId"))?;
        let mut stream = client
            .subscribe(methods::WATCH_SPACES, serde_json::json!({}))
            .await?;
        let snapshot = stream
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("WatchSpaces stream ended before a snapshot"))?;
        let spaces: Vec<Space> = serde_json::from_value(snapshot)?;
        let local: Vec<Space> = spaces
            .into_iter()
            .filter(|s| s.device_id == device)
            .collect();
        Ok::<_, anyhow::Error>((device, local))
    };
    tokio::time::timeout(FETCH_TIMEOUT, fetch)
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}s", FETCH_TIMEOUT.as_secs()))?
}
