//! Reclaiming attempt checkouts (gh#72): when a worktree and its branch stop
//! being anybody's, and what the box looks like before they do.
//!
//! Every dispatch cuts a checkout under the engine's worktree root plus a local
//! branch, and until this landed nothing ever removed either: settle, orphan,
//! cancel and retry-replace all close the attempt row and walk away, so an
//! always-on box accumulated one full checkout per attempt forever. herdr-board
//! had a `gc` for this; the port deliberately left it behind (`docs/BOARD.md`)
//! and nothing took its place.
//!
//! Two decisions, kept pure so the arithmetic is testable without a database, a
//! checkout, or an engine:
//!
//! - [`standing`] — whose the checkout is: a live attempt's, a task that is not
//!   finished with it, or nobody's.
//! - [`decide`] — what to do about it now, given how long it has been nobody's.
//!
//! The discipline is the duration cap's ([`crate::overrun`]): the sweep rides
//! the interval sync, so the retention window is aged in wall time and a burst
//! of watch events cannot run the clock faster than the clock.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::model::{Attempt, Task};

/// Total checkout bytes past which `doctor` calls the worktree root a problem.
///
/// Not a policy the board enforces — nothing deletes on size — only the point
/// at which "you have a lot of checkouts" is worth saying out loud. Twenty
/// gibibytes is a handful of node_modules-shaped repos, which is exactly the
/// shape of box this bites first.
pub const WARN_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Number of checkouts past which `doctor` says so, whatever they weigh. A
/// board doing its job keeps a week of them; fifty means nothing is collecting.
pub const WARN_CHECKOUTS: usize = 50;

/// How long [`usage`] may walk before it answers with what it has. `doctor` is
/// something a person runs and waits for, and a checkout tree is exactly the
/// kind of directory that takes a minute to add up.
const USAGE_BUDGET: Duration = Duration::from_secs(3);

/// Whose an attempt's checkout is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// An agent may be in it right now. Untouchable, whatever else is true.
    Live,
    /// Closed, but the work is not finished with: the task is still open
    /// upstream, or its pull request is still in review. A retry reuses this
    /// exact branch and lands on the previous attempt's commits, so deleting it
    /// here would throw away the work the retry is meant to continue.
    Held,
    /// Closed, and the task has left the board — merged, closed upstream, or
    /// marked done. Nobody is coming back for it.
    Spent,
}

/// What to do about one closed attempt's checkout this cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing. The overwhelming case, and the one that must cost nothing.
    Keep,
    /// It became collectable just now — start the retention clock.
    Mark,
    /// It was collectable and is not any more: the issue reopened, a retry was
    /// dispatched, a pull request went back into review. Stop the clock, so a
    /// task that finishes for good next month gets its full window then.
    Unmark,
    /// The window has run out. Delete the checkout and the branch.
    Collect,
}

/// Whose the checkout of `attempt` is, given the task it belongs to.
///
/// Read off the facts rather than off `task.state`: the rendered state is
/// derived, and a sweep that keyed on it would depend on having re-derived
/// first. Upstream-final (closed or deleted) or an operator's `mark done` is
/// what "left the board" means, and an open pull request outranks both — review
/// delivery still compares a chat's cwd against this checkout
/// ([`crate::review`]), and a reviewer may yet check the branch out by hand.
pub fn standing(task: &Task, attempt: &Attempt) -> Standing {
    // Retries reuse the branch, and git allows a branch in one worktree only —
    // so a task's live attempt is very likely sitting in *this* directory. Any
    // live attempt on the task makes every one of its checkouts untouchable.
    if attempt.outcome.is_none() || task.live_attempt().is_some() {
        return Standing::Live;
    }
    if task.pr_open {
        return Standing::Held;
    }
    if task.upstream.is_final() || task.local_done {
        return Standing::Spent;
    }
    Standing::Held
}

/// What to do about a checkout that has been [`Standing::Spent`] for
/// `spent_secs` (`None` = not marked yet), under a `retain_secs` window.
pub fn decide(standing: Standing, spent_secs: Option<i64>, retain_secs: u64) -> Verdict {
    if standing != Standing::Spent {
        // Still somebody's. A mark left from before belongs to a task that has
        // since come back to life, and the clock has to stop with it.
        return if spent_secs.is_some() {
            Verdict::Unmark
        } else {
            Verdict::Keep
        };
    }
    match spent_secs {
        None => Verdict::Mark,
        Some(secs) if secs >= retain_secs as i64 => Verdict::Collect,
        Some(_) => Verdict::Keep,
    }
}

