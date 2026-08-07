//! Reading, validating and editing `routing.toml` from somewhere that is not a
//! shell on the box (gh#75).
//!
//! `routing.toml` is a hand-edited file, documented as "not managed config",
//! and it lives on whichever device hosts the board — normally the always-on
//! one. That made adding a repo, pointing a route at a different agent account,
//! or lifting a cap an ssh-and-edit job, which is fine for the person who set
//! the box up and a dead end for everybody else on the org.
//!
//! This module is the file half of the surface [`crate::adopt`] already proved
//! out: **every write re-parses and re-validates the whole file before it lands,
//! and leaves the previous contents in `routing.toml.bak`**. It reuses
//! [`adopt::apply`] for exactly that, so there is one writer discipline and not
//! two.
//!
//! Two things it deliberately does not do:
//!
//! - **Re-serialize the config.** Every edit here is a text edit, for the reason
//!   [`adopt::add_to_array`] gives: the file is full of comments explaining
//!   choices, and rewriting it from the parsed structure throws all of them away
//!   silently. [`RoutingConfig`] gained `Serialize` so a *reader* (a settings
//!   page, `routes list --json`) can be handed the parse; nothing writes TOML
//!   from it.
//! - **Reach the `.env`.** Credentials are the other hand-edited file, and
//!   moving secrets over the wire is a different decision from moving routes.
//!   `doctor` still says which keys are missing.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::adopt;
use crate::config::{Paths, RoutingConfig};

/// `routing.toml` as it stands: the text, what it parses to, and what is wrong
/// with it.
///
/// One reply rather than three calls because they are one answer — a settings
/// page showing routes it parsed while the file on disk says something else
/// would be the whole bug this replaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingView {
    /// Absolute path on the board's device. Named so an operator can still ssh
    /// in — this surface is a convenience, not a lock-out.
    pub path: String,
    /// False when there is no file yet; `text` is then empty and `config` is
    /// the defaults. Not an error: `comet-board init` writes the first one, and
    /// a board with no routes renders every row `no route`.
    pub exists: bool,
    /// The file, verbatim. What an editing surface round-trips, and the only
    /// thing that carries the comments.
    pub text: String,
    /// The parse. Absent only when the file does not parse at all, which is the
    /// one state where there is nothing structured to show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<RoutingConfig>,
    /// Everything wrong with it: the parse error alone when it does not parse,
    /// else every validation failure ([`RoutingConfig::problems`]). Empty means
    /// the board is running on exactly this.
    pub problems: Vec<String>,
    /// A `routing.toml.bak` is sitting beside it — the previous contents, left
    /// by the last write through here.
    pub backup: bool,
}

impl RoutingView {
    pub fn valid(&self) -> bool {
        self.problems.is_empty()
    }
}

/// Read `routing.toml`, parse it, and say what is wrong with it.
///
/// Never fails on the file's *content*: a config that does not parse is a
/// [`RoutingView`] with a problem on it, because "show me what is broken" is
/// the request this is answering. An unreadable *directory* still errors.
pub fn read(paths: &Paths) -> Result<RoutingView> {
    let path = paths.routing();
    let (exists, text) = match std::fs::read_to_string(&path) {
        Ok(text) => (true, text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, String::new()),
        Err(e) => bail!("reading {}: {e}", path.display()),
    };
    Ok(view(&path, exists, text))
}

fn view(path: &Path, exists: bool, text: String) -> RoutingView {
    let (config, problems) = match toml::from_str::<RoutingConfig>(&text) {
        Ok(cfg) => {
            let problems = cfg.problems();
            (Some(cfg), problems)
        }
        Err(e) => (
            None,
            vec![format!("{} does not parse: {e}", path.display())],
        ),
    };
    RoutingView {
        path: path.display().to_string(),
        exists,
        text,
        config,
        problems,
        backup: adopt::backup_path(path).exists(),
    }
}

// ---- rendering one route -----------------------------------------------

