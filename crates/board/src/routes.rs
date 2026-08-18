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
    /// Set (or, with `value: null`, remove) one `[users]` entry — whose name a
    /// teammate's dispatched commits carry (gh#162).
    ///
    /// Its own op rather than a `Default`-shaped one because the key is data,
    /// not a name from a fixed list: `[users]` is keyed by the email a
    /// teammate signs in with, so what a typo produces here is not an ignored
    /// key but an entry that no dispatch will ever match. The check is
    /// therefore on the *shape* of the key, and the value goes through the
    /// same author parse [`crate::git_identity`] reads it back with.
    ///
    /// `comet-board member add` is the verb; a caller that already holds the
    /// address (a settings page, an agent) can send this directly.
    User {
        user: String,
        #[serde(default)]
        value: Option<String>,
    },
    /// Set (or, with `value: null`, remove) one key under `[account."<slot>"]`
    /// — what one agent-account slot's subscription costs its owner (gh#182).
    ///
    /// Its own op for the same reason [`Edit::User`] is: the key is data, not a
    /// name off a fixed list. It is an account — the email a login signs in as,
    /// or the slot id `doctor` prints — and a typo there produces an entry no
    /// stats row will ever match rather than a refusal.
    ///
    /// This is the write behind the Accounts page's plan field (gh#178): the
    /// cost of a subscription is the one number in this file that comet cannot
    /// discover, and the page that lists the logins is where a person is
    /// already looking when they think about what they pay for one.
    Account {
        slot: String,
        key: String,
        #[serde(default)]
        value: Option<String>,
    },
    /// Set (or, with `value: null`, remove) one key on the `[[automation]]`
    /// named `automation` (gh#490) — pause and resume are `enabled` through
    /// here, so the settings page, the board popover and an ssh session all
    /// move the same key.
    ///
    /// Addressed by *name* rather than by index, unlike [`Edit::Route`]:
    /// automations are created and deleted from surfaces that race each
    /// other, and an index is the one address a concurrent delete silently
    /// re-points at a different rule.
    Automation {
        automation: String,
        key: String,
        #[serde(default)]
        value: Option<String>,
    },
    /// Create a rule (gh#490): a disabled skeleton named `name`, or — with
    /// `from` — a copy of that rule's block, renamed and **disabled**. A
    /// duplicate never starts enabled, whatever its source said: enabling is
    /// the authorization, and it is given to a rule, not inherited by its
    /// copies.
    AutomationAdd {
        name: String,
        #[serde(default)]
        from: Option<String>,
    },
    /// Delete a rule's whole block (gh#490). Its history in `board.db` keeps
    /// itself until retention prunes it; work already running is untouched —
    /// deleting a rule prevents future dispatches, never cancels attempts.
    AutomationRemove { name: String },
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
    // The turn guardrails (gh#270). Strings, not integers, because `off` is a
    // value they both take — and the one somebody reaches for at 02:00 when a
    // route's work legitimately fails more than the board expects.
    ("max_tool_failures", Kind::Str),
    ("max_tool_calls", Kind::Str),
    ("archive_chats", Kind::Str),
    ("billing_guard", Kind::Str),
    ("agent_instructions", Kind::Bool),
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
    ("max_tool_failures", Kind::Str),
    ("max_tool_calls", Kind::Str),
    // All three retentions (gh#72, gh#139, gh#186): the shelf and the disk fill
    // up on a box nobody has a shell on, which is the box where this surface is
    // the only way to say how long they may. The build output is the one that
    // fills a disk in a day rather than a month, so it is the one somebody
    // reaches for while the disk is already full.
    ("retain_worktrees", Kind::Str),
    ("retain_build_output", Kind::Str),
    ("archive_chats", Kind::Str),
    ("billing_guard", Kind::Str),
    // The one write that reaches into a file the board does not own — the box
    // user's own instruction file (gh#272). Here because turning it off is the
    // whole opt-out, and the box where somebody wants it off is as likely to be
    // reached over ssh as sat at.
    ("agent_instructions", Kind::Bool),
];

/// The keys [`Edit::Account`] may set on one `[account."<slot>"]` table.
const ACCOUNT_KEYS: &[(&str, Kind)] = &[
    ("email", Kind::Str),
    ("plan", Kind::Str),
    ("monthly_usd", Kind::Money),
];

