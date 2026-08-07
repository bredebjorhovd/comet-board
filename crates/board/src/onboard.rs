//! `comet-board onboard <owner/repo>` — clone, space, adopt in one verb (gh#97).
//!
//! Putting a repo on the board took three mechanisms that knew nothing about
//! each other: a clone somebody made on the box by hand, a `createSpace` from
//! the desktop (or, the night this was written, a hand-built RPC seeder), and
//! `comet-board adopt`. The App side had already stopped needing a human —
//! all-repos installations — and gh#75 made routing remote. The first two steps
//! were the last thing that still required a shell on the box.
//!
//! So this is one verb, and everything it does happens **on the device that
//! hosts the board**:
//!
//! 1. **Resolve** the repo against GitHub under the board's own credential. Not
//!    a formality — the laptop running the CLI usually has no GitHub credential
//!    at all, and a repo the App cannot see is one that would clone, get a
//!    space, get a route, and then poll nothing forever.
//! 2. **Clone**, with the askpass minting gh#68 built rather than with whoever
//!    owns the box. The board's credential is the one that will push the branch;
//!    it should be the one that fetched the code.
//! 3. **`createSpace`** for the checkout — the same op the desktop picker sends.
//! 4. **Adopt** — the route and the `[github] repos` half, through the same
//!    validating writer every other config edit goes through.
//!
//! ## Idempotence
//!
//! Every step answers "already done" rather than failing, because the failure
//! mode this verb exists to remove is a half-onboarded repo: a clone with no
//! space, a space with no route, a route for a repo nothing polls. Re-running it
//! has to be the fix, not a second mess. So an existing checkout of the right
//! repo is reused, an existing space for that path is reused (the engine dedupes
//! on `(device, path)` anyway), and a repo already both polled and routed says
//! so and writes nothing.
//!
//! What is *not* idempotent-by-reuse: a directory that exists and is not a
//! checkout of this repo. That is refused. Cloning over somebody's files, or
//! adopting a checkout of a different repo under the name of this one, are both
//! worse than stopping.
//!
//! This module holds the decisions; the engine (`crates/engine/src/rpc.rs`)
//! holds the effects, because the clone is `repos.rs`'s and the space is the
//! workspace doc's.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::adopt::{Adopted, Missing, RepoPreview};
use crate::config::{RoutingConfig, expand_tilde};
use crate::sources::github_app::TokenProvider;
use comet_proto::view::repos::SpaceSlug;

/// Whether a step created something or found it already there.
///
/// Reported per step rather than folded into one "ok": re-running `onboard`
/// after a half-finished first run is the intended repair, and the operator
/// needs to see *which* half was missing to trust that it is now whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Created,
    Reused,
}

impl Step {
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Created => "created",
            Step::Reused => "reused",
        }
    }
}

/// What one `onboard` did, end to end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Onboarded {
    /// `owner/repo` as GitHub spells it, not as it was typed.
    pub slug: String,
    /// The device that did all of it — the board's host.
    pub device_id: String,
    /// The checkout on that device.
    pub path: String,
    pub clone: Step,
    pub space: Step,
    pub space_id: String,
    /// The space's display name, which is what the written route's `workspace =`
    /// names.
    pub space_name: String,
    /// What was written to `routing.toml`. `None` = the repo was already both
    /// polled and routed, and nothing needed writing.
    pub adopted: Option<Adopted>,
    /// What the repo would contribute, when it could be asked. Best-effort: no
    /// network degrades to onboarding without the numbers, exactly as `adopt`'s
    /// preview does.
    pub preview: Option<RepoPreview>,
    /// Whether the board will comment on and close this repo's issues.
    ///
    /// Reported, never set. Writeback is off by default on purpose — writing to
    /// somebody's issues is not a thing to start doing because a repo was
    /// pointed at the board — so onboarding says where it stands and leaves the
    /// decision where it was.
    pub writeback: bool,
    /// Issues are switched off on the repo, so polling it polls nothing.
    pub issues_disabled: bool,
    /// The repo is archived — onboarding one is almost never what was meant.
    pub archived: bool,
}

