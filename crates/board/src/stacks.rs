//! A stack arrives as strangers: grouping the siblings back together (gh#283).
//!
//! 1/9 (gh#282) taught the sync loop to read the `stack` object off the pulls it
//! already fetches, and stored it flat on each row — number, size, position,
//! target branch. That makes every layer *say* it is in a stack; it does not
//! make the layers know about each other. A five-layer stack is still five rows
//! that each derive to `review` separately, and the row a reader opens can name
//! its position and nothing else in the chain.
//!
//! This is the join. One pass over the tasks builds the map — every layer the
//! board can see, ordered bottom-first — and every member's row carries it, so
//! a surface can draw the stack from the row in front of it and the AND behind
//! [`comet_proto::view::board::landing`] has the layers below to AND over.
//!
//! Two things this deliberately does not do:
//!
//! - **It does not invent an aggregate row.** A stack is not a task: nothing
//!   dispatched it, no issue backs it, no attempt settles it, and a row with
//!   none of those would be a row every other part of the board has to make an
//!   exception for. The layers stay the rows — each with its own chat, its own
//!   review, its own upstream issue — and the *grouping* rides them. A surface
//!   that wants the aggregate (the review screen, gh#234) has the whole map on
//!   any one member.
//! - **It does not touch [`crate::settled`].** A pull request is still the
//!   agent's own statement that its layer is finished, whether that layer sits
//!   mid-stack or was retargeted onto trunk when its parent merged. What the
//!   stack changes is not whether the attempt is over — it is what the board may
//!   claim about *merging* it, which is [`landing`]'s business and lives on the
//!   row rather than in the settle.
//!
//! [`landing`]: comet_proto::view::board::landing

use std::collections::HashMap;

use comet_proto::view::board::{RowStack, StackLayer};

use crate::model::Task;

/// A stack number, scoped to the repository that issued it.
///
/// GitHub numbers stacks per repository, the way it numbers pull requests, so
/// two repos can both have a stack 3. Grouping on the number alone would weld
/// two unrelated chains into one and then answer "can this land" by ANDing
/// somebody else's pull requests — the same class of bug as matching a PR to a
/// task on an unqualified branch name (herdr-board AGE-20).
type StackKey = (String, i64);

/// Every stack the board can see, keyed so a row can find its own siblings.
#[derive(Debug, Default)]
pub struct Stacks {
    layers: HashMap<StackKey, Vec<StackLayer>>,
}

impl Stacks {
    /// Group one board's worth of tasks into stacks.
    ///
    /// Takes the whole task list because that is what the grouping *is*: a
    /// sibling is another row, and no amount of looking at one row finds it.
    pub fn of(tasks: &[Task]) -> Self {
        let mut layers: HashMap<StackKey, Vec<StackLayer>> = HashMap::new();
        for task in tasks {
            let Some(key) = stack_key(task) else { continue };
            layers.entry(key).or_default().push(StackLayer {
                id: task.id.clone(),
                identifier: task.identifier.clone(),
                pr_number: task.pr_number,
                position: task.pr_stack.as_ref().and_then(|s| s.position),
                open: task.pr_open,
                mergeable: task.pr_mergeable.clone(),
            });
        }
        for chain in layers.values_mut() {
            // Bottom first. A layer GitHub gave no position sorts last rather
            // than first: an unplaced layer must never be read as the bottom of
            // the stack, because everything above it would then be told it has
            // an unmergeable parent. The pull request number and then the task
            // id break ties, so the order is stable across polls.
            chain.sort_by(|a, b| {
                let key = |l: &StackLayer| {
                    (
                        l.position.unwrap_or(i64::MAX),
                        l.pr_number.unwrap_or(i64::MAX),
                        l.id.clone(),
                    )
                };
                key(a).cmp(&key(b))
            });
        }
        Self { layers }
    }

    /// The stack this task's row carries, or `None` when it is not in one.
    ///
    /// Position and size stay GitHub's own — never `layers.len()`, which counts
    /// what the board happens to poll. A stack whose lower layers live in a repo
    /// the board does not watch is still five layers deep, and a reader told
    /// "2 of 2" about it would merge on the strength of it.
    pub fn row_stack(&self, task: &Task) -> Option<RowStack> {
        let stack = task.pr_stack.as_ref()?;
        let layers = self.layers.get(&stack_key(task)?).cloned().unwrap_or_default();
        Some(RowStack {
            number: stack.number,
            position: stack.position,
            size: stack.size,
            base_ref: stack.base_ref.clone(),
            layers,
        })
    }
}

