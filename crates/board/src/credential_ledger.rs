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
//! Two readers. [`crate::doctor`] shows the last failure that is still
//! standing, so a broken box says so when somebody asks it — and, since
//! gh#515, so a box whose last failure was GitHub's own outage two days ago
//! does *not*. And the settle path ([`crate::sync`]) compares
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

/// Past this age a recorded failure is history rather than news (gh#515).
///
/// `doctor` is what an operator runs when the board looks wrong, so a red line
/// that is neither current nor actionable costs exactly what the check exists
/// to save: attention, spent on infrastructure that is fine. A day is long
/// enough that this morning's failed dispatch still shouts, and short enough
/// that the one from the day before yesterday has stopped.
pub const FRESH_SECS: i64 = 24 * 3_600;

/// Whose problem a recorded failure was (gh#515).
///
/// The distinction is the whole difference between the two things an operator
/// can do with a `doctor` line. One of them they can act on; the other one
/// they can only wait out, and a check that cannot tell them apart spends a
/// morning looking for a broken wrapper that was never broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// GitHub answered for itself — a 5xx, a gateway timeout, a rate limit.
    /// Nothing on this box caused it and nothing on this box will fix it.
    Upstream,
    /// Everything else: a shim that will not exec, no credential configured,
    /// an App that is not installed on the repo. These have a fix on this box.
    Local,
}

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
        format!("{} {}", self.at, self.what())
    }

    /// The same line without the stamp [`Entry::summary`] leads with, for a
    /// reader that has already said how long ago this was (gh#515).
    pub fn what(&self) -> String {
        let mut s = self.event.as_str().to_string();
        if !self.repo.is_empty() {
            s.push_str(&format!(" {}", self.repo));
        }
        s.push_str(&format!(" via {}", self.tool));
        if let Some(err) = &self.error {
            s.push_str(&format!(": {err}"));
        }
        s
    }

    /// When this was recorded, or `None` if the stamp will not parse — which
    /// only a hand-edited ledger produces, and which callers should read as
    /// "no idea how old", never as "new".
    pub fn at_utc(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        chrono::DateTime::parse_from_rfc3339(&self.at)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    }

    /// Seconds since this was recorded.
    pub fn age_secs(&self, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
        self.at_utc().map(|at| (now - at).num_seconds().max(0))
    }

    /// Recorded within [`FRESH_SECS`]. An age nobody can compute is not fresh:
    /// the point of the window is to stop an old failure shouting, and a stamp
    /// that will not parse is the one case where it might be arbitrarily old.
    pub fn is_fresh(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        self.age_secs(now).is_some_and(|secs| secs < FRESH_SECS)
    }

    /// Whose problem this was, read off the error the caller quoted (gh#515).
    ///
    /// Read rather than recorded, because the callers that write these lines
    /// are inside git's credential prompt with a string somebody else handed
    /// them; asking each of them to classify would be asking the layer with
    /// the least context. Unclassifiable means [`Cause::Local`] — the reading
    /// that keeps the check loud, so a cause this does not recognise is a
    /// quiet false alarm rather than a silent real one.
    pub fn cause(&self) -> Cause {
        let Some(error) = self.error.as_deref() else {
            return Cause::Local;
        };
        let error = error.to_ascii_lowercase();
        if http_status(&error).is_some_and(|status| (500..600).contains(&status)) {
            return Cause::Upstream;
        }
        const UPSTREAM: [&str; 8] = [
            "bad gateway",
            "service unavailable",
            "gateway timeout",
            "internal server error",
            "temporarily unavailable",
            "timed out",
            "timeout",
            "rate limit",
        ];
        if UPSTREAM.iter().any(|marker| error.contains(marker)) {
            Cause::Upstream
        } else {
            Cause::Local
        }
    }
}

