//! Repos that have a comet space but nothing on the board watching them.
//!
//! Creating a space for a new repo used to leave the board silent: its issues
//! were not polled and nothing said so. The two pieces of config that fix that
//! are independent — `[github] repos` controls **visibility**, a `[[route]]`
//! controls **dispatchability** — which is exactly what makes it easy to
//! half-fix. Miss the route and the row appears but dispatch refuses it; miss
//! the repos entry and the issue never arrives at all.
//!
//! So: **detect automatically, adopt explicitly.** Detection is cheap — the
//! workspace doc already stamps `git_detected` on every space, so the only
//! probing left is asking git for the remote. Writing is not silent, because
//! `routing.toml` is hand-edited and documented as "not managed config", and a
//! space gets created for plenty of repos that do not belong on the board.
//!
//! Everything written here goes through [`apply`], which re-parses and
//! *validates* the result before it replaces the file. A writer that could emit
//! a config `doctor` refuses would be worse than no writer at all. That is now
//! the discipline for *every* write to this file: [`crate::routes`] is the rest
//! of the surface (gh#75) and goes through the same [`apply`].
//!
//! Ported from herdr-board's `adopt.rs`; a herdr *workspace* is a comet
//! *space*. The label-picker flow survives as [`preview`] + the `labels`
//! argument to [`adopt_with`] — same decision, different surface (CLI flags
//! now, §board-view's screen later).

use crate::config::{RoutingConfig, shorten_home};
use anyhow::{Context, Result, bail};
use comet_proto::Space;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Which half of the config is missing.
///
/// Named rather than implied: "the row appears but cannot be dispatched" and
/// "the issue never arrives" are different bugs with the same cause, and an
/// operator looking at the list deserves to know which one they have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Missing {
    /// Not under `[github] repos`: nothing polls it.
    Polling,
    /// No route *names* it, so its rows either cannot be dispatched at all or
    /// would be dispatched into a catch-all's space instead of its own.
    Route,
    Both,
}

impl Missing {
    /// Suffix for one offered row.
    ///
    /// Says what is present as well as what is not. A bare "no route" on a repo
    /// the board *is* polling reads as "this space cannot be dispatched to",
    /// which is false whenever another route (a label match, a catch-all)
    /// already covers it.
    pub fn note(self) -> &'static str {
        match self {
            Missing::Polling => " · routed, not polled",
            Missing::Route => " · polled, no route for its issues",
            Missing::Both => "",
        }
    }
}

/// A git repo with a comet space that the board is not watching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unadopted {
    /// Space display name — the name the operator recognises, and what the
    /// written route's `workspace =` names.
    pub label: String,
    /// `owner/repo`, from the git remote.
    pub slug: String,
    /// Repo root on disk, for the route's `repo =`.
    pub repo_root: String,
    pub missing: Missing,
}

impl Unadopted {
    /// Selection id for a board view. The NUL prefix keeps it from ever
    /// colliding with a task id, exactly as section headers do.
    pub fn row_id(&self) -> String {
        row_id(&self.slug)
    }

    /// The bare repo name — `Florin-AS/tripletex-mcp` → `tripletex-mcp`.
    pub fn name(&self) -> &str {
        self.slug.rsplit('/').next().unwrap_or(&self.slug)
    }
}

pub fn row_id(slug: &str) -> String {
    format!("\u{0}unadopted:{slug}")
}

// ---- detection ---------------------------------------------------------

/// What git says about one space's folder.
///
/// A struct rather than three probes so [`detect`] takes one injected resolver
/// and the whole decision is testable without a checkout on disk. Production
/// callers pass [`probe`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceRepo {
    /// Repo root (`rev-parse --show-toplevel`) — the space path may sit
    /// anywhere inside the work tree.
    pub root: String,
    /// `remote get-url origin`, when there is one.
    pub remote: Option<String>,
    /// A linked worktree is an attempt's checkout, not a project to route to.
    pub linked_worktree: bool,
}

