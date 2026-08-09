//! The claim contract, and the remainder it exists to expose (§gh#183).
//!
//! A summary written by the model that wrote the code inherits its blind spots:
//! a misunderstanding comes back described fluently and confidently, and prose
//! marks its own homework. So the board does not ask an agent for a summary. It
//! asks for **claims** — a sentence and the paths it is about — and then
//! computes, from the diff, the set of changes no claim accounts for.
//!
//! That remainder is the product. A dependency nobody mentioned and a function
//! edited in passing are exactly what a reader would have missed, and they are
//! the one part of a review that has to be derived rather than asserted: an
//! agent asked "what did you not account for" answers from the same blind spot
//! it wrote the code with.
//!
//! ## Three rules, and each one is load-bearing
//!
//! 1. **A claim without an anchor is not a claim.** [`parse`] refuses a line
//!    with no `::` and no paths after it, naming the line. The refusal is the
//!    contract: prose is free to exist, it just cannot be submitted here.
//! 2. **The anchors are matched, never trusted.** A path a claim names that the
//!    diff never touched comes back in
//!    [`Remainder::unmatched_anchors`](Remainder) — an anchor that matches
//!    nothing is a claim about work that did not happen, and it is at least as
//!    interesting as an unclaimed file.
//! 3. **The remainder comes off the diff.** Never off the claims, never off
//!    anything the agent said. [`remainder`] takes the changed set as an
//!    argument for exactly that reason: the caller reads it from git.
//!
//! ## The format
//!
//! One claim per line:
//!
//! ```text
//! <what you did> :: <path> [<path>…]
//! ```
//!
//! `::` is the separator and the **last** one on the line wins, because a
//! sentence about `Db::open` is a sentence somebody will write. Paths are
//! separated by whitespace or commas, are repo-relative, and may name a
//! directory — `crates/board/src/` accounts for every changed file under it,
//! which is the honest spelling for "I rewrote this module" and is visibly
//! coarser than naming the files.
//!
//! A leading `-` or `*` bullet is tolerated: agents write lists, and refusing
//! one over its punctuation teaches nothing.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::evidence::RunEvidence;
use crate::model::{Attempt, Task};

/// What separates a claim's sentence from the paths it is about.
pub const ANCHOR: &str = "::";

/// How many claims one attempt may submit. Generous — a large branch honestly
/// described is a dozen — and here to bound a column the board reads on every
/// review, not to ration the contract.
pub const MAX_CLAIMS: usize = 100;

/// How long one claim's sentence may be. A claim is a sentence; a paragraph
/// here is prose wearing an anchor.
pub const MAX_TEXT: usize = 400;

/// One claim: a sentence, and the paths it is about.
///
/// `files` is never empty — [`parse`] is the only way to build one from agent
/// input and it refuses an anchorless claim outright, so anything stored
/// against an attempt is checkable by construction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub text: String,
    pub files: Vec<String>,
}

/// One file the attempt's branch touched, as git reports it.
///
/// Renames are read with `--no-renames` on purpose: to a reviewer a rename is
/// two paths that both changed, and collapsing them to one hides the arrival of
/// the new name — which is precisely the kind of thing an unclaimed set is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    /// git's status letter: `A` added, `M` modified, `D` deleted.
    pub status: String,
    /// Lines added and removed. Both zero for a binary file, which git reports
    /// as `-` and this records as nothing rather than as an empty change.
    pub added: u32,
    pub removed: u32,
    /// A binary file, which has no line counts to give.
    #[serde(default)]
    pub binary: bool,
}

/// One claim, checked against the diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimView {
    pub text: String,
    /// The paths the claim named, as submitted.
    pub files: Vec<String>,
    /// Changed files those paths account for.
    pub matched: Vec<String>,
    /// Paths the claim named that the diff does not contain. Not an error and
    /// not dropped: a claim anchored to a file nothing happened to is a claim
    /// that cannot be checked, and saying so is the point.
    pub unmatched: Vec<String>,
}

impl ClaimView {
    /// Did anything this claim named actually change? A claim where nothing
    /// did is the review's second-loudest row, after the unclaimed set.
    pub fn anchored(&self) -> bool {
        !self.matched.is_empty()
    }
}

