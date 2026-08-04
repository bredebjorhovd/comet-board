//! `comet-board init` — generate a starter `routing.toml` from the comet
//! spaces that actually exist, so nobody has to hand-write repo lists to see
//! anything.
//!
//! Ported from herdr-board's `init`, walking spaces instead of herdr
//! workspaces. Linked worktrees are skipped: those are attempts' checkouts,
//! not projects to route to.

use crate::adopt::{SpaceRepo, github_slug};
use crate::config::{GithubAuth, Paths, github_auth, linear_api_key, shorten_home};
use anyhow::Result;
use comet_proto::Space;
use std::path::Path;

/// Write the starter config. `spaces` is this device's space list; `probe_of`
/// resolves a space path to what git says about it (injected, same as
/// [`crate::adopt::detect`] — production callers pass [`crate::adopt::probe`]).
pub fn init<F>(paths: &Paths, spaces: &[Space], probe_of: F, force: bool) -> Result<()>
where
    F: Fn(&str) -> Option<SpaceRepo>,
{
    let target = paths.routing();
    if target.exists() && !force {
        anyhow::bail!(
            "{} already exists; pass --force to overwrite it",
            target.display()
        );
    }

    // Resolve every git-detected space once; routes and Linear guesses both
    // read from this.
    let candidates: Vec<(String, SpaceRepo)> = spaces
        .iter()
        .filter(|s| s.git_detected)
        .filter_map(|s| probe_of(&s.path).map(|repo| (s.display_name().to_string(), repo)))
        .filter(|(_, repo)| !repo.linked_worktree)
        .collect();

    let mut routes = String::new();
    let mut repos: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    // Spaces that are folders but not checkouts are worth naming: seeing your
    // notes directory in the "skipped" list is what tells you the sweep saw it.
    for s in spaces.iter().filter(|s| !s.git_detected) {
        skipped.push(format!("{} (not a git checkout)", s.display_name()));
    }

    // Linear routes need team keys, which only the API can supply. With a key
    // present there is no reason to make anyone look them up by hand.
    let mut linear_routes = String::new();
    match linear_api_key(paths) {
        None => {
            linear_routes.push_str(
                "# No LINEAR_API_KEY when this ran, so Linear teams could not be\n\
                 # discovered. Add the key and re-run `comet-board init --force`,\n\
                 # or add routes by hand:\n\
                 #\n\
                 # [[route]]\n\
                 # match = { linear_team = \"ABC\" }\n\
                 # workspace = \"my-space\"\n\
                 # repo = \"~/code/thing\"\n\
                 # runtime = \"claude-code\"\n",
            );
        }
        Some(key) => {
            let teams = crate::sources::linear::HttpTransport::new(key)
                .map(crate::sources::linear::Linear::new)
                .and_then(|l| l.teams());
            match teams {
                Err(e) => {
                    linear_routes.push_str(&format!("# Could not list Linear teams: {e}\n"));
                }
                Ok(teams) => {
                    for (key, name) in teams {
                        // Guess the space by name, then by key; say so when
                        // there is no match rather than inventing one.
                        let hit = candidates.iter().find(|(label, _)| {
                            label.eq_ignore_ascii_case(&name) || label.eq_ignore_ascii_case(&key)
                        });
                        match hit {
                            Some((label, repo)) => {
                                let repo = shorten_home(Path::new(&repo.root));
                                linear_routes.push_str(&format!(
                                    "\n# Linear team {key} ({name})\n[[route]]\nmatch = {{ linear_team = \"{key}\" }}\nworkspace = \"{label}\"\nrepo = \"{repo}\"\nruntime = \"claude-code\"\n"
                                ));
                            }
                            None => {
                                linear_routes.push_str(&format!(
                                    "\n# Linear team {key} ({name}) — no comet space matched by\n# name or key, so fill these in and uncomment.\n# [[route]]\n# match = {{ linear_team = \"{key}\" }}\n# workspace = \"CHANGE-ME\"\n# repo = \"~/code/CHANGE-ME\"\n# runtime = \"claude-code\"\n"
                                ));
                                skipped
                                    .push(format!("Linear team {key} ({name}) — no space matched"));
                            }
                        }
                    }
                }
            }
        }
    }

    for (label, repo) in &candidates {
        match repo.remote.as_deref().and_then(github_slug) {
            Some(slug) => {
                let root = shorten_home(Path::new(&repo.root));
                routes.push_str(&format!(
                    "\n[[route]]\nmatch = {{ gh_repo = \"{slug}\" }}\nworkspace = \"{label}\"\nrepo = \"{root}\"\nruntime = \"claude-code\"\n"
                ));
                if !repos.contains(&slug) {
                    repos.push(slug);
                }
            }
            None => skipped.push(format!("{label} (no GitHub remote)")),
        }
    }

    let repo_list = repos
        .iter()
        .map(|r| format!("\"{r}\""))
        .collect::<Vec<_>>()
        .join(", ");

    let body = starter_config(&linear_routes, &routes, &repo_list);

    std::fs::write(&target, body)?;
    println!("wrote {}", target.display());
    if !repos.is_empty() {
        println!(
            "routed {} GitHub repo(s): {}",
            repos.len(),
            repos.join(", ")
        );
    }
    for s in &skipped {
        println!("skipped {s}");
    }
    let mut missing = Vec::new();
    if linear_api_key(paths).is_none() {
        missing.push("LINEAR_API_KEY");
    }
    // Either credential answers for GitHub (gh#58) — the App is the one that
    // survives somebody else installing the board on their own repos, and the
    // token is the one that needs nothing registering. Naming both here is the
    // only place most people will meet the choice.
    if !repos.is_empty() && matches!(github_auth(paths), GithubAuth::None) {
        missing.push("GITHUB_TOKEN (or GITHUB_APP_ID + GITHUB_APP_PRIVATE_KEY_PATH)");
    }
    if missing.is_empty() {
        println!("\nnext: comet-board doctor");
    } else {
        println!(
            "\nnext: add {} to {}",
            missing.join(" and "),
            shorten_home(&paths.env_file())
        );
    }
    Ok(())
}

