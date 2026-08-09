//! `comet-board doctor` — check the environment: keys present, engine
//! reachable, db writable, routes valid.
//!
//! Ported from herdr-board's doctor with the herdr checks replaced by comet
//! ones: a route's `workspace` must name a comet *space*, its `runtime` must
//! resolve to a comet *harness*, and the process everything runs in is the
//! engine on the local IPC port rather than a separate `syncd`. The checks
//! herdr needed against a mux it distrusted (manifest overrides, daemon
//! pidfiles) have no equivalent and are gone.

use crate::config::{Credentials, GithubAuth, Paths, RoutingConfig, linear_api_key};
use crate::db::Db;
use crate::gc;
use crate::git_credentials;
use crate::git_identity;
use crate::runtime::harness_for_runtime;
use crate::skill;
use crate::sources::linear::{HttpTransport, Linear};
use anyhow::Result;
use comet_proto::view::board::RuntimeOption;
use comet_proto::{AgentAccount, EdgeHealth, Space};
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

    // What the attempts have left on the disk (gh#72). Beside the database
    // check because it is the same question — what is this box holding — and
    // the answer is read partly out of that database.
    checks.push(worktrees_check(
        paths,
        &crate::config::worktrees_root(),
        db_ok.as_ref().ok(),
    ));

    // The other half of what an attempt leaves behind (gh#139). Beside the
    // checkouts because it is the same question asked of the shelf instead of
    // the disk, and the two retentions are set in the same table.
    checks.push(chats_check(paths, db_ok.as_ref().ok()));

    // Read once and used three times below — the credential's own check, the
    // review-state check, and the decision whether to mention Linear at all.
    let linear_key = linear_api_key(paths);
    // Parsed here rather than only at the `match` below, because whether this
    // board wants anything from Linear is a question about its routes.
    let routing = RoutingConfig::load_unvalidated(&paths.routing());
    let linear_teams: Vec<String> = routing
        .as_ref()
        .map(|cfg| {
            cfg.linear_teams()
                .iter()
                .map(|t| (*t).to_string())
                .collect()
        })
        .unwrap_or_default();

    checks.push(linear_key_check(
        paths,
        linear_key.clone(),
        &linear_teams,
        |key| {
            HttpTransport::new(key)
                .map(Linear::new)
                .and_then(|l| l.viewer())
        },
    ));

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

            // The one Linear state the board resolves by name, so the one that
            // can be wrong. A missing state drops the writeback rather than
            // retrying it forever, and this is where that becomes visible.
            //
            // Only when Linear is in the picture at all, on the same rule the
            // `account` check follows: a GitHub-only board must not be handed a
            // line about a Linear setting it has no use for, because the line
            // reads as something left unconfigured (gh#96).
            if linear_key.is_some() || !linear_teams.is_empty() || cfg.linear.review_state.is_some()
            {
                checks.push(review_state_check(linear_key.clone(), &linear_teams, &cfg));
            }

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
                                format!(
                                    "no comet space named `{}` (have: {})",
                                    r.workspace,
                                    spaces
                                        .iter()
                                        .map(Space::display_name)
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                )
                            },
                        }
                    }
                });

                let repo = r.repo_path();
                let repo_ok = repo.join(".git").exists();
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

                // Only when the route names one: a board on one person's
                // laptop has no accounts to check and should not be told about
                // a feature it is not using (gh#59).
                if let Some(account) = r.account.as_deref().filter(|a| !a.is_empty()) {
                    checks.push(account_check(&name, account, r, accounts));
                }
            }
        }
    }

    checks.push(dispatched_push_check(paths));
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

    Ok(checks)
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