/// `owner/repo` from a git remote URL, for the SSH and HTTPS forms.
///
/// HTTPS remotes may carry userinfo — `https://x-access-token@github.com/o/r` is
/// what an App-authenticated clone starts life with (gh#97), and a remote a
/// human set up with their own username in it is just as real. The credential
/// half of a URL says nothing about which repo it names, so it is dropped rather
/// than allowed to make the remote unreadable.
pub fn github_slug(remote: &str) -> Option<String> {
    let r = remote.trim().trim_end_matches(".git");
    let rest = r
        .strip_prefix("git@github.com:")
        .or_else(|| r.strip_prefix("https://github.com/"))
        .or_else(|| r.strip_prefix("ssh://git@github.com/"))
        .or_else(|| strip_userinfo_https(r))?;
    let mut parts = rest.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// `https://<userinfo>@github.com/<rest>` → `<rest>`. Only github.com: a
/// userinfo form for another host is not a GitHub remote and must not be read
/// as one.
fn strip_userinfo_https(r: &str) -> Option<&str> {
    let after_scheme = r.strip_prefix("https://")?;
    let (userinfo, rest) = after_scheme.split_once('@')?;
    // A `@` after the first slash is inside the path, not userinfo.
    if userinfo.contains('/') {
        return None;
    }
    rest.strip_prefix("github.com/")
}

/// Which spaces the board is not watching.
///
/// `probe_of` resolves a space *path* to what git says about it; injected so
/// the whole decision is testable without a git checkout on disk. Callers are
/// expected to pass only this device's spaces: a route's `repo =` is a local
/// path, and probing another device's folder answers about the wrong disk.
pub fn detect<F>(spaces: &[Space], cfg: &RoutingConfig, probe_of: F) -> Vec<Unadopted>
where
    F: Fn(&str) -> Option<SpaceRepo>,
{
    let mut out: Vec<Unadopted> = Vec::new();
    for s in spaces {
        // The owning device already answered "is this a repo?" — a space that
        // is not `git_detected` has nothing to poll and nowhere to dispatch.
        if !s.git_detected {
            continue;
        }
        let Some(repo) = probe_of(&s.path) else {
            continue;
        };
        // Linked worktrees are attempts' checkouts, not projects.
        if repo.linked_worktree {
            continue;
        }
        // Without a GitHub remote there is nothing to write: `[github] repos`
        // and `match.gh_repo` both name an `owner/repo`, and a git remote that
        // is not GitHub cannot supply one.
        let Some(slug) = repo.remote.as_deref().and_then(github_slug) else {
            continue;
        };
        if cfg.adopt.ignores(&slug) {
            continue;
        }

        let Some(missing) = missing_for(cfg, &slug) else {
            continue;
        };
        // Two spaces can sit on one repo — the checkout and a clone of it. The
        // repo is what gets adopted, so it is offered once.
        if out.iter().any(|u| u.slug.eq_ignore_ascii_case(&slug)) {
            continue;
        }
        out.push(Unadopted {
            label: s.display_name().to_string(),
            slug,
            repo_root: repo.root,
            missing,
        });
    }
    out.sort_by(|a, b| a.slug.cmp(&b.slug));
    out
}

/// Which half of the config a repo is missing, or `None` when the board already
/// both polls and routes it.
///
/// The decision [`detect`] makes about one repo, named so `onboard` (gh#97) can
/// ask it about a repo that has no space yet — a clone it is about to make. Two
/// implementations of "is this repo on the board" would be two answers to the
/// question adoption exists to settle.
pub fn missing_for(cfg: &RoutingConfig, slug: &str) -> Option<Missing> {
    let polled = cfg
        .github
        .repos
        .iter()
        .any(|r| r.eq_ignore_ascii_case(slug));
    // A route that *names* this repo, not merely one that would match it.
    //
    // A catch-all matches everything, so asking `resolve` would call this repo
    // routed — and it would dispatch, into whatever space the catch-all names.
    // Starting an agent for a tripletex-mcp issue in somebody else's checkout is
    // a worse silent failure than the one this whole feature exists to remove,
    // so a catch-all does not count.
    let routed = cfg.routes.iter().any(|r| {
        r.match_
            .gh_repo
            .as_deref()
            .is_some_and(|g| g.eq_ignore_ascii_case(slug))
    });
    match (polled, routed) {
        (true, true) => None,
        (false, true) => Some(Missing::Polling),
        (true, false) => Some(Missing::Route),
        (false, false) => Some(Missing::Both),
    }
}

/// What git says about a folder — the production resolver for [`detect`].
pub fn probe(path: &str) -> Option<SpaceRepo> {
    let root = git_toplevel(path)?;
    // A linked worktree's git dir lives under the primary checkout's
    // `.git/worktrees/`; the primary's git dir IS its common dir.
    let git_dir = git_out(&root, &["rev-parse", "--absolute-git-dir"])?;
    let common = git_out(&root, &["rev-parse", "--git-common-dir"]);
    let linked_worktree = common.is_some_and(|c| {
        let common_abs = if Path::new(&c).is_absolute() {
            c
        } else {
            format!("{root}/{c}")
        };
        git_dir != common_abs
    });
    Some(SpaceRepo {
        remote: git_remote(&root),
        root,
        linked_worktree,
    })
}

fn git_out(dir: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The git remote a checkout pushes to, or `None`.
pub fn git_remote(repo_root: &str) -> Option<String> {
    git_out(repo_root, &["remote", "get-url", "origin"])
}

/// The repo root containing `dir`, if any.
///
/// A space path can be anywhere inside a checkout — or nowhere near one — so
/// this asks git rather than assuming the path *is* the root.
pub fn git_toplevel(dir: &str) -> Option<String> {
    git_out(dir, &["rev-parse", "--show-toplevel"])
}

// ---- what adopting is about to pull ------------------------------------

/// The open issues a repo would contribute, and the labels they carry.
///
/// Adoption already knows the repo, so it can ask before it writes. Pointing
/// the board at `bredebjorhovd/itsm-agent` put 83 rows on it in one poll — 76%
/// of everything not done — and nothing had been wrong: `labels = []` means
/// "every open issue", and that repo has 83 of them. The information needed to
/// poll only what is current was already on those issues, unused.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RepoPreview {
    /// Open issues, pull requests excluded.
    pub open_issues: usize,
    /// The page was full, so the count is a floor rather than a total.
    pub truncated: bool,
    /// Labels present on those issues, commonest first, then alphabetical.
    pub labels: Vec<(String, usize)>,
}

impl RepoPreview {
    /// How many issues that label alone would let through.
    pub fn count_of(&self, label: &str) -> usize {
        self.labels
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }

    /// How many issues a set of labels would let through. GitHub's `labels=`
    /// filter is an AND, so this is the smallest of them — never the sum, which
    /// would over-promise on issues carrying two of the chosen labels.
    pub fn count_for(&self, labels: &[String]) -> usize {
        labels
            .iter()
            .map(|l| self.count_of(l))
            .min()
            .unwrap_or(self.open_issues)
    }

    /// `83 open issues` / `100+ open issues`.
    pub fn count_phrase(&self) -> String {
        format!(
            "{}{} open issue{}",
            self.open_issues,
            if self.truncated { "+" } else { "" },
            if self.open_issues == 1 && !self.truncated {
                ""
            } else {
                "s"
            }
        )
    }
}

/// Ask GitHub what adopting this repo unfiltered would put on the board.
pub fn preview<T: crate::sources::github::Rest>(
    gh: &crate::sources::github::Github<T>,
    repo: &str,
) -> Result<RepoPreview> {
    let issues = gh.open_issues(repo)?;
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for issue in &issues {
        for label in &issue.labels {
            *counts.entry(label.clone()).or_default() += 1;
        }
    }
    let mut labels: Vec<(String, usize)> = counts.into_iter().collect();
    // Commonest first: the label that would let most of the backlog through is
    // the one worth seeing, and the tail is what you scroll for.
    labels.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(RepoPreview {
        open_issues: issues.len(),
        truncated: issues.len() >= crate::sources::github::PAGE,
        labels,
    })
}

// ---- writing routing.toml ----------------------------------------------

/// What one adoption wrote, for the caller to report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Adopted {
    pub wrote_route: bool,
    pub wrote_repo: bool,
    /// The `[[github.repo]] labels` written, if the operator picked any.
    pub labels: Option<Vec<String>>,
}

/// Adopt a repo, polling it for everything its `[github] labels` lets through.
pub fn adopt(path: &Path, u: &Unadopted) -> Result<Adopted> {
    adopt_with(path, u, None)
}

/// Adopt a repo: write the route it needs and the `[github] repos` entry, and
/// leave a suggestion for the one thing that cannot be derived.
///
/// `labels` is what the operator chose (CLI `--labels`, or §board-view's
/// picker). `None` keeps the global `[github] labels` — which is what adoption
/// always did, and is right whenever the repo's tracker is curated.
pub fn adopt_with(path: &Path, u: &Unadopted, labels: Option<&[String]>) -> Result<Adopted> {
    let before = read(path)?;
    let mut text = before.clone();
    let mut wrote_route = false;
    let mut wrote_repo = false;

    if u.missing != Missing::Polling {
        // Parsed from the file rather than taken from the caller's config: a
        // config the caller failed to load would leave them holding an empty
        // route list, and the insertion point is computed from that list.
        let cfg: RoutingConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        text = insert_route(&text, &cfg, u)?;
        wrote_route = true;
    }
    if u.missing != Missing::Route {
        text = add_to_array(&text, "github", "repos", &u.slug, NEW_GITHUB_TABLE);
        wrote_repo = true;
    }
    // After the repos entry, always: a `[[github.repo]]` naming a repo that is
    // not polled does not validate, and `apply` would then refuse the lot.
    if let Some(labels) = labels {
        text = insert_repo_table(&text, u, labels);
    }

    apply(path, &before, &text)?;
    Ok(Adopted {
        wrote_route,
        wrote_repo,
        labels: labels.map(<[String]>::to_vec),
    })
}

/// Stop offering a repo. Plenty of repos you open a space in are ones you are
/// only reading — another org's, a dependency, something you cloned to look at.
pub fn ignore(path: &Path, slug: &str) -> Result<()> {
    let before = read(path)?;
    let text = add_to_array(&before, "adopt", "ignore", slug, NEW_ADOPT_TABLE);
    apply(path, &before, &text)
}