/// The starter `routing.toml`. Separate from [`init`] so a test can check the
/// thing people are handed actually parses, without an engine to ask.
fn starter_config(linear_routes: &str, routes: &str, repo_list: &str) -> String {
    format!(
        r#"# Generated by `comet-board init` from the comet spaces that existed at
# the time. Edit freely — this is a starting point, not managed config.

[sync]
interval = "30s"
# Linear labels that mean "dispatchable". Issues assigned to you are always
# included; these widen that set.
labels = ["herd"]

{linear_routes}{routes}
[defaults]
max_concurrent_per_workspace = 3
# The identifier made branch-safe: `LIN-145` → `lin-145`. GitHub numbers issues
# per repository, so a GitHub identifier carries its repo as well — `gh#2` in
# tripletex-mcp is `gh-2-tripletex-mcp`.
branch_template = "board/{{identifier_lower}}"
# When an agent releases work through the board, prompt it in its own chat once
# that work settles, instead of only raising a notification at you. Off, because
# an orchestrator woken by every child it released cannot hold a train of
# thought. Turn it on if you dispatch from orchestrators rather than by hand.
# notify_dispatcher = true

[linear]
# Which state means "finished, waiting on a human". Uncomment and Linear moves
# there when an attempt settles; leave it commented and the ticket stays In
# Progress until its pull request merges. It has to be a name — `In Review` and
# `In Progress` are both `type: started`, so the API cannot be asked which one
# means review. `doctor` checks whatever you write here resolves.
# review_state = "In Review"

# Repos polled for issues and pull requests. Private repos need a credential in
# .env — either GITHUB_TOKEN (a personal access token: simplest, and writes are
# attributed to you), or a GitHub App: GITHUB_APP_ID plus
# GITHUB_APP_PRIVATE_KEY_PATH pointing at the PEM, chmod 600. The App is what
# lets somebody else install the board on their own repos without handing over a
# credential, and its writes land as `[bot]`. `comet-board doctor` says which
# one is live. Remove the label filter to see every open issue.
[github]
repos = [{repo_list}]
labels = []
writeback = true

# The fallback, not the law: a repo can answer for itself. Worth doing for any
# repo whose issues other people read — comments on a project of your own are
# provenance nobody minds, the same comments on a production repo are not.
# `doctor` names the repos it will write to.
#
# [[github.repo]]
# name = "owner/production-repo"
# writeback = false
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoutingConfig;

    fn tmp() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().to_path_buf(),
            state_dir: dir.path().to_path_buf(),
        };
        (dir, paths)
    }

    fn space(name: &str, path: &str, git: bool) -> Space {
        Space {
            id: format!("s-{name}"),
            device_id: "dev-1".into(),
            path: path.into(),
            name: Some(name.into()),
            git_detected: git,
            git_checked_at: None,
            checkout_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn probes(
        entries: &[(&'static str, &'static str, &'static str, bool)],
    ) -> impl Fn(&str) -> Option<SpaceRepo> {
        let map: std::collections::HashMap<String, SpaceRepo> = entries
            .iter()
            .map(|(path, root, remote, linked)| {
                (
                    path.to_string(),
                    SpaceRepo {
                        root: root.to_string(),
                        remote: (!remote.is_empty()).then(|| remote.to_string()),
                        linked_worktree: *linked,
                    },
                )
            })
            .collect();
        move |path: &str| map.get(path).cloned()
    }

    #[test]
    fn the_generated_starter_config_parses() {
        // `init` writes the first config anyone sees; a section that does not
        // load is a broken first run. The `[linear]` header ships uncommented
        // with `review_state` commented under it on purpose — uncommenting one
        // line has to be enough, and a bare key under `[defaults]` would parse
        // and then be silently ignored.
        let body = starter_config(
            "",
            "[[route]]\nmatch = { linear_team = \"AGE\" }\nworkspace = \"w\"\n\
             repo = \"/tmp\"\nruntime = \"claude-code\"\n",
            "\"o/r\"",
        );
        let (_d, p) = tmp();
        std::fs::write(p.routing(), &body).unwrap();
        let cfg = RoutingConfig::load(&p.routing()).unwrap();
        assert_eq!(cfg.routes.len(), 1);
        assert!(cfg.linear.review_state.is_none(), "shipped commented out");

        let enabled = body.replace("# review_state =", "review_state =");
        std::fs::write(p.routing(), enabled).unwrap();
        assert_eq!(
            RoutingConfig::load(&p.routing())
                .unwrap()
                .linear
                .review_state
                .as_deref(),
            Some("In Review"),
        );
    }

    #[test]
    fn init_routes_the_spaces_that_are_github_checkouts() {
        let (_d, p) = tmp();
        let spaces = [
            space("tally", "/code/tally", true),
            space("notes", "/notes", false),
            // An attempt's checkout, not a project.
            space("lin-142", "/wt/lin-142", true),
            // A checkout with no GitHub remote: routable by hand, not by us.
            space("internal", "/code/int", true),
        ];
        let probe = probes(&[
            (
                "/code/tally",
                "/code/tally",
                "git@github.com:Florin-AS/Tally.git",
                false,
            ),
            (
                "/wt/lin-142",
                "/code/tally",
                "git@github.com:Florin-AS/Tally.git",
                true,
            ),
            (
                "/code/int",
                "/code/int",
                "git@git.example.com:o/int.git",
                false,
            ),
        ]);
        init(&p, &spaces, probe, false).unwrap();

        let cfg = RoutingConfig::load(&p.routing()).unwrap();
        assert_eq!(cfg.routes.len(), 1, "only the GitHub checkout is routed");
        assert_eq!(cfg.routes[0].workspace, "tally");
        assert_eq!(
            cfg.routes[0].match_.gh_repo.as_deref(),
            Some("Florin-AS/Tally")
        );
        assert_eq!(cfg.github.repos, ["Florin-AS/Tally"]);
    }

    #[test]
    fn an_existing_config_is_not_overwritten_without_force() {
        let (_d, p) = tmp();
        std::fs::write(p.routing(), "# precious\n").unwrap();
        let err = init(&p, &[], |_| None, false).unwrap_err();
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(
            std::fs::read_to_string(p.routing()).unwrap(),
            "# precious\n"
        );

        init(&p, &[], |_| None, true).unwrap();
        assert!(RoutingConfig::load(&p.routing()).is_ok());
    }

    #[test]
    fn one_repo_held_by_two_spaces_is_polled_once_but_routed_twice() {
        // Two spaces on one repo: both get a route header (either name works as
        // a dispatch target — first match wins), but the poll list stays
        // deduplicated or GitHub would be asked everything twice.
        let (_d, p) = tmp();
        let spaces = [
            space("tally", "/code/tally", true),
            space("tally-clone", "/code/tally2", true),
        ];
        let probe = probes(&[
            (
                "/code/tally",
                "/code/tally",
                "https://github.com/Florin-AS/Tally",
                false,
            ),
            (
                "/code/tally2",
                "/code/tally2",
                "https://github.com/Florin-AS/Tally",
                false,
            ),
        ]);
        init(&p, &spaces, probe, false).unwrap();
        let cfg = RoutingConfig::load(&p.routing()).unwrap();
        assert_eq!(cfg.github.repos, ["Florin-AS/Tally"]);
        assert_eq!(cfg.routes.len(), 2);
    }
}
