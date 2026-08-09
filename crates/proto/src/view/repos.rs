//! The repo-first space picker's list (gh#118).
//!
//! The space picker used to ask "which folder on which machine?", which is a
//! question about infrastructure. For this fork the mental model is the other
//! way round — work lives in GitHub repos and the box is where they run — so the
//! picker asks "which repo?" and everything else follows from the answer: a repo
//! that already has a space opens it, wherever it lives; a repo that has none
//! gets onboarded onto the box.
//!
//! That list is a union of two things neither frontend can compute alone:
//!
//! - **The spaces**, from the workspace doc every viewport already holds. A
//!   space knows its device and its folder but not its *repo* — only the device
//!   holding the checkout can ask git — so the host supplies the `space → slug`
//!   links (`ListRepoSpaces`, gh#118) and this joins them.
//! - **The App's grant**, the repos the board could clone and poll
//!   (`ListAppRepos`, gh#97). The ones with no space yet are the picker's other
//!   half: "not connected yet".
//!
//! Pure and shared for the same reason the rest of [`crate::view`] is: comet has
//! three frontends and a picker that ordered or deduplicated its rows
//! differently on each of them would be three different products.

use crate::Space;

/// One of the host's spaces, resolved to the GitHub repo it is a checkout of.
///
/// Produced by the board host (`comet_board::onboard::space_links`) because the
/// answer lives on its disk, and named here because it is a wire shape three
/// frontends read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpaceSlug {
    pub space_id: String,
    /// `owner/repo`, from the space's git remote.
    pub slug: String,
}

/// A repo the board's GitHub App can see — the App's grant, flattened to what a
/// picker row needs.
///
/// The App's grant rather than the operator's repos, deliberately (gh#97): it is
/// exactly the set the box can clone and the sync loop can poll, so a repo
/// missing from it is one somebody has to go and install the App on, not one the
/// picker should offer and then fail on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoOffer {
    pub slug: String,
    pub private: bool,
    pub archived: bool,
    /// The board already both polls and routes it (`missing == null`).
    pub on_board: bool,
}

/// One row of the picker.
///
/// A row is *either* a space that exists (with a repo name when the host could
/// resolve one) *or* a repo with no space yet — [`RepoRow::space_id`] is the
/// difference, and it is also what picking the row does: open the space, or
/// onboard the repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRow {
    /// Stable identity for keyed rendering and keyboard selection: the slug when
    /// there is one, else the space id. Never collides — a slug always contains
    /// `/` and a space id never does.
    pub key: String,
    /// What the row is called: the repo, when it is known to be one. A space
    /// whose folder is not a GitHub checkout keeps its own name, because
    /// inventing an `owner/` for it would be a lie about where it came from.
    pub title: String,
    pub slug: Option<String>,
    /// The space this row opens. `None` = the row onboards instead.
    pub space_id: Option<String>,
    /// The device that space lives on. The picker never asks for one: a space
    /// knows its own host, and an onboard targets the board's.
    pub device_id: Option<String>,
    pub private: bool,
    pub archived: bool,
    /// The board polls and routes this repo, so its issues are on the board.
    pub on_board: bool,
    /// The space lives on the board's host — the box. Sorts first: it is where
    /// the work runs, and on a phone it is the only place it can.
    pub on_host: bool,
}

impl RepoRow {
    /// Picking this row opens a space rather than cloning one.
    pub fn connected(&self) -> bool {
        self.space_id.is_some()
    }

    /// The repo-side facts worth a word in the row's subtitle, if any.
    ///
    /// Not the device — that is the frontend's to name, since only it holds the
    /// device list — and not a status for the ordinary case: a connected repo
    /// whose issues are on the board is what the picker is *for*, and labelling
    /// every such row would leave nothing for the exceptions to say.
    pub fn note(&self) -> Option<String> {
        let mut parts: Vec<&str> = Vec::new();
        if self.private {
            parts.push("private");
        }
        if self.archived {
            parts.push("archived");
        }
        if !self.connected() {
            parts.push("not connected yet");
        } else if self.slug.is_some() && !self.on_board {
            // A space that is a GitHub checkout the board does not watch: its
            // issues are not on the board, which is exactly the surprise this
            // picker exists to prevent somebody discovering later.
            parts.push("not on the board");
        }
        (!parts.is_empty()).then(|| parts.join(" · "))
    }
}