/// Which stack, scoped to which repository.
///
/// The repository comes from the task id for a GitHub row, and from the pull
/// request's own URL otherwise — a Linear issue names no repository, and its
/// linked pull request is the only thing on the row that does. A task with
/// neither cannot be grouped and is left out: an unscoped stack number is not a
/// weaker key, it is the wrong one.
fn stack_key(task: &Task) -> Option<StackKey> {
    let number = task.pr_stack.as_ref()?.number;
    let repo = crate::model::gh_repo(&task.id)
        .map(str::to_string)
        .or_else(|| crate::model::pr_repo(task.pr_url.as_deref()?))?;
    Some((repo, number))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BoardState, PrStack, Source, UpstreamState};
    use comet_proto::view::board::{
        Landing, TaskRow, landing, landing_note, merge_confirmation, stack_line,
    };

    /// One layer of a stack, as the board holds it after a poll.
    fn layer(
        repo: &str,
        number: i64,
        position: i64,
        size: i64,
        base: &str,
        mergeable: Option<&str>,
    ) -> Task {
        Task {
            id: format!("gh:{repo}!{number}"),
            source: Source::Github,
            source_id: number.to_string(),
            identifier: format!("gh!{number}"),
            title: format!("layer {position}"),
            body: None,
            url: format!("https://github.com/{repo}/pull/{number}"),
            labels: vec![],
            state: BoardState::Review,
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Started,
            local_done: false,
            pr_url: Some(format!("https://github.com/{repo}/pull/{number}")),
            pr_number: Some(number),
            pr_open: true,
            pr_merged: false,
            pr_mergeable: mergeable.map(str::to_string),
            pr_base_ref: Some(base.to_string()),
            pr_stack: Some(PrStack {
                number: 7,
                size: Some(size),
                position: Some(position),
                base_ref: Some("main".into()),
            }),
            updated_at: crate::db::now(),
            synced_at: crate::db::now(),
            attempts: vec![],
        }
    }

    /// A three-layer stack: the bottom on trunk, two on the branch below them.
    fn three_layers(mergeable: [Option<&str>; 3]) -> Vec<Task> {
        vec![
            layer("o/r", 11, 1, 3, "main", mergeable[0]),
            layer("o/r", 12, 2, 3, "board/gh-11-lexer", mergeable[1]),
            layer("o/r", 13, 3, 3, "board/gh-12-parser", mergeable[2]),
        ]
    }

    /// The row a viewport would see for one task, with the stack attached — the
    /// same join `rows::board_rows` makes.
    fn row_of(tasks: &[Task], index: usize) -> TaskRow {
        let stacks = Stacks::of(tasks);
        let task = &tasks[index];
        let mut row = crate::rows::task_row(
            task,
            None,
            &crate::config::RoutingConfig::default(),
            stacks.row_stack(task),
        );
        // `task_row` fills this itself; asserted here so a change to one is a
        // change to both.
        assert_eq!(row.landing, landing(&row).as_str().map(str::to_string));
        row.landing = landing(&row).as_str().map(str::to_string);
        row
    }

    /// The headline: five strangers become one chain, bottom first, whatever
    /// order the poll happened to return them in.
    #[test]
    fn siblings_are_grouped_bottom_first() {
        let mut tasks = three_layers([Some("clean"); 3]);
        tasks.reverse();
        let row = row_of(&tasks, 0);
        let stack = row.stack.as_ref().expect("the top layer is in a stack");
        assert_eq!(
            stack.layers.iter().map(|l| l.pr_number).collect::<Vec<_>>(),
            vec![Some(11), Some(12), Some(13)],
        );
        // Its own place comes from GitHub, and the two under it are what
        // merging it would take with it.
        assert_eq!(stack.position, Some(3));
        assert_eq!(stack.below("gh:o/r!13").len(), 2);
    }

    /// Two repositories with a stack 7 each are two stacks. Grouping on the
    /// number alone would AND somebody else's pull requests into this one's
    /// answer.
    #[test]
    fn a_stack_number_is_scoped_to_its_repository() {
        let mut tasks = three_layers([Some("clean"); 3]);
        tasks.push(layer("other/repo", 4, 1, 1, "main", Some("dirty")));
        let row = row_of(&tasks, 2);
        let stack = row.stack.as_ref().unwrap();
        assert_eq!(stack.layers.len(), 3, "the other repo's stack 7 is not ours");
        assert!(landing(&row).ready(), "and cannot make this one wait");
    }

    /// The whole point of the issue: `clean` on a mid-stack pull request is
    /// clean *against the layer below*, and the board says so instead of
    /// implying the merge button is safe.
    #[test]
    fn a_clean_child_over_a_dirty_parent_is_not_ready_to_land() {
        let tasks = three_layers([Some("dirty"), Some("clean"), Some("clean")]);
        let top = row_of(&tasks, 2);
        assert_eq!(
            landing(&top),
            Landing::CleanAgainstBase {
                blocker: Some(top.stack.as_ref().unwrap().layer("gh:o/r!11").unwrap()),
            },
        );
        assert_eq!(top.landing.as_deref(), Some("waiting-on-stack"));
        assert_eq!(
            landing_note(&top).as_deref(),
            Some("clean against board/gh-12-parser · waiting on PR #11"),
        );
    }

    /// The layers below are clean, so merging this one lands all three —
    /// GitHub's semantics, and the only case where "ready to land" is true.
    #[test]
    fn a_clean_layer_over_clean_layers_lands_the_lot() {
        let tasks = three_layers([Some("clean"); 3]);
        let top = row_of(&tasks, 2);
        assert_eq!(landing(&top), Landing::Ready { below: 2 });
        assert_eq!(top.landing.as_deref(), Some("ready"));
        assert_eq!(
            landing_note(&top).as_deref(),
            Some("ready to land with 2 below")
        );
        // The bottom layer takes nothing with it and says so plainly.
        let bottom = row_of(&tasks, 0);
        assert_eq!(landing(&bottom), Landing::Ready { below: 0 });
        assert_eq!(landing_note(&bottom).as_deref(), Some("ready to land"));
    }

    /// Mergeability costs a call per open pull request and rides the full
    /// sweep, so "not asked yet" is the ordinary state of a fresh row. It must
    /// never round up: a clean layer over an unread one is clean against its
    /// base and nothing more.
    #[test]
    fn an_unread_layer_below_is_not_rounded_up_to_ready() {
        let tasks = three_layers([None, Some("clean"), Some("clean")]);
        let top = row_of(&tasks, 2);
        assert_eq!(landing(&top), Landing::CleanAgainstBase { blocker: None });
        assert_eq!(
            landing_note(&top).as_deref(),
            Some("clean against board/gh-12-parser"),
        );

        // And a row nobody has asked about at all claims nothing at all.
        let unread = row_of(&three_layers([None; 3]), 0);
        assert_eq!(landing(&unread), Landing::Unknown);
        assert_eq!(unread.landing, None);
        assert_eq!(landing_note(&unread), None);
    }

    /// gh#283's second open question, answered where it shows: a child whose
    /// parent merged is not waiting on it. The merged layer stays in the map
    /// while GitHub still reports it — it is history the reader wants — and is
    /// no longer an obstacle.
    #[test]
    fn a_merged_parent_stops_being_in_the_way() {
        let mut tasks = three_layers([Some("dirty"), Some("clean"), Some("clean")]);
        tasks[0].pr_open = false;
        tasks[0].pr_merged = true;
        // GitHub retargets the child onto trunk when the parent lands.
        tasks[1].pr_base_ref = Some("main".into());
        let middle = row_of(&tasks, 1);
        assert_eq!(landing(&middle), Landing::Ready { below: 0 });
        assert_eq!(landing_note(&middle).as_deref(), Some("ready to land"));
    }

    /// A landed layer makes no claim about merging. Mergeability is polled for
    /// open pull requests alone, so what a closed one last said is not a fact
    /// about anything — and it stays in the chain regardless, because the map
    /// is the reader's history of how the stack got here.
    #[test]
    fn a_closed_pull_request_makes_no_claim_about_merging() {
        let mut tasks = three_layers([Some("clean"); 3]);
        tasks[0].pr_open = false;
        tasks[0].pr_merged = true;
        let bottom = row_of(&tasks, 0);
        assert_eq!(bottom.pr_mergeable, None);
        assert_eq!(bottom.landing, None);
        assert_eq!(bottom.stack.as_ref().unwrap().layers.len(), 3);
    }

    /// GitHub's own objection outranks everything below it: there is no point
    /// telling somebody the parents are clean when this pull request conflicts.
    #[test]
    fn the_pull_requests_own_objection_comes_first() {
        let tasks = three_layers([Some("clean"), Some("clean"), Some("dirty")]);
        let top = row_of(&tasks, 2);
        assert_eq!(landing(&top), Landing::NotClean("dirty"));
        assert_eq!(top.landing.as_deref(), Some("not-clean"));
        assert_eq!(
            landing_note(&top).as_deref(),
            Some("conflicts with board/gh-12-parser"),
            "and it names the branch it conflicts with, which is not trunk",
        );
    }

    /// The detail's line: where in the stack, what it sits on, where the chain
    /// lands. The bottom layer's base *is* where the stack lands, and saying it
    /// twice would read as two branches.
    #[test]
    fn the_stack_line_names_the_layer_below_and_the_target() {
        let tasks = three_layers([Some("clean"); 3]);
        assert_eq!(
            stack_line(&row_of(&tasks, 1)).as_deref(),
            Some("stack 2 of 3 · onto board/gh-11-lexer · lands on main"),
        );
        assert_eq!(
            stack_line(&row_of(&tasks, 0)).as_deref(),
            Some("stack 1 of 3 · lands on main"),
        );
    }

    /// The confirm step's whole job (gh#290): a reader pressing merge on the
    /// top of a three-layer stack is merging three pull requests, and the board
    /// names them rather than leaving it at a count.
    #[test]
    fn the_confirmation_names_every_layer_the_merge_takes_with_it() {
        let tasks = three_layers([Some("clean"); 3]);
        assert_eq!(
            merge_confirmation(&row_of(&tasks, 2)),
            "merge PR #13 into main · this lands PR #11, PR #12 with it — \
             GitHub merges the group or none of it",
        );
        // The bottom layer takes nothing with it, and the sentence stays a
        // sentence about one pull request.
        assert_eq!(
            merge_confirmation(&row_of(&tasks, 0)),
            "merge PR #11 into main",
        );
    }

    /// A layer that already landed is history in the chain, not cargo — and the
    /// layers *above* are untouched by this merge, so neither is named.
    #[test]
    fn the_confirmation_leaves_out_what_the_merge_does_not_move() {
        let mut tasks = three_layers([Some("clean"); 3]);
        tasks[0].pr_open = false;
        tasks[0].pr_merged = true;
        tasks[1].pr_base_ref = Some("main".into());
        assert_eq!(
            merge_confirmation(&row_of(&tasks, 1)),
            "merge PR #12 into main",
            "the merged parent is gone and #13 is above, not below",
        );
    }

    /// GitHub evaluates its rules when the merge executes, not when it is
    /// submitted, so nothing upstream will stop a reader confirming on a layer
    /// the board can see is stuck. The confirm step is where that gets said.
    #[test]
    fn the_confirmation_carries_the_boards_objection_when_it_has_one() {
        let tasks = three_layers([Some("dirty"), Some("clean"), Some("clean")]);
        assert_eq!(
            merge_confirmation(&row_of(&tasks, 2)),
            "merge PR #13 into main · this lands PR #11, PR #12 with it — \
             GitHub merges the group or none of it · \
             clean against board/gh-12-parser · waiting on PR #11",
        );
    }

    /// A standalone pull request has no stack and gains no vocabulary: the
    /// board says `ready to land` or GitHub's objection, and never a position.
    #[test]
    fn a_standalone_pull_request_is_unchanged() {
        let mut task = layer("o/r", 20, 1, 1, "main", Some("clean"));
        task.pr_stack = None;
        let row = row_of(&[task], 0);
        assert!(row.stack.is_none());
        assert_eq!(landing(&row), Landing::Ready { below: 0 });
        assert_eq!(stack_line(&row), None);
        assert_eq!(landing_note(&row).as_deref(), Some("ready to land"));
    }

    /// The map is what the board can see; the count is GitHub's. A stack whose
    /// lower layers are pull requests in a repo nobody polls must not read as a
    /// two-layer stack that is ready to land.
    #[test]
    fn a_stack_the_board_only_half_sees_still_reports_its_true_size() {
        let mut top = layer("o/r", 13, 3, 3, "board/gh-12-parser", Some("clean"));
        top.pr_stack = Some(PrStack {
            number: 7,
            size: Some(3),
            position: Some(3),
            base_ref: Some("main".into()),
        });
        let row = row_of(&[top], 0);
        let stack = row.stack.as_ref().unwrap();
        assert_eq!(stack.size, Some(3), "GitHub's count, not ours");
        assert_eq!(stack.layers.len(), 1);
        assert_eq!(comet_proto::view::board::stack_note(&row).as_deref(), Some("3 of 3"));
        // Nothing below it is known, so nothing below it is claimed. The layers
        // the board cannot see are exactly the ones it must not vouch for —
        // `below` is empty here only because the map is, and GitHub's position
        // says it is not the bottom.
        assert_eq!(landing(&row), Landing::CleanAgainstBase { blocker: None });
    }

    /// A Linear row names no repository in its id, and its stack number means
    /// nothing without one. The pull request's own URL is where it comes from.
    #[test]
    fn a_pull_request_url_names_the_repository_for_a_row_that_does_not() {
        let mut linear = layer("o/r", 11, 1, 2, "main", Some("clean"));
        linear.id = "linear:LIN-142".into();
        linear.identifier = "LIN-142".into();
        let top = layer("o/r", 12, 2, 2, "board/lin-142", Some("clean"));
        let row = row_of(&[linear, top], 1);
        assert_eq!(
            row.stack.as_ref().unwrap().layers.len(),
            2,
            "the linear row's PR is in the same repo, and so in the same stack",
        );
    }
}
