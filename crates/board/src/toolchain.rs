//! Toolchains — what an agent on this box can actually run (gh#561).
//!
//! A dispatched agent's PATH is not a login shell's. The engine stamps its own
//! directories in front ([`crate::git_credentials::agent_bin_dir`], the app
//! payload) and every harness child inherits whatever process PATH the engine
//! was launched with — which, for a GUI or systemd launch, is `/usr/bin:/bin`
//! and nothing else. A human standing in the same checkout has `node`, `npm`
//! and `cargo` because their shell shaped PATH; an agent has them only if
//! someone put the directories on the child's PATH on purpose.
//!
//! This module is the single source of truth for that "on purpose" list:
//! [`agent_tool_dirs`] names the directories every harness child gets appended
//! to its PATH, and the detection half answers which tools a routed repo's
//! dispatches need, so `doctor` can ask whether the two meet before an attempt
//! ships work it could not verify.

use std::path::{Path, PathBuf};

/// Directories put on every agent's PATH, after whatever it inherited.
///
/// Gap-fillers, not overrides: the harness appends these behind the child's
/// own PATH so they can never shadow something the engine (or its launcher)
/// already resolved, and dedupe keeps an inherited entry from appearing twice.
/// What belongs here is "the install locations a human shell ends up with but
/// a GUI/systemd launch does not":
///
/// - Homebrew on either macOS architecture, and `/usr/local/bin` for
///   hand-installed tools (`brew` itself warns when its prefix is unreachable
///   from a process's PATH);
/// - `~/.local/bin` and `~/.cargo/bin`, where pipx/rustup/cargo-installed
///   tools land;
/// - the Node version managers' bin dirs — fnm's stable `aliases/default`,
///   nvm's installed versions, volta/bun/pnpm shims.
///
/// Only directories that exist are returned: a PATH entry pointing nowhere is
/// harmless but noisy, and `doctor` reports the same list it computes against.
pub fn agent_tool_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    // System-wide tool prefixes. On macOS a GUI launch inherits Apple's
    // default PATH (/usr/bin:/bin:/usr/sbin:/sbin), which holds none of these;
    // on Linux /usr/local/bin usually rides along already and dedupe drops it.
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    if let Some(home) = home {
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".cargo/bin"));
        dirs.extend(node_version_manager_bins(home));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| d.is_dir() && seen.insert(d.clone()));
    dirs
}

/// Bin directories where npm-installed CLIs land under Node version managers,
/// keyed off `home` rather than the environment: the same enumeration the
/// harness uses to *resolve* its own CLIs, asked here about what an *agent's*
/// tools get. GUI launches never see these on PATH — the managers shape PATH
/// in shell init (fnm's per-shell multishells, nvm's shell function), which a
/// Dock/Finder-launched app never runs.
fn node_version_manager_bins(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    // fnm: `aliases/default` is a stable symlink to the active default
    // installation (the multishell PATH entries are ephemeral, per-shell).
    let mut fnm_roots: Vec<PathBuf> = std::env::var_os("FNM_DIR")
        .map(PathBuf::from)
        .into_iter()
        .collect();
    fnm_roots.push(home.join(".local/share/fnm"));
    fnm_roots.push(home.join("Library/Application Support/fnm"));
    fnm_roots.push(home.join(".fnm"));
    for root in fnm_roots {
        dirs.push(root.join("aliases/default/bin"));
    }
    // volta / bun keep real shims in a fixed bin dir; pnpm has a global bin.
    dirs.push(home.join(".volta/bin"));
    dirs.push(home.join(".bun/bin"));
    dirs.push(home.join("Library/pnpm"));
    dirs.push(home.join(".local/share/pnpm"));
    // nvm: every installed version's bin, newest first.
    let nvm = home.join(".nvm/versions/node");
    if let Ok(entries) = std::fs::read_dir(&nvm) {
        let mut versions: Vec<PathBuf> = entries.flatten().map(|e| e.path().join("bin")).collect();
        versions.sort();
        versions.reverse();
        dirs.append(&mut versions);
    }
    dirs
}

/// A JavaScript package directory a checkout declares: the dir holding its
/// `package.json`, and which lockfile pins it (which decides the install
/// command an agent should run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsPackage {
    /// Directory holding the `package.json`, relative to the checkout root as
    /// given ("", `edge`, `apps/web`).
    pub dir: String,
    /// The lockfile found beside it, most specific first.
    pub lockfile: Option<&'static str>,
}