pub(crate) fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| {
        format!(
            "reading {} — run `comet-board init` if it does not exist yet",
            path.display()
        )
    })
}

/// Commit an edit to a file the operator hand-writes.
///
/// Three things happen before the new text lands, in this order: it has to
/// parse, it has to *validate* — so a catch-all can never end up shadowing what
/// we just wrote — and the previous contents are kept beside it. A one-command
/// writer that could corrupt somebody's routing config would not be worth
/// having.
pub(crate) fn apply(path: &Path, before: &str, after: &str) -> Result<()> {
    if before == after {
        return Ok(());
    }
    let cfg: RoutingConfig = toml::from_str(after)
        .context("the edit would not have parsed; routing.toml is untouched")?;
    cfg.check()
        .context("the edit would not have validated; routing.toml is untouched")?;

    // Nothing to preserve when there was no file: a `.bak` of nothing reads as
    // "there is a previous version to go back to", and there is not.
    if !before.is_empty() {
        let backup = backup_path(path);
        std::fs::write(&backup, before)
            .with_context(|| format!("writing the backup {}", backup.display()))?;
    }
    std::fs::write(path, after).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn backup_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(".bak");
    PathBuf::from(p)
}

const NEW_GITHUB_TABLE: &str = "\n# Repos polled for issues and pull requests. Private ones need a credential in\n# .env: GITHUB_TOKEN, or a GitHub App (GITHUB_APP_ID +\n# GITHUB_APP_PRIVATE_KEY_PATH). `comet-board doctor` says which is live.\n[github]\n";

const NEW_ADOPT_TABLE: &str = "\n# Repos with a comet space that the board should stop offering to adopt —\n# ones you are only reading rather than working in. Written by\n# `comet-board adopt --ignore`; delete a line to be offered it again.\n[adopt]\n";

/// The route block one adoption writes.
fn route_block(u: &Unadopted, runtime: &str) -> String {
    let repo = shorten_home(Path::new(&u.repo_root));
    format!(
        "# Adopted from the board's unadopted list.\n\
         [[route]]\n\
         match = {{ gh_repo = \"{slug}\" }}\n\
         workspace = \"{ws}\"\n\
         repo = \"{repo}\"\n\
         runtime = \"{runtime}\"\n\
         \n",
        ws = u.label,
        slug = u.slug,
    )
}

/// The `[[github.repo]]` block that narrows what one repo contributes.
fn repo_table_block(slug: &str, labels: &[String]) -> String {
    let list = labels
        .iter()
        .map(|l| toml_string(l))
        .collect::<Vec<_>>()
        .join(", ");
    let opening = if labels.is_empty() {
        "# Adopted from the board's unadopted list. An empty list is every\n\
         # open issue, said out loud — it overrides `[github] labels` rather\n\
         # than falling back to it."
    } else {
        "# Adopted from the board's unadopted list. Only these labels are\n\
         # polled, so the board carries what is current rather than the whole\n\
         # backlog. Add labels to widen it, `labels = []` for every open issue,\n\
         # or delete the table to fall back to `[github] labels`."
    };
    format!(
        "{opening}\n\
         [[github.repo]]\n\
         name = {name}\n\
         labels = [{list}]\n\
         \n",
        name = toml_string(slug),
    )
}