/// The picker's list: every space, plus every offered repo that has none.
///
/// Order is spaces first (the box's before anybody else's, then alphabetical),
/// then the repos with no space. Rows you can open outrank rows that would make
/// you wait for a clone, and among the openable ones the box outranks the
/// laptop, because the box is where work actually runs.
///
/// `host` is the board's device — `None` when the sweep has not found one, in
/// which case nothing is "on the box" and the spaces simply sort alphabetically.
pub fn repo_rows(
    spaces: &[Space],
    links: &[SpaceSlug],
    offers: &[RepoOffer],
    host: Option<&str>,
) -> Vec<RepoRow> {
    let slug_of = |space_id: &str| {
        links
            .iter()
            .find(|l| l.space_id == space_id)
            .map(|l| l.slug.clone())
    };
    let offer_of = |slug: &str| {
        offers
            .iter()
            .find(|o| o.slug.eq_ignore_ascii_case(slug))
            .cloned()
    };

    let mut rows: Vec<RepoRow> = spaces
        .iter()
        .map(|space| {
            let slug = slug_of(&space.id);
            let offer = slug.as_deref().and_then(offer_of);
            RepoRow {
                key: slug.clone().unwrap_or_else(|| space.id.clone()),
                title: slug
                    .clone()
                    .unwrap_or_else(|| space.display_name().to_string()),
                slug,
                space_id: Some(space.id.clone()),
                device_id: Some(space.device_id.clone()),
                private: offer.as_ref().is_some_and(|o| o.private),
                archived: offer.as_ref().is_some_and(|o| o.archived),
                on_board: offer.as_ref().is_some_and(|o| o.on_board),
                on_host: host.is_some_and(|h| h == space.device_id),
            }
        })
        .collect();
    // Two spaces on one repo — a checkout and a clone of it — are two places to
    // work, so both stay. Only the *offer* is deduplicated against them, below.
    rows.sort_by(|a, b| {
        b.on_host
            .cmp(&a.on_host)
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
            .then_with(|| a.key.cmp(&b.key))
    });

    let connected: Vec<String> = rows
        .iter()
        .filter_map(|r| r.slug.as_ref().map(|s| s.to_lowercase()))
        .collect();
    let mut unconnected: Vec<RepoRow> = offers
        .iter()
        .filter(|o| !connected.contains(&o.slug.to_lowercase()))
        .map(|o| RepoRow {
            key: o.slug.clone(),
            title: o.slug.clone(),
            slug: Some(o.slug.clone()),
            space_id: None,
            device_id: None,
            private: o.private,
            archived: o.archived,
            on_board: o.on_board,
            on_host: false,
        })
        .collect();
    unconnected.sort_by_key(|r| r.title.to_lowercase());
    rows.append(&mut unconnected);
    rows
}

/// A device the host sweep asked and never heard back from (gh#155).
///
/// Not the same thing as a device that hosts no board. That one *refuses*
/// `ListRepoSpaces` before doing any work, and being refused is an answer: it
/// is not a host, and it belongs nowhere in this picker. A device whose call
/// errored — relay down, 500, timeout — was never successfully asked, so
/// whatever it hosts is missing from the list and the list is not the whole
/// truth. The picker has to say which one this is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreachableHost {
    /// What to call it: the device's name where the frontend knows one, its id
    /// otherwise. Named, never counted — "1 device unreachable" is a fact
    /// nobody can act on.
    pub device: String,
    /// The transport's own words, for the line under the headline.
    pub detail: Option<String>,
    /// The fleet already believed this device was not there when it was asked
    /// (`HostPresence::Offline` — a lapsed heartbeat observed by a viewer whose
    /// own sync is up, gh#126/gh#145).
    ///
    /// Silence from a device nobody expected to answer is not news. A laptop
    /// shut for the night fails the same call as a box behind a broken relay,
    /// and treating them alike puts a warning on the palette every single time
    /// it opens — which is how a warning stops being read before the day it
    /// matters. `SyncDown` is deliberately NOT this: a viewer that cannot hear
    /// does not get to call anything absent, and that is exactly the state the
    /// outage behind this issue produced.
    pub believed_offline: bool,
}

/// The picker's headline for a sweep that lost a device it had every reason to
/// expect an answer from — `None` when there is no such device.
///
/// Names them, because the operator's next move is device-shaped: check the
/// relay, retry. And says what the silence cost — the repos that are missing —
/// since the visible list looks complete either way, which is the whole failure
/// this exists to prevent.
pub fn unreachable_note(hosts: &[UnreachableHost]) -> Option<String> {
    let names: Vec<&str> = hosts
        .iter()
        .filter(|h| !h.believed_offline)
        .map(|h| h.device.as_str())
        .collect();
    let names = join_names(&names)?;
    Some(format!(
        "Could not reach {names} — any repos hosted there are missing from this list."
    ))
}

