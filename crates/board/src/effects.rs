//! The effects of a branch, read off the branch (§gh#236).
//!
//! A summary written by the model that wrote the code inherits its blind spots:
//! a misunderstanding comes back described fluently and confidently. So every
//! claim on the review screen carries evidence the agent did not produce, and
//! this module is where that evidence is derived. [`crate::claims`] answers
//! *which files moved*; [`crate::evidence`] answers *what ran*. This one
//! answers the questions a reviewer asks before either: did the public surface
//! move, did the schema, did the config grow a key, did anything new arrive in
//! the dependency list, and are there more tests now than there were.
//!
//! Nothing here is parsed out of anything an agent said. The inputs are the
//! unified diff of `base..HEAD`, the two trees on either side of it, and the
//! run journal — all four read by the board, from git and from the harness.
//!
//! ## Every rule is lexical, and each one fails in a stated direction
//!
//! There is no parser per language here, for the reason §gh#183 gave for not
//! building one: it is a per-language project, and a lexical read of the lines
//! that actually moved is honest about what it knows and is the same for Rust,
//! Swift, TOML and whatever gets added next. What that costs is stated per
//! question rather than glossed, because the direction a rule fails in is the
//! whole of whether it is safe:
//!
//! - **The public surface** fails *loudly*. A file in a language whose spelling
//!   of "public" the board does not know makes the whole answer
//!   [`Surface::Unknown`] rather than [`Surface::Unchanged`] — a chip that said
//!   "unchanged" because nobody looked would be the exact failure this screen
//!   exists to prevent.
//! - **The schema** and **config keys** are recognised by a fixed list of
//!   spellings (SQL DDL, a schema-shaped path, `key = value` in a config file).
//!   A schema migration written some other way reads as no change. That one is
//!   a quiet miss and it is named here rather than hidden: the chip is a
//!   pointer, and the unclaimed set is what actually holds the line.
//! - **Dependencies** are read from the manifests themselves — the file before
//!   and the file after, both out of git — so this one is not a heuristic at
//!   all. A manifest the board cannot parse makes the answer unknown, never
//!   "none".
//! - **Tests** are counted in both trees by the same rule, so the two numbers
//!   are comparable even where the rule is imperfect. Whether they *pass* is
//!   not counted at all: it comes off the run journal's exit statuses, and a
//!   suite nobody ran says so instead of saying "all passing".
//!
//! ## What this module does not do
//!
//! It does not run the tests. The board reconciles on a loop over every live
//! attempt, and a test suite per attempt per tick would cost more than the
//! screen is worth — and, on this box, a `target/` per worktree (gh#186). What
//! it does instead is read the exit status of the runs that *did* happen, which
//! the harness recorded without being asked and which the agent did not write.

use serde::{Deserialize, Serialize};

use crate::claims::ChangedFile;
use crate::evidence::RunEvidence;

// ---- chips ---------------------------------------------------------------

/// What a chip is standing on, and so how loudly it reads.
///
/// Four, and the split that matters is [`Ground::Neutral`] against
/// [`Ground::Unknown`]: "the board looked and nothing moved" and "the board
/// could not look" are opposite facts that a single quiet chip would render
/// identically. Nothing here is ever the review's alarm hue — that one belongs
/// to the unclaimed set alone, and a screen with three things shouting has
/// nothing left to shout with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ground {
    /// A fact that is not news: the board checked, and the surface held.
    Neutral,
    /// Corroboration — evidence *for* something a claim asserted.
    Settled,
    /// Not wrong, but a thing to look at: a new dependency, a suite nobody ran,
    /// a claim with nothing behind it.
    Working,
    /// The board could not answer. Never rendered as either of the first two.
    Unknown,
}

/// One derived fact, phrased once, for every surface that draws it.
///
/// The phrasing lives here rather than in the renderers for the reason the
/// verdict does (§gh#183): two surfaces that worded the same attempt for
/// themselves would eventually disagree about it, and the one screen that must
/// never happen on is the screen whose whole job is to be trusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chip {
    pub text: String,
    pub ground: Ground,
}

impl Chip {
    fn new(text: impl Into<String>, ground: Ground) -> Self {
        Chip {
            text: text.into(),
            ground,
        }
    }
}

/// Is there anything here that *supports* a claim, as opposed to merely
/// describing it? What decides whether a claim's glyph is `✓` or `·`.
pub fn corroborated(chips: &[Chip]) -> bool {
    chips.iter().any(|c| c.ground == Ground::Settled)
}

// ---- what kind of file this is -------------------------------------------

/// What the board is willing to say about a file, decided by its name.
///
/// The distinction that carries weight is between a file that *cannot* hold a
/// public API (a changelog, a lockfile) and one that could but is written in a
/// language this module has no rule for. The first is irrelevant to the
/// question; the second makes the answer unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    Rust,
    /// TypeScript and JavaScript, which spell `export` the same way.
    TypeScript,
    Python,
    Go,
    Swift,
    /// Source the board can see is source and cannot read: Ruby, Java, C.
    OtherCode,
    /// A dependency manifest or its lockfile.
    Manifest,
    /// Configuration — TOML, JSON, YAML, `.env` — that is not a manifest.
    Config,
    /// A migration, a `.sql` file, a schema definition.
    Schema,
    /// Documentation, assets, anything nobody imports.
    Prose,
    /// An extension the board does not recognise at all.
    Opaque,
}

impl FileKind {
    /// Does the board know how this language spells a public declaration?
    pub fn reads_api(self) -> bool {
        matches!(
            self,
            FileKind::Rust
                | FileKind::TypeScript
                | FileKind::Python
                | FileKind::Go
                | FileKind::Swift
        )
    }

    /// Could this file carry a public API at all? A lockfile and a changelog
    /// cannot, so neither makes the answer unknown by being unreadable.
    pub fn may_hold_api(self) -> bool {
        matches!(self, FileKind::OtherCode | FileKind::Opaque) || self.reads_api()
    }
}