impl JsPackage {
    /// The command that installs this package's dependencies.
    pub fn install_command(&self) -> &'static str {
        match self.lockfile {
            Some("package-lock.json") => "npm ci",
            Some("yarn.lock") => "yarn install",
            Some("pnpm-lock.yaml") => "pnpm install",
            _ => "npm install",
        }
    }

    /// How a brief or a doctor row names it: `edge/` for a subdirectory, the
    /// repo root as `./`.
    pub fn label(&self) -> String {
        if self.dir.is_empty() {
            "./".to_string()
        } else {
            format!("{}/", self.dir)
        }
    }
}

/// Directories a bounded search for package manifests must not descend into:
/// dependency trees (the thing being looked for), build output, VCS internals.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "vendor",
    ".venv",
    "__pycache__",
];

/// The JS packages a checkout declares, root first, shallowest then sorted.
///
/// Bounded on purpose: a fresh worktree scan happens inside dispatch, and a
/// monorepo can hold hundreds of manifests. Two levels below the root covers
/// the shapes real repos use (`edge/`, `apps/web`, `packages/ui`) without
/// walking a tree that never ends. Unreadable entries are skipped — a
/// permission error in one corner is not a reason a dispatch gets no note at
/// all.
pub fn js_packages(root: &Path) -> Vec<JsPackage> {
    let mut out = Vec::new();
    if root.join("package.json").is_file() {
        out.push(JsPackage {
            dir: String::new(),
            lockfile: lockfile_at(root),
        });
    }
    walk_packages(root, root, 0, &mut out);
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out
}

fn walk_packages(root: &Path, dir: &Path, depth: usize, out: &mut Vec<JsPackage>) {
    // Depth 2 = root's children and grandchildren; the root itself was checked
    // by the caller. Deeper nesting is a layout nobody ships.
    if depth >= 2 {
        return;
    }
    let mut children = subdirs(dir);
    children.sort();
    for child in children {
        if child.join("package.json").is_file() {
            out.push(JsPackage {
                dir: relative_dir(&child, root),
                lockfile: lockfile_at(&child),
            });
        }
        walk_packages(root, &child, depth + 1, out);
    }
}

fn relative_dir(child: &Path, root: &Path) -> String {
    child
        .strip_prefix(root)
        .unwrap_or(child)
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string()
}

fn lockfile_at(dir: &Path) -> Option<&'static str> {
    ["package-lock.json", "pnpm-lock.yaml", "yarn.lock"]
        .into_iter()
        .find(|f| dir.join(f).is_file())
}

/// The packages whose dependencies are not installed — the fresh-worktree
/// state that makes a first `npm test` fail obscurely (gh#561).
pub fn missing_js_packages(root: &Path) -> Vec<JsPackage> {
    js_packages(root)
        .into_iter()
        .filter(|p| !root.join(&p.dir).join("node_modules").is_dir())
        .collect()
}

/// Tools a routed repo needs on an agent's PATH, detected from markers.
///
/// Marker → tool, deliberately coarse — this exists so `doctor` can say "a
/// dispatch here will reach for `cargo` and cannot find it" hours before one
/// tries:
///
/// - `Cargo.toml` or `rust-toolchain.toml`, anywhere the package walk looks →
///   `cargo`;
/// - any `package.json` → `node` and `npm`.
///
/// Returned sorted and deduped across the whole repo, not per directory: the
/// question is what the checkout needs, not where.
pub fn repo_tools(root: &Path) -> Vec<&'static str> {
    let mut tools: Vec<&'static str> = Vec::new();
    if js_manifests(root) {
        tools.push("node");
        tools.push("npm");
    }
    if rust_markers(root) {
        tools.push("cargo");
    }
    tools.sort_unstable();
    tools.dedup();
    tools
}

fn js_manifests(root: &Path) -> bool {
    !js_packages(root).is_empty()
}

fn rust_markers(root: &Path) -> bool {
    if root.join("Cargo.toml").is_file() || root.join("rust-toolchain.toml").is_file() {
        return true;
    }
    walk_markers(root, 0, &mut |dir| {
        dir.join("Cargo.toml").is_file() || dir.join("rust-toolchain.toml").is_file()
    })
}

/// The immediate child directories a search may descend into.
fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| {
            !matches!(
                p.file_name().and_then(|n| n.to_str()),
                Some(s) if SKIP_DIRS.contains(&s)
            )
        })
        .collect()
}

/// The same bounded walk [`js_packages`] uses, for arbitrary markers.
fn walk_markers(dir: &Path, depth: usize, hit: &mut impl FnMut(&Path) -> bool) -> bool {
    if depth >= 2 {
        return false;
    }
    let mut children = subdirs(dir);
    children.sort();
    for child in children {
        if hit(&child) || walk_markers(&child, depth + 1, hit) {
            return true;
        }
    }
    false
}