impl Onboarded {
    /// The one-line summary, for a CLI that has just finished.
    pub fn summary(&self) -> String {
        format!(
            "{} → {} (clone {}, space {}, {})",
            self.slug,
            self.path,
            self.clone.as_str(),
            self.space.as_str(),
            match &self.adopted {
                None => "already routed".to_string(),
                Some(a) => wrote_phrase(a),
            }
        )
    }

    /// The notes worth printing after the summary — the things that will
    /// otherwise be discovered as a board that stays empty.
    pub fn notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.archived {
            out.push(format!(
                "{} is archived — its issues are read-only, and nothing dispatched \
                 against it can be merged",
                self.slug
            ));
        }
        if self.issues_disabled {
            out.push(format!(
                "issues are disabled on {} — the board will poll it and find nothing",
                self.slug
            ));
        }
        if !self.writeback {
            out.push(
                "writeback is off for this repo: the board will not comment on or close \
                 its issues. `[[github.repo]] writeback = true` turns it on"
                    .to_string(),
            );
        }
        out
    }
}

/// What one adoption wrote, in words.
fn wrote_phrase(a: &Adopted) -> String {
    let mut wrote = Vec::new();
    if a.wrote_route {
        wrote.push("a [[route]]".to_string());
    }
    if a.wrote_repo {
        wrote.push("[github] repos".to_string());
    }
    if let Some(l) = &a.labels {
        wrote.push(if l.is_empty() {
            "a [[github.repo]] polling every open issue".to_string()
        } else {
            format!("a [[github.repo]] filter: {}", l.join(", "))
        });
    }
    if wrote.is_empty() {
        return "nothing to write".to_string();
    }
    format!("wrote {}", wrote.join(" + "))
}

// ---- what was asked for -------------------------------------------------

/// `owner/repo` out of whatever was typed.
///
/// A repo gets named three ways in practice — as a slug, as the browser URL
/// somebody copied, and as the clone URL — and refusing two of them teaches
/// nothing. What is *not* accepted is a bare name: `comet-board` alone could
/// mean any owner's, and guessing at the operator's would onboard a stranger's
/// repo under a name they recognise.
pub fn parse_slug(input: &str) -> Result<String> {
    let raw = input.trim();
    if raw.is_empty() {
        bail!("name a repo: comet-board onboard <owner/repo>");
    }
    // A URL in any of its forms is the remote parser's problem, and it already
    // knows every shape of one.
    if let Some(slug) = crate::adopt::github_slug(raw) {
        return Ok(slug);
    }
    let cleaned = raw.trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<&str> = cleaned.split('/').collect();
    if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
        let (owner, repo) = (parts[0], parts[1]);
        if owner.chars().all(is_slug_char) && repo.chars().all(is_slug_char) {
            return Ok(format!("{owner}/{repo}"));
        }
    }
    bail!(
        "`{raw}` is not an owner/repo — give it as `owner/repo`, or paste the \
         repository's github.com URL"
    )
}

/// What GitHub allows in an owner or repo name. Deliberately narrow: everything
/// here ends up in a URL, in a shell-adjacent path, and in a TOML file.
fn is_slug_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// The bare repo name — `Florin-AS/tripletex-mcp` → `tripletex-mcp`.
pub fn repo_name(slug: &str) -> &str {
    slug.rsplit('/').next().unwrap_or(slug)
}