/// The status in an error that quotes one — `github HTTP 504 for /repos/…`,
/// which is the shape [`crate::sources::github`] errors take. Only that shape:
/// a bare `504` somewhere in a path or a token is not a status and must not be
/// read as one. `https://` does not match, since the `s` eats the space.
fn http_status(lowercased: &str) -> Option<u16> {
    lowercased.match_indices("http ").find_map(|(at, marker)| {
        lowercased[at + marker.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u16>()
            .ok()
            .filter(|status| (100..600).contains(status))
    })
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

/// The last recorded failure the path has not since worked past (gh#515).
///
/// [`Event::Minted`] is the helper answering a real prompt on somebody's real
/// run, which is a stronger statement about the credential path than anything
/// that went wrong before it. So a failure with a mint after it is not a fact
/// about now, and this returns `None` for it: the ledger has already shown the
/// path working since. What comes back is a failure that is still the last
/// thing the path is known to have done — which is the most a file of history
/// can say about the present, and why [`crate::doctor`] probes as well.
pub fn standing_failure(paths: &Paths) -> Option<Entry> {
    for entry in entries(paths).into_iter().rev() {
        match entry.event {
            Event::Failed | Event::Unusable => return Some(entry),
            Event::Minted => return None,
            // Being handed the path says nothing about whether it works.
            Event::Handed => {}
        }
    }
    None
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
        let last = standing_failure(&p).expect("a failure");
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

    /// gh#515's first half: a failure the path has since worked past is not a
    /// fact about now, and the reader `doctor` uses must not return it.
    #[test]
    fn a_mint_after_a_failure_retires_it() {
        let (_tmp, p) = paths("superseded");
        unusable(&p, "o/r", Some("chat-1"), "the shim will not exec");
        assert!(standing_failure(&p).is_some());

        // Being handed the path again says nothing either way — only using it.
        handed(&p, "o/r", Some("chat-2"));
        assert!(standing_failure(&p).is_some());

        minted(&p, "git-askpass", "o/r", Some("chat-2"));
        assert_eq!(standing_failure(&p), None);

        // And a failure after that mint stands again: this is the last thing
        // the path is known to have done.
        failed(&p, "gh-token", "o/r", Some("chat-3"), "github said 401");
        assert_eq!(standing_failure(&p).map(|e| e.event), Some(Event::Failed));
    }

    /// gh#515's second half: an operator can act on a broken shim and can only
    /// wait out a 504, so the ledger has to tell them apart.
    #[test]
    fn a_failure_knows_whether_it_was_githubs_or_this_boxs() {
        let upstream = [
            "github HTTP 504 for /repos/o/r: We couldn't respond to your request in time.",
            "github HTTP 502 for /repos/o/r: Bad gateway",
            "minting a token timed out after 30s",
            "github HTTP 403: You have exceeded a secondary rate limit",
            "503 Service Unavailable",
        ];
        for error in upstream {
            let (_tmp, p) = paths("upstream");
            failed(&p, "gh-token", "o/r", Some("c"), error);
            let entry = standing_failure(&p).unwrap();
            assert_eq!(entry.cause(), Cause::Upstream, "{error}");
        }

        let local = [
            "cannot exec the askpass shim",
            "no GitHub credential to push o/r with",
            "github HTTP 404 for /repos/o/r: Not Found",
            // A digit that is not a status, in a path that is not a failure of
            // GitHub's: nothing here may be read as a 5xx.
            "cannot exec /opt/homebrew/504/bin/gh",
        ];
        for error in local {
            let (_tmp, p) = paths("local");
            failed(&p, "gh-token", "o/r", Some("c"), error);
            let entry = standing_failure(&p).unwrap();
            assert_eq!(entry.cause(), Cause::Local, "{error}");
        }

        // A failure with no error text at all is nobody's problem in
        // particular, which means it is treated as this box's.
        let (_tmp, p) = paths("bare");
        let bare = Entry::new(Event::Unusable, "dispatch", "o/r", None);
        assert_eq!(bare.cause(), Cause::Local);
        append(&p, bare);
        assert_eq!(standing_failure(&p).unwrap().cause(), Cause::Local);
    }

    /// The stamp is what makes a failure history, so it has to be readable —
    /// and an unreadable one must never pass for fresh.
    #[test]
    fn an_entry_reports_its_own_age() {
        let (_tmp, p) = paths("age");
        failed(&p, "gh-token", "o/r", Some("c"), "boom");
        let entry = standing_failure(&p).unwrap();
        let now = entry.at_utc().unwrap();
        assert_eq!(entry.age_secs(now), Some(0));
        assert!(entry.is_fresh(now));
        assert!(entry.is_fresh(now + chrono::Duration::seconds(FRESH_SECS - 1)));
        assert!(!entry.is_fresh(now + chrono::Duration::seconds(FRESH_SECS)));
        // A clock that went backwards is zero seconds ago, not negative.
        assert_eq!(entry.age_secs(now - chrono::Duration::hours(1)), Some(0));

        let mut bent = entry.clone();
        bent.at = "not a timestamp".into();
        assert_eq!(bent.age_secs(now), None);
        assert!(!bent.is_fresh(now));

        // `what` is `summary` without the stamp, and carries the same facts.
        assert_eq!(entry.summary(), format!("{} {}", entry.at, entry.what()));
        assert!(entry.what().contains("o/r"), "{}", entry.what());
        assert!(entry.what().contains("boom"), "{}", entry.what());
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
