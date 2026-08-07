//! The `comet-board` agent skill, shipped inside the binary (gh#133).
//!
//! Every runtime that dispatches from this board needs the same conventions in
//! context, and the way that used to happen was a file copied by hand into
//! three places — the operator's Mac, the box user, each agent-account slot.
//! Three copies drift, and a copy nobody re-copies goes stale silently against
//! a CLI whose flags it documents.
//!
//! So the text is an asset compiled in ([`SOURCE`]), stamped with the crate
//! version at render time ([`rendered`]), and written out by everything that
//! creates a place agents read from:
//!
//! - `comet-board skill install` — the operator's Mac and a teammate's laptop.
//! - the box wizard's board stage — a fresh box.
//! - `AgentAccounts::materialize` (comet-engine) — every slot, on every
//!   dispatch, because a slot overrides `CLAUDE_CONFIG_DIR` and so cannot see
//!   the user-level copy at all.
//! - `comet-board doctor` — says when what the agents see is not this binary's.
//!
//! Claude Code discovers `<config dir>/skills/<name>/SKILL.md`, and
//! `CLAUDE_CONFIG_DIR` relocates that whole tree — which is the same variable
//! the engine stamps on a dispatched child, so "install into a config dir" is
//! the one operation all four callers need.

use std::io;
use std::path::{Path, PathBuf};

/// The skill's directory name, and the name in its frontmatter.
pub const NAME: &str = "comet-board";

/// The version the installed copy is stamped with: this crate's, which is the
/// workspace version, which is the CLI's. That is the whole point — the skill
/// documents flags, and flags change with the binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The authored text, verbatim from `assets/skills/comet-board/SKILL.md`.
///
/// Not what gets written: it still carries [`VERSION_PLACEHOLDER`], because a
/// version baked into the repo file would be a second place to bump and a
/// diff on every release.
pub const SOURCE: &str = include_str!("../../../assets/skills/comet-board/SKILL.md");

/// Replaced by [`VERSION`] on the way out.
pub const VERSION_PLACEHOLDER: &str = "{{VERSION}}";

/// The trailing marker's opening, which `version_of` reads back.
const MARKER: &str = "<!-- comet-board skill ";

/// The text to install: [`SOURCE`] with the version stamped in.
pub fn rendered() -> String {
    SOURCE.replace(VERSION_PLACEHOLDER, VERSION)
}

/// `<config dir>/skills/comet-board/SKILL.md` — where Claude Code looks.
pub fn path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("skills").join(NAME).join("SKILL.md")
}

/// The Claude config dir of *this* shell: `$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
///
/// Inside a board-dispatched agent this is the slot dir the engine stamped, so
/// an agent running `skill install` writes where it itself reads — which is
/// the right answer, not a special case.
pub fn user_config_dir() -> PathBuf {
    match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".claude")
        }
    }
}

/// The version stamped into an installed copy, if it carries a marker.
pub fn version_of(text: &str) -> Option<&str> {
    let rest = text.rsplit_once(MARKER)?.1;
    let token = rest.split_whitespace().next()?;
    (!token.is_empty()).then_some(token)
}

/// What a config dir currently holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Byte-identical to what this binary ships.
    Current,
    /// Present and different. `version` is its stamp, when it has one — a copy
    /// edited in place at the same version reads as stale too, which is
    /// correct: an installed skill is a build artifact, not a file to edit.
    Stale { version: Option<String> },
    /// No `SKILL.md` there.
    Missing,
    /// There, and unreadable — a permissions problem worth naming rather than
    /// reporting as "not installed".
    Unreadable(String),
}

impl State {
    pub fn is_current(&self) -> bool {
        matches!(self, State::Current)
    }
}

/// Read what `config_dir` holds without changing it.
pub fn status_of(config_dir: &Path) -> State {
    let path = path_in(config_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) if text == rendered() => State::Current,
        Ok(text) => State::Stale {
            version: version_of(&text).map(str::to_string),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => State::Missing,
        Err(e) => State::Unreadable(format!("{e}")),
    }
}

/// One install's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub path: PathBuf,
    /// False when the file was already byte-identical. `materialize` runs this
    /// on every dispatch, so a no-op has to be a genuine no-op — nothing
    /// rewritten under a CLI that may be reading it.
    pub changed: bool,
}

/// Write the skill into `config_dir`, creating `skills/comet-board/`.
///
/// Idempotent, and a re-install always wins: the installed copy is this
/// binary's, and local edits to it are the drift this exists to end.
pub fn install_into(config_dir: &Path) -> io::Result<Installed> {
    let path = path_in(config_dir);
    let text = rendered();
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == text) {
        return Ok(Installed {
            path,
            changed: false,
        });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Written through a temp file in the same directory: a harness reading the
    // skill while a dispatch re-installs it must never see half a document.
    let tmp = path.with_extension("md.tmp");
    std::fs::write(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    Ok(Installed {
        path,
        changed: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_carries_the_placeholder_and_renders_this_version() {
        assert!(
            SOURCE.contains(VERSION_PLACEHOLDER),
            "the repo asset must not hardcode a version"
        );
        let out = rendered();
        assert!(!out.contains(VERSION_PLACEHOLDER));
        assert_eq!(version_of(&out), Some(VERSION));
    }

    #[test]
    fn frontmatter_names_the_skill() {
        assert!(SOURCE.starts_with("---\nname: comet-board\n"));
        assert!(SOURCE.contains("\ndescription: "));
    }

    #[test]
    fn install_is_idempotent_then_repairs_drift() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(status_of(tmp.path()), State::Missing);

        let first = install_into(tmp.path()).unwrap();
        assert!(first.changed);
        assert_eq!(first.path, path_in(tmp.path()));
        assert_eq!(status_of(tmp.path()), State::Current);

        // The dispatch path calls this every time; a no-op must not rewrite.
        assert!(!install_into(tmp.path()).unwrap().changed);

        std::fs::write(&first.path, "stale\n<!-- comet-board skill 0.0.1 -->\n").unwrap();
        assert_eq!(
            status_of(tmp.path()),
            State::Stale {
                version: Some("0.0.1".into())
            }
        );
        assert!(install_into(tmp.path()).unwrap().changed);
        assert_eq!(status_of(tmp.path()), State::Current);
    }

    #[test]
    fn an_unstamped_copy_is_stale_without_a_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "somebody's own text\n").unwrap();
        assert_eq!(status_of(tmp.path()), State::Stale { version: None });
    }
}