/// Which edge connections the engine on this box actually holds (gh#116).
///
/// Fails on exactly one state — [`EdgeHealth::dark`]: an engine wired to an
/// edge, holding none of it. That is the state nobody could see. A room or two
/// down is reported and not failed, because a client that has just dropped is
/// already redialing and doctor must not cry wolf at every edge deploy; an
/// engine that could not be asked is not failed either, since the `engine`
/// check above already says so, loudly and once.
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
    Check {
        name: "edge connections".into(),
        ok: !edge.dark(),
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
/// `ok` is false only when the root is genuinely large ([`gc::WARN_BYTES`] /
/// [`gc::WARN_CHECKOUTS`]); a board holding a week of checkouts on purpose is
/// working exactly as configured and must not fail the report for it. The
/// retention window is named either way, because `off` is a choice whose cost
/// is this line.
fn worktrees_check(paths: &Paths, root: &std::path::Path, db: Option<&Db>) -> Check {
    let usage = gc::usage(root);
    let about = format!(
        "{} checkout(s), {}{} in {}",
        usage.checkouts,
        if usage.truncated { "≥ " } else { "" },
        gc::human_bytes(usage.bytes),
        root.display(),
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
fn dispatched_push_check(paths: &Paths) -> Check {
    let credential = !matches!(Credentials::load(paths).github_auth(), GithubAuth::None);
    let exe = git_credentials::resolve_board_exe();
    let gh = git_credentials::resolve_gh(None);
    let detail = match (&exe, credential) {
        (None, _) => format!(
            "no comet-board binary found beside the engine or on PATH — agents push with \
             this box's own git credentials (set {})",
            git_credentials::BOARD_EXE_ENV
        ),
        (Some(_), false) => "no GitHub credential — agents push with this box's own git \
             credentials"
            .to_string(),
        (Some(exe), true) => format!(
            "{} git-askpass mints per push{}",
            exe.display(),
            match &gh {
                Some(gh) => format!("; `gh` at {} is wrapped to mint per call", gh.display()),
                None => "; no `gh` installed, so pull requests are opened by hand".into(),
            }
        ),
    };
    Check {
        name: "dispatched pushes".into(),
        ok: exe.is_some() && credential,
        detail,
    }
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
/// Unconditional for Linear and gated per repo for GitHub, which is the same
/// rule every other comment follows — and the reason this is worth a line: a
/// read-only repo gets no comment, so on those repos the board row really is
/// the only signal, and an operator should learn that here rather than by
/// waiting for a comment that is never coming.
fn blocked_notice_detail(github: &crate::config::GithubConfig) -> String {
    let mut s = "on — an agent that stops to ask, or whose run dies, leaves one comment \
                 on its issue per block (Linear always; GitHub where writeback is on)"
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

/// Is there a working Linear credential, and does this board want one (gh#96)?
///
/// Three states, and only one of them is a fault. A board with no key and no
/// route matching on a Linear team is a *GitHub-only board* — a supported
/// configuration, not a half-finished one, and saying `FAIL … missing` at it
/// (which is what this check did, inherited from a board where Linear always
/// existed) sends people looking for a break that is not there. That state
/// reads like the operator-notice line: `ok`, and worded so nobody mistakes it
/// for broken.
///
/// The two that do fail are the ones with real consequences. A key present but
/// rejected means every Linear poll is failing in the log; a key absent while a
/// route matches on `linear_team` means those tickets silently never arrive,
/// and the route can never fire.
///
/// `viewer` is injected so the check is testable without the network — it
/// answers who a key authenticates as, or why the API would not say.
fn linear_key_check<P>(paths: &Paths, key: Option<String>, teams: &[String], viewer: P) -> Check
where
    P: FnOnce(String) -> Result<String>,
{
    let name = "LINEAR_API_KEY".to_string();
    let Some(key) = key else {
        return match teams {
            [] => Check {
                name,
                ok: true,
                detail: format!(
                    "not configured — the board polls GitHub only (add it to {} to \
                     poll Linear as well)",
                    paths.env_file().display()
                ),
            },
            // Not a preference any more: the config asked for Linear.
            teams => Check {
                name,
                ok: false,
                detail: format!(
                    "not configured, but {} route(s) match on a Linear team ({}) — \
                     those tickets never reach the board and the route can never fire. \
                     Add the key to {}, or drop the `linear_team` match",
                    teams.len(),
                    teams.join(", "),
                    paths.env_file().display()
                ),
            },
        };
    };
    match viewer(key) {
        Ok(who) => Check {
            name,
            ok: true,
            detail: format!("accepted by Linear — polling as {who}"),
        },
        // An unreachable API is not a rejected key, and doctor must not report
        // it as one: a laptop on a train would otherwise fail this check every
        // time, which is the same false alarm gh#96 is about.
        Err(e) if unreachable_api(&e) => Check {
            name,
            ok: true,
            detail: format!("configured, not checked — Linear could not be reached ({e})"),
        },
        Err(e) => Check {
            name,
            ok: false,
            detail: format!(
                "rejected by Linear ({e:#}) — every poll fails with this. Replace it in {}, \
                 or remove it to run the board on GitHub alone",
                paths.env_file().display()
            ),
        },
    }
}

/// Did the API decline to answer at all, rather than decline the key?
///
/// Transport failures arrive as a `reqwest::Error` straight off the wire; a
/// rate limit is our own message from a 429. Everything else reached Linear and
/// came back with something to say about the credential.
fn unreachable_api(e: &anyhow::Error) -> bool {
    e.downcast_ref::<reqwest::Error>().is_some() || format!("{e:#}").contains("rate limited")
}

/// Does `[linear] review_state` name a state the configured teams actually have?
///
/// Unset is fine and is the default — it means the ticket stays where dispatch
/// left it while a PR waits. Set and unresolvable is not fine, and is invisible
/// otherwise: the writeback is dropped with a log line nobody reads.
///
/// Only reached when Linear is in the picture at all; a board with neither a
/// key nor a Linear route gets no line here (gh#96). `teams` are the teams the
/// board actually dispatches for — checking every team the key can see would
/// report states for workspaces this config never touches.
fn review_state_check(key: Option<String>, teams: &[String], cfg: &RoutingConfig) -> Check {
    let Some(want) = cfg.linear.review_state.as_deref() else {
        return Check {
            name: "linear review state".into(),
            ok: true,
            detail: "unset — a finished attempt leaves Linear in its started state \
                     (`[linear] review_state = \"In Review\"` to move it)"
                .into(),
        };
    };

    let linear = key
        .and_then(|k| HttpTransport::new(k).ok())
        .map(Linear::new);
    let (Some(linear), false) = (linear, teams.is_empty()) else {
        return Check {
            name: "linear review state".into(),
            ok: true,
            detail: format!("`{want}` — not checked (no key, or no route names a Linear team)"),
        };
    };

    let mut missing = Vec::new();
    for team in teams {
        match linear.state_id_named(team, want) {
            Ok(Ok(_)) => {}
            Ok(Err(have)) => missing.push(format!("{team} has: {}", have.join(", "))),
            Err(e) => missing.push(format!("{team}: {e}")),
        }
    }
    Check {
        name: "linear review state".into(),
        ok: missing.is_empty(),
        detail: if missing.is_empty() {
            format!(
                "`{want}` resolves in {} team(s) — a finished attempt moves the issue there",
                teams.len()
            )
        } else {
            format!("no state named `{want}` — {}", missing.join("; "))
        },
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
        // A `LINEAR_API_KEY` exported into the shell outranks the config
        // directory, so on a box that has one every `doctor` below would find a
        // credential these tests never wrote — and since gh#96 would go and ask
        // the real Linear about it. The config directory each test builds is
        // the only credential source any of them mean to have.
        static UNSET: std::sync::Once = std::sync::Once::new();
        UNSET.call_once(|| unsafe { std::env::remove_var("LINEAR_API_KEY") });

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
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), Some(&dark), None, None).unwrap();
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
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), Some(&partial), None, None).unwrap();
        let check = edge_check_in(&checks);
        assert!(check.ok, "{}", check.detail);
        assert!(
            check.detail.contains("workspace room down"),
            "{}",
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
            "[[route]]\nmatch = { linear_team = \"AGE\" }\nworkspace = \"w\"\n\
             repo = \"/nowhere\"\nruntime = \"claude-code\"\n",
        )
        .unwrap();
        let down = EngineStatus {
            reachable: false,
            detail: "connection refused".into(),
            version: None,
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
        let (ok, detail) =
            base_check_in(&doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap());
        assert!(!ok, "{detail}");
        assert!(detail.contains("origin"), "{detail}");
        assert!(detail.contains("HEAD"), "the opt-out is named: {detail}");

        routing("HEAD");
        let (ok, detail) =
            base_check_in(&doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap());
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
            "[[route]]\nmatch = { linear_team = \"AGE\" }\nworkspace = \"tally\"\n\
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
            option("codex", HarnessId::Codex, Some(RuntimeUnavailable::SignedOut)),
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
        assert!(c.detail.contains("opencode (not installed)"), "{}", c.detail);
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
            linear_team: Some("LIN".into()),
            linear_project: None,
            upstream: crate::model::UpstreamState::Started,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let a = db
            .insert_attempt(&crate::db::NewAttempt {
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

    #[test]
    fn an_unset_review_state_passes_and_says_what_setting_it_would_do() {
        let cfg: RoutingConfig = toml::from_str("").unwrap();
        let c = review_state_check(Some("lin_x".into()), &[], &cfg);
        assert!(c.ok);
        assert!(c.detail.contains("review_state"), "{}", c.detail);
    }

    // ── the Linear credential (gh#96) ───────────────────────────────────────

    /// A board with no key and no route matching a Linear team is a
    /// *GitHub-only board* — a supported configuration, not a half-finished
    /// one. Inherited from herdr-board, where Linear always existed, this check
    /// said `FAIL … missing` at it and sent people looking for a break that was
    /// not there. It passes, and is worded so nobody reads it as broken.
    #[test]
    fn a_github_only_board_is_not_failed_over_an_absent_linear_key() {
        let (_d, p) = tmp();
        let c = linear_key_check(&p, None, &[], |_| panic!("there is no key to probe"));
        assert!(c.ok, "{}", c.detail);
        assert!(c.detail.starts_with("not configured —"), "{}", c.detail);
        assert!(c.detail.contains("GitHub only"), "{}", c.detail);
        assert!(
            !c.detail.contains("missing"),
            "the word this line failed people with: {}",
            c.detail
        );
    }

    /// The other half of the rule. Once a route matches on `linear_team` the
    /// key has stopped being optional: without it those tickets never reach the
    /// board and the route can never fire — silently, which is what `doctor` is
    /// for. Same shape as the GitHub credential, which fails only once repos
    /// are configured.
    #[test]
    fn a_route_matching_a_linear_team_still_needs_the_key() {
        let (_d, p) = tmp();
        let c = linear_key_check(&p, None, &["AGE".into(), "TAL".into()], |_| {
            panic!("there is no key to probe")
        });
        assert!(!c.ok, "{}", c.detail);
        assert!(
            c.detail.contains("AGE") && c.detail.contains("TAL"),
            "{}",
            c.detail
        );
        assert!(c.detail.contains("linear_team"), "{}", c.detail);
    }

    /// A key that is there is worth checking, and only the API can say whether
    /// it still works — a revoked string in `.env` looked exactly like a good
    /// one before this.
    #[test]
    fn a_present_key_is_reported_by_what_linear_says_about_it() {
        let (_d, p) = tmp();
        let good = linear_key_check(&p, Some("lin_x".into()), &[], |k| {
            assert_eq!(k, "lin_x");
            Ok("sam@example.com".into())
        });
        assert!(good.ok, "{}", good.detail);
        assert!(good.detail.contains("sam@example.com"), "{}", good.detail);

        let rejected = linear_key_check(&p, Some("lin_x".into()), &[], |_| {
            Err(anyhow::anyhow!(
                "linear GraphQL error: Authentication required, not authenticated"
            ))
        });
        assert!(!rejected.ok, "{}", rejected.detail);
        assert!(rejected.detail.contains("rejected"), "{}", rejected.detail);
        assert!(
            rejected.detail.contains("Authentication required"),
            "the reason has to travel with the failure: {}",
            rejected.detail
        );
    }

    /// An API that would not answer is not a key that was rejected. Reporting
    /// one as the other is the same false alarm from the other direction — a
    /// laptop on a train would fail this check every time.
    #[test]
    fn an_unreachable_linear_does_not_condemn_a_configured_key() {
        let (_d, p) = tmp();
        let limited = linear_key_check(&p, Some("lin_x".into()), &[], |_| {
            Err(anyhow::anyhow!("linear rate limited (429)"))
        });
        assert!(limited.ok, "{}", limited.detail);
        assert!(limited.detail.contains("not checked"), "{}", limited.detail);

        // The other arm: nothing on the wire at all. Port 1 on this machine
        // refuses, so the error is a real `reqwest::Error` and never leaves it.
        let refused = reqwest::blocking::Client::new()
            .get("http://127.0.0.1:1/")
            .send()
            .expect_err("nothing listens on port 1");
        let offline = linear_key_check(&p, Some("lin_x".into()), &[], |_| Err(refused.into()));
        assert!(offline.ok, "{}", offline.detail);
    }

    /// And the report as a whole agrees: on a GitHub-only board nothing says
    /// Linear is unconfigured — including the review-state line, which is not
    /// printed at all, on the same rule the `account` check follows (gh#59).
    #[test]
    fn a_github_only_report_is_silent_about_linear() {
        let (_d, p) = tmp();
        std::fs::write(
            p.routing(),
            "[[route]]\nmatch = { gh_repo = \"o/r\" }\nworkspace = \"w\"\n\
             repo = \"/tmp\"\nruntime = \"claude-code\"\n",
        )
        .unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        assert!(
            !checks.iter().any(|c| c.name == "linear review state"),
            "a board with no Linear anywhere must not be handed a Linear setting"
        );
        let key = checks
            .iter()
            .find(|c| c.name == "LINEAR_API_KEY")
            .expect("the credential is still reported, just not as a fault");
        assert!(key.ok, "{}", key.detail);

        // With a Linear route in the file the line comes back — that board does
        // have a workflow state to get wrong.
        std::fs::write(
            p.routing(),
            "[[route]]\nmatch = { linear_team = \"AGE\" }\nworkspace = \"w\"\n\
             repo = \"/tmp\"\nruntime = \"claude-code\"\n",
        )
        .unwrap();
        let checks = doctor(&p, &engine_up(), Some(&[]), Some(&[]), None, None, None).unwrap();
        assert!(checks.iter().any(|c| c.name == "linear review state"));
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

        let check = worktrees_check(&p, &root, None);
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
        let check = worktrees_check(&p, &_d.path().join("nothing-here"), None);
        assert!(check.detail.contains("ever collected"), "{}", check.detail);
        // An empty root is still not a failure — nothing has been dispatched.
        assert!(check.ok);
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
