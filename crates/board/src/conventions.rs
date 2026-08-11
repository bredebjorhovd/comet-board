//! The board's conventions, written into the instruction file each runtime
//! reads on its own (gh#272).
//!
//! [`crate::skill`] closed this gap for exactly one harness. Claude Code
//! discovers `<config dir>/skills/comet-board/SKILL.md`, so a dispatch pointed
//! at an account slot gets the board's contract on demand — and Codex, which
//! has no skill mechanism at all, got nothing. `docs/agent-conventions.md` has
//! said since the port that its markers exist "so an installer can write it
//! into each runtime's global instruction file"; nothing was that installer.
//! This is.
//!
//! Both CLIs read one file out of the same config dir the engine already
//! relocates per run:
//!
//! | harness     | env var             | file        |
//! |-------------|---------------------|-------------|
//! | Claude Code | `CLAUDE_CONFIG_DIR` | `CLAUDE.md` |
//! | Codex       | `CODEX_HOME`        | `AGENTS.md` |
//!
//! Three decisions worth keeping written down, because each had an obvious
//! wrong answer:
//!
//! **The skill does not shrink, and the block is not a copy of it.** What goes
//! in an instruction file is in context on every turn, so it pays rent forever;
//! what goes in a skill is read when the agent decides it is relevant. So the
//! block carries only what has to be true *before* anything is invoked — you
//! were dispatched, commit and push, the push credential is the board's, claim
//! what you changed, delegate through the board — and then points at where the
//! rest lives. For Claude that pointer is the skill beside it. For Codex there
//! is nothing to point at, so the skill's own text is appended ([`rendered`]),
//! which is why Codex agents pay for a longer block than Claude ones: they have
//! no other channel.
//!
//! **It never touches the checkout.** A dispatched worktree ships its repo's
//! own `AGENTS.md` / `CLAUDE.md`, and those are the authority on how to write
//! code in it. This writes into the *config* dir, and the text says outright
//! that the repo wins wherever the two look like they disagree.
//!
//! **The block is managed, the file is not.** Everything between [`BEGIN`] and
//! [`END`] belongs to this binary and is rewritten whole on every dispatch;
//! everything outside is somebody's own instruction file and is never read for
//! meaning, reordered, or dropped. That is what makes writing into a box user's
//! `~/.claude/CLAUDE.md` — which is where a dispatch naming no account lands —
//! something that can be undone: turn `agent_instructions` off and the next
//! dispatch takes the block back out (see [`apply`]).

use std::io;
use std::path::{Path, PathBuf};

use comet_proto::HarnessId;

/// The managed region's opening marker. Verbatim from
/// `docs/agent-conventions.md`, which has carried the pair since the port.
pub const BEGIN: &str = "<!-- BEGIN comet-board conventions -->";

/// The managed region's closing marker.
pub const END: &str = "<!-- END comet-board conventions -->";

/// The version an installed block is stamped with: this crate's, which is the
/// CLI's. Same reasoning as [`crate::skill::VERSION`] — the text documents
/// flags, and flags change with the binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The authored preamble, verbatim from `assets/instructions/comet-board.md`.
///
/// Not what gets written: it still carries both placeholders below.
pub const SOURCE: &str = include_str!("../../../assets/instructions/comet-board.md");

/// Replaced by [`VERSION`] in the stamp line on the way out.
const VERSION_PLACEHOLDER: &str = "{{VERSION}}";

/// Replaced per harness: a pointer at the skill, or the skill itself.
const REFERENCE_PLACEHOLDER: &str = "{{REFERENCE}}";

/// What the stamp line reads back as, for [`version_of`].
const STAMP: &str = "<!-- comet-board conventions ";

/// The file `harness` reads its global instructions out of, relative to the
/// config dir the engine points its child at.
///
/// `None` for the harnesses that have no such file *and* no config dir comet
/// relocates — there is nowhere to put one, and inventing a path would write a
/// file nothing ever reads.
pub fn file_name(harness: HarnessId) -> Option<&'static str> {
    match harness {
        HarnessId::ClaudeCode => Some("CLAUDE.md"),
        HarnessId::Codex => Some("AGENTS.md"),
        HarnessId::Opencode | HarnessId::Cursor | HarnessId::Mock => None,
    }
}

/// Where the block goes inside `config_dir`.
pub fn path_in(config_dir: &Path, harness: HarnessId) -> Option<PathBuf> {
    file_name(harness).map(|name| config_dir.join(name))
}

