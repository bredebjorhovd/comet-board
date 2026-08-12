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
    // Only the credential this config cannot work without: routes were just
    // written for repos that answer 404 to a request with nothing on it.
    // Either one answers for GitHub (gh#58) — the App is the one that survives
    // somebody else installing the board on their own repos, and the token is
    // the one that needs nothing registering. Naming both here is the only
    // place most people will meet the choice.
    let mut missing = Vec::new();
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
    // Offered, not demanded: a GitHub-only board is a supported configuration
    // and listing the Linear key beside the GitHub one as a thing to "add" is
    // what made a working board look half-installed (gh#96).
    if linear_api_key(paths).is_none() {
        println!(
            "Linear is optional — this config polls GitHub only. Add LINEAR_API_KEY to {} \
             and re-run with --force to route Linear teams as well.",
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
# The identifier made branch-safe, then a short slug of the title: `gh#341`
# titled "The review page loads nothing" branches to
# `board/gh-341-review-page-loads`. A title that yields no slug leaves the
# identifier alone, which is always enough to name the task.
branch_template = "board/{{identifier_lower}}"
# What each branch is cut from. `origin/HEAD` is the remote's default branch,
# fetched before the worktree is cut, so a box whose folders sit on last week's
# checkout still starts agents on today's main. Name a branch (`main`,
# `origin/develop`) to pin one, or `HEAD` to branch from the checkout without
# fetching — which is what a repo with no remote needs.
base = "origin/HEAD"
# How long a finished attempt's checkout — and the branch under it — is kept
# before the board deletes both. The clock starts when the task leaves the board
# (merged, closed upstream, marked done), never while an attempt is live or a
# pull request is open, so a week here is a week to go and look at what an agent
# actually did. `off` keeps every checkout forever, which is what a box does
# until somebody notices the disk; `comet-board doctor` reports the running cost
# either way.
retain_worktrees = "7d"
# And how long the build output *inside* that checkout is kept — `target/`,
# `node_modules/`, `.next/`, `.turbo/`. A much shorter clock, because the two are
# kept for different reasons: a checkout is 14 MB of evidence somebody may go
# back for, and its `target/` is 20–36 GB of cache nothing reads once the run has
# ended. `on-settle` sweeps it as the attempt ends — it does not wait for the
# task to leave the board or for a pull request to close, and the only guard is
# that nobody is building in there. A review comment that restarts the agent
# rebuilds, slower; the alternative is paying tens of gigabytes per attempt for
# the chance of saving one build. Write a duration (`2h`, `1d`) to buy that
# rebuild back on a box with disk to spare. `off` keeps every cache for as long
# as its checkout lives, which is how eight checkouts became 109 GiB.
retain_build_output = "on-settle"
# And when its *chat* leaves the space's shelf. `on-settle` is no window at all:
# the same guards hold it — never while an attempt is live or blocked, never
# while a pull request is in review, never the pinned orchestrator, never a chat
# still waiting on work it released, never a chat you made yourself — and once
# none of them do, the task has merged or closed and its row has nothing left to
# say. Write a duration (`2d`, `1w`) for a grace
# period instead. Archiving is not deleting: the transcript is intact and
# Settings → Archived puts it back. `off` keeps every finished chat in the
# sidebar, which is how a shelf becomes a landfill at agent throughput. Set it
# per route (`archive_chats` on a `[[route]]`) where one space wants longer.
archive_chats = "on-settle"
# When an agent releases work through the board, prompt it in its own chat as
# that work settles or blocks. On, and tried first: it is the precise channel —
# the reader is the one agent whose plan that task was a step in. Turn it off if
# you dispatch by hand rather than from orchestrators, and every settle falls to
# the pin below instead.
# notify_dispatcher = true
# Where the board tells *you* — one URL, POSTed a small JSON body when a
# dispatched attempt blocks or settles (`{{"event": "on_blocked", …}}`). This is
# the only channel that reaches you when you are looking at neither the board
# nor the tracker; without it, an agent that stops to ask at 02:00 is a comment
# on its issue and a coloured row, and nothing else. `notify` switches it off
# without deleting the URL. `doctor` reports which of the two is missing.
# notify_webhook = "https://hooks.example.com/comet-board"
# One chat pinned as this board's orchestrator: it hears what nobody else can be
# told — work you released from the panel or the phone, work whose dispatching
# chat has been archived since, and every cap warning — and can drive the board
# with the ordinary `comet-board` verbs. Set it from the desktop app or the TUI
# ("Pin as orchestrator" on a session) rather than by hand — they know the chat
# id. Unset, those three reach no agent at all and the log says so once per
# event. `docs/orchestrator.md` is the brief to open that chat with.
# orchestrator_chat = "chat_01J8Z…"
# Whether a dispatch writes the board's conventions into the instruction file
# its runtime reads on its own — `CLAUDE.md` in the Claude config dir,
# `AGENTS.md` in `CODEX_HOME`. On, because a Codex agent has no skill mechanism
# and without this is the one agent on the box that has never heard of the board
# that started it. Only the text between the `comet-board conventions` markers
# is ours: your own instruction file is left exactly as it was around it, and
# turning this off takes the block back out on the next dispatch. Worth turning
# off on a box where a dispatch names no account — that one reads your own
# ~/.claude/CLAUDE.md, so the block lands there. Also settable per route.
# agent_instructions = true

[linear]
# Which state means "finished, waiting on a human". Uncomment and Linear moves
# there when an attempt settles; leave it commented and the ticket stays In
# Progress until its pull request merges. It has to be a name — `In Review` and
# `In Progress` are both `type: started`, so the API cannot be asked which one
# means review. `doctor` checks whatever you write here resolves.
# review_state = "In Review"

# Who drives this board, on GitHub. A dispatch records the email of whoever
# released it; with an address here, their dispatches commit as them — GitHub
# attributes the work to their account, and a deploy gate that checks the commit
# author (Vercel's does) lets their push through. Without an entry, everything
# commits under this box's own git identity, whoever released it — which is
# right on a box only you dispatch from. The `<id>+<login>@users.noreply.github.com`
# form is on https://github.com/settings/emails and always attributes; a plain
# address works too, as long as it is verified on that account.
[users]
# "ana@example.com" = "22494697+ana@users.noreply.github.com"

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
            branch: None,
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