/// Where the checkout goes.
///
/// `--dir` names the **checkout itself**, not a parent to clone into: `--dir
/// ~/dev/comet-board` is the folder that ends up holding the repo. That reads
/// the way people write it, and the alternative — a parent, with the repo name
/// appended — makes `--dir ~/dev/comet-board` silently produce
/// `~/dev/comet-board/comet-board`.
///
/// `~` is expanded **here**, which is to say on the board's device: an unquoted
/// `~/dev/x` was already expanded by the caller's shell against the caller's
/// home, and a quoted one means the box's home, which is the only home that can
/// be right for a path on the box.
///
/// A relative path is refused for the same reason. Relative to *what*? The
/// engine's working directory on a machine the caller is not sitting at is not
/// something anybody means, and resolving it silently would put the checkout
/// somewhere nobody could predict.
pub fn checkout_path(dir: Option<&str>, slug: &str, default_root: &Path) -> Result<PathBuf> {
    let Some(d) = dir.map(str::trim).filter(|d| !d.is_empty()) else {
        return Ok(default_root.join(repo_name(slug)));
    };
    let path = expand_tilde(d);
    if !path.is_absolute() {
        bail!(
            "--dir must be an absolute path on the board's device (got `{d}`) — a \
             relative one would resolve against the engine's working directory there, \
             which is nobody's idea of where to put a checkout"
        );
    }
    Ok(path)
}

// ---- the existing-clone decision ----------------------------------------

/// What is already at the checkout path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtPath {
    /// Nothing there — clone.
    Vacant,
    /// A checkout of this repo — reuse it.
    SameRepo,
    /// Something else is there, and it is not ours to overwrite.
    Occupied(String),
}

/// Decide what to do about whatever is at the checkout path.
///
/// `remote` is the checkout's `origin`, when the path is a git work tree at all;
/// `exists` is whether anything is there. Split this way so the whole rule is
/// testable without a checkout on disk — the same shape [`crate::adopt::detect`]
/// takes its probe in.
pub fn at_path(path: &Path, exists: bool, remote: Option<&str>, slug: &str) -> AtPath {
    if !exists {
        return AtPath::Vacant;
    }
    let Some(remote) = remote else {
        return AtPath::Occupied(format!(
            "{} already exists and is not a git checkout — onboarding will not \
             clone over it. Move it, or point --dir somewhere else",
            path.display()
        ));
    };
    match crate::adopt::github_slug(remote) {
        Some(found) if found.eq_ignore_ascii_case(slug) => AtPath::SameRepo,
        // A checkout of a *different* repo is the dangerous case: everything
        // downstream would succeed, and the board would dispatch this repo's
        // issues into somebody else's code.
        Some(found) => AtPath::Occupied(format!(
            "{} is a checkout of {found}, not {slug} — onboarding it would route \
             {slug}'s issues into the wrong repo",
            path.display()
        )),
        None => AtPath::Occupied(format!(
            "{} is a git checkout whose origin ({remote}) is not a GitHub remote \
             for {slug}",
            path.display()
        )),
    }
}

// ---- what to say when the App cannot see it -----------------------------

/// Why a repo the board cannot read is a repo nobody should onboard, in the
/// words of whoever can fix it.
///
/// GitHub answers 404 for "no such repo" and for "not yours" alike, so the
/// message cannot claim to know which — but it *can* name the credential in
/// force, because that is what decides who has to act. Under an App it is the
/// installer, and no amount of configuration on the box will help; under a
/// personal access token it is whoever owns the token.
pub fn unreachable(slug: &str, auth: &TokenProvider, err: &anyhow::Error) -> anyhow::Error {
    let fix = match auth {
        TokenProvider::App(app) => format!(
            "the board's GitHub App (id {}) cannot see it. Either the repo does not \
             exist, or the App is not installed on {}. Install it there — nothing on \
             this box can grant it",
            app.app_id(),
            owner_of(slug),
        ),
        TokenProvider::Static(_) => format!(
            "GITHUB_TOKEN cannot see it. Either the repo does not exist, or that \
             token is not scoped to {}",
            owner_of(slug),
        ),
        TokenProvider::Anonymous => "the board has no GitHub credential, so it can only \
             see public repos. Set GITHUB_TOKEN, or a GITHUB_APP_ID and \
             GITHUB_APP_PRIVATE_KEY_PATH pair"
            .to_string(),
    };
    anyhow::anyhow!("cannot onboard {slug}: {fix} ({err:#})")
}