/// The quiet counterpart: devices that were already known to be away.
///
/// A statement of the fleet's state, not a warning, and it carries no Retry —
/// retrying a device that is switched off cannot succeed. Frontends only pass
/// devices whose silence *costs* something (a device that holds spaces here);
/// an offline phone that hosts nothing has nothing to be missing.
pub fn absent_note(hosts: &[UnreachableHost]) -> Option<String> {
    let names: Vec<&str> = hosts
        .iter()
        .filter(|h| h.believed_offline)
        .map(|h| h.device.as_str())
        .collect();
    let plural = names.len() > 1;
    let names = join_names(&names)?;
    Some(format!(
        "{names} {} offline — repos hosted there are not listed.",
        if plural { "are" } else { "is" }
    ))
}

/// "a", "a and b", "a, b and c" — an empty list has nothing to say.
fn join_names(names: &[&str]) -> Option<String> {
    match names {
        [] => None,
        [one] => Some((*one).to_string()),
        [rest @ .., last] => Some(format!("{} and {last}", rest.join(", "))),
    }
}

/// Match rank of one row against a query: `0` prefix, `1` substring, `None` no
/// match. Case-insensitive; an empty query matches everything at rank 1.
///
/// Both the repo name alone and the whole `owner/repo` count as prefixes:
/// somebody looking for `comet-board` types `comet`, not `bredebjorhovd/comet`,
/// and a picker that made them type the owner first would be the folder browser
/// again with extra steps.
pub fn row_rank(query: &str, row: &RepoRow) -> Option<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(1);
    }
    let title = row.title.to_lowercase();
    let name = title.rsplit('/').next().unwrap_or(&title).to_string();
    if name.starts_with(&query) || title.starts_with(&query) {
        Some(0)
    } else if title.contains(&query) {
        Some(1)
    } else {
        None
    }
}

