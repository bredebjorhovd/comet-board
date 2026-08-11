//! Surviving the parent's merge (gh#286): what happens to a stacked child on
//! this box when GitHub rebases and retargets it on the server.
//!
//! 4/9 (gh#285) gave a dispatch a parent: `attempts.stacked_on` records the
//! attempt whose branch this one was cut from. That edge is a fact about the
//! box. When the parent's pull request lands, GitHub rebases every layer above
//! it onto the new base and retargets the requests — **server-side**, in a
//! repository this box only polls. Nothing here moves. Three things then stop
//! being true at once, and this module is the arithmetic for all three.
//!
//! - **The stamped base.** `attempts.base_sha` is the commit the checkout was
//!   cut from — the parent's old tip — and everything the board measures off a
//!   branch is measured from it: whether the agent produced anything
//!   ([`crate::sync::SyncEngine::attempt_has_commits`]), what the branch
//!   changed, what the claims are checked against. It stays exactly right for
//!   as long as the local branch does, and it stops being a base at all the
//!   moment anything rebases that branch — a diff against a commit that is no
//!   longer on the branch is the *parent's* work plus the child's, attributed
//!   to the child. [`footing`] is the check, and it is deliberately a question
//!   about the checkout rather than about the poll: see below.
//! - **The copy on origin.** GitHub rewrote it; this checkout did not. The two
//!   have diverged ([`against_origin`]), and the next `git push` out of that
//!   checkout is a force-push over GitHub's rebase. The board cannot prevent
//!   that, so it says so — once, to the agent standing in the checkout, in the
//!   words that fix it ([`rewritten_notice`]).
//! - **The parent's local branch.** GC deletes a checkout *and its branch*
//!   ([`crate::gc`]), and a merged parent goes `Spent` while the child cut from
//!   it is still alive. [`Dependents`] is the edge read the other way round, so
//!   the deletion defers the way an open pull request already makes it defer.
//!
//! ## The box does not rewrite an attempt's branch
//!
//! The tempting answer to the second one is to fetch and rebase the child
//! worktree whenever no agent is standing in it. This does not do that, and the
//! reason is what "standing in it" can actually be known to mean: the strongest
//! signal the board has is [`crate::review::still_the_authors_checkout`], which
//! asks whether a *chat's cwd* is that directory. A chat whose run ended still
//! answers yes, a human with the checkout open answers nothing at all, and a
//! rebase that stops on a conflict leaves a half-rebased worktree — with the
//! agent's commits reachable only from `ORIG_HEAD` — for whoever opens it next.
//!
//! So the board keeps its hands off the history and spends what it knows on
//! telling: the agent gets the two commands, the log line names the branch, and
//! the measurement re-foots itself from the checkout the moment somebody *does*
//! rebase. Re-stamping is on that evidence and never on the poll, which is the
//! other half of the same decision — a retarget seen on GitHub says nothing
//! about where the local branch starts, and a stamp moved on the strength of it
//! would describe a rebase that has not happened.

use std::collections::HashMap;

use crate::model::{Attempt, Task};

/// The stacking edge read downwards: which attempts are still building on
/// another attempt's branch.
///
/// [`Attempt::stacked_on`] points up — a child names the parent it was cut
/// from — because that is what the dispatch knows. Every question asked *of* a
/// parent needs the other direction, and there is no amount of looking at one
/// attempt that finds its dependents: like [`crate::stacks::Stacks`], this is
/// built once from the whole board and then answered from.
///
/// What counts as still building on it is deliberately the checkout and not the
/// run: a child whose attempt closed an hour ago keeps its worktree for the
/// retention window, a reviewer may check its branch out, and a retry lands on
/// its commits. All of that reads history the parent's branch is the only
/// remaining name for. So the hold lasts until the child's *own* checkout is
/// reclaimed — at which point the parent's branch is nobody's and can go with
/// the next sweep. Chains unwind top-down and cannot deadlock: each layer's own
/// standing is what frees it, and freeing it frees the one below.
#[derive(Debug, Default, Clone)]
pub struct Dependents {
    holders: HashMap<i64, Vec<i64>>,
}

