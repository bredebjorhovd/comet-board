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
//! The other half of this file is the *authoring* side (gh#287): the branch
//! convention an agent-written stack has to follow for its layers to reach the
//! board as one attempt's work rather than as strangers, and the block of brief
//! that tells a dispatched agent to write one. See [`layer_branch`].
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
                changes_requested: task.pr_changes_requested.is_some(),
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
        let layers = self
            .layers
            .get(&stack_key(task)?)
            .cloned()
            .unwrap_or_default();
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

// ---- addressing the other layers (gh#289) -------------------------------

/// One layer of a stack, as the layers around it need to see it.
///
/// Enough to *word* a fact about a neighbour without holding the neighbour's
/// whole task: which row it is (so a surface can navigate, and delivery can
/// resolve a chat), what it prints as, and its pull request. Not the branch —
/// the branch that matters for a rebase is the one GitHub reports on the pull
/// request, and the board's record of it goes stale exactly when a parent merges
/// (gh#285).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependent {
    pub id: String,
    pub identifier: String,
    pub pr_number: Option<i64>,
    /// The pull request is still open. A layer that has landed is nobody's
    /// parent any more — GitHub retargets its children onto its own base — so
    /// this is what stops a merged layer holding the chain above it hostage.
    pub open: bool,
    /// The review that last asked this layer to change (gh#289).
    pub changes_requested: Option<i64>,
}

/// Which rows are stacked on which, and so who else a fact about one row is a
/// fact about (gh#289).
///
/// [`Stacks`] answers what a row should *say* about its siblings — position,
/// size, the AND that decides whether merging it lands anything. This answers
/// the other question a stack raises, and the one propagation needs: given
/// something that just happened to one layer, which other layers is it about?
///
/// **Direction is the whole design.** Dependency points one way, so this does
/// too. `changes requested` on layer 2 genuinely invalidates layers 3..N: their
/// base is about to be rewritten, and when layer 2 force-pushes its fix GitHub
/// replays their commits on top of it, so their diffs move under both their
/// authors and their reviewers with no explanation in either place. `changes
/// requested` on layer 3 invalidates **nothing** about layer 2 — it is still
/// correct, still mergeable, still reviewable — so nothing goes down.
///
/// **Two edges, unioned**, because a stack reaches the board two ways and the
/// dependency is real either way:
///
/// - GitHub's own stack object (gh#282), grouped into chains by [`Stacks`]. Each
///   layer is stacked on the one below it.
/// - [`Attempt::stacked_on`](crate::model::Attempt::stacked_on) (gh#285) — a
///   dispatch the board cut from a sibling's branch. GitHub is never told that
///   is a stack, so its `stack` object is absent and the grouping above finds
///   nothing; the edge is no less real, and the child's diff no less dependent.
///
/// Where both speak, GitHub's wins: it is the topology the rebase will actually
/// follow.
#[derive(Debug, Default)]
pub struct Dependents {
    /// child id → the id of the layer it is stacked on.
    parent: HashMap<String, String>,
    /// parent id → the ids stacked directly on it. A `Vec` because two dispatches
    /// can be cut from one sibling: that is a fan rather than a chain, and both
    /// of them are dependents.
    children: HashMap<String, Vec<String>>,
    /// Every row in an edge, by id.
    rows: HashMap<String, Dependent>,
}

impl Dependents {
    /// Index one board's worth of tasks.
    pub fn of(tasks: &[Task]) -> Self {
        let mut me = Dependents::default();
        for task in tasks {
            me.rows.insert(
                task.id.clone(),
                Dependent {
                    id: task.id.clone(),
                    identifier: task.identifier.clone(),
                    pr_number: task.pr_number,
                    open: task.pr_open,
                    changes_requested: task.pr_changes_requested,
                },
            );
        }

        // The dispatched edge first, so GitHub's own topology overwrites it
        // where the two disagree.
        let mut owner: HashMap<i64, &str> = HashMap::new();
        for task in tasks {
            for attempt in &task.attempts {
                owner.insert(attempt.id, task.id.as_str());
            }
        }
        for task in tasks {
            // Newest attempt wins: a retry cut from somewhere else is where this
            // row's branch stands now.
            for attempt in &task.attempts {
                let Some(on) = attempt.stacked_on.and_then(|id| owner.get(&id)) else {
                    continue;
                };
                if *on != task.id {
                    me.parent.insert(task.id.clone(), on.to_string());
                }
            }
        }

        for chain in Stacks::of(tasks).layers.into_values() {
            for pair in chain.windows(2) {
                me.parent.insert(pair[1].id.clone(), pair[0].id.clone());
            }
        }

        for (child, parent) in &me.parent {
            me.children
                .entry(parent.clone())
                .or_default()
                .push(child.clone());
        }
        // Stable order, so a fan-out addresses its dependents the same way twice
        // and a log line does not reshuffle between cycles.
        for kids in me.children.values_mut() {
            kids.sort();
        }
        me
    }