/// Which kind of file this path is, by extension and by name.
///
/// Manifests are checked before config: `Cargo.toml` is both, and the
/// dependency question is the one worth asking about it. Schema is checked
/// before everything: a `.sql` under `migrations/` is a schema file wherever it
/// sits in the tree.
pub fn classify(path: &str) -> FileKind {
    let name = path.rsplit('/').next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    let ext = lower.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    if matches!(ext, "sql" | "prisma" | "graphql" | "gql")
        || path.contains("/migrations/")
        || path.starts_with("migrations/")
    {
        return FileKind::Schema;
    }
    if matches!(
        lower.as_str(),
        "cargo.toml"
            | "cargo.lock"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "pyproject.toml"
            | "poetry.lock"
            | "uv.lock"
            | "go.mod"
            | "go.sum"
            | "gemfile"
            | "gemfile.lock"
            | "composer.json"
            | "composer.lock"
    ) || lower.starts_with("requirements") && ext == "txt"
    {
        return FileKind::Manifest;
    }
    match ext {
        "rs" => FileKind::Rust,
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => FileKind::TypeScript,
        "py" => FileKind::Python,
        "go" => FileKind::Go,
        "swift" => FileKind::Swift,
        "rb" | "java" | "kt" | "kts" | "c" | "h" | "cc" | "cpp" | "hpp" | "cs" | "php" | "sh"
        | "bash" | "zsh" | "lua" | "ex" | "exs" | "scala" | "dart" | "m" | "mm" => {
            FileKind::OtherCode
        }
        "toml" | "json" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "properties" | "env" => {
            FileKind::Config
        }
        "md" | "markdown" | "txt" | "rst" | "adoc" | "png" | "jpg" | "jpeg" | "gif" | "svg"
        | "webp" | "ico" | "pdf" | "css" | "scss" | "html" => FileKind::Prose,
        _ if lower.starts_with(".env") => FileKind::Config,
        _ if matches!(
            lower.as_str(),
            "license" | "notice" | "authors" | "changelog"
        ) =>
        {
            FileKind::Prose
        }
        _ => FileKind::Opaque,
    }
}

// ---- reading the diff ----------------------------------------------------

/// What one changed file's own diff says about it.
///
/// Counts of *lines*, not of items: a `pub fn` whose signature moved twice is
/// two lines and this says two. The chips built from these say "items changed",
/// which is the honest reading of a number that came from counting lines that
/// declare things.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileScan {
    pub path: String,
    pub kind: Option<FileKind>,
    /// Test declarations this file's diff added, and removed.
    pub tests_added: u32,
    pub tests_removed: u32,
    /// Changed lines that declare something public.
    pub api: u32,
    /// Changed lines that are schema statements, or any changed line in a
    /// schema file.
    pub schema: u32,
    /// Distinct config keys the diff *added*. Removed keys are not counted:
    /// the chip is "no config keys added", and a key that left is a different
    /// question nobody asked here.
    pub config_keys: u32,
}

impl FileScan {
    fn kind(&self) -> FileKind {
        self.kind.unwrap_or(FileKind::Opaque)
    }
}

/// Read a `git diff -U0` of the branch into one scan per changed file.
///
/// `changed` fixes the set and the order — a path only the diff text mentions
/// is not invented here — and `-U0` is what makes every line this reads a line
/// that actually moved. Context is not evidence, which is the same rule
/// [`crate::claims::attach_symbols`] is built on and for the same reason: a
/// `pub fn` three lines above an edit is not a public API change.
///
/// The walk itself is [`crate::claims::for_each_moved_line`], so this reader
/// and the symbol index cannot drift apart about headers, moved lines, or the
/// read bound ([`crate::claims::MAX_DIFF_BYTES`]).
pub fn scan(changed: &[ChangedFile], diff: &str) -> Vec<FileScan> {
    let mut scans: Vec<FileScan> = changed
        .iter()
        .map(|f| FileScan {
            path: f.path.clone(),
            kind: Some(classify(&f.path)),
            ..Default::default()
        })
        .collect();
    let paths: Vec<String> = scans.iter().map(|s| s.path.clone()).collect();
    // Deduplicated per file as we go: a config key repeats on every line that
    // touches it, and the chip counts the key once.
    let mut keys: std::collections::HashSet<(usize, String)> = Default::default();
    crate::claims::for_each_moved_line(diff, &paths, &mut |line| {
        let crate::claims::MovedLine {
            file: ix,
            added,
            body,
        } = line;
        let kind = scans[ix].kind();
        if is_test_decl(kind, body) {
            if added {
                scans[ix].tests_added += 1;
            } else {
                scans[ix].tests_removed += 1;
            }
        }
        if is_public_decl(kind, body) {
            scans[ix].api += 1;
        }
        if kind == FileKind::Schema || is_schema_statement(body) {
            scans[ix].schema += 1;
        }
        if added
            && kind == FileKind::Config
            && let Some(key) = config_key(&scans[ix].path, body)
            && keys.insert((ix, key.to_string()))
        {
            scans[ix].config_keys += 1;
        }
    });
    scans
}

/// Does this line declare a test?
///
/// One rule per language the board reads, and nothing at all for the rest —
/// which under-counts tests in a Ruby or Java branch, and does so equally in
/// both trees, so the before/after pair stays comparable even where the total
/// is low.
pub fn is_test_decl(kind: FileKind, line: &str) -> bool {
    let t = line.trim_start();
    match kind {
        // `#[test]`, `#[tokio::test]`, `#[rstest]` — the attribute's last path
        // segment, so `#[cfg(test)]` (a module, not a test) is not one.
        FileKind::Rust => t
            .strip_prefix("#[")
            .map(|rest| rest.split(['(', ']', ',']).next().unwrap_or(""))
            .and_then(|attr| attr.rsplit("::").next())
            .is_some_and(|last| matches!(last.trim(), "test" | "rstest" | "test_case")),
        FileKind::TypeScript => ["it(", "test(", "it.only(", "test.only(", "it.each("]
            .iter()
            .any(|p| t.starts_with(p)),
        FileKind::Python => t.starts_with("def test_") || t.starts_with("async def test_"),
        FileKind::Go => t.starts_with("func Test"),
        FileKind::Swift => t.starts_with("func test"),
        _ => false,
    }
}