/// The claims, and everything they do not account for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remainder {
    pub claims: Vec<ClaimView>,
    /// **The output.** Changed files no claim names, in the diff's own order.
    pub unclaimed: Vec<ChangedFile>,
    /// Paths claims named that the diff never touched, deduplicated across
    /// claims.
    pub unmatched_anchors: Vec<String>,
    /// How many changed files at least one claim accounts for. With
    /// `unclaimed.len()` this is the whole diff, so a surface can render the
    /// proportion without re-deriving it.
    pub claimed: usize,
}

impl Remainder {
    /// Is every changed file accounted for? The only state in which the claims
    /// list is the whole story.
    pub fn complete(&self) -> bool {
        self.unclaimed.is_empty()
    }
}

/// Where the changed set came from — and, when it could not be had, why.
///
/// A review that quietly rendered an empty diff would say "nothing changed"
/// about a collected checkout, which is the opposite of true. The variant
/// travels with the review so the surface can say which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum DiffSource {
    /// Read from the attempt's checkout, just now.
    Checkout,
    /// The snapshot the board recorded while the attempt was still live. What
    /// answers after `gc` reclaims the worktree (gh#72) — the reason the
    /// snapshot is taken at all.
    Recorded,
    /// Neither: no checkout on disk and nothing recorded.
    Unavailable { reason: String },
}

/// What the agent was asked to do — the left-hand side of a review.
///
/// The issue, not the interpolated dispatch prompt: the prompt is a template
/// over exactly these fields plus the board's conventions, and it is not stored
/// anywhere an attempt can be read back from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub body: Option<String>,
}

/// Everything a review of one attempt is made of (§gh#183).
///
/// Assembled by [`review`] and printed by `comet-board review --json`. The
/// order of the fields is the order the question is asked in: what was asked,
/// what the agent says it did, what the board can see for itself, and what
/// nobody accounted for.
///
/// Serialized `snake_case` throughout, like [`crate::rows::TaskRow`] and unlike
/// the RPC *params* around it: the same orchestrating agents read `list --json`
/// and this, and one object changing case halfway through a CLI is a papercut
/// nobody should have to remember.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptReview {
    pub task_id: String,
    pub attempt: i64,
    /// Which attempt of the task this is, 1-based — a retry makes new claims,
    /// and "attempt 3" is how every other board surface names them.
    pub attempt_number: usize,
    pub state: String,
    pub outcome: Option<String>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub pr_url: Option<String>,
    pub brief: Brief,
    /// When the agent submitted its claims. `None` with an empty claims list
    /// means it never answered the contract at all, which is a different fact
    /// from claiming nothing and must not render as one.
    pub claimed_at: Option<String>,
    #[serde(flatten)]
    pub remainder: Remainder,
    /// Every file the branch touched — the denominator behind the remainder.
    pub changed: Vec<ChangedFile>,
    pub diff: DiffSource,
    /// Files changed in the checkout but not committed, right now.
    ///
    /// Not part of the diff and not claimable: the remainder is about the
    /// **branch**, which is what a reviewer can fetch. It is reported because
    /// of what it means at submission time — an agent that claims before it
    /// commits would otherwise be told every one of its changes is accounted
    /// for, having shown the board none of them. `None` when there was no
    /// checkout to ask.
    pub uncommitted: Option<u32>,
    pub evidence: RunEvidence,
}

impl AttemptReview {
    /// Did the agent answer the claim contract at all?
    pub fn claimed(&self) -> bool {
        self.claimed_at.is_some()
    }
}

// ---- the format ----------------------------------------------------------

