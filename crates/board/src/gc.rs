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
//! An attempt leaves three things behind, and gh#139 and gh#186 added the last
//! two: the checkout on the disk, the chat on its space's shelf, and the build
//! output *inside* the checkout. [`chat_standing`] and [`cache_standing`] ask
//! the same ownership question about those, and [`decide`] does not care which
//! it is aging — one rule, three windows, so a box cannot end up reclaiming the
//! work while keeping the conversation about it forever, or keeping 36 GB of
//! `target/` for the week it keeps a 14 MB checkout.
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
///
/// Applied to the *checkouts* and to the build output on separate lines since
/// gh#186 ([`Usage::checkout_bytes`], [`Usage::cache_bytes`]): the box that
/// prompted it read `109.5 GiB in worktrees` and could not tell from that line
/// that 99.96% of it was regenerable.
pub const WARN_BYTES: u64 = 20 * 1024 * 1024 * 1024;

/// Number of checkouts past which `doctor` says so, whatever they weigh. A
/// board doing its job keeps a week of them; fifty means nothing is collecting.
pub const WARN_CHECKOUTS: usize = 50;

/// How long [`usage`] may walk before it answers with what it has. `doctor` is
/// something a person runs and waits for, and a checkout tree is exactly the
/// kind of directory that takes a minute to add up.
const USAGE_BUDGET: Duration = Duration::from_secs(3);

/// Directory names that hold build output: regenerable by the tool that wrote
/// them, and the entire weight of a checkout (gh#186).
///
/// Measured on the box that prompted this: a `board-gh-161-comet-board` checkout
/// was 36 GB, of which 298 MB was the checkout and the rest was `target/`. Three
/// of those filled a 150 GB disk while two agents were building in it.
///
/// **A per-language list, deliberately, rather than a size heuristic.** "Delete
/// the biggest directory" would be right about `target/` and wrong about the one
/// repo that keeps fixtures or a dataset in the tree, and it would be wrong
/// silently. Every name here is a directory the ecosystem that creates it also
/// gitignores, and every one of them is rebuilt by running the build again:
///
/// - `target` — cargo (also maven/gradle, which are the same shape)
/// - `node_modules` — npm/pnpm/yarn, the JS repos this box also routes
/// - `.next` / `.turbo` — Next.js and Turborepo build caches, which sit beside
///   a `node_modules` and are the same kind of thing
///
/// Not here, on purpose: `dist` and `build` (some repos commit them), `.venv`
/// (an environment somebody may be *in*, not output), and anything matched by
/// pattern rather than by name.
pub const BUILD_OUTPUT_DIRS: &[&str] = &["target", "node_modules", ".next", ".turbo"];

/// How deep [`sweep_build_output`] and [`usage`] look for a
/// [`BUILD_OUTPUT_DIRS`] entry. `target/` sits at a checkout's root and a
/// monorepo's `packages/<name>/node_modules` three levels down; six is room for
/// both and a bound on the walk of a tree whose caches are already gone.
const BUILD_OUTPUT_DEPTH: usize = 6;

/// Is this directory name build output — a cache to be swept rather than a
/// checkout to be retained?
pub fn is_build_output(name: &str) -> bool {
    BUILD_OUTPUT_DIRS.contains(&name)
}

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
    /// The window has run out. Reclaim it — delete the checkout and its
    /// branch, or archive the chat off its shelf (gh#139).
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