/// The keys [`Edit::Automation`] may set (gh#490). `name` is deliberately
/// absent: it is the rule's identity — its history and its live attempts are
/// keyed on it — so renaming is a whole-file edit somebody does knowingly.
const AUTOMATION_KEYS: &[(&str, Kind)] = &[
    ("enabled", Kind::Bool),
    ("owner", Kind::Str),
    ("source", Kind::Str),
    ("labels", Kind::StrList),
    ("exclude_labels", Kind::StrList),
    ("route", Kind::Str),
    ("runtime", Kind::Str),
    ("model", Kind::Str),
    ("account", Kind::Str),
    ("max_per_eval", Kind::Int),
    ("max_concurrent", Kind::Int),
    ("daily_budget", Kind::Int),
    ("cooldown", Kind::Str),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Str,
    Int,
    Bool,
    /// A comma-separated list of words, written as a TOML string array —
    /// `auto, approved` becomes `["auto", "approved"]`. What a text field has
    /// where the config wants a list (gh#490's labels).
    StrList,
    /// An amount in US dollars. Its own kind because money is neither an
    /// integer (`monthly_usd = 17.50` is a real plan) nor free text, and
    /// because a negative subscription is the one value the config validator
    /// would refuse *after* the write — better to name it here, beside the
    /// number somebody typed.
    Money,
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
            Kind::StrList => {
                let items: Vec<String> = v
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(adopt::toml_string)
                    .collect();
                Ok(format!("[{}]", items.join(", ")))
            }
            // `$200`, `200`, `17.50`, `1,200` — what a person writes when asked
            // what a plan costs. The currency is dollars because the rate table
            // is; a board on another currency writes the converted figure, and
            // saying so is a job for the `plan` label beside it.
            Kind::Money => {
                let cleaned: String = v
                    .trim_start_matches(['$', '\u{a0}'])
                    .chars()
                    .filter(|c| *c != ',' && !c.is_whitespace())
                    .collect();
                match cleaned.parse::<f64>() {
                    Ok(n) if n.is_finite() && n >= 0.0 => Ok(trim_zeros(n)),
                    Ok(_) => bail!(
                        "`{value}` is not what a subscription costs — an amount per month, \
                         and never below zero"
                    ),
                    Err(_) => bail!("`{value}` is not an amount in dollars"),
                }
            }
        }
    }
}

/// An amount as TOML: `200`, not `200.00000000000001`.
///
/// Rounded to the cent first — a plan is billed in cents, and a float that came
/// out of a text field should not put four more digits in a file somebody
/// reads.
fn trim_zeros(amount: f64) -> String {
    let cents = (amount * 100.0).round() / 100.0;
    if cents.fract() == 0.0 {
        format!("{}", cents as i64)
    } else {
        format!("{cents:.2}")
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

/// The `[[automation]]` keys a caller may set (gh#490).
pub fn automation_keys() -> Vec<&'static str> {
    AUTOMATION_KEYS.iter().map(|(k, _)| *k).collect()
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
        Edit::User { user, value } => {
            let user = user.trim();
            if !crate::git_identity::plausible_email(user) {
                bail!(
                    "`{user}` is not a sign-in email. `[users]` is keyed by the address \
                     the teammate signs in to comet with — that is what a dispatch \
                     arrives as — and valued with their git author; their GitHub login \
                     goes on the other side."
                );
            }
            // Refused here rather than left to `apply`'s revalidation so the
            // error names the value somebody typed instead of reporting that
            // the whole config would not have validated.
            if let Some(value) = value
                && crate::git_identity::parse_author(value).is_none()
            {
                bail!(
                    "`{value}` is not a git author — write an email address or \
                     `Name <email>`. GitHub attributes commits to the account owning \
                     the address; `<id>+<login>@users.noreply.github.com` from \
                     https://github.com/settings/emails always works."
                );
            }
            let key = user_key(&before, user);
            set_in_table(
                &before,
                "[users]",
                &key,
                value.as_deref().map(adopt::toml_string),
            )
        }
        Edit::Account { slot, key, value } => {
            let slot = slot.trim();
            // The key is an account, and the two spellings it comes in — an
            // email or a slot id — share the only shape worth checking: one
            // word, no quoting to get wrong.
            if slot.is_empty() || slot.chars().any(|c| c.is_whitespace() || c.is_control()) {
                bail!(
                    "`{slot}` is not an account. `[account.\"…\"]` is keyed by the email a \
                     login signs in as, or by the slot id `comet-board doctor` lists."
                );
            }
            let kind = kind_of(ACCOUNT_KEYS, key, "[account]")?;
            let header = account_header(&before, slot);
            set_in_table(&before, &header, key, rendered(kind, value.as_deref())?)
        }
        Edit::Automation {
            automation,
            key,
            value,
        } => {
            let kind = kind_of(AUTOMATION_KEYS, key, "automation")?;
            let (at, end) = automation_block(&before, automation)?;
            set_between(&before, at, end, key, rendered(kind, value.as_deref())?)
        }
        Edit::AutomationAdd { name, from } => add_automation(&before, name, from.as_deref())?,
        Edit::AutomationRemove { name } => {
            let (at, end) = automation_block(&before, name)?;
            let lines: Vec<&str> = before.lines().collect();
            let mut out: Vec<String> = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                if i < at || i >= end {
                    out.push((*line).to_string());
                }
            }
            adopt::join(&out, &before)
        }
    };
    adopt::apply(&path, &before, &after)?;
    read(paths)
}

