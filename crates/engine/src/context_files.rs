//! The `@` picker's other half: searching the chat's own checkout, and
//! resolving what was picked against it at execute time (gh#424).
//!
//! **Which checkout.** The one the chat runs in — its `cwd`, which is the space
//! folder, an existing worktree, or the worktree the board cut for an attempt.
//! Never this device's idea of the repo: the search is a forwardable RPC, so a
//! phone asking about a chat on the box gets the box's files, and a reference
//! picked there means the same thing when the turn runs there.
//!
//! **Bounded, not indexed.** No index, no watcher, no cache. A checkout can be a
//! million files and this runs per keystroke, possibly over a relay, so the walk
//! carries a budget ([`SearchLimits`]) and says when it hit it
//! ([`comet_proto::ContextSearch::truncated`]) rather than quietly showing the
//! first twenty things it found. An index would be the other answer, and a
//! worse one here: it would have to be per-worktree, invalidated by the agent
//! itself editing files, and paid for on every box whether or not anybody ever
//! typed `@`.
//!
//! **Containment is a check, not a convention.** [`resolve`] refuses a reference
//! that leaves the checkout — textually (`..`, absolute) and, when the path
//! exists, by canonicalizing it and comparing against the canonical root, which
//! is what catches a symlink pointing out of the tree. The picker cannot produce
//! one of these; anything that does is a client bug or a forged command, and the
//! host is the only place that can tell the difference between "inside" and
//! "outside" anyway, because it is the only place that holds the tree.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use comet_proto::view::context::rank;
use comet_proto::{ContextMatch, ContextRef, ContextRefKind, ContextSearch, ResolvedRef};
use sha2::{Digest, Sha256};

use crate::EngineError;

/// What one search may spend.
#[derive(Debug, Clone, Copy)]
pub struct SearchLimits {
    /// Rows returned at most.
    pub max_results: usize,
    /// Directory entries looked at at most, across the whole walk.
    pub max_entries: usize,
    /// Directory levels below the root at most.
    pub max_depth: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            // The picker shows seven rows and says how many more matched; more
            // than this crossing a relay per keystroke buys nothing anybody
            // reads.
            max_results: 20,
            // Enough to cross a large repo's source tree, small enough that the
            // worst case is milliseconds. Measured against this checkout: the
            // whole thing is well under it once the skip list is applied.
            max_entries: 20_000,
            // A path fifteen levels down is not something anybody is going to
            // find by typing at it.
            max_depth: 12,
        }
    }
}

/// Directory names never walked into.
///
/// A fixed list rather than a `.gitignore` parse, deliberately. Honoring
/// gitignore properly means the whole ignore stack — root file, per-directory
/// files, negations, the global excludes file — evaluated per entry, and getting
/// it 90% right would be worse than being plainly rule-based: a picker that
/// hides a file for reasons the user cannot see is a picker they stop trusting.
/// These are the directories whose contents are never what somebody means by
/// `@`, and the list is short enough to read.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".turbo",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".cargo",
    ".gradle",
    "Pods",
    "DerivedData",
];