/// Does this line declare something public?
///
/// `pub(crate)` is deliberately not public: a review of a workspace crate cares
/// about what left the crate, and a visibility widened only inside it is not
/// the thing the chip is about.
pub fn is_public_decl(kind: FileKind, line: &str) -> bool {
    let t = line.trim_start();
    match kind {
        // `pub `, and so not `pub(crate)` — the paren is what tells them apart.
        FileKind::Rust => t.starts_with("pub "),
        FileKind::TypeScript => t.starts_with("export "),
        // Column zero, because a nested `def` is not module surface — and no
        // leading underscore, which is Python's whole convention for private.
        FileKind::Python => {
            (line.starts_with("def ")
                || line.starts_with("class ")
                || line.starts_with("async def "))
                && !exported_name(line).starts_with('_')
        }
        FileKind::Go => {
            ["func ", "type ", "var ", "const "]
                .iter()
                .any(|p| line.starts_with(p))
                && exported_name(line)
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_uppercase())
        }
        FileKind::Swift => t.starts_with("public ") || t.starts_with("open "),
        _ => false,
    }
}

/// The name a declaration line declares: what follows the keyword, stripped of
/// whatever punctuation opens after it.
///
/// `def test_x(self)` → `test_x`. `func Handle(w http.ResponseWriter)` →
/// `Handle`. `func (r *Repo) Save() error` → `Save`, because a Go method's
/// receiver sits between the keyword and the name and skipping it is the
/// difference between reading an exported method and missing every one of them.
fn exported_name(line: &str) -> &str {
    let mut rest = line.trim_start();
    if let Some(after) = rest.strip_prefix("async ") {
        rest = after.trim_start();
    }
    rest = rest
        .split_once(char::is_whitespace)
        .map(|(_, r)| r)
        .unwrap_or("")
        .trim_start();
    if rest.starts_with('(') {
        rest = rest
            .split_once(')')
            .map(|(_, r)| r)
            .unwrap_or("")
            .trim_start();
    }
    rest.split(['(', '[', '<', '{', ':', '=', ' ', '*', ','])
        .next()
        .unwrap_or(rest)
}

/// Statements that change a database's shape, wherever they are written —
/// including inside the source of a program that ships its own migrations,
/// which is how this repo's own schema moves ([`crate::db`]).
pub const DDL: &[&str] = &[
    "CREATE TABLE",
    "ALTER TABLE",
    "DROP TABLE",
    "ADD COLUMN",
    "DROP COLUMN",
    "RENAME COLUMN",
    "CREATE INDEX",
    "CREATE UNIQUE INDEX",
    "DROP INDEX",
    "CREATE VIEW",
    "CREATE TRIGGER",
];

/// Is this changed line a schema statement? Case-insensitive on the keyword,
/// which is the only part of SQL anybody spells consistently.
pub fn is_schema_statement(line: &str) -> bool {
    let upper = line.to_ascii_uppercase();
    DDL.iter().any(|stmt| upper.contains(stmt))
}

/// The config key an added line introduces, if it introduces one.
///
/// Four spellings, chosen by the file's extension rather than sniffed: `key =`,
/// `"key":`, `key:`, `KEY=`. A table header (`[section]`) is not a key, and
/// neither is a comment.
pub fn config_key<'a>(path: &str, line: &'a str) -> Option<&'a str> {
    let t = line.trim();
    if t.is_empty() || t.starts_with('#') || t.starts_with("//") || t.starts_with('[') {
        return None;
    }
    let ext = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    let (key, _) = match ext.as_str() {
        "json" => t.strip_prefix('"')?.split_once("\":")?,
        "toml" | "ini" | "cfg" | "conf" | "properties" => t.split_once('=')?,
        "yaml" | "yml" => t.trim_start_matches("- ").split_once(':')?,
        _ => t.split_once('=')?,
    };
    let key = key.trim().trim_matches('"');
    let named = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    named.then_some(key)
}

// ---- dependencies --------------------------------------------------------

/// Is this path a manifest whose dependency list the board can read?
///
/// Lockfiles are excluded on purpose. A lockfile moves on every transitive
/// bump, and the chip is about a dependency somebody *chose* — which is what a
/// manifest records and what a reviewer can act on.
pub fn is_manifest(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    matches!(
        name.as_str(),
        "cargo.toml" | "package.json" | "pyproject.toml" | "go.mod"
    ) || (name.starts_with("requirements") && name.ends_with(".txt"))
}

/// Every dependency one manifest names, or `None` if this is not a manifest
/// shape the board reads — which is the answer that makes a chip say "unknown"
/// rather than "none".
///
/// Empty content is `Some(vec![])`, not `None`: a manifest that did not exist
/// before this branch named no dependencies, and every entry in the new one is
/// therefore new.
pub fn manifest_deps(path: &str, content: &str) -> Option<Vec<String>> {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if content.trim().is_empty() {
        return is_manifest(path).then(Vec::new);
    }
    match name.as_str() {
        "cargo.toml" => cargo_deps(content),
        "pyproject.toml" => pyproject_deps(content),
        "package.json" => package_json_deps(content),
        "go.mod" => Some(go_mod_deps(content)),
        _ if name.starts_with("requirements") && name.ends_with(".txt") => {
            Some(requirements_deps(content))
        }
        _ => None,
    }
}

/// The `[dependencies]`, `[dev-dependencies]` and `[build-dependencies]` of a
/// Cargo manifest, including the `[target.'cfg(…)'.dependencies]` tables and
/// the workspace's own list.
fn cargo_deps(content: &str) -> Option<Vec<String>> {
    let value: toml::Value = toml::from_str(content).ok()?;
    let mut out = Vec::new();
    let sections = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut collect = |table: Option<&toml::Value>| {
        if let Some(toml::Value::Table(t)) = table {
            out.extend(t.keys().cloned());
        }
    };
    for section in sections {
        collect(value.get(section));
        collect(value.get("workspace").and_then(|w| w.get(section)));
    }
    if let Some(toml::Value::Table(targets)) = value.get("target") {
        for target in targets.values() {
            for section in sections {
                collect(target.get(section));
            }
        }
    }
    Some(sorted(out))
}

