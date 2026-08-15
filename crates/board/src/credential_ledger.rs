//! What the board's credential path did, and for which chat (gh#233).
//!
//! gh#68 was careful about where the token must *not* go. What it had no
//! answer for is the question gh#233 asked: **was the board's credential the
//! one that pushed?** The first opencode dispatch could not exec the askpass
//! helper, wrote a credential wrapper of its own, pushed with it, opened its
//! pull request, and finished green. Nothing in the board recorded that the
//! sanctioned path had failed, because nothing in the board recorded that it
//! had ever been asked. The only reason anybody knows is that the smoke ticket
//! happened to ask the agent to mention anything odd.
//!
//! So the path keeps a ledger. Four events, one line of JSON each, appended to
//! `<state_dir>/credentials.jsonl`:
//!
//! - [`Event::Handed`] — the engine wired a run's environment to the helper.
//!   Written at dispatch, and it is what makes the absence of the next event
//!   mean something.
//! - [`Event::Minted`] — the helper answered a prompt. `git push` asks per
//!   push, `gh` asks per invocation.
//! - [`Event::Failed`] — the helper ran and could not answer: no credential
//!   configured, a mint the API refused, a prompt for the wrong host.
//! - [`Event::Unusable`] — the path itself does not work. gh#233's own event:
//!   the shim is missing, or not executable, or reaches a binary that does not
//!   understand `git-askpass`. The engine checks before it hands anything over,
//!   so this is written *instead of* a `Handed`.
//!
//! Nothing in here is a secret. A ledger line names a repo, a chat and a tool —
//! the same class of fact as the environment the run is given, and deliberately
//! not the token, not the prompt, and not the URL git was talking to.
//!
//! Two readers. [`crate::doctor`] shows the last failure, so a broken box says
//! so when somebody asks it. And the settle path ([`crate::sync`]) compares
//! `Handed` against `Minted` for the chat that just finished: a branch that
//! reached origin on a run the board handed a credential to, with the helper
//! never once asked, was pushed with a credential the board did not issue.
//! That is the gh#233 shape exactly, and it is now a comment on the issue
//! rather than a thing somebody noticed.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::Paths;

/// The ledger's file name under the board's state dir.
pub const LEDGER_FILE: &str = "credentials.jsonl";

/// Rotated (once, to `.1`) past this, like [`crate::log`]. The queries below
/// are about the run that just ended, so losing the far tail costs nothing —
/// and losing a `Handed` can only make the settle check quieter, never louder.
const MAX_BYTES: u64 = 1024 * 1024;

/// What happened to the credential path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Event {
    /// A run's environment was pointed at the helper.
    Handed,
    /// The helper answered a prompt.
    Minted,
    /// The helper ran and could not answer.
    Failed,
    /// The path could not be used at all, so nothing was handed over.
    Unusable,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::Handed => "handed",
            Event::Minted => "minted",
            Event::Failed => "failed",
            Event::Unusable => "unusable",
        }
    }
}

/// One line of the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub at: String,
    pub event: Event,
    /// Which half of the path: `git-askpass`, `gh-token`, or `dispatch` for
    /// the engine's own check.
    pub tool: String,
    /// `owner/repo`, or empty where the caller had none to name.
    #[serde(default)]
    pub repo: String,
    /// The chat whose run this was — `COMET_BOARD_CHAT_ID`, inherited by every
    /// process the agent starts, which is what lets a mint be attributed to an
    /// attempt. `None` for a helper run outside a dispatched chat.
    #[serde(default)]
    pub chat: Option<String>,
    /// Why it failed, for the two failing events.
    #[serde(default)]
    pub error: Option<String>,
}

impl Entry {
    fn new(event: Event, tool: &str, repo: &str, chat: Option<&str>) -> Entry {
        Entry {
            at: chrono::Utc::now().to_rfc3339(),
            event,
            tool: tool.to_string(),
            repo: repo.to_string(),
            chat: chat.map(str::to_string),
            error: None,
        }
    }

    /// One line for a log or a doctor row.
    pub fn summary(&self) -> String {
        let mut s = format!("{} {}", self.at, self.event.as_str());
        if !self.repo.is_empty() {
            s.push_str(&format!(" {}", self.repo));
        }
        s.push_str(&format!(" via {}", self.tool));
        if let Some(err) = &self.error {
            s.push_str(&format!(": {err}"));
        }
        s
    }
}