/// Whose the *chat* of `attempt` is (gh#139) — the same question
/// [`standing`] answers about its checkout, plus the two things that are true
/// of chats and not of directories.
///
/// A chat and a checkout are the same attempt's leavings, so the rule is
/// deliberately the same rule and not a second one: live is untouchable,
/// review holds (delivery calls `chat_alive` on exactly this chat, and an
/// archived chat reads as gone), an issue still owed holds, and only a task
/// that has left the board is spent. What is added here:
///
/// - **The pinned orchestrator is never archived.** It is the board's own
///   standing agent ([`crate::config::Defaults::orchestrator_chat`]) — it
///   hears about every settle on the board, so it is *always* somebody's,
///   whatever the attempt it was once dispatched as. `Held`, not a skip, so a
///   mark left from before it was pinned is cleared.
/// - **A chatless attempt has nothing to archive.** A dispatch whose chat
///   never got recorded is `Held` for want of anything to do.
///
/// Hand-made chats need no rule: the sweep walks attempts, and a chat nobody
/// dispatched has none.
pub fn chat_standing(task: &Task, attempt: &Attempt, orchestrator: Option<&str>) -> Standing {
    let Some(chat) = attempt.pane_id.as_deref() else {
        return Standing::Held;
    };
    if orchestrator.is_some_and(|pinned| pinned == chat) {
        return Standing::Held;
    }
    standing(task, attempt)
}

/// Whose the *build output* inside an attempt's checkout is (gh#186) — the same
/// question [`standing`] answers about the checkout, asked of a cache instead of
/// evidence, and answered by fewer facts.
///
/// The two exist for different reasons, which is the whole of gh#186. The
/// checkout is retained because somebody may go back for it: review delivery
/// resumes an agent *in that directory*, a retry lands on its commits, and a
/// human looks at what last Tuesday's agent actually did. Nothing reads
/// `target/` after the run ends. It is regenerated by running the build, it is
/// two to three orders of magnitude heavier than the thing it sits inside, and
/// on a 150 GB disk a week of it does not fit.
///
/// So only one fact holds it: **is anybody building in there right now.** An
/// open attempt is, and so is a live retry — retries reuse the branch, so a
/// closed attempt's checkout is very often the live one's, and sweeping it would
/// delete a running `cargo build`'s output from under it. Everything else that
/// holds a *checkout* — an open pull request, an issue still owed, the task not
/// having left the board — is deliberately not consulted: a review comment that
/// restarts the agent rebuilds, slower, and the alternative is paying 36 GB per
/// attempt for the chance of saving one `cargo build`.
///
/// Never [`Standing::Held`]: there is no third state to be in. A cache is either
/// in use or regenerable.
pub fn cache_standing(task: &Task, attempt: &Attempt) -> Standing {
    if attempt.outcome.is_none() || task.live_attempt().is_some() {
        return Standing::Live;
    }
    Standing::Spent
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
    /// Bytes in files under the root, symlinks not followed. Everything,
    /// including [`Self::cache_bytes`].
    pub bytes: u64,
    /// Of those, the bytes inside a [`BUILD_OUTPUT_DIRS`] directory (gh#186) —
    /// the regenerable share, kept on its own much shorter clock.
    pub cache_bytes: u64,
    /// How many build-output directories those bytes are in.
    pub cache_dirs: usize,
    /// The walk ran out of [`USAGE_BUDGET`] — every byte count is a floor, not
    /// a total.
    pub truncated: bool,
}

impl Usage {
    /// The bytes `retain_worktrees` governs: everything that is not build
    /// output. This is the number gh#186 is about — 14 MB per checkout against
    /// the 36 GB the same directory reported before the split.
    pub fn checkout_bytes(&self) -> u64 {
        self.bytes.saturating_sub(self.cache_bytes)
    }

    /// Whether the *checkouts* are worth flagging: too heavy, or too many.
    ///
    /// Measured on [`Self::checkout_bytes`] since gh#186, not on the total. The
    /// build output has its own clock and its own `doctor` line now, and a box
    /// mid-build is *supposed* to have tens of gibibytes of it — a check that
    /// went red for that would go red daily on a working box and stop meaning
    /// anything, which is what the line it replaced had started doing.
    pub fn alarming(&self) -> bool {
        self.checkout_bytes() >= WARN_BYTES || self.checkouts >= WARN_CHECKOUTS
    }
}

/// What one [`sweep_build_output`] removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Swept {
    /// Build-output directories deleted.
    pub dirs: usize,
    /// Bytes they held, measured just before the delete.
    pub bytes: u64,
    /// Directories that would not delete, as `path: error`. The caller keeps
    /// the attempt unstamped and tries again next cycle.
    pub failed: Vec<String>,
}

