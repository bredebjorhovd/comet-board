//! Spaces-section derivations (gh#124): the honest space row and the
//! device-once grouping.
//!
//! A space row used to spend its loudest pixels on its least differentiating
//! fact — every row repeating "@ box · offline" in warning amber. The rules
//! here fix that at the derivation layer so both viewports agree:
//!
//! - [`space_title`] — what a space row is CALLED. A space is repo-first
//!   (gh#118): the `owner/repo` slug names it whenever a host has supplied the
//!   link, an explicit rename still wins, and the folder basename is the
//!   fallback for the scratch directory that is nobody's repo.
//! - [`space_titles`] — the same name, made UNIQUE within a device group
//!   (gh#138). A slug is not a folder: `~/dev/attn` and a board worktree at
//!   `~/.comet-native/worktrees/attn/board-gh-10-attn` are two real spaces on
//!   one machine that `space_title` calls `bredebjorhovd/attn` twice, which
//!   reads as a duplicated row. Colliding rows gain the shortest path tail
//!   that tells them apart.
//! - [`device_groups`] — where the device's name goes: once, as a group
//!   header above the spaces it hosts, instead of riding along on every row.
//!   The local device leads; remaining groups keep the spaces' given order.
//! - [`space_shelf`] — which of a space's chats its nested shelf draws
//!   (gh#138): the ones the Active list is NOT already drawing above.

use super::board::ActiveRow;
use crate::{Chat, Space};

/// One device's spaces, in the caller's (manual/creation) order.
///
/// The grouping preserves the input order within a group, so a caller that has
/// already applied a manual drag order keeps it. Reordering across groups is
/// meaningless — a space cannot change its device by being dragged.
#[derive(Debug, PartialEq)]
pub struct DeviceGroup<'a> {
    pub device_id: &'a str,
    pub spaces: Vec<&'a Space>,
}

/// Group ordered spaces by owning device: the local device's group first (your
/// own folders before any box's), then groups in first-appearance order of the
/// input. Pure and total — every space lands in exactly one group.
pub fn device_groups<'a>(ordered: &'a [Space], local_device: Option<&str>) -> Vec<DeviceGroup<'a>> {
    let mut groups: Vec<DeviceGroup<'a>> = Vec::new();
    for space in ordered {
        match groups.iter_mut().find(|g| g.device_id == space.device_id) {
            Some(group) => group.spaces.push(space),
            None => groups.push(DeviceGroup {
                device_id: &space.device_id,
                spaces: vec![space],
            }),
        }
    }
    if let Some(local) = local_device
        && let Some(ix) = groups.iter().position(|g| g.device_id == local)
        && ix > 0
    {
        let group = groups.remove(ix);
        groups.insert(0, group);
    }
    groups
}

/// What a space row is called: an explicit rename wins, then the repo slug
/// (`owner/repo`, per the gh#118 links), then the folder basename. Pure.
pub fn space_title<'a>(space: &'a Space, slug: Option<&'a str>) -> &'a str {
    if let Some(name) = space.name.as_deref()
        && !name.trim().is_empty()
    {
        return name;
    }
    if let Some(slug) = slug
        && !slug.trim().is_empty()
    {
        return slug;
    }
    space.display_name()
}

/// A row's name, split at the seam that matters when the row is too narrow
/// for all of it: the `base` a reader recognises, and the `qualifier` that
/// tells this row from its twin.
///
/// The two are separate fields and not one string because the surfaces that
/// elide do it from the right, and the qualifier is the last thing that may be
/// lost — a truncated `bredebjorhovd/attn · board-gh-10-attn` reads exactly
/// like the row it was minted to be different from. A renderer with room lays
/// them out with [`SpaceTitle::line`]; a renderer without gives the qualifier
/// its own width and shrinks the base instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceTitle {
    /// The name proper: rename, else repo slug, else folder basename.
    pub base: String,
    /// The path tail that separates this row from a same-named sibling in its
    /// device group. `None` when the base already stands alone.
    pub qualifier: Option<String>,
}

