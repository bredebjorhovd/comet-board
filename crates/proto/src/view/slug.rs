//! A short slug derived from a task's title (gh#364) — the descriptive half of
//! a name whose other half is the identifier.
//!
//! `gh#357` settled that the identifier is a task's name. It is also an index:
//! four agents in flight read `gh#341 gh#342 gh#343 gh#356`, and no amount of
//! agreement about which number is the name makes those four rows tell each
//! other apart. This is what goes beside them —
//!
//! ```text
//! gh#341 review-page-loads
//! gh#356 settle-announced-twice
//! gh#359 unpriced-model-keeps
//! ```
//!
//! **It is decoration on the key, and it drops first.** It is derived rather
//! than stored, nothing looks a task up by it, and every surface that renders
//! it must render the identifier without it when the room runs out. That is
//! what keeps gh#357's rule intact: a second *identity* — a codename, a
//! generated word — would be a second thing to learn and map back, which is
//! the problem that issue exists to remove.
//!
//! ### Why the first three words are the wrong three
//!
//! The obvious slug — the title's opening words — is worse than the number,
//! because English titles open with articles, hedges and auxiliaries. Measured
//! against this repo's own issue titles:
//!
//! | first four words | content words |
//! | --- | --- |
//! | `a-repo-can-end` | `repo-end-up` |
//! | `a-task-should-have` | `task-one-name` |
//! | `two-boards-can-be` | `two-boards-pointed` |
//! | `the-review-page-loads` | `review-page-loads` |
//!
//! So [`STOPWORDS`] comes out first and [`SLUG_WORDS`] content words go in.
//! Roughly four in five titles produce something genuinely useful and the rest
//! come out harmless, which is the trade this is allowed to make: the
//! identifier beside it still carries the meaning, so a poor slug costs a
//! reader nothing but the width.

/// How many content words a slug carries. Three is a phrase; four is a
/// sentence fragment that stops fitting beside the things it decorates.
pub const SLUG_WORDS: usize = 3;

/// The width a slug is capped at, in characters. Wide enough for three real
/// words, narrow enough that a row is still mostly the row.
pub const SLUG_MAX: usize = 28;

/// The words that carry no information about *which* task this is: articles,
/// auxiliaries and modals, the common prepositions and conjunctions, the
/// pronouns, and the hedges a title opens with.
///
/// Deliberately not a general-purpose stopword list. Three kinds of word that
/// most lists strip are content here:
///
/// - **Negations** (`no`, `not`) — `a-retarget-is-not-news` and
///   `the-dependency-no-page-admitted-to` are *about* the negation, and a slug
///   that drops it says the opposite of the title.
/// - **Particles** (`up`, `out`, `off`, `down`) — they finish the verb they
///   follow. `end up` is not `end`.
/// - **`only`, `one`, `once`, `two`** — quantity is usually the point of a
///   title that mentions it (`a-github-only-board`, `one-name-for-a-task`).
pub const STOPWORDS: &[&str] = &[
    // Articles.
    "a", "an", "the", //
    // Conjunctions and the connectives that join clauses.
    "and", "or", "but", "nor", "so", "if", "because", "while", "when", "whether", "than", "then",
    "that", "what", "which", "who", "whom", "whose", //
    // Prepositions. Particles (`up`, `out`, `off`, `down`) are NOT here.
    "of", "to", "in", "on", "at", "for", "with", "from", "by", "as", "into", "onto", "about",
    "after", "before", "between", "through", "during", "per", "via", "over", "under", "against",
    "within", "without", "upon", //
    // Demonstratives and pronouns.
    "this", "these", "those", "there", "here", "it", "its", "they", "them", "their", "we", "our",
    "us", "you", "your", "i", "my", "me", "he", "him", "his", "she", "her", "hers", //
    // Auxiliaries and modals.
    "is", "are", "was", "were", "be", "been", "being", "am", "do", "does", "did", "done", "has",
    "have", "had", "having", "can", "cannot", "could", "should", "would", "will", "shall", "may",
    "might", "must", "let", //
    // Hedges — the words a title uses to sound like a sentence.
    "just", "even", "still", "really", "actually", "simply", "quite", "rather", "also", "very",
    "too",
];

/// The slug for a title: up to [`SLUG_WORDS`] content words, joined with `-`,
/// capped at [`SLUG_MAX`] characters. `None` when the title yields nothing —
/// and `None` is a supported answer everywhere, because the identifier alone
/// is always enough.
///
/// The word rules, and what each one is protecting:
///
/// - **A word is what is left after the non-alphanumerics come out**, so
///   `task's` is `tasks` rather than `task` + `s`, and `gh#357` is `gh357`.
///   Splitting on the apostrophe spends a whole word on a possessive `s`.
/// - **A word carrying non-ASCII letters is dropped whole.** `Ålesund` cannot
///   survive an ASCII slug — `lesund` is not the word, and a branch name is
///   not the place to invent a transliteration. A title that loses every word
///   this way simply has no slug, which is the case the `None` is for.
/// - **The cap cuts between words, never inside one**, so the slug reads as a
///   phrase that stopped rather than a word that broke. A single word longer
///   than the cap is the one exception: it is cut, because something has to
///   give and one word is all there is.
pub fn title_slug(title: &str) -> Option<String> {
    let mut out = String::new();
    for word in content_words(title).take(SLUG_WORDS) {
        if out.is_empty() {
            out.push_str(&word);
            out.truncate(
                out.char_indices()
                    .nth(SLUG_MAX)
                    .map_or(out.len(), |(ix, _)| ix),
            );
            continue;
        }
        if out.chars().count() + 1 + word.chars().count() > SLUG_MAX {
            break;
        }
        out.push('-');
        out.push_str(&word);
    }
    (!out.is_empty()).then_some(out)
}