impl Dependents {
    /// Build the map from one board's worth of tasks.
    pub fn of(tasks: &[Task]) -> Self {
        let mut holders: HashMap<i64, Vec<i64>> = HashMap::new();
        for attempt in tasks.iter().flat_map(|t| t.attempts.iter()) {
            let Some(parent) = attempt.stacked_on else {
                continue;
            };
            if !holds_its_parent(attempt) {
                continue;
            }
            holders.entry(parent).or_default().push(attempt.id);
        }
        for ids in holders.values_mut() {
            ids.sort_unstable();
        }
        Self { holders }
    }

    /// Is anybody still building on this attempt's branch?
    pub fn holds(&self, attempt: i64) -> bool {
        self.holders.contains_key(&attempt)
    }

    /// Which attempts they are — for the log line that says why a checkout is
    /// being kept, which is the only reason anybody needs the ids.
    pub fn holders(&self, attempt: i64) -> &[i64] {
        self.holders.get(&attempt).map_or(&[], Vec::as_slice)
    }
}

/// Does this child still hold the branch it was cut from? A checkout it no
/// longer has — never recorded, or already reclaimed — holds nothing.
fn holds_its_parent(child: &Attempt) -> bool {
    child.worktree.is_some() && child.collected_at.is_none()
}

/// What the base commit stamped on an attempt is still worth, measured against
/// the checkout it was stamped from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Footing {
    /// The stamp is still an ancestor of HEAD: the branch starts where the
    /// dispatch said it did, and every measurement taken from it holds. The
    /// ordinary answer, including for every attempt that was never stacked.
    Stands,
    /// The history was rewritten under the stamp, and the checkout can name
    /// where the branch starts now. Measure from there — and re-stamp the row,
    /// because this is evidence read off the checkout rather than news read off
    /// a poll.
    Refooted(String),
    /// Rewritten, and nothing in the checkout names a new starting point: no
    /// base branch known, or no remote-tracking ref to take a merge base
    /// against. Measure nothing from the stamp and say so — a review that
    /// silently used it would attribute the layer below to this attempt.
    Adrift,
}

/// Read the stamp's footing off two facts from the checkout: whether the
/// stamped commit is still an ancestor of HEAD, and where git says the branch
/// forks from its current base.
///
/// A fork point equal to the stamp is [`Footing::Stands`] however the ancestry
/// check answered — the two agree about where the branch starts, so there is
/// nothing to re-stamp and no reason to log a rewrite that did not happen.
pub fn footing(stamp: &str, on_head: bool, fork: Option<&str>) -> Footing {
    if on_head {
        return Footing::Stands;
    }
    match fork {
        Some(fork) if fork == stamp => Footing::Stands,
        Some(fork) => Footing::Refooted(fork.to_string()),
        None => Footing::Adrift,
    }
}

/// Where the copy of an attempt's branch on origin stands against the checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remote {
    /// One contains the other: the checkout is behind, level, or holds
    /// unpushed commits on top. Every ordinary state, and none of them is this
    /// module's business — an unpushed branch is [`crate::settled`]'s question.
    InStep,
    /// Neither contains the other. Something rewrote the branch on origin; a
    /// stack whose lower layer has landed is the ordinary way that happens,
    /// and the next push out of this checkout would be a force-push over it.
    Rewritten,
}

/// Compare the checkout's HEAD with the head origin reports, asking `ancestor`
/// (`git merge-base --is-ancestor a b`) only when the two shas differ.
///
/// Takes the ancestry test as a closure because the shape of the answer is the
/// whole decision and the git calls are not: the caller supplies a checkout,
/// and a test supplies a graph.
pub fn against_origin(local: &str, remote: &str, ancestor: impl Fn(&str, &str) -> bool) -> Remote {
    if local == remote {
        return Remote::InStep;
    }
    if ancestor(local, remote) || ancestor(remote, local) {
        return Remote::InStep;
    }
    Remote::Rewritten
}