/// PEP 621's `project.dependencies` array and its optional groups, plus
/// Poetry's table — the two spellings that between them cover what people
/// actually write.
fn pyproject_deps(content: &str) -> Option<Vec<String>> {
    let value: toml::Value = toml::from_str(content).ok()?;
    let mut out = Vec::new();
    let mut requirements = |v: Option<&toml::Value>| {
        if let Some(toml::Value::Array(items)) = v {
            out.extend(
                items
                    .iter()
                    .filter_map(|i| i.as_str())
                    .map(requirement_name)
                    .map(str::to_string),
            );
        }
    };
    requirements(value.get("project").and_then(|p| p.get("dependencies")));
    if let Some(toml::Value::Table(groups)) = value
        .get("project")
        .and_then(|p| p.get("optional-dependencies"))
    {
        for group in groups.values() {
            requirements(Some(group));
        }
    }
    if let Some(toml::Value::Table(t)) = value
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
    {
        out.extend(t.keys().cloned());
    }
    Some(sorted(out))
}

fn package_json_deps(content: &str) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    let mut out = Vec::new();
    for section in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        if let Some(serde_json::Value::Object(map)) = value.get(section) {
            out.extend(map.keys().cloned());
        }
    }
    Some(sorted(out))
}

/// `require` lines, block form and single form. The module path only — a
/// version bump is not a new dependency.
fn go_mod_deps(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in content.lines() {
        let t = line.trim();
        if t.starts_with("require (") {
            in_block = true;
            continue;
        }
        if in_block {
            if t == ")" {
                in_block = false;
            } else if let Some(module) = t.split_whitespace().next()
                && !module.starts_with("//")
            {
                out.push(module.to_string());
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("require ")
            && let Some(module) = rest.split_whitespace().next()
        {
            out.push(module.to_string());
        }
    }
    sorted(out)
}

fn requirements_deps(content: &str) -> Vec<String> {
    sorted(
        content
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with('-'))
            .map(requirement_name)
            .map(str::to_string)
            .collect(),
    )
}

/// The name out of a PEP 508 requirement: `httpx[cli]>=0.27` → `httpx`.
fn requirement_name(requirement: &str) -> &str {
    requirement
        .split([' ', '[', '=', '<', '>', '!', '~', ';', ','])
        .next()
        .unwrap_or(requirement)
        .trim()
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names.dedup();
    names
}

/// Dependencies the branch added to one manifest: what the new file names and
/// the old one did not.
///
/// A removal is not reported. The chip exists because arriving code nobody
/// mentioned is a thing to look at; a dependency that *left* is a claim the
/// remainder already checks, in the file it left from.
pub fn deps_added(path: &str, before: &str, after: &str) -> Option<Vec<String>> {
    let before = manifest_deps(path, before)?;
    let after = manifest_deps(path, after)?;
    Some(
        after
            .into_iter()
            .filter(|dep| !before.contains(dep))
            .collect(),
    )
}

// ---- call sites ----------------------------------------------------------

/// How a symbol anchor's reach changed: lines naming it now, and lines naming
/// it before.
///
/// Counted the same way in both trees, so the *pair* is the fact even where the
/// count is approximate. A "call site" here is a line naming the symbol that
/// does not also declare it — which over-counts a doc comment mentioning the
/// name and under-counts nothing, and is the reading a reviewer wants when a
/// claim says a function's callers moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallSites {
    pub symbol: String,
    pub now: u32,
    /// The count in the base tree, or `None` when the board could not read it
    /// — which is not zero, and must never render as "new".
    pub before: Option<u32>,
}

/// Count call sites in one `git grep -h -w -F` output.
///
/// Pure, and takes the grep output rather than running it, so the counting rule
/// is testable without a repo — the same split [`crate::claims::remainder`]
/// makes for the remainder.
pub fn count_call_sites(symbol: &str, grep: &str) -> u32 {
    grep.lines()
        .filter(|line| !is_declaration(symbol, line))
        .count() as u32
}

/// Keywords that mean the line in front of them *is* the thing, rather than
/// using it.
const DECLARES: &[&str] = &[
    "fn",
    "func",
    "def",
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "type",
    "const",
    "static",
    "let",
    "var",
    "impl",
    "mod",
    "package",
    "macro_rules!",
];

/// Does this line declare the symbol rather than call it?
pub fn is_declaration(symbol: &str, line: &str) -> bool {
    let words: Vec<&str> = line
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '!')))
        .filter(|w| !w.is_empty())
        .collect();
    words
        .windows(2)
        .any(|pair| pair[1] == symbol && DECLARES.contains(&pair[0]))
}

// ---- the effects ---------------------------------------------------------

/// One of the four "did this move" questions, answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Surface {
    Unchanged,
    Changed {
        count: u32,
    },
    /// The board could not answer, and why — a file in a language it has no
    /// rule for. Named, because "unknown" without a reason tells a reviewer to
    /// go and find out without saying where.
    Unknown {
        reason: String,
    },
}

impl Surface {
    fn ground(&self) -> Ground {
        match self {
            Surface::Unchanged => Ground::Neutral,
            Surface::Changed { .. } => Ground::Working,
            Surface::Unknown { .. } => Ground::Unknown,
        }
    }
}

/// Whether the tests were run, and how it went — off the run journal, never off
/// a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestRun {
    /// Every test command that ran passed at least once.
    Passing,
    /// A test command ran and never once passed.
    Failing(u32),
    /// No test command ran at all. Not good news, and not bad news either: it
    /// is the absence of the evidence the rest of this row is built on.
    NeverRun,
}

