//! Which skills and slash commands a run can invoke (`ListSkills`; gh#134).
//!
//! **Discovery channel: the engine enumerates the dirs.** The alternative was
//! to read the list off the harness's own `system:init` frame, which does
//! announce its commands — but only once a run is live, and the composer needs
//! the list *before* the first prompt, in a chat that may never have run at
//! all. Enumerating also makes the slot question answerable, which the stream
//! cannot: a run whose chat names an agent account has `CLAUDE_CONFIG_DIR`
//! pointed at that account's dir (gh#59), so the box user's `~/.claude/skills`
//! is not what it reads, and offering that list would be offering skills the
//! run cannot invoke.
//!
//! That has a consequence worth stating plainly, because this is the surface
//! that makes it visible: a slot IS the run's config dir, so a board-dispatched
//! agent sees only what `materialize` put there — the board's own skill
//! (gh#133) — plus whatever its checkout ships in `.claude/`. The operator's
//! own `~/.claude/skills` is invisible from inside one. The honest list says so
//! rather than papering over it with the box user's.
//!
//! Layout read (Claude Code's own):
//! - `{config}/skills/<name>/SKILL.md` — one dir per skill, frontmatter inside;
//! - `{config}/commands/<name>.md`, and `<group>/<name>.md` one level down,
//!   which the CLI addresses as `/group:name`.
//!
//! Both are read under the run's config dir and under `{cwd}/.claude`. Nothing
//! here fails: an unreadable dir contributes no rows, because a picker that
//! errored because one of four directories was missing would be worse than one
//! that is short.

use std::path::{Path, PathBuf};

use comet_proto::{HarnessId, SkillDescriptor, SkillSource};

/// Frontmatter is at the head of the file; reading more than this to find a
/// `description:` would mean reading whole skill bodies to build a menu.
const FRONTMATTER_SCAN_BYTES: usize = 8 * 1024;

/// Where one run's skills come from.
pub struct SkillRoots {
    /// The run's config dir (`CLAUDE_CONFIG_DIR` as the child will see it).
    pub config_dir: Option<PathBuf>,
    /// The chat's working directory — `{cwd}/.claude` is the repo-level half.
    pub cwd: Option<PathBuf>,
}

/// Enumerate everything invocable, in [`comet_proto::view::skills::catalog`]
/// order (name-sorted, repo-level winning a collision).
pub fn list(roots: &SkillRoots) -> Vec<SkillDescriptor> {
    let mut found: Vec<SkillDescriptor> = Vec::new();
    if let Some(dir) = &roots.config_dir {
        collect(dir, SkillSource::Personal, &mut found);
    }
    if let Some(cwd) = &roots.cwd {
        collect(&cwd.join(".claude"), SkillSource::Project, &mut found);
    }
    comet_proto::view::skills::catalog(found)
}

/// Whether a harness has skills at all. Only Claude Code does today; the rest
/// return an empty list rather than an error, so a picker on a Codex chat is
/// simply empty instead of showing a failure the operator cannot act on.
pub fn harness_has_skills(harness: HarnessId) -> bool {
    matches!(harness, HarnessId::ClaudeCode)
}

fn collect(root: &Path, source: SkillSource, out: &mut Vec<SkillDescriptor>) {
    collect_skills(&root.join("skills"), source, out);
    collect_commands(&root.join("commands"), None, source, out);
}

/// `skills/<name>/SKILL.md`.
fn collect_skills(dir: &Path, source: SkillSource, out: &mut Vec<SkillDescriptor>) {
    for entry in read_dir(dir) {
        let path = entry.join("SKILL.md");
        if !path.is_file() {
            continue;
        }
        let Some(fallback) = file_name(&entry) else {
            continue;
        };
        let (name, description) = frontmatter(&path);
        out.push(SkillDescriptor {
            name: name.unwrap_or(fallback),
            description,
            source,
        });
    }
}

/// `commands/<name>.md`, plus one level of nesting addressed as `group:name`.
///
/// One level, not arbitrary depth: that is what the CLI's own `/group:name`
/// spelling can express, and a picker offering a name nobody can type is worse
/// than one that is missing a row.
fn collect_commands(
    dir: &Path,
    group: Option<&str>,
    source: SkillSource,
    out: &mut Vec<SkillDescriptor>,
) {
    for entry in read_dir(dir) {
        if entry.is_dir() {
            if group.is_none()
                && let Some(name) = file_name(&entry)
            {
                collect_commands(&entry, Some(&name), source, out);
            }
            continue;
        }
        if entry.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = entry.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let (_, description) = frontmatter(&entry);
        out.push(SkillDescriptor {
            name: match group {
                Some(group) => format!("{group}:{stem}"),
                None => stem.to_string(),
            },
            description,
            source,
        });
    }
}

/// Directory entries, sorted by path so the list is stable across calls
/// (`read_dir` order is the filesystem's, which differs per machine and, on
/// some, per call — a picker that reshuffled between keystrokes would be
/// unusable).
fn read_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    paths
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_owned)
        .filter(|n| !n.is_empty() && !n.starts_with('.'))
}