/// Delete every [`BUILD_OUTPUT_DIRS`] directory inside one checkout (gh#186),
/// and say what that was worth.
///
/// The whole checkout stays: this removes the cache from *inside* it, which is
/// why it is not [`crate::runtime::Runtime::reclaim_worktree`] and why the
/// attempt row must not be stamped `collected_at` for it — the worktree is still
/// there, still on its branch, still the directory review delivery would resume
/// an agent in.
///
/// Three refusals in the walk:
///
/// - **`.git` is never entered.** A checkout's git dir is not build output and
///   nothing under it is a candidate; the depth bound alone would not say so.
/// - **Symlinks are never followed** (nor deleted as directories): a link's
///   target can be outside the checkout, and `node_modules` in particular is
///   full of them. `remove_dir_all` on a symlinked directory would take
///   somebody else's tree with it.
/// - **A matched directory is not descended into** — it is measured, deleted,
///   and done. Nested `node_modules` inside a `node_modules` go with the parent.
///
/// Best-effort, and idempotent: a checkout that is already gone, or one that was
/// never built in, is an empty [`Swept`]. A delete that fails lands in
/// [`Swept::failed`] rather than aborting the rest — one unreadable directory
/// must not keep the other 30 GB.
pub fn sweep_build_output(checkout: &Path) -> Swept {
    let mut swept = Swept::default();
    sweep_dir(checkout, 0, &mut swept);
    swept
}

fn sweep_dir(dir: &Path, depth: usize, out: &mut Swept) {
    if depth > BUILD_OUTPUT_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        // `file_type` on a `DirEntry` does not follow the link, so a symlinked
        // directory reads as neither dir nor file here and is skipped whole.
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        if is_build_output(&name) {
            // Measured first: `remove_dir_all` has nothing to report, and the
            // number is the point of the log line it ends up in.
            let bytes = tree_bytes(&path);
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    out.dirs += 1;
                    out.bytes += bytes;
                }
                Err(e) => out.failed.push(format!("{}: {e}", path.display())),
            }
            continue;
        }
        sweep_dir(&path, depth + 1, out);
    }
}

/// Every byte under `dir`, symlinks counted as the links they are. Unbudgeted,
/// unlike [`usage`]: this runs to name what a delete on the next line reclaimed,
/// and the delete is walking the same tree anyway.
fn tree_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut bytes = 0;
    for entry in entries.flatten() {
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if meta.is_dir() {
            bytes += tree_bytes(&entry.path());
        } else {
            bytes += meta.len();
        }
    }
    bytes
}

/// Count and weigh the checkouts under the engine's worktree root, splitting
/// what is a checkout from what is build output (gh#186).
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
            add_tree(&checkout.path(), Site::default(), deadline, &mut u);
        }
    }
    u
}

/// Sum one checkout's files, stopping at `deadline`. Symlinks are counted as
/// the links they are (`symlink_metadata`), never followed — a checkout with a
/// symlink to `/` must not turn the report into a filesystem walk.
///
/// [`Site::in_cache`] is set on the way into a [`BUILD_OUTPUT_DIRS`] directory
/// and stays set below it, so everything a sweep would delete is counted as
/// cache and everything else is counted as checkout. Every condition it applies
/// is [`sweep_build_output`]'s — the depth bound, and `.git` being off limits —
/// because what the report calls regenerable has to be exactly what the sweep
/// would take.
fn add_tree(dir: &Path, site: Site, deadline: Instant, u: &mut Usage) {
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
            let below = site.enter(&entry.file_name().to_string_lossy());
            if below.in_cache && !site.in_cache {
                u.cache_dirs += 1;
            }
            add_tree(&entry.path(), below, deadline, u);
            if u.truncated {
                return;
            }
        } else {
            u.bytes += meta.len();
            if site.in_cache {
                u.cache_bytes += meta.len();
            }
        }
    }
}