impl TestRun {
    /// What a chip says after the counts.
    fn phrase(self) -> &'static str {
        match self {
            TestRun::Passing => "all passing",
            TestRun::Failing(_) => "not passing",
            TestRun::NeverRun => "never run",
        }
    }

    fn ground(self) -> Ground {
        match self {
            TestRun::Passing => Ground::Neutral,
            TestRun::Failing(_) | TestRun::NeverRun => Ground::Working,
        }
    }
}

/// Read the run journal for what happened to the tests.
pub fn test_run(evidence: &RunEvidence) -> TestRun {
    let tests: Vec<_> = evidence
        .checks
        .iter()
        .filter(|c| crate::evidence::runs_tests(&c.command))
        .collect();
    if tests.is_empty() {
        return TestRun::NeverRun;
    }
    let failing = tests.iter().filter(|c| !c.ever_passed()).count() as u32;
    if failing > 0 {
        TestRun::Failing(failing)
    } else {
        TestRun::Passing
    }
}

/// Everything the board derived from the branch itself (§gh#236).
///
/// [`Effects::read`] is the field that keeps this honest across a restart: the
/// default value of this struct is what an attempt from before this existed
/// deserializes to, and a default that rendered "Public API unchanged" would be
/// the board asserting something it never checked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effects {
    /// Did the board actually read this branch? `false` on every attempt whose
    /// checkout was gone before anything looked, and on every row recorded
    /// before this module existed.
    #[serde(default)]
    pub read: bool,
    /// One per changed file, in the changed set's order.
    #[serde(default)]
    pub files: Vec<FileScan>,
    /// Test declarations in the whole tree, after the branch and before it.
    /// `None` when the count could not be taken — never guessed from the delta,
    /// which would be a total the board did not measure.
    #[serde(default)]
    pub tests_after: Option<u32>,
    #[serde(default)]
    pub tests_before: Option<u32>,
    /// Dependencies the branch's manifests gained.
    #[serde(default)]
    pub deps_added: Vec<String>,
    /// Whether every changed manifest could be read on both sides. `false`
    /// makes the chip say unknown, so a manifest the board choked on cannot
    /// render as "no dependencies added".
    #[serde(default)]
    pub deps_known: bool,
}

impl Effects {
    /// The scan for one changed path.
    pub fn file(&self, path: &str) -> Option<&FileScan> {
        self.files.iter().find(|f| f.path == path)
    }

    /// Did the public surface move? [`Surface::Unknown`] the moment one changed
    /// file is code the board has no rule for — a surface answer that ignored
    /// the file it could not read would be worse than no answer.
    pub fn public_api(&self) -> Surface {
        let unread: Vec<&FileScan> = self
            .files
            .iter()
            .filter(|f| f.kind().may_hold_api() && !f.kind().reads_api())
            .collect();
        let count: u32 = self.files.iter().map(|f| f.api).sum();
        if count > 0 {
            return Surface::Changed { count };
        }
        match unread.first() {
            Some(first) => Surface::Unknown {
                reason: match unread.len() {
                    1 => format!("{} is not a language the board reads", first.path),
                    n => format!(
                        "{n} changed files are in languages the board does not read, \
                         starting with {}",
                        first.path
                    ),
                },
            },
            None => Surface::Unchanged,
        }
    }

    /// Did the schema move? Never unknown: the rule is a list of statements and
    /// a list of paths, and both are checked over every changed file whatever
    /// language it is in.
    pub fn schema(&self) -> Surface {
        let count: u32 = self.files.iter().map(|f| f.schema).sum();
        if count > 0 {
            Surface::Changed { count }
        } else {
            Surface::Unchanged
        }
    }

    /// Config keys the branch added.
    pub fn config_keys(&self) -> Surface {
        let count: u32 = self.files.iter().map(|f| f.config_keys).sum();
        if count > 0 {
            Surface::Changed { count }
        } else {
            Surface::Unchanged
        }
    }

    /// The effects row: one chip per question, in the order a reviewer asks
    /// them — what was verified first, then the three surfaces that should not
    /// have moved, then the one thing that arrived.
    ///
    /// A review the board never read gets one chip saying exactly that, rather
    /// than five chips of reassurance nobody earned.
    pub fn chips(&self, evidence: &RunEvidence) -> Vec<Chip> {
        if !self.read {
            return vec![Chip::new(
                "no effects read from this branch",
                Ground::Unknown,
            )];
        }
        let run = test_run(evidence);
        let mut out = vec![self.tests_chip(run)];
        let api = self.public_api();
        out.push(Chip::new(
            match &api {
                Surface::Unchanged => "Public API unchanged".to_string(),
                Surface::Changed { count } => {
                    format!("{} changed", plural(*count, "public item", "public items"))
                }
                Surface::Unknown { .. } => "Public API unknown".to_string(),
            },
            api.ground(),
        ));
        let schema = self.schema();
        out.push(Chip::new(
            match &schema {
                Surface::Changed { count } => {
                    format!("{} changed", plural(*count, "schema line", "schema lines"))
                }
                _ => "Schema unchanged".to_string(),
            },
            schema.ground(),
        ));
        let config = self.config_keys();
        out.push(Chip::new(
            match &config {
                Surface::Changed { count } => {
                    format!("{} added", plural(*count, "config key", "config keys"))
                }
                _ => "No config keys added".to_string(),
            },
            config.ground(),
        ));
        out.push(match (self.deps_known, self.deps_added.len()) {
            (false, _) => Chip::new("Dependencies unknown", Ground::Unknown),
            (true, 0) => Chip::new("No dependencies added", Ground::Neutral),
            (true, n) => Chip::new(
                format!("{} added", plural(n as u32, "dependency", "dependencies")),
                Ground::Working,
            ),
        });
        out
    }