/// What the worktree root holds — `doctor`'s view of the box before it hurts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Checkout directories: `{root}/<repo>/<checkout>`, the shape both
    /// `create_worktree` and `create_worktree_on` write.
    pub checkouts: usize,
    /// Bytes in files under the root, symlinks not followed.
    pub bytes: u64,
    /// The walk ran out of [`USAGE_BUDGET`] — `bytes` is a floor, not a total.
    pub truncated: bool,
}

impl Usage {
    /// Whether this is worth flagging: too big, or too many.
    pub fn alarming(&self) -> bool {
        self.bytes >= WARN_BYTES || self.checkouts >= WARN_CHECKOUTS
    }
}

/// Count and weigh the checkouts under the engine's worktree root.
///
/// Bounded by [`USAGE_BUDGET`] rather than exhaustive: a report that hangs for
/// a minute on somebody's twelve node_modules is a report nobody runs. On
/// expiry the totals so far are returned with `truncated` set, and the caller
/// says `≥` instead of `=`. A root that does not exist yet is an empty
/// [`Usage`], not an error — no dispatch has cut a worktree on this box.
pub fn usage(root: &Path) -> Usage {
    let mut u = Usage::default();
    let deadline = Instant::now() + USAGE_BUDGET;
    let Ok(repos) = std::fs::read_dir(root) else {
        return u;
    };
    for repo in repos.flatten() {
        let Ok(checkouts) = std::fs::read_dir(repo.path()) else {
            continue;
        };
        for checkout in checkouts.flatten() {
            if !checkout.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            u.checkouts += 1;
            add_tree(&checkout.path(), deadline, &mut u);
        }
    }
    u
}

/// Sum one checkout's files, stopping at `deadline`. Symlinks are counted as
/// the links they are (`symlink_metadata`), never followed — a checkout with a
/// symlink to `/` must not turn the report into a filesystem walk.
fn add_tree(dir: &Path, deadline: Instant, u: &mut Usage) {
    if Instant::now() >= deadline {
        u.truncated = true;
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if meta.is_dir() {
            add_tree(&entry.path(), deadline, u);
            if u.truncated {
                return;
            }
        } else {
            u.bytes += meta.len();
        }
    }
}

/// A retention window written the way `routing.toml` spells it: `7d`, `36h`,
/// `90m`. Not [`crate::overrun::human_secs`], which tops out at hours because a
/// duration cap does — a week reported as `168h` is a number nobody set.
pub fn human_window(secs: u64) -> String {
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if secs >= size && secs % size == 0 {
            return format!("{}{unit}", secs / size);
        }
    }
    format!("{secs}s")
}

