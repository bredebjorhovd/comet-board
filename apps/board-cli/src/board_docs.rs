//! `docs/board/` is the handoff record, one file per item (gh#203).
//!
//! It used to be one `### H<n>` section per item inside `docs/BOARD.md`, which
//! made the end of that file an append-point every branch wrote to at once: six
//! merge conflicts in one day, none of them about behaviour, and a conflicted
//! pull request gets no CI run at all. One file per item removes the shared
//! line; the directory listing is the index, so there is nothing left to append
//! to.
//!
//! What that trades away is a compiler. Prose referring to `§settle-logic` was
//! already unchecked — three references in `BOARD.md` pointed at the wrong
//! section on the day this landed, and two open pull requests had both claimed
//! `§gh#184` — and a filename is no more self-checking than a number. So the
//! check moves here: every `§key` written anywhere in the repo must name a
//! file in `docs/board/`, and a rename that orphans one fails the build
//! instead of being discovered by the next person who follows the reference.
//!
//! The grammar, both halves deliberate:
//!
//! - `§gh#187` — the item's ticket. The tracker assigns it, never the writer,
//!   so two agents working at the same time cannot collide on one.
//! - `§settle-logic` — a bare slug, for the eight written before this repo had
//!   a tracker. New items do not get one.
//!
//! `§1.10`, `§3.5` and friends belong to `docs/PARITY.md` and `ARCHITECTURE.md`
//! and are not this module's business; the grammar excludes them by starting a
//! key with a letter.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// This module writes `§` in its own tests, about the grammar rather than in
/// it. Nothing else is exempt — `docs/BOARD.md` included.
const EXEMPT: &[&str] = &["apps/board-cli/src/board_docs.rs"];

/// Extensions worth reading: where prose about the board is written.
const READ: &[&str] = &["rs", "md", "swift", "sh", "toml", "ts", "tsx"];

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every key `docs/board/` defines: `gh-187-….md` → `gh#187`, `settle-logic.md`
/// → `settle-logic`.
pub fn defined_keys(dir: &Path) -> std::io::Result<BTreeSet<String>> {
    let mut keys = BTreeSet::new();
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".md") else {
            continue;
        };
        keys.insert(key_of(stem));
    }
    Ok(keys)
}

/// A filename's key: the ticket if it has one, else the whole stem.
pub fn key_of(stem: &str) -> String {
    match stem.strip_prefix("gh-") {
        Some(rest) => {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if digits.is_empty() {
                stem.to_string()
            } else {
                format!("gh#{digits}")
            }
        }
        None => stem.to_string(),
    }
}

/// Every `§key` in `text`, in the board's grammar: `§gh#<digits>` or `§<slug>`
/// where the slug is lowercase-ASCII and starts with a letter. A key ending in
/// `'s` is possessive prose (`§settle-logic's inverse`), not part of the key.
pub fn references(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '§' {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        if chars.get(start..start + 3) == Some(&['g', 'h', '#']) {
            j = start + 3;
            let digits = j;
            while chars.get(j).is_some_and(char::is_ascii_digit) {
                j += 1;
            }
            if j > digits {
                out.push(chars[start..j].iter().collect());
            }
        } else if chars.get(start).is_some_and(char::is_ascii_lowercase) {
            while chars
                .get(j)
                .is_some_and(|c| c.is_ascii_lowercase() || *c == '-')
            {
                j += 1;
            }
            let key: String = chars[start..j].iter().collect();
            out.push(key.trim_end_matches('-').to_string());
        }
        i = j.max(start);
    }
    out
}