/// Opaque identity for the host checkout used by search and execution.
///
/// Include the canonical root even when the workspace row already carries a
/// repository checkout id. That turns a stale row whose cwd was replaced into
/// a refusal too, while keeping the wire value equality-only.
pub fn checkout_identity(device_id: &str, root: &Path, declared: Option<&str>) -> String {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(b"comet-context-checkout-v1\0");
    hasher.update(device_id.as_bytes());
    hasher.update([0]);
    hasher.update(declared.unwrap_or_default().as_bytes());
    hasher.update([0]);
    hasher.update(canonical.to_string_lossy().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Search `root` for files and directories matching `query`.
///
/// Breadth-first, so a shallow file is reached before a deep one and the budget
/// is spent where the answer usually is. Deterministic: each directory's entries
/// are sorted before they are walked, because `read_dir` order is the
/// filesystem's and a picker that reshuffled between keystrokes would be
/// unusable.
///
/// Never fails. An unreadable directory contributes nothing, a root that does
/// not exist is an empty answer — a red box under the composer mid-keystroke
/// helps nobody, and "no matches" is the honest rendering of both.
pub fn search(root: &Path, query: &str, limits: SearchLimits) -> ContextSearch {
    let mut scanned = 0usize;
    let mut budget_hit = false;
    // Ranked candidates: (rank, path length, discovery order, match).
    let mut ranked: Vec<(usize, usize, usize, ContextMatch)> = Vec::new();
    let mut discovered = 0usize;

    let mut queue: VecDeque<(PathBuf, String, usize)> = VecDeque::new();
    queue.push_back((root.to_path_buf(), String::new(), 0));

    while let Some((dir, prefix, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut names: Vec<(String, bool)> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_string();
                let is_dir = entry.file_type().ok()?.is_dir();
                Some((name, is_dir))
            })
            .collect();
        names.sort();
        for (name, is_dir) in names {
            if scanned >= limits.max_entries {
                budget_hit = true;
                break;
            }
            scanned += 1;
            if is_dir && SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = match prefix.is_empty() {
                true => name.clone(),
                false => format!("{prefix}/{name}"),
            };
            let row = ContextMatch {
                path: path.clone(),
                kind: match is_dir {
                    true => ContextRefKind::Directory,
                    false => ContextRefKind::File,
                },
            };
            if let Some(r) = rank(query, &row) {
                ranked.push((r, row.path.len(), discovered, row));
                discovered += 1;
            }
            if is_dir && depth + 1 <= limits.max_depth {
                queue.push_back((dir.join(&name), path, depth + 1));
            }
        }
        if scanned >= limits.max_entries {
            budget_hit = true;
            break;
        }
    }

    // Same order the picker would compute (view::context::filter): rank, then
    // the shallower path, then discovery order. Ranking here as well as there is
    // not duplication — it is what makes the CAP meaningful, since a host that
    // truncated arbitrarily would drop the best match as readily as the worst.
    ranked.sort_by_key(|&(r, len, ix, _)| (r, len, ix));
    let truncated = budget_hit || ranked.len() > limits.max_results;
    ranked.truncate(limits.max_results);
    ContextSearch {
        matches: ranked.into_iter().map(|(_, _, _, row)| row).collect(),
        truncated,
        checkout_id: None,
    }
}

/// Resolve references against the checkout they were picked from.
///
/// `Err` only for a reference that escapes the checkout — the one condition that
/// is not a fact about the tree but a violation of the contract, and the one the
/// host must refuse rather than report. A missing file is reported
/// ([`ResolvedRef::exists`]) and never fatal: pointing at a file the agent is
/// about to create is a legitimate instruction.
pub fn resolve(root: &Path, refs: &[ContextRef]) -> Result<Vec<ResolvedRef>, EngineError> {
    if refs.is_empty() {
        return Ok(Vec::new());
    }
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut out = Vec::with_capacity(refs.len());
    for reference in refs {
        let normalized = comet_proto::context::normalize(&reference.path).ok_or_else(|| {
            EngineError::Other(format!(
                "context reference escapes the checkout: {}",
                reference.path
            ))
        })?;
        let absolute = canonical_root.join(&normalized);
        // Validate every existing ancestor, not only the full leaf. A missing
        // `link/new.rs` must still be refused when `link` is a symlink outside
        // the checkout.
        let mut checked = canonical_root.clone();
        for component in Path::new(&normalized).components() {
            checked.push(component.as_os_str());
            match std::fs::symlink_metadata(&checked) {
                Ok(_) => {
                    let real = std::fs::canonicalize(&checked).map_err(|err| {
                        EngineError::Other(format!("context reference {normalized}: {err}"))
                    })?;
                    if !real.starts_with(&canonical_root) {
                        return Err(EngineError::Other(format!(
                            "context reference escapes the checkout: {normalized}"
                        )));
                    }
                    checked = real;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => break,
                Err(err) => {
                    return Err(EngineError::Other(format!(
                        "context reference {normalized}: {err}"
                    )));
                }
            }
        }
        let exists = absolute.exists();
        if exists {
            // The textual check cannot see a symlink; this can.
            let real = std::fs::canonicalize(&absolute).map_err(|err| {
                EngineError::Other(format!("context reference {normalized}: {err}"))
            })?;
            if !real.starts_with(&canonical_root) {
                return Err(EngineError::Other(format!(
                    "context reference escapes the checkout: {normalized}"
                )));
            }
        }
        out.push(ResolvedRef {
            reference: ContextRef {
                path: normalized,
                kind: reference.kind,
                checkout_id: reference.checkout_id.clone(),
            },
            absolute: absolute.to_string_lossy().to_string(),
            exists,
        });
    }
    Ok(out)
}

/// Fence references to the exact checkout the host advertised when they were
/// picked. Legacy references without a stamp remain readable; a stamped one
/// never silently follows a chat that moved to another root/worktree.
pub fn validate_checkout(
    refs: &[ContextRef],
    current_checkout_id: Option<&str>,
) -> Result<(), EngineError> {
    if current_checkout_id.is_none()
        || refs.iter().any(|reference| {
            reference.checkout_id.as_deref().is_none_or(str::is_empty)
                || reference.checkout_id.as_deref() != current_checkout_id
        })
    {
        return Err(EngineError::Other(
            "context reference belongs to a different checkout".into(),
        ));
    }
    Ok(())
}

/// What the agent is told about references that are not there.
///
/// Appended to the prompt, and only when something is missing — the references
/// themselves are already in the text the user wrote (`@src/main.rs`, relative
/// to the cwd the run starts in, which is this same checkout root), so the happy
/// path adds nothing at all. What the agent cannot work out for itself is that a
/// path it was pointed at has since gone, and being told beats a failed read
/// three tool calls later.
pub fn missing_note(resolved: &[ResolvedRef]) -> Option<String> {
    let missing: Vec<&str> = resolved
        .iter()
        .filter(|r| !r.exists)
        .map(|r| r.reference.path.as_str())
        .collect();
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "\n\n[context: not in this checkout right now — {}]",
        missing.join(", ")
    ))
}