impl SpaceTitle {
    /// Both halves on one line, `base · qualifier` — for surfaces with room
    /// for the whole name (the TUI, tooltips, a drag ghost).
    pub fn line(&self) -> String {
        match &self.qualifier {
            Some(tail) => format!("{} · {tail}", self.base),
            None => self.base.clone(),
        }
    }
}

/// Row titles for ONE device group: [`space_title`] for each, made unique
/// within the group.
///
/// The repo-first name is a good name and a poor identifier — a repo has one
/// slug and any number of checkouts, so the moment a second folder on the same
/// machine points at it (a worktree the board cut, a second clone), two rows
/// carry one word and the sidebar looks like it is repeating itself (gh#138).
/// Colliding rows therefore take the shortest path tail that separates them,
/// at a uniform depth so the group reads at one level:
///
/// ```text
/// bredebjorhovd/attn · attn
/// bredebjorhovd/attn · board-gh-10-attn
/// ```
///
/// The tail comes back as a [`SpaceTitle::qualifier`] rather than glued to the
/// name, because in a sidebar narrow enough to elide `bredebjorhovd/attn` on
/// its own an appended tail is the FIRST thing cut, and the fix is invisible
/// exactly where it was needed.
///
/// Uniqueness is per group, not global, because that is the scope a reader
/// scans: the same repo checked out on the laptop AND on the box is told apart
/// by the device header above each group, and re-saying it on the row would
/// undo gh#124.
///
/// Pure and index-preserving: `out[i]` names `rows[i]`. Two rows for literally
/// the same path keep the same title — there is no fact left to separate them
/// with, and that IS a duplicate.
pub fn space_titles(rows: &[(&Space, Option<&str>)]) -> Vec<SpaceTitle> {
    let bases: Vec<&str> = rows
        .iter()
        .map(|(space, slug)| space_title(space, *slug))
        .collect();
    let mut out: Vec<SpaceTitle> = bases
        .iter()
        .map(|base| SpaceTitle {
            base: (*base).to_string(),
            qualifier: None,
        })
        .collect();
    let mut done: Vec<&str> = Vec::new();
    for base in &bases {
        if done.contains(base) {
            continue;
        }
        done.push(base);
        let clash: Vec<usize> = bases
            .iter()
            .enumerate()
            .filter(|(_, other)| *other == base)
            .map(|(ix, _)| ix)
            .collect();
        if clash.len() < 2 {
            continue;
        }
        let deepest = clash
            .iter()
            .map(|ix| segments(&rows[*ix].0.path).len())
            .max()
            .unwrap_or(1)
            .max(1);
        // The shallowest tail that separates every colliding row — one depth
        // for the whole clash, so the qualifiers line up.
        let depth = (1..=deepest)
            .find(|depth| {
                let mut tails: Vec<String> = clash
                    .iter()
                    .map(|ix| path_tail(&rows[*ix].0.path, *depth))
                    .collect();
                tails.sort();
                let total = tails.len();
                tails.dedup();
                tails.len() == total
            })
            .unwrap_or(deepest);
        for ix in clash {
            let tail = path_tail(&rows[ix].0.path, depth);
            if !tail.is_empty() {
                out[ix].qualifier = Some(tail);
            }
        }
    }
    out
}

fn segments(path: &str) -> Vec<&str> {
    path.split(['/', '\\']).filter(|s| !s.is_empty()).collect()
}

/// The last `depth` path segments, `/`-joined. Empty for a path with none.
fn path_tail(path: &str, depth: usize) -> String {
    let segments = segments(path);
    let start = segments.len().saturating_sub(depth.max(1));
    segments[start..].join("/")
}

// ---------------------------------------------------------------------------
// What a collapsed row says about where the folder is (gh#229)
// ---------------------------------------------------------------------------