/// Where in a checkout [`add_tree`] is, in the only two terms that change what a
/// byte counts as.
#[derive(Debug, Clone, Copy, Default)]
struct Site {
    depth: usize,
    /// Inside a build-output directory: these bytes are the cache.
    in_cache: bool,
    /// Inside the git dir, where nothing is build output however it is named. A
    /// submodule's `.git/modules/target` is git's, and the sweep does not enter
    /// it either.
    in_git: bool,
}

impl Site {
    fn enter(self, name: &str) -> Site {
        Site {
            depth: self.depth + 1,
            in_cache: self.in_cache
                || (!self.in_git && self.depth <= BUILD_OUTPUT_DEPTH && is_build_output(name)),
            in_git: self.in_git || name == ".git",
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
            dispatched_by_device: None,
            dispatched_by_user: None,
            dispatched_by_verified: false,
            billed_to: None,
            collectable_at: None,
            collected_at: None,
            chat_archivable_at: None,
            chat_archived_at: None,
            cache_sweepable_at: None,
            cache_swept_at: None,
            tokens: None,
            model: None,
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

    // ---- the chat's standing (gh#139) ------------------------------------

    /// A chat is archived on exactly the terms its checkout is collected on.
    /// Stated as a test because the two are configured apart, and a board that
    /// reclaimed the work while keeping every conversation about it forever
    /// would be tidying half the mess.
    #[test]
    fn a_chat_is_owed_for_as_long_as_its_checkout_is() {
        for upstream in [
            UpstreamState::Unstarted,
            UpstreamState::Started,
            UpstreamState::Terminal,
            UpstreamState::Gone,
        ] {
            for outcome in [None, Some(Outcome::Done), Some(Outcome::Failed)] {
                for pr_open in [false, true] {
                    let mut t = task(upstream);
                    t.pr_open = pr_open;
                    t.attempts = vec![attempt(1, outcome)];
                    assert_eq!(
                        chat_standing(&t, &t.attempts[0], None),
                        standing(&t, &t.attempts[0]),
                        "{upstream:?} {outcome:?} pr_open={pr_open}"
                    );
                }
            }
        }
    }

    /// Review delivery asks `chat_alive` about this exact chat, and an archived
    /// chat answers no. Archiving one in review would break the loop that
    /// carries a human's comments to the agent that wrote the pull request —
    /// silently, for exactly the tasks somebody is still working on.
    #[test]
    fn a_chat_in_review_is_never_archived() {
        let mut t = task(UpstreamState::Terminal);
        t.pr_open = true;
        t.attempts = vec![attempt(1, Some(Outcome::Done))];
        assert_eq!(chat_standing(&t, &t.attempts[0], None), Standing::Held);
    }

    /// A blocked agent is a live attempt — outcome still open — so the chat
    /// where it stopped to ask is the one chat it would be worst to file away.
    #[test]
    fn a_blocked_attempts_chat_is_never_archived() {
        let mut t = task(UpstreamState::Terminal);
        t.attempts = vec![attempt(1, None)];
        assert_eq!(chat_standing(&t, &t.attempts[0], None), Standing::Live);
    }

    /// The pinned orchestrator hears about every settle on the board, so it is
    /// never finished — whatever the attempt it was once dispatched as. `Held`
    /// rather than skipped, so a clock started before it was pinned stops.
    #[test]
    fn the_pinned_orchestrator_is_never_archived() {
        let mut t = task(UpstreamState::Terminal);
        t.attempts = vec![attempt(1, Some(Outcome::Done))];
        assert_eq!(
            chat_standing(&t, &t.attempts[0], Some("chat-1")),
            Standing::Held
        );
        // Somebody else's pin leaves this chat exactly as spent as it was.
        assert_eq!(
            chat_standing(&t, &t.attempts[0], Some("chat-other")),
            Standing::Spent
        );
    }

    /// A dispatch whose chat was never recorded has nothing to archive, and
    /// must not read as spent — there is no id to send anywhere.
    #[test]
    fn an_attempt_with_no_chat_has_nothing_to_archive() {
        let mut t = task(UpstreamState::Terminal);
        let mut a = attempt(1, Some(Outcome::Done));
        a.pane_id = None;
        t.attempts = vec![a];
        assert_eq!(chat_standing(&t, &t.attempts[0], None), Standing::Held);
    }

    // ---- the build output's standing (gh#186) -----------------------------

    /// The whole of gh#186 as one assertion: a pull request in review holds the
    /// *checkout* — review delivery resumes an agent in that directory — and
    /// holds nothing at all inside it. That agent rebuilds; the alternative is
    /// 36 GB per attempt against the chance of saving one `cargo build`.
    #[test]
    fn review_holds_the_checkout_and_not_the_cache_inside_it() {
        let mut t = task(UpstreamState::Started);
        t.pr_open = true;
        t.attempts = vec![attempt(1, Some(Outcome::Done))];
        assert_eq!(standing(&t, &t.attempts[0]), Standing::Held);
        assert_eq!(cache_standing(&t, &t.attempts[0]), Standing::Spent);
    }

    /// Nor does an issue still owed, or a task that has simply not left the
    /// board: none of the reasons to keep a 14 MB checkout is a reason to keep a
    /// 36 GB cache, and the clock starts when the *attempt* ends.
    #[test]
    fn nothing_but_a_build_in_progress_holds_the_cache() {
        for upstream in [
            UpstreamState::Unstarted,
            UpstreamState::Started,
            UpstreamState::Terminal,
            UpstreamState::Gone,
        ] {
            for pr_open in [false, true] {
                for outcome in [
                    Outcome::Done,
                    Outcome::Failed,
                    Outcome::Cancelled,
                    Outcome::Orphaned,
                ] {
                    let mut t = task(upstream);
                    t.pr_open = pr_open;
                    t.attempts = vec![attempt(1, Some(outcome))];
                    assert_eq!(
                        cache_standing(&t, &t.attempts[0]),
                        Standing::Spent,
                        "{upstream:?} pr_open={pr_open} {outcome:?}"
                    );
                }
            }
        }
    }

    /// The one guard, in both its forms: the attempt itself still running, and a
    /// live retry that reuses the branch and therefore the same directory.
    /// Sweeping either would delete a running build's output from under it.
    #[test]
    fn a_build_in_progress_is_untouchable() {
        let mut t = task(UpstreamState::Started);
        t.attempts = vec![attempt(1, None)];
        assert_eq!(cache_standing(&t, &t.attempts[0]), Standing::Live);

        let mut t = task(UpstreamState::Terminal);
        t.attempts = vec![attempt(1, Some(Outcome::Failed)), attempt(2, None)];
        assert_eq!(cache_standing(&t, &t.attempts[0]), Standing::Live);
    }

    /// `on-settle` is the default window, so the arithmetic that matters is
    /// `decide`'s at zero: one cycle to mark, the next to sweep. The same
    /// function ages all three windows — a cache is not a fourth rule.
    #[test]
    fn a_cache_with_no_window_is_marked_then_swept() {
        assert_eq!(decide(Standing::Spent, None, 0), Verdict::Mark);
        assert_eq!(decide(Standing::Spent, Some(0), 0), Verdict::Collect);
        // And a build starting again stops the clock, whatever the window.
        assert_eq!(decide(Standing::Live, Some(0), 0), Verdict::Unmark);
    }

    // ---- sweeping it (gh#186) ---------------------------------------------

    /// A checkout laid out the way the box's are: source, a `target/` that is
    /// most of the weight, a nested `node_modules`, and a `.git` that is neither.
    fn built_checkout() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (path, bytes) in [
            ("src/main.rs", 1024),
            ("target/debug/binary", 8192),
            ("target/debug/deps/lib.rlib", 4096),
            ("web/node_modules/react/index.js", 2048),
            (".git/objects/ab/cdef", 512),
            // A directory named like build output *inside* the git dir, to prove
            // the walk never enters it.
            (".git/modules/target/x", 64),
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, vec![b'x'; bytes]).unwrap();
        }
        dir
    }

