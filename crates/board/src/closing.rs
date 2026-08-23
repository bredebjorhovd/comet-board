//! The pull request's closing reference — the one line of its body GitHub
//! itself acts on (gh#548).
//!
//! A dispatched agent announces closure in whatever prose the repo speaks, and
//! GitHub understands none of it. Tally PR #967 opened with **Lukker gh#932.**
//! — right for the repo's language and still nothing to GitHub: its closing
//! keywords are English only (`close/closes/closed`, `fix/fixes/fixed`,
//! `resolve/resolves/resolved`), and `gh#932` is not a reference form it parses
//! (`#932`, `owner/repo#932`, `GH-932` and a full issue URL are). So GitHub
//! built no linked-issue relation at all: nothing under *Development*, an empty
//! `closingIssuesReferences`, a merge that closed nothing. It went unnoticed
//! because writeback closes the issue on settle anyway — but that is the board
//! doing it, not GitHub, and every native consequence of the link was silently
//! absent. Orion PR #50 failed in the other direction: no closing keyword
//! anywhere. This is instruction drift in both spellings, which is why the ask
//! and the check live together here.
//!
//! Two halves:
//!
//! - [`brief`] is the ask, appended by [`crate::dispatch::resolve_prompt`] for
//!   every GitHub-issue task: put `Closes #<n>` on a line of its own, keep the
//!   rest of the body in the repo's own language. The two are not in tension —
//!   one line is GitHub's, the prose around it is the repo's.
//! - [`parses_as_closing`] is the check, run by
//!   [`crate::sync::SyncEngine::link_pull_requests`] the first time an open
//!   pull request attaches to its task: does the body carry anything GitHub's
//!   parser would act on? It accepts what GitHub accepts, not what the brief
//!   asks for — the contract is GitHub's parser, and the brief's "own line" is
//!   how an agent reliably produces parseable text, not the definition.

use crate::model::Task;

/// The paragraph every dispatch for a GitHub issue gets: close the issue from
/// the pull request body, in GitHub's own grammar (§gh#548).
///
/// Appended after interpolation on the same rule as [`super::claims::brief`]
/// and `pr_base_line`: a route's own `prompt` is somebody's wording for the
/// *task*, and whether merging the request closes the issue is a fact about
/// being dispatched against GitHub at all.
///
/// `None` for every task whose id does not end in an issue number — a Linear
/// row has nothing on GitHub to close, and the pull-request id form (`!`) is
/// itself a pull request. The number comes off the task id rather than the
/// identifier because the reference has to be the bare number: `Closes gh#932`
/// would be the Tally failure again, wearing a keyword this time.
pub fn brief(task: &Task) -> Option<String> {
    let issue = crate::model::gh_number(&task.id)?;
    Some(format!(
        "\n\nOpen your pull request with a closing reference GitHub can act on \
         — on its own line in the body: `Closes #{issue}`. The keywords GitHub \
         parses are English only (close, fixes, resolves, …), and the \
         reference has to be `#{issue}`, `GH-{issue}`, `owner/repo#{issue}` or \
         a full issue URL; prose in any other language, or an unparseable \
         spelling like `gh#{issue}`, creates no linked-issue relation — no \
         link under Development, and a merge that closes nothing. Everything \
         else in the body stays in whatever language the repo writes in; this \
         one line is the part GitHub reads."
    ))
}

/// Does `body` carry a closing reference GitHub's parser would act on for
/// `issue`? `None`-bodied requests parse as nothing — there is nothing to read.
///
/// An approximation of GitHub's matcher, lenient where being strict would
/// reject a body GitHub itself accepts:
///
/// - **Keywords anywhere in prose.** `This also fixes #12` links, so scanning
///   is token-based over the whole body, not a per-line rule. The brief asks
///   for the canonical form on its own line; the check asks only what GitHub
///   asks.
/// - **Keyword forms:** `close(s|d)?`, `fix(e[sd])?`, `resolv(e[sd])?`,
///   case-insensitive, optionally followed by punctuation such as the colon in
///   `Fixes:` — emphasis markers (`**Closes**`) survive stripping too.
/// - **Reference forms for the number:** `#932`, `GH-932`,
///   `owner/repo#932` (any two-name prefix, since forks cross repositories),
///   and a full URL naming `/issues/932`. Trailing sentence punctuation is
///   stripped before matching, so `Closes #932.` reads whole.
///
/// Deliberately not approximated: multi-reference lists (`closes #1, #2`)
/// count every reference-shaped token after a keyword until ordinary prose
/// intervenes, so a list naming `issue` matches even when other numbers sit
/// between. What matters is that the task's own issue is reachable, not that
/// the body be minimal; a false positive costs one silent poll, while a false
/// negative would re-warn forever about a body GitHub already honours.
pub fn parses_as_closing(body: Option<&str>, issue: i64) -> bool {
    const KEYWORDS: [&str; 9] = [
        "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
    ];
    let Some(body) = body else {
        return false;
    };
    let hash_ref = format!("#{issue}");
    let gh_ref = format!("GH-{issue}");
    let url_ref = format!("/issues/{issue}");
    let mut after_keyword = false;
    for raw in body.split_whitespace() {
        let token = raw
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '*' | '_' | '`' | '~' | '"' | '\'' | '(' | ')' | '[' | ']'
                )
            })
            // Punctuation may ride on either half: `Fixes:` is still the
            // keyword, and `#932.` is still the reference.
            .trim_end_matches(['.', ',', ';', ':', '!', '?'])
            .to_ascii_lowercase();
        if KEYWORDS.contains(&token.as_str()) {
            after_keyword = true;
            continue;
        }
        if references_issue(&token, &hash_ref, &gh_ref, &url_ref) && after_keyword {
            return true;
        }
        // A comma/space list keeps the keyword alive across reference tokens;
        // any word of prose ends it, exactly where GitHub's list syntax ends.
        after_keyword &= token.starts_with('#') || token.contains('#') || token.starts_with("gh-");
    }
    false
}

