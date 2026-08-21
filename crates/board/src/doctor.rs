//! `comet-board doctor` — check the environment: keys present, engine
//! reachable, db writable, routes valid.
//!
//! Ported from herdr-board's doctor with the herdr checks replaced by comet
//! ones: a route's `workspace` must name a comet *space*, its `runtime` must
//! resolve to a comet *harness*, and the process everything runs in is the
//! engine on the local IPC port rather than a separate `syncd`. The checks
//! herdr needed against a mux it distrusted (manifest overrides, daemon
//! pidfiles) have no equivalent and are gone.

use crate::config::{Credentials, GithubAuth, Paths, RoutingConfig};
use crate::conventions;
use crate::db::Db;
use crate::gc;
use crate::git_credentials;
use crate::git_identity;
use crate::runtime::harness_for_runtime;
use crate::skill;
use crate::sources::github::{CapabilityEvidence, PushCapabilities, WriteCapability};
use anyhow::Result;
use comet_proto::view::board::RuntimeOption;
use comet_proto::{AgentAccount, EdgeHealth, HarnessId, Space};
use std::path::{Path, PathBuf};

pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

/// What the caller learned by dialing the engine's IPC port.
///
/// Doctor itself stays synchronous and network-free toward the engine; the
/// binary probes the WebSocket (and fetches the space list) before calling in.
pub struct EngineStatus {
    pub reachable: bool,
    /// `listening on 127.0.0.1:27654 (device d-…)`, or why the dial failed.
    pub detail: String,
    /// The running engine's own `CARGO_PKG_VERSION`, from `LocalDevice`
    /// (gh#156). `None` when the engine could not be asked, or is old enough
    /// not to say — both leave the version check reading off the disk instead.
    pub version: Option<String>,
    /// What the engine's release checker last saw at `{edge}/releases`
    /// (gh#197) — the first frame of the `UpdateStatus` stream. `None` when the
    /// engine could not be asked or does not know the verb; the check then says
    /// "not checked" rather than inventing a verdict about the edge.
    ///
    /// Read from the engine rather than fetched here on purpose: doctor's own
    /// network calls are to GitHub, about this box's config, and a
    /// second opinion about the release feed would be a second thing that can
    /// disagree with the updater that actually performs updates.
    pub update: Option<comet_update::UpdateStatus>,
    /// The other devices on this account that answered with a board of their
    /// own (gh#195). `None` when the sweep could not be run at all — the engine
    /// was not reachable, so nothing was asked and nothing may be claimed.
    pub peers: Option<Peers>,
}

/// What a sweep of the other devices found (gh#195).
///
/// Three facts, and the third is the one gh#155 paid for: a device that was
/// asked and refused hosts no board, a device that could not be asked says
/// nothing at all, and reporting the second as the first is how a board goes
/// missing from a report that looks complete.
#[derive(Debug, Clone, Default)]
pub struct Peers {
    /// Other devices that answered with a board of their own.
    pub boards: Vec<PeerBoard>,
    /// Devices that were present and still could not be asked — display names.
    /// Their silence is a fact about the connection, not about their board.
    pub unreachable: Vec<String>,
    /// How many other devices the sweep asked, present or not.
    pub asked: usize,
}

/// A second board, as its own host describes it (gh#195).
///
/// Only the two fields that can collide. A board is not a problem for being
/// somewhere else; it is a problem for polling a source this one also polls,
/// because both boards then see the same issue as `ready` and either can
/// release it.
#[derive(Debug, Clone)]
pub struct PeerBoard {
    /// The device's display name, or its id when it has none.
    pub device: String,
    /// The `owner/repo` slugs under its `[github] repos`.
    pub repos: Vec<String>,
    /// Why its config could not be read as a config, when it could not be —
    /// the peer answered, but with a `routing.toml` that does not parse, so
    /// what it polls is unknown rather than empty.
    pub unparsed: bool,
}

/// Check the environment. Plain stdout — the report is the output.
///
/// `spaces` is this device's space list, and `accounts` its saved agent
/// logins, or `None` when the engine could not be asked. Route checks against
/// a `None` say "not checked" rather than failing every route over one dead
/// engine — the engine check itself is the one that fails loudly. `edge` is
/// the same engine's live edge-connection census (gh#116), and `members` how
/// many people are in the workspace (gh#161) — the fact that turns "a dispatch
/// that names no account spends the box's login" from a tautology into a
/// warning. `runtimes` is the same engine's `ListBoardRuntimes` answer, which
/// since gh#187 says which harnesses could actually start here.
pub fn doctor(
    paths: &Paths,
    engine: &EngineStatus,
    spaces: Option<&[Space]>,
    accounts: Option<&[AgentAccount]>,
    edge: Option<&EdgeHealth>,
    members: Option<usize>,
    runtimes: Option<&[RuntimeOption]>,
) -> Result<Vec<Check>> {
    let mut checks = Vec::new();

    checks.push(Check {
        name: "config dir".into(),
        ok: paths.config_dir.exists(),
        detail: paths.config_dir.display().to_string(),
    });

    let db_ok = Db::open(&paths.db());
    checks.push(Check {
        name: "database".into(),
        ok: db_ok.is_ok(),
        detail: match &db_ok {
            Ok(_) => format!("{} is writable", paths.db().display()),
            Err(e) => format!("{e:#}"),
        },
    });

    // What the attempts have left on the disk (gh#72), in the two halves gh#186
    // separated: the checkouts, and the build output inside them. Beside the
    // database check because it is the same question — what is this box holding
    // — and the answer is read partly out of that database. One walk feeds both
    // lines: it is time-boxed, and paying that budget twice would only make the
    // second half of one measurement disagree with the first.
    let root = crate::config::worktrees_root();
    let usage = gc::usage(&root);
    checks.push(worktrees_check(paths, &usage, &root, db_ok.as_ref().ok()));
    checks.push(build_output_check(paths, &usage, db_ok.as_ref().ok()));

    // The other half of what an attempt leaves behind (gh#139). Beside the
    // checkouts because it is the same question asked of the shelf instead of
    // the disk, and the two retentions are set in the same table.
    checks.push(chats_check(paths, db_ok.as_ref().ok()));

    // …and whether anything this box starts survives long enough to be work
    // (gh#390). Beside the two above because all three read the same table
    // about the same attempts, and first among the board's own questions
    // because it is the one that invalidates the others: a box where no run
    // lives five minutes is a box whose every other check being green is the
    // problem, not the reassurance.
    checks.push(runs_check(db_ok.as_ref().ok(), chrono::Utc::now()));

    let routing = RoutingConfig::load_unvalidated(&paths.routing());

    // The engine hosts the board loop (there is no separate daemon), so
    // "reachable" answers both "can I list spaces" and "is anything polling".
    checks.push(Check {
        name: "engine".into(),
        ok: engine.reachable,
        detail: engine.detail.clone(),
    });

    // Whether the binary printing this report is the one that engine shipped
    // with (gh#156). Directly under the engine check because it is a fact about
    // the pair, and the pair is what was allowed to drift: every release
    // upgraded the engine and left the board CLI wherever it was, so the first
    // symptom was an agent typing a verb that did not exist.
    checks.push(cli_version_check(
        env!("CARGO_PKG_VERSION"),
        engine.version.as_deref(),
        std::env::current_exe().ok().as_deref(),
        &crate::config::data_dir().join("app"),
    ));

    // And whether this box is on the release the edge is handing out (gh#197).
    // Directly under the pair above because it is the third version in the same
    // sentence: the CLI, the engine beside it, and the one an upgrade would
    // fetch.
    checks.push(release_check(
        engine.update.as_ref(),
        chrono::Utc::now().timestamp_millis(),
    ));

    // Reachable from this shell is not the same as reachable from anywhere
    // else (gh#116). Every other check on this box runs over the loopback IPC
    // port, which stays perfectly healthy while the edge sockets are dead —
    // the exact state the box was in for 25 minutes after an edge redeploy,
    // dispatching happily and invisible to every remote viewer.
    checks.push(edge_connections_check(edge));

    // Which harnesses could actually start on this box (gh#187). Beside the
    // engine that answered it, because it is a fact about the box in the same
    // way the routes and the credentials below are — and because it used to be
    // invisible: `runtime_options()` was a constant, so every picker offered
    // every harness on every device and a dispatch to a missing CLI failed
    // after the worktree was cut.
    checks.push(harnesses_check(runtimes));

    // …and whether the box has the memory to run any of them (gh#533). Beside
    // the harness census because it is the same question about the same box
    // asked one layer down: a harness that could start here is not the same as
    // a box with room to start it. One reading feeds all four host lines, for
    // `worktrees_check`'s reason — paying for the measurement twice would only
    // let one half of it disagree with the other.
    //
    // The unit line is here rather than with the engine checks above on purpose:
    // what it is about is not whether the engine is reachable but whether it
    // survives its own children, which is what everything under it is counting.
    let host = crate::pressure::Snapshot::read();
    let floor = routing
        .as_ref()
        .ok()
        .and_then(crate::config::RoutingConfig::min_memory_headroom);
    checks.push(host_memory_check(&host, floor));
    checks.push(swap_check(&host));
    checks.push(load_check(host.load));
    checks.push(oom_kills_check(
        crate::pressure::read_oom_journal(comet_update::service::UNIT, OOM_JOURNAL_DAYS).as_deref(),
        host.oom,
    ));
    checks.push(unit_governance_check(
        comet_update::service::effective_governance().as_ref(),
    ));

    // And whether anything *else* on this account is a board (gh#195). Beside
    // the engine checks because it is the same conversation — the sweep runs
    // over the same IPC connection, one relayed call per device — and because
    // every check under it describes this board as though it were the only one.
    checks.push(board_hosts_check(
        routing.as_ref().ok(),
        engine.peers.as_ref(),
    ));

    // Read before the match takes ownership of `routing`: the instruction-file
    // check runs last, long after `cfg` is gone, and "no block anywhere" reads
    // as broken until you know the routes asked for none (gh#272).
    let instructions_policy = routing.as_ref().ok().map(|cfg| {
        (
            cfg.agent_instructions(None),
            cfg.routes
                .iter()
                .filter(|r| cfg.agent_instructions(Some(r)))
                .count(),
            cfg.routes.len(),
        )
    });

    // Routing is where most misconfiguration lives, so it is checked in detail.
    // Parsing is deliberately separated from validation: a single bad runtime
    // must not hide the problems in every other route.
    match routing {
        Err(e) => checks.push(Check {
            // `{:#}` so the cause chain shows: "reading X: No such file" is
            // actionable, "reading X" alone is not.
            name: "routing.toml".into(),
            ok: false,
            detail: format!("{e:#}"),
        }),
        Ok(cfg) => {
            let valid = cfg.check();
            checks.push(Check {
                name: "routing.toml".into(),
                ok: valid.is_ok(),
                detail: match &valid {
                    Ok(()) => format!("{} route(s)", cfg.routes.len()),
                    Err(e) => format!("{e:#}"),
                },
            });
            // A credential is only optional until repos are configured: GitHub
            // answers 404 (not 401) for a private repo you cannot see, so
            // without this check the first symptom is a mystery 404 in the log.
            let repos = &cfg.github.repos;
            let rest = crate::sources::github::HttpRest::from_paths(paths);
            checks.extend(github_auth_checks(
                paths,
                repos,
                rest.as_ref().map_err(|e| format!("{e:#}")),
            ));

            checks.push(Check {
                name: "github writeback".into(),
                ok: true,
                detail: writeback_detail(&cfg.github),
            });

            checks.push(Check {
                name: "review delivery".into(),
                ok: true,
                detail: if cfg.github.deliver_reviews {
                    "on — a review on a pull request is queued into the chat that \
                     wrote it, while that chat is still alive on the attempt's checkout"
                        .into()
                } else {
                    "off — a review reaches nobody until you go and tell the agent \
                     (`[github] deliver_reviews = true` to enable)"
                        .into()
                },
            });

            checks.push(Check {
                name: "settle notice".into(),
                ok: true,
                detail: settle_notice_detail(&cfg.defaults),
            });

            // The counterpart to the settle notice, and the one gh#71 existed
            // for: an attempt that blocks settles nothing, so no outcome
            // comment fires and the row is the only trace. This says what the
            // board does about that, per repo, because writeback is per repo.
            checks.push(Check {
                name: "blocked notice".into(),
                ok: true,
                detail: blocked_notice_detail(&cfg.github),
            });

            checks.push(operator_notice_check(&cfg.defaults));

            // Whose subscription the dispatches on this box spend, and what the
            // board says about it (gh#101) — then the same question asked of
            // the dispatches that name nobody at all (gh#161).
            checks.push(billing_guard_check(&cfg));
            checks.push(default_account_check(&cfg, accounts, members));

            // What the board prices that spend at, and how old those rates are
            // (gh#182). Reported beside the billing checks because it is the
            // same subject seen from the other end — who pays, then how much —
            // and it names the table's date rather than implying freshness.
            checks.push(rates_check(&cfg, &today_local()));
            checks.push(subscriptions_check(&cfg, accounts));

            // The pin (gh#104). Reported next to the other notice lines because
            // it is another answer to the same question — who hears about a
            // settle — and because an unpinned board is the one state where
            // nobody hears about work an operator released by hand.
            checks.push(orchestrator_check(&cfg.defaults, db_ok.as_ref().ok()));

            // Whose name a teammate's dispatch commits under (gh#107). Beside
            // the box's own identity below, and always printed for the same
            // reason the duration cap is: with no map every dispatch commits as
            // the box, which looks exactly like a map that is working.
            checks.push(dispatch_authorship_check(&cfg, accounts));

            // And whose name a verdict carries (gh#369) — the other end of the
            // same pull request. Asked of the credential rather than of the
            // config, because who opens a dispatched pull request is decided by
            // whatever the agent pushes with, and only GitHub can put a name to
            // a personal access token.
            checks.push(review_identity_check(
                &cfg,
                &Credentials::load(paths),
                opener(paths, rest.as_ref().ok()),
            ));

            for repo in repos {
                // The same client for every repo, deliberately: under an App
                // that shares one token cache, so a box watching six repos
                // behind one installation mints once here rather than six times.
                let reachable = rest.as_ref().ok().map(|r| {
                    use crate::sources::github::Rest;
                    r.get(&format!("/repos/{repo}"))
                });
                // Which issues this repo actually contributes, and which key
                // decided that. `labels = []` means every open issue, and a
                // repo whose backlog is its roadmap will fill the board with
                // it — so the answer in force is worth stating rather than
                // leaving to be worked out from two places in the file.
                let filter = match (
                    cfg.github.settings_for(repo).is_some(),
                    cfg.github.labels_for(repo),
                ) {
                    (true, []) => " · its own filter: every open issue".to_string(),
                    (true, l) => format!(" · its own filter: labels {}", l.join(" + ")),
                    (false, []) => " · [github] labels = []: every open issue".to_string(),
                    (false, l) => format!(" · [github] labels: {}", l.join(" + ")),
                };
                // And whether the board writes here, on the same line as the
                // repo it is about, so the summary above has a per-repo answer
                // to be checked against.
                let writes = match (
                    cfg.github
                        .settings_for(repo)
                        .is_some_and(|r| r.writeback.is_some()),
                    cfg.github.writeback_for(repo),
                ) {
                    (true, true) => " · writeback: on, its own",
                    (true, false) => " · read-only, its own",
                    (false, true) => " · writeback: on, from [github]",
                    (false, false) => " · read-only, from [github]",
                };
                let filter = format!("{filter}{writes}");
                let (ok, detail) = match reachable {
                    Some(Ok(_)) => (true, format!("reachable{filter}")),
                    Some(Err(e)) if e.to_string().contains("404") => (
                        false,
                        "404 — either it does not exist, or the credential cannot \
                         see it (under an App: it is not installed here)"
                            .to_string(),
                    ),
                    Some(Err(e)) => (false, format!("{e}")),
                    None => (false, "could not build an HTTP client".to_string()),
                };
                checks.push(Check {
                    name: format!("github {repo}"),
                    ok,
                    detail,
                });
            }

            // After the repo loop on purpose: those calls are what mint the
            // installation tokens, so this reports what the board is actually
            // holding rather than what it would hold if it ever asked.
            checks.extend(app_token_checks(repos, rest.as_ref().ok()));
            // A token that can reach a repo — and even write its ordinary
            // contents — can still be refused when a ref update contains
            // `.github/workflows/**`. Report the two grants independently so
            // the box is repaired before a completed CI task reaches push.
            checks.extend(push_capability_checks(repos, rest.as_ref().ok()));

            // A repo whose space exists but whose config does not is silent,
            // not broken — `ok` stays true, or a repo you are only reading
            // would make the whole report exit non-zero.
            checks.push(Check {
                name: "unadopted repos".into(),
                ok: true,
                detail: crate::adopt::doctor_detail(&cfg, spaces),
            });

            for r in &cfg.routes {
                let name = r.display_name().to_string();

                // Read before the space check as well as after it: whether the
                // checkout is there is half of what repairs a missing space
                // (gh#342), and asking the disk twice could answer twice.
                let repo = r.repo_path();
                let repo_ok = repo.join(".git").exists();

                checks.push(match spaces {
                    // Route checks must not fail nineteen times over one dead
                    // engine; the engine check above is the loud one.
                    None => Check {
                        name: format!("route {name}: space"),
                        ok: true,
                        detail: format!(
                            "`{}` not checked — the engine is not reachable",
                            r.workspace
                        ),
                    },
                    Some(spaces) => {
                        let ws_ok = spaces
                            .iter()
                            .any(|s| s.display_name().eq_ignore_ascii_case(&r.workspace));
                        Check {
                            name: format!("route {name}: space"),
                            ok: ws_ok,
                            detail: if ws_ok {
                                format!("`{}` exists", r.workspace)
                            } else {
                                // The state, then the repair — the way every
                                // other failing line here reads (gh#342). A
                                // route with no space is a row nothing can be
                                // dispatched on, and it used to be reported
                                // with no hint that one verb fixes it.
                                format!(
                                    "no comet space named `{}` ({}) — {}",
                                    r.workspace,
                                    have_phrase(spaces),
                                    crate::onboard::missing_space_repair(
                                        r,
                                        repo_ok,
                                        &crate::config::clone_root(),
                                        |p| crate::adopt::git_remote(&p.to_string_lossy()),
                                    )
                                )
                            },
                        }
                    }
                });

                checks.push(Check {
                    name: format!("route {name}: repo"),
                    ok: repo_ok,
                    detail: if repo_ok {
                        repo.display().to_string()
                    } else {
                        format!("{} is not a git repo", repo.display())
                    },
                });

                // Where this route's dispatches branch from (gh#67). Local
                // only: doctor asks whether the repo has the remote the base
                // names, not whether the network is up — a fetch here would
                // hang the report on every unreachable remote, and dispatch
                // refuses loudly on its own when the fetch fails.
                if repo_ok {
                    checks.push(base_check(&name, cfg.base(r), &repo));
                }

                let harness = harness_for_runtime(&r.runtime).map(harness_name);
                checks.push(Check {
                    name: format!("route {name}: runtime"),
                    ok: harness.is_some(),
                    detail: match harness {
                        Some(h) if h == r.runtime => format!("`{}`", r.runtime),
                        Some(h) => format!("`{}` → comet harness `{h}`", r.runtime),
                        None => format!("`{}` is not a comet harness", r.runtime),
                    },
                });

                // Always reported, unlike `account` below, because here it is
                // the *absence* that is the risk: `off` and "never configured"
                // look identical on the board, and one of them means an agent
                // on this route runs until somebody looks (gh#70).
                let cap = cfg.max_duration_secs(Some(r));
                checks.push(Check {
                    name: format!("route {name}: duration cap"),
                    ok: true,
                    detail: match (cap, r.max_duration.is_some()) {
                        (Some(secs), true) => {
                            format!("{} per attempt", crate::overrun::human_secs(secs as i64))
                        }
                        (Some(secs), false) => format!(
                            "{} per attempt (from [defaults])",
                            crate::overrun::human_secs(secs as i64)
                        ),
                        // Not a failure — turning the cap off is a choice, and
                        // it was every board's behaviour before gh#70. Named
                        // out loud so it stays a choice.
                        (None, _) => "off — attempts here run until somebody cancels them".into(),
                    },
                });

                // And the same argument for the *turn* guardrails (gh#270),
                // which catch what the wall clock cannot: `off` and "never
                // heard of it" look identical from the board, and one of them
                // means a run on this route can fail at full speed for as long
                // as the cap above allows.
                let limits = cfg.turn_limits(Some(r));
                let inherited = r.max_tool_failures.is_none() && r.max_tool_calls.is_none();
                checks.push(Check {
                    name: format!("route {name}: turn guardrails"),
                    ok: true,
                    detail: match (limits.tool_failures, limits.tool_calls) {
                        (None, None) => {
                            "off — a run here can retry a failing call for as long as its \
                             duration cap allows"
                                .into()
                        }
                        (failures, calls) => {
                            let mut parts = Vec::new();
                            if let Some(n) = failures {
                                parts.push(format!("{n} failures in a row"));
                            }
                            if let Some(n) = calls {
                                parts.push(format!("{n} tool calls per turn"));
                            }
                            format!(
                                "{}{}",
                                parts.join(", "),
                                if inherited { " (from [defaults])" } else { "" }
                            )
                        }
                    },
                });

                // Only when the route names one: a board on one person's
                // laptop has no accounts to check and should not be told about
                // a feature it is not using (gh#59).
                if let Some(account) = r.account.as_deref().filter(|a| !a.is_empty()) {
                    checks.push(account_check(&name, account, r, accounts));
                }
            }
        }
    }

    checks.push(dispatched_push_check(paths, chrono::Utc::now()));
    checks.push(agent_path_check(
        &crate::config::data_dir().join("app"),
        git_credentials::agent_bin_dir().as_deref(),
    ));
    checks.push(git_identity_check(&git_identity::box_identity(
        &paths.config_dir,
    )));

    // What the agents on this machine have been taught about the board
    // (gh#133). Last because it is about the harness rather than the board,
    // and because the fix is one command.
    checks.push(agent_skill_check(
        &skill::user_config_dir(),
        &crate::config::agent_account_dirs(),
    ));
    // And what they are handed without asking for it (gh#272) — the same
    // question one file over, for the channel Codex has and skills are not.
    checks.push(agent_instructions_check(
        &instruction_dirs(&crate::config::agent_account_dirs()),
        instructions_policy,
    ));
    // And the one *tool* a dispatch can ask an agent for that this box may not
    // have (gh#287). Beside the two above because it is the same question again
    // — what can an agent here actually do — asked of the machine rather than
    // of a config dir.
    checks.push(gh_stack_check(
        gh_extensions(),
        db_ok
            .as_ref()
            .ok()
            .and_then(|db| db.stacked_task_count().ok())
            .unwrap_or(0),
    ));

    Ok(checks)
}