/// One route's `match`, in the config's own words.
///
/// Here rather than in each frontend because it is a *reading* of the config,
/// not a layout: an empty match matches everything, and a surface that printed
/// nothing where every other row carries a condition would read as "no
/// condition set yet" — the opposite of what it means.
pub fn match_summary(m: &crate::config::RouteMatch) -> String {
    let parts: Vec<String> = [
        m.linear_team.as_ref().map(|v| format!("linear_team={v}")),
        m.linear_project
            .as_ref()
            .map(|v| format!("linear_project={v}")),
        m.gh_repo.as_ref().map(|v| format!("gh_repo={v}")),
        m.label.as_ref().map(|v| format!("label={v}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        "catch-all".into()
    } else {
        parts.join(" + ")
    }
}

/// The attempt cap in force on a route: its own, else `[defaults]`, named as
/// inherited. Which of the two it is decides where somebody goes to change it.
pub fn cap_summary(route: &crate::config::Route, default: &str) -> String {
    match &route.max_duration {
        Some(d) => d.clone(),
        None => format!("{default} (default)"),
    }
}

// ---- editing -----------------------------------------------------------

/// One change to `routing.toml`.
///
/// Targeted edits rather than "here is the new file" for everything, because a
/// caller that can only send whole files has to reconstruct the comments, and
/// the ones a settings page actually performs — an account, a cap, a runtime —
/// are one key each. [`Edit::Text`] is still here for the rest: a `prompt`, a
/// `match`, a route reordering. Nothing outside this list can be expressed, and
/// that is the point: an unknown key is refused by name instead of written and
/// then ignored forever.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum Edit {
    /// Replace the whole file.
    ///
    /// `base` is the text the editor started from. When it is given and no
    /// longer matches what is on disk, the write is refused rather than
    /// silently discarding whatever landed in between — this is a file people
    /// still edit by hand, over ssh, while a settings page is open on it.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
    },
    /// Set one key on the `route`-th `[[route]]` (0-based, document order).
    /// `value: null` removes the key, falling the route back to `[defaults]`.
    Route {
        route: usize,
        key: String,
        #[serde(default)]
        value: Option<String>,
    },
    /// Set one key under `[defaults]`. `value: null` removes it.
    Default {
        key: String,
        #[serde(default)]
        value: Option<String>,
    },
}

/// The keys [`Edit::Route`] may set, and the TOML each writes.
///
/// A list rather than "whatever you send" because a misspelt key in a TOML file
/// is not an error — it parses, it is ignored, and the route goes on behaving
/// the way it did while somebody believes they changed it.
const ROUTE_KEYS: &[(&str, Kind)] = &[
    ("name", Kind::Str),
    ("workspace", Kind::Str),
    ("repo", Kind::Str),
    ("runtime", Kind::Str),
    ("account", Kind::Str),
    ("branch_template", Kind::Str),
    ("base", Kind::Str),
    ("max_concurrent", Kind::Int),
    ("max_duration", Kind::Str),
    ("archive_chats", Kind::Str),
    ("billing_guard", Kind::Str),
];

/// The keys [`Edit::Default`] may set.
const DEFAULT_KEYS: &[(&str, Kind)] = &[
    ("max_concurrent_per_workspace", Kind::Int),
    ("branch_template", Kind::Str),
    ("base", Kind::Str),
    ("notify", Kind::Bool),
    ("notify_dispatcher", Kind::Bool),
    // The pin (gh#104). Here rather than only in the frontends' hands because
    // this is how it is *unset*: a settings surface can clear a key it wrote,
    // and a board reached over ssh can too.
    ("orchestrator_chat", Kind::Str),
    ("new_source", Kind::Str),
    ("max_duration", Kind::Str),
    // Both retentions (gh#72, gh#139): the shelf and the disk fill up on a box
    // nobody has a shell on, which is the box where this surface is the only
    // way to say how long they may.
    ("retain_worktrees", Kind::Str),
    ("archive_chats", Kind::Str),
    ("billing_guard", Kind::Str),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    Int,
    Bool,
}

impl Kind {
    /// Render a value the caller sent as a string into TOML of this kind.
    ///
    /// Everything arrives as a string because that is what a text field and a
    /// CLI argument both have; the schema is here, not on the wire.
    fn render(self, value: &str) -> Result<String> {
        let v = value.trim();
        match self {
            Kind::Str => Ok(adopt::toml_string(value)),
            Kind::Int => match v.parse::<u64>() {
                Ok(n) => Ok(n.to_string()),
                Err(_) => bail!("`{value}` is not a whole number"),
            },
            Kind::Bool => match v.to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" | "1" => Ok("true".into()),
                "false" | "no" | "off" | "0" => Ok("false".into()),
                _ => bail!("`{value}` is not true or false"),
            },
        }
    }
}