    /// `Tests 41 → 47, all passing` — the two counts from the two trees, the
    /// verdict from the journal.
    ///
    /// The counts and the run are separate facts and the chip says both,
    /// because either alone is misleading: six new tests nobody ran is not
    /// evidence, and a passing suite that did not grow is not evidence *about
    /// this branch*.
    fn tests_chip(&self, run: TestRun) -> Chip {
        let added: u32 = self.files.iter().map(|f| f.tests_added).sum();
        let ground = match (self.tests_before, self.tests_after) {
            // A suite that got smaller is a thing to look at whatever the run
            // said: every test still passing is exactly what deleting the
            // failing one looks like from here.
            (Some(before), Some(after)) if after < before => Ground::Working,
            _ => run.ground(),
        };
        match (self.tests_before, self.tests_after) {
            (Some(before), Some(after)) => Chip::new(
                format!("Tests {before} → {after}, {}", run.phrase()),
                ground,
            ),
            _ if added > 0 => Chip::new(
                format!("{} added, {}", plural(added, "test", "tests"), run.phrase()),
                ground,
            ),
            _ => Chip::new(format!("Tests {}", run.phrase()), ground),
        }
    }
}

/// `1 dependency` / `2 dependencies` — the same spelling everywhere, like
/// [`crate::claims`]'s own counter.
fn plural(n: u32, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else {
        format!("{n} {plural}")
    }
}

// ---- the chips under one claim -------------------------------------------

