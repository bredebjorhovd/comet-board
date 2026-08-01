//! `comet-board doctor` — check the environment: keys present, engine
//! reachable, db writable, routes valid.
//!
//! Ported from herdr-board's doctor with the herdr checks replaced by comet
//! ones: a route's `workspace` must name a comet *space*, its `runtime` must
//! resolve to a comet *harness*, and the process everything runs in is the
//! engine on the local IPC port rather than a separate `syncd`. The checks
//! herdr needed against a mux it distrusted (manifest overrides, daemon
//! pidfiles) have no equivalent and are gone.

use crate::config::{Paths, RoutingConfig, github_token, linear_api_key};
use crate::db::Db;
use crate::runtime::harness_for_runtime;
use crate::sources::linear::{HttpTransport, Linear};
use anyhow::Result;
use comet_proto::Space;

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
}

/// Check the environment. Plain stdout — the report is the output.
///
/// `spaces` is this device's space list, or `None` when the engine could not
/// be asked. Route checks against a `None` say "not checked" rather than
/// failing every route over one dead engine — the engine check itself is the
/// one that fails loudly.
pub fn doctor(
    paths: &Paths,
    engine: &EngineStatus,
    spaces: Option<&[Space]>,
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

    checks.push(Check {
        name: "LINEAR_API_KEY".into(),
        ok: linear_api_key(paths).is_some(),
        detail: match linear_api_key(paths) {
            Some(_) => "present".into(),
            None => format!("missing — add it to {}", paths.env_file().display()),
        },
    });

    // The engine hosts the board loop (there is no separate daemon), so
    // "reachable" answers both "can I list spaces" and "is anything polling".
    checks.push(Check {
        name: "engine".into(),
        ok: engine.reachable,
        detail: engine.detail.clone(),
    });

    // Routing is where most misconfiguration lives, so it is checked in detail.
    // Parsing is deliberately separated from validation: a single bad runtime
    // must not hide the problems in every other route.
    match RoutingConfig::load_unvalidated(&paths.routing()) {
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
            // The token is only optional until repos are configured: GitHub
            // answers 404 (not 401) for a private repo you cannot see, so
            // without this check the first symptom is a mystery 404 in the log.
            let repos = &cfg.github.repos;
            checks.push(Check {
                name: "GITHUB_TOKEN".into(),
                ok: repos.is_empty() || github_token(paths).is_some(),
                detail: match (github_token(paths).is_some(), repos.is_empty()) {
                    (true, _) => "present".into(),
                    (false, true) => "not needed — no repos under [github]".into(),
                    (false, false) => format!(
                        "missing, but {} repo(s) are configured — private repos \
                         answer 404 without it. Add it to {}",
                        repos.len(),
                        paths.env_file().display()
                    ),
                },
            });

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
                detail: settle_notice_detail(cfg.defaults.notify_dispatcher),
            });

            // The one Linear state the board resolves by name, so the one that
            // can be wrong. A missing state drops the writeback rather than
            // retrying it forever, and this is where that becomes visible.
            checks.push(review_state_check(paths, &cfg));

            for repo in repos {
                let reachable = crate::sources::github::HttpRest::new(github_token(paths))
                    .ok()
                    .map(|r| {
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
                        "404 — either it does not exist, or the token cannot see it".to_string(),
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
            }
        }
    }

    Ok(checks)
}

/// Whether a dispatching agent is told its released work settled.
///
/// The one setting whose failure is invisible: writeback failing leaves a ticket
/// open, an unresolvable review state is reported by name, but a notice that
/// never fires produces nothing at all — no error, no log line, no changed row.
/// It reads as "nobody told me" rather than "the board is misconfigured", which
/// is why it belongs in `doctor` next to the other two.
fn settle_notice_detail(notify_dispatcher: bool) -> String {
    if notify_dispatcher {
        "on — an agent that released work is prompted in its own chat \
         when that work settles"
            .into()
    } else {
        "off — only you are notified when released work settles; the agent \
         that released it waits to be asked (`[defaults] notify_dispatcher \
         = true` to enable)"
            .into()
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

/// Does `[linear] review_state` name a state the configured teams actually have?
///
/// Unset is fine and is the default — it means the ticket stays where dispatch
/// left it while a PR waits. Set and unresolvable is not fine, and is invisible
/// otherwise: the writeback is dropped with a log line nobody reads.
fn review_state_check(paths: &Paths, cfg: &RoutingConfig) -> Check {
    let Some(want) = cfg.linear.review_state.as_deref() else {
        return Check {
            name: "linear review state".into(),
            ok: true,
            detail: "unset — a finished attempt leaves Linear in its started state \
                     (`[linear] review_state = \"In Review\"` to move it)"
                .into(),
        };
    };
    // Teams the board actually dispatches for. Checking every team the key can
    // see would report states for workspaces this config never touches.
    let mut teams: Vec<&str> = cfg
        .routes
        .iter()
        .filter_map(|r| r.match_.linear_team.as_deref())
        .collect();
    teams.sort_unstable();
    teams.dedup();

    let linear = linear_api_key(paths)
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
    for team in &teams {
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
        let checks = doctor(&p, &engine_up(), Some(&[])).unwrap();
        assert!(checks.iter().any(|c| c.name == "routing.toml" && !c.ok));
        // The database check must still pass — doctor creates it.
        assert!(checks.iter().any(|c| c.name == "database" && c.ok));
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
        };
        let checks = doctor(&p, &down, None).unwrap();
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
        let checks = doctor(&p, &engine_up(), Some(&spaces)).unwrap();
        // Case-insensitive, like every other name match on the board.
        let c = checks
            .iter()
            .find(|c| c.name == "route tally: space")
            .unwrap();
        assert!(c.ok, "{}", c.detail);

        let checks = doctor(&p, &engine_up(), Some(&[])).unwrap();
        let c = checks
            .iter()
            .find(|c| c.name == "route tally: space")
            .unwrap();
        assert!(!c.ok);
        assert!(c.detail.contains("no comet space named"), "{}", c.detail);
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
        let checks = doctor(&p, &engine_up(), Some(&[])).unwrap();
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

    /// gh#27's shape, inherited: a notice that never fires is the one failure
    /// that produces nothing to look at, so the line saying whether it is on
    /// has to be there in both states — and has to name the key.
    #[test]
    fn doctor_says_whether_the_dispatcher_is_told() {
        let on = settle_notice_detail(true);
        assert!(on.starts_with("on —"), "{on}");
        assert!(on.contains("released work is prompted"), "{on}");

        let off = settle_notice_detail(false);
        assert!(off.starts_with("off —"), "{off}");
        assert!(
            off.contains("notify_dispatcher"),
            "off has to name the key to turn it on: {off}"
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
        let checks = doctor(&p, &engine_up(), Some(&[])).unwrap();
        let c = checks
            .iter()
            .find(|c| c.name == "settle notice")
            .expect("doctor is silent about notify_dispatcher");
        assert!(c.detail.starts_with("on —"), "{:?}", c.detail);
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
        let (_d, p) = tmp();
        let cfg: RoutingConfig = toml::from_str("").unwrap();
        let c = review_state_check(&p, &cfg);
        assert!(c.ok);
        assert!(c.detail.contains("review_state"), "{}", c.detail);
    }
}