/// The spaces this device does have, for a route that names one it does not.
///
/// An empty list is spelled out rather than left as the empty half of `have: `
/// (gh#342), which is what the report printed on a box holding no spaces at all
/// — a different diagnosis from "the wrong one exists" and worth reading as one:
/// nothing here is a workspace yet.
fn have_phrase(spaces: &[Space]) -> String {
    if spaces.is_empty() {
        return "this device has no spaces at all".to_string();
    }
    format!(
        "have: {}",
        spaces
            .iter()
            .map(Space::display_name)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Can a dispatch on this box ask for a stack (gh#287)?
///
/// `gh stack` is a `gh` *extension*, and it installs into the XDG data dir
/// (`~/.local/share/gh/extensions/`) — box-level and shared by every slot,
/// never per-run: `GH_CONFIG_DIR`, which is the only gh path the engine
/// relocates, does not move it. So this is a property of the machine, and the
/// first box that has never had the one command run on it is where a
/// `dispatch --stack` finds out, halfway through a task, with `unknown command
/// "stack" for "gh"`.
///
/// **It never fails the report.** An absent extension is not a broken board:
/// the brief tells the agent how to repair it in place, and gh#324 measured
/// that `gh extension install` succeeds through the board's own `gh` shim on the
/// minted installation token, needing no login of anybody's. A FAIL here would
/// be a FAIL an operator is right to ignore, which is the expensive kind. This
/// line is so that nobody has to discover any of that from a dead run.
///
/// What gh#335 changed is *which sentence* it says when the extension is
/// missing. `routing.toml` has no stacking flag — the ask is per dispatch
/// (`--stack`), and the flag is not kept on the attempt — so "a route with
/// stacking enabled" is not a state this board can be in, and the only durable
/// evidence that this box stacks is its own history: `stacked` is how many rows
/// hold a pull request GitHub says is in a stack ([`Db::stacked_task_count`]).
/// Zero of them and the missing extension is a fact about what one flag would do
/// here; some of them and it is a box already doing the thing it lacks the tool
/// for — the next `--stack` run spends its opening minutes installing tooling
/// inside a billed, capped attempt, and the operator cannot run `gh stack view`
/// on the stacks the board is already holding. Same non-failure, different news.
///
/// `extensions` is what `gh extension list` printed, or `None` when `gh` could
/// not be run at all — which is worth saying plainly rather than reporting as
/// "not installed", the same distinction gh#155 drew for an unreachable device.
///
/// [`Db::stacked_task_count`]: crate::db::Db::stacked_task_count
fn gh_stack_check(extensions: Option<String>, stacked: usize) -> Check {
    let name = "gh stack".to_string();
    let detail = match &extensions {
        Some(list) if list.contains("gh-stack") => {
            "installed — `dispatch --stack` can ask an agent for layered pull requests".to_string()
        }
        Some(_) if stacked > 0 => format!(
            "not installed, and this board holds {stacked} stacked pull request{} — \
             `gh extension install github/gh-stack` (box-level, one command). Until then \
             the next `dispatch --stack` installs it mid-run on the board's own credential, \
             and `gh stack view` here cannot read the stacks the board already has",
            if stacked == 1 { "" } else { "s" }
        ),
        Some(_) => "not installed — `gh extension install github/gh-stack` (box-level, one \
                    command); until then a `dispatch --stack` agent installs it itself on \
                    the board's own credential"
            .to_string(),
        None => "not checked — `gh` could not be run from this shell".to_string(),
    };
    Check {
        name,
        ok: true,
        detail,
    }
}

/// What `gh extension list` says, or `None` when `gh` is not here to ask.
fn gh_extensions() -> Option<String> {
    let out = std::process::Command::new("gh")
        .args(["extension", "list"])
        .output()
        .ok()?;
    // A box with no extensions at all exits non-zero on some versions and
    // prints nothing on others; both are "gh answered, and it has none".
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Is the board CLI the one the engine beside it shipped with (gh#156)?
///
/// The bug this exists for was invisible by construction. The release payload
/// carried the engine alone, so `install.sh` upgraded `comet` on every release
/// and never touched `comet-board`; the team box ran a source build symlinked by
/// hand in August, three weeks of verbs behind the board it was driving, and
/// nothing said so until an agent typed `onboard` and got "unknown subcommand".
/// Every other check here asks about the environment the CLI can see. This one
/// asks about the CLI.
///
/// Two candidate answers for "the engine's version", in order: the version the
/// engine reported over IPC — authoritative, it is the process actually running
/// the board — and, when it could not be asked, the version of the payload
/// installed on this disk. Neither available is "not checked": a laptop with no
/// managed install and no engine up has nothing to be wrong about.
///
/// `cli` is this crate's `CARGO_PKG_VERSION`, which is the CLI's: one workspace
/// version, and the binary links this crate.
fn cli_version_check(
    cli: &str,
    running: Option<&str>,
    exe: Option<&Path>,
    app_root: &Path,
) -> Check {
    let name = "cli version".to_string();
    // Which copy on this box answered. Stated whether or not there is drift:
    // when there is, it is most of what to do about it; when there is not, it is
    // the only line in the report that says which binary is talking.
    let here = exe
        .map(|p| format!(" · {}", crate::config::shorten_home(p)))
        .unwrap_or_default();

    let installed = installed_payload_version(app_root);
    let (engine, source) = match (running, installed.as_deref()) {
        (Some(v), _) => (v, "the engine running here"),
        (None, Some(v)) => (v, "the release installed here"),
        (None, None) => {
            return Check {
                name,
                ok: true,
                detail: format!("v{cli}{here} — no engine to compare against"),
            };
        }
    };

    if engine == cli {
        return Check {
            name,
            ok: true,
            detail: format!("v{cli}, same as {source}{here}"),
        };
    }

    // Deliberately not "this is not from a release": a copy installed from an
    // older tarball is from a release, just not this one. What is provable from
    // the version mismatch alone is that whatever installed the engine did not
    // put this binary down, and that is what gets said.
    let answering = match exe {
        Some(p) => format!(
            " The binary answering is {}, which is not what that release installed.",
            crate::config::shorten_home(p)
        ),
        None => String::new(),
    };
    // A payload with no `comet-board` in it predates this fix, and re-running
    // the installer against it would relink nothing. Say that rather than
    // handing over a command that cannot work yet.
    let fix = if app_root.join("current").exists() && !app_root.join("current/comet-board").exists()
    {
        format!(
            "the release at {} ships the engine alone, so there is no matching \
             comet-board to link yet — upgrade to one that carries both",
            crate::config::shorten_home(&app_root.join("current"))
        )
    } else {
        "re-run the installer to put them back in step: \
         curl -fsSL https://edge.comet.offhand.dev/install.sh | sh"
            .to_string()
    };
    // The fix goes last so the line ends on something copy-pasteable.
    Check {
        name,
        ok: false,
        detail: format!(
            "v{cli}, but {source} is v{engine} — a verb this CLI does not have does \
             not exist on this box.{answering} {fix}"
        ),
    }
}

/// The version of the managed payload installed on this box, read off the
/// `app/current` symlink (`~/.comet-native/app/<version>`).
///
/// The link target's name, not a file inside it: that name *is* the version the
/// installer staged, and reading it costs no process spawn. Anything that does
/// not start with a digit is not one of our versioned dirs — a `current` a human
/// pointed somewhere else is no answer at all, and a wrong answer here would
/// invent drift that is not there.
fn installed_payload_version(app_root: &Path) -> Option<String> {
    let target = std::fs::read_link(app_root.join("current")).ok()?;
    let name = target.file_name()?.to_str()?;
    name.starts_with(|c: char| c.is_ascii_digit())
        .then(|| name.to_string())
}

/// Is this box on the release the edge is actually handing out (gh#197)?
///
/// The failure it exists for is a publish that half-landed. Releasing is two
/// halves — assets onto the GitHub release, artifacts into the bucket the edge
/// serves — and cutting v0.3.5 the second half died on a 502 from
/// api.cloudflare.com. What that left was a release page with all four assets, a
/// tag, and `latest.txt` still naming 0.3.4: `install.sh` reads `latest.txt`, so
/// a box upgrading in that window is handed the *previous* release and reports
/// success. It was caught because somebody checked the version by hand.
///
/// So the fact worth printing is the one nothing on a box could see: what the
/// edge would give you if you upgraded right now, beside what you are running.
/// Behind is printed and not failed — that is a box mid-window between a release
/// and its next check, and failing it would go red on every machine for hours
/// after every release.
///
/// The one state that fails is the edge serving something *older* than this box
/// runs. That is not a box anybody forgot to upgrade; it is the install surface
/// pointing backwards, and re-running the installer here — or on any box that
/// has never installed at all — fetches the older release and calls it done.
///
/// `now_ms` is passed rather than read so the age line is testable.
fn release_check(update: Option<&comet_update::UpdateStatus>, now_ms: i64) -> Check {
    let name = "release".to_string();
    let not_checked = |detail: &str| Check {
        name: name.clone(),
        ok: true,
        detail: detail.to_string(),
    };
    let Some(update) = update else {
        return not_checked("not checked — the engine did not answer");
    };
    let here = &update.current_version;
    let Some(latest) = update.latest_version.as_deref() else {
        // A just-booted engine has not run its first check yet; one that has and
        // failed carries the reason, which is worth more than "unknown".
        return not_checked(&match &update.error {
            Some(err) => format!(
                "not checked — v{here} here, and the engine's last look at the \
                 edge failed ({err})"
            ),
            None => format!("not checked yet — v{here} here, the engine has not looked yet"),
        });
    };
    // How stale the answer is. An engine checks every 6h, so a report quoting a
    // version without saying when it learned it is quoting something that could
    // predate the release being asked about.
    let age = match update.checked_at {
        Some(at) => format!(
            " · last checked {} ago",
            crate::overrun::human_secs((now_ms - at) / 1000)
        ),
        None => String::new(),
    };

    if latest == here {
        return Check {
            name,
            ok: true,
            detail: format!("v{here} — the version the edge is handing out{age}"),
        };
    }
    if comet_update::version_newer(latest, here) {
        return Check {
            name,
            ok: true,
            detail: format!(
                "v{here} here, but the edge serves v{latest} — this box is behind. \
                 `comet update`, or re-run the installer: \
                 curl -fsSL https://edge.comet.offhand.dev/install.sh | sh{age}"
            ),
        };
    }
    if comet_update::version_newer(here, latest) {
        return Check {
            name,
            ok: false,
            detail: format!(
                "v{here} here, but the edge serves v{latest} — the install surface \
                 points backwards. Anything that upgrades from it now gets v{latest} \
                 and reports success, which is what a release that failed halfway \
                 through publishing looks like from here. Check the release run for \
                 v{here} and re-run its failed job{age}"
            ),
        };
    }
    // Neither newer nor equal: one of the two does not parse as a version — a
    // truncated or garbage `latest.txt` is exactly the sort of thing a
    // half-finished publish leaves, and the updater silently ignores it (an
    // unparseable version is never "newer"), so this is the only place it shows.
    Check {
        name,
        ok: false,
        detail: format!(
            "v{here} here, and the edge serves `{latest}`, which is not a version \
             this can compare against — nothing will ever update from it{age}"
        ),
    }
}

/// Does the skill the agents read match the binary whose flags it documents
/// (gh#133)?
///
/// Two populations, and only one of them is anybody's job. `user_dir` is the
/// machine's own Claude config dir, written by `comet-board skill install` and
/// by nothing else — a stale copy there stays stale until somebody re-installs,
/// so that is the state worth failing on. The slot dirs are re-stamped by
/// `AgentAccounts::materialize` on every dispatch, so a stale one repairs
/// itself the next time it is used: reported, never failed, or every doctor run
/// after a version bump would go red over a file the next dispatch fixes.
fn agent_skill_check(user_dir: &Path, slot_dirs: &[PathBuf]) -> Check {
    let name = "agent skill".to_string();
    let fix = "run `comet-board skill install`";
    let stale_slots = slot_dirs
        .iter()
        .filter(|d| !skill::status_of(d).is_current())
        .count();
    // Only ever an addition to the detail: see the doc comment.
    let slots = match (slot_dirs.len(), stale_slots) {
        (0, _) => String::new(),
        (total, 0) => format!(" · {total} agent-account slot(s), all current"),
        (total, stale) => format!(
            " · {stale} of {total} agent-account slot(s) behind — the next dispatch \
             re-stamps them"
        ),
    };
    let path = skill::path_in(user_dir);
    match skill::status_of(user_dir) {
        skill::State::Current => Check {
            name,
            ok: true,
            detail: format!("{} is v{}{slots}", path.display(), skill::VERSION),
        },
        skill::State::Missing => Check {
            name,
            ok: false,
            detail: format!(
                "not installed — agents here do not know the board's conventions \
                 ({fix}, writes {}){slots}",
                path.display()
            ),
        },
        skill::State::Stale { version } => Check {
            name,
            ok: false,
            detail: format!(
                "{} is {}, this binary ships v{} — {fix}{slots}",
                path.display(),
                match &version {
                    Some(v) => format!("v{v}"),
                    None => "not the shipped text".into(),
                },
                skill::VERSION
            ),
        },
        skill::State::Unreadable(e) => Check {
            name,
            ok: false,
            detail: format!("{}: {e}{slots}", path.display()),
        },
    }
}

/// Every config dir on this box that a dispatch would write an instruction file
/// into (gh#272): the two CLIs' own dirs, plus each materialized account slot.
///
/// A slot dir is named by its id and nothing else, so which of the two files
/// belongs in it is read off the credential the CLI left there
/// ([`conventions::slot_harness`]); a slot that has never been materialized is
/// skipped rather than guessed at, which is also what keeps a Codex slot from
/// being reported as a Claude dir missing its `CLAUDE.md`.
///
/// A CLI's own dir counts only once it exists — a box with no `codex` installed
/// would otherwise carry a permanent "one file without a block" for a runtime it
/// cannot dispatch to at all.
fn instruction_dirs(slot_dirs: &[PathBuf]) -> Vec<(HarnessId, PathBuf)> {
    let mut dirs: Vec<(HarnessId, PathBuf)> = [HarnessId::ClaudeCode, HarnessId::Codex]
        .into_iter()
        .filter_map(|h| conventions::user_config_dir(h).map(|d| (h, d)))
        .filter(|(_, d)| d.is_dir())
        .collect();
    dirs.extend(
        slot_dirs
            .iter()
            .filter_map(|d| conventions::slot_harness(d).map(|h| (h, d.clone()))),
    );
    dirs
}

/// What the agents on this box are told about the board *without asking*
/// (gh#272) — the marker-managed block in each runtime's instruction file.
///
/// Reported, and failed on exactly one thing: a file this cannot safely touch.
/// A missing block is not a fault — the next dispatch on a route that wants one
/// writes it, and on a board that has turned them off there should be none — and
/// a stale one is not either, for [`agent_skill_check`]'s reason: it repairs
/// itself the next time anything dispatches through that dir. What does not
/// repair itself is a file whose markers are broken or which cannot be read,
/// because the writer refuses both rather than guess where a block ends.
fn agent_instructions_check(
    dirs: &[(HarnessId, PathBuf)],
    policy: Option<(bool, usize, usize)>,
) -> Check {
    let name = "agent instructions".to_string();
    let policy = match policy {
        None => String::new(),
        Some((_, on, total)) if total > 0 => format!(", on for {on} of {total} route(s)"),
        Some((default_on, _, _)) => {
            format!(", {} by default", if default_on { "on" } else { "off" })
        }
    };
    let (mut current, mut stale, mut absent) = (0usize, 0usize, 0usize);
    let mut trouble: Vec<String> = Vec::new();
    for (harness, dir) in dirs {
        let path = match conventions::path_in(dir, *harness) {
            Some(p) => p,
            None => continue,
        };
        match conventions::status_of(dir, *harness) {
            conventions::State::Current => current += 1,
            conventions::State::Stale { .. } => stale += 1,
            conventions::State::Absent => absent += 1,
            conventions::State::Malformed => trouble.push(format!(
                "{} has one conventions marker without the other — repair or remove them \
                 by hand; nothing will write over it",
                path.display()
            )),
            conventions::State::Unreadable(e) => trouble.push(format!("{}: {e}", path.display())),
        }
    }
    if !trouble.is_empty() {
        return Check {
            name,
            ok: false,
            detail: trouble.join(" · "),
        };
    }
    let counted = current + stale + absent;
    Check {
        name,
        ok: true,
        detail: format!(
            "{current} of {counted} instruction file(s) carry v{}{}{policy}",
            conventions::VERSION,
            match (stale, absent) {
                (0, 0) => String::new(),
                (0, a) => format!(" ({a} without a block)"),
                (s, 0) => format!(" ({s} behind — the next dispatch rewrites them)"),
                (s, a) =>
                    format!(" ({s} behind — the next dispatch rewrites them; {a} without a block)"),
            },
        ),
    }
}

/// Which edge connections the engine on this box actually holds (gh#116).
///
/// Fails on two states, both of which are "this engine is not reachable and
/// does not know it".
///
/// [`EdgeHealth::dark`] — wired to an edge, holding none of it — is the state
/// gh#116 could not see. [`EdgeHealth::churning`] is the state gh#527 could
/// not see, and it is the more dangerous of the two precisely because every
/// other reading is green: the rooms keep JOINING, so a point-in-time census
/// counts them live, and they keep dying, so nothing typed on a phone is ever
/// answered. On 2026-08-19 this check printed "10 of 10 live" and passed for
/// an entire evening while the fleet was in a dial/die/redial loop.
///
/// A room or two down is still reported and not failed: a client that has just
/// dropped is already redialing and doctor must not cry wolf at every edge
/// deploy. An engine that could not be asked is not failed either, since the
/// `engine` check above already says so, loudly and once.
fn edge_connections_check(edge: Option<&EdgeHealth>) -> Check {
    let Some(edge) = edge else {
        return Check {
            name: "edge connections".into(),
            ok: true,
            detail: "not checked — the engine did not answer".into(),
        };
    };
    let mut detail = edge.summary();
    if edge.dark() {
        detail.push_str(
            ". A remote viewer sees no board on this device. Recovery is automatic within \
             a few minutes; if this persists, restart the engine (`comet daemon restart`)",
        );
    }
    if edge.churning() {
        detail.push_str(
            ". Rooms that answer a join and then die are the edge failing MID-SESSION, not \
             refusing: check the edge's own account of it (a room's `/stats` now reports \
             which sockets vanished and whether it aborted itself), and check whether the \
             Workers plan's duration cap is the thing killing them",
        );
    }
    Check {
        name: "edge connections".into(),
        ok: !edge.dark() && !edge.churning(),
        detail,
    }
}

/// Which harnesses can actually start on this box, and why the rest cannot
/// (gh#187).
///
/// `doctor` already reports the routes, the credentials and the skill; "which
/// agents could I even spin up here" is the same kind of fact and was the one
/// nothing said out loud. It is the shell's copy of what the pickers now show,
/// read from the same `ListBoardRuntimes` answer so the two cannot disagree.
///
/// Failing rather than warning when *nothing* can run: a box with no harness is
/// a board that can poll, derive, and never dispatch — and that is the state
/// this check exists to catch before an operator releases a task into it. One
/// missing runtime out of four is a choice, not a fault, so it prints and
/// passes.
///
/// `mock` is left out of the census entirely. It is dispatchable on purpose and
/// always available, so counting it would let a box with no real harness report
/// "1 ready" and pass.
fn harnesses_check(runtimes: Option<&[RuntimeOption]>) -> Check {
    let name = "harnesses".to_string();
    let Some(runtimes) = runtimes else {
        return Check {
            name,
            ok: true,
            detail: "not checked — the engine did not answer".into(),
        };
    };
    let real: Vec<&RuntimeOption> = runtimes
        .iter()
        .filter(|r| r.harness != comet_proto::HarnessId::Mock)
        .collect();
    if real.is_empty() {
        // An engine old enough to answer without the availability field, or one
        // whose catalog is empty. Either way there is nothing to report on.
        return Check {
            name,
            ok: true,
            detail: "not checked — the engine listed no runtimes".into(),
        };
    }
    let ready: Vec<&str> = real
        .iter()
        .filter(|r| r.available())
        .map(|r| r.name.as_str())
        .collect();
    let blocked: Vec<String> = real
        .iter()
        .filter_map(|r| Some(format!("{} ({})", r.name, r.unavailable?.reason())))
        .collect();
    let detail = match (ready.is_empty(), blocked.is_empty()) {
        (true, _) => format!(
            "none can start here — {}. Nothing this board dispatches will run",
            blocked.join(", ")
        ),
        (false, true) => format!("{} ready", ready.join(", ")),
        (false, false) => format!("{} ready · {}", ready.join(", "), blocked.join(", ")),
    };
    Check {
        name,
        ok: !ready.is_empty(),
        detail,
    }
}

/// Is this the only board on the account, and if not, does the other one poll
/// anything this one does (gh#195)?
///
/// The failure it exists for has no symptom until it has a bad one. Two boards
/// existed on this account for months — the Mac's, one route, and the box's,
/// seventeen — each with its own `board.db`, each polling GitHub, neither aware
/// the other was there. Nothing was wrong, and nothing was enforcing that:
/// `[github] repos` is per-board config, so the day one slug appears in both
/// lists both boards see the same issue as `ready` and `dispatchable`, and
/// either can release it. Two agents, two worktrees, two branches on one ticket
/// — and each board's row looks perfectly normal, because the other's attempt is
/// invisible to it.
///
/// So the fact worth printing is the one no single board could see: who else is
/// polling, and what. A second board is *reported and not failed* — two boards
/// over disjoint repos is a legitimate setup, and the design's "one host device"
/// is about where the store lives, not about how many may exist. What fails is
/// an actual collision: a repo on both lists, which is the race itself rather
/// than the shape that permits it.
///
/// `local` is `None` when this board's own `routing.toml` does not parse — the
/// peers are still named, since who else is out there is worth knowing even
/// then, but nothing is compared against a config that could not be read.
fn board_hosts_check(local: Option<&RoutingConfig>, peers: Option<&Peers>) -> Check {
    let name = "board hosts".to_string();
    let Some(peers) = peers else {
        return Check {
            name,
            ok: true,
            detail: "not checked — the engine did not answer, so no other device was asked".into(),
        };
    };
    // Said whenever it applies, on every branch below: a sweep that skipped a
    // device is a sweep whose silence proves nothing (gh#155), and "no other
    // board" from an incomplete sweep is exactly the reassuring sentence this
    // check must never print by accident.
    let unreachable = match peers.unreachable.as_slice() {
        [] => String::new(),
        names => format!(
            " · could not ask {} — a board there would be invisible from here",
            names.join(", ")
        ),
    };

    if peers.boards.is_empty() {
        let detail = match peers.asked {
            0 => "this device is the only one registered — nothing else on this account \
                  can be polling"
                .to_string(),
            n => format!(
                "the only board among {n} device(s) on this account — the rest host none{unreachable}"
            ),
        };
        return Check {
            name,
            ok: true,
            detail,
        };
    }

    // What each of them polls, named rather than counted: the operator's next
    // move is to take a slug out of one of two files, and that needs the slug.
    let census = peers
        .boards
        .iter()
        .map(|b| format!("{} ({})", b.device, polls(b)))
        .collect::<Vec<_>>()
        .join(" · ");
    let Some(local) = local else {
        return Check {
            name,
            ok: true,
            detail: format!(
                "{} other board(s) on this account: {census}. Nothing compared against \
                 this one — its routing.toml did not parse{unreachable}",
                peers.boards.len()
            ),
        };
    };

    let clashes = overlaps(local, &peers.boards);
    if clashes.is_empty() {
        return Check {
            name,
            ok: true,
            detail: format!(
                "{} other board(s) on this account: {census} — nothing they poll is \
                 also polled here, which is the only thing keeping them out of each \
                 other's work{unreachable}",
                peers.boards.len()
            ),
        };
    }
    Check {
        name,
        ok: false,
        detail: format!(
            "{} — polled by this board and by the other. Both see the same issue as \
             ready, either can dispatch it, and neither records the other's attempt: \
             two agents, two worktrees, two branches on one ticket. Take it off one of \
             the two boards' `[github] repos`{unreachable}",
            clashes.join(" · ")
        ),
    }
}

/// One peer's [`PeerBoard`], from its `ReadBoardConfig` reply (gh#195).
///
/// A reply whose `config` is absent is a board whose `routing.toml` does not
/// parse — reported as unknown rather than as empty, because "polls nothing"
/// would rule out a collision this cannot see.
///
/// Here rather than beside the sweep that reads it, because there are now two
/// sweeps: `doctor`'s, from the CLI, and the one the board's host runs before
/// it writes a repo into its own config (gh#343). They have to read a peer's
/// answer the same way, or the check that refuses and the check that reports
/// would be able to disagree about what the same board polls.
pub fn peer_board(device: String, reply: &serde_json::Value) -> PeerBoard {
    let config = reply
        .get("routing")
        .and_then(|r| r.get("config"))
        .and_then(|c| serde_json::from_value::<RoutingConfig>(c.clone()).ok());
    match config {
        Some(cfg) => PeerBoard {
            device,
            repos: cfg.github.repos.clone(),
            unparsed: false,
        },
        None => PeerBoard {
            device,
            repos: Vec::new(),
            unparsed: true,
        },
    }
}

/// Which other board on this account already polls `slug` — the refusal
/// `onboard` and `routes add` owe before the config is written (gh#343).
///
/// [`board_hosts_check`] is the same comparison, and it runs too late to help:
/// the repo is already in both files by the time anybody runs `doctor`, and
/// what it has cost by then is a duplicate attempt — two agents, two branches,
/// one ticket, each board's row looking perfectly normal. Asked at write time
/// instead, the same fact costs one flag.
///
/// `Some(_)` is the whole sentence to refuse with, naming the board and what to
/// do about it. It says `--force` out loud because two boards polling one repo
/// *is* a legitimate choice on a board where nobody dispatches, and because the
/// settings page onboards through the same refusal with no flag to type: what
/// it can offer a reader there is the other half of the sentence, which is
/// taking the slug off one of the two boards.
///
/// What does **not** refuse, deliberately: a peer that could not be asked, and
/// a peer whose own `routing.toml` does not parse. Both are "unknown", not
/// "collides", and refusing on either would block every add on this account for
/// as long as somebody's laptop is shut or somebody's config is broken —
/// failure modes with no bound on how long they last. `doctor` still names both
/// afterwards, which is the surface that can afford to be uncertain out loud.
pub fn already_polled(slug: &str, boards: &[PeerBoard]) -> Option<String> {
    let want = slug.trim().to_ascii_lowercase();
    // Case-folded for the same reason `overlaps` is: GitHub reads `Tally` and
    // `tally` as one repo, and a comparison that did not would wave through the
    // collision on exactly the day somebody retyped a slug from memory.
    let hosts: Vec<&str> = boards
        .iter()
        .filter(|b| {
            b.repos
                .iter()
                .any(|r| r.trim().to_ascii_lowercase() == want)
        })
        .map(|b| b.device.as_str())
        .collect();
    if hosts.is_empty() {
        return None;
    }
    Some(format!(
        "{slug} is already polled by the board on {} — both boards would see the same \
         issue as ready, either could dispatch it, and neither would record the other's \
         attempt: two agents, two worktrees, two branches on one ticket. Take it off \
         that board's `[github] repos`, or pass --force if this board is meant to share \
         the repo (nobody dispatching from one of the two makes it safe)",
        hosts.join(", ")
    ))
}

/// What one peer board polls, in one clause. `unparsed` is its own sentence
/// because "polls nothing" and "would not say" are different answers, and the
/// second is the one where a collision cannot be ruled out.
fn polls(board: &PeerBoard) -> String {
    if board.unparsed {
        return "its routing.toml does not parse — what it polls is unknown".into();
    }
    if board.repos.is_empty() {
        return "polls nothing".into();
    }
    board.repos.join(", ")
}

/// Every repo both this board and a peer poll, as sentences naming the peer.
///
/// Case-insensitively: GitHub treats `Tally` and `tally` as one repo and so
/// does its API, so a comparison that did not would miss the collision on
/// exactly the day somebody retyped a slug from memory — and the point of this
/// check is to catch the one nobody noticed.
fn overlaps(local: &RoutingConfig, boards: &[PeerBoard]) -> Vec<String> {
    let fold = |s: &String| s.trim().to_ascii_lowercase();
    let repos: Vec<String> = local.github.repos.iter().map(fold).collect();
    let mut out = Vec::new();
    for board in boards {
        for repo in &board.repos {
            if repos.contains(&fold(repo)) {
                out.push(format!("{repo} is on {} too", board.device));
            }
        }
    }
    out
}

/// Does this box have a git identity, and will GitHub attribute what it signs
/// (gh#107)?
///
/// The failure it exists for is a box that has none: git does not stop at a
/// missing `user.email`, it invents one from the box user and hostname (or the
/// harness's own setup writes one), and the first dispatched agent commits
/// under an address belonging to no account. Nothing rejects the commit — it
/// just attributes to nobody, and a contributor gate on the deploy side
/// (Vercel's) refuses to build the push. That is the one state worth failing:
/// an anonymous box is not a preference, it is a box nobody has finished
/// setting up.
///
/// A configured address that is not GitHub's noreply form is `ok` with
/// guidance, never a failure, because this check *cannot know*: whether an
/// address is on somebody's account is answered by `GET /user/emails`, which is
/// a user-scoped call the board's App may not make. Failing every board whose
/// operator uses their real (verified, perfectly attributable) work address
/// would be the gh#96 false alarm again.
fn git_identity_check(id: &git_identity::BoxIdentity) -> Check {
    let name = "git identity".to_string();
    let (Some(who), Some(email)) = (&id.name, &id.email) else {
        return Check {
            name,
            ok: false,
            detail: format!(
                "not configured ({}) — git invents an author from the box user and \
                 hostname, so dispatched agents commit as nobody and deploy gates that \
                 check the author refuse the push. \
                 `git config --global user.name \"…\"` and `git config --global \
                 user.email \"<id>+<login>{}\"` (from https://github.com/settings/emails)",
                match (&id.name, &id.email) {
                    (None, None) => "no user.name, no user.email",
                    (None, Some(_)) => "no user.name",
                    _ => "no user.email",
                },
                git_identity::NOREPLY_SUFFIX
            ),
        };
    };
    if git_identity::is_github_noreply(email) {
        return Check {
            name,
            ok: true,
            detail: format!(
                "{who} <{email}> — GitHub attributes what this box commits to @{}",
                git_identity::noreply_login(email).unwrap_or("that account")
            ),
        };
    }
    Check {
        name,
        ok: true,
        detail: format!(
            "{who} <{email}> — not a GitHub noreply address. Attribution works only if \
             this address is on the account's verified list, which nothing here can \
             check (the board's App cannot read anybody's emails); if it is not, \
             commits attribute to nobody and deploy gates that check the author refuse \
             the push. `<id>+<login>{}` always works",
            git_identity::NOREPLY_SUFFIX
        ),
    }
}

/// Whose name a *teammate's* dispatch commits under (gh#107).
///
/// The box's identity answers for the operator; it does not answer for #66's
/// teammate driving the same board from a laptop. Their attempt records who
/// they are (gh#74) and, with an address for that person here, the harness
/// child is stamped with it — so their commits read as theirs on GitHub and
/// pass the same contributor gate the box's own do.
///
/// Always printed, never a failure. No map is the single-operator default and
/// is exactly right on a box only one person dispatches from; what it must not
/// do is stay invisible, because "everything lands as the box" and "the map is
/// working" look identical on GitHub until somebody reads the commit list.
///
/// `accounts` is the box's saved CLI logins, which are here for the pairing
/// gh#162 named: a mapped teammate with no login of their own commits as
/// themselves and spends somebody else's subscription. The two facts live in
/// different places, which is exactly why nothing had put them on one line.
/// `None` — the engine could not be asked — says nothing about the pairing
/// rather than reporting every teammate as missing one.
fn dispatch_authorship_check(cfg: &RoutingConfig, accounts: Option<&[AgentAccount]>) -> Check {
    let name = "dispatch authorship".to_string();
    if cfg.users.is_empty() {
        return Check {
            name,
            ok: true,
            detail: "no `[users]` map — every dispatch commits under this box's own git \
                     identity, whoever released it. `comet-board member add \
                     <their-sign-in-email> --github <login>` maps a teammate so their \
                     work lands as theirs; docs/teammate.md is the rest of the sequence"
                .into(),
        };
    }
    // Named, with what each one resolves to, because the mapping is the part
    // that is silently wrong: an address for the wrong account still commits,
    // still pushes, and attributes to somebody else entirely.
    let roster = crate::members::roster(cfg, accounts);
    let mut entries = Vec::new();
    let mut unresolved = Vec::new();
    let mut unlinked = Vec::new();
    let mut slotless = Vec::new();
    for m in &roster.members {
        match &m.author {
            Some(author) => {
                if !m.noreply {
                    unlinked.push(m.user.clone());
                }
                if roster.accounts_known && m.needs_account() {
                    slotless.push(m.user.clone());
                }
                entries.push(format!("{} → {} <{}>", m.user, author.name, author.email));
            }
            None => unresolved.push(format!("{} = \"{}\"", m.user, m.value)),
        }
    }
    let mut sentences = Vec::new();
    if !entries.is_empty() {
        sentences.push(entries.join(" · "));
    }
    if !unlinked.is_empty() {
        sentences.push(format!(
            "Not a GitHub noreply address for {} — attribution depends on the address \
             being on that account, which nothing here can check",
            unlinked.join(", ")
        ));
    }
    if !unresolved.is_empty() {
        sentences.push(format!(
            "Not an address at all: {} — those dispatches commit as the box",
            unresolved.join(", ")
        ));
    }
    // The other half of onboarding, on the line that already names the person
    // (gh#162). Mapped and slotless is not a config error — one shared
    // subscription is a real arrangement — but it is the half nobody thinks
    // about, and it is invisible until a usage page says so.
    if !slotless.is_empty() {
        sentences.push(format!(
            "No agent account of their own for {} — their runs spend whichever \
             subscription the route names, or this box's own (see Agent accounts, \
             docs/teammate.md)",
            slotless.join(", ")
        ));
    }
    let detail = sentences.join(". ");
    Check {
        name,
        // The unparseable entries are `routing.toml`'s own failure (they are in
        // `problems()`, so the `routing.toml` check above is already red); this
        // line names them rather than failing twice for one mistake.
        ok: true,
        detail,
    }
}

/// Who opens a dispatched pull request on this box (gh#369).
///
/// Not a preference — a consequence. The dispatched agent pushes and runs `gh
/// pr create` through the board's credential path ([`crate::git_credentials`]),
/// so whichever credential that path hands over is the author of every pull
/// request the board produces.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Opener {
    /// The board's GitHub App. A bot: nobody reviews as it, which is exactly
    /// what the invariant wants on this side.
    App,
    /// A personal access token, and the account GitHub says it belongs to.
    /// `None` when GitHub could not be asked — an unnamed person is still a
    /// person, and the check says so rather than clearing the box.
    Person(Option<String>),
    /// No board credential at all: the agent falls back to whatever git
    /// credentials the box user has, which is a person nobody here can name.
    BoxUser,
}

/// Put a name to the credential that opens pull requests, asking GitHub only
/// when there is a token to ask about.
///
/// An App is not asked: `GET /user` under its JWT is a refusal, and under an
/// installation token it names the App — neither is a person, which is the only
/// thing this needs to know.
fn opener(paths: &Paths, rest: Option<&crate::sources::github::HttpRest>) -> Opener {
    match Credentials::load(paths).github_auth() {
        GithubAuth::App { .. } => Opener::App,
        GithubAuth::None => Opener::BoxUser,
        GithubAuth::Token(_) => {
            Opener::Person(rest.and_then(|r| crate::sources::github::Github::new(r).viewer().ok()))
        }
    }
}

/// Whether a verdict this board casts can be a verdict at all (gh#369).
///
/// GitHub refuses `APPROVE` and `REQUEST_CHANGES` on a pull request the caller
/// opened. Both halves of avoiding that are box configuration, and neither is
/// visible from anywhere else:
///
/// - the identity that **opens** a dispatched pull request should be a bot, so
///   no human ever reviews as it;
/// - the identity that **casts** a verdict should be the reviewer's own, which
///   is a `GITHUB_USER_TOKEN_<LOGIN>` beside their `[users]` entry.
///
/// `ok` is false for the collision only — one identity on both sides, which is
/// the state where an approval can never be more than a comment saying it
/// approves. A board with no member tokens at all is not failing: it is the
/// gh#365 arrangement, working as designed, and the line says what it costs.
fn review_identity_check(cfg: &RoutingConfig, credentials: &Credentials, opener: Opener) -> Check {
    let name = "review identity".to_string();
    let opens = match &opener {
        Opener::App => "the board's App opens dispatched pull requests — a bot, which is \
                        what lets a person's verdict on one be a verdict"
            .to_string(),
        Opener::Person(Some(login)) => {
            format!("dispatched pull requests are opened by @{login} (GITHUB_TOKEN)")
        }
        Opener::Person(None) => "dispatched pull requests are opened by whoever owns \
                                 GITHUB_TOKEN — GitHub could not be asked which account \
                                 that is"
            .to_string(),
        Opener::BoxUser => "no board credential, so dispatched agents push and open pull \
                            requests with this box's own git credentials — whoever that is"
            .to_string(),
    };
    // Who can cast a verdict under their own name, and who reviews as the
    // board. A member whose entry names no GitHub account has no login to key a
    // token on, which `dispatch authorship` above already reports as the
    // weaker mapping it is.
    let mut casts = Vec::new();
    let mut as_board = Vec::new();
    let mut collides = Vec::new();
    for user in cfg.users.keys() {
        let Some(login) = cfg.github_login_for(user) else {
            continue;
        };
        match credentials.user_token(&login) {
            Some(token) => {
                casts.push(format!("@{login}"));
                // The same account both sides, reached the other way: the
                // board's own token *is* this person's. GitHub reads one
                // account, whichever variable it arrived in.
                if credentials.github_token.as_deref() == Some(token) {
                    collides.push(format!(
                        "@{login}'s review token is the board's own GITHUB_TOKEN"
                    ));
                }
            }
            None => as_board.push(format!("@{login}")),
        }
        if let Opener::Person(Some(opener)) = &opener
            && opener.eq_ignore_ascii_case(&login)
        {
            collides.push(format!(
                "@{login} both opens and reviews — GitHub refuses an approval on your own \
                 pull request, so theirs can only ever arrive as a comment. Register a \
                 GitHub App (GITHUB_APP_ID / GITHUB_APP_PRIVATE_KEY_PATH) so the bot \
                 opens them"
            ));
        }
    }
    let mut sentences = vec![opens];
    if !casts.is_empty() {
        sentences.push(format!(
            "{} casts verdicts under their own name",
            casts.join(", ")
        ));
    }
    if !as_board.is_empty() {
        sentences.push(format!(
            "{} reviews as the board — an approval arrives as a comment saying it \
             approves (gh#365) until {} is set",
            as_board.join(", "),
            crate::config::user_token_env(as_board[0].trim_start_matches('@')),
        ));
    }
    if cfg.users.is_empty() {
        sentences.push(
            "no `[users]` map, so every verdict is the board's. `comet-board member add \
             <their-sign-in-email> --github <login>` names a reviewer; their token goes \
             in the board's .env as GITHUB_USER_TOKEN_<LOGIN>"
                .into(),
        );
    }
    sentences.extend(collides.clone());
    Check {
        name,
        ok: collides.is_empty(),
        detail: sentences.join(". "),
    }
}

/// What the attempts have left on the disk, and what will reclaim it (gh#72).
///
/// The check exists because the failure it reports is invisible until it is
/// terminal: every dispatch cuts a full checkout plus a branch, nothing removed
/// either before gh#72, and the first symptom on a busy box is a disk with no
/// space left in the middle of somebody's run. Three numbers answer it — how
/// many checkouts, how much disk, and how many of them the board is still
/// holding open (a live attempt, a pull request in review, an issue still
/// owed).
///
/// `ok` is false only when the *checkouts* are genuinely large
/// ([`gc::WARN_BYTES`] against [`gc::Usage::checkout_bytes`], or
/// [`gc::WARN_CHECKOUTS`]); a board holding a week of checkouts on purpose is
/// working exactly as configured and must not fail the report for it. The
/// retention window is named either way, because `off` is a choice whose cost
/// is this line. Build output is [`build_output_check`]'s — it is the bulk of
/// the bytes and it answers to a different key, so it gets its own verdict
/// rather than turning this one red on a box that is merely busy (gh#186).
fn worktrees_check(
    paths: &Paths,
    usage: &gc::Usage,
    root: &std::path::Path,
    db: Option<&Db>,
) -> Check {
    let floor = if usage.truncated { "≥ " } else { "" };
    // The split gh#186 asked for. `109.5 GiB in worktrees` was true and useless:
    // it hid that 99.96% of the number was regenerable, and it named
    // `retain_worktrees` — which governs the other 0.04% — as the thing to
    // change. The total stays, because the disk is the total.
    let about = format!(
        "{} checkout(s), {floor}{} in {} ({floor}{} of checkout, {floor}{} of build \
         output — see below)",
        usage.checkouts,
        gc::human_bytes(usage.bytes),
        root.display(),
        gc::human_bytes(usage.checkout_bytes()),
        gc::human_bytes(usage.cache_bytes),
    );
    // What the board still has a claim on. Not the same as "on disk": a
    // checkout cut by comet itself, or one left by an attempt whose row has
    // been reaped, is on the disk and in nobody's records.
    let held = db.and_then(|db| db.collectable_attempts().ok()).map(|a| {
        let marked = a.iter().filter(|a| a.collectable_at.is_some()).count();
        format!(
            "{} tracked by the board, {marked} on the retention clock",
            a.len()
        )
    });
    let retention = match RoutingConfig::load_unvalidated(&paths.routing()) {
        Ok(cfg) => match cfg.retain_worktrees_secs() {
            Some(secs) => format!(
                "collected {} after their task leaves the board",
                gc::human_window(secs)
            ),
            None => "retain_worktrees = off — nothing here is ever collected".to_string(),
        },
        // A routing.toml that will not parse is the loud check above; here it
        // only means the window cannot be quoted.
        Err(_) => "retention unknown — routing.toml did not parse".to_string(),
    };
    let detail = [Some(about), held, Some(retention)]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    Check {
        name: "worktrees".into(),
        ok: !usage.alarming(),
        detail,
    }
}

/// What the build output inside those checkouts weighs, and what will sweep it
/// (gh#186).
///
/// Its own line because it is its own thing on its own clock: a checkout is 14 MB
/// of evidence and its `target/` is 20–36 GB of cache, and the one number the box
/// reported before this ("109.5 GiB in worktrees") named neither. The share is
/// what makes the sentence useful — 99.96% regenerable is a different problem
/// from 109 GiB of source.
///
/// Red in exactly one state: a lot of build output and nothing that will ever
/// sweep it. A busy box mid-build has tens of gibibytes of `target/` and is
/// working correctly, so size alone must not fail the report — that is how the
/// line this replaces stopped meaning anything. `retain_build_output = off` with
/// 20 GiB behind it is the gh#186 failure itself, and it is worth an exit code.
fn build_output_check(paths: &Paths, usage: &gc::Usage, db: Option<&Db>) -> Check {
    let floor = if usage.truncated { "≥ " } else { "" };
    let about = format!(
        "{floor}{} in {} build-output director{} ({})",
        gc::human_bytes(usage.cache_bytes),
        usage.cache_dirs,
        if usage.cache_dirs == 1 { "y" } else { "ies" },
        gc::BUILD_OUTPUT_DIRS.join(", "),
    );
    // Of the checkouts the board still tracks, how many could it sweep — and how
    // many has it already. A swept row still holds its checkout (that is the
    // point of the split), so this is not the same census the line above reports.
    let held = db.and_then(|db| db.collectable_attempts().ok()).map(|a| {
        let swept = a.iter().filter(|a| a.cache_swept_at.is_some()).count();
        format!(
            "{} checkout(s) tracked by the board, {swept} already swept",
            a.len()
        )
    });
    let window = match RoutingConfig::load_unvalidated(&paths.routing()) {
        Ok(cfg) => cfg.retain_build_output_secs(),
        Err(_) => return unparsed_retention_check("build output"),
    };
    let retention = match window {
        // No window is its own sentence, as it is for chats: "swept 0 seconds
        // after" is a number where the operator wants the rule.
        Some(0) => "swept as each attempt ends".to_string(),
        Some(secs) => format!("swept {} after each attempt ends", gc::human_window(secs)),
        None => "retain_build_output = off — kept for as long as the checkout is".to_string(),
    };
    Check {
        name: "build output".into(),
        ok: window.is_some() || usage.cache_bytes < gc::WARN_BYTES,
        detail: [Some(about), held, Some(retention)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · "),
    }
}

/// The one thing a retention line can say when `routing.toml` will not parse:
/// the window is unquotable. Not a failure of its own — the `routing.toml` check
/// above is already red, and failing twice for one mistake reads as two.
fn unparsed_retention_check(name: &str) -> Check {
    Check {
        name: name.into(),
        ok: true,
        detail: "retention unknown — routing.toml did not parse".into(),
    }
}

/// What the attempts have left on the space shelves, and what will clear it
/// (gh#139).
///
/// The mirror of [`worktrees_check`] on the other half of a dispatch's
/// leavings. Nothing here is measured off the disk: a chat costs a row, not a
/// gigabyte, and the cost this reports is attention — the six chats somebody is
/// working in, lost among two hundred finished ones.
///
/// Never `ok: false`. A board keeping every chat forever is a board configured
/// to, which is why the window is named either way and `off` is worded as the
/// choice it is rather than as a fault.
fn chats_check(paths: &Paths, db: Option<&Db>) -> Check {
    let held = db
        .and_then(|db| db.archivable_chat_attempts().ok())
        .map(|attempts| {
            let marked = attempts
                .iter()
                .filter(|a| a.chat_archivable_at.is_some())
                .count();
            format!(
                "{} board chat(s) still on their shelves, {marked} on the archive clock",
                attempts.len()
            )
        });
    let retention = match RoutingConfig::load_unvalidated(&paths.routing()) {
        Ok(cfg) => {
            // The board-wide window, plus a count of the routes that answer
            // differently — quoting one number for a per-route setting would be
            // quoting it at whoever reads it about the wrong route.
            let overrides = cfg
                .routes
                .iter()
                .filter(|r| r.archive_chats.is_some())
                .count();
            let window = match cfg.archive_chats_secs(None) {
                // No window is its own sentence: "archived 0 seconds after" is
                // a number where the operator wants the rule.
                Some(0) => "archived as their task leaves the board".to_string(),
                Some(secs) => format!(
                    "archived {} after their task leaves the board",
                    gc::human_window(secs)
                ),
                None => "archive_chats = off — chats stay on the shelf forever".to_string(),
            };
            match overrides {
                0 => window,
                n => format!("{window} ({n} route(s) set their own)"),
            }
        }
        // Same as the worktree check's: a routing.toml that will not parse is
        // the loud check above, and here it only means the window is unquotable.
        Err(_) => "retention unknown — routing.toml did not parse".to_string(),
    };
    Check {
        name: "chats".into(),
        ok: true,
        detail: [held, Some(retention)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · "),
    }
}

/// Does anything this box starts actually run (gh#390)?
///
/// The check that was missing on the morning of gh#390. Twelve attempts across
/// three harnesses died within minutes of starting, one after another, and
/// `doctor` reported every check ok — because every check was about
/// *configuration*, and the configuration was fine. Nothing asked the one
/// question an operator has when the board keeps handing back empty attempts:
/// is this box still able to run anything at all?
///
/// It reads the board's own history rather than probing, because probing would
/// answer a different question — a run doctor starts is not a run a dispatch
/// starts, with its account, its sandbox and its worktree — and because the
/// history is free. The rule and the thresholds are [`crate::runs::health`];
/// this is the reading and the sentence.
///
/// A `FAIL` here means: stop dispatching to this box and look at the engine
/// log. It is deliberately hard to trip — one attempt finishing anywhere in the
/// window clears it — because an operator who learns to ignore this line has
/// lost the only check that would have caught the night gh#390 describes.
fn runs_check(db: Option<&Db>, now: chrono::DateTime<chrono::Utc>) -> Check {
    let Some(db) = db else {
        return Check {
            name: "runs".into(),
            ok: true,
            detail: "not checked — the board database could not be opened".into(),
        };
    };
    let since = crate::db::rfc3339(now - chrono::Duration::seconds(crate::runs::WINDOW_SECS));
    let attempts = match db.attempts_since(&since) {
        Ok(a) => a,
        Err(e) => {
            return Check {
                name: "runs".into(),
                ok: true,
                detail: format!("not checked — reading recent attempts: {e:#}"),
            };
        }
    };
    let samples: Vec<crate::runs::Sample> = attempts
        .iter()
        // A live attempt is evidence that a run started, not yet evidence about
        // how it ends. Counting one as a young death would fail the box for
        // every dispatch made in the last five minutes.
        .filter_map(|a| {
            let ended = a.ended_at.as_deref()?;
            Some(crate::runs::Sample {
                lived_secs: span_secs(&a.started_at, ended).unwrap_or(0),
                finished: a.outcome == Some(crate::model::Outcome::Done),
            })
        })
        .collect();
    let window = crate::runs::WINDOW_SECS / 3600;
    match crate::runs::health(&samples) {
        crate::runs::Health::Quiet => Check {
            name: "runs".into(),
            ok: true,
            detail: format!("no attempt has finished in the last {window}h — nothing to judge"),
        },
        crate::runs::Health::Healthy { ran, young } => Check {
            name: "runs".into(),
            ok: true,
            detail: format!(
                "{ran} attempt(s) ended in the last {window}h, {young} of them within {} of \
                 starting — runs are starting on this box",
                gc::human_window(crate::runs::YOUNG_SECS as u64),
            ),
        },
        crate::runs::Health::Dying { ran, young } => Check {
            name: "runs".into(),
            ok: false,
            detail: format!(
                "all {young} of the last {ran} attempt(s) died within {} of starting and none \
                 finished — runs are not surviving on this box. Dispatching more work here \
                 will burn attempts; check the engine log for the runs' own errors before \
                 releasing anything else",
                gc::human_window(crate::runs::YOUNG_SECS as u64),
            ),
        },
    }
}

// ---- what the box has left, and what it has already killed (gh#533) --------

/// How far back the oom-kill question is asked, in days.
///
/// A week, which is [`crate::runs`]'s reasoning at a longer scale: a box that
/// OOM-killed four agents on Tuesday is still a box with too little memory on
/// Friday, and one that did it last month has since been changed or has not
/// been busy. The counters cannot answer this at all — they reset with the unit,
/// and the first thing anybody does to a box that keeps dying is restart it.
const OOM_JOURNAL_DAYS: u32 = 7;

/// Sustained load per core past which the box is oversubscribed enough to say
/// so. Two: a build box at 1.5× cores on the fifteen-minute average is working,
/// and one at 2× has a queue that is not draining.
const LOAD_PER_CORE_WARN: f64 = 2.0;

/// What the box has left, and what the dispatch gate makes of it (gh#533).
///
/// Never red. A box that is momentarily tight is a box that is *working* — the
/// same rule the build-output check follows, and for the same reason: a line
/// that goes red every time three agents are building stops being read. What it
/// is for is the question an operator asks after a deferred dispatch — "why is
/// nothing being released" — and the answer is the reading, the floor, and
/// which of the two the box is on the wrong side of.
fn host_memory_check(snap: &crate::pressure::Snapshot, floor: Option<f64>) -> Check {
    let Some(mem) = snap.memory else {
        return Check {
            name: "host memory".into(),
            ok: true,
            detail: "not measurable on this platform — no dispatch is ever held for memory here"
                .into(),
        };
    };
    let mut parts = vec![format!(
        "{} of {} available ({:.0}%)",
        crate::pressure::bytes(mem.available),
        crate::pressure::bytes(mem.total),
        mem.available_share() * 100.0,
    )];
    if let Some(psi) = snap.psi {
        parts.push(format!(
            "{:.1}% of the last 10s stalled on memory (PSI some)",
            psi.some_avg10
        ));
    }
    parts.push(match floor {
        None => "min_memory_headroom = off — dispatch does not look at memory".to_string(),
        Some(floor) => match crate::pressure::headroom(snap, floor) {
            crate::pressure::Headroom::Tight(reason) => {
                format!("a dispatch now would defer: {reason}")
            }
            _ => format!("floor {:.0}% — there is room to dispatch", floor * 100.0),
        },
    });
    Check {
        name: "host memory".into(),
        ok: true,
        detail: parts.join(" · "),
    }
}

/// Whether the box has any slack at all between "busy" and "the kernel picks a
/// victim" (gh#533).
///
/// A warning and not a failure: plenty of boxes run swapless deliberately, and
/// a board that refused to be green on one would be telling its operator to
/// change something they decided. What it is not allowed to be is *silent* —
/// the box on 2026-08-19 had 15.6 GiB, no swap and three heavy builds, and
/// swaplessness is the reason the kernel's only available response was a kill.
fn swap_check(snap: &crate::pressure::Snapshot) -> Check {
    let Some(mem) = snap.memory else {
        return Check {
            name: "swap".into(),
            ok: true,
            detail: "not checked — this platform does not report swap here".into(),
        };
    };
    if !mem.swapless() {
        return Check {
            name: "swap".into(),
            ok: true,
            detail: format!(
                "{} of swap, {} free",
                crate::pressure::bytes(mem.swap_total),
                crate::pressure::bytes(mem.swap_free)
            ),
        };
    }
    Check {
        name: "swap".into(),
        ok: true,
        detail: format!(
            "warn — no swap on a {} box: the kernel's only answer to a memory spike is to kill \
             something, and agent builds are what it will find. Add some: `sudo fallocate -l 4G \
             /swapfile && sudo chmod 600 /swapfile && sudo mkswap /swapfile && sudo swapon \
             /swapfile && echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab`",
            crate::pressure::bytes(mem.total)
        ),
    }
}

/// Has the kernel been killing things here (gh#533)?
///
/// The one red line of the four, because it is the only one that reports
/// something that has *already happened*. Everything else here is a shape the
/// box is in; this is a list of times work was destroyed, and gh#526 is what it
/// costs when nobody is told: four kills over one night, each read as an agent
/// being flaky.
///
/// Evidence, never a verdict on its own. The journal lines carry their
/// timestamps because "3 oom kills" with no dates is a number an operator
/// cannot act on, and the live counter is named separately because it means
/// something different — kills since this unit last started, which is usually
/// "since the last time somebody restarted it to make the problem go away".
fn oom_kills_check(
    journal: Option<&[crate::pressure::OomJournalEntry]>,
    counters: Option<crate::pressure::OomCounters>,
) -> Check {
    let live = counters.map(|c| c.cgroup).unwrap_or(0);
    let Some(journal) = journal else {
        // Not Linux, no journalctl, or no such unit. The counter may still have
        // something to say; if it does not, nothing was checked.
        return Check {
            name: "oom kills".into(),
            ok: live == 0,
            detail: if live == 0 {
                "not checked — the engine's unit journal could not be read here".into()
            } else {
                format!(
                    "{live} process(es) of this unit have been OOM-killed since it started — \
                     the unit journal could not be read for when. See `comet-board doctor`'s \
                     host memory and swap lines"
                )
            },
        };
    };
    if journal.is_empty() && live == 0 {
        return Check {
            name: "oom kills".into(),
            ok: true,
            detail: format!("none in the last {OOM_JOURNAL_DAYS} days"),
        };
    }
    let latest = journal.last();
    let mut detail = format!(
        "{} oom-kill event(s) in the engine's unit journal in the last {OOM_JOURNAL_DAYS} days",
        journal.len()
    );
    if let Some(entry) = latest {
        detail.push_str(&format!(" — latest {}: {}", entry.at, entry.line));
    }
    if live > 0 {
        detail.push_str(&format!(" · {live} of them since the unit last started"));
    }
    detail.push_str(
        ". The box is running out of memory under its own agents: give it swap, lower \
         `[defaults] max_concurrent_per_workspace`, or raise `min_memory_headroom` so \
         dispatch defers sooner",
    );
    Check {
        name: "oom kills".into(),
        ok: false,
        detail,
    }
}

/// Is the box carrying more than it can run (gh#533)?
///
/// The fifteen-minute average and no other, because the word in the question is
/// *sustained*: one-minute load spikes to twice the cores every time a build
/// links, and a check that fired on that would fire all afternoon. A warning,
/// like swap — a box deliberately oversubscribed is a decision, and load is not
/// what kills a run. It is here because it is the corroborating reading: memory
/// pressure and a load that never comes down are the same three builds seen
/// from two directions.
fn load_check(load: Option<crate::pressure::Load>) -> Check {
    let Some(load) = load else {
        return Check {
            name: "load".into(),
            ok: true,
            detail: "not checked — this platform does not report a load average here".into(),
        };
    };
    let per_core = load.per_core();
    let about = format!(
        "{:.2} {:.2} {:.2} over {} core(s) — {per_core:.1}× per core sustained",
        load.one, load.five, load.fifteen, load.cores
    );
    Check {
        name: "load".into(),
        ok: true,
        detail: if per_core > LOAD_PER_CORE_WARN {
            format!(
                "warn — {about}. Everything on this box is waiting for a turn; agents will \
                 time out on work they would otherwise finish"
            )
        } else {
            about
        },
    }
}

/// Does the engine's own unit still scope an OOM kill to the process the kernel
/// chose (gh#529, gh#533)?
///
/// The check that exists because a fix can ship and not arrive. gh#529's three
/// lines are written when a unit is *rendered* — at install, or by `comet daemon
/// install` — and an engine update rewrites the binary and nothing else, so
/// every box installed before that release kept `OOMPolicy=stop` and kept dying
/// for it. Since gh#533 the lines also ship as a drop-in re-asserted on every
/// update; this is what says whether that reached *this* box, and prints the
/// paste that fixes it when it did not.
///
/// Red, unlike its three neighbours, because the state it names is not a shape
/// the box is in — it is the engine having no protection against the thing the
/// line above it is counting.
fn unit_governance_check(governance: Option<&comet_update::service::Governance>) -> Check {
    let Some(g) = governance else {
        return Check {
            name: "engine unit".into(),
            ok: true,
            detail: "not checked — no systemd user manager to ask here".into(),
        };
    };
    if !g.loaded() {
        return Check {
            name: "engine unit".into(),
            ok: true,
            detail: format!(
                "{} is {} — the engine is not running as a service on this box, so there is no \
                 unit to govern",
                comet_update::service::UNIT,
                g.load_state
            ),
        };
    }
    if g.complete() {
        return Check {
            name: "engine unit".into(),
            ok: true,
            detail: format!(
                "OOMPolicy={}, MemoryHigh={}, MemoryMax={} — an OOM-killed agent is an event, \
                 not an engine death",
                g.oom_policy, g.memory_high, g.memory_max
            ),
        };
    }
    let mut missing = Vec::new();
    if !g.survives_an_oom_kill() {
        missing.push(format!(
            "OOMPolicy={} — one OOM-killed agent child will stop the whole engine, taking every \
             warm session and the pinned dispatcher chat with it",
            if g.oom_policy.is_empty() {
                "stop (systemd's default)"
            } else {
                &g.oom_policy
            }
        ));
    }
    if !g.throttles() {
        missing.push(
            "MemoryHigh is unset — the cgroup never throttles into reclaim before the \
             kernel acts"
                .to_string(),
        );
    }
    if !g.capped() {
        missing.push(
            "MemoryMax is unset — a breach is resolved boxwide rather than inside this unit"
                .to_string(),
        );
    }
    Check {
        name: "engine unit".into(),
        ok: false,
        detail: format!(
            "{}. Fix it with:\n\n{}\n",
            missing.join(" · "),
            comet_update::service::resource_dropin_fix()
        ),
    }
}

/// Seconds between two RFC3339 stamps, or `None` if either will not parse.
fn span_secs(from: &str, to: &str) -> Option<i64> {
    let from = chrono::DateTime::parse_from_rfc3339(from).ok()?;
    let to = chrono::DateTime::parse_from_rfc3339(to).ok()?;
    Some((to - from).num_seconds().max(0))
}

/// Can a dispatched agent on this box push and open a pull request (gh#68)?
///
/// The question this exists for is a headless one: a box with no keychain, no
/// stored https credential and nobody to run `gh auth login` pushes with
/// nothing at all, and finds out at the end of a run. The parts are the
/// credential (checked above, as the board's own), the `comet-board` binary
/// the engine points `GIT_ASKPASS` at, and a `gh` for the wrapper to wrap.
///
/// A missing `gh` is not a failure: `git push` still works, and a PR opened by
/// hand from the branch is a normal way to finish. A missing binary is, because
/// then nothing was handed to the agent at all.
///
/// Since gh#233 the parts are not merely counted, they are **run**. The check
/// that a `comet-board` exists is the check that failed gh#233: everything was
/// installed, everything resolved, and no push on the box could authenticate,
/// because `GIT_ASKPASS` was being handed a string git cannot exec. So this
/// builds the shim a dispatch would build and asks it the one question that
/// needs no credential and no network — which username does a GitHub App push
/// with — in a scratch directory, so a `doctor` run by the wrong user cannot
/// leave a file the engine then cannot rewrite.
///
/// The ledger is read as well as probed, because a failure that happened during
/// somebody's real run is a fact the probe cannot reach (gh#233). But *only*
/// the probe is present tense, which is what gh#515 got wrong: any recorded
/// failure, of any age and any cause, turned the line red. A GitHub outage from
/// two days ago read as "`gh` has dropped off this box", and the healthy
/// present-tense clause sat in front of it where nobody looked.
/// [`push_verdict`] is the rule that replaced it.
fn dispatched_push_check(paths: &Paths, now: chrono::DateTime<chrono::Utc>) -> Check {
    let credential = !matches!(Credentials::load(paths).github_auth(), GithubAuth::None);
    let exe = git_credentials::resolve_board_exe();
    let gh = git_credentials::resolve_gh(None);
    let live = match (&exe, credential) {
        (None, _) => Live::Broken(format!(
            "no comet-board binary found beside the engine or on PATH — agents push with \
             this box's own git credentials (set {})",
            git_credentials::BOARD_EXE_ENV
        )),
        (Some(_), false) => Live::Broken(
            "no GitHub credential — agents push with this box's own git credentials".to_string(),
        ),
        (Some(exe), true) => match askpass_answers(exe) {
            Ok(()) => Live::Working(format!(
                "the askpass helper answers, and mints per push{}",
                match &gh {
                    Some(gh) => format!("; `gh` at {} is wrapped to mint per call", gh.display()),
                    None => "; no `gh` installed, so pull requests are opened by hand".into(),
                }
            )),
            Err(err) => Live::Broken(format!(
                "the credential path does not work — no dispatched agent on this box can \
                 push with the board's App: {err:#}"
            )),
        },
    };
    push_verdict(live, crate::credential_ledger::standing_failure(paths), now)
}

/// What the probe found — the only present-tense fact the check above has.
enum Live {
    /// The shim built, and answered git's username prompt.
    Working(String),
    /// It did not, or there was nothing to build it from.
    Broken(String),
}

/// Turn the probe and the ledger into one line an operator can act on (gh#515).
///
/// Four readings, and the ordering of the sentence follows the reading rather
/// than the order the facts were gathered in:
///
/// - The probe fails: red, and the probe's sentence leads, because it is the
///   one naming a thing to fix. History trails it as context.
/// - The probe passes and the ledger's last standing failure is GitHub's own —
///   a 5xx, a timeout, a rate limit: **not** red at any age. There is nothing
///   on this box to act on and never was; the only move it could ask for is
///   "wait", and red does not mean wait.
/// - The probe passes and a *local* failure is younger than
///   [`crate::credential_ledger::FRESH_SECS`]: still red. The probe is not the
///   whole path — it cannot mint against the API, and it cannot be the run that
///   failed — so a fresh failure it cannot reproduce is exactly the gh#233
///   shape and still leads the sentence.
/// - Anything older, or with a mint after it (which
///   [`crate::credential_ledger::standing_failure`] has already dropped):
///   history, on an `ok` line, phrased in the past tense.
fn push_verdict(
    live: Live,
    standing: Option<crate::credential_ledger::Entry>,
    now: chrono::DateTime<chrono::Utc>,
) -> Check {
    use crate::credential_ledger::Cause;

    let check = |ok: bool, detail: String| Check {
        name: "dispatched pushes".into(),
        ok,
        detail,
    };
    let (live_ok, live_detail) = match live {
        Live::Working(detail) => (true, detail),
        Live::Broken(detail) => (false, detail),
    };
    let Some(entry) = standing else {
        return check(live_ok, live_detail);
    };
    let when = match entry.age_secs(now) {
        Some(secs) => human_age(secs),
        None => format!("at {}", entry.at),
    };
    let what = clip(&entry.what(), 160);

    if !live_ok {
        // The actionable sentence is already first; the failure is context for
        // it, and reads as history because it is dated.
        return check(
            false,
            format!("{live_detail} · last failure {when}: {what}"),
        );
    }
    match entry.cause() {
        Cause::Upstream => check(
            true,
            format!("{live_detail} · history: the last failure was GitHub's own, {when} — {what}"),
        ),
        Cause::Local if entry.is_fresh(now) => check(
            false,
            format!(
                "a dispatched run could not use the credential path {when} — {what}. The live \
                 check cannot reproduce it, so read that run's own log rather than this box's \
                 config · {live_detail}"
            ),
        ),
        // Past the window, and — since `standing_failure` stops at a mint —
        // demonstrably the last thing that went wrong rather than the newest of
        // many. Both halves of "not happening now" are in the ledger.
        Cause::Local => check(
            true,
            format!("{live_detail} · history: last failure {when} and none since — {what}"),
        ),
    }
}

/// How long ago, at the resolution a `doctor` line is read at: `2d ago`,
/// `5h ago`, `12m ago`, `just now`.
///
/// Not [`gc::human_window`], which spells a *configured* window and so refuses
/// to round — an age is never a round number of days, and `179100s ago` is not
/// a sentence.
fn human_age(secs: i64) -> String {
    match secs {
        s if s >= 86_400 => format!("{}d ago", s / 86_400),
        s if s >= 3_600 => format!("{}h ago", s / 3_600),
        s if s >= 60 => format!("{}m ago", s / 60),
        _ => "just now".into(),
    }
}

/// A quoted error, cut to something that fits on a `doctor` row. GitHub's 5xx
/// bodies are prose and the whole of one buries every other check on screen.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", text[..at].trim_end()),
        None => text.to_string(),
    }
}

/// Build the askpass shim in a scratch directory and ask it git's username
/// prompt. Removed afterwards: this is a diagnostic, not an install.
fn askpass_answers(board_exe: &Path) -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join(format!("comet-doctor-askpass-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let result = git_credentials::install_askpass_shim(&dir, board_exe)
        .and_then(|shim| git_credentials::verify_askpass(&shim));
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Can a dispatched agent on this box run `comet-board` at all (gh#184)?
///
/// The question nobody had asked, because the answer looked obvious from a
/// login shell. A dispatched agent's PATH is not a login shell's: the engine
/// runs as a systemd **user** service, which inherits `/usr/local/bin:…:/bin`
/// and nothing else, and `install.sh` links the CLI into `~/.local/bin` —
/// which appears nowhere in it. Every board verb the skill hands an agent was
/// `command not found` on the one machine that runs dispatched agents, and
/// nothing said so: an agent that cannot reach the board does not crash, it
/// simply stops checking `dispatchable`, stops releasing sub-work through the
/// board, stops waiting, and gets on with the ticket alone.
///
/// The fix is that the engine prepends its own app directory (`app/<version>/`,
/// which since gh#156 holds both binaries) to every harness child's PATH. So
/// what this checks is that the directory has a `comet-board` in it — the one
/// way the guarantee can come back apart is a payload that ships the engine
/// alone, which is exactly the state gh#156 was about.
///
/// `resolved` is where *this* CLI resolves from, used when there is no managed
/// install to read. Both answers are about the payload on this disk rather than
/// about the process: an engine somebody started from a build tree prepends
/// that tree instead, and no check here can see it.
fn agent_path_check(app_root: &Path, resolved: Option<&Path>) -> Check {
    let name = "agent PATH".to_string();
    let current = app_root.join("current");
    let (ok, detail) = if current.exists() {
        if current.join("comet-board").exists() {
            (
                true,
                format!(
                    "agents get {} on PATH — the comet-board the engine shipped with",
                    crate::config::shorten_home(&current)
                ),
            )
        } else {
            (
                false,
                format!(
                    "the release at {} ships the engine alone, so the directory an agent \
                     gets on PATH holds no comet-board and every board verb it types is \
                     `command not found` — upgrade to a release that carries both",
                    crate::config::shorten_home(&current)
                ),
            )
        }
    } else {
        match resolved {
            Some(dir) => (
                true,
                format!(
                    "no managed install — agents get {}, where this CLI resolves from",
                    crate::config::shorten_home(dir)
                ),
            ),
            None => (
                false,
                format!(
                    "no comet-board found beside the engine or on PATH — an agent on this \
                     box can run no board verb at all (set {})",
                    git_credentials::BOARD_EXE_ENV
                ),
            ),
        }
    };
    Check { name, ok, detail }
}

/// Which GitHub credential is live, and what it can reach (gh#58).
///
/// Two facts an operator cannot get anywhere else. "GITHUB_TOKEN present" was
/// enough while there was one way to authenticate; with an App in the picture
/// the questions are *which* identity the board writes as and *whose* repos it
/// was granted — and under an App the answer is set by the installer, not by
/// anything in this config directory.
fn github_auth_checks(
    paths: &Paths,
    repos: &[String],
    rest: Result<&crate::sources::github::HttpRest, String>,
) -> Vec<Check> {
    let credentials = Credentials::load(paths);
    let mut checks = Vec::new();

    // Half an App is the failure with no symptom: the board falls back to the
    // token and keeps working, writing as a person rather than as the bot.
    if let Some(missing) = credentials.github_app_half_configured() {
        checks.push(Check {
            name: "github app".into(),
            ok: false,
            detail: format!(
                "half configured — {missing} is not set, so the App is ignored and \
                 the board is running on GITHUB_TOKEN. Set both in {}, or neither",
                paths.env_file().display()
            ),
        });
    }

    match credentials.github_auth() {
        GithubAuth::None => checks.push(Check {
            name: "github auth".into(),
            ok: repos.is_empty(),
            detail: if repos.is_empty() {
                "none — not needed, no repos under [github]".into()
            } else {
                format!(
                    "none, but {} repo(s) are configured — private repos answer 404 \
                     without a credential. Add GITHUB_TOKEN, or a GITHUB_APP_ID and \
                     GITHUB_APP_PRIVATE_KEY_PATH pair, to {}",
                    repos.len(),
                    paths.env_file().display()
                )
            },
        }),
        GithubAuth::Token(_) => checks.push(Check {
            name: "github auth".into(),
            ok: true,
            detail: "GITHUB_TOKEN — a personal access token, so writes are \
                     attributed to whoever owns it"
                .into(),
        }),
        GithubAuth::App { app_id, key_path } => {
            checks.push(key_permissions_check(&key_path));
            // The reason, not just the fact: the usual way to get here is a
            // GITHUB_APP_PRIVATE_KEY_PATH pointing at something that is not the
            // PEM, and "could not be built" alone sends nobody anywhere.
            let (ok, detail) = match rest.map(|r| r.auth().app()) {
                Err(e) => (false, format!("app {app_id} — {e}")),
                Ok(None) => (
                    false,
                    format!("app {app_id} — configured, but the client is not on the App"),
                ),
                Ok(Some(app)) => match (app.app_slug(), app.installations()) {
                    (Err(e), _) | (_, Err(e)) => (false, format!("app {app_id} — {e:#}")),
                    (Ok(slug), Ok(installs)) if installs.is_empty() => (
                        false,
                        format!(
                            "app {app_id} (@{slug}) — registered, but installed nowhere. \
                             Install it on the repos you want polled"
                        ),
                    ),
                    (Ok(slug), Ok(installs)) => (
                        true,
                        format!(
                            "app {app_id} (@{slug}) — {}",
                            installs
                                .iter()
                                .map(|i| format!(
                                    "{} (installation {}, {} repos)",
                                    i.account, i.id, i.selection
                                ))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ),
                },
            };
            checks.push(Check {
                name: "github auth".into(),
                ok,
                detail,
            });
        }
    }
    checks
}

/// The private key is the App. A PEM anyone else on the box can read is a
/// credential anyone else on the box has — which matters more since #55 let
/// several people drive it.
fn key_permissions_check(key_path: &std::path::Path) -> Check {
    let name = "github app key".into();
    let Ok(meta) = std::fs::metadata(key_path) else {
        return Check {
            name,
            ok: false,
            detail: format!("{} cannot be read", key_path.display()),
        };
    };
    // Windows has no mode bits to read; there the check is only that the file
    // is there, which the metadata call above already answered.
    #[cfg(not(unix))]
    let (ok, detail) = {
        let _ = &meta;
        (true, key_path.display().to_string())
    };
    #[cfg(unix)]
    let (ok, detail) = {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        let private = mode & 0o077 == 0;
        (
            private,
            if private {
                format!("{} ({mode:04o})", key_path.display())
            } else {
                format!(
                    "{} is {mode:04o} — readable beyond its owner. `chmod 600` it",
                    key_path.display()
                )
            },
        )
    };
    Check { name, ok, detail }
}

/// What the App is actually holding for each configured repo: the installation
/// serving it, and how long its token has left.
///
/// Empty under a personal access token, which has no installation and no
/// expiry to report.
fn app_token_checks(
    repos: &[String],
    rest: Option<&crate::sources::github::HttpRest>,
) -> Vec<Check> {
    let Some(app) = rest.and_then(|r| r.auth().app()) else {
        return Vec::new();
    };
    repos
        .iter()
        .map(
            |repo| match (app.cached_installation(repo), app.cached_ttl(repo)) {
                (Some(id), Some(ttl)) => Check {
                    name: format!("github {repo} token"),
                    ok: ttl > 0,
                    detail: format!("installation {id} · expires in {}", minutes(ttl)),
                },
                // Reached the repo but never needed a token, or never reached it at
                // all — the per-repo check above already said which.
                _ => Check {
                    name: format!("github {repo} token"),
                    ok: false,
                    detail: "no installation token — the App is not installed on this repo".into(),
                },
            },
        )
        .collect()
}

/// Whether each configured repo can receive ordinary and workflow-file pushes.
///
/// One check per repo because App permissions are installation-specific and a
/// classic token's `public_repo` scope depends on whether that repo is public.
/// The probe returns only permission evidence, never the bearer used to obtain
/// it (gh#440).
fn push_capability_checks(
    repos: &[String],
    rest: Option<&crate::sources::github::HttpRest>,
) -> Vec<Check> {
    repos
        .iter()
        .map(|repo| {
            let capabilities = rest
                .ok_or_else(|| "could not build the configured GitHub client".to_string())
                .and_then(|rest| rest.push_capabilities(repo).map_err(|e| format!("{e:#}")));
            push_capability_check(repo, capabilities)
        })
        .collect()
}

fn push_capability_check(repo: &str, capabilities: Result<PushCapabilities, String>) -> Check {
    let name = format!("github {repo} pushes");
    let Ok(capabilities) = capabilities else {
        return Check {
            name,
            ok: false,
            detail: "repository contents: unknown · workflow files: unknown — the capability \
                     probe failed. Restore GitHub access, then run `comet-board doctor` again"
                .into(),
        };
    };
    let status = format!(
        "repository contents: {} · workflow files: {}",
        capabilities.contents.word(),
        capabilities.workflows.word()
    );
    let remediation = match capabilities.evidence {
        CapabilityEvidence::AppInstallation if capabilities.contents != WriteCapability::Write => {
            " — grant the GitHub App `Contents: Read and write`, then approve the updated permission on this installation"
        }
        CapabilityEvidence::AppInstallation if !capabilities.can_write_workflows() => {
            " — grant the GitHub App `Workflows: Read and write`, then approve the updated permission on this installation"
        }
        CapabilityEvidence::ClassicOauthScopes
            if capabilities.contents != WriteCapability::Write =>
        {
            " — ensure the token holder has push access to this repository, then refresh or replace GITHUB_TOKEN with `repo` (or `public_repo` for a public repository) scope"
        }
        CapabilityEvidence::ClassicOauthScopes if !capabilities.can_write_workflows() => {
            " — refresh or replace GITHUB_TOKEN with the classic `workflow` scope in addition to its repository scope"
        }
        CapabilityEvidence::OpaqueToken => {
            " — GitHub exposed no OAuth-scope evidence (fine-grained/unknown token), so this fails closed; use an App installation reporting Contents + Workflows write, or a classic token reporting `repo` + `workflow`"
        }
        CapabilityEvidence::Anonymous => {
            " — configure GITHUB_TOKEN, or GITHUB_APP_ID with GITHUB_APP_PRIVATE_KEY_PATH"
        }
        CapabilityEvidence::ProbeFailed => {
            " — capability evidence was unavailable; restore GitHub access and run `comet-board doctor` again"
        }
        _ => "",
    };
    Check {
        name,
        ok: capabilities.can_write_workflows(),
        detail: format!("{status}{remediation}"),
    }
}

fn minutes(secs: i64) -> String {
    match secs {
        s if s <= 0 => "already expired".into(),
        s if s < 120 => format!("{s}s"),
        s => format!("{}m", s / 60),
    }
}

/// Can this route's `base` be resolved at all — i.e. does the repo have the
/// `origin` the base is fetched from (gh#67)?
///
/// Local check only. Whether origin *answers* is a network question, and a
/// doctor that fetches every route's remote is a doctor that hangs; dispatch
/// refuses loudly on a failed fetch, which is the case this cannot pre-empt.
/// What it does pre-empt is the config-level mistake: a route pointing at a
/// clone with no remote, where every dispatch fails until somebody sets
/// `base = "HEAD"`.
fn base_check(name: &str, base: &str, repo: &std::path::Path) -> Check {
    let check = |ok: bool, detail: String| Check {
        name: format!("route {name}: base"),
        ok,
        detail,
    };
    if matches!(base.trim(), "" | "HEAD") {
        return check(
            true,
            "`HEAD` — branches from the repo's current checkout, no fetch".into(),
        );
    }
    let origin = std::process::Command::new("git")
        .args(["-C", &repo.to_string_lossy(), "remote", "get-url", "origin"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    match origin {
        Some(url) => check(true, format!("`{base}` — fetched from {url}")),
        None => check(
            false,
            format!(
                "`{base}` needs an `origin` remote; {} has none. Set \
                 `base = \"HEAD\"` on the route to branch from the checkout instead",
                repo.display()
            ),
        ),
    }
}

/// Does a route's `account` name a login this device has saved, for the
/// harness the route dispatches to?
///
/// Both halves matter. An unknown id fails every dispatch on the route at the
/// point the chat would be created; an id belonging to the *other* CLI is the
/// subtler one — it resolves as a saved account and still cannot be handed to
/// this route's harness, because `CLAUDE_CONFIG_DIR` and `CODEX_HOME` are not
/// interchangeable.
fn account_check(
    name: &str,
    account: &str,
    route: &crate::config::Route,
    accounts: Option<&[AgentAccount]>,
) -> Check {
    let check = |ok: bool, detail: String| Check {
        name: format!("route {name}: account"),
        ok,
        detail,
    };
    let Some(accounts) = accounts else {
        return check(
            true,
            format!("`{account}` not checked — the engine is not reachable"),
        );
    };
    let Some(found) = accounts.iter().find(|a| a.id == account) else {
        let known: Vec<String> = accounts
            .iter()
            .map(|a| {
                format!(
                    "{} ({}, {})",
                    a.id,
                    a.email.as_deref().unwrap_or("unknown"),
                    harness_name(a.harness)
                )
            })
            .collect();
        return check(
            false,
            if known.is_empty() {
                format!(
                    "`{account}` is not a saved login — this device has none; sign one                      in under Agent accounts first"
                )
            } else {
                format!(
                    "`{account}` is not a saved login (have: {})",
                    known.join(", ")
                )
            },
        );
    };
    match harness_for_runtime(&route.runtime) {
        // A bad runtime is already its own failing check; not repeating it here.
        None => check(true, format!("`{account}` — runtime unresolved")),
        Some(harness) if harness == found.harness => check(
            true,
            format!(
                "`{account}` — {} ({})",
                found.email.as_deref().unwrap_or("unknown"),
                harness_name(harness)
            ),
        ),
        Some(harness) => check(
            false,
            format!(
                "`{account}` is a {} login, but this route dispatches to {} — an                  account cannot be lent across CLIs",
                harness_name(found.harness),
                harness_name(harness)
            ),
        ),
    }
}

/// Which agent hears that a settle or a block happened.
///
/// The one setting whose failure is invisible: writeback failing leaves a ticket
/// open, an unresolvable review state is reported by name, but a notice that
/// never fires produces nothing at all — no error, no log line, no changed row.
/// It reads as "nobody told me" rather than "the board is misconfigured", which
/// is why it belongs in `doctor` next to the other two.
///
/// Since gh#165 the two agent channels are one chain, so this line answers
/// "who takes this event" rather than reporting a switch. It reads the pin as
/// well as the switch for exactly that reason: `notify_dispatcher = false` on a
/// board with an orchestrator is a routing choice, and on a board without one
/// it is silence — two facts one boolean cannot tell apart.
fn settle_notice_detail(defaults: &crate::config::Defaults) -> String {
    match (defaults.notify_dispatcher, defaults.orchestrator()) {
        (true, Some(chat)) => format!(
            "on — a settle or a block goes to the chat that released the work; when nothing \
             did, or that chat is gone, the orchestrator ({chat}) takes it instead"
        ),
        (true, None) => "on — a settle or a block goes to the chat that released the work. \
                         Work released from the panel or the phone released it from no chat, \
                         and with nothing pinned that reaches no agent at all (see \
                         `orchestrator`)"
            .into(),
        (false, Some(chat)) => format!(
            "off — the chat that released work is never told; every settle and block goes to \
             the orchestrator ({chat}), including a copy of every child it released itself \
             (`[defaults] notify_dispatcher = true` routes them to the dispatcher first)"
        ),
        // Deliberately not "only you are notified": until gh#71 that was what
        // this line said, and there was no channel that notified you either.
        // A `doctor` line describing a notice nobody sends is worse than no
        // line, because it is the answer somebody stops investigating at.
        (false, None) => "off — no agent is told when work settles or blocks; the board row \
                          and the comment on the issue are the whole trail (`[defaults] \
                          notify_dispatcher = true` to enable)"
            .into(),
    }
}

/// Whether one agent is running this board, and whether the chat named as that
/// agent can actually be one (gh#104).
///
/// Unpinned is a preference, not a fault: a board driven by a human at the panel
/// wants no orchestrator, and a `doctor` that exited non-zero over that would
/// stop meaning anything. What the line has to do is say what an unpinned board
/// costs — the three cases that reach nobody — so that "nobody picked this up"
/// is legible as a setting rather than as a bug.
///
/// The fault it does catch is the one misconfiguration the pin allows: pinning
/// a chat the board itself dispatched. That chat is an attempt — it holds a
/// workspace slot, it has a task of its own, and the exemption from
/// `max_duration` now keeps it alive past every cap. None of that is what
/// somebody meant by "run the board", and unpinned it would simply have
/// finished.
fn orchestrator_check(defaults: &crate::config::Defaults, db: Option<&Db>) -> Check {
    let name = "orchestrator".to_string();
    let Some(chat) = defaults.orchestrator() else {
        return Check {
            name,
            ok: true,
            detail: "not pinned — nothing takes what no dispatcher can be told. Work you \
                     release from the panel or the phone, a settle whose dispatching chat \
                     has been archived, and every cap warning reach no agent: they are a row \
                     colour and a comment on the issue, and the log says so once per event \
                     (pin a session in the app, or `[defaults] orchestrator_chat = \
                     \"<chat-id>\"`)"
                .into(),
        };
    };
    // A live attempt on the pinned chat is the real misconfiguration. A closed
    // one is history — the chat outlived its attempt and somebody pinned it
    // afterwards, which is odd but harmless — so only the live case is a fault.
    let dispatched = db
        .and_then(|db| db.live_attempts().ok())
        .map(|attempts| {
            attempts
                .into_iter()
                .find(|a| a.pane_id.as_deref() == Some(chat))
        })
        .unwrap_or(None);
    match dispatched {
        Some(attempt) => Check {
            name,
            ok: false,
            detail: format!(
                "chat {chat} is pinned, but the board dispatched it — it is the live \
                 attempt on {}, so it holds a workspace slot and is now exempt from its \
                 own time cap. Pin a chat you opened yourself",
                attempt.task_id
            ),
        },
        None => Check {
            name,
            ok: true,
            detail: format!(
                "chat {chat} — {}, one message per event. Unpin to stop them",
                if defaults.notify_dispatcher {
                    "the addressee of last resort: work no chat released, work whose \
                     dispatching chat is gone, and every cap warning. A settle its \
                     dispatcher was told about is not repeated here"
                } else {
                    "every settle, block, orphan and cap warning on this board is prompted \
                     into it — `notify_dispatcher` is off, so nothing goes to a dispatcher \
                     first and this chat gets the whole board"
                }
            ),
        },
    }
}

/// What happens upstream when an agent stops and cannot go on (gh#71).
///
/// Gated per repo, which is the same rule every other comment follows — and
/// the reason this is worth a line: a read-only repo gets no comment, so on
/// those repos the board row really is the only signal, and an operator should
/// learn that here rather than by waiting for a comment that is never coming.
fn blocked_notice_detail(github: &crate::config::GithubConfig) -> String {
    let mut s = "on — an agent that stops to ask, or whose run dies, leaves one comment \
                 on its issue per block (where writeback is on)"
        .to_string();
    let reads = github.read_only_repos();
    if !reads.is_empty() {
        s.push_str(&format!(
            ". No comment on the read-only repos, so a block there shows on the board \
             and nowhere else: {}",
            reads.join(", ")
        ));
    }
    s
}

/// Whether anything reaches a human who is looking at neither the board nor
/// the issue tracker (gh#71).
///
/// Two keys answer this, so `doctor` reads them together: `notify_webhook` is
/// the address and `notify` is the mute switch. No address is *not configured*
/// rather than a fault — not wanting an out-of-band channel is a legitimate
/// answer, and a `doctor` that exits non-zero over a preference stops meaning
/// anything. What is a fault is an address that cannot be posted to: the
/// operator asked for the notice, and every one of them is being dropped into
/// a log line.
///
/// What this line must never do is what the settle-notice line did before
/// gh#71 — imply somebody is being told when nobody is.
fn operator_notice_check(defaults: &crate::config::Defaults) -> Check {
    let name = "operator notice".to_string();
    let Some(url) = defaults.notify_webhook.as_deref() else {
        return Check {
            name,
            ok: true,
            detail: "not configured — nothing reaches you out of band. A blocked agent \
                     comments on its issue and colours a row on the board, and at 02:00 \
                     that is all (`[defaults] notify_webhook = \"https://…\"` for a POST \
                     on every block and settle)"
                .into(),
        };
    };
    if let Some(problem) = crate::notify::webhook_url_problem(url) {
        return Check {
            name,
            ok: false,
            detail: format!(
                "`[defaults] notify_webhook` cannot be posted to ({problem}) — every \
                 notice is dropped with a warning in the log and nothing reaches you"
            ),
        };
    }
    Check {
        name,
        ok: true,
        detail: if defaults.notify {
            format!(
                "on — `on_blocked` and `on_settled` are POSTed to {}",
                crate::notify::webhook_host(url)
            )
        } else {
            format!(
                "muted — {} is configured but `[defaults] notify = false` silences it; \
                 a blocked agent still comments on its issue",
                crate::notify::webhook_host(url)
            )
        },
    }
}

/// What the board does about a dispatch that spends somebody else's
/// subscription (gh#101).
///
/// Reported the way the notices are, and worded to the same rule: `off` has to
/// read as a *choice* rather than as something left unconfigured. On a
/// one-person box `off` is the right answer, and a `doctor` that nagged about
/// it would be a `doctor` people stop reading — so this never fails, exactly
/// like [`operator_notice_check`].
///
/// The honesty is not optional either — and gh#161 changed what honest says.
/// A relayed dispatch is compared against the identity the edge verified and
/// the relay stamped on the frame, which no frontend can write; a dispatch
/// issued on the box carries no stamp and is compared against the frontend's
/// claim, which is correct there and worth saying out loud rather than
/// implying. Neither line hedges about the other's case.
/// Today, in the box's own reckoning — the clock the rate table's age is
/// measured against, and the same local day the stats buckets use.
fn today_local() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

/// What the board prices tokens at, and how old that is (gh#182).
///
/// **Reports the date rather than implying freshness.** The rates ship inside
/// the binary — no provider publishes a pricing API, and scraping one for money
/// on a box with no network guarantees fails silently into a number nobody
/// re-checks — so the only honest thing a check can do is say when the snapshot
/// was taken and go `ok: false` once it is old enough to have missed a change.
/// Not a failure of the board: a failure of *this line* to still be trustworthy,
/// which is what a person reading `doctor` needs to know.
fn rates_check(cfg: &crate::config::RoutingConfig, today: &str) -> Check {
    let prices = crate::prices::Prices::from_config(cfg);
    let table = &prices.table;
    let age = table.age_days(today);
    let overrides = match table.overridden.as_slice() {
        [] => String::new(),
        some => format!(" · overridden here: {}", some.join(", ")),
    };
    let stale = table.is_stale(today);
    let vintage = match age {
        Some(days) if stale => format!(
            "list prices last checked {} ({days} days ago) — old enough to have missed a \
             change; re-check the published prices and override what moved in \
             `[defaults.rates]`",
            table.as_of
        ),
        Some(days) => format!(
            "list prices as of {} ({days} days ago), shipped with this binary",
            table.as_of
        ),
        None => format!(
            "list prices as of {}, shipped with this binary",
            table.as_of
        ),
    };
    Check {
        name: "token rates".into(),
        ok: !stale,
        detail: format!(
            "{} model(s) priced · {vintage}{overrides}. A model with no rate is reported \
             unpriced rather than free",
            table.entries.len()
        ),
    }
}

/// What the plans behind that spend cost — the half only a person can tell the
/// board (gh#182).
///
/// Always printed, like the duration cap, and for the same reason: with nothing
/// configured every account reports as *unknown*, which looks exactly like a
/// board that was told and got zero. Unknown is the honest default — comet
/// never sees anybody's invoice — but it is worth saying which state you are in.
fn subscriptions_check(
    cfg: &crate::config::RoutingConfig,
    accounts: Option<&[AgentAccount]>,
) -> Check {
    if cfg.accounts.is_empty() {
        return Check {
            name: "subscription cost".into(),
            ok: true,
            detail: "not configured — the stats page prices what the board ran at list \
                     price, and says nothing about what the plans behind it cost. Add \
                     `[account.\"<slot>\"] monthly_usd = …` to compare the two"
                .into(),
        };
    }
    let configured: Vec<String> = cfg
        .accounts
        .iter()
        .map(|(slot, account)| {
            let who = account.email.as_deref().unwrap_or(slot);
            match &account.plan {
                Some(plan) => format!(
                    "{who}: {plan} at {}/month",
                    comet_proto::view::rates::human_usd(account.monthly_usd)
                ),
                None => format!(
                    "{who}: {}/month",
                    comet_proto::view::rates::human_usd(account.monthly_usd)
                ),
            }
        })
        .collect();
    // A plan written against a slot this device has never saved is settings
    // that apply to nothing — silently, which is the failure mode every other
    // config check here exists to remove.
    let unknown: Vec<&str> = match accounts {
        None => Vec::new(),
        Some(saved) => cfg
            .accounts
            .keys()
            .filter(|slot| {
                !saved.iter().any(|a| {
                    a.id.eq_ignore_ascii_case(slot)
                        || a.email
                            .as_deref()
                            .is_some_and(|e| e.eq_ignore_ascii_case(slot))
                })
            })
            .filter(|slot| {
                // An entry keyed by email, whose email matches a saved login,
                // is fine however it was written.
                !cfg.accounts[*slot].email.as_deref().is_some_and(|email| {
                    saved.iter().any(|a| {
                        a.email
                            .as_deref()
                            .is_some_and(|e| e.eq_ignore_ascii_case(email))
                    })
                })
            })
            .map(String::as_str)
            .collect(),
    };
    Check {
        name: "subscription cost".into(),
        ok: unknown.is_empty(),
        detail: match unknown.as_slice() {
            [] => configured.join(" · "),
            some => format!(
                "{} · no saved login matches {}, so those plans price nothing (\
                 `comet-board doctor` lists the slot ids under agent accounts)",
                configured.join(" · "),
                some.join(", ")
            ),
        },
    }
}

fn billing_guard_check(cfg: &crate::config::RoutingConfig) -> Check {
    use crate::billing::GuardMode;
    let mode = cfg.billing_guard(None);
    // A route that answers differently from the board is worth naming here: the
    // whole `doctor` line would otherwise describe a default that the route
    // somebody actually dispatches on does not use.
    let overrides: Vec<String> = cfg
        .routes
        .iter()
        .filter(|r| r.billing_guard.is_some() && cfg.billing_guard(Some(r)) != mode)
        .map(|r| {
            format!(
                "{} = {}",
                r.display_name(),
                cfg.billing_guard(Some(r)).as_str()
            )
        })
        .collect();
    let detail = match mode {
        GuardMode::Warn => "warn — a dispatch that spends someone else's subscription says so \
             in the picker, on the CLI, in the dispatch comment and on the row, \
             and releases anyway. Nothing is refused: the visibility is the \
             point, and a box where two people share one plan is a normal box"
            .to_string(),
        GuardMode::RequireOwn => "require-own — a dispatch that would spend someone else's \
             subscription is refused unless it names them (`--bill`). A \
             teammate's dispatch is matched against the identity the edge \
             verified, so misreporting a signed-in user changes nothing; a \
             dispatch issued on this box carries no such identity and is \
             matched against what its frontend said, which is all a local \
             shell can be asked for"
            .to_string(),
        GuardMode::Off => "off — nothing is said when a dispatch spends someone else's \
             subscription. The right answer on a box where one person's plan \
             pays for everything, and a choice rather than an oversight \
             (`[defaults] billing_guard = \"warn\"` to hear about it)"
            .to_string(),
    };
    Check {
        name: "billing guard".into(),
        ok: true,
        detail: match overrides.as_slice() {
            [] => detail,
            some => format!("{detail}. Per route: {}", some.join(", ")),
        },
    }
}

/// What a dispatch that names no account spends, and who else is in a position
/// to spend it (gh#161).
///
/// The other half of the billing guard, from the side nobody looks at: `account`
/// is optional on a route, and a dispatch that names none falls to this box's
/// own CLI login. On a one-person box that is not a default at all — it is the
/// only login there is, and saying anything about it would be noise. On a
/// workspace with somebody else in it, it is the box owner's plan paying for
/// whoever pressed enter, and the fact that it is *quiet* is exactly why the
/// question never gets asked.
///
/// Never fails, for [`billing_guard_check`]'s reason: sharing one plan
/// deliberately is a normal way to run a box, and the line exists so that
/// choice is made rather than defaulted into.
fn default_account_check(
    cfg: &crate::config::RoutingConfig,
    accounts: Option<&[AgentAccount]>,
    members: Option<usize>,
) -> Check {
    let name = "default account".to_string();
    let unnamed: Vec<String> = cfg
        .routes
        .iter()
        .filter(|r| r.account.as_deref().unwrap_or("").trim().is_empty())
        .map(|r| r.display_name().to_string())
        .collect();
    if cfg.routes.is_empty() || unnamed.is_empty() {
        return Check {
            name,
            ok: true,
            detail: "every route names an `account` — no dispatch falls through to this \
                     box's own CLI login"
                .into(),
        };
    }
    // Whose login that fallback actually is, where the engine could say. The
    // active login per harness is what a dispatch with no slot runs under.
    let mine: Vec<String> = accounts
        .unwrap_or_default()
        .iter()
        .filter(|a| a.active)
        .filter_map(|a| a.email.clone())
        .filter(|e| !e.trim().is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let login = match mine.as_slice() {
        [] => "this box's own CLI login".to_string(),
        some => format!("this box's own CLI login ({})", some.join(", ")),
    };
    let routes = match unnamed.as_slice() {
        [one] => format!("route `{one}` names no `account`"),
        many => format!("{} routes name no `account`", many.len()),
    };
    let detail = match members {
        // The case this check exists for.
        Some(n) if n > 1 => format!(
            "{routes}, so a dispatch that names none runs on {login} — and there are {n} \
             people in this workspace. A teammate's release spends the box owner's plan \
             unless they pass `--account <their slot>`; `[defaults] billing_guard = \
             \"require-own\"` refuses those outright"
        ),
        Some(_) => format!(
            "{routes}, so a dispatch that names none runs on {login} — which is yours: \
             one person in this workspace, and the fallback is the only login there is"
        ),
        None => format!(
            "{routes}, so a dispatch that names none runs on {login}. Who else could \
             spend it was not checked — the engine could not be asked for the workspace \
             roster"
        ),
    };
    Check {
        name,
        ok: true,
        detail,
    }
}

/// Which repos the board will write to, by name.
///
/// Named repos, not a posture. A global `ON` was enough while one flag answered
/// for every repo; once a repo can answer for itself, the operator's question is
/// about one repo in particular — and reading it off two keys in `routing.toml`
/// is exactly what `doctor` is for.
fn writeback_detail(github: &crate::config::GithubConfig) -> String {
    let writes = github.writeback_repos();
    let reads = github.read_only_repos();
    match (writes.as_slice(), reads.as_slice()) {
        ([], []) => "no repos under [github] to write to".into(),
        ([], _) => format!(
            "off for every repo — the board only reads GitHub (`[github] writeback \
             = true`, or per repo, to enable). Read-only: {}",
            reads.join(", ")
        ),
        (_, []) => format!(
            "ON for every repo — dispatch comments on real issues and closes them \
             on done: {}",
            writes.join(", ")
        ),
        _ => format!(
            "ON — comments and closes on: {}. Read-only: {}",
            writes.join(", "),
            reads.join(", ")
        ),
    }
}

/// The kebab-case id a harness answers to (`ClaudeCode` → `claude-code`) —
/// read back off its wire form so the report and the wire cannot drift.
fn harness_name(h: comet_proto::HarnessId) -> String {
    serde_json::to_value(h)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{h:?}"))
}

pub fn print_doctor(checks: &[Check]) -> bool {
    let mut all_ok = true;
    for c in checks {
        if !c.ok {
            all_ok = false;
        }
        println!(
            "{} {:<26} {}",
            if c.ok { "ok  " } else { "FAIL" },
            c.name,
            c.detail
        );
    }
    all_ok
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::github::HttpRest;
    use crate::sources::github_app::{TokenProvider, test_app};

    fn tmp() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().to_path_buf(),
            state_dir: dir.path().to_path_buf(),
        };
        (dir, paths)
    }

    fn engine_up() -> EngineStatus {
        EngineStatus {
            reachable: true,
            detail: "listening on 127.0.0.1:27654".into(),
            // The version this crate was built at, so the `cli version` check
            // stays quiet in every test that is about something else.
            version: Some(env!("CARGO_PKG_VERSION").into()),
            // No release answer, so the `release` check says "not checked" in
            // every test that is about something else — the same reason the
            // version above is the matching one.
            update: None,
            // No sweep, for the same reason: every test that is not about the
            // other devices on the account gets "not checked" (gh#195).
            peers: None,
        }
    }

    fn space(name: &str) -> Space {
        Space {
            id: format!("s-{name}"),
            device_id: "dev-1".into(),
            path: format!("/code/{name}"),
            name: Some(name.into()),
            git_detected: true,
            git_checked_at: None,
            checkout_id: None,
            branch: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn doctor_reports_a_missing_routing_file_without_panicking() {
        let (_d, p) = tmp();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        assert!(checks.iter().any(|c| c.name == "routing.toml" && !c.ok));
        // The database check must still pass — doctor creates it.
        assert!(checks.iter().any(|c| c.name == "database" && c.ok));
    }

    // --- cli version (gh#156) ------------------------------------------------

    /// A managed install: `app/<version>` with a `current` symlink onto it, and
    /// `payload` deciding whether that release carries a `comet-board` at all.
    fn app_root_at(dir: &Path, version: &str, payload: &[&str]) -> PathBuf {
        let app_root = dir.join("app");
        let versioned = app_root.join(version);
        std::fs::create_dir_all(&versioned).unwrap();
        for bin in payload {
            std::fs::write(versioned.join(bin), b"#!/bin/sh\n").unwrap();
        }
        std::os::unix::fs::symlink(&versioned, app_root.join("current")).unwrap();
        app_root
    }

    /// The gh#156 box, exactly: engine upgraded to a new release, `comet-board`
    /// still the hand-built binary somebody symlinked weeks ago. Nothing on that
    /// machine said so until an agent typed a verb that did not exist.
    #[test]
    fn a_board_cli_older_than_its_engine_fails() {
        let d = tempfile::tempdir().unwrap();
        let app_root = app_root_at(d.path(), "0.3.4", &["comet", "comet-board"]);
        let exe = d.path().join("src/target/release/comet-board");
        let c = cli_version_check("0.2.9", Some("0.3.4"), Some(&exe), &app_root);
        assert!(!c.ok, "{}", c.detail);
        assert!(c.detail.contains("v0.2.9"), "{}", c.detail);
        assert!(c.detail.contains("v0.3.4"), "{}", c.detail);
        // The path is the actionable half: which of the copies on this box is
        // the one that answered.
        assert!(
            c.detail.contains("src/target/release/comet-board"),
            "{}",
            c.detail
        );
        assert!(
            c.detail.contains("not what that release installed"),
            "{}",
            c.detail
        );
    }

    /// The same drift, against a release that predates shipping the CLI. Telling
    /// this box to re-run the installer would relink nothing, so it must not.
    #[test]
    fn drift_against_an_engine_only_payload_says_so_instead_of_offering_the_installer() {
        let d = tempfile::tempdir().unwrap();
        let app_root = app_root_at(d.path(), "0.3.4", &["comet"]);
        let c = cli_version_check("0.2.9", Some("0.3.4"), None, &app_root);
        assert!(!c.ok, "{}", c.detail);
        assert!(c.detail.contains("ships the engine alone"), "{}", c.detail);
        assert!(!c.detail.contains("install.sh"), "{}", c.detail);
    }

    /// Matching versions are a passing line, not a silent one — it is the only
    /// place the report says which binary is talking.
    #[test]
    fn a_cli_shipped_with_its_engine_passes() {
        let d = tempfile::tempdir().unwrap();
        let app_root = app_root_at(d.path(), "0.3.4", &["comet", "comet-board"]);
        let exe = app_root.join("0.3.4/comet-board");
        let c = cli_version_check("0.3.4", Some("0.3.4"), Some(&exe), &app_root);
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("v0.3.4"), "{}", c.detail);
        // Says which binary answered, and accuses it of nothing.
        assert!(c.detail.contains("comet-board"), "{}", c.detail);
        assert!(!c.detail.contains("not what that release"), "{}", c.detail);
    }

    /// Engine unreachable, or too old to report a version: the installed
    /// payload's own directory name answers instead. This is what makes the
    /// check work on a box whose engine is down — which is exactly when someone
    /// is running doctor.
    #[test]
    fn an_unreachable_engine_falls_back_to_the_installed_payload() {
        let d = tempfile::tempdir().unwrap();
        let app_root = app_root_at(d.path(), "0.3.4", &["comet", "comet-board"]);
        let c = cli_version_check("0.2.9", None, None, &app_root);
        assert!(!c.ok, "{}", c.detail);
        assert!(
            c.detail.contains("the release installed here is v0.3.4"),
            "{}",
            c.detail
        );
    }

    /// A laptop with no managed install and no engine up has nothing to be
    /// wrong about, and must not be failed for it.
    #[test]
    fn nothing_to_compare_against_is_not_a_failure() {
        let d = tempfile::tempdir().unwrap();
        let c = cli_version_check("0.3.4", None, None, &d.path().join("app"));
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("no engine to compare"), "{}", c.detail);
    }

    // --- release (gh#197) ---------------------------------------------------

    fn update_status(here: &str, latest: Option<&str>) -> comet_update::UpdateStatus {
        comet_update::UpdateStatus {
            current_version: here.into(),
            latest_version: latest.map(str::to_string),
            update_available: latest.is_some_and(|l| comet_update::version_newer(l, here)),
            checked_at: Some(0),
            error: None,
            can_apply: None,
        }
    }

    /// The ordinary answer: the box runs what the edge hands out.
    #[test]
    fn a_box_on_the_published_release_passes() {
        let c = release_check(Some(&update_status("0.3.5", Some("0.3.5"))), 60_000);
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("v0.3.5"), "{}", c.detail);
        // The age matters: a version quoted without one could predate the
        // release being asked about.
        assert!(c.detail.contains("last checked 1m ago"), "{}", c.detail);
    }

    /// Behind is reported, never failed — every box is behind for the window
    /// between a release and its next check, and a doctor that goes red across
    /// the fleet on every release teaches people to ignore it.
    #[test]
    fn a_box_behind_the_edge_is_reported_and_not_failed() {
        let c = release_check(Some(&update_status("0.3.4", Some("0.3.5"))), 0);
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("edge serves v0.3.5"), "{}", c.detail);
        assert!(c.detail.contains("comet update"), "{}", c.detail);
    }

    /// The gh#197 state, seen from a box: the install surface points at a
    /// version older than what runs here, so anything that upgrades from it —
    /// including a machine installing for the first time — gets the old release
    /// and reports success. That is a publish that half-landed, and it is the
    /// one verdict here worth an exit code.
    #[test]
    fn an_edge_serving_an_older_release_than_this_box_fails() {
        let c = release_check(Some(&update_status("0.3.5", Some("0.3.4"))), 0);
        assert!(!c.ok, "{}", c.detail);
        assert!(c.detail.contains("points backwards"), "{}", c.detail);
        assert!(c.detail.contains("re-run its failed job"), "{}", c.detail);
    }

    /// A truncated or garbage `latest.txt` is the other thing a half-finished
    /// publish leaves. The updater ignores it in silence (an unparseable version
    /// is never "newer"), so nothing else on the box would ever say so.
    #[test]
    fn an_uncomparable_version_at_the_edge_fails() {
        let c = release_check(Some(&update_status("0.3.5", Some("nightly"))), 0);
        assert!(!c.ok, "{}", c.detail);
        assert!(c.detail.contains("not a version"), "{}", c.detail);
    }

    /// An engine that could not be asked, one that has not looked yet, and one
    /// whose look failed: three different sentences, none of them a failure.
    /// Doctor must not invent a verdict about the edge from silence.
    #[test]
    fn nothing_known_about_the_edge_is_never_a_failure() {
        let none = release_check(None, 0);
        assert!(none.ok);
        assert!(
            none.detail.contains("the engine did not answer"),
            "{}",
            none.detail
        );

        let mut booting = update_status("0.3.5", None);
        booting.checked_at = None;
        let c = release_check(Some(&booting), 0);
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("has not looked yet"), "{}", c.detail);

        let mut failed = update_status("0.3.5", None);
        failed.checked_at = None;
        failed.error = Some("dns error".into());
        let c = release_check(Some(&failed), 0);
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("dns error"), "{}", c.detail);
    }

    /// `current` pointed at something that is not a versioned dir is not a
    /// version. Reading one out of it would invent drift that is not there.
    #[test]
    fn a_current_symlink_outside_the_versioned_layout_yields_no_version() {
        let d = tempfile::tempdir().unwrap();
        let app_root = d.path().join("app");
        let elsewhere = d.path().join("checkout/target/release");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::create_dir_all(&app_root).unwrap();
        std::os::unix::fs::symlink(&elsewhere, app_root.join("current")).unwrap();
        assert_eq!(installed_payload_version(&app_root), None);
    }

    // --- agent PATH (gh#184) -------------------------------------------------

    /// The box after the fix: the payload holds both binaries, so the directory
    /// the engine prepends to every agent's PATH has a `comet-board` in it.
    #[test]
    fn a_payload_with_both_binaries_lets_an_agent_run_the_board() {
        let d = tempfile::tempdir().unwrap();
        let app_root = app_root_at(d.path(), "0.3.5", &["comet", "comet-board"]);
        let c = agent_path_check(&app_root, None);
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("on PATH"), "{}", c.detail);
    }

    /// The one way the guarantee comes apart: a release that ships the engine
    /// alone. The PATH entry is still there and still useless, which is
    /// precisely the silence this check exists to end.
    #[test]
    fn a_payload_without_the_cli_is_a_path_with_no_board_in_it() {
        let d = tempfile::tempdir().unwrap();
        let app_root = app_root_at(d.path(), "0.3.5", &["comet"]);
        let c = agent_path_check(&app_root, None);
        assert!(!c.ok, "{}", c.detail);
        assert!(c.detail.contains("command not found"), "{}", c.detail);
    }

    /// A laptop with no managed install: the answer is where this CLI resolves
    /// from, and it is not a failure.
    #[test]
    fn no_managed_install_answers_from_where_the_cli_resolves() {
        let d = tempfile::tempdir().unwrap();
        let c = agent_path_check(&d.path().join("app"), Some(&d.path().join("target/debug")));
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("no managed install"), "{}", c.detail);
    }

    /// Nothing anywhere: no board verb an agent types can work, and saying so
    /// is the whole job.
    #[test]
    fn no_comet_board_anywhere_fails_loudly() {
        let d = tempfile::tempdir().unwrap();
        let c = agent_path_check(&d.path().join("app"), None);
        assert!(!c.ok, "{}", c.detail);
        assert!(
            c.detail.contains(git_credentials::BOARD_EXE_ENV),
            "{}",
            c.detail
        );
    }

    // --- board hosts (gh#195) ------------------------------------------------

    fn cfg_polling(repos: &[&str]) -> RoutingConfig {
        let text = format!(
            "[github]\nrepos = [{}]\n",
            repos
                .iter()
                .map(|r| format!("\"{r}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        toml::from_str(&text).expect("the fixture parses")
    }

    fn peer(device: &str, repos: &[&str]) -> PeerBoard {
        PeerBoard {
            device: device.into(),
            repos: repos.iter().map(|r| (*r).to_string()).collect(),
            unparsed: false,
        }
    }

    /// The account this issue was found on: a Mac board and a box board, one
    /// route and seventeen, no repo in common. Legitimate, and worth saying out
    /// loud — the fact nothing on either machine could see.
    #[test]
    fn a_second_board_over_disjoint_repos_is_reported_and_not_failed() {
        let peers = Peers {
            boards: vec![peer("box", &["tally/Tally", "tally/oppgang"])],
            unreachable: Vec::new(),
            asked: 2,
        };
        let c = board_hosts_check(Some(&cfg_polling(&["bredebjorhovd/attn"])), Some(&peers));
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("box"), "{}", c.detail);
        // Named, not counted: the operator's next move needs the slugs.
        assert!(c.detail.contains("tally/Tally"), "{}", c.detail);
    }

    /// The day one slug lands in both lists. Both boards call the issue ready,
    /// either dispatches it, and neither row shows the other's attempt — the
    /// one state worth an exit code.
    #[test]
    fn one_repo_on_two_boards_fails() {
        let peers = Peers {
            boards: vec![peer("box", &["bredebjorhovd/ATTN"])],
            unreachable: Vec::new(),
            asked: 1,
        };
        let c = board_hosts_check(Some(&cfg_polling(&["bredebjorhovd/attn"])), Some(&peers));
        // Case-folded: GitHub reads the two spellings as one repo, and a check
        // that did not would miss the collision it exists for.
        assert!(!c.ok, "{}", c.detail);
        assert!(c.detail.contains("bredebjorhovd/ATTN"), "{}", c.detail);
        assert!(c.detail.contains("box"), "{}", c.detail);
    }

    /// gh#155's lesson on this surface: a device that could not be asked is not
    /// a device that hosts no board, and a sweep that says "the only board" on
    /// an incomplete answer is the reassuring sentence this must never print by
    /// accident.
    #[test]
    fn a_device_that_could_not_be_asked_is_named_rather_than_ruled_out() {
        let peers = Peers {
            boards: Vec::new(),
            unreachable: vec!["box".into()],
            asked: 2,
        };
        let c = board_hosts_check(Some(&cfg_polling(&["o/r"])), Some(&peers));
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("could not ask box"), "{}", c.detail);
        assert!(c.detail.contains("invisible from here"), "{}", c.detail);
    }

    /// A lone device: nothing else on the account can be polling, and there is
    /// no warning to give.
    #[test]
    fn the_only_device_on_the_account_says_so() {
        let c = board_hosts_check(Some(&cfg_polling(&["o/r"])), Some(&Peers::default()));
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("only one registered"), "{}", c.detail);
    }

    /// What the box would answer: its `routing.toml`, parsed, with the repos it
    /// polls. A legacy `linear_team` route in the peer's file (gh#471) still
    /// parses and contributes nothing to the census.
    #[test]
    fn a_peers_reply_carries_what_that_board_polls() {
        let reply = serde_json::json!({
            "routing": {
                "path": "/home/comet/.comet-native/board/routing.toml",
                "exists": true,
                "text": "",
                "config": {
                    "github": { "repos": ["tally/Tally", "tally/oppgang"] },
                    "route": [{
                        "match": { "linear_team": "AGE" },
                        "workspace": "tally",
                        "repo": "/home/comet/code/tally",
                        "runtime": "claude-code",
                    }],
                },
                "problems": [],
                "backup": false,
            },
            "unadopted": [],
        });
        let board = peer_board("box".into(), &reply);
        assert!(!board.unparsed);
        assert_eq!(board.repos, ["tally/Tally", "tally/oppgang"]);
    }

    /// A peer whose config did not parse answers without one. "Polls nothing"
    /// would rule out a collision this cannot see, so it is unknown instead.
    #[test]
    fn a_peer_with_no_parse_is_unknown_rather_than_empty() {
        let reply = serde_json::json!({
            "routing": {
                "path": "/home/comet/.comet-native/board/routing.toml",
                "exists": true,
                "text": "[[route]\n",
                "problems": ["routing.toml does not parse"],
                "backup": false,
            },
            "unadopted": [],
        });
        let board = peer_board("box".into(), &reply);
        assert!(board.unparsed);
        assert!(board.repos.is_empty());
    }

    // --- refusing the second board at write time (gh#343) --------------------

    /// The whole point of moving the check earlier: the slug another board
    /// already polls is refused before it can be written, and the sentence
    /// names the board it is on.
    #[test]
    fn a_repo_another_board_polls_is_refused_by_name() {
        let boards = [peer("Tokenmaxxer9000", &["bredebjorhovd/itsm-agent"])];
        let refusal = already_polled("bredebjorhovd/itsm-agent", &boards)
            .expect("the collision this exists for");
        assert!(refusal.contains("Tokenmaxxer9000"), "{refusal}");
        // The two ways out, both said: take it off the other board, or say
        // out loud that sharing it is intended.
        assert!(refusal.contains("--force"), "{refusal}");
    }

    /// Same repo, other spelling. GitHub reads them as one repo and so must
    /// this, or the refusal misses the day somebody retypes a slug.
    #[test]
    fn a_differently_cased_slug_is_the_same_repo() {
        let boards = [peer("box", &["bredebjorhovd/ATTN"])];
        assert!(already_polled(" bredebjorhovd/attn ", &boards).is_some());
    }

    /// Two boards over disjoint repos is the legitimate setup gh#195 declined
    /// to fail on, and adding to it must stay silent.
    #[test]
    fn a_repo_nobody_else_polls_is_written_without_a_word() {
        let boards = [peer("box", &["tally/Tally", "tally/oppgang"])];
        assert!(already_polled("bredebjorhovd/attn", &boards).is_none());
    }

    /// A peer that would not say what it polls does not refuse: unknown is not
    /// a collision, and a board blocked by somebody else's broken config would
    /// be a worse failure than the one being prevented. `doctor` still names it.
    #[test]
    fn a_peer_whose_config_does_not_parse_does_not_refuse_the_write() {
        let boards = [PeerBoard {
            device: "box".into(),
            repos: Vec::new(),
            unparsed: true,
        }];
        assert!(already_polled("bredebjorhovd/attn", &boards).is_none());
    }

    /// A peer whose own `routing.toml` will not parse polls *something*
    /// unknown, which is not the same as polling nothing — the collision it may
    /// be in cannot be ruled out, and the line says so.
    #[test]
    fn a_peer_whose_config_does_not_parse_is_unknown_not_empty() {
        let peers = Peers {
            boards: vec![PeerBoard {
                device: "box".into(),
                repos: Vec::new(),
                unparsed: true,
            }],
            unreachable: Vec::new(),
            asked: 1,
        };
        let c = board_hosts_check(Some(&cfg_polling(&["o/r"])), Some(&peers));
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("unknown"), "{}", c.detail);
    }

    /// No sweep at all — a laptop with the engine down. "Not checked", never
    /// "no other board": the same rule every other engine-fed check follows.
    #[test]
    fn no_sweep_is_not_checked_rather_than_no_other_board() {
        let checks = doctor(
            &tmp().1,
            &engine_up(),
            Some(&[]),
            Some(&[]),
            None,
            None,
            None,
        )
        .unwrap();
        let c = checks
            .iter()
            .find(|c| c.name == "board hosts")
            .expect("board hosts is always reported");
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("not checked"), "{}", c.detail);
    }

    fn edge_check_in(checks: &[Check]) -> &Check {
        checks
            .iter()
            .find(|c| c.name == "edge connections")
            .expect("edge connections is always reported")
    }

    /// gh#116: the state the box was actually in — engine up, IPC answering,
    /// every edge socket dead. Doctor has to fail on it, because nothing else
    /// on this box can tell.
    #[test]
    fn an_engine_holding_no_edge_connections_fails() {
        let (_d, p) = tmp();
        let dark = EdgeHealth {
            edge_url: Some("https://edge.example".into()),
            host_relay: Some(false),
            workspace_room: Some(false),
            org_registry: Some(false),
            chat_rooms_open: 1,
            chat_rooms_live: 0,
            ..EdgeHealth::default()
        };
        let checks = doctor(
            &p,
            &engine_up(),
            Some(&[]),
            Some(&[]),
            Some(&dark),
            None,
            None,
        )
        .unwrap();
        let check = edge_check_in(&checks);
        assert!(!check.ok, "{}", check.detail);
        assert!(check.detail.contains("0 of 4 live"), "{}", check.detail);
        assert!(
            check.detail.contains("no board on this device"),
            "{}",
            check.detail
        );
    }

    /// One room down while others are live is a client mid-redial, not an
    /// outage — reported, never failed, or every edge deploy would fail doctor.
    #[test]
    fn one_room_down_is_reported_but_does_not_fail() {
        let (_d, p) = tmp();
        let partial = EdgeHealth {
            edge_url: Some("https://edge.example".into()),
            host_relay: Some(true),
            workspace_room: Some(false),
            org_registry: Some(true),
            chat_rooms_open: 0,
            chat_rooms_live: 0,
            ..EdgeHealth::default()
        };
        let checks = doctor(
            &p,
            &engine_up(),
            Some(&[]),
            Some(&[]),
            Some(&partial),
            None,
            None,
        )
        .unwrap();
        let check = edge_check_in(&checks);
        assert!(check.ok, "{}", check.detail);
        assert!(
            check.detail.contains("workspace room down"),
            "{}",
            check.detail
        );
    }

    /// gh#527: the state the box was actually in on 2026-08-19 — every socket
    /// live when asked, every room dying a second later, nothing reaching the
    /// phone. Doctor passed this all evening. It must not any more.
    #[test]
    fn an_engine_whose_rooms_keep_dying_fails_even_though_every_socket_is_live() {
        let (_d, p) = tmp();
        let churning = EdgeHealth {
            edge_url: Some("https://edge.example".into()),
            host_relay: Some(true),
            workspace_room: Some(true),
            org_registry: Some(true),
            chat_rooms_open: 7,
            chat_rooms_live: 7,
            rooms_churning: 8,
            sessions_died_young_last_hour: 61,
            churning_rooms: vec![comet_proto::RoomChurn {
                room_id: "ws4/org/user".into(),
                died_young_last_hour: 22,
                sessions_last_hour: 22,
            }],
            ..EdgeHealth::default()
        };
        let checks = doctor(
            &p,
            &engine_up(),
            Some(&[]),
            Some(&[]),
            Some(&churning),
            None,
            None,
        )
        .unwrap();
        let check = edge_check_in(&checks);
        assert!(!churning.dark(), "the sockets really are live");
        assert!(!check.ok, "{}", check.detail);
        assert!(check.detail.contains("10 of 10 live"), "{}", check.detail);
        assert!(check.detail.contains("CHURNING"), "{}", check.detail);
        assert!(
            check.detail.contains("duration cap"),
            "the check has to say where to look: {}",
            check.detail
        );
    }

    /// No answer from the engine is the `engine` check's failure to report, not
    /// this one's — one dead engine must not produce two red lines.
    #[test]
    fn an_unaskable_engine_leaves_the_edge_check_unchecked() {
        let (_d, p) = tmp();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let check = edge_check_in(&checks);
        assert!(check.ok);
        assert!(check.detail.contains("not checked"), "{}", check.detail);
    }

    #[test]
    fn an_unreachable_engine_fails_its_own_check_and_no_others() {
        let (_d, p) = tmp();
        std::fs::write(
            p.routing(),
            "[[route]]\nmatch = { gh_repo = \"o/r\" }\nworkspace = \"w\"\n\
             repo = \"/nowhere\"\nruntime = \"claude-code\"\n",
        )
        .unwrap();
        let down = EngineStatus {
            reachable: false,
            detail: "connection refused".into(),
            version: None,
            update: None,
            peers: None,
        };
        let checks = doctor(&p, &down, None, Some(&[]), None, None, None).unwrap();
        assert!(checks.iter().any(|c| c.name == "engine" && !c.ok));
        // The route's space is "not checked", not failed: one dead engine must
        // not fail every route and bury its own report.
        let space = checks
            .iter()
            .find(|c| c.name == "route w: space")
            .expect("the route is still reported");
        assert!(space.ok, "{}", space.detail);
        assert!(space.detail.contains("not checked"), "{}", space.detail);
    }

    /// A route pointing at a clone with no remote fails its `base` check rather
    /// than failing every dispatch on it (gh#67) — and the opt-out passes.
    #[test]
    fn a_route_whose_repo_has_no_origin_fails_the_base_check() {
        let (d, p) = tmp();
        let repo = d.path().join("local-only");
        std::fs::create_dir_all(&repo).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["-C", &repo.to_string_lossy(), "init", "-b", "main"])
                .output()
                .unwrap()
                .status
                .success()
        );
        let routing = |base: &str| {
            std::fs::write(
                p.routing(),
                format!(
                    "[[route]]\nmatch = {{ label = \"x\" }}\nworkspace = \"w\"\n\
                     repo = \"{}\"\nruntime = \"claude-code\"\nbase = \"{base}\"\n",
                    repo.display()
                ),
            )
            .unwrap();
        };
        let base_check_in = |checks: &[Check]| {
            checks
                .iter()
                .find(|c| c.name == "route w: base")
                .map(|c| (c.ok, c.detail.clone()))
                .expect("the route's base is checked")
        };

        routing("origin/HEAD");
        let (ok, detail) = base_check_in(
            &doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap(),
        );
        assert!(!ok, "{detail}");
        assert!(detail.contains("origin"), "{detail}");
        assert!(detail.contains("HEAD"), "the opt-out is named: {detail}");

        routing("HEAD");
        let (ok, detail) = base_check_in(
            &doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap(),
        );
        assert!(ok, "{detail}");
    }

    fn account(id: &str, email: &str, harness: comet_proto::HarnessId) -> AgentAccount {
        AgentAccount {
            id: id.into(),
            harness,
            email: Some(email.into()),
            plan_label: None,
            active: false,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        }
    }

    fn routing_with_account(p: &Paths, runtime: &str, account: &str) {
        std::fs::write(
            p.routing(),
            format!(
                "[[route]]\nmatch = {{ label = \"x\" }}\nworkspace = \"w\"\n\
                 repo = \"/tmp\"\nruntime = \"{runtime}\"\naccount = \"{account}\"\n"
            ),
        )
        .unwrap();
    }

    fn account_check_in(checks: &[Check]) -> &Check {
        checks
            .iter()
            .find(|c| c.name == "route w: account")
            .expect("the route names an account, so it is checked")
    }

    /// The failure this catches is otherwise found at dispatch time, once per
    /// task, by whoever released it (gh#59).
    #[test]
    fn a_routes_account_is_checked_against_the_saved_logins() {
        let (_d, p) = tmp();
        routing_with_account(&p, "claude-code", "8f2c1d0a7b6e4539");
        let saved = [account(
            "8f2c1d0a7b6e4539",
            "sam@example.com",
            comet_proto::HarnessId::ClaudeCode,
        )];

        let ok = doctor(&p, &engine_up(), Some(&[]), Some(&saved), None, None, None).unwrap();
        let c = account_check_in(&ok);
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("sam@example.com"), "{}", c.detail);

        // An id this device has never saved: named, along with what it does have.
        routing_with_account(&p, "claude-code", "ffffffffffffffff");
        let bad = doctor(&p, &engine_up(), Some(&[]), Some(&saved), None, None, None).unwrap();
        let c = account_check_in(&bad);
        assert!(!c.ok, "{}", c.detail);
        assert!(c.detail.contains("ffffffffffffffff"), "{}", c.detail);
        assert!(c.detail.contains("8f2c1d0a7b6e4539"), "{}", c.detail);
    }

    /// The subtle one: a real saved login, for the other CLI. It resolves as an
    /// account and still cannot be handed to this route's harness.
    #[test]
    fn an_account_belonging_to_the_other_cli_fails_its_route() {
        let (_d, p) = tmp();
        routing_with_account(&p, "codex", "8f2c1d0a7b6e4539");
        let saved = [account(
            "8f2c1d0a7b6e4539",
            "sam@example.com",
            comet_proto::HarnessId::ClaudeCode,
        )];
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&saved), None, None, None).unwrap();
        let c = account_check_in(&checks);
        assert!(!c.ok, "{}", c.detail);
        assert!(c.detail.contains("claude-code"), "{}", c.detail);
        assert!(c.detail.contains("codex"), "{}", c.detail);
    }

    /// A route with no `account` is the single-user default and says nothing —
    /// and one dead engine must not fail every route that has one.
    #[test]
    fn accounts_are_silent_when_unused_and_unchecked_when_unreachable() {
        let (_d, p) = tmp();
        std::fs::write(
            p.routing(),
            "[[route]]\nmatch = { label = \"x\" }\nworkspace = \"w\"\n\
             repo = \"/tmp\"\nruntime = \"claude-code\"\n",
        )
        .unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        assert!(!checks.iter().any(|c| c.name == "route w: account"));

        routing_with_account(&p, "claude-code", "8f2c1d0a7b6e4539");
        let checks = doctor(&p, &engine_up(), Some(&[]), None, None, None, None).unwrap();
        let c = account_check_in(&checks);
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("not checked"), "{}", c.detail);
    }

    #[test]
    fn a_routes_space_is_checked_against_the_space_list() {
        let (_d, p) = tmp();
        std::fs::write(
            p.routing(),
            "[[route]]\nmatch = { gh_repo = \"o/r\" }\nworkspace = \"tally\"\n\
             repo = \"/nowhere\"\nruntime = \"claude-code\"\n",
        )
        .unwrap();
        let spaces = [space("Tally")];
        let checks = doctor(&p, &engine_up(), Some(&spaces), Some(&[]), None, None, None).unwrap();
        // Case-insensitive, like every other name match on the board.
        let c = checks
            .iter()
            .find(|c| c.name == "route tally: space")
            .unwrap();
        assert!(c.ok, "{}", c.detail);

        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let c = checks
            .iter()
            .find(|c| c.name == "route tally: space")
            .unwrap();
        assert!(!c.ok);
        assert!(c.detail.contains("no comet space named"), "{}", c.detail);
        // An empty space list is said in words, not as the empty half of
        // `(have: )` — which is what a box holding no spaces printed (gh#342).
        assert!(c.detail.contains("no spaces at all"), "{}", c.detail);
    }

    /// gh#342: the failing line names the repair, the way every other failing
    /// line here does. The state was reachable — a clone with no space, a space
    /// somebody deleted — and nothing said that one idempotent verb fixes it.
    #[test]
    fn a_route_with_no_space_is_told_which_verb_repairs_it() {
        let (d, p) = tmp();
        let repo = d.path().join("repos").join("itsm-agent");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(
            p.routing(),
            format!(
                "[github]\nrepos = [\"b/itsm-agent\"]\n\n\
                 [[route]]\nmatch = {{ gh_repo = \"b/itsm-agent\" }}\n\
                 workspace = \"itsm-agent\"\nrepo = \"{}\"\nruntime = \"claude-code\"\n",
                repo.display()
            ),
        )
        .unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let c = checks
            .iter()
            .find(|c| c.name == "route itsm-agent: space")
            .unwrap();
        assert!(!c.ok, "{}", c.detail);
        assert!(
            c.detail.contains("comet-board onboard b/itsm-agent"),
            "{}",
            c.detail
        );
        // The checkout is there, so the repair says so — an operator who thinks
        // this re-clones will not run it.
        assert!(c.detail.contains("reuses the clone"), "{}", c.detail);
    }

    /// `doctor` says which harnesses this box can start, and why the rest
    /// cannot (gh#187) — the shell's copy of what the pickers show.
    #[test]
    fn the_harness_census_names_the_ready_and_the_reason_for_the_rest() {
        use comet_proto::HarnessId;
        use comet_proto::view::board::RuntimeUnavailable;
        let (_d, p) = tmp();

        let option = |name: &str, harness, unavailable| RuntimeOption {
            name: name.into(),
            label: name.into(),
            harness,
            unavailable,
        };
        let runtimes = vec![
            option("claude-code", HarnessId::ClaudeCode, None),
            option(
                "opencode",
                HarnessId::Opencode,
                Some(RuntimeUnavailable::NotInstalled),
            ),
            option(
                "codex",
                HarnessId::Codex,
                Some(RuntimeUnavailable::SignedOut),
            ),
            // Always available, and deliberately not part of the census.
            option("mock", HarnessId::Mock, None),
        ];
        let checks = doctor(
            &p,
            &engine_up(),
            Some(&[]),
            Some(&[]),
            None,
            None,
            Some(&runtimes),
        )
        .unwrap();
        let c = checks.iter().find(|c| c.name == "harnesses").unwrap();
        assert!(c.ok, "one harness ready is not a fault: {}", c.detail);
        assert!(c.detail.contains("claude-code ready"), "{}", c.detail);
        // Both axes named apart — one is an install, the other a login.
        assert!(
            c.detail.contains("opencode (not installed)"),
            "{}",
            c.detail
        );
        assert!(c.detail.contains("codex (signed out)"), "{}", c.detail);
        assert!(!c.detail.contains("mock"), "{}", c.detail);

        // A box that can start nothing is a board that can only ever poll.
        let none: Vec<RuntimeOption> = runtimes
            .iter()
            .cloned()
            .map(|mut o| {
                if o.harness != HarnessId::Mock {
                    o.unavailable = Some(RuntimeUnavailable::NotInstalled);
                }
                o
            })
            .collect();
        let checks = doctor(
            &p,
            &engine_up(),
            Some(&[]),
            Some(&[]),
            None,
            None,
            Some(&none),
        )
        .unwrap();
        let c = checks.iter().find(|c| c.name == "harnesses").unwrap();
        assert!(!c.ok, "{}", c.detail);
        assert!(c.detail.contains("none can start here"), "{}", c.detail);

        // An engine that could not be asked says so rather than condemning the
        // box on the strength of a failed lookup.
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let c = checks.iter().find(|c| c.name == "harnesses").unwrap();
        assert!(c.ok && c.detail.contains("not checked"), "{}", c.detail);
    }

    #[test]
    fn a_routes_runtime_is_checked_against_comet_harnesses() {
        let (_d, p) = tmp();
        // `load_unvalidated` on purpose: doctor reports a bad runtime as that
        // route's failure rather than refusing the whole file.
        std::fs::write(
            p.routing(),
            "[[route]]\nmatch = { label = \"x\" }\nworkspace = \"w\"\nrepo = \"/tmp\"\n\
             runtime = \"claude\"\n\n\
             [[route]]\nmatch = { label = \"y\" }\nworkspace = \"w\"\nrepo = \"/tmp\"\n\
             runtime = \"gpt-piloted-typewriter\"\n",
        )
        .unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let alias = checks
            .iter()
            .find(|c| c.name == "route w: runtime" && c.ok)
            .expect("the alias resolves");
        assert!(alias.detail.contains("claude-code"), "{}", alias.detail);
        assert!(
            checks.iter().any(|c| c.name == "route w: runtime"
                && !c.ok
                && c.detail.contains("not a comet harness")),
            "the typo is named"
        );
    }

    /// The cap is reported whether or not the route sets one: `off` and
    /// "never thought about it" look identical on the board, and one of them
    /// means an agent on this route runs until somebody looks (gh#70).
    #[test]
    fn every_route_says_what_bounds_its_attempts() {
        let (_d, p) = tmp();
        std::fs::write(
            p.routing(),
            "[[route]]\nmatch = { label = \"x\" }\nworkspace = \"inherits\"\nrepo = \"/tmp\"\n\
             runtime = \"claude\"\n\n\
             [[route]]\nmatch = { label = \"y\" }\nworkspace = \"long\"\nrepo = \"/tmp\"\n\
             runtime = \"claude\"\nmax_duration = \"6h\"\n\n\
             [[route]]\nmatch = { label = \"z\" }\nworkspace = \"unbounded\"\nrepo = \"/tmp\"\n\
             runtime = \"claude\"\nmax_duration = \"off\"\n",
        )
        .unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let detail = |name: &str| {
            checks
                .iter()
                .find(|c| c.name == format!("route {name}: duration cap"))
                .unwrap_or_else(|| panic!("{name} has no cap line"))
                .detail
                .clone()
        };
        assert_eq!(detail("inherits"), "2h per attempt (from [defaults])");
        assert_eq!(detail("long"), "6h per attempt");
        assert!(
            detail("unbounded").starts_with("off —"),
            "{}",
            detail("unbounded")
        );
        // Off is a choice, not a broken route: it must not fail the report.
        assert!(
            checks
                .iter()
                .all(|c| c.name != "route unbounded: duration cap" || c.ok)
        );
    }

    /// gh#27's shape, inherited: a notice that never fires is the one failure
    /// that produces nothing to look at, so the line saying whether it is on
    /// has to be there in both states — and has to name the key.
    ///
    /// gh#165: it also has to say which of the two agent channels *takes* the
    /// event, because they became one chain with a fallback hop. Reporting two
    /// independent switches was the thing that made "which chat gets this?"
    /// unanswerable from the report.
    #[test]
    fn doctor_says_which_channel_takes_a_settle() {
        let mut d = crate::config::Defaults::default();
        assert!(d.notify_dispatcher, "the default state since gh#165");

        // Dispatcher-first, and nothing behind it.
        let bare = settle_notice_detail(&d);
        assert!(bare.starts_with("on —"), "{bare}");
        assert!(bare.contains("the chat that released the work"), "{bare}");
        assert!(
            bare.contains("reaches no agent at all"),
            "an unpinned board has to be told what it drops: {bare}"
        );

        // Dispatcher-first with the fallback behind it: the whole chain, in
        // order, in one line.
        d.orchestrator_chat = Some("chat-boss".into());
        let chained = settle_notice_detail(&d);
        assert!(chained.contains("that chat is gone"), "{chained}");
        assert!(chained.contains("chat-boss"), "{chained}");

        // Off with a pin is a routing choice, and the line says what it costs
        // the pinned chat rather than reporting a switch.
        d.notify_dispatcher = false;
        let routed = settle_notice_detail(&d);
        assert!(routed.starts_with("off —"), "{routed}");
        assert!(routed.contains("chat-boss"), "{routed}");
        assert!(routed.contains("notify_dispatcher"), "{routed}");

        // Off with nothing pinned is the one silent state.
        d.orchestrator_chat = None;
        let silent = settle_notice_detail(&d);
        assert!(silent.contains("no agent is told"), "{silent}");
        assert!(
            silent.contains("notify_dispatcher"),
            "off has to name the key to turn it on: {silent}"
        );
    }

    /// gh#71. `notify` used to be parsed, documented, and read nowhere — and
    /// `doctor` implied it notified you. The line now has to be true in every
    /// state, and only the one genuine misconfiguration may fail: an address
    /// that cannot be posted to. Not wanting the channel is not a fault, or
    /// `doctor` would exit 1 on a default install and stop meaning anything.
    #[test]
    fn doctor_tells_the_truth_about_the_operator_channel() {
        let mut d = crate::config::Defaults::default();
        assert!(d.notify && d.notify_webhook.is_none(), "the default state");
        let c = operator_notice_check(&d);
        assert!(c.ok, "a preference is not a failure: {}", c.detail);
        assert!(c.detail.starts_with("not configured —"), "{}", c.detail);
        assert!(c.detail.contains("notify_webhook"), "{}", c.detail);

        d.notify_webhook = Some("https://hooks.example.com/x".into());
        let c = operator_notice_check(&d);
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.starts_with("on —"), "{}", c.detail);
        assert!(c.detail.contains("hooks.example.com"), "{}", c.detail);
        assert!(
            !c.detail.contains("/x"),
            "a webhook URL is the credential; name the host only: {}",
            c.detail
        );

        // Muted is a decision, and says which key made it.
        d.notify = false;
        let c = operator_notice_check(&d);
        assert!(c.ok && c.detail.starts_with("muted —"), "{}", c.detail);

        // A typo is silence with a warning in a log nobody reads — the one
        // state worth failing over.
        d.notify = true;
        d.notify_webhook = Some("hooks.example.com/x".into());
        assert!(!operator_notice_check(&d).ok);
    }

    /// A block comments on its issue — except on a repo the board only reads,
    /// where it comments nowhere, and an operator should learn that here.
    #[test]
    fn doctor_names_the_repos_where_a_block_shows_nowhere() {
        let cfg: RoutingConfig = toml::from_str(
            "[github]\nrepos = [\"o/mine\", \"o/theirs\"]\nwriteback = true\n\n\
             [[github.repo]]\nname = \"o/theirs\"\nwriteback = false\n",
        )
        .unwrap();
        let detail = blocked_notice_detail(&cfg.github);
        assert!(detail.starts_with("on —"), "{detail}");
        assert!(detail.contains("o/theirs"), "{detail}");
        assert!(!detail.contains("o/mine"), "{detail}");
    }

    /// gh#101. Three modes, one line, and it never fails — `off` is the right
    /// answer on a one-person box, and a `doctor` that nagged about a
    /// preference is a `doctor` people stop reading. What it must never do is
    /// describe the match as stronger — or weaker — than it is (gh#161).
    #[test]
    fn the_billing_guard_line_reports_the_mode_and_what_the_match_is_made_of() {
        let mut cfg = RoutingConfig::default();

        let warn = billing_guard_check(&cfg);
        assert!(warn.ok);
        assert!(warn.detail.starts_with("warn —"), "{}", warn.detail);

        cfg.defaults.billing_guard = "require-own".into();
        let strict = billing_guard_check(&cfg);
        assert!(strict.ok, "a stricter mode is not a fault");
        assert!(strict.detail.contains("--bill"), "{}", strict.detail);
        // Both halves of the truth, and neither hedging about the other's
        // case: verified over the relay, the frontend's word on the box.
        assert!(
            strict.detail.contains("the identity the edge verified"),
            "the relayed half is the lock: {}",
            strict.detail
        );
        assert!(
            strict.detail.contains("issued on this box"),
            "the local half is still a claim, and says so: {}",
            strict.detail
        );

        cfg.defaults.billing_guard = "off".into();
        let off = billing_guard_check(&cfg);
        assert!(off.ok, "off is a choice, not an oversight");
        assert!(
            off.detail.contains("The right answer on a box where"),
            "worded as a choice: {}",
            off.detail
        );
    }

    /// gh#161's other half: the default nobody set. The same unnamed `account`
    /// is unremarkable on a one-person box and the whole problem on a shared
    /// one, so the line turns on who else is in the workspace — and never
    /// fails, because sharing a plan on purpose is a normal way to run a box.
    #[test]
    fn the_default_account_line_turns_on_who_else_could_spend_it() {
        let shared: RoutingConfig = toml::from_str(
            "[[route]]\nname = \"platform\"\nmatch = { label = \"team\" }\n\
             workspace = \"w\"\nrepo = \"/tmp\"\nruntime = \"claude-code\"\n",
        )
        .unwrap();
        let mine = [AgentAccount {
            active: true,
            ..account(
                "slot-box",
                "brede@tally.no",
                comet_proto::HarnessId::ClaudeCode,
            )
        }];

        let team = default_account_check(&shared, Some(&mine), Some(3));
        assert!(team.ok, "a shared plan is a choice, not a fault");
        assert!(team.detail.contains("brede@tally.no"), "{}", team.detail);
        assert!(
            team.detail.contains("3 people in this workspace"),
            "the fact that makes the default wrong: {}",
            team.detail
        );
        assert!(team.detail.contains("--account"), "{}", team.detail);

        // Alone, the same config says nothing worth acting on.
        let solo = default_account_check(&shared, Some(&mine), Some(1));
        assert!(solo.detail.contains("which is yours"), "{}", solo.detail);
        assert!(
            !solo.detail.contains("--account"),
            "nothing to fix on a one-person box: {}",
            solo.detail
        );

        // Unasked is its own answer, and not a guess in either direction.
        let unknown = default_account_check(&shared, Some(&mine), None);
        assert!(
            unknown.detail.contains("was not checked"),
            "{}",
            unknown.detail
        );

        // A route that names its account has already answered the question.
        let named: RoutingConfig = toml::from_str(
            "[[route]]\nname = \"platform\"\nmatch = { label = \"team\" }\n\
             workspace = \"w\"\nrepo = \"/tmp\"\nruntime = \"claude-code\"\n\
             account = \"slot-ana\"\n",
        )
        .unwrap();
        let settled = default_account_check(&named, Some(&mine), Some(3));
        assert!(
            settled.detail.starts_with("every route names an `account`"),
            "{}",
            settled.detail
        );
    }

    #[test]
    fn doctor_emits_the_default_account_check() {
        let (_d, p) = tmp();
        std::fs::write(p.routing(), "[github]\nrepos = []\n").unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, Some(2), None).unwrap();
        assert!(
            checks.iter().any(|c| c.name == "default account"),
            "doctor is silent about what an unnamed account spends"
        );
    }

    /// A route that answers differently from the board is named, or the line
    /// describes a default the route people actually dispatch on does not use.
    #[test]
    fn the_billing_guard_line_names_the_routes_that_disagree() {
        let cfg: RoutingConfig = toml::from_str(
            "[defaults]\nbilling_guard = \"warn\"\n\n\
             [[route]]\nname = \"platform\"\nmatch = { label = \"team\" }\n\
             workspace = \"w\"\nrepo = \"/tmp\"\nruntime = \"claude\"\n\
             billing_guard = \"require-own\"\n\n\
             [[route]]\nname = \"scratch\"\nmatch = { label = \"mine\" }\n\
             workspace = \"w\"\nrepo = \"/tmp\"\nruntime = \"claude\"\n",
        )
        .unwrap();
        let detail = billing_guard_check(&cfg).detail;
        assert!(detail.contains("platform = require-own"), "{detail}");
        assert!(
            !detail.contains("scratch"),
            "a route that agrees with the board is not news: {detail}"
        );
    }

    #[test]
    fn doctor_emits_the_billing_guard_check() {
        let (_d, p) = tmp();
        std::fs::write(p.routing(), "[github]\nrepos = []\n").unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let c = checks
            .iter()
            .find(|c| c.name == "billing guard")
            .expect("doctor is silent about whose subscription dispatches spend");
        assert!(c.detail.starts_with("warn —"), "{:?}", c.detail);
    }

    /// The rates ship inside the binary, so the only honest thing this line
    /// can do is say when they were taken — and stop calling itself ok once
    /// that is old enough to have missed a change (gh#182).
    #[test]
    fn the_rates_line_dates_itself_and_goes_amber_when_the_table_is_old() {
        let cfg = RoutingConfig::default();
        let fresh = rates_check(&cfg, "2026-08-09");
        assert!(fresh.ok);
        assert!(
            fresh
                .detail
                .contains(comet_proto::view::rates::BUILTIN_AS_OF),
            "the date is the point: {}",
            fresh.detail
        );
        assert!(
            fresh.detail.contains("unpriced rather than free"),
            "and the rule for what it cannot price: {}",
            fresh.detail
        );

        let old = rates_check(&cfg, "2027-08-09");
        assert!(!old.ok, "a year-old price list is not a clean check");
        assert!(old.detail.contains("[defaults.rates]"), "{}", old.detail);

        // An override is named, so a reader can tell which rows are not the
        // shipped ones.
        let overridden: RoutingConfig =
            toml::from_str("[defaults.rates.\"claude-opus-5\"]\ninput = 4.0\noutput = 20.0\n")
                .unwrap();
        let detail = rates_check(&overridden, "2026-08-09").detail;
        assert!(
            detail.contains("overridden here: claude-opus-5"),
            "{detail}"
        );
    }

    #[test]
    fn the_subscription_line_says_which_of_the_two_unknowns_it_is() {
        // Nothing configured: the stats page prices the work and says nothing
        // about the bill — which is a state worth naming, because it looks
        // exactly like a plan configured at zero.
        let bare = subscriptions_check(&RoutingConfig::default(), Some(&[]));
        assert!(bare.ok);
        assert!(bare.detail.starts_with("not configured"), "{}", bare.detail);

        let cfg: RoutingConfig = toml::from_str(
            "[account.\"slot-box\"]\nemail = \"brede@tally.no\"\n\
             plan = \"Claude Max 20x\"\nmonthly_usd = 200\n",
        )
        .unwrap();
        let mine = [account(
            "slot-box",
            "brede@tally.no",
            comet_proto::HarnessId::ClaudeCode,
        )];
        let known = subscriptions_check(&cfg, Some(&mine));
        assert!(known.ok);
        assert!(known.detail.contains("Claude Max 20x"), "{}", known.detail);
        assert!(known.detail.contains("$200"), "{}", known.detail);

        // A plan written against a slot this device has never saved prices
        // nothing — silently, until this says so.
        let orphan = subscriptions_check(&cfg, Some(&[]));
        assert!(!orphan.ok);
        assert!(orphan.detail.contains("slot-box"), "{}", orphan.detail);
    }

    #[test]
    fn doctor_emits_the_rate_checks() {
        let (_d, p) = tmp();
        std::fs::write(p.routing(), "[github]\nrepos = []\n").unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        assert!(
            checks.iter().any(|c| c.name == "token rates"),
            "doctor is silent about what the board prices tokens at"
        );
        assert!(
            checks.iter().any(|c| c.name == "subscription cost"),
            "doctor is silent about what the plans behind it cost"
        );
    }

    #[test]
    fn doctor_emits_the_settle_notice_check() {
        let (_d, p) = tmp();
        std::fs::write(
            p.routing(),
            "[defaults]\nnotify_dispatcher = true\n\n[github]\nrepos = []\n",
        )
        .unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let c = checks
            .iter()
            .find(|c| c.name == "settle notice")
            .expect("doctor is silent about notify_dispatcher");
        assert!(c.detail.starts_with("on —"), "{:?}", c.detail);
    }

    /// An unpinned board is a legitimate preference, so the line has to be
    /// `ok` — and it still has to say what is *not* happening, because "nobody
    /// picked this up" reads as a bug until you know no agent was told.
    #[test]
    fn doctor_says_when_no_agent_is_running_the_board() {
        let (_d, p) = tmp();
        std::fs::write(p.routing(), "[github]\nrepos = []\n").unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let c = checks
            .iter()
            .find(|c| c.name == "orchestrator")
            .expect("doctor is silent about the pin");
        assert!(c.ok, "not pinning anything is not a fault");
        assert!(c.detail.starts_with("not pinned"), "{}", c.detail);
        assert!(c.detail.contains("orchestrator_chat"), "{}", c.detail);
    }

    #[test]
    fn doctor_names_the_pinned_chat() {
        let (_d, p) = tmp();
        std::fs::write(
            p.routing(),
            "[defaults]\norchestrator_chat = \"chat-boss\"\n\n[github]\nrepos = []\n",
        )
        .unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let c = checks.iter().find(|c| c.name == "orchestrator").unwrap();
        assert!(c.ok);
        assert!(c.detail.contains("chat-boss"), "{}", c.detail);
        // gh#165: what it receives is the question, and on a default board the
        // answer is "what nobody else could be told" — not the whole board.
        assert!(c.detail.contains("last resort"), "{}", c.detail);
        assert!(c.detail.contains("is not repeated here"), "{}", c.detail);
    }

    /// With the dispatcher wake off, the pin really is the whole board again —
    /// and an operator who turned it off should read that here rather than
    /// discover it as volume in the pinned chat.
    #[test]
    fn the_pin_says_when_it_is_taking_the_whole_board() {
        let (_d, p) = tmp();
        std::fs::write(
            p.routing(),
            "[defaults]\norchestrator_chat = \"chat-boss\"\nnotify_dispatcher = false\n\n\
             [github]\nrepos = []\n",
        )
        .unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let c = checks.iter().find(|c| c.name == "orchestrator").unwrap();
        assert!(c.ok);
        assert!(c.detail.contains("gets the whole board"), "{}", c.detail);
    }

    /// The one misconfiguration the pin allows, and it is a quiet one: a
    /// board-dispatched chat pinned as the orchestrator holds a workspace slot
    /// and — since gh#104 — never hits its own time cap either.
    #[test]
    fn doctor_refuses_a_pin_on_a_chat_the_board_dispatched() {
        let (_d, p) = tmp();
        std::fs::write(
            p.routing(),
            "[defaults]\norchestrator_chat = \"chat-9\"\n\n[github]\nrepos = []\n",
        )
        .unwrap();
        let db = Db::open(&p.db()).unwrap();
        db.upsert_task(&crate::db::UpsertTask {
            id: "linear:LIN-142".into(),
            source: crate::model::Source::Linear,
            source_id: "uuid-1".into(),
            identifier: "LIN-142".into(),
            title: "Add retry".into(),
            body: None,
            url: "https://linear.app/x".into(),
            labels: vec![],
            source_state: None,
            upstream: crate::model::UpstreamState::Started,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let a = db
            .insert_attempt(&crate::db::NewAttempt {
                automation: None,
                automation_owner: None,
                stacked_on: None,
                task_id: "linear:LIN-142".into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: None,
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: None,
                account: None,
                repo_path: None,
                dispatched_by_device: None,
                dispatched_by_user: None,
                dispatched_by_verified: false,
                billed_to: None,
            })
            .unwrap();
        db.set_attempt_pane(a, "chat-9").unwrap();
        drop(db);

        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        let c = checks.iter().find(|c| c.name == "orchestrator").unwrap();
        assert!(!c.ok, "{}", c.detail);
        assert!(c.detail.contains("linear:LIN-142"), "{}", c.detail);
    }

    /// AGE-23, inherited. A global `ON` is not an answer once the answer
    /// differs per repo: the operator's question is whether *Tally* is on the
    /// list, without opening routing.toml.
    #[test]
    fn doctor_names_the_repos_it_will_write_to() {
        let cfg: RoutingConfig = toml::from_str(
            "[github]\nrepos = [\"bredebjorhovd/OIOS\", \"Florin-AS/Tally\"]\n\
             writeback = true\n\n\
             [[github.repo]]\nname = \"Florin-AS/Tally\"\nwriteback = false\n",
        )
        .unwrap();
        let detail = writeback_detail(&cfg.github);
        assert!(detail.contains("bredebjorhovd/OIOS"), "{detail}");
        assert!(
            detail.contains("Read-only: Florin-AS/Tally"),
            "the repo it will not write to has to be named as such: {detail}"
        );
    }

    #[test]
    fn doctor_still_names_them_when_every_repo_answers_the_same_way() {
        // Both uniform cases still list names — "ON" and "off" were the two
        // answers that stopped being enough.
        let on: RoutingConfig =
            toml::from_str("[github]\nrepos = [\"o/a\", \"o/b\"]\nwriteback = true\n").unwrap();
        let detail = writeback_detail(&on.github);
        assert!(detail.contains("o/a") && detail.contains("o/b"), "{detail}");

        let off: RoutingConfig = toml::from_str("[github]\nrepos = [\"o/a\"]\n").unwrap();
        let detail = writeback_detail(&off.github);
        assert!(detail.contains("off for every repo"), "{detail}");
        assert!(detail.contains("o/a"), "{detail}");

        // And with nothing configured it says so rather than listing nothing.
        let none: RoutingConfig = toml::from_str("[github]\nrepos = []\n").unwrap();
        assert!(writeback_detail(&none.github).contains("no repos"));
    }

    /// gh#471: the report as a whole says nothing about Linear. The check
    /// lines it used to print (`LINEAR_API_KEY`, `linear review state`) are
    /// gone, not "not configured" — a removed connector must not read as an
    /// unconfigured one.
    #[test]
    fn the_report_says_nothing_about_linear() {
        let (_d, p) = tmp();
        std::fs::write(
            p.routing(),
            "[[route]]\nmatch = { gh_repo = \"o/r\" }\nworkspace = \"w\"\n\
             repo = \"/tmp\"\nruntime = \"claude-code\"\n",
        )
        .unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        for c in &checks {
            assert!(
                !c.name.to_lowercase().contains("linear"),
                "no check line mentions Linear: {}",
                c.name
            );
        }
    }

    // ── github auth (gh#58) ─────────────────────────────────────────────────

    /// A `.env` naming an App, plus the PEM file it points at (contents
    /// irrelevant — the App itself is built from the fake below).
    fn app_env(p: &Paths, mode: u32) -> std::path::PathBuf {
        let pem = p.config_dir.join("app.pem");
        std::fs::write(&pem, "not really a key").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&pem, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        std::fs::write(
            p.env_file(),
            format!(
                "GITHUB_APP_ID=123456\nGITHUB_APP_PRIVATE_KEY_PATH={}\n",
                pem.display()
            ),
        )
        .unwrap();
        pem
    }

    /// A REST client whose credential is a fake App. The checks read the
    /// provider and never the wire, so the transport is one that refuses to be
    /// called.
    fn rest_on(app: std::rc::Rc<crate::sources::github_app::AppAuth>) -> HttpRest {
        HttpRest::over(Box::new(NoWire), TokenProvider::App(app))
    }

    /// The disk report exists to make gh#72's leak visible before it is
    /// terminal, so it has to say all three things: what is there, what the
    /// board still tracks, and what will ever remove it.
    #[test]
    fn the_worktree_check_counts_the_disk_and_names_the_retention() {
        let (_d, p) = tmp();
        let root = _d.path().join("worktrees");
        for name in ["board-gh-7-widget", "board-lin-1-widget"] {
            let dir = root.join("widget").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("big"), vec![b'x'; 2048]).unwrap();
        }
        std::fs::write(p.routing(), "[defaults]\nretain_worktrees = \"7d\"\n").unwrap();

        let check = worktrees_check(&p, &gc::usage(&root), &root, None);
        assert!(check.ok, "two checkouts is not a problem: {}", check.detail);
        assert!(check.detail.contains("2 checkout(s)"), "{}", check.detail);
        assert!(check.detail.contains("4.0 KiB"), "{}", check.detail);
        assert!(check.detail.contains("7d"), "{}", check.detail);
    }

    /// Turning collection off is a choice; the check is where its cost shows.
    #[test]
    fn retention_off_is_said_out_loud() {
        let (_d, p) = tmp();
        std::fs::write(p.routing(), "[defaults]\nretain_worktrees = \"off\"\n").unwrap();
        let root = _d.path().join("nothing-here");
        let check = worktrees_check(&p, &gc::usage(&root), &root, None);
        assert!(check.detail.contains("ever collected"), "{}", check.detail);
        // An empty root is still not a failure — nothing has been dispatched.
        assert!(check.ok);
    }

    /// The line gh#186 is about. The box read `8 checkout(s), 109.5 GiB` and
    /// could not tell from it that 99.96% was regenerable — so both halves are
    /// named, and the cache's own window is quoted beside its own number.
    #[test]
    fn the_two_reports_separate_the_checkout_from_its_build_output() {
        let (_d, p) = tmp();
        let root = _d.path().join("worktrees");
        let checkout = root.join("widget").join("board-gh-7-widget");
        std::fs::create_dir_all(checkout.join("src")).unwrap();
        std::fs::write(checkout.join("src").join("main.rs"), vec![b'x'; 1024]).unwrap();
        std::fs::create_dir_all(checkout.join("target").join("debug")).unwrap();
        std::fs::write(
            checkout.join("target").join("debug").join("bin"),
            vec![b'x'; 8192],
        )
        .unwrap();
        std::fs::write(p.routing(), "[defaults]\nretain_worktrees = \"7d\"\n").unwrap();

        let usage = gc::usage(&root);
        let worktrees = worktrees_check(&p, &usage, &root, None);
        // The total is still there — the disk is the total — but the split is on
        // the same line, so nobody reaches for `retain_worktrees` over 8 KiB of
        // `target/` again.
        assert!(worktrees.detail.contains("9.0 KiB"), "{}", worktrees.detail);
        assert!(
            worktrees.detail.contains("1.0 KiB of checkout"),
            "{}",
            worktrees.detail
        );

        let build = build_output_check(&p, &usage, None);
        assert!(build.ok);
        assert!(build.detail.contains("8.0 KiB"), "{}", build.detail);
        assert!(
            build.detail.contains("1 build-output directory"),
            "{}",
            build.detail
        );
        assert!(build.detail.contains("target"), "{}", build.detail);
        // The default window, spelled as the rule and not as `0s`.
        assert!(
            build.detail.contains("swept as each attempt ends"),
            "{}",
            build.detail
        );
    }

    /// `retain_build_output = off` is what the board did before gh#186, and the
    /// only state where the build-output line is a failure rather than a number:
    /// a box mid-build has tens of gibibytes of `target/` and is working.
    #[test]
    fn build_output_kept_forever_is_a_failure_only_once_it_is_large() {
        let (_d, p) = tmp();
        std::fs::write(p.routing(), "[defaults]\nretain_build_output = \"off\"\n").unwrap();

        let small = gc::Usage {
            checkouts: 1,
            bytes: 1024,
            cache_bytes: 512,
            cache_dirs: 1,
            truncated: false,
        };
        let check = build_output_check(&p, &small, None);
        assert!(check.ok, "{}", check.detail);
        assert!(
            check.detail.contains("retain_build_output = off"),
            "{}",
            check.detail
        );

        let full = gc::Usage {
            cache_bytes: gc::WARN_BYTES,
            bytes: gc::WARN_BYTES,
            ..small
        };
        assert!(!build_output_check(&p, &full, None).ok);
        // …and the same weight with a sweep behind it is not a fault at all.
        std::fs::write(p.routing(), "[defaults]\nretain_build_output = \"2h\"\n").unwrap();
        let check = build_output_check(&p, &full, None);
        assert!(check.ok, "{}", check.detail);
        assert!(
            check.detail.contains("2h after each attempt ends"),
            "{}",
            check.detail
        );
    }

    /// A worktree root that is mostly `target/` must not fail the *checkout*
    /// line: that verdict is `retain_worktrees`'s, the bytes it names are 14 MB
    /// per checkout, and a red line over a running build is a red line nobody
    /// reads twice.
    #[test]
    fn a_heavy_cache_does_not_fail_the_checkout_line() {
        let (_d, p) = tmp();
        std::fs::write(p.routing(), "[defaults]\nretain_worktrees = \"7d\"\n").unwrap();
        let root = _d.path().join("worktrees");
        let mostly_cache = gc::Usage {
            checkouts: 3,
            bytes: gc::WARN_BYTES * 5,
            cache_bytes: gc::WARN_BYTES * 5 - 1024,
            cache_dirs: 3,
            truncated: false,
        };
        assert!(worktrees_check(&p, &mostly_cache, &root, None).ok);
        // The checkouts themselves crossing the line still fails it.
        let heavy = gc::Usage {
            cache_bytes: 0,
            ..mostly_cache
        };
        assert!(!worktrees_check(&p, &heavy, &root, None).ok);
    }

    /// The shelf's half of the same report (gh#139): the window, and the fact
    /// that routes may answer differently — quoting one number for a per-route
    /// setting would be quoting it about the wrong route.
    #[test]
    fn the_chat_check_names_the_window_and_the_routes_that_differ() {
        let (_d, p) = tmp();
        std::fs::write(
            p.routing(),
            "[defaults]\narchive_chats = \"14d\"\n\n\
             [[route]]\nmatch = { label = \"keep\" }\nworkspace = \"w\"\n\
             repo = \"/tmp\"\nruntime = \"claude\"\narchive_chats = \"off\"\n",
        )
        .unwrap();
        let check = chats_check(&p, None);
        assert!(check.ok);
        assert!(check.detail.contains("14d"), "{}", check.detail);
        assert!(check.detail.contains("1 route(s)"), "{}", check.detail);
    }

    /// Keeping every chat forever is a choice; the check is where its cost
    /// shows — and it is a cost, never a failure.
    #[test]
    fn chat_archiving_off_is_said_out_loud() {
        let (_d, p) = tmp();
        std::fs::write(p.routing(), "[defaults]\narchive_chats = \"off\"\n").unwrap();
        let check = chats_check(&p, None);
        assert!(check.detail.contains("forever"), "{}", check.detail);
        assert!(check.ok);
    }

    #[test]
    fn doctor_names_the_app_its_slug_and_who_installed_it() {
        // "GITHUB_TOKEN present" answered the only question there used to be.
        // With an App the questions are which identity the board writes as and
        // whose repos it was granted — and the second is set by the installer,
        // not by anything in this config directory.
        let (_d, p) = tmp();
        app_env(&p, 0o600);
        let rest = rest_on(test_app(&[("o/r", 42)]).0);
        let checks = github_auth_checks(&p, &["o/r".to_string()], Ok(&rest));
        let auth = checks.iter().find(|c| c.name == "github auth").unwrap();
        assert!(auth.detail.contains("app 123456"), "{}", auth.detail);
        // Slug and installations come off the App endpoints under the JWT.
        assert!(auth.detail.contains('@'), "{}", auth.detail);
    }

    #[test]
    fn a_world_readable_private_key_fails_its_own_check() {
        // The private key *is* the App. Since #55 the box can have several
        // people on it, and a 0644 PEM is a credential all of them hold.
        let (_d, p) = tmp();
        app_env(&p, 0o644);
        let rest = rest_on(test_app(&[]).0);
        let checks = github_auth_checks(&p, &[], Ok(&rest));
        let key = checks.iter().find(|c| c.name == "github app key").unwrap();
        assert!(!key.ok, "{}", key.detail);
        assert!(key.detail.contains("chmod 600"), "{}", key.detail);
    }

    #[test]
    fn half_an_app_is_reported_rather_than_silently_ignored() {
        let (_d, p) = tmp();
        std::fs::write(p.env_file(), "GITHUB_TOKEN=ghp_x\nGITHUB_APP_ID=123456\n").unwrap();
        let checks = github_auth_checks(&p, &["o/r".to_string()], Err("no client".into()));
        let half = checks.iter().find(|c| c.name == "github app").unwrap();
        assert!(!half.ok, "{}", half.detail);
        assert!(
            half.detail.contains("GITHUB_APP_PRIVATE_KEY_PATH"),
            "{}",
            half.detail
        );
        // And the board is still running, on the token — said out loud.
        let auth = checks.iter().find(|c| c.name == "github auth").unwrap();
        assert!(auth.ok, "{}", auth.detail);
        assert!(auth.detail.contains("GITHUB_TOKEN"), "{}", auth.detail);
    }

    #[test]
    fn a_missing_credential_only_fails_once_repos_are_configured() {
        let (_d, p) = tmp();
        std::fs::write(p.env_file(), "").unwrap();
        let none = github_auth_checks(&p, &[], Err("no client".into()));
        assert!(none.iter().find(|c| c.name == "github auth").unwrap().ok);

        let some = github_auth_checks(&p, &["o/r".to_string()], Err("no client".into()));
        let auth = some.iter().find(|c| c.name == "github auth").unwrap();
        assert!(!auth.ok, "{}", auth.detail);
        assert!(auth.detail.contains("GITHUB_APP_ID"), "{}", auth.detail);
    }

    #[test]
    fn the_token_report_names_the_installation_and_how_long_it_has_left() {
        let (app, _api, _clock) = test_app(&[("o/a", 42), ("o/b", 42)]);
        // Two repos, one installation: what the board would hold after a cycle.
        app.token_for_repo("o/a").unwrap();
        app.token_for_repo("o/b").unwrap();
        let rest = rest_on(app);
        let checks = app_token_checks(&["o/a".into(), "o/b".into()], Some(&rest));
        assert_eq!(checks.len(), 2);
        for c in &checks {
            assert!(c.ok, "{}", c.detail);
            assert!(c.detail.contains("installation 42"), "{}", c.detail);
            assert!(c.detail.contains("expires in 60m"), "{}", c.detail);
        }
    }

    #[test]
    fn a_repo_the_app_never_reached_is_reported_as_having_no_token() {
        let (app, _, _) = test_app(&[("o/a", 42)]);
        let rest = rest_on(app);
        let checks = app_token_checks(&["o/unreachable".into()], Some(&rest));
        assert!(!checks[0].ok, "{}", checks[0].detail);
        assert!(
            checks[0].detail.contains("not installed"),
            "{}",
            checks[0].detail
        );
    }

    #[test]
    fn a_personal_access_token_has_no_token_checks_to_report() {
        let rest = HttpRest::new(Some("ghp_x".into())).unwrap();
        assert!(app_token_checks(&["o/r".into()], Some(&rest)).is_empty());
    }

    #[test]
    fn workflow_push_reporting_distinguishes_the_apps_two_permissions() {
        let missing = push_capability_check(
            "o/r",
            Ok(PushCapabilities {
                contents: WriteCapability::Write,
                workflows: WriteCapability::Missing,
                evidence: CapabilityEvidence::AppInstallation,
            }),
        );
        assert!(!missing.ok, "{}", missing.detail);
        assert!(missing.detail.contains("repository contents: write"));
        assert!(missing.detail.contains("workflow files: missing"));
        assert!(missing.detail.contains("Workflows: Read and write"));
        assert!(missing.detail.contains("approve the updated permission"));

        let ready = push_capability_check(
            "o/r",
            Ok(PushCapabilities {
                contents: WriteCapability::Write,
                workflows: WriteCapability::Write,
                evidence: CapabilityEvidence::AppInstallation,
            }),
        );
        assert!(ready.ok, "{}", ready.detail);
        assert_eq!(
            ready.detail,
            "repository contents: write · workflow files: write"
        );
    }

    #[test]
    fn classic_pat_reporting_names_the_missing_workflow_scope() {
        let check = push_capability_check(
            "o/r",
            Ok(PushCapabilities {
                contents: WriteCapability::Write,
                workflows: WriteCapability::Missing,
                evidence: CapabilityEvidence::ClassicOauthScopes,
            }),
        );
        assert!(!check.ok, "{}", check.detail);
        assert!(check.detail.contains("classic `workflow` scope"));
        assert!(check.detail.contains("repository scope"));
    }

    #[test]
    fn classic_pat_reporting_names_missing_repository_push_access() {
        let check = push_capability_check(
            "o/r",
            Ok(PushCapabilities {
                contents: WriteCapability::Missing,
                workflows: WriteCapability::Missing,
                evidence: CapabilityEvidence::ClassicOauthScopes,
            }),
        );
        assert!(!check.ok, "{}", check.detail);
        assert!(check.detail.contains("token holder has push access"));
        assert!(check.detail.contains("`repo`"));
    }

    #[test]
    fn opaque_fine_grained_token_evidence_fails_closed() {
        let check = push_capability_check(
            "o/r",
            Ok(PushCapabilities {
                contents: WriteCapability::Unknown,
                workflows: WriteCapability::Unknown,
                evidence: CapabilityEvidence::OpaqueToken,
            }),
        );
        assert!(!check.ok, "{}", check.detail);
        assert!(check.detail.contains("fine-grained/unknown token"));
        assert!(check.detail.contains("fails closed"));
        assert!(check.detail.contains("`repo` + `workflow`"));
    }

    #[test]
    fn no_credential_push_reporting_is_explicit_and_actionable() {
        let check = push_capability_check("o/r", Ok(PushCapabilities::anonymous()));
        assert!(!check.ok, "{}", check.detail);
        assert!(check.detail.contains("repository contents: missing"));
        assert!(check.detail.contains("workflow files: missing"));
        assert!(check.detail.contains("GITHUB_APP_PRIVATE_KEY_PATH"));
    }

    // ── git identity (gh#107) ───────────────────────────────────────────────

    /// The one state worth failing: a box with no identity at all. Git does not
    /// stop there — it invents one — so the first dispatched agent commits
    /// under an address belonging to no account, and a deploy gate that checks
    /// the author refuses the push. A fresh box has to refuse to look anonymous.
    #[test]
    fn a_box_with_no_git_identity_fails_and_says_what_to_type() {
        let c = git_identity_check(&git_identity::BoxIdentity::default());
        assert!(!c.ok, "{}", c.detail);
        assert!(
            c.detail.contains("no user.name, no user.email"),
            "{}",
            c.detail
        );
        assert!(
            c.detail.contains("git config --global user.email"),
            "{}",
            c.detail
        );
        assert!(
            c.detail.contains("users.noreply.github.com"),
            "{}",
            c.detail
        );

        // Half of one is still anonymous, and says which half is missing.
        let half = git_identity::BoxIdentity {
            name: Some("The Box".into()),
            email: None,
        };
        let c = git_identity_check(&half);
        assert!(!c.ok, "{}", c.detail);
        assert!(c.detail.contains("no user.email"), "{}", c.detail);
    }

    /// A noreply address is attributable by construction — GitHub minted it —
    /// so the line names the account rather than offering advice.
    #[test]
    fn a_noreply_identity_passes_and_names_the_account_it_attributes_to() {
        let c = git_identity_check(&git_identity::BoxIdentity {
            name: Some("The Box".into()),
            email: Some("22494697+ana@users.noreply.github.com".into()),
        });
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("@ana"), "{}", c.detail);
    }

    /// The case `doctor` cannot decide: whether an ordinary address is on that
    /// account's verified list is a `GET /user/emails` away, and the board's App
    /// may not make that call. Guidance, not a failure — failing every operator
    /// who uses their real work address is the gh#96 false alarm again.
    #[test]
    fn an_address_that_might_not_be_linked_is_guidance_rather_than_a_failure() {
        let c = git_identity_check(&git_identity::BoxIdentity {
            name: Some("Ana Ruiz".into()),
            email: Some("ana@example.com".into()),
        });
        assert!(c.ok, "a preference is not a failure: {}", c.detail);
        assert!(
            c.detail.contains("not a GitHub noreply address"),
            "{}",
            c.detail
        );
        assert!(c.detail.contains("nothing here can check"), "{}", c.detail);
    }

    /// With no `[users]` map every dispatch commits as the box — which looks
    /// exactly like a map that is working, right up until somebody reads the
    /// commit list. Said out loud, in both states, like the duration cap.
    #[test]
    fn doctor_says_whose_name_a_teammates_dispatch_commits_under() {
        let empty = dispatch_authorship_check(&RoutingConfig::default(), None);
        assert!(empty.ok, "{}", empty.detail);
        assert!(
            empty.detail.starts_with("no `[users]` map"),
            "{}",
            empty.detail
        );
        // The line points at the verb that fixes it, rather than at TOML to
        // hand-write (gh#162) — the whole reason onboarding was oral tradition.
        assert!(
            empty.detail.contains("comet-board member add"),
            "{}",
            empty.detail
        );
        assert!(
            empty.detail.contains("docs/teammate.md"),
            "{}",
            empty.detail
        );

        let cfg: RoutingConfig = toml::from_str(
            "[users]\n\"ana@example.com\" = \"22494697+ana@users.noreply.github.com\"\n\
             \"sam@example.com\" = \"Sam Ito <sam@corp.example>\"\n\
             \"kim@example.com\" = \"kim\"\n",
        )
        .unwrap();
        let c = dispatch_authorship_check(&cfg, None);
        assert!(c.ok, "{}", c.detail);
        // Every entry named, with what it resolves to: an address for the wrong
        // account commits and pushes exactly as happily as the right one.
        assert!(
            c.detail
                .contains("ana@example.com → ana <22494697+ana@users.noreply.github.com>"),
            "{}",
            c.detail
        );
        assert!(
            c.detail
                .contains("sam@example.com → Sam Ito <sam@corp.example>"),
            "{}",
            c.detail
        );
        // The two weaker forms are called out rather than passed over.
        assert!(
            c.detail
                .contains("Not a GitHub noreply address for sam@example.com"),
            "{}",
            c.detail
        );
        assert!(
            c.detail.contains("Not an address at all: kim@example.com"),
            "{}",
            c.detail
        );
        // And the config itself refuses to load with that last one in it.
        assert!(
            cfg.problems()
                .iter()
                .any(|p| p.contains("[users] \"kim@example.com\"")),
            "{:?}",
            cfg.problems()
        );
    }

    /// The pairing nobody thinks about (gh#162): mapped, so their commits are
    /// theirs, and no login of their own, so their runs spend somebody else's
    /// subscription. Two facts in two files, and until this line they were
    /// never printed together.
    #[test]
    fn doctor_names_a_mapped_teammate_with_no_agent_account() {
        let cfg: RoutingConfig = toml::from_str(
            "[users]\n\"ana@example.com\" = \"1+ana@users.noreply.github.com\"\n\
             \"sam@example.com\" = \"2+sam@users.noreply.github.com\"\n",
        )
        .unwrap();
        let accounts = [account(
            "slot-ana",
            "ana@example.com",
            comet_proto::HarnessId::ClaudeCode,
        )];
        let c = dispatch_authorship_check(&cfg, Some(&accounts));
        assert!(c.ok, "{}", c.detail);
        assert!(
            c.detail
                .contains("No agent account of their own for sam@example.com"),
            "{}",
            c.detail
        );
        // Ana has one; naming her too would make the line noise on a box where
        // everybody is set up.
        assert!(
            !c.detail.contains("for ana@example.com, sam@example.com"),
            "{}",
            c.detail
        );

        // An engine that could not be asked says nothing about the pairing,
        // rather than accusing everybody of missing a slot (gh#155).
        let unknown = dispatch_authorship_check(&cfg, None);
        assert!(
            !unknown.detail.contains("No agent account"),
            "{}",
            unknown.detail
        );
    }

    // ── who opens, who reviews (gh#369) ─────────────────────────────────────

    fn mapped(users: &[(&str, &str)]) -> RoutingConfig {
        let mut cfg = RoutingConfig::default();
        for (user, author) in users {
            cfg.users.insert(user.to_string(), author.to_string());
        }
        cfg
    }

    /// The arrangement the split asks for: a bot opens, and the people who
    /// review hold credentials of their own. Nothing to fix, and the line still
    /// says which members review as the board — that is the difference between
    /// an approval and a comment that says it approves.
    #[test]
    fn doctor_says_who_opens_a_pull_request_and_who_can_really_approve_it() {
        let cfg = mapped(&[
            ("ana@example.com", "22494697+ana@users.noreply.github.com"),
            ("sam@example.com", "8134+samito@users.noreply.github.com"),
        ]);
        let c = review_identity_check(
            &cfg,
            &Credentials::with_user_token("ana", "ghu_ana"),
            Opener::App,
        );
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("the board's App opens"), "{}", c.detail);
        assert!(
            c.detail
                .contains("@ana casts verdicts under their own name"),
            "{}",
            c.detail
        );
        assert!(
            c.detail.contains("@samito reviews as the board"),
            "{}",
            c.detail
        );
        // And what to set for him, spelled exactly as the file wants it.
        assert!(
            c.detail.contains("GITHUB_USER_TOKEN_SAMITO"),
            "{}",
            c.detail
        );
    }

    /// The failure gh#369 is about, on the machine that had it: one person
    /// opens every dispatched pull request and is also the only person who
    /// reviews them. GitHub refuses that approval, always, and no amount of
    /// member tokens fixes it — the fix is a bot on the opening side.
    #[test]
    fn doctor_fails_when_one_account_opens_the_pull_request_and_reviews_it() {
        let cfg = mapped(&[(
            "brede@tally.no",
            "22494697+bredebjorhovd@users.noreply.github.com",
        )]);
        let c = review_identity_check(
            &cfg,
            &Credentials::with_user_token("bredebjorhovd", "ghu_brede"),
            Opener::Person(Some("bredebjorhovd".into())),
        );
        assert!(!c.ok, "{}", c.detail);
        assert!(
            c.detail.contains("@bredebjorhovd both opens and reviews"),
            "{}",
            c.detail
        );
        assert!(c.detail.contains("GITHUB_APP_ID"), "{}", c.detail);
    }

    /// The same collision reached the other way: the board's own token *is* the
    /// reviewer's. Two variables, one account, and GitHub reads the account.
    #[test]
    fn doctor_fails_when_the_boards_token_is_also_a_members_review_token() {
        let cfg = mapped(&[("ana@example.com", "1+ana@users.noreply.github.com")]);
        let mut credentials = Credentials::with_user_token("ana", "ghp_shared");
        credentials.github_token = Some("ghp_shared".into());
        let c = review_identity_check(&cfg, &credentials, Opener::Person(None));
        assert!(!c.ok, "{}", c.detail);
        assert!(
            c.detail
                .contains("@ana's review token is the board's own GITHUB_TOKEN"),
            "{}",
            c.detail
        );
    }

    /// Every sentence this check can print, read as a person reads it (gh#369).
    ///
    /// A Rust string literal wrapped across source lines keeps the newline
    /// *and* the source indentation unless the line ends in `\`, and the first
    /// cut of this check shipped six that did not — twelve to thirty-four
    /// literal spaces, mid-sentence, in the one report somebody reads when they
    /// have already lost an hour to a 422 that explains nothing. `cargo fmt`
    /// does not look inside a string and clippy has no opinion about one, so
    /// the guard has to be a test, and it has to cover every branch rather than
    /// the branch a fixture happens to take.
    #[test]
    fn every_sentence_the_review_identity_line_can_print_reads_as_prose() {
        let mapped_and_credentialled = mapped(&[
            ("ana@example.com", "1+ana@users.noreply.github.com"),
            ("sam@example.com", "2+samito@users.noreply.github.com"),
        ]);
        let mut shared = Credentials::with_user_token("ana", "ghp_shared");
        shared.github_token = Some("ghp_shared".into());
        let cases = [
            // Every opener, and every state a member can be in: with a token,
            // without one, colliding with the opener, and no map at all.
            (&mapped_and_credentialled, &shared, Opener::App),
            (
                &mapped_and_credentialled,
                &shared,
                Opener::Person(Some("ana".into())),
            ),
            (&mapped_and_credentialled, &shared, Opener::Person(None)),
            (&mapped_and_credentialled, &shared, Opener::BoxUser),
            (
                &RoutingConfig::default(),
                &Credentials::default(),
                Opener::BoxUser,
            ),
        ];
        for (cfg, credentials, opener) in cases {
            let c = review_identity_check(cfg, credentials, opener.clone());
            assert!(
                !c.detail.contains("  ") && !c.detail.contains('\n'),
                "under {opener:?} the line carries its own source layout:\n{}",
                c.detail
            );
        }
    }

    /// A board with no map is not broken — it is gh#365's arrangement, and the
    /// line says what it costs and what would change it.
    #[test]
    fn doctor_says_a_board_with_no_map_reviews_as_itself() {
        let c = review_identity_check(
            &RoutingConfig::default(),
            &Credentials::default(),
            Opener::BoxUser,
        );
        assert!(c.ok, "{}", c.detail);
        assert!(
            c.detail.contains("this box's own git credentials"),
            "{}",
            c.detail
        );
        assert!(
            c.detail.contains("GITHUB_USER_TOKEN_<LOGIN>"),
            "{}",
            c.detail
        );
    }

    // ── the agent skill (gh#133) ────────────────────────────────────────────

    #[test]
    fn a_missing_skill_fails_and_names_the_one_command_that_fixes_it() {
        let dir = tempfile::tempdir().unwrap();
        let c = agent_skill_check(dir.path(), &[]);
        assert!(!c.ok);
        assert!(
            c.detail.contains("comet-board skill install"),
            "{}",
            c.detail
        );
        // No slots configured is not a sentence about slots.
        assert!(!c.detail.contains("slot"), "{}", c.detail);
    }

    #[test]
    fn a_current_skill_passes_and_says_which_version() {
        let dir = tempfile::tempdir().unwrap();
        skill::install_into(dir.path()).unwrap();
        let c = agent_skill_check(dir.path(), &[]);
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains(&format!("v{}", skill::VERSION)));
    }

    #[test]
    fn a_stale_skill_fails_naming_both_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = skill::path_in(dir.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "old\n<!-- comet-board skill 0.0.1 -->\n").unwrap();
        let c = agent_skill_check(dir.path(), &[]);
        assert!(!c.ok);
        assert!(c.detail.contains("v0.0.1"), "{}", c.detail);
        assert!(
            c.detail.contains(&format!("ships v{}", skill::VERSION)),
            "{}",
            c.detail
        );
    }

    /// The rule the doc comment states: a slot behind the binary is reported,
    /// never failed, because the next dispatch re-stamps it. Failing here would
    /// turn every version bump into a red doctor over a self-healing file.
    #[test]
    fn a_stale_slot_is_reported_but_does_not_fail_the_check() {
        let user = tempfile::tempdir().unwrap();
        skill::install_into(user.path()).unwrap();
        let good = tempfile::tempdir().unwrap();
        skill::install_into(good.path()).unwrap();
        let bad = tempfile::tempdir().unwrap();

        let slots = [good.path().to_path_buf(), bad.path().to_path_buf()];
        let c = agent_skill_check(user.path(), &slots);
        assert!(c.ok, "{}", c.detail);
        assert!(
            c.detail.contains("1 of 2 agent-account slot(s) behind"),
            "{}",
            c.detail
        );

        let c = agent_skill_check(user.path(), &slots[..1]);
        assert!(
            c.detail.contains("1 agent-account slot(s), all current"),
            "{}",
            c.detail
        );
    }

    /// gh#272: a missing block is a fact, not a fault — the next dispatch on a
    /// route that wants one writes it, and a board with them turned off should
    /// have none at all. The one thing that does not fix itself is a file this
    /// refuses to touch.
    #[test]
    fn instruction_files_are_reported_and_only_a_broken_one_fails() {
        let written = tempfile::tempdir().unwrap();
        conventions::install_into(written.path(), HarnessId::Codex).unwrap();
        let empty = tempfile::tempdir().unwrap();
        let dirs = [
            (HarnessId::Codex, written.path().to_path_buf()),
            (HarnessId::ClaudeCode, empty.path().to_path_buf()),
        ];

        let c = agent_instructions_check(&dirs, Some((true, 2, 3)));
        assert!(c.ok, "{}", c.detail);
        assert!(
            c.detail.contains("1 of 2 instruction file(s)"),
            "{}",
            c.detail
        );
        assert!(c.detail.contains("1 without a block"), "{}", c.detail);
        assert!(c.detail.contains("on for 2 of 3 route(s)"), "{}", c.detail);

        // A board with no routes at all still says what it would do.
        let c = agent_instructions_check(&dirs, Some((false, 0, 0)));
        assert!(c.detail.contains("off by default"), "{}", c.detail);

        // Half a marker pair: nothing will write over it, so somebody has to.
        std::fs::write(
            conventions::path_in(empty.path(), HarnessId::ClaudeCode).unwrap(),
            format!("{}\nhalf a block\n", conventions::BEGIN),
        )
        .unwrap();
        let c = agent_instructions_check(&dirs, None);
        assert!(!c.ok);
        assert!(c.detail.contains("CLAUDE.md"), "{}", c.detail);
    }

    /// gh#287: the extension is box-level, and whether this box wants it is not
    /// written anywhere — so the line reports, and never reddens a report over
    /// a capability nothing here may ever ask for. The three answers are
    /// distinct on purpose: a box that has it, a box that has not, and a shell
    /// that could not ask.
    #[test]
    fn the_gh_stack_line_reports_and_never_fails() {
        let installed = gh_stack_check(
            Some("gh stack\tgithub/gh-stack\tv0.1.0\ngh co\tgithub/gh-co\tv1\n".into()),
            0,
        );
        assert!(installed.ok);
        assert!(
            installed.detail.starts_with("installed"),
            "{}",
            installed.detail
        );

        let missing = gh_stack_check(Some("gh co\tgithub/gh-co\tv1\n".into()), 0);
        assert!(missing.ok, "an absent extension is not a broken board");
        assert!(
            missing
                .detail
                .contains("gh extension install github/gh-stack"),
            "{}",
            missing.detail
        );

        let no_gh = gh_stack_check(None, 0);
        assert!(no_gh.ok);
        assert!(no_gh.detail.contains("not checked"), "{}", no_gh.detail);
    }

    /// gh#335: the same non-failure, saying something else. A box holding
    /// stacked pull requests and missing the extension is not the same news as
    /// a box that has never stacked, and the line an operator reads should not
    /// be. The count is the only durable evidence there is — `routing.toml` has
    /// no stacking flag to consult.
    #[test]
    fn a_board_that_already_stacks_gets_a_different_missing_line() {
        let missing = gh_stack_check(Some("gh co\tgithub/gh-co\tv1\n".into()), 4);
        assert!(
            missing.ok,
            "still not a broken board — the agent installs it itself"
        );
        assert!(
            missing.detail.contains("4 stacked pull requests"),
            "{}",
            missing.detail
        );
        assert!(
            missing.detail.contains("gh stack view"),
            "the operator's own half of the cost is what is new here: {}",
            missing.detail
        );

        // One is one, and reads like it.
        let one = gh_stack_check(Some("gh co\tgithub/gh-co\tv1\n".into()), 1);
        assert!(
            one.detail.contains("1 stacked pull request —"),
            "{}",
            one.detail
        );

        // Installed is installed, however much this board has stacked.
        let installed = gh_stack_check(Some("gh stack\tgithub/gh-stack\tv0.1.0\n".into()), 4);
        assert!(
            installed.detail.starts_with("installed"),
            "{}",
            installed.detail
        );
    }

    // ---- is the box still able to run anything (gh#390)? -----------------

    /// One attempt that started and ended without producing anything — a run
    /// that died within seconds of starting, which is what the whole box was
    /// doing on the morning gh#390 describes.
    fn dead_run(db: &Db, task: &str, outcome: crate::model::Outcome) {
        let a = started_run(db, task);
        db.close_attempt(a, outcome).unwrap();
    }

    /// The same, left running.
    fn started_run(db: &Db, task: &str) -> i64 {
        if db.get_task(task).unwrap().is_none() {
            db.upsert_task(&crate::db::UpsertTask {
                id: task.into(),
                source: crate::model::Source::Github,
                source_id: "1".into(),
                identifier: task.into(),
                title: "Something".into(),
                body: None,
                url: "https://github.com/o/r/issues/1".into(),
                labels: vec![],
                source_state: None,
                upstream: crate::model::UpstreamState::Started,
                updated_at: crate::db::now(),
            })
            .unwrap();
        }
        db.insert_attempt(&crate::db::NewAttempt {
            automation: None,
            automation_owner: None,
            stacked_on: None,
            task_id: task.into(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: None,
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
            account: None,
            repo_path: None,
            dispatched_by_device: None,
            dispatched_by_user: None,
            dispatched_by_verified: false,
            billed_to: None,
        })
        .unwrap()
    }

    #[test]
    fn a_board_with_no_history_judges_nothing() {
        let (_d, p) = tmp();
        let db = Db::open(&p.db()).unwrap();
        let c = runs_check(Some(&db), chrono::Utc::now());
        assert!(c.ok);
        assert!(c.detail.contains("nothing to judge"), "{}", c.detail);
    }

    /// The check that was missing: twelve attempts died in minutes across three
    /// harnesses and `doctor` said every check was ok.
    #[test]
    fn doctor_fails_a_box_where_every_run_dies_in_minutes() {
        let (_d, p) = tmp();
        let db = Db::open(&p.db()).unwrap();
        for n in 1..=3 {
            dead_run(&db, &format!("gh:o/r#{n}"), crate::model::Outcome::Orphaned);
        }
        let c = runs_check(Some(&db), chrono::Utc::now());
        assert!(!c.ok, "{}", c.detail);
        assert!(
            c.detail.contains("runs are not surviving on this box"),
            "{}",
            c.detail
        );
    }

    /// Deliberately hard to trip: one attempt that finished anywhere in the
    /// window means runs demonstrably still start here, whatever else is wrong.
    #[test]
    fn one_finished_attempt_clears_the_window() {
        let (_d, p) = tmp();
        let db = Db::open(&p.db()).unwrap();
        for n in 1..=3 {
            dead_run(&db, &format!("gh:o/r#{n}"), crate::model::Outcome::Orphaned);
        }
        dead_run(&db, "gh:o/r#4", crate::model::Outcome::Done);
        let c = runs_check(Some(&db), chrono::Utc::now());
        assert!(c.ok, "{}", c.detail);
        assert!(
            c.detail.contains("runs are starting on this box"),
            "{}",
            c.detail
        );
    }

    /// A live attempt is evidence a run started, not yet evidence about how it
    /// ends — counting one as a young death would fail the box for every
    /// dispatch made in the last five minutes.
    #[test]
    fn a_running_attempt_is_not_counted_as_a_dead_one() {
        let (_d, p) = tmp();
        let db = Db::open(&p.db()).unwrap();
        for n in 1..=3 {
            dead_run(&db, &format!("gh:o/r#{n}"), crate::model::Outcome::Orphaned);
        }
        // …and a fourth that is still going. It is not a death yet, so it
        // neither rescues the box nor damns it.
        started_run(&db, "gh:o/r#4");
        let c = runs_check(Some(&db), chrono::Utc::now());
        assert!(!c.ok);
        assert!(c.detail.contains("of the last 3"), "{}", c.detail);
    }

    // ---- what the box has left (gh#533) ----------------------------------

    fn gib(n: f64) -> u64 {
        (n * 1024.0 * 1024.0 * 1024.0) as u64
    }

    /// The Mylder box on the night of 2026-08-19: 15.6 GiB, no swap, three
    /// heavy builds, 1.2 GiB left.
    fn tight_box() -> crate::pressure::Snapshot {
        crate::pressure::Snapshot {
            memory: Some(crate::pressure::Memory {
                total: gib(15.6),
                available: gib(1.2),
                swap_total: 0,
                swap_free: 0,
            }),
            psi: Some(crate::pressure::Psi {
                some_avg10: 34.2,
                some_avg60: 20.0,
                full_avg10: 8.1,
            }),
            load: Some(crate::pressure::Load {
                one: 18.5,
                five: 17.0,
                fifteen: 16.0,
                cores: 4,
            }),
            oom: Some(crate::pressure::OomCounters {
                cgroup: 4,
                boxwide: 4,
            }),
        }
    }

    /// The reading is what the operator came for after a deferred dispatch:
    /// the numbers, the floor, and which side of it the box is on. Never red —
    /// a box that is momentarily tight is a box that is working.
    #[test]
    fn the_host_memory_line_says_why_a_dispatch_would_defer() {
        let c = host_memory_check(&tight_box(), Some(0.15));
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("1.2 GiB of 15.6 GiB"), "{}", c.detail);
        assert!(c.detail.contains("PSI some"), "{}", c.detail);
        assert!(
            c.detail.contains("a dispatch now would defer"),
            "{}",
            c.detail
        );
    }

    /// The gate turned off has to say so here, or the line reads as a box with
    /// room when nothing is looking at all.
    #[test]
    fn the_host_memory_line_names_a_gate_that_is_off() {
        let c = host_memory_check(&tight_box(), None);
        assert!(
            c.detail.contains("min_memory_headroom = off"),
            "{}",
            c.detail
        );
    }

    /// A box that cannot be measured is described as unmeasured, never as
    /// healthy.
    #[test]
    fn an_unmeasurable_box_is_not_reported_as_a_box_with_room() {
        let c = host_memory_check(&crate::pressure::Snapshot::default(), Some(0.15));
        assert!(c.ok);
        assert!(c.detail.contains("not measurable"), "{}", c.detail);
        let swap = swap_check(&crate::pressure::Snapshot::default());
        assert!(swap.detail.contains("not checked"), "{}", swap.detail);
        let load = load_check(None);
        assert!(load.detail.contains("not checked"), "{}", load.detail);
    }

    /// Swaplessness warns and hands over the whole command — the box where this
    /// fires is a headless VPS being read over ssh.
    #[test]
    fn no_swap_warns_with_the_one_liner_that_fixes_it() {
        let c = swap_check(&tight_box());
        assert!(c.ok, "a deliberate choice is not a failed check");
        assert!(c.detail.starts_with("warn — no swap"), "{}", c.detail);
        assert!(c.detail.contains("mkswap /swapfile"), "{}", c.detail);
        assert!(c.detail.contains("/etc/fstab"), "{}", c.detail);
    }

    #[test]
    fn a_box_with_swap_just_says_how_much() {
        let mut snap = tight_box();
        snap.memory = Some(crate::pressure::Memory {
            swap_total: gib(4.0),
            swap_free: gib(3.5),
            ..snap.memory.unwrap()
        });
        let c = swap_check(&snap);
        assert!(!c.detail.contains("warn"), "{}", c.detail);
        assert!(c.detail.contains("4.0 GiB of swap"), "{}", c.detail);
    }

    /// Sustained, which is the fifteen-minute figure — a one-minute spike is a
    /// build linking.
    #[test]
    fn a_sustained_queue_warns_and_a_busy_afternoon_does_not() {
        let hot = load_check(Some(crate::pressure::Load {
            one: 30.0,
            five: 20.0,
            fifteen: 16.0,
            cores: 4,
        }));
        assert!(hot.ok);
        assert!(hot.detail.starts_with("warn —"), "{}", hot.detail);
        assert!(hot.detail.contains("4.0× per core"), "{}", hot.detail);

        // 1.5× per core sustained is a build box doing its job.
        let busy = load_check(Some(crate::pressure::Load {
            one: 9.0,
            five: 7.0,
            fifteen: 6.0,
            cores: 4,
        }));
        assert!(!busy.detail.contains("warn"), "{}", busy.detail);
    }

    /// The one red line: work that has already been destroyed, with the dates
    /// that make it actionable.
    #[test]
    fn oom_kills_fail_the_report_with_their_timestamps() {
        let journal = crate::pressure::parse_oom_journal(
            "2026-08-19T23:41:02+0200 mylder systemd[1401]: comet-native.service: A process of \
             this unit has been killed by the OOM killer.\n\
             2026-08-20T00:12:44+0200 mylder systemd[1401]: comet-native.service: A process of \
             this unit has been killed by the OOM killer.\n",
        );
        let c = oom_kills_check(
            Some(&journal),
            Some(crate::pressure::OomCounters {
                cgroup: 1,
                boxwide: 1,
            }),
        );
        assert!(!c.ok, "an OOM kill is worth an exit code");
        assert!(c.detail.contains("2 oom-kill event(s)"), "{}", c.detail);
        assert!(
            c.detail.contains("2026-08-20T00:12:44+0200"),
            "{}",
            c.detail
        );
        assert!(
            c.detail.contains("1 of them since the unit last started"),
            "{}",
            c.detail
        );
        // …and what to do about it, which is the whole point of failing.
        assert!(c.detail.contains("min_memory_headroom"), "{}", c.detail);
    }

    #[test]
    fn a_quiet_week_is_green() {
        let c = oom_kills_check(Some(&[]), Some(crate::pressure::OomCounters::default()));
        assert!(c.ok);
        assert!(c.detail.contains("none in the last"), "{}", c.detail);
    }

    /// A journal that could not be read is "not checked" — but a counter that
    /// still says something is still evidence, and must not be swallowed by the
    /// journal's silence.
    #[test]
    fn an_unreadable_journal_does_not_hide_the_counter() {
        let quiet = oom_kills_check(None, None);
        assert!(quiet.ok);
        assert!(quiet.detail.contains("not checked"), "{}", quiet.detail);

        let counted = oom_kills_check(
            None,
            Some(crate::pressure::OomCounters {
                cgroup: 3,
                boxwide: 3,
            }),
        );
        assert!(!counted.ok);
        assert!(
            counted.detail.contains("3 process(es)"),
            "{}",
            counted.detail
        );
    }

    // ---- the unit that has to keep being true (gh#529 → gh#533) ----------

    fn governance(text: &str) -> comet_update::service::Governance {
        comet_update::service::parse_governance(text).unwrap()
    }

    /// The state every box installed before gh#529 is still in, and the reason
    /// this check exists: the fix shipped, and it did not arrive.
    #[test]
    fn a_unit_that_would_die_with_its_child_fails_and_prints_the_fix() {
        let c = unit_governance_check(Some(&governance(
            "LoadState=loaded\nOOMPolicy=stop\nMemoryHigh=infinity\nMemoryMax=infinity\n",
        )));
        assert!(!c.ok);
        assert!(c.detail.contains("OOMPolicy=stop"), "{}", c.detail);
        assert!(c.detail.contains("MemoryHigh is unset"), "{}", c.detail);
        assert!(c.detail.contains("MemoryMax is unset"), "{}", c.detail);
        // The paste, whole — the box this fires on is reached over ssh.
        assert!(c.detail.contains("mkdir -p"), "{}", c.detail);
        assert!(c.detail.contains("OOMPolicy=continue"), "{}", c.detail);
        assert!(c.detail.contains("daemon-reload"), "{}", c.detail);
    }

    #[test]
    fn a_governed_unit_is_green() {
        let c = unit_governance_check(Some(&governance(
            "LoadState=loaded\nOOMPolicy=continue\nMemoryHigh=12252659712\n\
             MemoryMax=14703191654\n",
        )));
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("not an engine death"), "{}", c.detail);
    }

    /// A source build in a terminal has no unit, and a box with no unit is not
    /// a misconfigured box. Nor is a platform with no systemd to ask.
    #[test]
    fn a_box_running_the_engine_by_hand_is_not_failed_for_it() {
        let none = unit_governance_check(None);
        assert!(none.ok);
        assert!(none.detail.contains("not checked"), "{}", none.detail);

        let unloaded = unit_governance_check(Some(&governance(
            "LoadState=not-found\nOOMPolicy=\nMemoryHigh=\nMemoryMax=\n",
        )));
        assert!(unloaded.ok);
        assert!(
            unloaded.detail.contains("not running as a service"),
            "{}",
            unloaded.detail
        );
    }

    // ---- dispatched pushes: what a recorded failure is allowed to say (gh#515)

    /// The live clause from the box gh#515 was reported on: healthy, and said
    /// so in the present tense.
    const HEALTHY: &str = "the askpass helper answers, and mints per push; `gh` at \
                           /opt/homebrew/bin/gh is wrapped to mint per call";

    fn ledger_failure(
        now: chrono::DateTime<chrono::Utc>,
        ago: chrono::Duration,
        error: &str,
    ) -> crate::credential_ledger::Entry {
        crate::credential_ledger::Entry {
            at: (now - ago).to_rfc3339(),
            event: crate::credential_ledger::Event::Unusable,
            tool: "dispatch".into(),
            repo: "bredebjorhovd/comet-board".into(),
            chat: Some("chat-1".into()),
            error: Some(error.into()),
        }
    }

    /// gh#515, exactly as printed: a two-day-old GitHub outage rendered as a
    /// red FAIL, with the healthy present-tense state buried in front of it.
    /// The operator went looking for a `gh` that had dropped off the PATH.
    #[test]
    fn a_two_day_old_github_outage_is_history_and_not_a_failure() {
        let now = chrono::Utc::now();
        let c = push_verdict(
            Live::Working(HEALTHY.into()),
            Some(ledger_failure(
                now,
                chrono::Duration::days(2),
                "the askpass credential handoff … exited exit status: 1: Error: github HTTP 504 \
                 for /repos/bredebjorhovd/comet-board: We couldn't respond to your request in \
                 time.",
            )),
            now,
        );
        assert!(c.ok, "{}", c.detail);
        // Said as history, dated, and attributed to the side that caused it.
        assert!(c.detail.contains("history"), "{}", c.detail);
        assert!(c.detail.contains("2d ago"), "{}", c.detail);
        assert!(c.detail.contains("GitHub's own"), "{}", c.detail);
        // And the healthy state is still there — it was never the wrong fact,
        // only the wrong colour.
        assert!(c.detail.starts_with(HEALTHY), "{}", c.detail);
    }

    /// An outage is not this box's problem at any age: the only action it could
    /// ask for is "wait", and red does not mean wait.
    #[test]
    fn a_fresh_github_outage_is_still_not_this_boxs_fault() {
        let now = chrono::Utc::now();
        let c = push_verdict(
            Live::Working(HEALTHY.into()),
            Some(ledger_failure(
                now,
                chrono::Duration::minutes(20),
                "github HTTP 503 for /repos/o/r: Service Unavailable",
            )),
            now,
        );
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.contains("20m ago"), "{}", c.detail);
    }

    /// The gh#233 shape survives: a run failed on something this box can fix,
    /// it failed today, and the probe cannot reproduce it. Still red — and now
    /// the actionable sentence leads instead of trailing.
    #[test]
    fn a_fresh_local_failure_the_probe_cannot_reproduce_still_leads_in_red() {
        let now = chrono::Utc::now();
        let c = push_verdict(
            Live::Working(HEALTHY.into()),
            Some(ledger_failure(
                now,
                chrono::Duration::hours(3),
                "cannot exec the askpass shim",
            )),
            now,
        );
        assert!(!c.ok, "{}", c.detail);
        assert!(
            c.detail
                .starts_with("a dispatched run could not use the credential path 3h ago"),
            "{}",
            c.detail
        );
        // The healthy clause is context now, not the opening.
        assert!(c.detail.contains(HEALTHY), "{}", c.detail);
    }

    /// Same failure, past the window. Nothing about the box changed, but a day
    /// of nothing going wrong is the answer to "is this happening now".
    #[test]
    fn a_local_failure_ages_out_of_red() {
        let now = chrono::Utc::now();
        let c = push_verdict(
            Live::Working(HEALTHY.into()),
            Some(ledger_failure(
                now,
                chrono::Duration::hours(30),
                "cannot exec the askpass shim",
            )),
            now,
        );
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.starts_with(HEALTHY), "{}", c.detail);
        assert!(c.detail.contains("and none since"), "{}", c.detail);
    }

    /// When the probe itself fails there is something to fix on this box right
    /// now, so it opens the line and the ledger trails as context.
    #[test]
    fn a_broken_probe_leads_and_the_ledger_trails_it() {
        let now = chrono::Utc::now();
        let broken = "the credential path does not work — no dispatched agent on this box can \
                      push with the board's App: cannot exec";
        let c = push_verdict(
            Live::Broken(broken.into()),
            Some(ledger_failure(
                now,
                chrono::Duration::days(2),
                "github HTTP 504 for /repos/o/r: We couldn't respond in time.",
            )),
            now,
        );
        assert!(!c.ok);
        assert!(c.detail.starts_with(broken), "{}", c.detail);
        assert!(c.detail.contains("last failure 2d ago"), "{}", c.detail);

        // With nothing recorded, the probe's sentence is the whole line.
        let c = push_verdict(Live::Broken(broken.into()), None, now);
        assert!(!c.ok);
        assert_eq!(c.detail, broken);
    }

    /// A healthy box with nothing standing against it says one thing.
    #[test]
    fn a_healthy_path_with_no_standing_failure_says_only_that() {
        let c = push_verdict(Live::Working(HEALTHY.into()), None, chrono::Utc::now());
        assert!(c.ok);
        assert_eq!(c.detail, HEALTHY);
    }

    #[test]
    fn an_age_is_written_the_way_it_is_read() {
        assert_eq!(human_age(0), "just now");
        assert_eq!(human_age(59), "just now");
        assert_eq!(human_age(60), "1m ago");
        assert_eq!(human_age(3_599), "59m ago");
        assert_eq!(human_age(3_600), "1h ago");
        assert_eq!(human_age(179_100), "2d ago");
    }

    /// GitHub's 5xx bodies are prose; the whole of one buries every check under
    /// it. Clipping happens on a character boundary, whatever the bytes.
    #[test]
    fn a_quoted_error_is_clipped_without_splitting_a_character() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("exactly-10", 10), "exactly-10");
        assert_eq!(clip("more than ten", 10), "more than…");
        assert_eq!(clip("måltid på øya", 6), "måltid…");
    }

    /// These checks read the provider, never the wire. Answering at all would
    /// only hide a call that should not be happening.
    struct NoWire;

    impl crate::sources::github::Transport for NoWire {
        fn send(
            &self,
            _: reqwest::Method,
            path: &str,
            _: Option<&serde_json::Value>,
            _: Option<&str>,
        ) -> Result<crate::sources::github::Reply> {
            panic!("the auth checks must not make a REST call ({path})")
        }
    }
}