    #[test]
    fn the_sweep_takes_the_build_output_and_leaves_the_checkout() {
        let dir = built_checkout();
        let root = dir.path();
        let swept = sweep_build_output(root);

        assert_eq!(swept.dirs, 2, "target/ and the nested node_modules");
        assert_eq!(swept.bytes, 8192 + 4096 + 2048);
        assert!(swept.failed.is_empty());
        assert!(!root.join("target").exists());
        assert!(!root.join("web").join("node_modules").exists());
        // What the attempt is retained *for* is all still there.
        assert!(root.join("src").join("main.rs").exists());
        assert!(root.join(".git").join("modules").join("target").exists());
        assert!(root.join("web").exists(), "the package dir stays");

        // Idempotent: a second sweep of a swept checkout finds nothing, and one
        // of a checkout that has been reclaimed whole is not an error.
        assert_eq!(sweep_build_output(root), Swept::default());
        assert_eq!(
            sweep_build_output(&root.join("gone")),
            Swept::default(),
            "a checkout that is not there sweeps nothing"
        );
    }

    /// `node_modules` is full of symlinks and a checkout may hold one to
    /// anywhere. Following one would hand `remove_dir_all` somebody else's tree
    /// — a shared cargo cache, a sibling checkout, `/`.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_cache_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let elsewhere = root.join("somebody-elses");
        std::fs::create_dir_all(elsewhere.join("deep")).unwrap();
        std::fs::write(elsewhere.join("deep").join("f"), vec![b'x'; 128]).unwrap();
        let checkout = root.join("checkout");
        std::fs::create_dir_all(&checkout).unwrap();
        std::os::unix::fs::symlink(&elsewhere, checkout.join("target")).unwrap();