/// Has GitHub moved this child out from under the layer it was cut from?
///
/// Two spellings of one event, and either is enough: the parent's pull request
/// is merged, or the child's own request no longer targets the parent's branch.
/// The second catches the case the first cannot — a stack retargeted by
/// somebody's hand, or a parent the board polls in a repository it does not
/// hold the issue for — and the first catches the window before the child's own
/// row has been re-polled.
///
/// An unknown base ref is never a retarget: a row the board has not seen a pull
/// request for has not been observed moving anywhere.
pub fn parent_landed(
    parent_merged: bool,
    child_base_ref: Option<&str>,
    parent_branch: Option<&str>,
) -> bool {
    if parent_merged {
        return true;
    }
    match (child_base_ref, parent_branch) {
        (Some(base), Some(branch)) => base != branch,
        _ => false,
    }
}

/// What the board says into the authoring chat when it finds the branch
/// rewritten under the checkout.
///
/// Written for an agent about to push. It states the fact, the consequence — a
/// plain push from here is a force-push over GitHub's rebase — and the two
/// commands that fix it, `gh stack` first because a layer of a stack is what
/// this happens to. Nothing about the board's own bookkeeping: the re-stamp
/// happens on its own the moment the rebase lands, and an agent told about it
/// would only have something else to be careful of.
pub fn rewritten_notice(branch: &str, base_ref: Option<&str>) -> String {
    // A base the board has not seen is left as a placeholder rather than
    // guessed at: `origin/main` in a command an agent will paste is a fact, and
    // this one would be an invented one.
    let onto = match base_ref {
        Some(base) => format!("origin/{base}"),
        None => "origin/<the branch your pull request now targets>".to_string(),
    };
    let prose = match base_ref {
        Some(base) => format!("onto origin/{base}"),
        None => "onto its new base".to_string(),
    };
    format!(
        "comet-board: `{branch}` has been rewritten on origin.\n\n\
         The layer below yours landed, so GitHub rebased your branch {prose} \
         and retargeted your pull request. This checkout still holds the history \
         from before that, so `git push` from here would force your old commits \
         back over GitHub's rebase and undo it.\n\n\
         Bring the checkout across before you push again:\n\n\
         ```\n\
         gh stack sync    # if this branch is a layer of a stack\n\
         git fetch origin && git rebase {onto}    # otherwise\n\
         ```\n\n\
         Resolve any conflicts and carry on. If you have already pushed since \
         reading this, check the pull request holds what you meant it to."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Outcome, Source, UpstreamState};
    use comet_proto::view::board::BoardState;

    fn task(id: &str, attempts: Vec<Attempt>) -> Task {
        Task {
            id: id.into(),
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
            upstream: UpstreamState::Started,
            local_done: false,
            pr_url: None,
            pr_number: None,
            pr_open: false,
            pr_merged: false,
            pr_mergeable: None,
            pr_base_ref: None,
            pr_stack: None,
            updated_at: String::new(),
            synced_at: String::new(),
            attempts,
        }
    }

    /// A layer's attempt: its own id, and the attempt it was cut from.
    fn layer(id: i64, stacked_on: Option<i64>) -> Attempt {
        Attempt {
            id,
            worktree: Some(format!("/wt/{id}")),
            branch: Some(format!("board/gh-{id}")),
            outcome: Some(Outcome::Done),
            stacked_on,
            ..crate::model::tests::blank_attempt()
        }
    }

    // ---- the edge, read downwards ----------------------------------------

    /// The headline: a child cut from an attempt keeps that attempt's branch
    /// alive, and the parent has no way of knowing it without this map.
    #[test]
    fn a_child_holds_the_attempt_it_was_cut_from() {
        let tasks = vec![
            task("gh:o/r#1", vec![layer(1, None)]),
            task("gh:o/r#2", vec![layer(2, Some(1))]),
        ];
        let deps = Dependents::of(&tasks);
        assert!(deps.holds(1));
        assert_eq!(deps.holders(1), [2]);
        // And the child itself is nobody's parent.
        assert!(!deps.holds(2));
        assert_eq!(deps.holders(2), [] as [i64; 0]);
    }

    /// Two dispatches cut from one parent are two holds, and the ids are
    /// ordered so the log line that names them is stable across cycles.
    #[test]
    fn several_children_all_hold_and_are_named_in_order() {
        let tasks = vec![
            task("gh:o/r#1", vec![layer(1, None)]),
            task("gh:o/r#3", vec![layer(3, Some(1))]),
            task("gh:o/r#2", vec![layer(2, Some(1))]),
        ];
        assert_eq!(Dependents::of(&tasks).holders(1), [2, 3]);
    }

    /// The hold ends with the child's *checkout*, not with its run: a closed
    /// child still has a branch somebody may look at, and the parent's branch
    /// is the only remaining name for the history under it.
    #[test]
    fn the_hold_ends_when_the_childs_own_checkout_is_reclaimed() {
        let mut child = layer(2, Some(1));
        child.collected_at = Some("2026-08-11T00:00:00Z".into());
        let tasks = vec![
            task("gh:o/r#1", vec![layer(1, None)]),
            task("gh:o/r#2", vec![child]),
        ];
        assert!(!Dependents::of(&tasks).holds(1));

        // And a dispatch that never got a checkout at all holds nothing.
        let mut stillborn = layer(3, Some(1));
        stillborn.worktree = None;
        let tasks = vec![
            task("gh:o/r#1", vec![layer(1, None)]),
            task("gh:o/r#3", vec![stillborn]),
        ];
        assert!(!Dependents::of(&tasks).holds(1));
    }

    /// A chain unwinds from the top: the middle layer holds the bottom while
    /// the top holds the middle, and each is freed by its own collection.
    #[test]
    fn a_chain_unwinds_from_the_top() {
        let mut top = layer(3, Some(2));
        let tasks = |top: &Attempt| {
            vec![
                task("gh:o/r#1", vec![layer(1, None)]),
                task("gh:o/r#2", vec![layer(2, Some(1))]),
                task("gh:o/r#3", vec![top.clone()]),
            ]
        };
        let deps = Dependents::of(&tasks(&top));
        assert!(deps.holds(1) && deps.holds(2));
        top.collected_at = Some("2026-08-11T00:00:00Z".into());
        let deps = Dependents::of(&tasks(&top));
        assert!(deps.holds(1), "the middle layer still stands on the bottom");
        assert!(!deps.holds(2), "and nothing stands on the middle any more");
    }

    /// The overwhelming case, and the one that must cost nothing: a board with
    /// no stacking on it has no holds at all.
    #[test]
    fn an_unstacked_board_holds_nothing() {
        let tasks = vec![task("gh:o/r#1", vec![layer(1, None), layer(2, None)])];
        let deps = Dependents::of(&tasks);
        assert!(!deps.holds(1) && !deps.holds(2));
    }

    // ---- the stamp's footing ---------------------------------------------

    #[test]
    fn a_stamp_still_on_the_branch_stands() {
        assert_eq!(footing("base", true, None), Footing::Stands);
        assert_eq!(footing("base", true, Some("other")), Footing::Stands);
    }

    /// The rewrite: the stamp is off the branch, and the checkout knows where
    /// the branch starts now. That fork point is what everything measures from,
    /// and what the row is re-stamped with.
    #[test]
    fn a_rewritten_branch_is_refooted_on_its_fork_point() {
        assert_eq!(
            footing("old-parent-tip", false, Some("new-trunk-tip")),
            Footing::Refooted("new-trunk-tip".into())
        );
    }

    /// A fork point that *is* the stamp is not a rewrite however the ancestry
    /// check answered — an unreadable commit, a shallow checkout — and must not
    /// re-stamp or log one.
    #[test]
    fn a_fork_point_equal_to_the_stamp_is_no_rewrite() {
        assert_eq!(footing("base", false, Some("base")), Footing::Stands);
    }

    /// Rewritten with nothing to re-foot on: the honest answer is that this
    /// branch has no base the board can name, not the old one.
    #[test]
    fn a_rewritten_branch_with_no_fork_point_is_adrift() {
        assert_eq!(footing("base", false, None), Footing::Adrift);
    }

    // ---- against origin ---------------------------------------------------

    /// A tiny history: `ancestors[c]` is what commit `c` contains.
    fn graph(edges: &[(&'static str, &'static [&'static str])]) -> impl Fn(&str, &str) -> bool {
        let map: HashMap<String, Vec<String>> = edges
            .iter()
            .map(|(c, anc)| {
                (
                    c.to_string(),
                    anc.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                )
            })
            .collect();
        move |a: &str, b: &str| map.get(b).is_some_and(|anc| anc.iter().any(|x| x == a))
    }

    #[test]
    fn a_branch_nobody_rewrote_is_in_step() {
        let dag = graph(&[("remote", &["local"]), ("local", &[])]);
        // Level, origin ahead, and the checkout ahead: all ordinary.
        assert_eq!(against_origin("local", "local", &dag), Remote::InStep);
        assert_eq!(against_origin("local", "remote", &dag), Remote::InStep);
        assert_eq!(against_origin("remote", "local", &dag), Remote::InStep);
    }

    /// The case this whole module exists for: GitHub rebased the branch, so
    /// the two histories share no tip. A push from the checkout would put the
    /// pre-rebase commits back.
    #[test]
    fn a_rebased_remote_has_diverged() {
        let dag = graph(&[("rebased", &["trunk"]), ("local", &["parent"])]);
        assert_eq!(against_origin("local", "rebased", &dag), Remote::Rewritten);
    }

    // ---- the trigger ------------------------------------------------------

    #[test]
    fn a_merged_parent_is_a_landing() {
        assert!(parent_landed(true, None, None));
    }

    /// The other spelling: the child's own request has been moved off the
    /// parent's branch, which is what GitHub does in the same motion.
    #[test]
    fn a_retargeted_child_is_a_landing_too() {
        assert!(parent_landed(false, Some("main"), Some("board/gh-11")));
        assert!(!parent_landed(
            false,
            Some("board/gh-11"),
            Some("board/gh-11")
        ));
    }

    /// An unobserved base is not evidence of anything. A row whose pull
    /// request the board has not seen must not read as retargeted.
    #[test]
    fn an_unknown_base_is_never_a_landing() {
        assert!(!parent_landed(false, None, Some("board/gh-11")));
        assert!(!parent_landed(false, Some("main"), None));
    }

    // ---- what the agent is told -------------------------------------------

    /// It has to be actionable without a lookup: the branch, what happened,
    /// what a push would do now, and the two commands.
    #[test]
    fn the_notice_names_the_branch_the_risk_and_the_repair() {
        let notice = rewritten_notice("board/gh-12-widget", Some("main"));
        assert!(notice.contains("`board/gh-12-widget` has been rewritten on origin"));
        assert!(notice.contains("force"));
        assert!(notice.contains("gh stack sync"));
        assert!(notice.contains("git rebase origin/main"));
        // A base the board never saw is left a placeholder, not invented into
        // a command an agent would paste.
        let unknown = rewritten_notice("board/gh-12-widget", None);
        assert!(unknown.contains("origin/<the branch your pull request now targets>"));
    }
}