/// The whole managed block, markers included, for `harness`.
///
/// Claude gets the preamble and a pointer at the skill installed beside it
/// (`AgentAccounts::materialize` writes both into the same dir). Codex gets the
/// preamble and the skill's own body, because for Codex this file is the only
/// channel there is.
pub fn rendered(harness: HarnessId) -> String {
    // Hard-wrapped like the asset around it: this lands in a file people open.
    let reference = match harness {
        HarnessId::ClaudeCode => concat!(
            "The rest of the contract — every verb, its flags, and what the board does\n",
            "with what you send — is the `comet-board` skill, installed in this same\n",
            "config dir. Read it before you dispatch, claim, or review.",
        )
        .to_string(),
        _ => format!(
            concat!(
                "The rest of the contract follows: every verb, its flags, and what the\n",
                "board does with what you send. It is the same text a Claude Code session\n",
                "on this box gets as a skill.\n\n{}",
            ),
            skill_body()
        ),
    };
    let body = SOURCE
        .replace(REFERENCE_PLACEHOLDER, &reference)
        .replace(VERSION_PLACEHOLDER, VERSION);
    format!(
        concat!(
            "{BEGIN}\n{body}\n\n",
            "{STAMP}{VERSION} — written by comet-board on dispatch.\n",
            "     Edits inside these markers are overwritten; text outside them is kept.\n",
            "     Turn it off with `comet-board routes defaults agent_instructions false`. -->\n",
            "{END}\n",
        ),
        BEGIN = BEGIN,
        body = body.trim(),
        STAMP = STAMP,
        VERSION = VERSION,
        END = END,
    )
}

/// The skill's text with the parts an instruction file has no use for removed:
/// the YAML frontmatter (a Claude Code discovery header) and the trailing
/// provenance comment (this block carries its own stamp).
fn skill_body() -> String {
    let text = crate::skill::rendered();
    let after_frontmatter = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .map(|(_, body)| body)
        .unwrap_or(&text);
    let trimmed = match after_frontmatter.rfind("<!-- comet-board skill ") {
        Some(at) => &after_frontmatter[..at],
        None => after_frontmatter,
    };
    trimmed.trim().to_string()
}

/// The version stamped into an installed block, if it carries a stamp.
pub fn version_of(text: &str) -> Option<&str> {
    let rest = text.rsplit_once(STAMP)?.1;
    let token = rest.split_whitespace().next()?;
    (!token.is_empty()).then_some(token)
}

/// What an instruction file currently holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// The block is there and byte-identical to what this binary renders.
    Current,
    /// A block is there and differs. `version` is its stamp, when it has one.
    Stale { version: Option<String> },
    /// No block. The file may or may not exist — either way there is nothing of
    /// ours in it.
    Absent,
    /// One marker without the other, or `END` before `BEGIN`. Nothing is
    /// written over this: a half-marked file is somebody's edit in progress, and
    /// guessing where the block ends is how a splice eats a paragraph.
    Malformed,
    /// There, and unreadable — worth naming rather than reporting as absent.
    Unreadable(String),
}

impl State {
    pub fn is_current(&self) -> bool {
        matches!(self, State::Current)
    }
}

/// Read what `config_dir` holds without changing it.
pub fn status_of(config_dir: &Path, harness: HarnessId) -> State {
    let Some(path) = path_in(config_dir, harness) else {
        return State::Absent;
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return State::Absent,
        Err(e) => return State::Unreadable(format!("{e}")),
    };
    match region(&text) {
        Ok(None) => State::Absent,
        Ok(Some((start, end))) => {
            let block = &text[start..end];
            if format!("{block}\n") == rendered(harness) {
                State::Current
            } else {
                State::Stale {
                    version: version_of(block).map(str::to_string),
                }
            }
        }
        Err(()) => State::Malformed,
    }
}

/// One write's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub path: PathBuf,
    /// False when the file already said exactly this. Every dispatch calls
    /// [`apply`], so a no-op has to be a genuine no-op — nothing rewritten
    /// under a CLI that may be reading it.
    pub changed: bool,
}