    /// Every layer stacked on `id`, transitively, nearest first.
    ///
    /// Nearest first because that is the order a rebase reaches them in, and the
    /// order a reader would name them. Transitive because the invalidation is:
    /// rewriting layer 2 moves layer 3, which moves layer 4, and a notice that
    /// stopped at the direct child would leave every layer above it wondering
    /// why its diff moved — the bug this exists to close, one layer up.
    pub fn above(&self, id: &str) -> Vec<&Dependent> {
        let mut out: Vec<&Dependent> = Vec::new();
        let mut queue: Vec<&str> = vec![id];
        // A malformed edge — two rows each recorded as the other's parent — must
        // cost a short answer, never a hung sync cycle. There cannot be more
        // layers than there are rows.
        while let Some(next) = queue.pop() {
            if out.len() > self.rows.len() {
                break;
            }
            for child in self.children.get(next).into_iter().flatten() {
                if out.iter().any(|d| d.id == *child) {
                    continue;
                }
                if let Some(row) = self.rows.get(child) {
                    out.push(row);
                    queue.push(child);
                }
            }
        }
        out
    }

    /// The nearest layer below `id` that has been asked to change and is still
    /// open, if there is one.
    ///
    /// Walks down rather than checking only the direct parent, because a merged
    /// layer is not in the way: GitHub retargets its children onto its own base,
    /// so the layer that will actually rebase this one is the nearest open one
    /// underneath.
    pub fn changes_below(&self, id: &str) -> Option<&Dependent> {
        let mut at = id;
        for _ in 0..=self.rows.len() {
            let parent = self.parent.get(at)?;
            let row = self.rows.get(parent.as_str())?;
            if row.open && row.changes_requested.is_some() {
                return Some(row);
            }
            at = parent.as_str();
        }
        None
    }
}

// ---- authoring a stack (gh#287) -----------------------------------------

/// The branch an attempt's `layer`-th stack layer must be called: layer 1 is
/// the attempt branch itself, and every layer above it is that name with `-2`,
/// `-3`, … on the end.
///
/// The board names one branch per attempt ([`crate::dispatch::resolve_branch`])
/// and links a pull request to a task by that name
/// (`pr_matches_branch`). An agent that decomposes its task into a stack
/// creates branches the board never named, and unless their names *say* whose
/// they are, every layer above the first arrives as a pull request belonging to
/// nobody — imported as a row of its own, dispatchable, reviewed by no chat.
/// So the names are a convention rather than the agent's choice, and
/// [`stack_brief`] states it as one.
///
/// **Not `board/gh-12-widget/2`**, which is what this issue was filed asking
/// for and which git cannot store: a ref is a path, so `refs/heads/…/widget`
/// being a file forbids `refs/heads/…/widget/2` from being a directory. The
/// bottom layer *is* the attempt branch — that is the whole point of the
/// convention — so a separator that nests is the one shape unavailable. A
/// suffix on the last segment is the next most legible thing, and it sorts
/// beside its own stack in every branch listing.
///
/// Layer numbering starts at 1 so that it reads as GitHub's own `position`,
/// which also counts the bottom as 1.
pub fn layer_branch(attempt_branch: &str, layer: i64) -> String {
    match layer {
        ..=1 => attempt_branch.to_string(),
        n => format!("{attempt_branch}-{n}"),
    }
}