/// Is this space a git work tree — the question the row's glyph answers.
///
/// Owner-stamped `git_detected` is the evidence, and the gh#118 slug is only a
/// fallback for the row whose owner has not stamped it yet: a checkout is a
/// checkout whether or not a board host is reachable to name its repo, and
/// keying the glyph on the slug alone made every space look like a plain
/// folder on a laptop that hosts no board — and, the other way round, gave the
/// scratch directory the same mark as the repos the moment anything answered.
pub fn is_repo_space(space: &Space, slug: Option<&str>) -> bool {
    space.git_detected || slug.is_some_and(|s| !s.trim().is_empty())
}

/// What the space row says it is checked out on, or `None` for a folder that
/// is not a work tree (and for a row whose owner has not stamped one yet).
///
/// A detached HEAD is reported as `"HEAD"` by git and by
/// [`crate::Space::branch`]; the row says `detached` instead, because "HEAD"
/// is a branch name only to somebody who already knows it is not one.
pub fn space_branch(space: &Space) -> Option<&str> {
    let branch = space
        .branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())?;
    Some(if branch == "HEAD" { "detached" } else { branch })
}

// ---------------------------------------------------------------------------
// One full row per chat (gh#138)
// ---------------------------------------------------------------------------
//
// Active and the spaces tree answer different questions — "what is alive" and
// "what lives here" — and gh#124 let both answer with a full session row. When
// the box's whole load sits in one space, which is the common case, that space
// expands into a verbatim copy of the list directly above it: three agents,
// six rows, one screen height.
//
// So the surfaces divide the chats instead of the questions: **Active owns a
// chat while its session is live; the space's shelf shows it once it is idle.**
// The tree still answers what lives here — it just stops re-listing what Active
// has already said is alive, and says so in the two places a reader would
// otherwise see a hole: a count on the space row, and the shelf's own note.

/// Where every row the Active list draws actually lives: `(chat id, space id)`.
///
/// [`super::board::AgentRow`] carries no space — an attempt is named by its
/// issue, not its folder — so the join happens here, once, rather than three
/// times in three viewports.
pub fn active_placements<'a>(
    active: &'a [ActiveRow],
    chats: &'a [Chat],
    spaces: &[Space],
) -> Vec<(&'a str, Option<&'a str>)> {
    active
        .iter()
        .map(|row| {
            let chat_id = row.chat_id();
            let space = chats
                .iter()
                .find(|chat| chat.id == chat_id)
                .and_then(|chat| chat.space_id.as_deref())
                // Chats and Spaces arrive on independent watches. Until a
                // referenced Space arrives (or after it has been removed),
                // the placement is unresolved and must use Active's fallback
                // rather than disappear between the two renderers.
                .filter(|space_id| spaces.iter().any(|space| space.id == *space_id));
            (chat_id, space)
        })
        .collect()
}

/// Whether the selected Space's disclosure is structurally required to stay
/// open because it owns the visible Orchestrator row.
///
/// This is shared by rendering and the chevron action so persisted collapsed
/// state cannot briefly move the Orchestrator outside its Space or hide it.
pub fn space_disclosure_forced_open(selected: bool, has_orchestrator: bool) -> bool {
    selected && has_orchestrator
}

/// The live chats a space's disclosure draws, in the order Active derived them
/// (needs-you first, then working).
///
/// Since gh#258 the sidebar has no Active group above Spaces: Spaces is the
/// primary group, and a live run's row is drawn under the space it belongs to
/// rather than in a list that displaces the tree. This is the half of that
/// split a space claims; [`homeless_live`] is the remainder.
pub fn space_live<'a>(space_id: &str, active: &[(&'a str, Option<&'a str>)]) -> Vec<&'a str> {
    active
        .iter()
        .filter(|(_, space)| *space == Some(space_id))
        .map(|(chat, _)| *chat)
        .collect()
}