/// Filter + rank the rows for a search query, preserving [`repo_rows`]'s order
/// within each rank.
pub fn filter_rows(query: &str, rows: &[RepoRow]) -> Vec<RepoRow> {
    let mut ranked: Vec<(usize, usize, &RepoRow)> = rows
        .iter()
        .enumerate()
        .filter_map(|(ix, row)| row_rank(query, row).map(|rank| (rank, ix, row)))
        .collect();
    ranked.sort_by_key(|&(rank, ix, _)| (rank, ix));
    ranked.into_iter().map(|(_, _, row)| row.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};

    fn space(id: &str, device: &str, path: &str) -> Space {
        Space {
            id: id.into(),
            device_id: device.into(),
            path: path.into(),
            name: None,
            git_detected: true,
            git_checked_at: None,
            checkout_id: None,
            branch: None,
            created_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        }
    }

    fn link(space_id: &str, slug: &str) -> SpaceSlug {
        SpaceSlug {
            space_id: space_id.into(),
            slug: slug.into(),
        }
    }

    fn offer(slug: &str, on_board: bool) -> RepoOffer {
        RepoOffer {
            slug: slug.into(),
            private: false,
            archived: false,
            on_board,
        }
    }

    /// The whole point of the fork's picker: the box's repos first, everything
    /// else you could open next, and the repos you have not connected last —
    /// because picking one of those means waiting for a clone.
    #[test]
    fn the_box_comes_first_then_other_devices_then_repos_with_no_space() {
        let spaces = vec![
            space("s-laptop", "dev-laptop", "/Users/b/src/scratch"),
            space("s-box-z", "dev-box", "/box/src/zebra"),
            space("s-box-a", "dev-box", "/box/src/comet-board"),
        ];
        let links = vec![
            link("s-box-a", "bredebjorhovd/comet-board"),
            link("s-box-z", "bredebjorhovd/zebra"),
        ];
        let offers = vec![
            offer("bredebjorhovd/comet-board", true),
            offer("bredebjorhovd/itsm-agent", false),
        ];
        let rows = repo_rows(&spaces, &links, &offers, Some("dev-box"));
        let keys: Vec<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "bredebjorhovd/comet-board",
                "bredebjorhovd/zebra",
                "s-laptop",
                "bredebjorhovd/itsm-agent",
            ]
        );
        // A repo that already has a space is offered once — as the space, which
        // is the row that opens rather than clones.
        assert!(rows[0].connected());
        assert_eq!(rows[0].device_id.as_deref(), Some("dev-box"));
        assert!(!rows[3].connected());
        assert_eq!(rows[3].note().as_deref(), Some("not connected yet"));
    }

    /// A space whose folder is not a GitHub checkout is still somewhere to work.
    /// It keeps its own name rather than being dropped or given an invented one.
    #[test]
    fn a_space_that_is_not_a_github_checkout_keeps_its_folder_name() {
        let spaces = vec![space("s-notes", "dev-box", "/box/notes")];
        let rows = repo_rows(&spaces, &[], &[], Some("dev-box"));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "notes");
        assert_eq!(rows[0].slug, None);
        assert!(rows[0].connected());
        // Nothing to say about a repo it is not claiming to be.
        assert_eq!(rows[0].note(), None);
    }

    /// The picker's promise is that picking a repo puts its issues on the board.
    /// A space the board is not watching breaks that promise quietly, so the row
    /// says so.
    #[test]
    fn a_connected_repo_the_board_does_not_watch_says_so() {
        let spaces = vec![space("s-a", "dev-box", "/box/src/a")];
        let links = vec![link("s-a", "o/a")];
        let rows = repo_rows(&spaces, &links, &[offer("o/a", false)], Some("dev-box"));
        assert_eq!(rows[0].note().as_deref(), Some("not on the board"));

        let rows = repo_rows(&spaces, &links, &[offer("o/a", true)], Some("dev-box"));
        assert_eq!(rows[0].note(), None);
    }

    /// Search is over the repo name people actually say, not the owner they
    /// rarely do.
    #[test]
    fn typing_the_repo_name_finds_it_without_the_owner() {
        let offers = vec![
            offer("bredebjorhovd/comet-board", false),
            offer("acme/widgets", false),
            offer("comet-labs/other", false),
        ];
        let rows = repo_rows(&[], &[], &offers, None);
        let matched = filter_rows("comet", &rows);
        let hits: Vec<&str> = matched.iter().map(|r| r.key.as_str()).collect();
        // The owner-prefix match ranks alongside the name match; the unrelated
        // repo is gone.
        assert_eq!(hits, vec!["bredebjorhovd/comet-board", "comet-labs/other"]);
        assert!(filter_rows("zzz", &rows).is_empty());
        assert_eq!(filter_rows("", &rows).len(), 3);
    }

    fn unreachable(device: &str) -> UnreachableHost {
        UnreachableHost {
            device: device.into(),
            detail: None,
            believed_offline: false,
        }
    }

    fn absent(device: &str) -> UnreachableHost {
        UnreachableHost {
            believed_offline: true,
            ..unreachable(device)
        }
    }

    /// The gh#155 failure: the box was asked, the relay 500'd, and the picker
    /// showed the laptop's repos as if that were everything. A sweep that lost
    /// a device says so, by name, and says what it cost.
    #[test]
    fn a_sweep_that_lost_a_device_names_it() {
        assert_eq!(unreachable_note(&[]), None, "a complete sweep says nothing");
        assert_eq!(
            unreachable_note(&[unreachable("box")]).as_deref(),
            Some("Could not reach box — any repos hosted there are missing from this list.")
        );
        assert_eq!(
            unreachable_note(&[unreachable("box"), unreachable("MacBook Pro")]).as_deref(),
            Some(
                "Could not reach box and MacBook Pro — any repos hosted there are missing \
                 from this list."
            )
        );
        assert_eq!(
            unreachable_note(&[unreachable("a"), unreachable("b"), unreachable("c")]).as_deref(),
            Some("Could not reach a, b and c — any repos hosted there are missing from this list.")
        );
    }

    /// A device the fleet already believes is away is absent, not unreachable.
    /// It never reaches the warning line — an asleep phone fails this call on
    /// every single open, and a warning that is always on is a warning nobody
    /// reads on the day the box is the one that went quiet.
    #[test]
    fn a_device_known_to_be_offline_is_not_a_warning() {
        assert_eq!(unreachable_note(&[absent("iPhone")]), None);
        assert_eq!(
            absent_note(&[absent("iPhone")]).as_deref(),
            Some("iPhone is offline — repos hosted there are not listed.")
        );
        assert_eq!(
            absent_note(&[absent("box"), absent("iPhone")]).as_deref(),
            Some("box and iPhone are offline — repos hosted there are not listed.")
        );

        // Both at once: the box was expected to answer and did not, while the
        // phone was never expected to. Two different sentences, and only the
        // first one is a warning.
        let mixed = [unreachable("box"), absent("iPhone")];
        assert_eq!(
            unreachable_note(&mixed).as_deref(),
            Some("Could not reach box — any repos hosted there are missing from this list.")
        );
        assert_eq!(
            absent_note(&mixed).as_deref(),
            Some("iPhone is offline — repos hosted there are not listed.")
        );
        assert_eq!(absent_note(&[unreachable("box")]), None);
    }

    /// With no board host found, nothing is "on the box" — the list still works,
    /// it just has no reason to prefer one device.
    #[test]
    fn without_a_host_the_spaces_are_simply_alphabetical() {
        let spaces = vec![
            space("s-b", "dev-laptop", "/b/beta"),
            space("s-a", "dev-box", "/a/alpha"),
        ];
        let rows = repo_rows(&spaces, &[], &[], None);
        let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(titles, vec!["alpha", "beta"]);
        assert!(rows.iter().all(|r| !r.on_host));
    }
}