fn references_issue(token: &str, hash_ref: &str, gh_ref: &str, url_ref: &str) -> bool {
    token == hash_ref
        || token == gh_ref.to_ascii_lowercase()
        // `owner/repo#932` — any repository prefix; the branch scoping in
        // `link_pull_requests` has already tied this request to the task's
        // repo, so a foreign prefix here is noise, not a wrong issue.
        || token
            .rsplit_once('#')
            .is_some_and(|(prefix, n)| n == hash_ref.trim_start_matches('#') && !prefix.is_empty())
        || token.contains(url_ref)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_with_id(id: &str) -> Task {
        crate::model::Task {
            id: id.into(),
            source: crate::model::Source::Github,
            source_id: String::new(),
            identifier: String::new(),
            title: String::new(),
            body: None,
            url: String::new(),
            labels: vec![],
            state: crate::model::BoardState::Ready,
            source_state: None,
            upstream: crate::model::UpstreamState::Unstarted,
            local_done: false,
            pr_url: None,
            pr_number: None,
            pr_open: false,
            pr_merged: false,
            pr_mergeable: None,
            pr_base_ref: None,
            pr_head_ref: None,
            pr_stack: None,
            pr_changes_requested: None,
            updated_at: String::new(),
            synced_at: String::new(),
            attempts: vec![],
        }
    }

    // ---- the ask ----------------------------------------------------------

    #[test]
    fn a_github_issue_task_gets_the_canonical_reference_line() {
        let brief = brief(&task_with_id("gh:Florin-AS/Tally#932")).unwrap();
        assert!(brief.contains("`Closes #932`"), "{brief}");
        // The failure that opened the ticket, named so an agent can recognise
        // its own draft as the thing that links nothing.
        assert!(brief.contains("`gh#932`"), "{brief}");
    }

    #[test]
    fn linear_tasks_and_pull_request_rows_are_asked_nothing() {
        assert_eq!(brief(&task_with_id("linear:LIN-142")), None);
        assert_eq!(brief(&task_with_id("gh:owner/widget!508")), None);
    }

    // ---- the check --------------------------------------------------------

    #[test]
    fn the_canonical_line_parses() {
        assert!(parses_as_closing(
            Some("Summary\n\nCloses #932.\n\nNotes."),
            932
        ));
        // Case, colon, inline prose: all things GitHub itself honours.
        assert!(parses_as_closing(Some("this FIXES: #7"), 7));
        assert!(parses_as_closing(
            Some("It also resolves #41 along the way"),
            41
        ));
        assert!(parses_as_closing(Some("**Closed** #5 earlier"), 5));
    }

    #[test]
    fn every_reference_form_github_parses_is_accepted() {
        assert!(parses_as_closing(Some("Fix GH-932 here"), 932));
        assert!(parses_as_closing(
            Some("Resolve https://github.com/Florin-AS/Tally/issues/932"),
            932
        ));
        assert!(parses_as_closing(Some("Closes Florin-AS/Tally#932."), 932));
        // Lists keep the keyword alive across sibling references.
        assert!(parses_as_closing(Some("Closes #1, #932, #3"), 932));
    }

    #[test]
    fn the_tally_body_parses_as_nothing() {
        // The exact failure: Norwegian keyword, board-flavoured reference.
        assert!(!parses_as_closing(Some("**Lukker gh#932.**"), 932));
    }

    #[test]
    fn a_body_without_the_number_or_without_a_keyword_is_not_a_closure() {
        // Right reference, no keyword — orion PR #50's shape.
        assert!(!parses_as_closing(Some("See #932 for context"), 932));
        // Keyword, wrong number.
        assert!(!parses_as_closing(Some("Closes #931"), 932));
        // Keyword pointing elsewhere does not leak onto later numbers.
        assert!(!parses_as_closing(
            Some("Fixes #1 and then prose mentions #932"),
            932
        ));
        assert!(!parses_as_closing(None, 932));
        assert!(!parses_as_closing(Some(""), 932));
    }
}