/// `name` and `description` out of a leading `---` YAML block.
///
/// Hand-rolled rather than a YAML dependency: two scalar keys off the head of a
/// file is not a parse, and the failure mode we care about (no frontmatter at
/// all) has to degrade to "use the filename" either way.
fn frontmatter(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(bytes) = std::fs::read(path) else {
        return (None, None);
    };
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(FRONTMATTER_SCAN_BYTES)]);
    let mut lines = head.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let mut name = String::new();
    let mut description = String::new();
    // Which top-level key the current line belongs to. Skill descriptions are
    // routinely long enough to wrap across several indented lines, and a
    // subtitle truncated at the author's line break rather than at the row's
    // width would look like the description itself was cut off.
    let mut key: Option<&str> = None;
    for line in lines {
        if line.trim() == "---" {
            break;
        }
        // An indented line continues the key above it — including an indented
        // `description:` under `metadata:`, which belongs to that block and
        // must not be mistaken for the skill's own.
        if !line.starts_with(char::is_whitespace) {
            key = match line.split_once(':') {
                Some((k, _)) => Some(k.trim()),
                None => None,
            };
        }
        let target = match key {
            Some("name") => &mut name,
            Some("description") => &mut description,
            _ => continue,
        };
        // The key's own line contributes only what follows the colon.
        let text = match line.starts_with(char::is_whitespace) {
            true => line,
            false => line.split_once(':').map(|(_, v)| v).unwrap_or_default(),
        };
        for word in text.split_whitespace() {
            if !target.is_empty() {
                target.push(' ');
            }
            target.push_str(word);
        }
    }
    (unquote(name), unquote(description))
}

/// A collapsed frontmatter scalar, unquoted, or `None` when it is empty.
fn unquote(value: String) -> Option<String> {
    let unquoted = value
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(&value);
    (!unquoted.is_empty()).then(|| unquoted.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch tree of our own — nothing here may read, let alone write, the
    /// developer's real `~/.claude`.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "comet-skills-{tag}-{}-{:?}",
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

    fn write(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, body).expect("write");
    }

    #[test]
    fn reads_both_layouts_from_both_roots() {
        let tmp = TempDir::new("skills");
        let config = tmp.path().join("config");
        let cwd = tmp.path().join("repo");

        write(
            &config.join("skills/comet-board/SKILL.md"),
            "---\nname: comet-board\ndescription: Read and drive the comet board\n---\n\nbody\n",
        );
        write(
            &config.join("commands/debug.md"),
            "---\ndescription: read the log\n---\n",
        );
        write(
            &config.join("commands/vercel/deploy.md"),
            "no frontmatter\n",
        );
        write(
            &cwd.join(".claude/skills/tdd/SKILL.md"),
            "---\ndescription: tests first\n---\n",
        );

        let rows = list(&SkillRoots {
            config_dir: Some(config),
            cwd: Some(cwd),
        });
        assert_eq!(
            rows.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["comet-board", "debug", "tdd", "vercel:deploy"]
        );
        assert_eq!(
            rows[0].description.as_deref(),
            Some("Read and drive the comet board")
        );
        assert_eq!(rows[0].source, SkillSource::Personal);
        // A skill dir with no frontmatter name is still named — by its folder.
        let tdd = rows.iter().find(|s| s.name == "tdd").expect("tdd");
        assert_eq!(tdd.source, SkillSource::Project);
        // A command with no frontmatter at all is offered with no subtitle
        // rather than dropped.
        let deploy = rows.last().expect("deploy");
        assert_eq!(deploy.description, None);
    }

    #[test]
    fn the_repo_wins_a_name_collision() {
        let tmp = TempDir::new("skills-collide");
        let config = tmp.path().join("config");
        let cwd = tmp.path().join("repo");
        write(
            &config.join("skills/deploy/SKILL.md"),
            "---\ndescription: mine\n---\n",
        );
        write(
            &cwd.join(".claude/skills/deploy/SKILL.md"),
            "---\ndescription: the repo's\n---\n",
        );

        let rows = list(&SkillRoots {
            config_dir: Some(config),
            cwd: Some(cwd),
        });
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].description.as_deref(), Some("the repo's"));
        assert_eq!(rows[0].source, SkillSource::Project);
    }

    #[test]
    fn missing_dirs_are_an_empty_list_not_an_error() {
        // The board-dispatched shape: a slot dir holding credentials and the
        // one skill `materialize` stamps into it (gh#133), and nothing of the
        // operator's own — which is exactly what that run can invoke.
        let tmp = TempDir::new("skills-empty");
        let account = tmp.path().join("accounts/0123456789abcdef");
        write(&account.join(".credentials.json"), "{}");
        write(
            &account.join("skills/comet-board/SKILL.md"),
            "---\nname: comet-board\ndescription: Read and drive the comet board\n---\n",
        );
        let rows = list(&SkillRoots {
            config_dir: Some(account),
            cwd: None,
        });
        assert_eq!(
            rows.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["comet-board"]
        );
        // Nothing configured at all is likewise empty rather than a failure.
        assert!(
            list(&SkillRoots {
                config_dir: Some(tmp.path().join("nope")),
                cwd: Some(tmp.path().join("also-nope")),
            })
            .is_empty()
        );
    }

    #[test]
    fn frontmatter_reading_is_scalar_only() {
        let tmp = TempDir::new("skills-frontmatter");
        let root = tmp.path();
        // Quoted, wrapped, and shadowed by a nested key of the same name.
        write(
            &root.join("skills/a/SKILL.md"),
            "---\nname: \"renamed\"\ndescription: 'a description\n  that wrapped'\nmetadata:\n  description: not this one\n---\n",
        );
        // No frontmatter marker at all.
        write(
            &root.join("skills/b/SKILL.md"),
            "description: not frontmatter\n",
        );

        let rows = list(&SkillRoots {
            config_dir: Some(root.to_path_buf()),
            cwd: None,
        });
        let renamed = rows.iter().find(|s| s.name == "renamed").expect("renamed");
        assert_eq!(
            renamed.description.as_deref(),
            Some("a description that wrapped"),
            "a wrapped scalar joins into one subtitle, quotes stripped"
        );
        let b = rows.iter().find(|s| s.name == "b").expect("b");
        assert_eq!(b.description, None);
    }
}