/// The evidence that attaches to one claim: what the board found in the files
/// that claim's own anchors reached.
///
/// A claim with nothing here is not an accusation — plenty of correct work has
/// no test — but it is the difference between a sentence and a checked
/// sentence, and this screen exists to keep those two apart.
pub fn claim_chips(
    matched: &[String],
    call_sites: &[CallSites],
    effects: &Effects,
    run: TestRun,
) -> Vec<Chip> {
    let mut out = Vec::new();
    let added: u32 = matched
        .iter()
        .filter_map(|path| effects.file(path))
        .map(|f| f.tests_added)
        .sum();
    if added > 0 {
        out.push(match run {
            TestRun::Passing => Chip::new(
                format!(
                    "{} {}",
                    plural(added, "new test", "new tests"),
                    if added == 1 { "passes" } else { "pass" }
                ),
                Ground::Settled,
            ),
            TestRun::Failing(_) => Chip::new(
                format!("{}, not passing", plural(added, "new test", "new tests")),
                Ground::Working,
            ),
            TestRun::NeverRun => Chip::new(
                format!("{}, never run", plural(added, "new test", "new tests")),
                Ground::Working,
            ),
        });
    }
    for sites in call_sites {
        out.push(Chip::new(
            match sites.before {
                Some(before) if before == sites.now => format!(
                    "{}, unchanged",
                    plural(sites.now, "call site", "call sites")
                ),
                Some(0) => format!("{}, new", plural(sites.now, "call site", "call sites")),
                Some(before) => format!(
                    "{}, was {before}",
                    plural(sites.now, "call site", "call sites")
                ),
                None => plural(sites.now, "call site", "call sites"),
            },
            Ground::Settled,
        ));
    }
    // The sentence the design asks for by name, and the reason the glyph drops
    // from `✓` to `·`: no test in this branch reaches anything this claim is
    // anchored to. Said whenever it is true, even beside a call-site chip —
    // "somebody calls it" and "something checks it" are not the same news.
    if added == 0 && effects.read && !matched.is_empty() {
        out.push(Chip::new("no test covers this", Ground::Working));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{Check, RanCommand};

    fn changed(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            status: "M".into(),
            added: 1,
            removed: 0,
            binary: false,
            symbols: Vec::new(),
        }
    }

    fn diff_of(path: &str, lines: &[&str]) -> String {
        let mut out = format!("diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@\n");
        for line in lines {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    fn passing() -> RunEvidence {
        crate::evidence::gather(&[RanCommand {
            command: "cargo test -p comet-board".into(),
            failed: false,
        }])
    }

    /// The whole point of the row: the answer comes off the diff, and a file
    /// the board cannot read makes the answer *unknown* rather than reassuring.
    #[test]
    fn a_language_the_board_cannot_read_makes_the_public_surface_unknown() {
        let mixed = [changed("crates/board/src/db.rs"), changed("lib/thing.rb")];
        let effects = Effects {
            read: true,
            files: scan(
                &mixed,
                &diff_of("crates/board/src/db.rs", &["+    let x = 1;"]),
            ),
            deps_known: true,
            ..Default::default()
        };
        assert!(matches!(effects.public_api(), Surface::Unknown { .. }));
        // …and the same diff without the unreadable file is an honest
        // "unchanged", or the chip would be useless.
        let effects = Effects {
            files: scan(
                &[changed("crates/board/src/db.rs")],
                &diff_of("crates/board/src/db.rs", &["+    let x = 1;"]),
            ),
            ..effects
        };
        assert_eq!(effects.public_api(), Surface::Unchanged);
    }

    #[test]
    fn a_public_declaration_that_moved_is_counted_and_a_crate_private_one_is_not() {
        let files = scan(
            &[changed("src/lib.rs")],
            &diff_of(
                "src/lib.rs",
                &[
                    "+pub fn open() {}",
                    "-pub struct Db;",
                    "+pub(crate) fn hidden() {}",
                    "+    let quiet = 1;",
                ],
            ),
        );
        assert_eq!(files[0].api, 2);
    }

    /// Prose is not evidence and neither is context, so the scan reads a `-U0`
    /// diff and nothing else — but a markdown file full of `pub fn` in a code
    /// fence must not count as an API change either.
    #[test]
    fn prose_never_counts_as_a_public_api_change() {
        let files = scan(
            &[changed("docs/BOARD.md")],
            &diff_of("docs/BOARD.md", &["+pub fn open() {}"]),
        );
        assert_eq!(files[0].api, 0);
        assert_eq!(files[0].kind, Some(FileKind::Prose));
    }

    #[test]
    fn a_test_attribute_is_a_test_and_a_cfg_test_module_is_not() {
        assert!(is_test_decl(FileKind::Rust, "    #[test]"));
        assert!(is_test_decl(FileKind::Rust, "#[tokio::test]"));
        assert!(is_test_decl(FileKind::Rust, "#[test_case(1, 2)]"));
        assert!(!is_test_decl(FileKind::Rust, "#[cfg(test)]"));
        assert!(!is_test_decl(FileKind::Rust, "fn test_thing() {}"));
        assert!(is_test_decl(FileKind::Go, "func TestOpen(t *testing.T) {"));
        assert!(is_test_decl(FileKind::Python, "def test_open():"));
        assert!(is_test_decl(FileKind::TypeScript, "  it('opens', () => {"));
        // A language with no rule under-counts rather than guessing.
        assert!(!is_test_decl(FileKind::OtherCode, "  it 'opens' do"));
    }

    #[test]
    fn schema_ddl_is_found_wherever_it_is_written() {
        let files = scan(
            &[changed("crates/board/src/db.rs"), changed("README.md")],
            &format!(
                "{}{}",
                diff_of(
                    "crates/board/src/db.rs",
                    &["+        ALTER TABLE attempts ADD COLUMN effects TEXT"],
                ),
                diff_of("README.md", &["+ nothing to see"]),
            ),
        );
        assert_eq!(files[0].schema, 1);
        assert_eq!(files[1].schema, 0);
        let effects = Effects {
            read: true,
            files,
            ..Default::default()
        };
        assert_eq!(effects.schema(), Surface::Changed { count: 1 });
    }

    #[test]
    fn config_keys_are_counted_once_each_and_only_where_they_were_added() {
        let files = scan(
            &[changed("app/settings.toml")],
            &diff_of(
                "app/settings.toml",
                &[
                    "+[sync]",
                    "+interval = \"5m\"",
                    "+interval = \"5m\"",
                    "-retired = true",
                    "+# a comment = not a key",
                ],
            ),
        );
        assert_eq!(files[0].config_keys, 1);
    }

    /// A manifest is read as a manifest, before it is read as config: a Cargo
    /// bump is a dependency question and not a settings question.
    #[test]
    fn a_dependency_the_branch_added_is_named_and_a_version_bump_is_not() {
        let before = r#"
[package]
name = "x"

[dependencies]
serde = "1"

[dev-dependencies]
tempfile = "3"
"#;
        let after = r#"
[package]
name = "x"

[dependencies]
serde = "1.2"
toml = "1.1"

[dev-dependencies]
tempfile = "3"
"#;
        let added = deps_added("crates/board/Cargo.toml", before, after).expect("a Cargo manifest");
        assert_eq!(added, vec!["toml".to_string()]);
        assert_eq!(classify("crates/board/Cargo.toml"), FileKind::Manifest);
    }

    #[test]
    fn every_manifest_shape_the_board_claims_to_read_actually_parses() {
        assert_eq!(
            manifest_deps("package.json", r#"{"dependencies":{"react":"19"}}"#),
            Some(vec!["react".to_string()])
        );
        assert_eq!(
            manifest_deps(
                "go.mod",
                "module x\n\nrequire (\n\tgithub.com/a/b v1.2.3\n)\n"
            ),
            Some(vec!["github.com/a/b".to_string()])
        );
        assert_eq!(
            manifest_deps("requirements.txt", "httpx[cli]>=0.27\n# comment\n"),
            Some(vec!["httpx".to_string()])
        );
        assert_eq!(
            manifest_deps(
                "pyproject.toml",
                "[project]\ndependencies = [\"httpx>=0.27\"]\n"
            ),
            Some(vec!["httpx".to_string()])
        );
        // A manifest that will not parse is unknown, never empty.
        assert_eq!(manifest_deps("Cargo.toml", "[[[nope"), None);
        assert_eq!(manifest_deps("src/main.rs", "fn main() {}"), None);
    }

    /// The chip a reviewer looks at first, and the two states it must never
    /// confuse: a suite that grew and passed, and a suite nobody ran.
    #[test]
    fn the_tests_chip_says_both_counts_and_never_calls_an_unrun_suite_passing() {
        let effects = Effects {
            read: true,
            tests_before: Some(41),
            tests_after: Some(47),
            deps_known: true,
            ..Default::default()
        };
        let chips = effects.chips(&passing());
        assert_eq!(chips[0].text, "Tests 41 → 47, all passing");
        assert_eq!(chips[0].ground, Ground::Neutral);

        let quiet = effects.chips(&RunEvidence::default());
        assert_eq!(quiet[0].text, "Tests 41 → 47, never run");
        assert_eq!(quiet[0].ground, Ground::Working);

        let failing = crate::evidence::gather(&[RanCommand {
            command: "cargo test".into(),
            failed: true,
        }]);
        let failed = effects.chips(&failing);
        assert_eq!(failed[0].text, "Tests 41 → 47, not passing");
        assert_eq!(failed[0].ground, Ground::Working);

        // A suite that shrank is a thing to look at however green the run was:
        // every test passing is what deleting the failing one looks like.
        let smaller = Effects {
            tests_after: Some(39),
            ..effects.clone()
        };
        let chips = smaller.chips(&passing());
        assert_eq!(chips[0].text, "Tests 41 → 39, all passing");
        assert_eq!(chips[0].ground, Ground::Working);
    }

    /// Go puts a method's receiver between the keyword and the name, and a
    /// reader that stopped at the second word would call every exported method
    /// in a Go branch private — which is a miss in the one direction the public
    /// surface must not miss in.
    #[test]
    fn a_declarations_name_survives_a_go_receiver_and_a_python_async() {
        assert_eq!(exported_name("func (r *Repo) Save() error {"), "Save");
        assert_eq!(
            exported_name("func Handle(w http.ResponseWriter) {"),
            "Handle"
        );
        assert_eq!(exported_name("async def test_open(client):"), "test_open");
        assert_eq!(exported_name("class Router:"), "Router");
        assert!(is_public_decl(
            FileKind::Go,
            "func (r *Repo) Save() error {"
        ));
        assert!(!is_public_decl(
            FileKind::Go,
            "func (r *Repo) save() error {"
        ));
        assert!(!is_public_decl(FileKind::Python, "def _private():"));
    }

    /// A new dependency is the one chip on this row that is not neutral.
    #[test]
    fn the_row_is_quiet_except_where_something_arrived() {
        let effects = Effects {
            read: true,
            tests_before: Some(41),
            tests_after: Some(47),
            deps_added: vec!["toml".into()],
            deps_known: true,
            ..Default::default()
        };
        let chips = effects.chips(&passing());
        let texts: Vec<&str> = chips.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "Tests 41 → 47, all passing",
                "Public API unchanged",
                "Schema unchanged",
                "No config keys added",
                "1 dependency added",
            ]
        );
        let loud: Vec<&Chip> = chips
            .iter()
            .filter(|c| c.ground != Ground::Neutral)
            .collect();
        assert_eq!(loud.len(), 1);
        assert_eq!(loud[0].text, "1 dependency added");
    }

    /// The exit condition, as one assertion: a review nobody read says so, and
    /// cannot be mistaken for five clean results.
    #[test]
    fn an_unread_branch_says_so_rather_than_rendering_as_verified() {
        let chips = Effects::default().chips(&passing());
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].ground, Ground::Unknown);
        assert!(chips[0].text.contains("no effects read"));
        // …and a manifest the board could not read is unknown on its own chip,
        // rather than quietly becoming "no dependencies added".
        let unread_manifest = Effects {
            read: true,
            deps_known: false,
            ..Default::default()
        };
        let chips = unread_manifest.chips(&passing());
        assert!(
            chips
                .iter()
                .any(|c| c.text == "Dependencies unknown" && c.ground == Ground::Unknown)
        );
    }

    #[test]
    fn a_claim_with_a_new_test_behind_it_is_corroborated_and_one_without_says_so() {
        let touched = [changed("crates/board/src/claims.rs")];
        let effects = Effects {
            read: true,
            files: scan(
                &touched,
                &diff_of(
                    "crates/board/src/claims.rs",
                    &["+    #[test]", "+    fn a_symbol_anchor_matches() {}"],
                ),
            ),
            deps_known: true,
            ..Default::default()
        };
        let matched = vec!["crates/board/src/claims.rs".to_string()];
        let chips = claim_chips(&matched, &[], &effects, TestRun::Passing);
        assert_eq!(chips[0].text, "1 new test passes");
        assert!(corroborated(&chips));

        // The same claim, with a run nobody ever executed: the chip is still
        // there and it is no longer corroboration.
        let unrun = claim_chips(&matched, &[], &effects, TestRun::NeverRun);
        assert_eq!(unrun[0].text, "1 new test, never run");
        assert!(!corroborated(&unrun));
    }

    #[test]
    fn a_claim_no_test_reaches_says_so_and_loses_its_tick() {
        let touched = [changed("crates/ui/src/review.rs")];
        let effects = Effects {
            read: true,
            files: scan(
                &touched,
                &diff_of("crates/ui/src/review.rs", &["+    let chip = 1;"]),
            ),
            deps_known: true,
            ..Default::default()
        };
        let matched = vec!["crates/ui/src/review.rs".to_string()];
        let chips = claim_chips(&matched, &[], &effects, TestRun::Passing);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].text, "no test covers this");
        assert_eq!(chips[0].ground, Ground::Working);
        assert!(!corroborated(&chips));
    }

    #[test]
    fn call_sites_are_counted_in_both_trees_and_a_declaration_is_not_one() {
        let grep = "pub fn active_placements(&self) -> Vec<Slot> {\n\
                        let live = self.active_placements();\n\
                    // active_placements is the one that moved\n";
        assert_eq!(count_call_sites("active_placements", grep), 2);

        let sites = [CallSites {
            symbol: "active_placements".into(),
            now: 1,
            before: Some(2),
        }];
        let chips = claim_chips(&[], &sites, &Effects::default(), TestRun::Passing);
        assert_eq!(chips[0].text, "1 call site, was 2");
        assert_eq!(chips[0].ground, Ground::Settled);

        // A count the board could not take on the far side is a count, not a
        // "new".
        let unknown = [CallSites {
            symbol: "shelf_note".into(),
            now: 3,
            before: None,
        }];
        let chips = claim_chips(&[], &unknown, &Effects::default(), TestRun::Passing);
        assert_eq!(chips[0].text, "3 call sites");
    }

    #[test]
    fn a_manifest_and_a_migration_and_a_readme_are_told_apart_by_name() {
        assert_eq!(classify("Cargo.toml"), FileKind::Manifest);
        assert_eq!(classify("db/migrations/003_add.sql"), FileKind::Schema);
        assert_eq!(classify("prisma/schema.prisma"), FileKind::Schema);
        assert_eq!(classify("app/config.yaml"), FileKind::Config);
        assert_eq!(classify(".env.example"), FileKind::Config);
        assert_eq!(classify("README.md"), FileKind::Prose);
        assert_eq!(classify("crates/ui/src/review.rs"), FileKind::Rust);
        assert_eq!(classify("web/app/page.tsx"), FileKind::TypeScript);
        assert_eq!(classify("scripts/thing"), FileKind::Opaque);
    }

    /// The run journal is where "passing" comes from, and only the commands
    /// that actually run tests count: a green `cargo build` is not a test.
    #[test]
    fn only_a_test_command_answers_the_test_question() {
        let build = crate::evidence::gather(&[RanCommand {
            command: "cargo build".into(),
            failed: false,
        }]);
        assert_eq!(test_run(&build), TestRun::NeverRun);
        assert_eq!(test_run(&passing()), TestRun::Passing);
        let mixed = RunEvidence {
            checks: vec![
                Check {
                    command: "cargo test".into(),
                    runs: 3,
                    failed: 3,
                },
                Check {
                    command: "pytest -q".into(),
                    runs: 1,
                    failed: 0,
                },
            ],
            ..Default::default()
        };
        assert_eq!(test_run(&mixed), TestRun::Failing(1));
    }
}