/// Which layer of the stack cut from `attempt_branch` this head ref is: `Some(1)`
/// for the attempt branch itself, `Some(n)` for `<attempt branch>-n`, `None` for
/// a branch that is nothing to do with it.
///
/// Deliberately strict about the suffix — digits only, no leading zero, never
/// below 2 — because the cost of a false positive is a real pull request
/// swallowed into somebody else's attempt. `board/gh-28-x` and `board/gh-28-x-2`
/// are both branches the board itself can name (the second for issue 28 of a
/// repo called `x-2`), and the only reason that pair is safe is that linking is
/// scoped to one repository first: two repos are never each other's layers.
/// Callers that reach beyond one repo must keep that scope.
pub fn layer_of(head_ref: &str, attempt_branch: &str) -> Option<i64> {
    if attempt_branch.is_empty() || head_ref.is_empty() {
        return None;
    }
    if head_ref == attempt_branch {
        return Some(1);
    }
    let suffix = head_ref.strip_prefix(attempt_branch)?.strip_prefix('-')?;
    if suffix.starts_with('0') || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok().filter(|n| *n >= 2)
}

/// The block [`crate::dispatch::resolve_prompt`] appends when a dispatch asks
/// for a stack — the only channel the board has to the thing doing the writing.
///
/// It is appended rather than interpolated for [`crate::dispatch::pr_base`]'s
/// reason: a route's own `prompt` is somebody's wording for the *task*, and
/// whether this particular release wants layers is a fact about the dispatch
/// that the same somebody could not have known when they wrote the template.
///
/// What it has to carry, and why each part is in it:
///
/// - **The convention, as a requirement.** [`layer_branch`] is what makes the
///   layers this attempt's; an agent that names them anything else has produced
///   work the board cannot attribute, and no later poll repairs it.
/// - **The non-interactive flags, spelled out.** `gh stack submit` opens an
///   editor and creates *drafts* unless told otherwise, and a stack of drafts
///   is a stack nobody reviews. The upstream skill's own rule is to never rely
///   on TTY detection, and this is that rule with our two flags named.
/// - **The install.** The extension is box-level (`~/.local/share/gh/…`), not
///   per-run, so a fresh box has not got it and the failure is a bare `unknown
///   command "stack" for "gh"` halfway through a task. gh#324 measured that
///   `gh extension install` succeeds through the board's `gh` shim on the
///   minted installation token, so the agent can repair it in place — which is
///   the difference between a box-level prerequisite and a dispatch that dies.
/// - **A floor.** Two layers is the smallest thing that is a stack; below that
///   the ceremony buys nothing and one pull request is the honest shape.
///
/// `base` is [`crate::dispatch::pr_base`]'s answer: the branch the *stack*
/// lands on, or `None` when the repo's own default is already right. Only the
/// bottom layer has a base to argue about — GitHub bases every layer above it
/// on the layer below and retargets them as the stack merges.
pub fn stack_brief(branch: &str, base: Option<&str>) -> String {
    let init = match base {
        Some(base) => format!("gh stack init {branch} --base {base}"),
        None => format!("gh stack init {branch}"),
    };
    let second = layer_branch(branch, 2);
    let third = layer_branch(branch, 3);
    format!(
        "\n\nThis dispatch asks for a **stacked** pull request: decompose the work \
         into layers and open one pull request per layer, with GitHub's `gh stack` \
         extension. Design the stack before you write any of it — one dependent \
         concern per layer, foundations at the bottom and the code that needs them \
         above. If the work does not honestly split into at least two layers, open \
         one ordinary pull request and say in its description why it did not.\
         \n\nThe layer branch names are the board's, not yours: layer 1 is `{branch}` \
         — the branch you are standing on, never renamed and never replaced — and \
         every layer above it is `{second}`, `{third}`, and so on in order. That \
         naming is how the board knows the layers are one attempt's work; a layer \
         branched under any other name arrives as a pull request belonging to \
         nobody.\
         \n\n```\
         \n{init}  # adopt the branch you are on as layer 1\
         \n# write and commit layer 1\
         \ngh stack add {second}  # cut the next layer at HEAD\
         \n# write and commit layer 2, and so on\
         \ngh stack submit --auto --open  # push every branch, open every pull request\
         \ngh stack view --json  # confirm what GitHub now holds\
         \n```\
         \n\nPass the non-interactive flags every time: `--auto --open` on `submit` \
         (bare `submit` opens an editor, and without `--open` every layer arrives \
         as a draft nobody reviews) and `--json` on `view`. After changing a layer, \
         replay the ones above it with `gh stack rebase --upstack`; `gh stack sync` \
         catches the stack up with the trunk. Exit code 3 is a rebase conflict — \
         resolve it and continue, do not start over. Do not merge the stack: \
         `gh stack merge` is the operator's, the same way `gh pr merge` is.\
         \n\nIf `gh` answers `unknown command \"stack\"`, this box has never \
         installed the extension: run `gh extension install github/gh-stack` once \
         — it installs for the whole box and your GitHub credential is the \
         board's, so it needs no login of yours — and carry on."
    )
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
            pr_head_ref: None,
            pr_stack: Some(PrStack {
                number: 7,
                size: Some(size),
                position: Some(position),
                base_ref: Some("main".into()),
            }),
            pr_changes_requested: None,
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
        let deps = Dependents::of(tasks);
        let task = &tasks[index];
        let mut row = crate::rows::task_row(
            task,
            None,
            &crate::config::RoutingConfig::default(),
            stacks.row_stack(task),
            deps.changes_below(&task.id).and_then(|d| d.pr_number),
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
        assert_eq!(
            stack.layers.len(),
            3,
            "the other repo's stack 7 is not ours"
        );
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
            "merge gh!13 into main · this lands PR #11, PR #12 with it — \
             GitHub merges the group or none of it",
        );
        // The bottom layer takes nothing with it, and the sentence stays a
        // sentence about one pull request.
        assert_eq!(
            merge_confirmation(&row_of(&tasks, 0)),
            "merge gh!11 into main",
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
            "merge gh!12 into main",
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
            "merge gh!13 into main · this lands PR #11, PR #12 with it — \
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
        assert_eq!(
            comet_proto::view::board::stack_note(&row).as_deref(),
            Some("3 of 3")
        );
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

    // ---- addressing the other layers (gh#289) ---------------------------

    /// One attempt of `task`, optionally cut from another attempt's branch.
    fn cut_from(id: i64, task: &str, stacked_on: Option<i64>) -> crate::model::Attempt {
        crate::model::Attempt {
            id,
            task_id: task.into(),
            stacked_on,
            ..crate::model::tests::blank_attempt()
        }
    }

    /// The headline edge, and the direction it points. A review of the bottom
    /// layer is a fact about every layer on top of it — nearest first, because
    /// that is the order a replay reaches them in — and about nothing underneath.
    #[test]
    fn the_layers_above_are_the_dependents_and_nothing_propagates_down() {
        let tasks = three_layers([Some("clean"); 3]);
        let deps = Dependents::of(&tasks);
        let above =
            |id: &str| -> Vec<Option<i64>> { deps.above(id).iter().map(|d| d.pr_number).collect() };
        assert_eq!(above("gh:o/r!11"), vec![Some(12), Some(13)]);
        assert_eq!(above("gh:o/r!12"), vec![Some(13)]);
        // Changes requested on the top layer invalidates nothing below it: those
        // layers are still correct, still mergeable, still reviewable.
        assert!(above("gh:o/r!13").is_empty());
    }

    /// The transitive half, stated on its own because stopping at the direct
    /// child is the bug one layer up: rewriting layer 1 moves layer 2, which
    /// moves layer 3, and layer 3 would be left wondering why its diff moved.
    #[test]
    fn a_dependents_own_dependents_are_dependents_too() {
        let mut tasks = three_layers([Some("clean"); 3]);
        // Strip GitHub's stack object, so the chain is only the dispatched edge
        // — pairwise, and it has to be walked to reach the top.
        for (i, task) in tasks.iter_mut().enumerate() {
            task.pr_stack = None;
            let id = i as i64 + 1;
            task.attempts = vec![cut_from(id, &task.id.clone(), (id > 1).then_some(id - 1))];
        }
        let deps = Dependents::of(&tasks);
        assert_eq!(
            deps.above("gh:o/r!11")
                .iter()
                .map(|d| d.pr_number)
                .collect::<Vec<_>>(),
            vec![Some(12), Some(13)],
        );
    }

    /// The edge GitHub was never told about (gh#285). `dispatch --onto` sets a
    /// base and nothing else; there is no `stack` object to group on, and the
    /// child's diff is no less dependent for it.
    #[test]
    fn a_dispatch_cut_from_a_sibling_is_a_dependency_github_never_heard_of() {
        let mut parent = layer("o/r", 11, 1, 1, "main", Some("clean"));
        parent.pr_stack = None;
        parent.attempts = vec![cut_from(1, "gh:o/r!11", None)];
        let mut child = layer("o/r", 12, 1, 1, "board/gh-11-lexer", Some("clean"));
        child.pr_stack = None;
        child.attempts = vec![cut_from(2, "gh:o/r!12", Some(1))];

        let tasks = vec![parent, child];
        assert!(
            Stacks::of(&tasks).row_stack(&tasks[1]).is_none(),
            "GitHub calls neither of these a stack",
        );
        let deps = Dependents::of(&tasks);
        assert_eq!(deps.above("gh:o/r!11").len(), 1, "and it is still an edge");
        assert!(deps.above("gh:o/r!12").is_empty());
    }

    /// An attempt cut from another attempt of the *same* task is a retry, not a
    /// stack: a row that depended on itself would be told about its own reviews.
    #[test]
    fn a_row_is_never_stacked_on_itself() {
        let mut task = layer("o/r", 11, 1, 1, "main", Some("clean"));
        task.pr_stack = None;
        task.attempts = vec![
            cut_from(1, "gh:o/r!11", None),
            cut_from(2, "gh:o/r!11", Some(1)),
        ];
        assert!(Dependents::of(&[task]).above("gh:o/r!11").is_empty());
    }

    /// What a row underneath has to be for this row to be waiting on it: open,
    /// and asked to change. Two layers down still counts — the whole chain
    /// replays.
    #[test]
    fn the_nearest_open_layer_asked_to_change_is_what_a_row_waits_on() {
        let mut tasks = three_layers([Some("clean"); 3]);
        tasks[0].pr_changes_requested = Some(900);
        let deps = Dependents::of(&tasks);
        assert!(
            deps.changes_below("gh:o/r!11").is_none(),
            "nothing is below the bottom layer",
        );
        assert_eq!(deps.changes_below("gh:o/r!12").unwrap().pr_number, Some(11));
        assert_eq!(
            deps.changes_below("gh:o/r!13").unwrap().pr_number,
            Some(11),
            "two layers down and still the thing that moves this one",
        );

        // The nearest of two, because that is the branch this row's base
        // actually is.
        tasks[1].pr_changes_requested = Some(901);
        let deps = Dependents::of(&tasks);
        assert_eq!(deps.changes_below("gh:o/r!13").unwrap().pr_number, Some(12));
    }

    /// A layer that landed is nobody's obstacle — GitHub retargets its children
    /// onto its own base — so the walk goes past it to whatever is still open.
    /// The same rule `landing` keeps for a merged parent.
    #[test]
    fn a_layer_that_landed_stops_holding_the_chain_above_it() {
        let mut tasks = three_layers([Some("clean"); 3]);
        tasks[0].pr_changes_requested = Some(900);
        tasks[0].pr_open = false;
        tasks[0].pr_merged = true;
        let deps = Dependents::of(&tasks);
        assert!(
            deps.changes_below("gh:o/r!12").is_none(),
            "whatever was asked of it, it is not going to be replayed",
        );

        // …and the layer above the merged one is read past, not stopped at.
        tasks[1].pr_changes_requested = Some(901);
        let deps = Dependents::of(&tasks);
        assert_eq!(deps.changes_below("gh:o/r!13").unwrap().pr_number, Some(12));
    }

    /// The whole point of the issue, as the row says it: a layer that GitHub
    /// calls `clean` is not reviewable while the branch under it is about to be
    /// rewritten — and the answer outranks `clean`, because `clean` is the one
    /// answer that gets somebody to press merge.
    #[test]
    fn a_layer_over_a_parent_asked_to_change_is_not_reviewable() {
        let mut tasks = three_layers([Some("clean"); 3]);
        tasks[0].pr_changes_requested = Some(900);

        let top = row_of(&tasks, 2);
        assert_eq!(landing(&top), Landing::ChangesBelow(11));
        assert_eq!(top.landing.as_deref(), Some("changes-below"));
        assert_eq!(
            landing_note(&top).as_deref(),
            Some("PR #11 below was asked to change · this rebases under it"),
        );
        // And the other place a reader is about to do something irreversible.
        assert!(
            merge_confirmation(&top)
                .ends_with("PR #11 below was asked to change · this rebases under it"),
            "{}",
            merge_confirmation(&top),
        );

        // The layer that was asked to change is not waiting on itself, and says
        // exactly what it said before.
        let bottom = row_of(&tasks, 0);
        assert_eq!(bottom.changes_below, None);
        assert_eq!(landing(&bottom), Landing::Ready { below: 0 });
    }

    /// The map marks the layer the stack is waiting on, so a surface with the
    /// chain in hand does not have to join the fact back on per layer.
    #[test]
    fn the_map_marks_the_layer_that_was_asked_to_change() {
        let mut tasks = three_layers([Some("clean"); 3]);
        tasks[1].pr_changes_requested = Some(900);
        let stack = row_of(&tasks, 2).stack.unwrap();
        assert_eq!(
            stack
                .layers
                .iter()
                .map(|l| l.changes_requested)
                .collect::<Vec<_>>(),
            vec![false, true, false],
        );
    }

    /// A closed pull request's reader is not warned that a diff is about to
    /// move: there is no diff of theirs left to read. The same rule
    /// `pr_mergeable` keeps on the row.
    #[test]
    fn a_closed_layer_carries_no_warning_about_its_parent() {
        let mut tasks = three_layers([Some("clean"); 3]);
        tasks[0].pr_changes_requested = Some(900);
        tasks[1].pr_open = false;
        tasks[1].pr_merged = true;
        let middle = row_of(&tasks, 1);
        assert_eq!(middle.changes_below, None);
        assert_eq!(middle.landing, None);
    }

    // ---- authoring (gh#287) ---------------------------------------------

    /// The bottom layer is the attempt branch itself, unchanged. Everything
    /// hangs off that: the board named it, the dispatcher recorded it, and the
    /// pull request link keys on it.
    #[test]
    fn the_bottom_layer_is_the_branch_the_board_already_named() {
        assert_eq!(layer_branch("board/gh-12-widget", 1), "board/gh-12-widget");
        assert_eq!(
            layer_of("board/gh-12-widget", "board/gh-12-widget"),
            Some(1)
        );
    }

    /// A suffix, not a path segment: `refs/heads/board/gh-12-widget` is a file,
    /// so git will not also let it be a directory holding `2`. The convention
    /// the issue asked for is the one shape that cannot be stored.
    #[test]
    fn layers_suffix_the_branch_rather_than_nesting_under_it() {
        assert_eq!(
            layer_branch("board/gh-12-widget", 2),
            "board/gh-12-widget-2"
        );
        assert_eq!(
            layer_branch("board/gh-12-widget", 5),
            "board/gh-12-widget-5"
        );
        assert_eq!(
            layer_of("board/gh-12-widget-5", "board/gh-12-widget"),
            Some(5)
        );
    }

    /// The strictness is the point: everything that is not the convention is
    /// somebody else's branch, and swallowing one into this attempt would hide
    /// a real pull request behind a coincidence.
    #[test]
    fn nothing_but_the_convention_reads_as_a_layer() {
        let base = "board/gh-12-widget";
        for other in [
            "board/gh-12-widget-",    // no number
            "board/gh-12-widget-0",   // no layer 0
            "board/gh-12-widget-1",   // layer 1 is the branch itself
            "board/gh-12-widget-02",  // a padded number is not our spelling
            "board/gh-12-widget-2b",  // digits only
            "board/gh-12-widget/2",   // the shape git cannot store
            "board/gh-12-widget-two", //
            "board/gh-12-widgetry-2", // a different branch that starts the same
            "board/gh-120-widget-2",  //
        ] {
            assert_eq!(layer_of(other, base), None, "{other} is not a layer");
        }
        assert_eq!(layer_of("", base), None);
        assert_eq!(
            layer_of(base, ""),
            None,
            "every branch would be a layer of it"
        );
    }

    /// The brief has to be usable without a lookup: the exact branch names, the
    /// exact flags, and the one command that repairs a box that has never
    /// installed the extension.
    #[test]
    fn the_brief_names_the_branches_the_flags_and_the_install() {
        let brief = stack_brief("board/gh-12-widget", None);
        assert!(brief.contains("gh stack init board/gh-12-widget "));
        assert!(brief.contains("gh stack add board/gh-12-widget-2"));
        assert!(brief.contains("board/gh-12-widget-3"));
        assert!(brief.contains("--auto --open"));
        assert!(brief.contains("view --json"));
        assert!(brief.contains("gh extension install github/gh-stack"));
        assert!(
            !brief.contains("--base"),
            "the repo default needs no --base, and naming one would be a guess",
        );
    }

    /// A dispatch that was cut from something other than the repo default says
    /// so once, on the only layer that has a base of its own.
    #[test]
    fn a_named_base_reaches_the_bottom_layer_only() {
        let brief = stack_brief("board/gh-12-widget", Some("release-1.x"));
        assert!(brief.contains("gh stack init board/gh-12-widget --base release-1.x"));
        assert_eq!(brief.matches("--base").count(), 1);
    }
}