/// The line range of the `[[automation]]` block whose `name` is `name` —
/// header line to the line the next header starts on (or EOF). Matched
/// case-insensitively and unquoted, the way every other name in this file is
/// read back.
fn automation_block(text: &str, name: &str) -> Result<(usize, usize)> {
    let name = name.trim();
    let headers = adopt::header_lines(text);
    let total = text.lines().count();
    let lines: Vec<&str> = text.lines().collect();
    for (i, (at, header)) in headers.iter().enumerate() {
        if header != "[[automation]]" {
            continue;
        }
        let end = headers
            .get(i + 1)
            .map(|(next, _)| *next)
            .unwrap_or(total);
        let named = lines[*at..end].iter().any(|line| {
            let Some((key, value)) = line.split_once('=') else {
                return false;
            };
            key.trim() == "name" && value.trim().trim_matches(['"', '\'']).eq_ignore_ascii_case(name)
        });
        if named {
            return Ok((*at, end));
        }
    }
    bail!("there is no automation named `{name}`");
}

/// Append a new `[[automation]]` block: a disabled skeleton, or — with `from`
/// — a disabled copy of that rule. The copy keeps everything but the name and
/// the enablement, comments included: duplicating is how a working rule
/// becomes the starting point for the next one.
fn add_automation(text: &str, name: &str, from: Option<&str>) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("an automation needs a name");
    }
    if automation_block(text, name).is_ok() {
        bail!("there is already an automation named `{name}`");
    }
    let mut block: Vec<String> = match from {
        Some(from) => {
            let (at, end) = automation_block(text, from)?;
            let lines: Vec<&str> = text.lines().collect();
            let mut copied: Vec<String> = Vec::new();
            let mut had_enabled = false;
            for line in &lines[at..end] {
                let key = line.split_once('=').map(|(k, _)| k.trim());
                match key {
                    Some("name") => copied.push(format!("name = {}", adopt::toml_string(name))),
                    Some("enabled") => {
                        had_enabled = true;
                        copied.push("enabled = false".to_string());
                    }
                    _ => copied.push((*line).to_string()),
                }
            }
            if !had_enabled {
                copied.insert(1, "enabled = false".to_string());
            }
            // Trim the trailing blank lines a mid-file block carries; the
            // append below re-adds exactly one separator.
            while copied.last().is_some_and(|l| l.trim().is_empty()) {
                copied.pop();
            }
            copied
        }
        None => vec![
            "[[automation]]".to_string(),
            format!("name = {}", adopt::toml_string(name)),
            "enabled = false".to_string(),
        ],
    };
    let mut out: Vec<String> = text.lines().map(str::to_string).collect();
    if out.last().is_some_and(|l| !l.trim().is_empty()) {
        out.push(String::new());
    }
    out.append(&mut block);
    Ok(adopt::join(&out, text))
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

