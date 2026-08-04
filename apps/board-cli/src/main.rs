//! `comet-board` — the board's command line, and the agents' entry point
//! (docs/BOARD.md §H6; the operator trio landed with §H8).
//!
//! A thin binary over `crates/board`, attaching to the local engine exactly as
//! `comet-tui` does: the engine owns the board loop and the workspace doc, and
//! this speaks the typed RPC on the localhost IPC WebSocket — `WatchBoard` for
//! `list`/`wait`, `DispatchTask`/`CancelTask` for the verbs, `WatchSpaces` for
//! the one thing the setup commands cannot know (which spaces exist on this
//! device). Everything else (config, detection, the routing.toml writer, the
//! report, ticket writing) is library code with tests, plus [`ops`] here.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use comet_board::adopt;
use comet_board::config::Paths;
use comet_board::doctor::EngineStatus;
use comet_board::log::Logger;
use comet_proto::Space;
use comet_rpc::{RpcClient, connect_ws, methods};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

mod ops;

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
    /// List what is on the board. `--json` for orchestrating agents.
    List {
        /// Only this state: blocked, working, ready, review, failed, done.
        #[arg(long)]
        state: Option<String>,
        /// Only this source: linear or github.
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Release a task into a coding-agent chat.
    Dispatch {
        #[arg(long)]
        task: String,
        /// The dispatching chat's id — provenance, never authority. Normally
        /// omitted: a dispatch from inside a board-dispatched chat reads its
        /// own id from COMET_BOARD_CHAT_ID. Pass this only when releasing work
        /// on behalf of a chat that is not you.
        #[arg(long)]
        via: Option<String>,
        /// Override the route's configured runtime for this dispatch — e.g.
        /// claude-code, opencode, codex, cursor. `comet-board list --json`
        /// shows each row's default. An unknown one is refused, naming the
        /// set the engine offers.
        #[arg(long)]
        runtime: Option<String>,
        /// Override the harness's default model for this dispatch. Checked
        /// against that runtime's catalog before anything is dispatched; an
        /// unknown one is refused, naming the catalog.
        #[arg(long)]
        model: Option<String>,
        /// Agent-account slot id to run under — whose Claude/Codex
        /// subscription this dispatch spends. Defaults to the route's
        /// `account`, and failing that the device's own CLI login.
        /// `comet-board doctor` lists the ids this device has saved.
        #[arg(long)]
        account: Option<String>,
    },
    /// Cancel a task's live attempt. The issue stays open.
    Cancel {
        #[arg(long)]
        task: String,
    },
    /// Block until watched work settles. The counterpart to `dispatch`.
    Wait {
        /// Task to watch; repeat for several. Omit to watch everything in
        /// flight right now.
        #[arg(long)]
        task: Vec<String>,
        /// States that count as settled. Defaults to review, failed and done.
        #[arg(long)]
        state: Vec<String>,
        /// Give up after this many seconds.
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Write a ticket. Cheaper than not writing one.
    New {
        title: String,
        /// Description. `-` reads it from stdin.
        #[arg(long)]
        body: Option<String>,
        /// Linear team key. Only needed when you have more than one.
        #[arg(long)]
        team: Option<String>,
        /// Labels to apply — this is what routes it.
        #[arg(long)]
        label: Vec<String>,
        /// Which tracker to write to: linear or github. Defaults to
        /// `[defaults] new_source`.
        #[arg(long)]
        source: Option<String>,
        /// `owner/repo` when writing to GitHub and more than one is configured.
        #[arg(long)]
        repo: Option<String>,
        /// Dispatch it as soon as it reaches the board.
        #[arg(long)]
        dispatch: bool,
    },
    /// What the board knows about its own throughput.
    Stats {
        /// Only the last N days. Omit for everything.
        #[arg(long)]
        since_days: Option<i64>,
        #[arg(long)]
        json: bool,
    },
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
    /// git's askpass helper: print the credential for pushing to the repo named
    /// by COMET_BOARD_ASKPASS_REPO (gh#58).
    ///
    /// Not for people. `git` runs this itself when `GIT_ASKPASS` points at it,
    /// which is how an App's installation token reaches a push without being
    /// written into `.git/config`, argv, or the environment.
    #[command(hide = true)]
    GitAskpass {
        /// The prompt git is asking — "Username for …" or "Password for …".
        prompt: Option<String>,
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

    // One-shot commands on a current-thread runtime; the async work is the IPC
    // round-trips (`wait` holds its subscription open, but it is still the
    // only thing running). The tracker clients in `new` are blocking and run
    // outside the runtime on purpose.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    match cli.command {
        // Answered before anything else touches the engine: git runs this
        // synchronously in the middle of a push, and it has no business dialling
        // an IPC port to print one line.
        Command::GitAskpass { prompt } => {
            let repo = std::env::var(comet_board::git_credentials::ASKPASS_REPO_ENV).ok();
            let secret = comet_board::git_credentials::askpass(
                &paths,
                prompt.as_deref().unwrap_or_default(),
                repo.as_deref(),
            )?;
            // Straight to stdout, which is the pipe git is holding. Nowhere else.
            println!("{secret}");
            Ok(())
        }
        Command::List {
            state,
            source,
            json,
        } => {
            ops::validate_filters(state.as_deref(), source.as_deref())?;
            let rows = runtime.block_on(async {
                let client = ops::attach(port).await?;
                ops::board_rows(&client).await
            })?;
            ops::print_tasks(
                &ops::filter_rows(rows, state.as_deref(), source.as_deref()),
                json,
            )
        }
        Command::Dispatch {
            task,
            via,
            runtime: runtime_flag,
            model,
            account,
        } => {
            let via = ops::provenance(via);
            let d = runtime.block_on(async {
                let client = ops::attach(port).await?;
                ops::dispatch_checked(
                    &client,
                    &task,
                    via.as_deref(),
                    runtime_flag.as_deref(),
                    model.as_deref(),
                    account.as_deref(),
                )
                .await
            })?;
            println!(
                "dispatched {task} → chat {} (attempt {}, {})",
                d.chat_id, d.attempt, d.cwd
            );
            if let Some(v) = &via {
                println!("released by chat {v}");
            }
            if runtime_flag.is_some() || model.is_some() || account.is_some() {
                println!(
                    "overrides: {}",
                    [
                        runtime_flag.as_deref().map(|r| format!("runtime={r}")),
                        model.as_deref().map(|m| format!("model={m}")),
                        account.as_deref().map(|a| format!("account={a}")),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(", ")
                );
            }
            Ok(())
        }
        Command::Cancel { task } => {
            runtime.block_on(async {
                let client = ops::attach(port).await?;
                // The row first: `CancelTask` answers `{ok}`, and the parent
                // that will not be notified is on the row. Best-effort — a
                // failed read must not block the cancel it precedes.
                let parent = ops::board_rows(&client)
                    .await
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|r| r.id == task))
                    .and_then(|r| r.dispatched_by_chat);
                ops::cancel(&client, &task).await?;
                println!("cancelled {task} — the issue is still open");
                // Nothing tells the parent. Say so where the caller will see
                // it, rather than leaving a waiting agent to be discovered.
                if let Some(p) = parent {
                    println!("dispatched by chat {p} — not notified");
                }
                Ok(())
            })
        }
        Command::Wait {
            task,
            state,
            timeout,
            json,
        } => {
            // Finished, one way or another: work to look at, work that broke,
            // or work whose ticket closed under it.
            let states = if state.is_empty() {
                vec!["review".to_string(), "failed".into(), "done".into()]
            } else {
                state
            };
            let rows = runtime.block_on(async {
                let client = ops::attach(port).await?;
                ops::wait_for(&client, &task, &states, timeout.map(Duration::from_secs)).await
            })?;
            ops::print_tasks(&rows, json)
        }
        Command::New {
            title,
            body,
            team,
            label,
            source,
            repo,
            dispatch: and_dispatch,
        } => {
            let body = match body.as_deref() {
                Some("-") => {
                    let mut buf = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                    Some(buf)
                }
                other => other.map(str::to_string),
            };
            // Unvalidated on purpose, same as adopt: writing a ticket must stay
            // usable while some unrelated route is broken. An absent file still
            // works — the defaults say where new tickets go.
            let routing = paths.routing();
            let cfg = if routing.exists() {
                comet_board::config::RoutingConfig::load_unvalidated(&routing)?
            } else {
                comet_board::config::RoutingConfig::default()
            };
            let (identifier, url) = ops::new_task(
                &paths,
                &cfg,
                &ops::NewTask {
                    title: &title,
                    body: body.as_deref(),
                    team: team.as_deref(),
                    labels: &label,
                    source: source.as_deref(),
                    repo: repo.as_deref(),
                },
            )?;
            println!("{identifier}  {url}");

            if and_dispatch {
                // It has to be on the board before it can be dispatched, and
                // the engine's sync loop is what puts it there — wait for its
                // next poll rather than failing on the race.
                // `AGE-14` is Linear; `owner/repo#87` is GitHub.
                let id = if identifier.contains('/') {
                    format!("gh:{identifier}")
                } else {
                    format!("linear:{identifier}")
                };
                let pickup = Duration::from_secs(cfg.sync.interval_secs() * 2 + 30);
                let via = ops::provenance(None);
                let d = runtime.block_on(async {
                    let client = ops::attach(port).await?;
                    ops::await_row(&client, &id, pickup).await?;
                    ops::dispatch(&client, &id, via.as_deref(), None, None, None).await
                })?;
                println!(
                    "dispatched {id} → chat {} (attempt {})",
                    d.chat_id, d.attempt
                );
            }
            Ok(())
        }
        Command::Stats { since_days, json } => {
            let log = Arc::new(Logger::new(paths.logfile(), false));
            let s = comet_board::stats::run(&paths, log, since_days)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                comet_board::stats::print(&s);
            }
            Ok(())
        }
        Command::Doctor => {
            let (engine, spaces, accounts) = match runtime.block_on(fetch_spaces(port)) {
                Ok((device, spaces, accounts)) => (
                    EngineStatus {
                        reachable: true,
                        detail: format!(
                            "listening on 127.0.0.1:{port} (device {device}, {} space(s) here)",
                            spaces.len()
                        ),
                    },
                    Some(spaces),
                    Some(accounts),
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
                    None,
                ),
            };
            let checks = comet_board::doctor::doctor(
                &paths,
                &engine,
                spaces.as_deref(),
                accounts.as_deref(),
            )?;
            if !comet_board::doctor::print_doctor(&checks) {
                std::process::exit(1);
            }
            Ok(())
        }
        Command::Init { force } => {
            let (_, spaces, _) = runtime.block_on(fetch_spaces(port)).with_context(|| {
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

            let (_, spaces, _) = runtime.block_on(fetch_spaces(port)).with_context(|| {
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
            // credential or no network degrades to adopting without the numbers.
            let preview = comet_board::sources::github::HttpRest::from_paths(&paths)
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
async fn fetch_spaces(
    port: u16,
) -> Result<(String, Vec<Space>, Vec<comet_proto::AgentAccount>)> {
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
        // The saved agent logins, for the route `account` check. Offline list
        // (no `forceUsage`): doctor wants the ids and who they belong to, not
        // a round of rate-limit probes.
        let accounts: Vec<comet_proto::AgentAccount> = client
            .call(methods::LIST_AGENT_ACCOUNTS, serde_json::json!({}))
            .await
            .ok()
            .and_then(|v| serde_json::from_value::<comet_proto::AgentAccountsSnapshot>(v).ok())
            .map(|s| s.accounts)
            .unwrap_or_default();
        Ok::<_, anyhow::Error>((device, local, accounts))
    };
    tokio::time::timeout(FETCH_TIMEOUT, fetch)
        .await
        .map_err(|_| anyhow::anyhow!("timed out after {}s", FETCH_TIMEOUT.as_secs()))?
}