fn owner_of(slug: &str) -> &str {
    slug.split('/').next().unwrap_or(slug)
}

// ---- the routing half ---------------------------------------------------

/// Whether adopting this repo has anything left to write, and what.
///
/// Straight through to [`crate::adopt::missing_for`] — onboarding must not have
/// its own opinion about what "on the board" means, or a repo could be onboarded
/// twice, or reported as onboarded while `adopt` still offers it.
pub fn routing_gap(cfg: &RoutingConfig, slug: &str) -> Option<Missing> {
    crate::adopt::missing_for(cfg, slug)
}

// ---- the picker's list --------------------------------------------------

/// One repo an "Onboard a repo…" picker can offer: what the App can see,
/// crossed with what the board already watches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub slug: String,
    pub private: bool,
    pub archived: bool,
    /// The installation granting it — what to name when a clone is refused.
    pub installation: i64,
    /// Which half of the config it lacks. `None` = already polled and routed,
    /// so onboarding it would write nothing.
    pub missing: Option<Missing>,
}

impl Candidate {
    /// Whether the board already both polls and routes it.
    pub fn on_board(&self) -> bool {
        self.missing.is_none()
    }

    /// The line under the slug in a picker.
    pub fn note(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.private {
            parts.push("private");
        }
        if self.archived {
            parts.push("archived");
        }
        parts.push(match self.missing {
            None => "on the board",
            Some(Missing::Both) => "not on the board",
            Some(Missing::Polling) => "routed, not polled",
            Some(Missing::Route) => "polled, no route",
        });
        parts.join(" · ")
    }
}

/// Cross the App's grant with the board's config.
///
/// Repos already on the board are kept rather than filtered out: a picker that
/// silently drops them cannot answer "is this repo already set up?", which is
/// the question somebody opening it half the time actually has.
pub fn candidates(
    repos: &[crate::sources::github_app::AppRepo],
    cfg: &RoutingConfig,
) -> Vec<Candidate> {
    repos
        .iter()
        .map(|r| Candidate {
            slug: r.slug.clone(),
            private: r.private,
            archived: r.archived,
            installation: r.installation,
            missing: routing_gap(cfg, &r.slug),
        })
        .collect()
}

/// Which of this device's spaces are checkouts of which GitHub repo.
///
/// The workspace doc names a space by *folder*, which is the wrong name for a
/// picker that asks "which repo?" (gh#118) — and the right one cannot be guessed
/// from the folder: `~/src/comet` is a name, not an owner. Only the device
/// holding the checkout can answer, by asking git for the remote, so this is
/// what it answers with. The row shape lives in `comet_proto` because three
/// frontends read it.
///
/// `probe_of` resolves a space *path* to what git says about it — injected for
/// the same reason [`crate::adopt::detect`] injects it, and expected to be
/// [`crate::adopt::probe`] in production. Callers pass only this device's
/// spaces: another device's folder is on another disk.
///
/// Deliberately *not* filtered by the routing config: this answers "what repo is
/// this space?", which is true whether or not the board watches it. Filtering by
/// what is adopted is [`candidates`]'s job, and doing it twice in two places
/// with two answers is the bug [`crate::adopt::missing_for`] exists to prevent.
///
/// Linked worktrees are skipped: an attempt's checkout is a copy of the project,
/// not the place to land somebody who picked the project.
pub fn space_links<F>(spaces: &[comet_proto::Space], probe_of: F) -> Vec<SpaceSlug>
where
    F: Fn(&str) -> Option<crate::adopt::SpaceRepo>,
{
    spaces
        .iter()
        .filter(|s| s.git_detected)
        .filter_map(|s| {
            let repo = probe_of(&s.path)?;
            if repo.linked_worktree {
                return None;
            }
            let slug = repo.remote.as_deref().and_then(crate::adopt::github_slug)?;
            Some(SpaceSlug {
                space_id: s.id.clone(),
                slug,
            })
        })
        .collect()
}