fn kind_of(keys: &[(&str, Kind)], key: &str, table: &str) -> Result<Kind> {
    match keys.iter().find(|(k, _)| *k == key) {
        Some((_, kind)) => Ok(*kind),
        None => bail!(
            "`{key}` is not a {table} key this can set. Try one of: {}. \
             Anything else (a prompt, a match, the order of the routes) is a \
             whole-file edit.",
            keys.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// The route keys a caller may set, for a picker or a `--help`.
pub fn route_keys() -> Vec<&'static str> {
    ROUTE_KEYS.iter().map(|(k, _)| *k).collect()
}

/// The `[defaults]` keys a caller may set.
pub fn default_keys() -> Vec<&'static str> {
    DEFAULT_KEYS.iter().map(|(k, _)| *k).collect()
}

/// Apply one edit and return the file as it now stands.
///
/// The write goes through [`adopt::apply`], so the same three things happen as
/// for an adoption: the result has to parse, it has to validate, and the
/// previous contents land in `routing.toml.bak` first. An edit that would break
/// the config leaves the file untouched and comes back as an error naming what
/// it would have broken.
pub fn edit(paths: &Paths, edit: &Edit) -> Result<RoutingView> {
    let path = paths.routing();
    // An absent file is the empty one. Writing the *first* `routing.toml` from
    // here is the case this whole surface is for — a box nobody has a shell on
    // is exactly the box where `comet-board init` is awkward to reach.
    let before = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => bail!("reading {}: {e}", path.display()),
    };
    let after = match edit {
        Edit::Text { text, base } => {
            if let Some(base) = base
                && base != &before
            {
                bail!(
                    "{} changed on disk since it was read — reload and reapply. \
                     (The board's config is still a file somebody can edit by hand.)",
                    path.display()
                );
            }
            text.clone()
        }
        Edit::Route { route, key, value } => {
            let kind = kind_of(ROUTE_KEYS, key, "route")?;
            set_in_route(&before, *route, key, rendered(kind, value.as_deref())?)?
        }
        Edit::Default { key, value } => {
            let kind = kind_of(DEFAULT_KEYS, key, "[defaults]")?;
            set_in_table(
                &before,
                "[defaults]",
                key,
                rendered(kind, value.as_deref())?,
            )
        }
    };
    adopt::apply(&path, &before, &after)?;
    read(paths)
}

fn rendered(kind: Kind, value: Option<&str>) -> Result<Option<String>> {
    value.map(|v| kind.render(v)).transpose()
}

/// Set (or, with `None`, remove) `key` inside the `n`-th `[[route]]` block.
fn set_in_route(text: &str, n: usize, key: &str, value: Option<String>) -> Result<String> {
    let headers = adopt::header_lines(text);
    let routes: Vec<usize> = headers
        .iter()
        .filter(|(_, name)| name == "[[route]]")
        .map(|(i, _)| *i)
        .collect();
    let Some(&at) = routes.get(n) else {
        bail!(
            "there is no route {} — routing.toml has {}",
            n + 1,
            match routes.len() {
                0 => "none".to_string(),
                1 => "1".to_string(),
                k => format!("{k}"),
            }
        );
    };
    let end = headers
        .iter()
        .map(|(i, _)| *i)
        .find(|i| *i > at)
        .unwrap_or_else(|| text.lines().count());
    Ok(set_between(text, at, end, key, value))
}

/// Set (or remove) `key` in a named top-level table, creating the table when it
/// is not there. Table order carries no meaning in TOML — only `[[route]]`
/// order does — so a created table is appended.
fn set_in_table(text: &str, header: &str, key: &str, value: Option<String>) -> String {
    let headers = adopt::header_lines(text);
    let Some(&(at, _)) = headers.iter().find(|(_, name)| name == header) else {
        let Some(value) = value else {
            // Removing a key from a table that does not exist is already true.
            return text.to_string();
        };
        let mut out: Vec<String> = text.lines().map(str::to_string).collect();
        if out.last().is_some_and(|l| !l.trim().is_empty()) {
            out.push(String::new());
        }
        out.push(header.to_string());
        out.push(format!("{key} = {value}"));
        return adopt::join(&out, text);
    };
    let end = headers
        .iter()
        .map(|(i, _)| *i)
        .find(|i| *i > at)
        .unwrap_or_else(|| text.lines().count());
    set_between(text, at, end, key, value)
}