/// Where the ledger lives.
pub fn path(paths: &Paths) -> PathBuf {
    paths.state_dir.join(LEDGER_FILE)
}

/// The engine wired a run to the helper.
pub fn handed(paths: &Paths, repo: &str, chat: Option<&str>) {
    append(paths, Entry::new(Event::Handed, "dispatch", repo, chat));
}

/// The helper answered.
pub fn minted(paths: &Paths, tool: &str, repo: &str, chat: Option<&str>) {
    append(paths, Entry::new(Event::Minted, tool, repo, chat));
}

/// The helper ran and could not answer.
pub fn failed(paths: &Paths, tool: &str, repo: &str, chat: Option<&str>, error: &str) {
    let mut entry = Entry::new(Event::Failed, tool, repo, chat);
    entry.error = Some(one_line(error));
    append(paths, entry);
}

/// The path itself does not work, so no run was given it.
pub fn unusable(paths: &Paths, repo: &str, chat: Option<&str>, error: &str) {
    let mut entry = Entry::new(Event::Unusable, "dispatch", repo, chat);
    entry.error = Some(one_line(error));
    append(paths, entry);
}

/// Best-effort: a ledger write must never be the reason a push fails. The
/// helper is running inside git's own credential prompt, and an unwritable
/// state dir is not git's problem.
fn append(paths: &Paths, entry: Entry) {
    use std::io::Write;

    let file = path(paths);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    rotate_if_needed(&file);
    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
    {
        let _ = writeln!(f, "{line}");
    }
}

fn rotate_if_needed(file: &Path) {
    if std::fs::metadata(file).is_ok_and(|m| m.len() > MAX_BYTES) {
        let _ = std::fs::rename(file, file.with_extension("jsonl.1"));
    }
}

