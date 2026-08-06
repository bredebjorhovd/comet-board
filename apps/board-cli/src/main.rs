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
//!
//! The board it drives need not be this device's. `--device` names the box that
//! hosts it, and the board RPCs are relay-forwardable (gh#55), so the dial
//! stays localhost and the local engine forwards — the same thing the desktop
//! panel and the TUI pane do (gh#73).

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
    /// Which device hosts the board — its name or id (env:
    /// COMET_BOARD_DEVICE). Omit for this device's own board.
    ///
    /// The board store lives on exactly one device, usually the always-on box,
    /// and the board RPCs are relay-forwarded: this still dials the local
    /// engine, which passes the call on. Applies to the verbs that ask the
    /// engine — list, dispatch, retry, cancel, wait, `new --dispatch`, and
    /// `routes`, which reads and writes the host's routing.toml (gh#75). The
    /// rest (doctor, init, adopt, stats) read this device's own files either
    /// way, and are unaffected.
    #[arg(long, global = true)]
    device: Option<String>,
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
        /// Spend an account that is not yours, on purpose (gh#101).
        ///
        /// Under `[defaults] billing_guard = "require-own"` a dispatch that
        /// would charge somebody else's subscription is refused; this is the
        /// acknowledgement that releases it, and it has to NAME the payer —
        /// the agent-account slot id (which also selects it, so `--account` is
        /// then redundant) or the email on the login being spent, which is the
        /// only spelling there is when that login is the box's own. Under
        /// `warn` and `off` it changes nothing but which slot is picked.
        #[arg(long)]
        bill: Option<String>,
    },
    /// Release a task again — the desktop panel's Retry, from a shell.
    ///
    /// On a blocked row this ends the live attempt and releases a fresh one in
    /// the same call, which cancel-then-dispatch cannot do: between the two the
    /// row is `ready` and anything else may take its slot. On a failed or ready
    /// row nothing is live, so it is an ordinary dispatch.
    Retry {
        #[arg(long)]
        task: String,
        /// The retrying chat's id — provenance, never authority. Normally
        /// omitted; see `dispatch --via`.
        #[arg(long)]
        via: Option<String>,
        /// Override the route's configured runtime for this attempt.
        #[arg(long)]
        runtime: Option<String>,
        /// Override the harness's default model for this attempt.
        #[arg(long)]
        model: Option<String>,
        /// Agent-account slot id to run under.
        #[arg(long)]
        account: Option<String>,
        /// Spend an account that is not yours, on purpose (gh#101).
        ///
        /// Under `[defaults] billing_guard = "require-own"` a dispatch that
        /// would charge somebody else's subscription is refused; this is the
        /// acknowledgement that releases it, and it has to NAME the payer —
        /// the agent-account slot id (which also selects it, so `--account` is
        /// then redundant) or the email on the login being spent, which is the
        /// only spelling there is when that login is the box's own. Under
        /// `warn` and `off` it changes nothing but which slot is picked.
        #[arg(long)]
        bill: Option<String>,
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
        /// Also return when a watched task goes `blocked` — its agent is
        /// waiting on an answer, and nothing else will settle until it gets
        /// one. Adds to the settle states rather than replacing them.
        #[arg(long)]
        blocked_is_settled: bool,
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
    /// Read and change the board's `routing.toml` — over the RPC, so `--device`
    /// reaches the box that hosts the board (gh#75).
    ///
    /// The counterpart to `adopt`, which does the same writing against *this*
    /// device's files. Use this one whenever the board is not yours: it needs no
    /// shell on the box, and every write is validated and backed up before it
    /// lands, exactly as `adopt`'s is.
    Routes {
        #[command(subcommand)]
        command: RoutesCommand,
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
    /// Print a GitHub token for the repo named by COMET_BOARD_ASKPASS_REPO
    /// (gh#68).
    ///
    /// Not for people either: the `gh` shim on a dispatched agent's PATH runs
    /// this so `gh pr create` authenticates as the board's App installation
    /// with a token minted for that invocation, rather than one that was
    /// already an hour old when the agent got round to opening the PR.
    #[command(hide = true)]
    GhToken,
}

#[derive(Subcommand)]
enum RoutesCommand {
    /// The routes in force, what is wrong with the config, and what is not
    /// routed yet.
    List {
        /// The whole reply as JSON — text, parse, problems, and what is
        /// unadopted.
        #[arg(long)]
        json: bool,
    },
    /// Print `routing.toml` verbatim. Comments and all: this is the file.
    Show,
    /// Route a repo that has a space on the board's device but nothing
    /// watching it — the `[[route]]` and `[github] repos` halves, written
    /// together.
    Add {
        /// `owner/repo`, as `comet-board routes list` offers it.
        slug: String,
        /// Poll only issues carrying one of these labels (comma-separated), so
        /// a roadmap-sized backlog does not land on the board whole.
        #[arg(long, value_delimiter = ',')]
        labels: Option<Vec<String>>,
        /// Poll every open issue, said out loud (writes `labels = []`).
        #[arg(long, conflicts_with = "labels")]
        all_issues: bool,
    },
    /// Stop offering a repo — you are only reading it.
    Ignore { slug: String },
    /// Set one key on one route: `routes set 2 account brede-personal`.
    ///
    /// The number is the one `routes list` prints. Anything this cannot
    /// express — a prompt, a match, the order of the routes — is `routes edit`.
    Set {
        /// Route number, as printed by `routes list` (1-based).
        route: usize,
        /// One of: name, workspace, repo, runtime, account, branch_template,
        /// base, max_concurrent, max_duration, billing_guard.
        key: String,
        /// The new value. Omit with `--unset` to remove the key, which falls
        /// the route back to `[defaults]`.
        value: Option<String>,
        /// Remove the key instead of setting it.
        #[arg(long, conflicts_with = "value")]
        unset: bool,
    },
    /// Set one key under `[defaults]`: `routes defaults max_duration 4h`.
    Defaults {
        /// One of: max_concurrent_per_workspace, branch_template, base,
        /// notify, notify_dispatcher, new_source, max_duration,
        /// billing_guard.
        key: String,
        value: Option<String>,
        #[arg(long, conflicts_with = "value")]
        unset: bool,
    },
    /// Open `routing.toml` in `$EDITOR` and write it back, validated.
    ///
    /// The escape hatch that keeps this surface honest: everything the typed
    /// edits cannot say is still one command away, on a board you have no shell
    /// on. A file that would not validate is refused and the box keeps the
    /// config it had; an edit that lands on a file somebody changed meanwhile
    /// is refused too, rather than quietly overwriting them.
    Edit,
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
    // Which device's board the verbs drive. The environment carries it so an
    // orchestrator points its whole shell at the box once, instead of
    // threading the flag through every call it makes.
    let device = cli
        .device
        .or_else(|| std::env::var("COMET_BOARD_DEVICE").ok())
        .filter(|d| !d.is_empty());
    let device = device.as_deref();

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
        // Same deal for `gh`, and for the same reason it is answered before
        // anything dials the engine: this runs inside every `gh` the agent
        // invokes.
        Command::GhToken => {
            let repo = std::env::var(comet_board::git_credentials::ASKPASS_REPO_ENV)
                .ok()
                .filter(|r| !r.is_empty())
                .with_context(|| {
                    format!(
                        "{} is not set — nothing to mint for",
                        comet_board::git_credentials::ASKPASS_REPO_ENV
                    )
                })?;
            println!("{}", comet_board::git_credentials::token(&paths, &repo)?);
            Ok(())
        }
        Command::List {
            state,
            source,
            json,
        } => {
            ops::validate_filters(state.as_deref(), source.as_deref())?;
            let rows = runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                ops::board_rows(&board).await
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
            bill,
        } => {
            let via = ops::provenance(via);
            let opts = ops::DispatchOpts {
                via: via.as_deref(),
                runtime: runtime_flag.as_deref(),
                model: model.as_deref(),
                account: account.as_deref(),
                bill: bill.as_deref(),
                // Filled in from the engine below — this shell has no way to
                // know who is signed in without asking.
                via_user: None,
                replace: false,
            };
            let d = runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                // Who this shell is, before anything else: it rides on the
                // dispatch as `viaUser` (gh#74) and is the other half of the
                // billing comparison (gh#101).
                let me = ops::signed_in_email(&board).await;
                let opts = ops::DispatchOpts {
                    via_user: me.as_deref(),
                    ..opts
                };
                warn_if_cross_billed(&board, &task, opts).await;
                ops::dispatch_checked(&board, &task, opts).await
            })?;
            println!(
                "dispatched {task} → chat {} (attempt {}, {})",
                d.chat_id, d.attempt, d.cwd
            );
            if let Some(v) = &via {
                println!("released by chat {v}");
            }
            print_overrides(&opts);
            Ok(())
        }
        Command::Retry {
            task,
            via,
            runtime: runtime_flag,
            model,
            account,
            bill,
        } => {
            let via = ops::provenance(via);
            let opts = ops::DispatchOpts {
                via: via.as_deref(),
                runtime: runtime_flag.as_deref(),
                model: model.as_deref(),
                account: account.as_deref(),
                bill: bill.as_deref(),
                // `replace` is not set here: `retry` reads the row and decides,
                // because ending a live attempt is not something a verb should
                // do without looking at what it is ending.
                ..Default::default()
            };
            let r = runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                let me = ops::signed_in_email(&board).await;
                let opts = ops::DispatchOpts {
                    via_user: me.as_deref(),
                    ..opts
                };
                // A retry bills someone too — it is a fresh attempt on the same
                // account, and the one that ends a live agent is exactly the
                // one nobody wants to have spent on the wrong plan.
                warn_if_cross_billed(&board, &task, opts).await;
                ops::retry(&board, &task, opts).await
            })?;
            println!(
                "retried {task} → chat {} (attempt {}, {})",
                r.dispatched.chat_id, r.dispatched.attempt, r.dispatched.cwd
            );
            // A retry that ended a live agent has to say so — that is the
            // difference between this and a first dispatch, and the operator
            // is the only one who can tell whether the answer it was waiting
            // for is now lost.
            if r.replaced {
                println!("replaced the live attempt the {} row was stuck on", r.was);
            }
            if let Some(v) = &via {
                println!("released by chat {v}");
            }
            print_overrides(&opts);
            Ok(())
        }
        Command::Cancel { task } => {
            runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                // The row first: `CancelTask` answers `{ok}`, and the parent
                // that will not be notified is on the row. Best-effort — a
                // failed read must not block the cancel it precedes.
                let parent = ops::board_rows(&board)
                    .await
                    .ok()
                    .and_then(|rows| rows.into_iter().find(|r| r.id == task))
                    .and_then(|r| r.dispatched_by_chat);
                ops::cancel(&board, &task).await?;
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
            blocked_is_settled,
            timeout,
            json,
        } => {
            let states = ops::settle_states(&state, blocked_is_settled);
            let rows = runtime.block_on(async {
                let board = ops::attach(port, device).await?;
                ops::wait_for(&board, &task, &states, timeout.map(Duration::from_secs)).await
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
                    let board = ops::attach(port, device).await?;
                    ops::await_row(&board, &id, pickup).await?;
                    let me = ops::signed_in_email(&board).await;
                    let opts = ops::DispatchOpts {
                        via: via.as_deref(),
                        via_user: me.as_deref(),
                        ..Default::default()
                    };
                    // The route's own account, on a ticket that did not exist a
                    // second ago — the case where nobody has had a chance to
                    // notice whose plan it lands on (gh#101).
                    warn_if_cross_billed(&board, &id, opts).await;
                    ops::dispatch(&board, &id, opts).await
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
        Command::Routes { command } => runtime.block_on(async {
            let board = ops::attach(port, device).await?;
            routes(&board, command).await
        }),
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

/// `comet-board routes …` — the routing.toml surface (gh#75).
///
/// Every branch is one RPC round trip against the board's host, and every write
/// answers with a fresh read: what gets printed after a change is the file as it
/// now stands, not what we hoped it would say.
async fn routes(board: &ops::Board, command: RoutesCommand) -> Result<()> {
    match command {
        RoutesCommand::List { json } => {
            let cfg = ops::read_config(board).await?;
            ops::print_config(&cfg, board.host(), json)
        }
        RoutesCommand::Show => {
            let cfg = ops::read_config(board).await?;
            if !cfg.routing.exists {
                // Not on stdout: `routes show > routing.toml` must not capture
                // an explanation as if it were config.
                eprintln!(
                    "{} does not exist yet — `comet-board init` writes the first one",
                    cfg.routing.path
                );
                return Ok(());
            }
            print!("{}", cfg.routing.text);
            Ok(())
        }
        RoutesCommand::Add {
            slug,
            labels,
            all_issues,
        } => {
            let labels: Option<Vec<String>> = if all_issues { Some(Vec::new()) } else { labels };
            let cfg = ops::write_config(
                board,
                serde_json::json!({ "op": "adopt", "slug": slug, "labels": labels }),
            )
            .await?;
            println!("adopted {slug}");
            ops::print_write_result(&cfg);
            Ok(())
        }
        RoutesCommand::Ignore { slug } => {
            let cfg = ops::write_config(board, serde_json::json!({ "op": "ignore", "slug": slug }))
                .await?;
            println!("{slug} will not be offered again (see [adopt] ignore)");
            ops::print_write_result(&cfg);
            Ok(())
        }
        RoutesCommand::Set {
            route,
            key,
            value,
            unset,
        } => {
            let value = settable(value, unset, &key)?;
            // 1-based for a person counting routes in a file, 0-based on the
            // wire. `routes list` prints the former.
            let index = route
                .checked_sub(1)
                .ok_or_else(|| anyhow::anyhow!("routes are numbered from 1"))?;
            let cfg = ops::write_config(
                board,
                serde_json::json!({"op": "route", "route": index, "key": key, "value": value}),
            )
            .await?;
            ops::print_write_result(&cfg);
            Ok(())
        }
        RoutesCommand::Defaults { key, value, unset } => {
            let value = settable(value, unset, &key)?;
            let cfg = ops::write_config(
                board,
                serde_json::json!({"op": "default", "key": key, "value": value}),
            )
            .await?;
            ops::print_write_result(&cfg);
            Ok(())
        }
        RoutesCommand::Edit => edit_routing(board).await,
    }
}

/// A `set`'s value: given, or explicitly cleared. Refusing the empty case is
/// the point — `routes set 1 account` with neither a value nor `--unset` is
/// somebody who meant one of them, and guessing which would either write an
/// empty string or delete their configuration.
fn settable(value: Option<String>, unset: bool, key: &str) -> Result<Option<String>> {
    match (value, unset) {
        (Some(v), _) => Ok(Some(v)),
        (None, true) => Ok(None),
        (None, false) => bail!("give a value for `{key}`, or `--unset` to remove it"),
    }
}

/// `routes edit` — the file into `$EDITOR` and back, with the read it started
/// from carried along so a hand-edit on the box in the meantime is refused
/// rather than overwritten.
async fn edit_routing(board: &ops::Board) -> Result<()> {
    let before = ops::read_config(board).await?;
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|e| !e.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("$EDITOR is not set — set it, or use `comet-board routes set`")
        })?;

    // A scratch copy: the real file is on the board's device, which may not be
    // this one, and even when it is the write has to go through the validating
    // writer rather than round the side of it.
    let scratch =
        std::env::temp_dir().join(format!("comet-board-routing-{}.toml", std::process::id()));
    std::fs::write(&scratch, &before.routing.text)
        .with_context(|| format!("writing {}", scratch.display()))?;

    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(&scratch)
        .status()
        .with_context(|| format!("running $EDITOR ({editor})"))?;
    let edited = std::fs::read_to_string(&scratch)
        .with_context(|| format!("reading {} back", scratch.display()));
    let _ = std::fs::remove_file(&scratch);
    if !status.success() {
        bail!("$EDITOR exited {status} — routing.toml is untouched");
    }
    let edited = edited?;

    if edited == before.routing.text {
        println!("no changes");
        return Ok(());
    }
    let cfg = ops::write_config(
        board,
        serde_json::json!({"op": "text", "text": edited, "base": before.routing.text}),
    )
    .await?;
    ops::print_write_result(&cfg);
    Ok(())
}

/// Say who a release is about to charge, before it charges them (gh#101).
///
/// On stderr, and never fatal: the engine applies the guard itself — it is the
/// only side that can refuse under `require-own` — so this is the line that
/// makes the spend visible in the surface the operator is actually looking at.
/// A release the engine then refuses has said it twice, which is the right
/// number of times for the sentence "somebody else is paying for this".
async fn warn_if_cross_billed(board: &ops::Board, task: &str, opts: ops::DispatchOpts<'_>) {
    if let Some(warning) = ops::cross_billing_warning(board, task, opts).await {
        eprintln!("{warning}");
    }
}

/// Echo the overrides a release was given, so the confirmation says what the
/// attempt is actually running under and not just that it started.
fn print_overrides(opts: &ops::DispatchOpts<'_>) {
    let parts: Vec<String> = [
        opts.runtime.map(|r| format!("runtime={r}")),
        opts.model.map(|m| format!("model={m}")),
        opts.account.map(|a| format!("account={a}")),
        opts.bill.map(|b| format!("bill={b}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !parts.is_empty() {
        println!("overrides: {}", parts.join(", "));
    }
}

/// This device's spaces, from the engine: `LocalDevice` for the device id, the
/// first `WatchSpaces` snapshot for the rows, filtered to spaces this device
/// owns — a route's `repo =` is a local path, and another device's folders are
/// not on this disk.
async fn fetch_spaces(port: u16) -> Result<(String, Vec<Space>, Vec<comet_proto::AgentAccount>)> {
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