/// Replace, remove or insert `key = value` between a table header and the next
/// one.
///
/// The multi-line-string tracking is the load-bearing part, exactly as it is in
/// [`adopt::header_lines`]: a route's `prompt = """…"""` is ordinary in this
/// file, and a line inside one that happens to read `base = ...` is prose, not
/// configuration. Editing it would rewrite the agent's brief.
fn set_between(text: &str, header: usize, end: usize, key: &str, value: Option<String>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut open: Option<&'static str> = None;
    let mut found = None;
    for (i, line) in lines.iter().enumerate().take(end) {
        if i > header && open.is_none() {
            let trimmed = line.trim_start();
            if trimmed
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
            {
                found = Some(i);
                break;
            }
        }
        adopt::scan_multiline(line, &mut open);
    }

    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    match (found, value) {
        (Some(i), Some(value)) => {
            let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
            out[i] = format!("{indent}{key} = {value}");
        }
        (Some(i), None) => {
            out.remove(i);
        }
        (None, Some(value)) => {
            // Directly under the header: a route's keys belong with the header
            // that opens it, and inserting at the end of the block would put it
            // after any trailing comment that belongs to the *next* one.
            out.insert(header + 1, format!("{key} = {value}"));
        }
        (None, None) => {}
    }
    adopt::join(&out, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# The board's routes.
[sync]
interval = "30s"

# Offhand's own work.
[[route]]
match = { linear_team = "OFF" }
workspace = "offhand"
repo = "~/code/offhand"
runtime = "claude-code"
prompt = """
You are working on: {title}
base = "not-a-key"
"""

[[route]]
match = { label = "fintech" }
workspace = "fintech"
repo = "~/code/tripletex-int"
runtime = "claude-code"
account = "old-slot"

[defaults]
max_concurrent_per_workspace = 3
"#;

    fn paths_with(text: &str) -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().to_path_buf(),
            state_dir: dir.path().to_path_buf(),
        };
        std::fs::write(paths.routing(), text).unwrap();
        (dir, paths)
    }

    #[test]
    fn a_match_reads_as_the_config_spells_it() {
        let (_dir, paths) = paths_with(SAMPLE);
        let cfg = read(&paths).unwrap().config.unwrap();
        assert_eq!(match_summary(&cfg.routes[0].match_), "linear_team=OFF");
        assert_eq!(match_summary(&cfg.routes[1].match_), "label=fintech");
    }

    /// An empty match matches everything, and every surface has to say so.
    #[test]
    fn an_empty_match_says_catch_all() {
        let (_dir, paths) =
            paths_with("[[route]]\nworkspace = \"w\"\nrepo = \"/tmp\"\nruntime = \"codex\"\n");
        let cfg = read(&paths).unwrap().config.unwrap();
        assert_eq!(match_summary(&cfg.routes[0].match_), "catch-all");
    }

    #[test]
    fn an_inherited_cap_is_named_as_inherited() {
        let (_dir, paths) = paths_with(SAMPLE);
        let cfg = read(&paths).unwrap().config.unwrap();
        assert_eq!(cap_summary(&cfg.routes[0], "2h"), "2h (default)");
        let view = edit(
            &paths,
            &Edit::Route {
                route: 0,
                key: "max_duration".into(),
                value: Some("6h".into()),
            },
        )
        .unwrap();
        assert_eq!(
            cap_summary(&view.config.unwrap().routes[0], "2h"),
            "6h",
            "a route with its own cap does not read as the default's"
        );
    }

    #[test]
    fn reads_the_file_its_parse_and_a_clean_bill() {
        let (_dir, paths) = paths_with(SAMPLE);
        let view = read(&paths).unwrap();
        assert!(view.exists);
        assert_eq!(view.text, SAMPLE);
        assert_eq!(view.config.as_ref().unwrap().routes.len(), 2);
        assert!(view.valid(), "{:?}", view.problems);
        assert!(!view.backup, "nothing has been written yet");
    }

    #[test]
    fn an_absent_file_is_an_empty_config_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().to_path_buf(),
            state_dir: dir.path().to_path_buf(),
        };
        let view = read(&paths).unwrap();
        assert!(!view.exists);
        assert!(view.text.is_empty());
        assert!(view.config.unwrap().routes.is_empty());
    }

    #[test]
    fn a_file_that_does_not_parse_reports_instead_of_failing() {
        let (_dir, paths) = paths_with("[[route]]\nworkspace = \n");
        let view = read(&paths).unwrap();
        assert!(view.config.is_none());
        assert_eq!(view.problems.len(), 1);
        assert!(view.problems[0].contains("does not parse"), "{view:?}");
    }

    /// The whole reason `problems` exists: an editor that shows one at a time
    /// makes fixing three of them three round trips.
    #[test]
    fn every_validation_failure_is_reported_at_once() {
        let (_dir, paths) = paths_with(
            r#"
[[route]]
match = { label = "a" }
workspace = "a"
repo = "/tmp"
runtime = "nonesuch"
max_duration = "banana"
"#,
        );
        let view = read(&paths).unwrap();
        assert_eq!(view.problems.len(), 2, "{:?}", view.problems);
        assert!(view.problems[0].contains("not a comet harness"));
        assert!(view.problems[1].contains("max_duration"));
    }

    #[test]
    fn setting_a_route_key_replaces_it_in_place() {
        let (_dir, paths) = paths_with(SAMPLE);
        let view = edit(
            &paths,
            &Edit::Route {
                route: 1,
                key: "account".into(),
                value: Some("brede-personal".into()),
            },
        )
        .unwrap();
        let route = &view.config.as_ref().unwrap().routes[1];
        assert_eq!(route.account.as_deref(), Some("brede-personal"));
        // The comments and the other route survived.
        assert!(view.text.contains("# Offhand's own work."));
        assert_eq!(view.config.as_ref().unwrap().routes.len(), 2);
        assert!(view.backup, "the previous contents are kept beside it");
        assert_eq!(
            std::fs::read_to_string(adopt::backup_path(&paths.routing())).unwrap(),
            SAMPLE
        );
    }

    #[test]
    fn setting_an_absent_key_inserts_it_under_the_header() {
        let (_dir, paths) = paths_with(SAMPLE);
        let view = edit(
            &paths,
            &Edit::Route {
                route: 0,
                key: "max_duration".into(),
                value: Some("6h".into()),
            },
        )
        .unwrap();
        assert_eq!(
            view.config.as_ref().unwrap().routes[0]
                .max_duration
                .as_deref(),
            Some("6h")
        );
    }

    /// A `prompt = """…"""` containing a line that reads like a key is prose.
    /// Editing it would rewrite the agent's brief.
    #[test]
    fn a_key_inside_a_prompt_is_not_a_key() {
        let (_dir, paths) = paths_with(SAMPLE);
        let view = edit(
            &paths,
            &Edit::Route {
                route: 0,
                key: "base".into(),
                value: Some("origin/main".into()),
            },
        )
        .unwrap();
        let cfg = view.config.as_ref().unwrap();
        assert_eq!(cfg.routes[0].base.as_deref(), Some("origin/main"));
        assert!(
            view.text.contains("base = \"not-a-key\""),
            "the prompt's own line was rewritten:\n{}",
            view.text
        );
    }

    #[test]
    fn clearing_a_key_removes_the_line() {
        let (_dir, paths) = paths_with(SAMPLE);
        let view = edit(
            &paths,
            &Edit::Route {
                route: 1,
                key: "account".into(),
                value: None,
            },
        )
        .unwrap();
        assert!(view.config.as_ref().unwrap().routes[1].account.is_none());
        assert!(!view.text.contains("old-slot"));
    }

    #[test]
    fn defaults_are_set_in_their_table_and_created_when_absent() {
        let (_dir, paths) = paths_with(SAMPLE);
        let view = edit(
            &paths,
            &Edit::Default {
                key: "max_concurrent_per_workspace".into(),
                value: Some("5".into()),
            },
        )
        .unwrap();
        assert_eq!(
            view.config
                .as_ref()
                .unwrap()
                .defaults
                .max_concurrent_per_workspace,
            5
        );

        let (_dir2, empty) = paths_with("[sync]\ninterval = \"30s\"\n");
        let view = edit(
            &empty,
            &Edit::Default {
                key: "notify".into(),
                value: Some("off".into()),
            },
        )
        .unwrap();
        assert!(!view.config.as_ref().unwrap().defaults.notify);
        assert!(view.text.contains("[defaults]"));
    }

    /// The writer refuses what `doctor` would refuse, and leaves the file
    /// exactly as it was — that is the whole reason it is allowed to write.
    #[test]
    fn an_edit_that_would_not_validate_is_refused_and_changes_nothing() {
        let (_dir, paths) = paths_with(SAMPLE);
        let err = edit(
            &paths,
            &Edit::Route {
                route: 0,
                key: "runtime".into(),
                value: Some("claude-codex".into()),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("would not have validated"), "{err}");
        assert_eq!(std::fs::read_to_string(paths.routing()).unwrap(), SAMPLE);
    }

    #[test]
    fn an_unknown_key_is_refused_by_name() {
        let (_dir, paths) = paths_with(SAMPLE);
        let err = edit(
            &paths,
            &Edit::Route {
                route: 0,
                key: "runtiem".into(),
                value: Some("codex".into()),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("`runtiem` is not a route key"), "{err}");
        assert!(err.contains("branch_template"), "it names the set: {err}");
    }

    #[test]
    fn a_bad_value_for_a_typed_key_is_refused_before_anything_is_written() {
        let (_dir, paths) = paths_with(SAMPLE);
        let err = edit(
            &paths,
            &Edit::Route {
                route: 0,
                key: "max_concurrent".into(),
                value: Some("lots".into()),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not a whole number"), "{err}");
        assert_eq!(std::fs::read_to_string(paths.routing()).unwrap(), SAMPLE);
    }

    #[test]
    fn there_is_no_route_seven() {
        let (_dir, paths) = paths_with(SAMPLE);
        let err = edit(
            &paths,
            &Edit::Route {
                route: 6,
                key: "account".into(),
                value: Some("x".into()),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no route 7"), "{err}");
    }

    #[test]
    fn a_whole_file_write_validates_and_backs_up() {
        let (_dir, paths) = paths_with(SAMPLE);
        let replacement = "[[route]]\nmatch = { label = \"x\" }\nworkspace = \"w\"\n\
                           repo = \"/tmp\"\nruntime = \"codex\"\n";
        let view = edit(
            &paths,
            &Edit::Text {
                text: replacement.into(),
                base: Some(SAMPLE.into()),
            },
        )
        .unwrap();
        assert_eq!(view.config.as_ref().unwrap().routes.len(), 1);
        assert_eq!(
            std::fs::read_to_string(adopt::backup_path(&paths.routing())).unwrap(),
            SAMPLE
        );
    }

    /// Somebody is still allowed to ssh in and edit this file. A settings page
    /// that has had it open for an hour must not silently undo that.
    #[test]
    fn a_whole_file_write_refuses_a_stale_base() {
        let (_dir, paths) = paths_with(SAMPLE);
        let err = edit(
            &paths,
            &Edit::Text {
                text: "[sync]\n".into(),
                base: Some("something else entirely".into()),
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("changed on disk"), "{err}");
        assert_eq!(std::fs::read_to_string(paths.routing()).unwrap(), SAMPLE);
    }

    /// The first `routing.toml` on a box nobody has a shell on.
    #[test]
    fn a_write_can_create_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().to_path_buf(),
            state_dir: dir.path().to_path_buf(),
        };
        let view = edit(
            &paths,
            &Edit::Text {
                text: "[sync]\ninterval = \"60s\"\n".into(),
                base: None,
            },
        )
        .unwrap();
        assert!(view.exists);
        assert_eq!(view.config.as_ref().unwrap().sync.interval, "60s");
        assert!(
            !view.backup,
            "a .bak of nothing claims a previous version that never existed"
        );
    }

    #[test]
    fn a_whole_file_write_with_no_base_does_not_check_one() {
        let (_dir, paths) = paths_with(SAMPLE);
        let view = edit(
            &paths,
            &Edit::Text {
                text: "[sync]\ninterval = \"60s\"\n".into(),
                base: None,
            },
        )
        .unwrap();
        assert_eq!(view.config.as_ref().unwrap().sync.interval, "60s");
    }

    #[test]
    fn edits_round_trip_through_json_the_way_the_rpc_sends_them() {
        let sent = serde_json::json!({
            "op": "route", "route": 1, "key": "account", "value": "slot-2"
        });
        let edit: Edit = serde_json::from_value(sent).unwrap();
        assert!(matches!(edit, Edit::Route { route: 1, .. }));
        // An omitted `value` is a removal, not a missing field.
        let cleared: Edit =
            serde_json::from_value(serde_json::json!({"op": "default", "key": "notify"})).unwrap();
        assert!(matches!(cleared, Edit::Default { value: None, .. }));
    }
}