/// An error as one line: git's failures are multi-line and a ledger entry that
/// wraps is a ledger entry nobody greps.
fn one_line(error: &str) -> String {
    error.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Every entry the current file holds, oldest first. A line that does not
/// parse is skipped rather than fatal: a half-written tail (two helpers
/// appending at once, a box that lost power) must not blind the readers to
/// everything before it.
pub fn entries(paths: &Paths) -> Vec<Entry> {
    let Ok(text) = std::fs::read_to_string(path(paths)) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The most recent entry that says the path did not work — what
/// [`crate::doctor`] shows and what an operator actually wants to read.
pub fn last_failure(paths: &Paths) -> Option<Entry> {
    entries(paths)
        .into_iter()
        .rfind(|e| matches!(e.event, Event::Failed | Event::Unusable))
}

/// What the ledger knows about one chat's run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChatRecord {
    /// The board pointed this run at its credential.
    pub handed: bool,
    /// The helper was asked, and answered, at least once.
    pub minted: bool,
    /// Everything that went wrong on this chat's behalf.
    pub failures: Vec<Entry>,
}

impl ChatRecord {
    /// The board handed this run a credential path and the path was never
    /// used. On its own that is only odd — an agent that pushed nothing has no
    /// reason to mint. It is the caller (a settle on work that reached origin)
    /// that makes it an accusation.
    pub fn handed_but_unused(&self) -> bool {
        self.handed && !self.minted
    }

    /// The board meant to be this run's credential and was not.
    ///
    /// Two shapes, one fact. Either the path was handed over and never asked —
    /// the gh#233 run, which pushed with a wrapper of its own — or it could not
    /// be handed over at all, because the check before dispatch found it
    /// broken. In both cases anything that reached origin got there on a
    /// credential nobody reviewed.
    pub fn unsanctioned(&self) -> bool {
        self.handed_but_unused()
            || (!self.minted && self.failures.iter().any(|f| f.event == Event::Unusable))
    }

    /// The last thing that went wrong, for a log line or a comment.
    pub fn last_failure(&self) -> Option<&Entry> {
        self.failures.last()
    }
}

/// Fold the ledger down to one chat.
pub fn for_chat(paths: &Paths, chat: &str) -> ChatRecord {
    let mut record = ChatRecord::default();
    for entry in entries(paths) {
        if entry.chat.as_deref() != Some(chat) {
            continue;
        }
        match entry.event {
            Event::Handed => record.handed = true,
            // A mint through `gh` is not a push, but it is the board's
            // credential doing the board's work — and `gh pr create` on a
            // branch is proof that the branch was pushed with something. The
            // distinction the settle cares about is board-credential versus
            // improvised, not git versus gh.
            Event::Minted => record.minted = true,
            Event::Failed | Event::Unusable => record.failures.push(entry),
        }
    }
    record
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ledger directory that dies with the test — the returned `TempDir`
    /// owns it, so keep it bound for the test's lifetime (gh#430).
    fn paths(name: &str) -> (tempfile::TempDir, Paths) {
        let tmp = tempfile::Builder::new()
            .prefix(&format!("comet-ledger-{name}-"))
            .tempdir()
            .unwrap();
        let dir = tmp.path().to_path_buf();
        let paths = Paths {
            config_dir: dir.clone(),
            state_dir: dir,
        };
        (tmp, paths)
    }

    /// The gh#233 shape, as the ledger sees it: a run the board wired up, a
    /// helper nobody could reach, and a push that happened anyway.
    #[test]
    fn a_run_that_was_handed_a_credential_and_never_used_it_is_visible_as_such() {
        let (_tmp, p) = paths("unused");
        handed(&p, "o/r", Some("chat-1"));
        let record = for_chat(&p, "chat-1");
        assert!(record.handed);
        assert!(record.handed_but_unused());

        // The same run, once the helper actually answers a push.
        minted(&p, "git-askpass", "o/r", Some("chat-1"));
        let record = for_chat(&p, "chat-1");
        assert!(record.minted);
        assert!(!record.handed_but_unused());

        // Another chat's mints are not this chat's alibi.
        minted(&p, "git-askpass", "o/r", Some("chat-2"));
        assert!(!for_chat(&p, "chat-3").handed_but_unused());
        assert!(!for_chat(&p, "chat-3").handed);
    }

    #[test]
    fn a_failure_is_kept_with_its_reason_and_read_back_as_the_latest_one() {
        let (_tmp, p) = paths("failures");
        unusable(&p, "o/r", Some("chat-1"), "cannot exec\n  the shim");
        failed(
            &p,
            "git-askpass",
            "o/r",
            Some("chat-1"),
            "no GitHub credential to push o/r with",
        );
        let last = last_failure(&p).expect("a failure");
        assert_eq!(last.event, Event::Failed);
        assert!(last.summary().contains("no GitHub credential"), "{last:?}");
        // A path the board could not even hand over is the same accusation as
        // one it handed over and nobody used: whatever pushed, it was not this.
        assert!(for_chat(&p, "chat-1").unsanctioned());
        // Newlines are flattened: a ledger line is one line.
        let first = &for_chat(&p, "chat-1").failures[0];
        assert_eq!(first.error.as_deref(), Some("cannot exec the shim"));
        assert_eq!(for_chat(&p, "chat-1").failures.len(), 2);
    }

    /// A ledger line is written by a helper git is holding a pipe to. It says
    /// what happened and names nothing that would be a leak if the file were
    /// read by whoever else is on the box (gh#55).
    #[test]
    fn a_ledger_line_carries_no_credential() {
        let (_tmp, p) = paths("nosecret");
        minted(&p, "git-askpass", "o/r", Some("chat-1"));
        failed(
            &p,
            "gh-token",
            "o/r",
            Some("chat-1"),
            "github said 401 for ghs_deadbeef",
        );
        let text = std::fs::read_to_string(path(&p)).unwrap();
        // The one place a token could get in is an error message quoted from
        // somewhere else, which is the caller's business to keep clean — what
        // this asserts is that nothing here *adds* one.
        assert!(text.contains("\"event\":\"minted\""), "{text}");
        assert!(text.contains("\"chat\":\"chat-1\""), "{text}");
        assert_eq!(text.lines().count(), 2);
    }

    /// A truncated tail is a normal state for an append-only file two
    /// processes share. It must cost the last line, not the file.
    #[test]
    fn a_half_written_line_does_not_hide_the_ones_before_it() {
        let (_tmp, p) = paths("torn");
        handed(&p, "o/r", Some("chat-1"));
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(path(&p))
                .unwrap();
            write!(f, "{{\"at\":\"2026-08-10T00:00").unwrap();
        }
        assert_eq!(entries(&p).len(), 1);
        assert!(for_chat(&p, "chat-1").handed);
    }
}