/// Write the block into `config_dir`'s instruction file, or take it back out.
///
/// The whole of the opt-out surface: `agent_instructions` is resolved per route
/// against `[defaults]` at dispatch and arrives here as `enabled`. Turning it
/// off *removes* rather than merely stopping, because slot dirs are reused —
/// otherwise a route that opted out would keep serving whatever the last
/// dispatch on that slot left behind, which is the exact staleness the marker
/// pair exists to prevent.
pub fn apply(config_dir: &Path, harness: HarnessId, enabled: bool) -> io::Result<Option<Outcome>> {
    if enabled {
        install_into(config_dir, harness)
    } else {
        remove_from(config_dir, harness)
    }
}

/// Put this binary's block in `config_dir`'s instruction file, creating it if
/// there is none. `Ok(None)` for a harness with no instruction file.
///
/// Idempotent, and a re-install always wins inside the markers: the block is a
/// build artifact, and local edits to it are the drift this exists to end.
/// Everything outside them survives untouched, including a file that was
/// nothing but somebody's own notes before this ran.
pub fn install_into(config_dir: &Path, harness: HarnessId) -> io::Result<Option<Outcome>> {
    let Some(path) = path_in(config_dir, harness) else {
        return Ok(None);
    };
    let existing = read_existing(&path)?;
    let next = splice(&existing, &rendered(harness)).map_err(|()| malformed(&path))?;
    write_if_changed(&path, &existing, &next).map(Some)
}

/// Take the block out of `config_dir`'s instruction file, leaving everything
/// else. A file that was only ever the block is removed rather than left as a
/// blank one — an empty `CLAUDE.md` reads as an intentionally empty instruction
/// file, and it is not one.
pub fn remove_from(config_dir: &Path, harness: HarnessId) -> io::Result<Option<Outcome>> {
    let Some(path) = path_in(config_dir, harness) else {
        return Ok(None);
    };
    let existing = read_existing(&path)?;
    let Some((start, end)) = region(&existing).map_err(|()| malformed(&path))? else {
        return Ok(Some(Outcome {
            path,
            changed: false,
        }));
    };
    let mut next = String::with_capacity(existing.len());
    next.push_str(existing[..start].trim_end());
    let rest = existing[end..].trim();
    if !next.is_empty() && !rest.is_empty() {
        next.push_str("\n\n");
    }
    next.push_str(rest);
    if next.trim().is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)?;
            return Ok(Some(Outcome {
                path,
                changed: true,
            }));
        }
        return Ok(Some(Outcome {
            path,
            changed: false,
        }));
    }
    next.push('\n');
    write_if_changed(&path, &existing, &next).map(Some)
}

fn read_existing(path: &Path) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

fn malformed(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{} has one of the comet-board conventions markers without the other — \
             not touching it. Repair or remove the markers by hand.",
            path.display()
        ),
    )
}

/// The half-open byte range of the managed block (markers included), if the
/// file has a well-formed pair. `Err(())` is a broken pair.
#[allow(clippy::result_unit_err)]
pub fn region(text: &str) -> Result<Option<(usize, usize)>, ()> {
    match (text.find(BEGIN), text.find(END)) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) if end > start => Ok(Some((start, end + END.len()))),
        _ => Err(()),
    }
}

/// `block` in place of whatever managed region `existing` had, or appended
/// after it when it had none.
///
/// Appended rather than prepended: whoever wrote the file wrote its opening
/// line, and a machine-written block above it would displace the first thing a
/// human meant an agent to read.
fn splice(existing: &str, block: &str) -> Result<String, ()> {
    let Some((start, end)) = region(existing)? else {
        let head = existing.trim_end();
        return Ok(if head.is_empty() {
            block.to_string()
        } else {
            format!("{head}\n\n{block}")
        });
    };
    let mut out = String::with_capacity(existing.len() + block.len());
    out.push_str(&existing[..start]);
    out.push_str(block.trim_end());
    let tail = &existing[end..];
    if tail.trim().is_empty() {
        out.push('\n');
    } else {
        out.push_str(tail);
    }
    Ok(out)
}