/// A TOML basic string. Labels are somebody else's data — `area:design` is
/// fine, and a label with a quote in it would otherwise write a file that does
/// not parse, which `apply` would then refuse in full.
pub(crate) fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Insert the `[[github.repo]]` table for a repo, directly below the `[github]`
/// table it belongs to.
///
/// A no-op when the repo already has one: two tables for one repo do not
/// validate, and `apply` would then refuse the whole adoption rather than the
/// one part of it that was redundant.
fn insert_repo_table(text: &str, u: &Unadopted, labels: &[String]) -> String {
    if toml::from_str::<RoutingConfig>(text)
        .ok()
        .is_some_and(|c| c.github.settings_for(&u.slug).is_some())
    {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let headers = header_lines(text);
    // Below `[github]` rather than at the end of the file: it configures what
    // `repos` lists, and a reader looking at one should see the other. Table
    // order carries no meaning in TOML — only `[[route]]` order does — so
    // appending is the safe fallback when there is no `[github]` table yet.
    let at = headers
        .iter()
        .position(|(_, name)| name == "[github]")
        .and_then(|ix| headers.get(ix + 1))
        .map(|(i, _)| start_of_block(&lines, *i))
        .unwrap_or(lines.len());

    let mut out: Vec<String> = lines[..at].iter().map(|s| s.to_string()).collect();
    if out.last().is_some_and(|l| !l.trim().is_empty()) {
        out.push(String::new());
    }
    out.extend(
        repo_table_block(&u.slug, labels)
            .lines()
            .map(str::to_string),
    );
    out.extend(lines[at..].iter().map(|s| s.to_string()));
    join(&out, text)
}

/// The runtime to write.
///
/// Whatever the existing routes mostly use, because that is the operator's
/// established habit and a route that disagrees with every other one is a
/// surprise. `claude-code` only when there is nothing to copy.
fn habitual_runtime(cfg: &RoutingConfig) -> String {
    let mut counts: std::collections::BTreeMap<&str, usize> = Default::default();
    for r in &cfg.routes {
        *counts.entry(r.runtime.as_str()).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(r, _)| r.to_string())
        .unwrap_or_else(|| "claude-code".into())
}

/// Insert a route **ahead of whatever is currently last**.
///
/// This is the load-bearing part. First matching route wins, so a naive append
/// lands after the catch-all, is shadowed by it, and never fires — silently,
/// which is the failure mode this whole feature exists to remove.
///
/// "Ahead of the last route" rather than "ahead of the empty `match`" on
/// purpose. Validation already guarantees an empty-`match` catch-all *is* last,
/// so for that case the two rules are the same — but the catch-alls people
/// actually write are usually a team-wide `match = { linear_team = "AGE" }`,
/// which validation cannot recognise as one and which the README tells you to
/// keep last. Going in ahead of the last route respects both, and costs nothing
/// when the routes are disjoint: order only matters where matches overlap, and
/// a route naming one repo is the more specific of any pair it overlaps with.
fn insert_route(text: &str, cfg: &RoutingConfig, u: &Unadopted) -> Result<String> {
    let block = route_block(u, &habitual_runtime(cfg));
    let lines: Vec<&str> = text.lines().collect();
    let headers = header_lines(text);

    // Correlate parsed routes with their headers by document order — the same
    // order `toml` reports them in.
    let route_headers: Vec<usize> = headers
        .iter()
        .filter(|(_, name)| name == "[[route]]")
        .map(|(i, _)| *i)
        .collect();
    if route_headers.len() != cfg.routes.len() {
        // Nothing in the file matches what was parsed from it; refuse rather
        // than guess at an insertion point.
        bail!(
            "routing.toml has {} `[[route]]` header(s) but parsed {} route(s) — \
             not editing a file this writer does not understand",
            route_headers.len(),
            cfg.routes.len()
        );
    }

    // With no routes at all there is nothing to stay behind: append. Nothing
    // outside the `[[route]]` sequence cares where one sits.
    let at = route_headers
        .last()
        .map(|i| start_of_block(&lines, *i))
        .unwrap_or(lines.len());

    let mut out: Vec<String> = lines[..at].iter().map(|s| s.to_string()).collect();
    // Never run the block straight onto the line above it; a route that reads
    // as a continuation of the one before is exactly the kind of thing nobody
    // notices until it matters.
    if out.last().is_some_and(|l| !l.trim().is_empty()) {
        out.push(String::new());
    }
    out.extend(block.lines().map(str::to_string));
    out.extend(lines[at..].iter().map(|s| s.to_string()));
    Ok(join(&out, text))
}

/// Walk back from a table header over the comment block attached to it, so an
/// insertion does not land between a comment and the route it describes.
pub(crate) fn start_of_block(lines: &[&str], header: usize) -> usize {
    let mut i = header;
    while i > 0 {
        let prev = lines[i - 1].trim_start();
        if prev.starts_with('#') {
            i -= 1;
        } else {
            break;
        }
    }
    i
}

fn header_lines_after(lines: &[&str], after: usize) -> Option<usize> {
    let text = lines.join("\n");
    header_lines(&text)
        .into_iter()
        .map(|(i, _)| i)
        .find(|i| *i > after)
}

/// Add one string to a `key = [...]` array in a top-level table, creating
/// either as needed. A value already present is left alone.
///
/// Deliberately a text edit rather than a re-serialization: `routing.toml` is
/// hand-written and full of comments explaining choices, and rewriting it from
/// the parsed structure would silently throw all of that away.
fn add_to_array(text: &str, table: &str, key: &str, value: &str, preamble: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let headers = header_lines(text);
    let header = format!("[{table}]");

    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    let entry = format!("\"{value}\"");

    let Some(&(table_at, _)) = headers.iter().find(|(_, name)| *name == header) else {
        // No such table. Appending is safe: table order carries no meaning in
        // TOML — only the order of `[[route]]` entries among themselves does.
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.extend(preamble.lines().map(str::to_string));
        out.push(format!("{key} = [{entry}]"));
        return join(&out, text);
    };

    let end = header_lines_after(&lines, table_at).unwrap_or(lines.len());
    let key_at = (table_at + 1..end).find(|&i| {
        let t = lines[i].trim_start();
        t.strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    });

    let Some(key_at) = key_at else {
        out.insert(table_at + 1, format!("{key} = [{entry}]"));
        return join(&out, text);
    };

    // The array may span lines; find the `]` that closes it.
    let Some(close) = (key_at..end).find(|&i| lines[i].contains(']')) else {
        return text.to_string();
    };

    // Everything between the brackets, so the separator can be decided from
    // what is actually there rather than from the shape of one line.
    let joined = lines[key_at..=close].join("\n");
    let inner = joined
        .split_once('[')
        .map(|(_, r)| r)
        .and_then(|r| r.rsplit_once(']'))
        .map(|(l, _)| l.trim())
        .unwrap_or("");
    if inner.split(',').any(|v| v.trim() == entry) {
        // Already listed. Adopting twice must not corrupt the array.
        return text.to_string();
    }
    // A trailing comma is a separator that is already there; a second one is a
    // parse error.
    let sep = if inner.is_empty() || inner.ends_with(',') {
        ""
    } else {
        ", "
    };

    let line = out[close].clone();
    let cut = line.rfind(']').expect("find() matched a `]`");
    let (head, tail) = line.split_at(cut);
    if head.trim().is_empty() && close > key_at {
        // A `]` on a line of its own: keep the array multi-line rather than
        // collapsing the new value onto the closing bracket.
        if !sep.is_empty()
            && let Some(prev) = (key_at..close).rev().find(|&i| !out[i].trim().is_empty())
        {
            // The last entry has no trailing comma; it needs one now.
            out[prev] = format!("{},", out[prev].trim_end());
        }
        let indent = " ".repeat(head.chars().count() + 2);
        out.insert(close, format!("{indent}{entry},"));
    } else {
        out[close] = format!("{}{sep}{entry}{tail}", head.trim_end());
    }
    join(&out, text)
}

/// Reassemble lines, preserving whether the original ended in a newline.
pub(crate) fn join(lines: &[String], original: &str) -> String {
    let mut s = lines.join("\n");
    if original.ends_with('\n') || original.is_empty() {
        s.push('\n');
    }
    s
}

/// Line indices of top-level table headers, with the header text.
///
/// The multi-line-string tracking is not decoration: `prompt = """..."""` is in
/// the shipped example config, and a prompt that happens to contain a line
/// starting with `[` would otherwise be read as a table header and every
/// insertion point computed from it would be wrong.
pub(crate) fn header_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut open: Option<&'static str> = None;
    for (i, line) in text.lines().enumerate() {
        if open.is_none() {
            let t = line.trim();
            if t.starts_with('[') {
                let end = t.find("] ").map(|p| p + 1).unwrap_or(t.len());
                let name = t[..end].trim_end().to_string();
                if name.ends_with(']') {
                    out.push((i, name));
                }
            }
        }
        scan_multiline(line, &mut open);
    }
    out
}

/// Track `"""` / `'''` across a line.
pub(crate) fn scan_multiline(line: &str, open: &mut Option<&'static str>) {
    let mut rest = line;
    loop {
        match *open {
            Some(delim) => match rest.find(delim) {
                Some(i) => {
                    rest = &rest[i + delim.len()..];
                    *open = None;
                }
                None => return,
            },
            None => {
                let candidates = [("\"\"\"", rest.find("\"\"\"")), ("'''", rest.find("'''"))];
                let Some((delim, at)) = candidates
                    .into_iter()
                    .filter_map(|(d, p)| p.map(|p| (d, p)))
                    .min_by_key(|(_, p)| *p)
                else {
                    return;
                };
                // An opener inside a comment opens nothing.
                if rest.find('#').is_some_and(|h| h < at) {
                    return;
                }
                rest = &rest[at + delim.len()..];
                *open = Some(delim);
            }
        }
    }
}