/// The spelling `[users]` already uses for an address, or a freshly quoted one.
///
/// Emails are compared case-insensitively wherever the map is *read*
/// ([`RoutingConfig::git_author_for`]), so writing `Ana@Example.com` beside an
/// existing `ana@example.com` would be two entries for one person and a map
/// whose behaviour depends on which one TOML happened to keep. The key already
/// in the file wins, verbatim — including its quoting, since a re-quoted
/// version of the same address would not match the line and would append a
/// duplicate.
///
/// Read off the text rather than off the parse for that last reason: the parse
/// gives the logical key, and it is the literal that has to be edited.
fn user_key(text: &str, user: &str) -> String {
    let headers = adopt::header_lines(text);
    if let Some(&(at, _)) = headers.iter().find(|(_, name)| name == "[users]") {
        let end = headers
            .iter()
            .map(|(i, _)| *i)
            .find(|i| *i > at)
            .unwrap_or_else(|| text.lines().count());
        for line in text.lines().take(end).skip(at + 1) {
            let Some((key, _)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if key.starts_with('#') {
                continue;
            }
            if key.trim_matches(['"', '\'']).eq_ignore_ascii_case(user) {
                return key.to_string();
            }
        }
    }
    adopt::toml_string(user)
}

/// The `[account."…"]` header already in the file for this slot, or a freshly
/// quoted one.
///
/// Same rule as [`user_key`] and for the same reason: the slot is matched
/// case-insensitively wherever it is *read* ([`crate::prices`]), so writing
/// `[account."Brede@tally.no"]` beside an existing `[account."brede@tally.no"]`
/// would be two plans for one subscription and the board would add them both up.
fn account_header(text: &str, slot: &str) -> String {
    for (_, name) in adopt::header_lines(text) {
        let Some(rest) = name.strip_prefix("[account.") else {
            continue;
        };
        let Some(key) = rest.strip_suffix(']') else {
            continue;
        };
        if key
            .trim()
            .trim_matches(['"', '\''])
            .eq_ignore_ascii_case(slot)
        {
            return name;
        }
    }
    format!("[account.{}]", adopt::toml_string(slot))
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
        // The map's own op (gh#162), keyed by data rather than by a name from
        // a list.
        let user: Edit = serde_json::from_value(serde_json::json!({
            "op": "user", "user": "ana@example.com",
            "value": "22494697+ana@users.noreply.github.com"
        }))
        .unwrap();
        assert!(matches!(user, Edit::User { .. }));
        // And the plan field on the Accounts page (gh#178), which is the same
        // shape a key away.
        let plan: Edit = serde_json::from_value(serde_json::json!({
            "op": "account", "slot": "brede@tally.no", "key": "monthly_usd", "value": "200"
        }))
        .unwrap();
        assert!(matches!(plan, Edit::Account { .. }));
    }

    // ── the `[users]` map (gh#162) ──────────────────────────────────────────

    fn user_edit(paths: &Paths, user: &str, value: Option<&str>) -> Result<RoutingView> {
        edit(
            paths,
            &Edit::User {
                user: user.into(),
                value: value.map(str::to_string),
            },
        )
    }

    /// The first entry writes the table; the second joins it; a third with the
    /// same person corrects the line rather than adding one.
    #[test]
    fn a_member_add_writes_the_map_and_re_running_it_changes_nothing() {
        let (_dir, paths) = paths_with(SAMPLE);
        let view = user_edit(
            &paths,
            "ana@example.com",
            Some("22494697+ana@users.noreply.github.com"),
        )
        .unwrap();
        assert_eq!(
            view.config.as_ref().unwrap().users["ana@example.com"],
            "22494697+ana@users.noreply.github.com"
        );
        assert!(view.problems.is_empty(), "{:?}", view.problems);
        // The routes above it are untouched — this is a text edit, and the
        // comments in the file are the reason.
        assert!(view.text.contains("# Offhand's own work."));

        let again = user_edit(
            &paths,
            "ana@example.com",
            Some("22494697+ana@users.noreply.github.com"),
        )
        .unwrap();
        assert_eq!(again.text, view.text, "a repeat write moved the file");

        let two = user_edit(
            &paths,
            "sam@example.com",
            Some("Sam Ito <8134+sam@users.noreply.github.com>"),
        )
        .unwrap();
        let users = &two.config.as_ref().unwrap().users;
        assert_eq!(users.len(), 2, "{users:?}");

        // The dispatch-time reader agrees with what was written.
        let author = two
            .config
            .as_ref()
            .unwrap()
            .git_author_for("SAM@example.com")
            .unwrap();
        assert_eq!(author.name, "Sam Ito");
    }

    /// The map is read case-insensitively, so it must be *written* that way
    /// too: a second entry differing only in case is one person with two
    /// answers, and which one wins is whichever TOML happened to keep.
    #[test]
    fn a_second_spelling_of_one_address_corrects_the_entry_it_already_has() {
        let (_dir, paths) = paths_with(SAMPLE);
        user_edit(
            &paths,
            "ana@example.com",
            Some("1+ana@users.noreply.github.com"),
        )
        .unwrap();
        let view = user_edit(
            &paths,
            "Ana@Example.COM",
            Some("22494697+ana@users.noreply.github.com"),
        )
        .unwrap();
        let users = &view.config.as_ref().unwrap().users;
        assert_eq!(users.len(), 1, "{users:?}");
        assert_eq!(
            users["ana@example.com"],
            "22494697+ana@users.noreply.github.com"
        );
    }

    /// Offboarding, and the entry for the wrong account.
    #[test]
    fn removing_somebody_leaves_the_rest_of_the_map_alone() {
        let (_dir, paths) = paths_with(SAMPLE);
        user_edit(
            &paths,
            "ana@example.com",
            Some("1+ana@users.noreply.github.com"),
        )
        .unwrap();
        user_edit(
            &paths,
            "sam@example.com",
            Some("2+sam@users.noreply.github.com"),
        )
        .unwrap();
        let view = user_edit(&paths, "ANA@example.com", None).unwrap();
        let users = &view.config.as_ref().unwrap().users;
        assert_eq!(users.len(), 1, "{users:?}");
        assert!(users.contains_key("sam@example.com"));
        // Removing what is not there is already true, and must not error.
        assert!(user_edit(&paths, "nobody@example.com", None).is_ok());
    }

    /// Both halves are refused by name, and the file is untouched — the value
    /// because `[users]` holding a non-address is the unattributable commit
    /// gh#107 exists to prevent, the key because a `member add` given a login
    /// where the sign-in email goes writes an entry no dispatch can match.
    #[test]
    fn a_key_or_a_value_that_is_not_an_address_is_refused_rather_than_written() {
        let (_dir, paths) = paths_with(SAMPLE);
        let before = read(&paths).unwrap().text;

        let bad_value = user_edit(&paths, "ana@example.com", Some("ana")).unwrap_err();
        assert!(
            format!("{bad_value:#}").contains("not a git author"),
            "{bad_value:#}"
        );
        let bad_key = user_edit(&paths, "ana", Some("1+ana@users.noreply.github.com")).unwrap_err();
        assert!(
            format!("{bad_key:#}").contains("not a sign-in email"),
            "{bad_key:#}"
        );
        assert_eq!(read(&paths).unwrap().text, before, "the file moved");
    }

    /// A file whose `[users]` table was hand-written keeps its own spelling of
    /// a key — re-quoting it would not match the line and would append a
    /// duplicate the whole config then fails to parse on.
    #[test]
    fn a_hand_written_key_is_edited_in_place_whatever_its_quoting() {
        let (_dir, paths) = paths_with(
            "[users]\n'ana@example.com' = \"1+ana@users.noreply.github.com\"\n\n[defaults]\n",
        );
        let view = user_edit(
            &paths,
            "ana@example.com",
            Some("22494697+ana@users.noreply.github.com"),
        )
        .unwrap();
        assert!(
            view.text
                .contains("'ana@example.com' = \"22494697+ana@users.noreply.github.com\""),
            "{}",
            view.text
        );
        assert_eq!(view.config.as_ref().unwrap().users.len(), 1);
    }

    // ── what a plan costs (gh#182), entered from Accounts (gh#178) ──────────

    fn plan_edit(paths: &Paths, slot: &str, key: &str, value: Option<&str>) -> Result<RoutingView> {
        edit(
            paths,
            &Edit::Account {
                slot: slot.into(),
                key: key.into(),
                value: value.map(str::to_string),
            },
        )
    }

    #[test]
    fn a_plan_cost_lands_in_its_own_account_table_and_reads_back() {
        let (_dir, paths) = paths_with(SAMPLE);
        let view = plan_edit(&paths, "brede@tally.no", "monthly_usd", Some("$200")).unwrap();
        assert!(
            view.text.contains("[account.\"brede@tally.no\"]"),
            "{}",
            view.text
        );
        assert!(view.text.contains("monthly_usd = 200"), "{}", view.text);
        let cfg = view.config.as_ref().unwrap();
        assert_eq!(
            cfg.accounts["brede@tally.no"].monthly_usd,
            comet_proto::view::rates::Usd::from_dollars(200.0)
        );

        // A second key joins the table that is now there rather than opening a
        // second one — two `[account."…"]` headers for one slot do not parse.
        let view = plan_edit(&paths, "brede@tally.no", "plan", Some("Claude Max 20x")).unwrap();
        assert_eq!(view.text.matches("[account.").count(), 1, "{}", view.text);
        let cfg = view.config.as_ref().unwrap();
        assert_eq!(
            cfg.accounts["brede@tally.no"].plan.as_deref(),
            Some("Claude Max 20x")
        );
        assert_eq!(
            cfg.accounts["brede@tally.no"].monthly_usd,
            comet_proto::view::rates::Usd::from_dollars(200.0)
        );

        // And clearing it takes the line out — an unknown plan is not a $0 one.
        let view = plan_edit(&paths, "brede@tally.no", "monthly_usd", None).unwrap();
        assert!(!view.text.contains("monthly_usd"), "{}", view.text);
    }

    /// The slot is matched however it was written down — the same rule
    /// `[users]` follows, and for the same reason: two tables for one
    /// subscription would be counted twice.
    #[test]
    fn an_existing_account_table_keeps_its_spelling() {
        let (_dir, paths) =
            paths_with("[account.\"Brede@Tally.no\"]\nmonthly_usd = 100\n\n[defaults]\n");
        let view = plan_edit(&paths, "brede@tally.no", "monthly_usd", Some("200")).unwrap();
        assert_eq!(view.text.matches("[account.").count(), 1, "{}", view.text);
        assert!(
            view.text.contains("[account.\"Brede@Tally.no\"]"),
            "{}",
            view.text
        );
        assert_eq!(view.config.as_ref().unwrap().accounts.len(), 1);
    }

    #[test]
    fn an_amount_is_read_the_way_a_person_writes_one() {
        let (_dir, paths) = paths_with(SAMPLE);
        for (typed, written) in [
            ("200", "monthly_usd = 200"),
            ("$1,200", "monthly_usd = 1200"),
            ("17.50", "monthly_usd = 17.5"),
            (" 20 ", "monthly_usd = 20"),
        ] {
            let view = plan_edit(&paths, "slot@example.com", "monthly_usd", Some(typed)).unwrap();
            assert!(view.text.contains(written), "{typed}:\n{}", view.text);
        }
        // What is not an amount is refused by name, before anything is written.
        let before = read(&paths).unwrap().text;
        for bad in ["free", "-200", "two hundred"] {
            let err = plan_edit(&paths, "slot@example.com", "monthly_usd", Some(bad))
                .unwrap_err()
                .to_string();
            assert!(err.contains(bad), "{err}");
        }
        assert_eq!(read(&paths).unwrap().text, before, "the file moved");
    }

    #[test]
    fn an_account_edit_refuses_a_key_it_cannot_set_and_a_slot_that_is_not_one() {
        let (_dir, paths) = paths_with(SAMPLE);
        let err = plan_edit(&paths, "brede@tally.no", "montly_usd", Some("200"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`montly_usd` is not a [account] key"), "{err}");
        assert!(err.contains("monthly_usd"), "it names the set: {err}");

        let err = plan_edit(&paths, "  ", "monthly_usd", Some("200"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is not an account"), "{err}");
        assert_eq!(read(&paths).unwrap().text, SAMPLE, "the file moved");
    }

    // ---- automation ops (gh#490) ----------------------------------------

    fn automation_edit(
        paths: &Paths,
        name: &str,
        key: &str,
        value: Option<&str>,
    ) -> Result<RoutingView> {
        edit(
            paths,
            &Edit::Automation {
                automation: name.into(),
                key: key.into(),
                value: value.map(str::to_string),
            },
        )
    }

    /// The whole lifecycle through the ops the settings page sends: created
    /// paused, filled in key by key, enabled, copied (disabled), removed.
    #[test]
    fn automations_are_added_edited_duplicated_and_removed_by_name() {
        let (_dir, paths) = paths_with(SAMPLE);
        let view = edit(
            &paths,
            &Edit::AutomationAdd {
                name: "approved".into(),
                from: None,
            },
        )
        .unwrap();
        let rule = &view.config.as_ref().unwrap().automations[0];
        assert_eq!(rule.name, "approved");
        assert!(!rule.enabled, "a fresh rule never starts enabled");

        // A second rule with the same name is one history for two rules, and
        // is refused before anything is written.
        let err = edit(
            &paths,
            &Edit::AutomationAdd {
                name: "Approved".into(),
                from: None,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already an automation"), "{err}");

        automation_edit(&paths, "approved", "owner", Some("Brede")).unwrap();
        // The list spelling a text field has, written as a TOML array.
        automation_edit(&paths, "approved", "labels", Some("auto, approved")).unwrap();
        automation_edit(&paths, "approved", "account", Some("slot-1")).unwrap();
        let view = automation_edit(&paths, "approved", "enabled", Some("true")).unwrap();
        let rule = &view.config.as_ref().unwrap().automations[0];
        assert!(rule.enabled);
        assert_eq!(rule.labels, vec!["auto", "approved"]);
        assert_eq!(rule.owner.as_deref(), Some("Brede"));

        // Duplicate: everything copied, name replaced, enablement dropped —
        // authorization is given to a rule, never inherited by its copies.
        let view = edit(
            &paths,
            &Edit::AutomationAdd {
                name: "approved-docs".into(),
                from: Some("approved".into()),
            },
        )
        .unwrap();
        let copy = view
            .config
            .as_ref()
            .unwrap()
            .automations
            .iter()
            .find(|a| a.name == "approved-docs")
            .unwrap();
        assert!(!copy.enabled);
        assert_eq!(copy.labels, vec!["auto", "approved"]);
        assert_eq!(copy.owner.as_deref(), Some("Brede"));

        let view = edit(
            &paths,
            &Edit::AutomationRemove {
                name: "approved-docs".into(),
            },
        )
        .unwrap();
        assert_eq!(view.config.as_ref().unwrap().automations.len(), 1);
        // And the routes around the rules never moved.
        assert_eq!(view.config.as_ref().unwrap().routes.len(), 2);
        assert!(view.text.contains("# Offhand's own work."));
    }

    /// Enabling a rule that cannot run is refused by the validating writer —
    /// with the config's own sentence — and the file does not move. This is
    /// the write behind the settings page's Enable, so the refusal is the
    /// confirmation dialog's error.
    #[test]
    fn enabling_a_half_written_rule_is_refused_and_changes_nothing() {
        let (_dir, paths) = paths_with(SAMPLE);
        edit(
            &paths,
            &Edit::AutomationAdd {
                name: "hasty".into(),
                from: None,
            },
        )
        .unwrap();
        let before = read(&paths).unwrap().text;
        // The whole chain: `apply` wraps the validator's sentence, and the
        // RPC layer prints the chain (`{e:#}`), so the page reads both.
        let err = format!("{:#}", automation_edit(&paths, "hasty", "enabled", Some("true")).unwrap_err());
        assert!(err.contains("no owner") || err.contains("no required labels"), "{err}");
        assert_eq!(read(&paths).unwrap().text, before, "the file moved");
    }

    /// The ops address rules by name, and an unknown name or key is refused
    /// by name rather than written and ignored.
    #[test]
    fn automation_ops_refuse_unknown_names_and_keys() {
        let (_dir, paths) = paths_with(SAMPLE);
        let err = automation_edit(&paths, "ghost", "owner", Some("x"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no automation named `ghost`"), "{err}");

        edit(
            &paths,
            &Edit::AutomationAdd {
                name: "real".into(),
                from: None,
            },
        )
        .unwrap();
        let err = automation_edit(&paths, "real", "onwer", Some("x"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("`onwer` is not a automation key"), "{err}");
        assert!(err.contains("owner"), "it names the set: {err}");
    }

    /// The ops as the RPC sends them — the page writes JSON, and a renamed
    /// tag would strand it.
    #[test]
    fn automation_edits_round_trip_through_json() {
        let add: Edit = serde_json::from_value(serde_json::json!({
            "op": "automationAdd", "name": "approved", "from": "other"
        }))
        .unwrap();
        assert!(matches!(add, Edit::AutomationAdd { ref name, ref from }
            if name == "approved" && from.as_deref() == Some("other")));
        let set: Edit = serde_json::from_value(serde_json::json!({
            "op": "automation", "automation": "approved", "key": "enabled", "value": "true"
        }))
        .unwrap();
        assert!(matches!(set, Edit::Automation { ref automation, ref key, ref value }
            if automation == "approved" && key == "enabled" && value.as_deref() == Some("true")));
        let remove: Edit = serde_json::from_value(serde_json::json!({
            "op": "automationRemove", "name": "approved"
        }))
        .unwrap();
        assert!(matches!(remove, Edit::AutomationRemove { ref name } if name == "approved"));
    }
}