/// The live chats no current space claims — either the chat has no `space_id`
/// or [`active_placements`] normalized a dangling reference to `None` while
/// the independent Space watch catches up.
///
/// These are the only rows that still need a group of their own, and it sits
/// BELOW the spaces tree: a live run must always have a row somewhere, and the
/// hierarchy the tree states is not the thing that should give way for it.
pub fn homeless_live<'a>(active: &[(&'a str, Option<&'a str>)]) -> Vec<&'a str> {
    active
        .iter()
        .filter(|(_, space)| space.is_none())
        .map(|(chat, _)| *chat)
        .collect()
}

/// What a space's nested shelf draws, and what it defers to Active.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SpaceShelf<'a> {
    /// The chats the shelf draws, in the caller's (tab) order: the space's
    /// own, minus every chat Active is drawing above.
    pub idle: Vec<&'a str>,
    /// How many of this space's chats Active holds — the count the space row
    /// carries so the reader can see where the missing rows went.
    pub running: usize,
}

/// Split a space's chats between the Active list and its own shelf.
///
/// `order` is the space's shelf order (the tab strip's, so the two agree);
/// `active` is [`active_placements`] for the whole sidebar. The count is taken
/// from the placements rather than from `order` on purpose: an archived chat
/// that is working anyway is in Active (the board half never hides a live run)
/// and out of the tab order, and the space row should still admit it exists.
pub fn space_shelf<'a>(
    space_id: &str,
    order: impl IntoIterator<Item = &'a str>,
    active: &[(&str, Option<&str>)],
) -> SpaceShelf<'a> {
    SpaceShelf {
        idle: order
            .into_iter()
            .filter(|chat| !active.iter().any(|(held, _)| held == chat))
            .collect(),
        running: active
            .iter()
            .filter(|(_, space)| *space == Some(space_id))
            .count(),
    }
}

/// The space row's compact tie to the Active list — `· 3 running` — or `None`
/// when the space has nothing up there.
///
/// It rides beside the aggregate dot, which says WHAT the most urgent member is
/// doing; this says how many members are doing it, which is the fact a
/// collapsed row used to hide and an expanded row used to prove by repeating
/// the list.
pub fn running_label(running: usize) -> Option<String> {
    running_count_label(running).map(|label| format!("· {label}"))
}

/// The same fact without the leading separator — for a surface that draws a
/// status dot in front of it instead (gh#229: the desktop row pairs the count
/// with a dot coloured by the most urgent chat under it, and `· 3 running`
/// beside a dot says the separator twice).
pub fn running_count_label(running: usize) -> Option<String> {
    (running > 0).then(|| format!("{running} running"))
}