        let swept = sweep_build_output(&checkout);
        assert_eq!(
            swept,
            Swept::default(),
            "a link is not a directory to sweep"
        );
        assert!(elsewhere.join("deep").join("f").exists());
    }

    /// What `doctor` calls regenerable has to be exactly what the sweep would
    /// take, or the report is describing a different decision than the one the
    /// board makes.
    #[test]
    fn usage_attributes_the_cache_to_the_same_directories_the_sweep_takes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let checkout = root.join("comet-board").join("board-gh-186-comet-board");
        std::fs::create_dir_all(&checkout).unwrap();
        let built = built_checkout();
        for entry in ["src", "target", "web", ".git"] {
            copy_tree(&built.path().join(entry), &checkout.join(entry));
        }

        let before = usage(root);
        assert_eq!(before.checkouts, 1);
        assert_eq!(before.cache_dirs, 2);
        assert_eq!(before.cache_bytes, 8192 + 4096 + 2048);
        assert_eq!(before.bytes, before.cache_bytes + 1024 + 512 + 64);
        // The number `retain_worktrees` actually governs — 14 MB against 36 GB
        // on the box, and the line that was missing from `doctor`.
        assert_eq!(before.checkout_bytes(), 1024 + 512 + 64);

        let swept = sweep_build_output(&checkout);
        let after = usage(root);
        assert_eq!(swept.bytes, before.cache_bytes);
        assert_eq!(after.cache_bytes, 0);
        assert_eq!(after.cache_dirs, 0);
        assert_eq!(after.bytes, before.checkout_bytes());
        // The checkout is still a checkout: one directory, still counted.
        assert_eq!(after.checkouts, 1);
    }

    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    /// A cache is only a cache while it is where the build put it. Past the
    /// depth bound the walk stops, and the report and the sweep stop together —
    /// they must not disagree about what is regenerable.
    #[test]
    fn the_report_and_the_sweep_share_one_depth_bound() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("repo").join("checkout");
        let deep =
            (0..=BUILD_OUTPUT_DEPTH + 1).fold(checkout.clone(), |p, i| p.join(format!("d{i}")));
        std::fs::create_dir_all(deep.join("target")).unwrap();
        std::fs::write(deep.join("target").join("f"), vec![b'x'; 256]).unwrap();

        assert_eq!(sweep_build_output(&checkout), Swept::default());
        let u = usage(dir.path());
        assert_eq!(u.cache_bytes, 0, "not counted as cache either");
        assert_eq!(u.bytes, 256, "still counted as bytes on the disk");
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