/// Write through a temp file in the same directory: a harness reading the
/// instruction file while a dispatch rewrites it must never see half a
/// document, and half a document here is half a contract.
fn write_if_changed(path: &Path, existing: &str, next: &str) -> io::Result<Outcome> {
    if existing == next && path.exists() {
        return Ok(Outcome {
            path: path.to_path_buf(),
            changed: false,
        });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("md.comet-board.tmp");
    std::fs::write(&tmp, next.as_bytes())?;
    std::fs::rename(&tmp, path)?;
    Ok(Outcome {
        path: path.to_path_buf(),
        changed: true,
    })
}

/// The config dir a dispatch naming *no* account reads for `harness` — the
/// CLI's own, relocated by the same variable the engine stamps per run.
///
/// The engine resolves this from its own `AgentAccountsConfig`; this is the
/// outside-in copy, for `doctor`, which has to answer a question about dirs it
/// does not create. Both read the same two variables, which is what makes the
/// answers the same.
pub fn user_config_dir(harness: HarnessId) -> Option<PathBuf> {
    let env_dir = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let home = || PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    match harness {
        HarnessId::ClaudeCode => Some(
            env_dir("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".claude")),
        ),
        HarnessId::Codex => Some(
            env_dir("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home().join(".codex")),
        ),
        _ => None,
    }
}

/// Which harness a materialized account slot dir belongs to, read off the
/// credential file `AgentAccounts::materialize` wrote into it.
///
/// `doctor` needs this and has no engine to ask: a slot dir is named by its id
/// and nothing else, so the only way to know which instruction file *should* be
/// in it is what the CLI writing that dir left there. `None` for a dir that has
/// neither — a slot that has never been materialized, most likely.
pub fn slot_harness(dir: &Path) -> Option<HarnessId> {
    if dir.join(".credentials.json").exists() || dir.join(".claude.json").exists() {
        Some(HarnessId::ClaudeCode)
    } else if dir.join("auth.json").exists() {
        Some(HarnessId::Codex)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(dir: &Path, harness: HarnessId) -> String {
        std::fs::read_to_string(path_in(dir, harness).unwrap()).unwrap()
    }

    #[test]
    fn the_asset_carries_both_placeholders_and_the_block_carries_neither() {
        assert!(SOURCE.contains(REFERENCE_PLACEHOLDER));
        for harness in [HarnessId::ClaudeCode, HarnessId::Codex] {
            let block = rendered(harness);
            assert!(!block.contains(REFERENCE_PLACEHOLDER));
            assert!(!block.contains(VERSION_PLACEHOLDER));
            assert!(block.starts_with(BEGIN));
            assert!(block.trim_end().ends_with(END));
            assert_eq!(version_of(&block), Some(VERSION));
        }
    }

    /// The split the module doc argues for: Claude points at the skill it can
    /// invoke, Codex carries the skill's text because it cannot invoke one.
    #[test]
    fn codex_gets_the_skill_and_claude_gets_a_pointer_to_it() {
        let claude = rendered(HarnessId::ClaudeCode);
        let codex = rendered(HarnessId::Codex);
        assert!(claude.contains("`comet-board` skill"));
        assert!(!claude.contains("## Every verb"));
        assert!(codex.contains("## Every verb"));
        assert!(codex.contains("comet-board claim"));
        // Frontmatter is a Claude Code discovery header; pasted into an
        // instruction file it is a stray YAML fence.
        assert!(!codex.contains("\nname: comet-board\n"));
        assert!(codex.len() > claude.len());
    }

    #[test]
    fn a_harness_with_no_instruction_file_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(file_name(HarnessId::Mock), None);
        assert_eq!(install_into(tmp.path(), HarnessId::Mock).unwrap(), None);
        assert_eq!(status_of(tmp.path(), HarnessId::Mock), State::Absent);
        assert!(std::fs::read_dir(tmp.path()).unwrap().next().is_none());
    }

    #[test]
    fn install_is_idempotent_then_repairs_drift() {
        let tmp = tempfile::tempdir().unwrap();
        let harness = HarnessId::ClaudeCode;
        assert_eq!(status_of(tmp.path(), harness), State::Absent);

        let first = install_into(tmp.path(), harness).unwrap().unwrap();
        assert!(first.changed);
        assert_eq!(first.path, path_in(tmp.path(), harness).unwrap());
        assert_eq!(status_of(tmp.path(), harness), State::Current);

        // Every dispatch calls this; a no-op must not rewrite.
        assert!(!install_into(tmp.path(), harness).unwrap().unwrap().changed);

        std::fs::write(
            &first.path,
            format!("{BEGIN}\nold conventions\n{STAMP}0.0.1 -->\n{END}\n"),
        )
        .unwrap();
        assert_eq!(
            status_of(tmp.path(), harness),
            State::Stale {
                version: Some("0.0.1".into())
            }
        );
        assert!(install_into(tmp.path(), harness).unwrap().unwrap().changed);
        assert_eq!(status_of(tmp.path(), harness), State::Current);
    }

    /// The property the whole marker scheme is for: a box user's own
    /// `CLAUDE.md` is somebody's file, and a dispatch is a guest in it.
    #[test]
    fn text_outside_the_markers_survives_install_update_and_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let harness = HarnessId::ClaudeCode;
        let path = path_in(tmp.path(), harness).unwrap();
        let mine = "# Working with me\n\n- Be concise.\n";
        std::fs::write(&path, mine).unwrap();

        install_into(tmp.path(), harness).unwrap();
        let after = read(tmp.path(), harness);
        assert!(after.starts_with(mine.trim_end()));
        assert!(after.contains(BEGIN));

        // An update rewrites the block in place, not the file.
        std::fs::write(
            &path,
            after.replace("You may be running under", "STALE TEXT"),
        )
        .unwrap();
        install_into(tmp.path(), harness).unwrap();
        let updated = read(tmp.path(), harness);
        assert!(updated.starts_with(mine.trim_end()));
        assert!(!updated.contains("STALE TEXT"));
        assert_eq!(status_of(tmp.path(), harness), State::Current);

        // And removal gives the file back exactly as it was.
        assert!(remove_from(tmp.path(), harness).unwrap().unwrap().changed);
        assert_eq!(read(tmp.path(), harness), mine);
        assert_eq!(status_of(tmp.path(), harness), State::Absent);
        // Idempotent both ways.
        assert!(!remove_from(tmp.path(), harness).unwrap().unwrap().changed);
    }

    #[test]
    fn text_after_the_block_survives_too() {
        let tmp = tempfile::tempdir().unwrap();
        let harness = HarnessId::Codex;
        let path = path_in(tmp.path(), harness).unwrap();
        std::fs::write(&path, format!("before\n\n{BEGIN}\nold\n{END}\n\nafter\n")).unwrap();
        install_into(tmp.path(), harness).unwrap();
        let after = read(tmp.path(), harness);
        assert!(after.starts_with("before\n"));
        assert!(after.trim_end().ends_with("after"));
        assert!(!after.contains("\nold\n"));
        assert_eq!(status_of(tmp.path(), harness), State::Current);

        remove_from(tmp.path(), harness).unwrap();
        assert_eq!(read(tmp.path(), harness), "before\n\nafter\n");
    }

    /// A slot dir reused by a route that opted out must not keep serving the
    /// last route's conventions — and a file that was only ever ours goes.
    #[test]
    fn apply_writes_then_takes_the_whole_file_back_out() {
        let tmp = tempfile::tempdir().unwrap();
        let harness = HarnessId::Codex;
        apply(tmp.path(), harness, true).unwrap();
        let path = path_in(tmp.path(), harness).unwrap();
        assert!(path.exists());

        apply(tmp.path(), harness, false).unwrap();
        assert!(!path.exists());
        assert_eq!(status_of(tmp.path(), harness), State::Absent);
        // Nothing to remove is not an error, and does not recreate the file.
        assert!(!apply(tmp.path(), harness, false).unwrap().unwrap().changed);
        assert!(!path.exists());
    }

    #[test]
    fn a_half_marked_file_is_refused_rather_than_spliced() {
        let tmp = tempfile::tempdir().unwrap();
        let harness = HarnessId::ClaudeCode;
        let path = path_in(tmp.path(), harness).unwrap();
        let broken = format!("mine\n{BEGIN}\nhalf a block\n");
        std::fs::write(&path, &broken).unwrap();

        assert_eq!(status_of(tmp.path(), harness), State::Malformed);
        assert!(install_into(tmp.path(), harness).is_err());
        assert!(remove_from(tmp.path(), harness).is_err());
        assert_eq!(read(tmp.path(), harness), broken, "the file is untouched");

        // Markers the wrong way round, same answer.
        std::fs::write(&path, format!("{END}\ninverted\n{BEGIN}\n")).unwrap();
        assert_eq!(status_of(tmp.path(), harness), State::Malformed);
        assert!(install_into(tmp.path(), harness).is_err());
    }

    #[test]
    fn a_slot_dir_names_its_harness_by_what_the_cli_left_in_it() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(slot_harness(tmp.path()), None);
        std::fs::write(tmp.path().join("auth.json"), "{}").unwrap();
        assert_eq!(slot_harness(tmp.path()), Some(HarnessId::Codex));
        std::fs::write(tmp.path().join(".credentials.json"), "{}").unwrap();
        assert_eq!(slot_harness(tmp.path()), Some(HarnessId::ClaudeCode));
    }
}