/// One line for `doctor`.
///
/// Never a failure: a repo you are only reading is not a broken environment,
/// and `doctor` exiting non-zero over one would make the whole report ignorable.
pub fn doctor_detail(cfg: &RoutingConfig, spaces: Option<&[Space]>) -> String {
    let Some(spaces) = spaces else {
        return "not checked — the engine is not reachable, so spaces could not be listed".into();
    };
    let found = detect(spaces, cfg, probe);
    if found.is_empty() {
        if cfg.adopt.ignore.is_empty() {
            "every space with a GitHub remote is on the board".into()
        } else {
            format!(
                "every space with a GitHub remote is on the board ({} ignored)",
                cfg.adopt.ignore.len()
            )
        }
    } else {
        format!(
            "{} not on the board: {} — run `comet-board adopt`",
            found.len(),
            found
                .iter()
                .map(|u| u.slug.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouteContext;

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

    fn cfg(text: &str) -> RoutingConfig {
        toml::from_str(text).unwrap()
    }

    /// A fixture resolver: path → (root, remote, linked).
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

    const CATCH_ALL: &str = r#"
[sync]
labels = ["herd"]

[[route]]
match = { gh_repo = "Florin-AS/Tally" }
workspace = "tally"
repo = "~/code/tally"
runtime = "claude-code"

# Anything else lands in the scratch space.
[[route]]
workspace = "scratch"
repo = "~/code/scratch"
runtime = "claude-code"

[github]
repos = ["Florin-AS/Tally"]
"#;

    // ---- detection -----------------------------------------------------

    #[test]
    fn a_space_with_no_config_at_all_is_offered() {
        let cfg = cfg("[github]\nrepos = []\n");
        let found = detect(
            &[space("tripletex-mcp", "/code/tm", true)],
            &cfg,
            probes(&[(
                "/code/tm",
                "/code/tm",
                "git@github.com:Florin-AS/tripletex-mcp.git",
                false,
            )]),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].slug, "Florin-AS/tripletex-mcp");
        assert_eq!(found[0].missing, Missing::Both);
        assert_eq!(found[0].name(), "tripletex-mcp");
        assert_eq!(found[0].label, "tripletex-mcp");
    }

    #[test]
    fn the_half_fixes_are_told_apart() {
        // The two keys are independent, which is what makes it easy to get one
        // of them and think you are done.
        let polled_only = cfg("[github]\nrepos = [\"o/r\"]\n");
        let found = detect(
            &[space("r", "/code/r", true)],
            &polled_only,
            probes(&[("/code/r", "/code/r", "https://github.com/o/r", false)]),
        );
        assert_eq!(
            found[0].missing,
            Missing::Route,
            "rows appear, dispatch refuses"
        );

        let routed_only = cfg(
            "[[route]]\nmatch = { gh_repo = \"o/r\" }\nworkspace = \"r\"\nrepo = \"/code/r\"\nruntime = \"claude\"\n",
        );
        let found = detect(
            &[space("r", "/code/r", true)],
            &routed_only,
            probes(&[("/code/r", "/code/r", "https://github.com/o/r", false)]),
        );
        assert_eq!(found[0].missing, Missing::Polling, "nothing ever polls it");
    }

    #[test]
    fn a_catch_all_does_not_count_as_having_adopted_a_repo() {
        // Dispatch would work — and would start an agent in the catch-all's
        // checkout rather than this repo's. Dispatching to the wrong space is
        // worse than the row not being there at all.
        let found = detect(
            &[space("thing", "/code/thing", true)],
            &cfg(CATCH_ALL),
            probes(&[(
                "/code/thing",
                "/code/thing",
                "git@github.com:o/thing.git",
                false,
            )]),
        );
        assert_eq!(found[0].missing, Missing::Both);

        // ...and adopting it gives the repo a route of its own, ahead of the
        // catch-all, so dispatch lands in the right checkout.
        let out = adopt_text(CATCH_ALL, &found[0]);
        let parsed = cfg(&out);
        let route = parsed
            .resolve(&RouteContext {
                gh_repo: Some("o/thing".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(route.workspace, "thing");
    }

    #[test]
    fn an_adopted_repo_is_not_offered_again() {
        let found = detect(
            &[space("tally", "/code/tally", true)],
            &cfg(CATCH_ALL),
            probes(&[(
                "/code/tally",
                "/code/tally",
                "git@github.com:Florin-AS/Tally.git",
                false,
            )]),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn ignoring_is_what_stops_it_being_offered() {
        // Plenty of repos you open a space in are another org's.
        let cfg = cfg("[adopt]\nignore = [\"someorg/vendor\"]\n");
        let found = detect(
            &[space("vendor", "/code/v", true)],
            &cfg,
            probes(&[(
                "/code/v",
                "/code/v",
                "git@github.com:SomeOrg/vendor.git",
                false,
            )]),
        );
        assert!(found.is_empty(), "the ignore list is case-insensitive");
    }

    #[test]
    fn attempts_and_non_repos_are_never_offered() {
        let cfg = cfg("");
        let found = detect(
            &[
                // A linked worktree the board cut for an attempt.
                space("lin-142", "/wt/lin-142", true),
                // A space that is not a checkout at all — the owning device
                // said so (`git_detected = false`), so it is not even probed.
                space("notes", "/notes", false),
                // A checkout whose remote is not GitHub: nothing to write.
                space("internal", "/code/int", true),
            ],
            &cfg,
            probes(&[
                (
                    "/wt/lin-142",
                    "/wt/lin-142",
                    "git@github.com:o/tm.git",
                    true,
                ),
                (
                    "/code/int",
                    "/code/int",
                    "git@git.example.com:o/int.git",
                    false,
                ),
            ]),
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn one_repo_is_offered_once_however_many_spaces_hold_it() {
        let found = detect(
            &[
                space("thing", "/code/thing", true),
                space("thing-clone", "/code/thing2", true),
            ],
            &cfg(""),
            probes(&[
                (
                    "/code/thing",
                    "/code/thing",
                    "git@github.com:o/thing.git",
                    false,
                ),
                (
                    "/code/thing2",
                    "/code/thing2",
                    "https://github.com/o/thing.git",
                    false,
                ),
            ]),
        );
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn a_space_deep_inside_a_checkout_offers_the_checkouts_root() {
        // A space's folder need not be the repo root; the route's `repo =`
        // must be, or worktree creation would run somewhere git refuses.
        let found = detect(
            &[space("docs", "/code/thing/docs", true)],
            &cfg(""),
            probes(&[(
                "/code/thing/docs",
                "/code/thing",
                "git@github.com:o/thing.git",
                false,
            )]),
        );
        assert_eq!(found[0].repo_root, "/code/thing");
    }

    #[test]
    fn github_slugs_parse_from_both_remote_forms() {
        assert_eq!(
            github_slug("git@github.com:offhand/tally.git").as_deref(),
            Some("offhand/tally")
        );
        assert_eq!(
            github_slug("https://github.com/offhand/tally").as_deref(),
            Some("offhand/tally")
        );
        assert_eq!(
            github_slug("ssh://git@github.com/offhand/tally.git").as_deref(),
            Some("offhand/tally")
        );
        // Userinfo in an HTTPS remote — what an App-authenticated clone starts
        // life with (gh#97). Failing to read it would make an onboarded checkout
        // invisible to detection, which is the exact silent gap adoption exists
        // to close. The credential half names no repo, so it is dropped.
        assert_eq!(
            github_slug("https://x-access-token@github.com/offhand/tally.git").as_deref(),
            Some("offhand/tally")
        );
        // Not GitHub, so not a GitHub source — userinfo included.
        assert_eq!(github_slug("git@gitlab.com:offhand/tally.git"), None);
        assert_eq!(
            github_slug("https://user@gitlab.com/offhand/tally.git"),
            None
        );
        // A `@` after the first slash is in the path, not userinfo.
        assert_eq!(github_slug("https://example.test/x@github.com/o/r"), None);
        assert_eq!(github_slug(""), None);
    }

    // ---- the writer ----------------------------------------------------

    fn unadopted() -> Unadopted {
        Unadopted {
            label: "tripletex-mcp".into(),
            slug: "Florin-AS/tripletex-mcp".into(),
            repo_root: "/code/tripletex-mcp".into(),
            missing: Missing::Both,
        }
    }

    fn adopt_text(text: &str, u: &Unadopted) -> String {
        adopt_text_with(text, u, None)
    }

    fn adopt_text_with(text: &str, u: &Unadopted, labels: Option<&[String]>) -> String {
        let parsed = cfg(text);
        let mut out = text.to_string();
        if u.missing != Missing::Polling {
            out = insert_route(&out, &parsed, u).unwrap();
        }
        if u.missing != Missing::Route {
            out = add_to_array(&out, "github", "repos", &u.slug, NEW_GITHUB_TABLE);
        }
        if let Some(labels) = labels {
            out = insert_repo_table(&out, u, labels);
        }
        // Everything the writer emits has to survive the gate `apply` puts it
        // through, or the command does nothing at all.
        let reparsed: RoutingConfig = toml::from_str(&out).expect("must parse");
        reparsed.check().expect("must validate");
        out
    }

    #[test]
    fn a_new_route_lands_ahead_of_the_catch_all() {
        // A naive append is shadowed by the catch-all and never matches —
        // silently. This is the whole reason the writer is not one `push`.
        let out = adopt_text(CATCH_ALL, &unadopted());
        let parsed = cfg(&out);
        let ours = parsed
            .routes
            .iter()
            .position(|r| r.match_.gh_repo.as_deref() == Some("Florin-AS/tripletex-mcp"))
            .expect("the route was written");
        let catch_all = parsed
            .routes
            .iter()
            .position(|r| r.match_.is_empty())
            .expect("the catch-all is still there");
        assert!(ours < catch_all, "shadowed by the catch-all:\n{out}");
        assert_eq!(catch_all, parsed.routes.len() - 1, "and it is still last");

        // ...and it actually resolves now.
        let route = parsed
            .resolve(&RouteContext {
                gh_repo: Some("Florin-AS/tripletex-mcp".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(route.workspace, "tripletex-mcp");
    }

    #[test]
    fn a_team_wide_catch_all_also_keeps_its_place() {
        // The catch-alls people actually write are usually a team route, which
        // `validate` cannot recognise as one — it has a `match`. The README
        // tells you to keep it last, so adoption does.
        let text = r#"[[route]]
match = { label = "tally" }
workspace = "tally"
repo = "~/dev/tally"
runtime = "claude-code"

# last: anything in the team with no repo label
[[route]]
match = { linear_team = "AGE" }
workspace = "herdr-board"
repo = "~/dev/herdr-board"
runtime = "claude-code"
"#;
        let out = adopt_text(text, &unadopted());
        let parsed = cfg(&out);
        assert_eq!(parsed.routes.len(), 3);
        assert_eq!(
            parsed.routes[2].match_.linear_team.as_deref(),
            Some("AGE"),
            "the team route stopped being last:\n{out}"
        );
        assert_eq!(
            parsed.routes[1].match_.gh_repo.as_deref(),
            Some("Florin-AS/tripletex-mcp")
        );
    }

    #[test]
    fn the_catch_alls_own_comment_stays_with_it() {
        let out = adopt_text(CATCH_ALL, &unadopted());
        let lines: Vec<&str> = out.lines().collect();
        let comment = lines
            .iter()
            .position(|l| l.contains("Anything else lands in the scratch"))
            .unwrap();
        assert!(
            lines[comment + 1].trim() == "[[route]]" && lines[comment + 2].contains("scratch"),
            "the insertion split a comment from its route:\n{out}"
        );
    }

    #[test]
    fn adopting_writes_both_halves() {
        let out = adopt_text(CATCH_ALL, &unadopted());
        let parsed = cfg(&out);
        assert!(
            parsed
                .github
                .repos
                .iter()
                .any(|r| r == "Florin-AS/tripletex-mcp"),
            "the repo was not added to [github] repos:\n{out}"
        );
        assert_eq!(parsed.github.repos.len(), 2, "the existing one survived");
    }

    #[test]
    fn only_the_missing_half_is_written() {
        let mut u = unadopted();
        u.missing = Missing::Polling;
        let out = adopt_text(CATCH_ALL, &u);
        assert_eq!(cfg(&out).routes.len(), 2, "no route was needed:\n{out}");
        assert!(
            cfg(&out)
                .github
                .repos
                .iter()
                .any(|r| r.contains("tripletex"))
        );

        let mut u = unadopted();
        u.missing = Missing::Route;
        let out = adopt_text(CATCH_ALL, &u);
        assert_eq!(cfg(&out).routes.len(), 3);
        assert_eq!(cfg(&out).github.repos.len(), 1, "repos was left alone");
    }

    #[test]
    fn a_file_with_no_routes_at_all_still_gets_one() {
        let out = adopt_text("[sync]\ninterval = \"30s\"\n", &unadopted());
        let parsed = cfg(&out);
        assert_eq!(parsed.routes.len(), 1);
        assert_eq!(parsed.sync.interval, "30s", "the rest survived:\n{out}");
    }

    #[test]
    fn a_route_inside_a_prompt_string_is_not_mistaken_for_a_header() {
        // The shipped example config has a `prompt = """..."""`, and a prompt
        // that mentions `[[route]]` would otherwise move every insertion point.
        let text = "[[route]]\nmatch = { gh_repo = \"o/a\" }\nworkspace = \"a\"\nrepo = \"/a\"\nruntime = \"claude\"\nprompt = \"\"\"\n[github] is not a table here\n[[route]] neither\n\"\"\"\n\n[[route]]\nworkspace = \"catch\"\nrepo = \"/c\"\nruntime = \"claude\"\n";
        assert_eq!(
            header_lines(text)
                .iter()
                .filter(|(_, n)| n == "[[route]]")
                .count(),
            2,
            "the prompt body was read as config"
        );
        let out = adopt_text(text, &unadopted());
        let parsed = cfg(&out);
        assert_eq!(parsed.routes.len(), 3);
        assert!(parsed.routes[2].match_.is_empty(), "the catch-all is last");
        assert!(
            parsed.routes[0]
                .prompt
                .as_deref()
                .unwrap()
                .contains("[[route]] neither"),
            "the prompt was edited"
        );
    }

    #[test]
    fn arrays_grow_however_they_are_written() {
        for (input, want) in [
            ("[github]\nrepos = []\n", vec!["o/new"]),
            ("[github]\nrepos = [\"a/b\"]\n", vec!["a/b", "o/new"]),
            ("[github]\nrepos = [\n  \"a/b\",\n]\n", vec!["a/b", "o/new"]),
            // No `repos` key at all.
            ("[github]\nlabels = []\n", vec!["o/new"]),
            // No `[github]` table at all.
            ("[sync]\ninterval = \"30s\"\n", vec!["o/new"]),
        ] {
            let out = add_to_array(input, "github", "repos", "o/new", NEW_GITHUB_TABLE);
            assert_eq!(cfg(&out).github.repos, want, "from:\n{input}\ngot:\n{out}");
        }
    }

    #[test]
    fn ignoring_writes_a_list_that_detection_then_honours() {
        let text = add_to_array("[sync]\n", "adopt", "ignore", "o/vendor", NEW_ADOPT_TABLE);
        let parsed = cfg(&text);
        assert!(parsed.adopt.ignores("O/Vendor"));
        // And it round-trips: ignoring a second repo keeps the first.
        let text = add_to_array(&text, "adopt", "ignore", "o/other", NEW_ADOPT_TABLE);
        assert_eq!(cfg(&text).adopt.ignore, vec!["o/vendor", "o/other"]);
    }

    #[test]
    fn the_runtime_copies_whatever_the_other_routes_use() {
        // A route that disagrees with every other one is a surprise.
        let c = cfg(
            "[[route]]\nmatch = { label = \"a\" }\nworkspace = \"a\"\nrepo = \"/a\"\nruntime = \"codex\"\n\n[[route]]\nmatch = { label = \"b\" }\nworkspace = \"b\"\nrepo = \"/b\"\nruntime = \"codex\"\n",
        );
        assert_eq!(habitual_runtime(&c), "codex");
        assert_eq!(habitual_runtime(&cfg("")), "claude-code");
    }

    // ---- adopting a backlog (herdr-board AGE-28) -------------------------

    fn preview_of(issues: serde_json::Value) -> RepoPreview {
        use crate::sources::github::{FixtureRest, Github};
        let gh = Github::new(FixtureRest::new(vec![("/repos".into(), issues)]));
        let p = preview(&gh, "b/itsm-agent").unwrap();
        // Open issues only: `state=all` would count years of closed ones and
        // say nothing about what is about to arrive.
        assert!(
            gh.rest.asked.borrow()[0].contains("state=open"),
            "{:?}",
            gh.rest.asked.borrow()
        );
        p
    }

    fn issue(number: i64, labels: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "number": number, "node_id": format!("n{number}"), "title": "t",
            "html_url": "u", "state": "open", "updated_at": "t",
            "labels": labels.iter().map(|l| serde_json::json!({ "name": l })).collect::<Vec<_>>()
        })
    }

    #[test]
    fn the_preview_counts_what_adopting_unfiltered_would_pull() {
        let p = preview_of(serde_json::json!([
            issue(1, &["release-a"]),
            issue(2, &["release-a", "area:design"]),
            issue(3, &["release-b"]),
            issue(4, &[]),
        ]));
        assert_eq!(p.open_issues, 4);
        assert!(!p.truncated);
        assert_eq!(p.count_phrase(), "4 open issues");
        // Commonest first: the label that would let most of the backlog through
        // is the one worth looking at.
        assert_eq!(
            p.labels,
            vec![
                ("release-a".to_string(), 2),
                ("area:design".to_string(), 1),
                ("release-b".to_string(), 1),
            ]
        );
    }

    #[test]
    fn a_pull_request_is_not_an_open_issue() {
        // GitHub's issues endpoint returns both, and counting PRs would
        // over-state what adopting is about to pull.
        let p = preview_of(serde_json::json!([
            issue(1, &["release-a"]),
            serde_json::json!({ "number": 2, "node_id": "n2", "title": "a PR",
                                "html_url": "u", "state": "open", "updated_at": "t",
                                "pull_request": { "url": "x" }, "labels": [] }),
        ]));
        assert_eq!(p.open_issues, 1);
    }

    #[test]
    fn a_full_page_is_reported_as_a_floor_rather_than_a_total() {
        // Nothing paginates. Saying `100 open issues` when there may be 400
        // would be a number the board made up.
        let many: Vec<serde_json::Value> = (1..=crate::sources::github::PAGE as i64)
            .map(|n| issue(n, &["release-a"]))
            .collect();
        let p = preview_of(serde_json::Value::Array(many));
        assert!(p.truncated);
        assert_eq!(p.count_phrase(), "100+ open issues");
    }

    #[test]
    fn a_chosen_filter_is_counted_as_github_would_apply_it() {
        // `labels=a,b` is an AND. Summing them would promise more rows than
        // arrive, which is the same over-claiming the preview exists to stop.
        let p = preview_of(serde_json::json!([
            issue(1, &["release-a"]),
            issue(2, &["release-a", "area:design"]),
            issue(3, &["release-b"]),
        ]));
        assert_eq!(p.count_for(&["release-a".into()]), 2);
        assert_eq!(
            p.count_for(&["release-a".into(), "area:design".into()]),
            1,
            "an AND, not a sum"
        );
        // No filter at all is the whole open set.
        assert_eq!(p.count_for(&[]), 3);
    }

    #[test]
    fn adopting_with_labels_writes_a_table_the_poller_then_honours() {
        // The bug: adopting a repo whose backlog is 83 open issues put all 83
        // on the board, because `labels = []` means "everything".
        let out = adopt_text_with(CATCH_ALL, &unadopted(), Some(&["release-a".into()]));
        let parsed = cfg(&out);
        assert_eq!(
            parsed.github.labels_for("Florin-AS/tripletex-mcp"),
            ["release-a"]
        );
        // And the repo it configures is polled at all — a table naming an
        // unlisted repo does not validate.
        assert!(
            parsed
                .github
                .repos
                .iter()
                .any(|r| r == "Florin-AS/tripletex-mcp")
        );
        // The other repo keeps the global answer.
        assert!(parsed.github.labels_for("Florin-AS/Tally").is_empty());
    }

    #[test]
    fn choosing_everything_over_a_global_filter_is_writable() {
        // The override has to work in both directions: a repo whose tracker is
        // curated wants everything even when the global list is narrow.
        let text = "[github]\nrepos = [\"a/b\"]\nlabels = [\"herd\"]\n";
        let out = adopt_text_with(text, &unadopted(), Some(&[]));
        let parsed = cfg(&out);
        assert!(
            parsed
                .github
                .labels_for("Florin-AS/tripletex-mcp")
                .is_empty()
        );
        assert_eq!(
            parsed.github.labels_for("a/b"),
            ["herd"],
            "and only that repo"
        );
    }

    #[test]
    fn adopting_without_a_filter_writes_no_table_at_all() {
        // Unchanged behaviour: a repo whose tracker is already curated does not
        // need a table saying so.
        let out = adopt_text_with(CATCH_ALL, &unadopted(), None);
        assert!(!out.contains("[[github.repo]]"), "{out}");
        assert!(cfg(&out).github.per_repo.is_empty());
    }

    #[test]
    fn the_table_lands_under_the_github_table_it_configures() {
        let out = adopt_text_with(CATCH_ALL, &unadopted(), Some(&["release-a".into()]));
        let lines: Vec<&str> = out.lines().collect();
        let github = lines.iter().position(|l| l.trim() == "[github]").unwrap();
        let table = lines
            .iter()
            .position(|l| l.trim() == "[[github.repo]]")
            .unwrap();
        assert!(
            table > github,
            "the table reads as configuring nothing:\n{out}"
        );
        // Between them, `repos` — which is the list it is narrowing.
        let repos = lines
            .iter()
            .position(|l| l.trim_start().starts_with("repos"))
            .unwrap();
        assert!(github < repos && repos < table, "{out}");
    }

    #[test]
    fn adopting_twice_does_not_write_a_second_table() {
        // Two tables for one repo do not validate, so a second write would take
        // the whole adoption down rather than just being redundant.
        let once = adopt_text_with(CATCH_ALL, &unadopted(), Some(&["release-a".into()]));
        let twice = insert_repo_table(&once, &unadopted(), &["release-b".into()]);
        assert_eq!(once, twice);
        cfg(&twice).check().unwrap();
    }

    #[test]
    fn a_second_repo_gets_a_table_of_its_own_beside_the_first() {
        // The insertion point is computed from `[github]`, and after the first
        // adoption the header directly below it is another `[[github.repo]]`.
        let first = adopt_text_with(CATCH_ALL, &unadopted(), Some(&["release-a".into()]));
        let second = Unadopted {
            label: "brreg".into(),
            slug: "Florin-AS/brreg".into(),
            repo_root: "/code/brreg".into(),
            missing: Missing::Both,
        };
        let out = adopt_text_with(&first, &second, Some(&["bug".into()]));
        let parsed = cfg(&out);
        assert_eq!(parsed.github.per_repo.len(), 2);
        assert_eq!(parsed.github.labels_for("Florin-AS/brreg"), ["bug"]);
        assert_eq!(
            parsed.github.labels_for("Florin-AS/tripletex-mcp"),
            ["release-a"],
            "the first repo's filter was disturbed:\n{out}"
        );
        assert_eq!(parsed.github.repos.len(), 3);
    }

    #[test]
    fn a_label_with_a_quote_in_it_does_not_break_the_file() {
        // Labels are somebody else's data, and a file that does not parse is an
        // adoption that silently does nothing.
        let out = adopt_text_with(CATCH_ALL, &unadopted(), Some(&["needs \"design\"".into()]));
        assert_eq!(
            cfg(&out).github.labels_for("Florin-AS/tripletex-mcp"),
            ["needs \"design\""]
        );
    }

    #[test]
    fn a_repo_that_is_already_polled_is_adopted_without_touching_its_filter() {
        // `Missing::Route` means the operator already chose what this repo
        // contributes; adopting the missing half must not re-decide it.
        let mut u = unadopted();
        u.missing = Missing::Route;
        let text = "[github]\nrepos = [\"Florin-AS/tripletex-mcp\"]\nlabels = [\"herd\"]\n";
        let out = adopt_text_with(text, &u, None);
        assert!(!out.contains("[[github.repo]]"), "{out}");
        assert_eq!(
            cfg(&out).github.labels_for("Florin-AS/tripletex-mcp"),
            ["herd"]
        );
    }

    #[test]
    fn the_written_labels_come_back_for_the_caller_to_report() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routing.toml");
        std::fs::write(&path, CATCH_ALL).unwrap();

        let done = adopt_with(&path, &unadopted(), Some(&["release-a".into()])).unwrap();
        assert_eq!(done.labels.as_deref(), Some(&["release-a".to_string()][..]));
        assert!(done.wrote_route && done.wrote_repo);
        // And the file on disk is one the loader accepts.
        let reloaded = RoutingConfig::load(&path).unwrap();
        assert_eq!(
            reloaded.github.labels_for("Florin-AS/tripletex-mcp"),
            ["release-a"]
        );
    }

    #[test]
    fn a_broken_edit_never_reaches_the_file() {
        // The gate is the point: this is a one-command writer aimed at a file
        // somebody hand-maintains.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routing.toml");
        std::fs::write(&path, CATCH_ALL).unwrap();

        let err = apply(&path, CATCH_ALL, "this is not toml = = =").unwrap_err();
        assert!(format!("{err:#}").contains("untouched"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CATCH_ALL);

        // ...including an edit that parses but would shadow a route.
        let shadowed = format!(
            "[[route]]\nworkspace = \"c\"\nrepo = \"/c\"\nruntime = \"claude\"\n{CATCH_ALL}"
        );
        let err = apply(&path, CATCH_ALL, &shadowed).unwrap_err();
        assert!(format!("{err:#}").contains("untouched"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CATCH_ALL);

        // A good edit lands, and the previous contents are kept beside it.
        let good = adopt_text(CATCH_ALL, &unadopted());
        apply(&path, CATCH_ALL, &good).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), good);
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            CATCH_ALL
        );
    }

    #[test]
    fn the_commands_drive_a_real_file_from_silent_to_watched() {
        // What `comet-board adopt` and `adopt --ignore` actually do, through
        // the public entry points and a routing.toml on disk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routing.toml");
        std::fs::write(&path, CATCH_ALL).unwrap();

        let spaces = [
            space("tripletex-mcp", "/code/tm", true),
            space("vendor", "/code/v", true),
        ];
        let probe = probes(&[
            (
                "/code/tm",
                "/code/tm",
                "git@github.com:Florin-AS/tripletex-mcp.git",
                false,
            ),
            (
                "/code/v",
                "/code/v",
                "git@github.com:SomeOrg/vendor.git",
                false,
            ),
        ]);

        let reload = |path: &Path| RoutingConfig::load(path).unwrap();
        let found = detect(&spaces, &reload(&path), &probe);
        assert_eq!(found.len(), 2);

        // Adopt the one you work in.
        let tm = found.iter().find(|u| u.name() == "tripletex-mcp").unwrap();
        let done = adopt(&path, tm).unwrap();
        assert!(done.wrote_route && done.wrote_repo);

        // Ignore the one you are only reading.
        let vendor = found.iter().find(|u| u.name() == "vendor").unwrap();
        ignore(&path, &vendor.slug).unwrap();

        let cfg = reload(&path);
        assert!(detect(&spaces, &cfg, &probe).is_empty(), "still offered");
        assert!(
            cfg.github
                .repos
                .iter()
                .any(|r| r == "Florin-AS/tripletex-mcp")
        );
        assert!(cfg.adopt.ignores("SomeOrg/vendor"));
        assert!(
            cfg.routes.last().unwrap().match_.is_empty(),
            "the catch-all stopped being last"
        );
        // Ignoring again is a no-op rather than a corrupted array.
        ignore(&path, &vendor.slug).unwrap();
        assert_eq!(reload(&path).adopt.ignore.len(), 1);
    }

    #[test]
    fn adopting_every_space_leaves_nothing_to_adopt() {
        // The round trip the operator actually performs: detect, adopt, and the
        // row is gone — including the catch-all still being last afterwards.
        let spaces = [
            space("tripletex-mcp", "/code/tm", true),
            space("brreg", "/code/brreg", true),
        ];
        let probe = probes(&[
            (
                "/code/tm",
                "/code/tm",
                "git@github.com:Florin-AS/tripletex-mcp.git",
                false,
            ),
            (
                "/code/brreg",
                "/code/brreg",
                "https://github.com/Florin-AS/brreg.git",
                false,
            ),
        ]);
        let mut text = CATCH_ALL.to_string();
        loop {
            let parsed = cfg(&text);
            let found = detect(&spaces, &parsed, &probe);
            let Some(u) = found.first() else { break };
            text = adopt_text(&text, u);
        }
        let parsed = cfg(&text);
        assert_eq!(parsed.github.repos.len(), 3);
        assert!(parsed.routes.last().unwrap().match_.is_empty());
        assert!(detect(&spaces, &parsed, &probe).is_empty());
    }
}