/// Bytes as an operator reads them: `4.2 GiB`, `860 MiB`, `12 KiB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Outcome, Source, UpstreamState};
    use comet_proto::view::board::BoardState;

    const WEEK: u64 = 7 * 86_400;

    fn task(upstream: UpstreamState) -> Task {
        Task {
            id: "gh:o/r#7".into(),
            source: Source::Github,
            source_id: "7".into(),
            identifier: "gh#7".into(),
            title: "t".into(),
            body: None,
            url: String::new(),
            labels: Vec::new(),
            state: BoardState::Ready,
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream,
            local_done: false,
            pr_url: None,
            pr_number: None,
            pr_open: false,
            pr_merged: false,
            pr_mergeable: None,
            updated_at: String::new(),
            synced_at: String::new(),
            attempts: Vec::new(),
        }
    }

    fn attempt(id: i64, outcome: Option<Outcome>) -> Attempt {
        Attempt {
            id,
            task_id: "gh:o/r#7".into(),
            pane_id: Some("chat-1".into()),
            workspace: "ws".into(),
            runtime: "mock".into(),
            worktree: Some("/wt/board-gh-7-r".into()),
            repo_path: Some("/repo/r".into()),
            branch: Some("board/gh-7-r".into()),
            started_at: "2026-01-01T00:00:00Z".into(),
            ended_at: outcome.is_some().then(|| "2026-01-02T00:00:00Z".into()),
            outcome,
            account: None,
            missing_ticks: 0,
            agent_status: None,
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
            saw_working: true,
            settled_at: None,
            reopened: 0,
            screen_print: None,
            screen_at: None,
            nudges: 0,
            nudged_at: None,
            blocked_count: 0,
            overrun_warned_at: None,
            collectable_at: None,
            collected_at: None,
        }
    }

    /// The exit criterion's other half: a running agent's checkout is never
    /// touched, however finished the task looks from upstream.
    #[test]
    fn a_live_attempt_is_untouchable() {
        let mut t = task(UpstreamState::Terminal);
        t.attempts = vec![attempt(1, None)];
        assert_eq!(standing(&t, &t.attempts[0]), Standing::Live);
    }

    /// A retry lands on the previous attempt's commits in the same branch, so
    /// the closed attempt's checkout is the live one's checkout.
    #[test]
    fn a_closed_attempt_beside_a_live_retry_is_untouchable() {
        let mut t = task(UpstreamState::Terminal);
        t.attempts = vec![attempt(1, Some(Outcome::Failed)), attempt(2, None)];
        assert_eq!(standing(&t, &t.attempts[0]), Standing::Live);
    }

    #[test]
    fn review_holds_the_checkout() {
        let mut t = task(UpstreamState::Terminal);
        t.pr_open = true;
        t.attempts = vec![attempt(1, Some(Outcome::Done))];
        assert_eq!(standing(&t, &t.attempts[0]), Standing::Held);
    }

    /// The retry case stated as a rule: a failed attempt on an issue that is
    /// still open keeps its checkout, because that is what the retry continues.
    #[test]
    fn a_failed_attempt_on_an_open_issue_keeps_its_checkout() {
        let mut t = task(UpstreamState::Started);
        t.attempts = vec![attempt(1, Some(Outcome::Failed))];
        assert_eq!(standing(&t, &t.attempts[0]), Standing::Held);
    }

    #[test]
    fn a_closed_issue_with_no_open_pull_request_is_spent() {
        for outcome in [
            Outcome::Done,
            Outcome::Failed,
            Outcome::Cancelled,
            Outcome::Orphaned,
        ] {
            let mut t = task(UpstreamState::Terminal);
            t.attempts = vec![attempt(1, Some(outcome))];
            assert_eq!(standing(&t, &t.attempts[0]), Standing::Spent, "{outcome:?}");
        }
        // Deleted upstream counts too — `gone` is final.
        let mut t = task(UpstreamState::Gone);
        t.attempts = vec![attempt(1, Some(Outcome::Done))];
        assert_eq!(standing(&t, &t.attempts[0]), Standing::Spent);
        // And so does the operator saying so on an issue still open upstream.
        let mut t = task(UpstreamState::Started);
        t.local_done = true;
        t.attempts = vec![attempt(1, Some(Outcome::Done))];
        assert_eq!(standing(&t, &t.attempts[0]), Standing::Spent);
    }

    #[test]
    fn the_window_is_measured_from_the_mark_not_from_the_attempt() {
        assert_eq!(decide(Standing::Spent, None, WEEK), Verdict::Mark);
        assert_eq!(
            decide(Standing::Spent, Some(WEEK as i64 - 1), WEEK),
            Verdict::Keep
        );
        assert_eq!(
            decide(Standing::Spent, Some(WEEK as i64), WEEK),
            Verdict::Collect
        );
    }

    /// A checkout that comes back to life stops the clock, and the next time it
    /// is spent it gets the whole window again — a task reopened on day six
    /// must not be collected on day seven.
    #[test]
    fn coming_back_to_life_stops_the_clock() {
        assert_eq!(
            decide(Standing::Live, Some(WEEK as i64), WEEK),
            Verdict::Unmark
        );
        assert_eq!(decide(Standing::Held, Some(1), WEEK), Verdict::Unmark);
        // Nothing to undo, nothing to do.
        assert_eq!(decide(Standing::Live, None, WEEK), Verdict::Keep);
        assert_eq!(decide(Standing::Held, None, WEEK), Verdict::Keep);
    }

    #[test]
    fn usage_counts_checkouts_and_weighs_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (repo, checkout) in [
            ("widget", "board-gh-7-widget"),
            ("widget", "comet/x"),
            ("api", "board-lin-1-api"),
        ] {
            let path = root.join(repo).join(checkout.replace('/', "-"));
            std::fs::create_dir_all(path.join("src")).unwrap();
            std::fs::write(path.join("src").join("main.rs"), vec![b'x'; 1024]).unwrap();
        }
        let u = usage(root);
        assert_eq!(u.checkouts, 3);
        assert_eq!(u.bytes, 3 * 1024);
        assert!(!u.truncated);
        // A root nothing has cut into yet is empty, not an error.
        assert_eq!(usage(&root.join("nope")), Usage::default());
    }

    #[test]
    fn a_window_is_reported_in_the_unit_it_was_written_in() {
        assert_eq!(human_window(WEEK), "7d");
        assert_eq!(human_window(36 * 3_600), "36h");
        assert_eq!(human_window(90 * 60), "90m");
        assert_eq!(human_window(45), "45s");
        // 25h is not a whole number of days, and saying `1d` would be a lie.
        assert_eq!(human_window(25 * 3_600), "25h");
    }

    #[test]
    fn bytes_read_the_way_an_operator_says_them() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(900), "900 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(4_509_715_660), "4.2 GiB");
    }
}