/// Parse a submitted block into claims, refusing anything without an anchor.
///
/// `worktree` is the attempt's checkout, used only to make an absolute path
/// repo-relative — an agent that pastes the path it has been typing all session
/// has anchored its claim correctly, and rejecting it would be pedantry about a
/// prefix the board itself handed out.
///
/// Every refusal names the line. A contract enforced with "invalid input" is a
/// contract nobody can satisfy on the second try.
pub fn parse(input: &str, worktree: Option<&str>) -> Result<Vec<Claim>> {
    let mut claims = Vec::new();
    for raw in input.lines() {
        let line = raw
            .trim()
            .trim_start_matches(['-', '*', '•'])
            .trim_start_matches(char::is_whitespace);
        if line.is_empty() {
            continue;
        }
        // The LAST separator: `Db::open` in the sentence is ordinary, a `::` in
        // a path is not.
        let Some(split) = line.rfind(ANCHOR) else {
            bail!(
                "no `{ANCHOR}` in: {line}\n\
                 A claim is a sentence and the paths it is about — \
                 `what you did {ANCHOR} path/one.rs path/two.rs`. \
                 Without an anchor it is prose, and prose is not checkable."
            );
        };
        let text = line[..split].trim();
        let files: Vec<String> = line[split + ANCHOR.len()..]
            .split([',', ' ', '\t'])
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(|p| normalize(p, worktree))
            .collect();
        if text.is_empty() {
            bail!("a claim with paths and nothing said about them: {line}");
        }
        if files.is_empty() {
            bail!(
                "no paths after `{ANCHOR}` in: {line}\n\
                 Name the files the claim is about; a claim nothing anchors \
                 cannot be checked against the diff."
            );
        }
        if text.chars().count() > MAX_TEXT {
            bail!(
                "a claim of {} characters is a paragraph, not a claim (max {MAX_TEXT}): {}…",
                text.chars().count(),
                text.chars().take(60).collect::<String>()
            );
        }
        claims.push(Claim {
            text: text.to_string(),
            files: dedup(files),
        });
    }
    if claims.is_empty() {
        bail!(
            "nothing to record. One claim per line: \
             `what you did {ANCHOR} path/one.rs path/two.rs`."
        );
    }
    if claims.len() > MAX_CLAIMS {
        bail!(
            "{} claims is more than one attempt can be about (max {MAX_CLAIMS})",
            claims.len()
        );
    }
    Ok(claims)
}