/// What an expanded shelf says INSTEAD of rows when Active holds all of them.
///
/// `None` when the shelf has rows to draw, or when it is empty for the ordinary
/// reason (the space has no sessions at all) — that case is the caller's own
/// empty state. Expanding a space and finding a gap would read as a bug; the
/// shelf answers with its own truth instead.
///
/// Read by the TUI, whose sidebar keeps the Active group above the tree. The
/// desktop's disclosure draws its own live rows since gh#258
/// ([`space_live`]), so nothing is "above" for it to point at.
pub fn shelf_note(shelf: &SpaceShelf<'_>) -> Option<String> {
    if !shelf.idle.is_empty() || shelf.running == 0 {
        return None;
    }
    Some(format!(
        "{} running above · no idle sessions",
        shelf.running
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn space(id: &str, device: &str, path: &str) -> Space {
        Space {
            id: id.into(),
            device_id: device.into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            branch: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn a_folder_that_is_not_a_work_tree_is_not_drawn_as_a_repo() {
        let mut scratch = space("s1", "box", "/srv/scratch");
        // No board answered, and the folder is nobody's checkout.
        assert!(!is_repo_space(&scratch, None));
        // A checkout is a checkout with no board host to name its repo…
        scratch.git_detected = true;
        assert!(is_repo_space(&scratch, None));
        // …and a slug is evidence too, for the row whose owner is offline and
        // has never stamped one.
        let stale = space("s2", "box", "/srv/attn");
        assert!(is_repo_space(&stale, Some("bredebjorhovd/attn")));
        assert!(!is_repo_space(&stale, Some("   ")));
    }

    #[test]
    fn a_collapsed_row_says_where_the_folder_is() {
        let mut s = space("s1", "box", "/srv/attn");
        assert_eq!(space_branch(&s), None);
        s.branch = Some("main".into());
        assert_eq!(space_branch(&s), Some("main"));
        // git's own word for no branch is the one word a reader would take for
        // a branch name.
        s.branch = Some("HEAD".into());
        assert_eq!(space_branch(&s), Some("detached"));
        s.branch = Some("   ".into());
        assert_eq!(space_branch(&s), None);
    }

    #[test]
    fn groups_preserve_input_order_within_a_device() {
        let spaces = vec![
            space("s1", "box", "/srv/a"),
            space("s2", "box", "/srv/b"),
            space("s3", "box", "/srv/c"),
        ];
        let groups = device_groups(&spaces, None);
        assert_eq!(groups.len(), 1);
        let ids: Vec<&str> = groups[0].spaces.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s2", "s3"]);
    }

    #[test]
    fn local_device_group_leads() {
        let spaces = vec![
            space("s1", "box", "/srv/a"),
            space("s2", "laptop", "/home/a"),
            space("s3", "box", "/srv/b"),
        ];
        let groups = device_groups(&spaces, Some("laptop"));
        assert_eq!(groups[0].device_id, "laptop");
        assert_eq!(groups[1].device_id, "box");
        // The box group keeps the input's relative order.
        let ids: Vec<&str> = groups[1].spaces.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["s1", "s3"]);
    }

    #[test]
    fn unknown_local_device_keeps_first_appearance_order() {
        let spaces = vec![
            space("s1", "box", "/srv/a"),
            space("s2", "laptop", "/home/a"),
        ];
        let groups = device_groups(&spaces, Some("phone"));
        assert_eq!(groups[0].device_id, "box");
        assert_eq!(groups[1].device_id, "laptop");
    }

    #[test]
    fn no_spaces_no_groups() {
        assert!(device_groups(&[], Some("laptop")).is_empty());
    }

    #[test]
    fn title_prefers_rename_then_slug_then_basename() {
        let mut s = space("s1", "box", "/srv/comet-board");
        assert_eq!(space_title(&s, None), "comet-board");
        assert_eq!(
            space_title(&s, Some("brede/comet-board")),
            "brede/comet-board"
        );
        s.name = Some("My board".into());
        assert_eq!(space_title(&s, Some("brede/comet-board")), "My board");
    }

    #[test]
    fn blank_slug_and_blank_rename_fall_through() {
        let mut s = space("s1", "box", "/srv/comet-board");
        s.name = Some("  ".into());
        assert_eq!(space_title(&s, Some("  ")), "comet-board");
    }

    // -- unique titles within a group (gh#138) ------------------------------

    #[test]
    fn one_repo_two_local_folders_are_told_apart() {
        // The screenshot's second finding: a checkout and the board worktree
        // cut from it, both on this Mac, both slugged `bredebjorhovd/attn`.
        let checkout = space("s1", "mac", "/Users/brede/dev/attn");
        let worktree = space(
            "s2",
            "mac",
            "/Users/brede/.comet-native/worktrees/attn/board-gh-10-attn",
        );
        let slug = Some("bredebjorhovd/attn");
        let titles = space_titles(&[(&checkout, slug), (&worktree, slug)]);
        assert_eq!(
            titles.iter().map(SpaceTitle::line).collect::<Vec<_>>(),
            [
                "bredebjorhovd/attn · attn",
                "bredebjorhovd/attn · board-gh-10-attn"
            ]
        );
    }

    #[test]
    fn the_separating_tail_is_a_field_a_narrow_row_can_keep() {
        // The operator screenshot after gh#138 shipped: both rows still read
        // `bredebjorhovd/attn…`, because the sidebar elides from the right and
        // the qualifier was glued to the end of the name. It comes back apart
        // so a renderer can spend its width on the half that differs.
        let checkout = space("s1", "mac", "/Users/brede/dev/attn");
        let worktree = space(
            "s2",
            "mac",
            "/Users/brede/.comet-native/worktrees/attn/board-gh-10-attn",
        );
        let slug = Some("bredebjorhovd/attn");
        let titles = space_titles(&[(&checkout, slug), (&worktree, slug)]);
        assert!(titles.iter().all(|t| t.base == "bredebjorhovd/attn"));
        assert_eq!(
            titles
                .iter()
                .map(|t| t.qualifier.as_deref())
                .collect::<Vec<_>>(),
            [Some("attn"), Some("board-gh-10-attn")]
        );
    }

    #[test]
    fn a_title_that_stands_alone_is_left_alone() {
        let one = space("s1", "mac", "/Users/brede/dev/attn");
        let two = space("s2", "mac", "/Users/brede/dev/comet");
        let titles = space_titles(&[
            (&one, Some("brede/attn")),
            (&two, Some("brede/comet-native")),
        ]);
        assert_eq!(
            titles.iter().map(SpaceTitle::line).collect::<Vec<_>>(),
            ["brede/attn", "brede/comet-native"]
        );
        assert!(titles.iter().all(|t| t.qualifier.is_none()));
    }

    #[test]
    fn the_tail_deepens_until_it_separates() {
        // Same basename: one segment says nothing, so both rows take two.
        let one = space("s1", "mac", "/Users/brede/dev/attn/api");
        let two = space("s2", "mac", "/Users/brede/work/web/api");
        let titles = space_titles(&[(&one, Some("brede/api")), (&two, Some("brede/api"))]);
        assert_eq!(
            titles.iter().map(SpaceTitle::line).collect::<Vec<_>>(),
            ["brede/api · attn/api", "brede/api · web/api"]
        );
    }

    #[test]
    fn identical_paths_have_nothing_left_to_separate_them() {
        let one = space("s1", "mac", "/Users/brede/dev/attn");
        let two = space("s2", "mac", "/Users/brede/dev/attn");
        let titles = space_titles(&[(&one, Some("brede/attn")), (&two, Some("brede/attn"))]);
        assert_eq!(titles[0], titles[1]);
    }

    #[test]
    fn a_rename_that_collides_is_disambiguated_too() {
        let mut one = space("s1", "mac", "/Users/brede/dev/attn");
        let mut two = space("s2", "mac", "/Users/brede/scratch/attn-2");
        one.name = Some("Attn".into());
        two.name = Some("Attn".into());
        let titles = space_titles(&[(&one, None), (&two, None)]);
        assert_eq!(
            titles.iter().map(SpaceTitle::line).collect::<Vec<_>>(),
            ["Attn · attn", "Attn · attn-2"]
        );
    }

    // -- one full row per chat (gh#138) -------------------------------------

    fn chat(id: &str, space_id: Option<&str>) -> Chat {
        Chat {
            creation_state: crate::ChatCreationState::Ready,
            id: id.into(),
            device_id: "mac".into(),
            title: Some(format!("chat {id}")),
            archived: false,
            cwd: None,
            branch: None,
            checkout_id: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: space_id.map(str::to_string),
            last_seen_at: None,
            forked_from: None,
        }
    }

    fn agent_row(chat_id: &str) -> ActiveRow {
        ActiveRow::Agent(super::super::board::AgentRow {
            task_id: format!("t-{chat_id}"),
            chat_id: chat_id.into(),
            identifier: "gh#25".into(),
            slug: None,
            branch: None,
            state: super::super::board::AgentState::Working,
            started_at: None,
            cap_secs: None,
            activity: None,
        })
    }

    #[test]
    fn placements_find_only_spaces_the_current_watch_knows() {
        let chats = vec![chat("c1", Some("s1")), chat("c2", None)];
        let active = vec![agent_row("c1"), agent_row("c2"), agent_row("gone")];
        let spaces = vec![space("s1", "local", "/s1")];
        let placements = active_placements(&active, &chats, &spaces);
        assert_eq!(
            placements,
            [("c1", Some("s1")), ("c2", None), ("gone", None)]
        );
    }

    #[test]
    fn a_dangling_space_reference_falls_back_to_active() {
        let chats = vec![chat("dangling", Some("not-in-the-space-watch"))];
        let active = vec![agent_row("dangling")];
        let spaces = vec![space("known", "local", "/known")];

        let placements = active_placements(&active, &chats, &spaces);

        assert_eq!(placements, [("dangling", None)]);
        assert_eq!(homeless_live(&placements), ["dangling"]);
    }

    #[test]
    fn a_selected_space_with_an_orchestrator_stays_disclosed() {
        assert!(space_disclosure_forced_open(true, true));
        assert!(!space_disclosure_forced_open(true, false));
        assert!(!space_disclosure_forced_open(false, true));
    }

    #[test]
    fn the_shelf_shows_what_active_is_not_showing() {
        let active = [("c1", Some("s1")), ("c2", Some("s1"))];
        let shelf = space_shelf("s1", ["c1", "c2", "c3"], &active);
        assert_eq!(shelf.idle, ["c3"]);
        assert_eq!(shelf.running, 2);
        // Rows remain: the count still links the surfaces, but the shelf has
        // its own truth to draw and says nothing extra.
        assert_eq!(shelf_note(&shelf), None);
        assert_eq!(running_label(shelf.running).as_deref(), Some("· 2 running"));
    }

    #[test]
    fn a_wholly_active_space_says_so_instead_of_a_gap() {
        let active = [("c1", Some("s1")), ("c2", Some("s1"))];
        let shelf = space_shelf("s1", ["c1", "c2"], &active);
        assert!(shelf.idle.is_empty());
        assert_eq!(
            shelf_note(&shelf).as_deref(),
            Some("2 running above · no idle sessions")
        );
    }

    #[test]
    fn an_empty_space_keeps_its_own_empty_state() {
        let shelf = space_shelf("s1", Vec::<&str>::new(), &[]);
        assert_eq!(shelf.running, 0);
        assert_eq!(shelf_note(&shelf), None);
        assert_eq!(running_label(0), None);
    }

    #[test]
    fn another_spaces_runs_are_never_counted_here() {
        let active = [("c1", Some("other")), ("c2", Some("s1"))];
        let shelf = space_shelf("s1", ["c2", "c3"], &active);
        assert_eq!(shelf.idle, ["c3"]);
        assert_eq!(shelf.running, 1);
    }

    #[test]
    fn a_space_claims_its_live_rows_in_actives_own_order() {
        // gh#258: Spaces is the primary group, so a live run's row is drawn
        // under the space it belongs to. The ORDER is Active's — needs-you
        // first, then working — and it survives the split.
        let active = [
            ("blocked", Some("s1")),
            ("elsewhere", Some("s2")),
            ("working", Some("s1")),
            ("loose", None),
        ];
        assert_eq!(space_live("s1", &active), ["blocked", "working"]);
        assert_eq!(space_live("s2", &active), ["elsewhere"]);
        assert!(space_live("s3", &active).is_empty());
    }

    #[test]
    fn only_a_spaceless_run_still_needs_a_group_of_its_own() {
        let active = [("in-a-space", Some("s1")), ("loose", None)];
        assert_eq!(homeless_live(&active), ["loose"]);
        // And when every run has a home the group disappears entirely, rather
        // than sitting empty above the tree it used to displace.
        assert!(homeless_live(&[("in-a-space", Some("s1"))]).is_empty());
    }

    #[test]
    fn an_archived_run_counts_even_though_no_shelf_row_holds_it() {
        // Archiving is a decision about a FINISHED chat; one that is working
        // anyway stays in Active and out of the tab order. The space row must
        // still admit it.
        let active = [("archived", Some("s1"))];
        let shelf = space_shelf("s1", ["c1"], &active);
        assert_eq!(shelf.idle, ["c1"]);
        assert_eq!(shelf.running, 1);
    }
}