/// Every repo file worth scanning, `git ls-files` order, exemptions applied.
fn tracked_files(root: &Path) -> Vec<(String, String)> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files"])
        .output()
        .expect("git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|p| !EXEMPT.contains(p))
        .filter(|p| {
            Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| READ.contains(&e))
        })
        .filter_map(|p| {
            let text = std::fs::read_to_string(root.join(p)).ok()?;
            Some((p.to_string(), text))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point: a `§` reference that names no file is a broken link,
    /// and a broken link found by the build beats one found by a reader.
    #[test]
    fn every_section_reference_resolves() {
        let root = repo_root();
        let keys = defined_keys(&root.join("docs/board")).expect("read docs/board");
        assert!(keys.len() > 40, "docs/board looks empty: {keys:?}");

        let mut broken: Vec<String> = Vec::new();
        for (path, text) in tracked_files(&root) {
            for key in references(&text) {
                if !keys.contains(&key) {
                    broken.push(format!("{path}: §{key}"));
                }
            }
        }
        assert!(
            broken.is_empty(),
            "these `§` references name no file in docs/board/:\n  {}\n\
             Either the file was renamed (fix the reference) or the key is a \
             typo. Keys are `gh#<ticket>` or, for the eight pre-tracker items, \
             the filename.",
            broken.join("\n  ")
        );
    }

    /// A ticket may key exactly one item. Two files claiming `gh#187` is the
    /// collision the old `H<n>` numbering allowed twice over.
    #[test]
    fn a_ticket_keys_exactly_one_file() {
        let dir = repo_root().join("docs/board");
        let mut seen: std::collections::BTreeMap<String, String> = Default::default();
        for entry in std::fs::read_dir(&dir).expect("read docs/board") {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".md") else {
                panic!("docs/board/{name} is not a .md file");
            };
            let key = key_of(stem);
            if let Some(other) = seen.insert(key.clone(), name.clone()) {
                panic!("docs/board/{name} and docs/board/{other} both key §{key}");
            }
        }
    }

    /// A file has to say what it is on line one, or the directory listing is
    /// the only index and the index is wrong.
    #[test]
    fn every_file_opens_with_its_headline() {
        let dir = repo_root().join("docs/board");
        for entry in std::fs::read_dir(&dir).expect("read docs/board") {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(dir.join(&name)).expect("read section");
            let first = text.lines().next().unwrap_or_default();
            assert!(
                first.starts_with("# ") && first.len() > 4,
                "docs/board/{name} must open with an `# ` headline, got {first:?}"
            );
            assert!(
                !text.contains("\n## "),
                "docs/board/{name} uses `##`; a section file's own headings \
                 start at `###` so the file reads as one document"
            );
        }
    }

    /// A new item is named after its ticket. The eight that predate the tracker
    /// are the whole exception, and it does not grow.
    #[test]
    fn only_the_original_port_plan_goes_unticketed() {
        const PRE_TRACKER: &[&str] = &[
            "board-service",
            "runtime-impl",
            "dispatch-pipeline",
            "settle-logic",
            "review-delivery",
            "board-cli",
            "board-view",
            "adopt-doctor-init",
        ];
        let dir = repo_root().join("docs/board");
        for entry in std::fs::read_dir(&dir).expect("read docs/board") {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            let stem = name.trim_end_matches(".md").to_string();
            if key_of(&stem).starts_with("gh#") {
                continue;
            }
            assert!(
                PRE_TRACKER.contains(&stem.as_str()),
                "docs/board/{name} has no ticket. Name it `gh-<ticket>-<headline>.md` \
                 — the tracker hands out keys, so two agents writing at once cannot \
                 claim the same one."
            );
        }
    }

    /// No index, or the append-point comes back. A list of items is a list of
    /// shared lines: two branches adding two items land on the same one, git
    /// calls it a conflict, and GitHub runs no CI on a pull request it cannot
    /// build a merge commit for. The directory listing is the index.
    #[test]
    fn nothing_links_the_sections_into_a_list() {
        let root = repo_root();
        let link = regex_free_links(&std::fs::read_to_string(root.join("docs/BOARD.md")).unwrap());
        assert!(
            link.is_empty(),
            "docs/BOARD.md links section files directly: {link:?}\n\
             That is an index, and an index is the append-point gh#203 removed. \
             Link the directory (`docs/board/`) and let `ls` be the list."
        );
    }

    /// Markdown links whose target is a file inside `docs/board/` — the
    /// directory itself does not count.
    fn regex_free_links(md: &str) -> Vec<String> {
        md.match_indices("](")
            .filter_map(|(i, _)| {
                let rest = &md[i + 2..];
                let end = rest.find(')')?;
                let target = &rest[..end];
                let tail = target
                    .strip_prefix("board/")
                    .or(target.strip_prefix("docs/board/"))?;
                (tail.len() > 1).then(|| target.to_string())
            })
            .collect()
    }

    #[test]
    fn the_grammar_reads_keys_and_leaves_other_namespaces_alone() {
        assert_eq!(
            references("see §gh#187 and §settle-logic."),
            ["gh#187", "settle-logic"]
        );
        // Possessive prose is not part of the key.
        assert_eq!(references("§settle-logic's inverse"), ["settle-logic"]);
        // PARITY.md and ARCHITECTURE.md own the numeric ones.
        assert_eq!(references("§3.5, §1.10, §7").len(), 0);
        // A bare `§` is nobody's key.
        assert_eq!(references("§ and § again").len(), 0);
        assert_eq!(key_of("gh-187-the-picker"), "gh#187");
        assert_eq!(key_of("settle-logic"), "settle-logic");
    }
}