/// Resolve `tool` over `dirs`: the first directory holding the executable.
pub fn find_tool(tool: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    };
    dirs.iter().map(|d| d.join(&name)).find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("comet-toolchain-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn tool_dirs_are_existing_gap_fillers_behind_the_inherited_path() {
        // Whatever else is true of this machine, the returned dirs all exist
        // and carry no duplicates — the properties appending code relies on.
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let dirs = agent_tool_dirs(home.as_deref());
        let mut seen = std::collections::HashSet::new();
        for d in &dirs {
            assert!(d.is_dir(), "{} does not exist", d.display());
            assert!(seen.insert(d.clone()), "{} listed twice", d.display());
        }
    }

    #[test]
    fn no_home_still_yields_the_system_prefixes() {
        let dirs = agent_tool_dirs(None);
        assert!(dirs.iter().any(|d| d == Path::new("/usr/local/bin")));
    }

    #[test]
    fn a_nested_package_without_node_modules_is_reported_with_its_lockfile() {
        let repo = scratch("missing");
        let edge = repo.join("edge");
        std::fs::create_dir_all(&edge).unwrap();
        std::fs::write(edge.join("package.json"), "{}").unwrap();
        std::fs::write(edge.join("package-lock.json"), "{}").unwrap();

        let missing = missing_js_packages(&repo);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].dir, "edge");
        assert_eq!(missing[0].lockfile, Some("package-lock.json"));
        assert_eq!(missing[0].install_command(), "npm ci");
        assert_eq!(missing[0].label(), "edge/");
    }

    #[test]
    fn an_installed_package_is_not_reported() {
        let repo = scratch("installed");
        let edge = repo.join("edge");
        std::fs::create_dir_all(edge.join("node_modules")).unwrap();
        std::fs::write(edge.join("package.json"), "{}").unwrap();
        assert!(missing_js_packages(&repo).is_empty());
        assert_eq!(js_packages(&repo).len(), 1);
    }

    #[test]
    fn dependency_trees_and_build_output_are_not_walked() {
        let repo = scratch("skip");
        let deep = repo.join("node_modules/pkg/sub/deeper");
        std::fs::create_dir_all(deep).unwrap();
        std::fs::write(repo.join("node_modules/pkg/sub/deeper/package.json"), "{}").unwrap();
        assert!(js_packages(&repo).is_empty());
    }

    #[test]
    fn lockfile_kinds_decide_the_install_command() {
        let repo = scratch("locks");
        for (dir, lock, want) in [
            ("a", "yarn.lock", "yarn install"),
            ("b", "pnpm-lock.yaml", "pnpm install"),
            ("c", "", "npm install"),
        ] {
            let d = repo.join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("package.json"), "{}").unwrap();
            if !lock.is_empty() {
                std::fs::write(d.join(lock), "").unwrap();
            }
            let found = js_packages(&repo)
                .into_iter()
                .find(|p| p.dir == dir)
                .unwrap();
            assert_eq!(found.install_command(), want, "{dir}");
            std::fs::remove_file(d.join("package.json")).unwrap();
            if !lock.is_empty() {
                std::fs::remove_file(d.join(lock)).unwrap();
            }
        }
    }

    #[test]
    fn markers_decide_which_tools_a_repo_needs() {
        let repo = scratch("tools");
        let edge = repo.join("edge");
        std::fs::create_dir_all(&edge).unwrap();
        std::fs::write(edge.join("package.json"), "{}").unwrap();
        std::fs::write(repo.join("Cargo.toml"), "[workspace]").unwrap();
        let mut tools = repo_tools(&repo);
        tools.sort_unstable();
        assert_eq!(tools, vec!["cargo", "node", "npm"]);
    }

    #[test]
    fn a_repo_with_no_known_markers_needs_nothing() {
        let repo = scratch("plain");
        std::fs::write(repo.join("README.md"), "hi").unwrap();
        assert!(repo_tools(&repo).is_empty());
    }

    #[test]
    fn find_tool_prefers_the_first_directory_that_holds_it() {
        let base = scratch("find");
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        #[cfg(unix)]
        {
            std::fs::write(b.join("tool"), "#!/bin/sh\n").unwrap();
            let found = find_tool("tool", &[a.clone(), b.clone()]).expect("found in b");
            assert_eq!(found, b.join("tool"));
        }
        assert_eq!(find_tool("absent", &[a, b]), None);
    }
}