/// The title's words with the [`STOPWORDS`] taken out, in order, lowercased and
/// reduced to ASCII alphanumerics. Lazy: a caller that wants three words reads
/// three words' worth of the title.
fn content_words(title: &str) -> impl Iterator<Item = String> + '_ {
    title
        .split(|c: char| !c.is_alphanumeric() && c != '\'' && c != '\u{2019}')
        .filter_map(|word| {
            // A word is dropped whole rather than mangled if any of its letters
            // are outside ASCII — see [`title_slug`].
            if word.chars().any(|c| c.is_alphanumeric() && !c.is_ascii()) {
                return None;
            }
            let word: String = word
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .map(|c| c.to_ascii_lowercase())
                .collect();
            (!word.is_empty() && !STOPWORDS.contains(&word.as_str())).then_some(word)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table from gh#364, which is the whole argument for the stopword
    /// pass: the naive slug beside it is what these titles' first words give.
    #[test]
    fn the_content_words_beat_the_first_words() {
        for (title, naive, slug) in [
            (
                "A repo can end up on two boards",
                "a-repo-can-end",
                "repo-end-up",
            ),
            (
                "A task's name should say something",
                "a-task-should-have",
                "tasks-name-say",
            ),
            (
                "Two boards can be pointed at one repo",
                "two-boards-can-be",
                "two-boards-pointed",
            ),
            (
                "The review page loads nothing",
                "the-review-page-loads",
                "review-page-loads",
            ),
        ] {
            let got = title_slug(title).expect("a title with content words has a slug");
            assert_eq!(got, slug, "for `{title}`");
            assert_ne!(got, naive, "the naive slug is what this exists to avoid");
        }
    }

    /// Real titles off this board, which is the population the rule was
    /// measured against.
    #[test]
    fn the_boards_own_titles_come_out_readable() {
        for (title, slug) in [
            (
                "a task has one name when comet surfaces it",
                "task-one-name",
            ),
            (
                "an unpriced model keeps its Breakdown row",
                "unpriced-model-keeps",
            ),
            (
                "a settle announced twice with nothing behind it is one settle",
                "settle-announced-twice",
            ),
            ("a retarget is not news", "retarget-not-news"),
            ("a GitHub-only board is not a broken one", "github-only-board"),
            ("the dependency no page admitted to", "dependency-no-page"),
            ("put gh stack in the agents' hands", "put-gh-stack"),
            ("per-run agent accounts", "run-agent-accounts"),
            ("wall-clock cap on an attempt", "wall-clock-cap"),
            ("worktree GC", "worktree-gc"),
        ] {
            assert_eq!(title_slug(title).as_deref(), Some(slug), "for `{title}`");
        }
    }

    #[test]
    fn the_cap_cuts_between_words() {
        // Three content words that would run to 34 characters; the third is
        // dropped rather than clipped.
        let slug = title_slug("The reconciliation endpoint disagrees").unwrap();
        assert_eq!(slug, "reconciliation-endpoint");
        assert!(slug.chars().count() <= SLUG_MAX);
    }

    /// The one exception, because something has to give: a single word past the
    /// cap is cut, since dropping it would leave no slug at all.
    #[test]
    fn one_word_longer_than_the_cap_is_cut() {
        let slug = title_slug("Internationalizationalization everywhere").unwrap();
        assert_eq!(slug.chars().count(), SLUG_MAX);
        assert!("internationalizationalization".starts_with(&slug));
    }

    #[test]
    fn a_title_with_nothing_in_it_has_no_slug() {
        // Nothing but stopwords, nothing but punctuation, nothing at all.
        assert_eq!(title_slug("It is what it is"), None);
        assert_eq!(title_slug("  —  "), None);
        assert_eq!(title_slug(""), None);
    }

    /// Norwegian titles are the reason a word is dropped whole: `Ålesund` has
    /// no honest ASCII form, and `lesund` in a branch name is worse than the
    /// identifier standing alone.
    #[test]
    fn a_word_that_cannot_be_ascii_is_dropped_whole() {
        assert_eq!(
            title_slug("Ålesund-integrasjonen feiler").as_deref(),
            Some("integrasjonen-feiler")
        );
        // And a title with no ASCII word in it at all has no slug — the case
        // the `None` exists for.
        assert_eq!(title_slug("Årsoppgjøret på Nordmøre"), None);
    }

    /// Every slug has to be safe in a branch name and a path: gh#364 spends the
    /// branch's descriptive budget on this, so a slug that needed escaping
    /// would be a slug that cannot go there.
    #[test]
    fn a_slug_is_always_branch_safe() {
        for title in [
            "A task's name -- should it say \"something\"?",
            "fix: the 50% case (again!)",
            "Ålesund/Bergen · sync",
            "gh#357 follow-up",
        ] {
            let Some(slug) = title_slug(title) else {
                continue;
            };
            assert!(
                slug.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "`{slug}` from `{title}`"
            );
            assert!(!slug.starts_with('-') && !slug.ends_with('-'), "{slug}");
            assert!(!slug.contains("--"), "{slug}");
        }
    }
}