/// A submitted path as the diff spells it: repo-relative, forward slashes, no
/// `./` and no trailing separator.
pub fn normalize(path: &str, worktree: Option<&str>) -> String {
    let path = path.trim().trim_matches(['`', '"', '\'']);
    let path = path.replace('\\', "/");
    let relative = worktree
        .map(|w| w.trim_end_matches('/'))
        .filter(|w| !w.is_empty())
        .and_then(|w| path.strip_prefix(w))
        .map(|rest| rest.trim_start_matches('/'))
        .unwrap_or(&path);
    relative
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn dedup(mut paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
    paths
}

// ---- the remainder -------------------------------------------------------

/// Does this claim path account for this changed file?
///
/// Exact match, or a directory the file sits under. Deliberately **not** a
/// basename match: an agent that writes `db.rs` for `crates/board/src/db.rs`
/// has not anchored anything — three crates here have a `db.rs` — and the
/// generous reading would silently claim files nobody looked at, which is the
/// whole failure this design is built around.
pub fn accounts_for(claim_path: &str, changed: &str) -> bool {
    if claim_path.is_empty() {
        return false;
    }
    if claim_path == changed {
        return true;
    }
    changed
        .strip_prefix(claim_path)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Map claims onto the diff and return what they do not account for.
///
/// `changed` is the branch's diff, read from git by the caller — never from
/// anything the agent submitted. That separation is the point of the whole
/// module: an agent cannot widen its own denominator.
pub fn remainder(claims: &[Claim], changed: &[ChangedFile]) -> Remainder {
    let mut views = Vec::with_capacity(claims.len());
    let mut accounted = vec![false; changed.len()];
    let mut unmatched_anchors: Vec<String> = Vec::new();

    for claim in claims {
        let mut matched = Vec::new();
        let mut unmatched = Vec::new();
        for path in &claim.files {
            let hits: Vec<String> = changed
                .iter()
                .enumerate()
                .filter(|(_, f)| accounts_for(path, &f.path))
                .map(|(ix, f)| {
                    accounted[ix] = true;
                    f.path.clone()
                })
                .collect();
            if hits.is_empty() {
                unmatched.push(path.clone());
                if !unmatched_anchors.contains(path) {
                    unmatched_anchors.push(path.clone());
                }
            } else {
                for hit in hits {
                    if !matched.contains(&hit) {
                        matched.push(hit);
                    }
                }
            }
        }
        views.push(ClaimView {
            text: claim.text.clone(),
            files: claim.files.clone(),
            matched,
            unmatched,
        });
    }

    let unclaimed: Vec<ChangedFile> = changed
        .iter()
        .zip(&accounted)
        .filter(|(_, done)| !**done)
        .map(|(f, _)| f.clone())
        .collect();
    Remainder {
        claims: views,
        claimed: changed.len() - unclaimed.len(),
        unclaimed,
        unmatched_anchors,
    }
}

/// Assemble the whole review from the pieces the caller has gathered.
///
/// Pure, and takes the diff rather than reading it, so the ranking and the
/// remainder are testable without a checkout — the same split
/// [`crate::settled::decide`] makes for the settle hierarchy.
pub fn review(
    task: &Task,
    attempt: &Attempt,
    changed: Vec<ChangedFile>,
    diff: DiffSource,
    uncommitted: Option<u32>,
    evidence: RunEvidence,
) -> AttemptReview {
    let remainder = remainder(&attempt.claims, &changed);
    AttemptReview {
        task_id: task.id.clone(),
        attempt: attempt.id,
        attempt_number: task
            .attempts
            .iter()
            .position(|a| a.id == attempt.id)
            .map(|ix| ix + 1)
            // An attempt read on its own, outside its task's list. Its own id
            // is not a count, so saying nothing beats saying a wrong number.
            .unwrap_or(0),
        state: task.state.as_str().to_string(),
        outcome: attempt.outcome.map(|o| o.as_str().to_string()),
        branch: attempt.branch.clone(),
        worktree: attempt.worktree.clone(),
        pr_url: task.pr_url.clone(),
        brief: Brief {
            identifier: task.identifier.clone(),
            title: task.title.clone(),
            url: task.url.clone(),
            body: task
                .body
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(str::to_string),
        },
        claimed_at: attempt.claims_at.clone(),
        remainder,
        changed,
        diff,
        uncommitted,
        evidence,
    }
}

// ---- reading the diff ----------------------------------------------------

/// The changed set from `git diff --numstat` + `--name-status` output.
///
/// Two commands rather than one because git will not print both at once, and
/// the review wants both: the status letter says whether a file arrived or
/// left, and the line counts are what make an unclaimed row readable at a
/// glance. The numstat is authoritative for membership — a path only
/// name-status knows about is still listed, with no counts, rather than
/// dropped.
pub fn parse_diff(numstat: &str, name_status: &str) -> Vec<ChangedFile> {
    let mut statuses: std::collections::HashMap<&str, String> = Default::default();
    for line in name_status.lines() {
        let mut parts = line.split('\t');
        let (Some(status), Some(path)) = (parts.next(), parts.next_back()) else {
            continue;
        };
        let status = status.trim();
        if status.is_empty() || path.is_empty() {
            continue;
        }
        statuses.insert(path, status.to_string());
    }
    let mut out = Vec::new();
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        // git prints `-` for both counts on a binary file.
        let binary = added == "-" || removed == "-";
        out.push(ChangedFile {
            path: path.to_string(),
            status: statuses
                .get(path)
                .cloned()
                .unwrap_or_else(|| "M".to_string()),
            added: added.parse().unwrap_or(0),
            removed: removed.parse().unwrap_or(0),
            binary,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BoardState, Outcome, Source, UpstreamState};

    fn changed(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            status: "M".into(),
            added: 10,
            removed: 2,
            binary: false,
        }
    }

    // ---- the format ------------------------------------------------------

    #[test]
    fn a_claim_is_a_sentence_and_the_paths_it_is_about() {
        let claims = parse(
            "Claims are stored against the attempt :: crates/board/src/db.rs, \
             crates/board/src/model.rs",
            None,
        )
        .unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].text, "Claims are stored against the attempt");
        assert_eq!(
            claims[0].files,
            ["crates/board/src/db.rs", "crates/board/src/model.rs"]
        );
    }

    /// The refusal *is* the contract: anything without an anchor is prose, and
    /// prose is what this design exists to stop taking at face value.
    #[test]
    fn prose_without_an_anchor_is_refused_by_line() {
        let err = parse(
            "I refactored the settle logic and it is much nicer now.",
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no `::`"), "{err}");
        assert!(err.contains("much nicer now"), "the line is named: {err}");

        let err = parse("A claim with an anchor and no paths ::", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no paths"), "{err}");

        // And an anchor with nothing said about it is not a claim either.
        assert!(parse(":: crates/board/src/db.rs", None).is_err());
    }

    /// `Db::open` in a sentence is ordinary prose; a `::` in a path is not. So
    /// the last separator wins.
    #[test]
    fn the_last_separator_wins_so_rust_paths_can_be_written_about() {
        let claims = parse("Db::open now migrates :: crates/board/src/db.rs", None).unwrap();
        assert_eq!(claims[0].text, "Db::open now migrates");
        assert_eq!(claims[0].files, ["crates/board/src/db.rs"]);
    }

    #[test]
    fn agents_write_bullet_lists_and_that_is_fine() {
        let claims = parse(
            "- First thing :: a.rs\n\
             * Second thing :: b.rs\n\
             \n\
             Third thing :: c.rs\n",
            None,
        )
        .unwrap();
        assert_eq!(claims.len(), 3);
        assert_eq!(claims[2].text, "Third thing");
    }

    /// The board handed the agent the absolute path; refusing it back would be
    /// pedantry about a prefix the board itself chose.
    #[test]
    fn an_absolute_path_inside_the_checkout_is_made_relative() {
        let claims = parse(
            "Did a thing :: /wt/gh-183-1/crates/board/src/db.rs ./docs/BOARD.md `Cargo.toml`",
            Some("/wt/gh-183-1"),
        )
        .unwrap();
        assert_eq!(
            claims[0].files,
            ["crates/board/src/db.rs", "docs/BOARD.md", "Cargo.toml"]
        );
    }

    #[test]
    fn a_path_named_twice_in_one_claim_is_named_once() {
        let claims = parse("Thing :: a.rs a.rs ./a.rs", None).unwrap();
        assert_eq!(claims[0].files, ["a.rs"]);
    }

    #[test]
    fn an_empty_submission_says_what_the_format_is() {
        let err = parse("   \n\n", None).unwrap_err().to_string();
        assert!(err.contains("One claim per line"), "{err}");
    }

    #[test]
    fn a_paragraph_wearing_an_anchor_is_refused() {
        let long = "x".repeat(MAX_TEXT + 1);
        let err = parse(&format!("{long} :: a.rs"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a claim"), "{err}");
    }

    // ---- the remainder ---------------------------------------------------

    /// The acceptance criterion of the whole ticket: the interesting set is the
    /// one the claims do not account for.
    #[test]
    fn the_unclaimed_set_is_what_the_claims_do_not_reach() {
        let claims = parse(
            "Stores claims on the attempt :: crates/board/src/db.rs crates/board/src/model.rs",
            None,
        )
        .unwrap();
        let diff = [
            changed("crates/board/src/db.rs"),
            changed("crates/board/src/model.rs"),
            // Nobody mentioned these two — a dependency and a function edited
            // in passing, which is exactly the pair this exists to surface.
            changed("Cargo.lock"),
            changed("crates/board/src/gc.rs"),
        ];
        let r = remainder(&claims, &diff);
        assert_eq!(r.claimed, 2);
        assert_eq!(
            r.unclaimed
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["Cargo.lock", "crates/board/src/gc.rs"]
        );
        assert!(!r.complete());
        assert!(r.claims[0].anchored());
    }

    #[test]
    fn a_diff_every_claim_reaches_is_a_complete_one() {
        let claims = parse("All of it :: src/a.rs src/b.rs", None).unwrap();
        let r = remainder(&claims, &[changed("src/a.rs"), changed("src/b.rs")]);
        assert!(r.complete());
        assert!(r.unclaimed.is_empty());
        assert_eq!(r.claimed, 2);
    }

    /// The honest spelling of "I rewrote this module", and visibly coarser than
    /// naming the files.
    #[test]
    fn a_directory_anchor_accounts_for_what_is_under_it() {
        let claims = parse("Rewrote the module :: crates/board/src/", None).unwrap();
        let r = remainder(
            &claims,
            &[
                changed("crates/board/src/db.rs"),
                changed("crates/board/src/sync.rs"),
                changed("crates/engine/src/board.rs"),
            ],
        );
        assert_eq!(r.unclaimed.len(), 1);
        assert_eq!(r.unclaimed[0].path, "crates/engine/src/board.rs");
        // A sibling whose name merely starts the same way is not underneath it.
        assert!(!accounts_for("crates/board", "crates/board-cli/x.rs"));
        assert!(accounts_for("crates/board", "crates/board/x.rs"));
    }

    /// Three crates here have a `db.rs`. A basename match would claim files
    /// nobody looked at — the exact failure this design is built around.
    #[test]
    fn a_bare_filename_anchors_nothing() {
        let claims = parse("Touched the database :: db.rs", None).unwrap();
        let r = remainder(&claims, &[changed("crates/board/src/db.rs")]);
        assert_eq!(r.unclaimed.len(), 1, "the change is still unaccounted for");
        assert_eq!(r.unmatched_anchors, ["db.rs"]);
        assert!(
            !r.claims[0].anchored(),
            "and the claim is checkable as wrong"
        );
    }

    /// A claim about a file that did not change is at least as interesting as
    /// an unclaimed file, and is never quietly dropped.
    #[test]
    fn an_anchor_the_diff_never_touched_is_reported_not_dropped() {
        let claims = parse(
            "Fixed the retry path :: src/retry.rs src/imaginary.rs",
            None,
        )
        .unwrap();
        let r = remainder(&claims, &[changed("src/retry.rs")]);
        assert_eq!(r.claims[0].matched, ["src/retry.rs"]);
        assert_eq!(r.claims[0].unmatched, ["src/imaginary.rs"]);
        assert_eq!(r.unmatched_anchors, ["src/imaginary.rs"]);
        assert!(r.complete(), "the diff itself is fully accounted for");
    }

    /// The state a review must be able to shout about: an agent that never
    /// answered the contract accounts for nothing at all.
    #[test]
    fn no_claims_means_the_whole_diff_is_the_remainder() {
        let diff = [changed("a.rs"), changed("b.rs")];
        let r = remainder(&[], &diff);
        assert_eq!(r.unclaimed.len(), 2);
        assert_eq!(r.claimed, 0);
        assert!(!r.complete());
    }

    #[test]
    fn two_claims_may_share_a_file_without_double_counting_it() {
        let claims = parse("One :: src/a.rs\nTwo :: src/a.rs src/b.rs", None).unwrap();
        let r = remainder(&claims, &[changed("src/a.rs"), changed("src/b.rs")]);
        assert_eq!(r.claimed, 2);
        assert!(r.complete());
    }

    // ---- reading git -----------------------------------------------------

    #[test]
    fn the_diff_is_read_from_numstat_and_name_status_together() {
        let files = parse_diff(
            "12\t3\tcrates/board/src/db.rs\n\
             40\t0\tcrates/board/src/claims.rs\n\
             -\t-\tdocs/screenshot.png\n",
            "M\tcrates/board/src/db.rs\n\
             A\tcrates/board/src/claims.rs\n\
             A\tdocs/screenshot.png\n",
        );
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].status, "M");
        assert_eq!((files[0].added, files[0].removed), (12, 3));
        assert_eq!(files[1].status, "A");
        assert!(files[2].binary);
        assert_eq!((files[2].added, files[2].removed), (0, 0));
    }

    /// A path only one of the two commands reported is still a changed file.
    /// Membership follows the numstat; the letter defaults to modified.
    #[test]
    fn a_file_name_status_did_not_mention_still_counts_as_changed() {
        let files = parse_diff("1\t1\tsrc/a.rs\n", "");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, "M");
    }

    // ---- the assembled review --------------------------------------------

    fn task_with(attempt: Attempt) -> Task {
        Task {
            id: "gh:o/r#183".into(),
            source: Source::Github,
            source_id: "183".into(),
            identifier: "gh#183".into(),
            title: "review backend".into(),
            body: Some("  ".into()),
            url: "https://github.com/o/r/issues/183".into(),
            labels: vec![],
            state: BoardState::Review,
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Started,
            local_done: false,
            pr_url: Some("https://github.com/o/r/pull/200".into()),
            pr_number: Some(200),
            pr_open: true,
            pr_merged: false,
            pr_mergeable: None,
            updated_at: "t".into(),
            synced_at: "t".into(),
            attempts: vec![attempt],
        }
    }

    fn attempt_with(claims: Vec<Claim>, claims_at: Option<&str>) -> Attempt {
        Attempt {
            claims,
            claims_at: claims_at.map(str::to_string),
            outcome: Some(Outcome::Done),
            ..crate::model::tests::blank_attempt()
        }
    }

    #[test]
    fn a_review_carries_the_brief_the_claims_and_what_they_missed() {
        let claims = parse("Did the storage :: crates/board/src/db.rs", None).unwrap();
        let attempt = attempt_with(claims, Some("2026-08-09T10:00:00Z"));
        let task = task_with(attempt.clone());
        let r = review(
            &task,
            &attempt,
            vec![changed("crates/board/src/db.rs"), changed("Cargo.lock")],
            DiffSource::Checkout,
            Some(0),
            RunEvidence::default(),
        );
        assert_eq!(r.brief.identifier, "gh#183");
        // An issue whose body is whitespace has no description; a surface must
        // not have to know which of the two the tracker sent.
        assert_eq!(r.brief.body, None);
        assert_eq!(r.attempt_number, 1);
        assert!(r.claimed());
        assert_eq!(r.remainder.unclaimed.len(), 1);
        assert_eq!(r.remainder.unclaimed[0].path, "Cargo.lock");
        assert_eq!(r.pr_url.as_deref(), Some("https://github.com/o/r/pull/200"));
    }

    /// "Never answered the contract" and "claimed nothing" are different facts,
    /// and the review keeps them apart all the way out.
    #[test]
    fn an_attempt_that_never_claimed_is_not_an_attempt_that_claimed_nothing() {
        let silent = attempt_with(vec![], None);
        let task = task_with(silent.clone());
        let r = review(
            &task,
            &silent,
            vec![changed("a.rs")],
            DiffSource::Recorded,
            None,
            RunEvidence::default(),
        );
        assert!(!r.claimed());
        assert_eq!(r.remainder.unclaimed.len(), 1);
    }

    /// The published shape (`comet-board review --json`, `ReadAttemptReview`).
    /// Pinned because orchestrating agents read it beside `list --json`, and a
    /// key that quietly changes case is a key that quietly stops being read.
    #[test]
    fn the_json_is_snake_case_and_flattens_the_remainder_into_the_review() {
        let claims = parse("Did it :: src/a.rs src/nope.rs", None).unwrap();
        let attempt = attempt_with(claims, Some("2026-08-09T10:00:00Z"));
        let task = task_with(attempt.clone());
        let json = serde_json::to_value(review(
            &task,
            &attempt,
            vec![changed("src/a.rs"), changed("src/b.rs")],
            DiffSource::Checkout,
            Some(0),
            RunEvidence::default(),
        ))
        .unwrap();

        assert_eq!(json["task_id"], "gh:o/r#183");
        assert_eq!(json["attempt_number"], 1);
        assert_eq!(json["claimed_at"], "2026-08-09T10:00:00Z");
        assert_eq!(json["brief"]["identifier"], "gh#183");
        assert_eq!(json["diff"]["source"], "checkout");
        assert_eq!(json["uncommitted"], 0);
        // The remainder is flattened in, so the answer is at the top level
        // where a reader (and a jq one-liner) looks for it.
        assert_eq!(json["unclaimed"][0]["path"], "src/b.rs");
        assert_eq!(json["unclaimed"][0]["added"], 10);
        assert_eq!(json["claimed"], 1);
        assert_eq!(json["unmatched_anchors"][0], "src/nope.rs");
        assert_eq!(json["claims"][0]["matched"][0], "src/a.rs");
        assert_eq!(json["evidence"]["commands"], 0);
        assert!(json["evidence"]["checks"].as_array().unwrap().is_empty());
    }

    /// A collected checkout with nothing recorded must not render as "nothing
    /// changed" — the opposite of true, and the reason the variant travels.
    #[test]
    fn an_unreadable_diff_says_so_rather_than_reading_as_an_empty_one() {
        let attempt = attempt_with(vec![], None);
        let task = task_with(attempt.clone());
        let r = review(
            &task,
            &attempt,
            vec![],
            DiffSource::Unavailable {
                reason: "the checkout was reclaimed".into(),
            },
            None,
            RunEvidence::default(),
        );
        assert!(matches!(r.diff, DiffSource::Unavailable { .. }));
        assert!(
            r.remainder.complete(),
            "vacuously, and the reader is told why"
        );
    }
}