/// The labels a `--labels` / `--all-issues` pair means.
///
/// `--all-issues` is `Some(vec![])`, which writes `labels = []` and *overrides*
/// a narrower global filter; no flag at all is `None`, which keeps `[github]
/// labels`. The difference is the whole reason the flag exists.
pub fn label_filter(labels: Option<Vec<String>>, all_issues: bool) -> Option<Vec<String>> {
    if all_issues { Some(Vec::new()) } else { labels }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repo_can_be_named_as_a_slug_or_as_any_url_that_carries_one() {
        for input in [
            "bredebjorhovd/comet-board",
            "  bredebjorhovd/comet-board  ",
            "bredebjorhovd/comet-board/",
            "https://github.com/bredebjorhovd/comet-board",
            "https://github.com/bredebjorhovd/comet-board.git",
            "git@github.com:bredebjorhovd/comet-board.git",
            "ssh://git@github.com/bredebjorhovd/comet-board",
            // The form an App-authenticated clone leaves behind.
            "https://x-access-token@github.com/bredebjorhovd/comet-board.git",
        ] {
            assert_eq!(
                parse_slug(input).unwrap(),
                "bredebjorhovd/comet-board",
                "{input}"
            );
        }
    }

    #[test]
    fn a_bare_name_is_refused_rather_than_guessed_at() {
        // Guessing an owner would onboard a stranger's repo under a name the
        // operator recognises.
        for bad in ["comet-board", "", "   ", "a/b/c", "owner/", "/repo"] {
            let err = parse_slug(bad).unwrap_err().to_string();
            assert!(
                err.contains("owner/repo"),
                "{bad} → {err} (must say what to type)"
            );
        }
        // A slug carrying something that has no business in a URL or a path.
        assert!(parse_slug("owner/re po").is_err());
        assert!(parse_slug("owner/re;po").is_err());
    }

    #[test]
    fn dir_names_the_checkout_and_is_expanded_on_the_board_device() {
        let root = Path::new("/data/repos");
        // No flag: the engine's own clone root, under the repo's bare name.
        assert_eq!(
            checkout_path(None, "Florin-AS/tripletex-mcp", root).unwrap(),
            Path::new("/data/repos/tripletex-mcp")
        );
        // The flag IS the checkout. Appending the repo name here would turn
        // `--dir ~/dev/comet-board` into `~/dev/comet-board/comet-board`.
        assert_eq!(
            checkout_path(Some("/srv/dev/board"), "o/board", root).unwrap(),
            Path::new("/srv/dev/board")
        );
        // Empty reads as absent rather than as the root directory.
        assert_eq!(
            checkout_path(Some("  "), "o/board", root).unwrap(),
            Path::new("/data/repos/board")
        );
        // `~` is the *box's* home — the only home a path on the box can mean.
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        assert_eq!(
            checkout_path(Some("~/dev/board"), "o/board", root).unwrap(),
            Path::new(&home).join("dev/board")
        );
        // Relative to what? Not the caller's shell — they are not on that
        // machine — and the engine's working directory there is nobody's idea
        // of where a checkout goes.
        let err = checkout_path(Some("dev/board"), "o/board", root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute path"), "{err}");
    }

    #[test]
    fn an_existing_checkout_of_the_same_repo_is_reused() {
        let p = Path::new("/dev/board");
        assert_eq!(at_path(p, false, None, "o/board"), AtPath::Vacant);
        assert_eq!(
            at_path(p, true, Some("https://github.com/o/board.git"), "o/board"),
            AtPath::SameRepo
        );
        // Casing is GitHub's business, not a reason to re-clone.
        assert_eq!(
            at_path(p, true, Some("git@github.com:O/Board.git"), "o/board"),
            AtPath::SameRepo
        );
    }

    #[test]
    fn a_directory_holding_something_else_is_refused_not_cloned_over() {
        let p = Path::new("/dev/board");
        // The dangerous one: everything downstream would succeed, and the board
        // would dispatch this repo's issues into another repo's code.
        let AtPath::Occupied(why) = at_path(p, true, Some("git@github.com:o/other.git"), "o/board")
        else {
            panic!("a checkout of another repo must be refused");
        };
        assert!(why.contains("o/other"), "{why}");
        assert!(why.contains("wrong repo"), "{why}");

        // Not a checkout at all.
        let AtPath::Occupied(why) = at_path(p, true, None, "o/board") else {
            panic!("an occupied directory must be refused");
        };
        assert!(why.contains("not a git checkout"), "{why}");

        // A checkout of something that is not on GitHub.
        let AtPath::Occupied(why) = at_path(p, true, Some("git@gitlab.com:o/board.git"), "o/board")
        else {
            panic!("a non-GitHub origin must be refused");
        };
        assert!(why.contains("not a GitHub remote"), "{why}");
    }

    #[test]
    fn an_unreachable_repo_names_whoever_can_actually_fix_it() {
        let boom = || anyhow::anyhow!("github HTTP 404 for /repos/acme/thing");

        // Under an App the operator cannot fix it from the box at all — the
        // installer has to act, and saying anything else sends them to the
        // wrong file.
        let (app, _, _) = crate::sources::github_app::test_app(&[]);
        let err = unreachable("acme/thing", &TokenProvider::App(app), &boom()).to_string();
        assert!(err.contains("not installed on acme"), "{err}");
        assert!(err.contains("nothing on this box can grant it"), "{err}");

        let err = unreachable(
            "acme/thing",
            &TokenProvider::Static("ghp_x".into()),
            &boom(),
        )
        .to_string();
        assert!(err.contains("GITHUB_TOKEN"), "{err}");
        assert!(err.contains("scoped to acme"), "{err}");

        let err = unreachable("acme/thing", &TokenProvider::Anonymous, &boom()).to_string();
        assert!(err.contains("no GitHub credential"), "{err}");
    }

    #[test]
    fn all_issues_writes_an_empty_list_and_no_flag_writes_nothing() {
        // `labels = []` overrides a narrower global filter; absent falls back to
        // it. Collapsing the two would silently widen or narrow every onboard.
        assert_eq!(label_filter(None, true), Some(Vec::new()));
        assert_eq!(label_filter(None, false), None);
        assert_eq!(
            label_filter(Some(vec!["ready".into()]), false),
            Some(vec!["ready".to_string()])
        );
    }

    fn cfg(toml_text: &str) -> RoutingConfig {
        toml::from_str(toml_text).unwrap()
    }

    #[test]
    fn the_routing_gap_is_adoptions_own_answer() {
        let both = cfg("");
        assert_eq!(routing_gap(&both, "o/r"), Some(Missing::Both));

        let polled = cfg("[github]\nrepos = [\"o/r\"]\n");
        assert_eq!(routing_gap(&polled, "o/r"), Some(Missing::Route));

        let whole = cfg("[github]\nrepos = [\"o/r\"]\n\n\
             [[route]]\nmatch = { gh_repo = \"o/r\" }\nworkspace = \"w\"\n\
             repo = \"/tmp\"\nruntime = \"claude-code\"\n");
        assert_eq!(routing_gap(&whole, "o/r"), None, "nothing left to write");
    }

    #[test]
    fn the_picker_keeps_repos_that_are_already_on_the_board() {
        use crate::sources::github_app::AppRepo;
        let repos = vec![
            AppRepo {
                slug: "o/watched".into(),
                private: true,
                archived: false,
                default_branch: "main".into(),
                installation: 42,
            },
            AppRepo {
                slug: "o/fresh".into(),
                private: false,
                archived: true,
                default_branch: "main".into(),
                installation: 42,
            },
        ];
        let cfg = cfg("[github]\nrepos = [\"o/watched\"]\n\n\
             [[route]]\nmatch = { gh_repo = \"o/watched\" }\nworkspace = \"w\"\n\
             repo = \"/tmp\"\nruntime = \"claude-code\"\n");
        let out = candidates(&repos, &cfg);
        // Kept, not filtered: "is this one already set up?" is half of why the
        // picker gets opened.
        assert!(out[0].on_board());
        assert_eq!(out[0].note(), "private · on the board");
        assert!(!out[1].on_board());
        assert_eq!(out[1].note(), "archived · not on the board");
    }

    #[test]
    fn the_summary_says_which_halves_were_already_there() {
        let mut done = Onboarded {
            slug: "o/r".into(),
            device_id: "dev-box".into(),
            path: "/dev/r".into(),
            clone: Step::Reused,
            space: Step::Created,
            space_id: "space-1".into(),
            space_name: "r".into(),
            adopted: None,
            preview: None,
            writeback: false,
            issues_disabled: false,
            archived: false,
        };
        let s = done.summary();
        assert!(s.contains("clone reused"), "{s}");
        assert!(s.contains("space created"), "{s}");
        assert!(s.contains("already routed"), "{s}");

        done.adopted = Some(Adopted {
            wrote_route: true,
            wrote_repo: true,
            suggested_label: "r".into(),
            labels: Some(vec!["ready".into()]),
        });
        let s = done.summary();
        assert!(s.contains("wrote a [[route]] + [github] repos"), "{s}");
        assert!(s.contains("filter: ready"), "{s}");

        // Writeback off is the default and worth one line — a board that never
        // comments is otherwise discovered by noticing it never commented.
        assert!(
            done.notes().iter().any(|n| n.contains("writeback is off")),
            "{:?}",
            done.notes()
        );
        done.writeback = true;
        assert!(done.notes().is_empty());

        done.archived = true;
        done.issues_disabled = true;
        let notes = done.notes().join(" | ");
        assert!(notes.contains("archived"), "{notes}");
        assert!(notes.contains("issues are disabled"), "{notes}");
    }

    fn space(id: &str, path: &str, git: bool) -> comet_proto::Space {
        comet_proto::Space {
            id: id.into(),
            device_id: "dev-box".into(),
            path: path.into(),
            name: None,
            git_detected: git,
            git_checked_at: None,
            checkout_id: None,
            created_at: chrono::Utc::now(),
        }
    }

    /// The picker names a space by its repo, so a space has to be resolvable to
    /// one — and only the things that really are one.
    #[test]
    fn a_space_is_linked_to_the_repo_its_checkout_actually_points_at() {
        let spaces = vec![
            space("s-comet", "/box/src/comet-board", true),
            space("s-notes", "/box/notes", false),
            space("s-nogh", "/box/src/vendored", true),
            space("s-tree", "/box/src/comet-board-wt", true),
        ];
        let probe = |path: &str| match path {
            "/box/src/comet-board" => Some(crate::adopt::SpaceRepo {
                root: "/box/src/comet-board".into(),
                remote: Some("git@github.com:bredebjorhovd/comet-board.git".into()),
                linked_worktree: false,
            }),
            // A git checkout whose remote is not GitHub names no `owner/repo`.
            "/box/src/vendored" => Some(crate::adopt::SpaceRepo {
                root: "/box/src/vendored".into(),
                remote: Some("git@git.sr.ht:~x/vendored".into()),
                linked_worktree: false,
            }),
            // An attempt's worktree is a copy of the project, not the project.
            "/box/src/comet-board-wt" => Some(crate::adopt::SpaceRepo {
                root: "/box/src/comet-board-wt".into(),
                remote: Some("https://github.com/bredebjorhovd/comet-board".into()),
                linked_worktree: true,
            }),
            _ => None,
        };
        let links = space_links(&spaces, probe);
        assert_eq!(
            links,
            vec![SpaceSlug {
                space_id: "s-comet".into(),
                slug: "bredebjorhovd/comet-board".into(),
            }]
        );
    }
}