/// The one-line account of a command's references, for the ledger's
/// `resolution` field — which every client already renders, so this is how a
/// stale reference becomes visible on the device that queued it.
pub fn resolution_note(resolved: &[ResolvedRef]) -> Option<String> {
    if resolved.is_empty() {
        return None;
    }
    let missing = resolved.iter().filter(|r| !r.exists).count();
    Some(match missing {
        0 => format!("{} context ref(s)", resolved.len()),
        n => format!("{} context ref(s), {n} missing", resolved.len()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "comet-context-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("mkdir");
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, "x").expect("write");
    }

    fn checkout(tag: &str) -> TempDir {
        let tmp = TempDir::new(tag);
        write(tmp.path(), "README.md");
        write(tmp.path(), "src/main.rs");
        write(tmp.path(), "src/composer.rs");
        write(tmp.path(), "crates/ui/src/composer.rs");
        write(tmp.path(), "node_modules/junk/composer.rs");
        write(tmp.path(), ".git/objects/composer.rs");
        tmp
    }

    #[test]
    fn the_walk_stays_inside_the_checkout_and_skips_the_heavy_directories() {
        let tmp = checkout("walk");
        let found = search(tmp.path(), "composer", SearchLimits::default());
        let paths: Vec<&str> = found.matches.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(
            paths,
            ["src/composer.rs", "crates/ui/src/composer.rs"],
            "name matches, shallower first; node_modules and .git never walked"
        );
        assert!(!found.truncated);

        // Directories are offered too, and marked as such.
        let found = search(tmp.path(), "src", SearchLimits::default());
        let src = found
            .matches
            .iter()
            .find(|m| m.path == "src")
            .expect("the src directory itself");
        assert_eq!(src.kind, ContextRefKind::Directory);

        // An empty query browses; a query nothing matches is an empty answer,
        // never an error.
        assert!(
            !search(tmp.path(), "", SearchLimits::default())
                .matches
                .is_empty()
        );
        assert!(
            search(tmp.path(), "zzzzz", SearchLimits::default())
                .matches
                .is_empty()
        );
        assert!(
            search(
                Path::new("/nonexistent-checkout"),
                "x",
                SearchLimits::default()
            )
            .matches
            .is_empty()
        );
    }

    #[test]
    fn a_budgeted_walk_says_it_was_budgeted() {
        let tmp = checkout("budget");
        // A cap below the match count truncates, and says so.
        let found = search(
            tmp.path(),
            "composer",
            SearchLimits {
                max_results: 1,
                ..SearchLimits::default()
            },
        );
        assert_eq!(found.matches.len(), 1);
        assert!(
            found.truncated,
            "a capped list must not read as the whole list"
        );

        // A scan budget that runs out mid-walk truncates too.
        let found = search(
            tmp.path(),
            "composer",
            SearchLimits {
                max_entries: 2,
                ..SearchLimits::default()
            },
        );
        assert!(found.truncated);

        // Depth zero never descends: only the root's own entries.
        let found = search(
            tmp.path(),
            "",
            SearchLimits {
                max_depth: 0,
                ..SearchLimits::default()
            },
        );
        assert!(
            found.matches.iter().all(|m| !m.path.contains('/')),
            "depth 0 stays at the root: {:?}",
            found.matches
        );
    }

    #[test]
    fn resolution_reports_the_missing_and_refuses_the_escaping() {
        let tmp = checkout("resolve");
        let resolved = resolve(
            tmp.path(),
            &[
                ContextRef::file("src/main.rs"),
                ContextRef::file("src/gone.rs"),
                ContextRef::directory("crates/ui"),
            ],
        )
        .expect("inside the checkout");
        assert_eq!(
            resolved.iter().map(|r| r.exists).collect::<Vec<_>>(),
            [true, false, true]
        );
        assert!(resolved[0].absolute.ends_with("src/main.rs"));
        assert_eq!(
            missing_note(&resolved).as_deref(),
            Some("\n\n[context: not in this checkout right now — src/gone.rs]")
        );
        assert_eq!(
            resolution_note(&resolved).as_deref(),
            Some("3 context ref(s), 1 missing")
        );
        // All present: the agent is told nothing extra, the ledger still counts.
        let clean = resolve(tmp.path(), &[ContextRef::file("README.md")]).expect("present");
        assert_eq!(missing_note(&clean), None);
        assert_eq!(resolution_note(&clean).as_deref(), Some("1 context ref(s)"));
        assert_eq!(resolution_note(&[]), None);

        // Escapes: refused, not resolved away.
        for escape in ["../outside", "/etc/passwd", "src/../../etc/passwd"] {
            assert!(
                resolve(tmp.path(), &[ContextRef::file(escape)]).is_err(),
                "{escape} must be refused"
            );
        }
    }

    /// The check the textual one cannot make: a path that is inside the
    /// checkout by name and outside it on the disk.
    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_checkout_is_still_an_escape() {
        let tmp = checkout("symlink");
        let outside = TempDir::new("symlink-target");
        write(outside.path(), "secrets.txt");
        std::os::unix::fs::symlink(
            outside.path().join("secrets.txt"),
            tmp.path().join("link.txt"),
        )
        .expect("symlink");

        let err = resolve(tmp.path(), &[ContextRef::file("link.txt")])
            .expect_err("a symlink out of the tree is an escape");
        assert!(err.to_string().contains("escapes the checkout"));

        std::fs::remove_file(tmp.path().join("link.txt")).expect("remove leaf link");
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("outside-dir"))
            .expect("directory symlink");
        let err = resolve(
            tmp.path(),
            &[ContextRef::file("outside-dir/not-created-yet.rs")],
        )
        .expect_err("a missing leaf beneath an escaping symlink is still an escape");
        assert!(err.to_string().contains("escapes the checkout"));
    }

    #[test]
    fn a_picked_reference_refuses_a_checkout_change_before_execution() {
        let first = checkout("identity-a");
        let second = checkout("identity-b");
        let picked = checkout_identity("device-a", first.path(), None);
        let current = checkout_identity("device-a", second.path(), None);
        let mut reference = ContextRef::file("src/main.rs");
        reference.checkout_id = Some(picked.clone());
        validate_checkout(&[reference.clone()], Some(&picked)).expect("same checkout");
        let err = validate_checkout(&[reference], Some(&current))
            .expect_err("a moved chat must not reinterpret the path");
        assert!(err.to_string().contains("different checkout"));
    }

    #[test]
    fn an_unstamped_reference_is_never_authority() {
        let reference = ContextRef::file("src/forged.rs");
        let err = validate_checkout(&[reference], Some("host-issued"))
            .expect_err("unstamped reference must be refused");
        assert!(err.to_string().contains("different checkout"));
    }
}
