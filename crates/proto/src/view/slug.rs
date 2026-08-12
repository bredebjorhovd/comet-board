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
//! because titles open with articles, hedges and auxiliaries. Measured against
//! this repo's own issue titles:
//!
//! | first four words | content words |
//! | --- | --- |
//! | `a-repo-can-end` | `repo-end-up` |
//! | `a-task-should-have` | `task-one-name` |
//! | `two-boards-can-be` | `two-boards-pointed` |
//! | `the-review-page-loads` | `review-page-loads` |
//!
//! So [`STOPWORDS`] comes out first and [`SLUG_WORDS`] content words go in.
//! The trade this is allowed to make is that a *poor* slug costs a reader
//! nothing but the width, because the identifier beside it still carries the
//! meaning.
//!
//! ### The population this ships to is not the one it was written against
//!
//! That table is drawn from this repo, and every title in it is English. The
//! board this runs on is not: **about one task in nine is Norwegian**, and the
//! first cut of this module dropped any word carrying a non-ASCII letter —
//! which in a Norwegian title is usually the word carrying the meaning. It
//! turned `fix(nav): ⌘K åpner søket` into `fix-nav-k`, a slug whose one
//! distinguishing word is a letter, sitting next to an identifier that was
//! perfectly clear on its own (gh#364 review).
//!
//! Two rules answer it, and both are scoped to a language rather than to a
//! character class: [`fold`] spells `å ø æ` the way every Norwegian system
//! already does, and [`STOPWORDS`] carries Norwegian function words so the
//! slots the fold just freed are not spent on `og` and `fra`. Neither is
//! guesswork about text in general — they are two languages' worth of list,
//! and a third language is a line to append.
//!
//! The reason this earns the care, when the slug is documented as decoration
//! that drops first: on a *row* it does drop, but this same slug names the
//! branch and the worktree path ([`crate`] is read by
//! `comet_board::dispatch::branch_slug`), and there it cannot drop and gets no
//! second chance. `board/gh-341-fix-nav-k` is what `git branch` says for the
//! life of that work.

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
/// **The list is language-scoped, and it is a list rather than a law.** It
/// covers the two languages this board's tasks are written in — English, and
/// the Norwegian that is about one title in nine. A language nobody has filed
/// in yet is not handled and will spend its three slots on `og`-shaped words
/// until somebody adds them; that is a line to append, not a rule to redesign.
/// Norwegian entries are spelled in their *folded* form ([`fold`] runs first),
/// so `på` is listed as `pa` and `når` would be `nar`.
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
    "too", //
    // Norwegian, in folded spelling. The same four categories, and the same
    // exception: `ikke` is a negation and stays content, for the reason `not`
    // does. `å` folds to `a` and is already struck as an English article,
    // which is the right answer for the infinitive marker too.
    //
    // `var` is deliberately absent, though it is the past tense of `er`: it is
    // also a keyword, and a title about `var` hoisting would lose the word it
    // is about. A list serving two languages has to lose the ties, and a
    // Norwegian title is far likelier to say `er` than `var`.
    "og", "eller", "men", "hvis", "pa", "av", "med", "som", "til", "fra", "en", "et", "den", "det",
    "de", "er", "kan", "skal", "vil", "har", "ved", "om",
];

/// The slug for a title: up to [`SLUG_WORDS`] content words, joined with `-`,
/// capped at [`SLUG_MAX`] characters. `None` when the title yields nothing —
/// and `None` is a supported answer everywhere, because the identifier alone
/// is always enough.
///
/// The word rules, and what each one is protecting:
///
/// - **An apostrophe does not split a word**, so `task's` is `tasks` rather
///   than `task` + `s` — splitting there spends a whole word on a possessive.
///   Everything else that is not a letter or a digit separates: `gh#357` is
///   `gh` and `357`.
/// - **A non-ASCII letter folds to the ASCII everyone already types**
///   ([`fold`]): `Kjør` is `kjor`, `byrå` is `byra`, `Ålesund` is `alesund`.
///   Not a transliteration this module invented — it is the convention every
///   Norwegian system uses, and the one already in every hand-typed branch
///   name here.
/// - **A letter with no ASCII spelling drops its whole word.** Cyrillic, Greek,
///   CJK: there is no honest ASCII for them, and half a word is worse than
///   none. A title that loses every word this way has no slug, which is the
///   case the `None` is for.
/// - **A one-character word is dropped.** `⌘K åpner søket` is about opening
///   the search, and a slug whose distinguishing word is the letter `k` says
///   less than the identifier beside it already does.
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
/// folded to ASCII. Lazy: a caller that wants three words reads three words'
/// worth of the title.
fn content_words(title: &str) -> impl Iterator<Item = String> + '_ {
    title
        .split(|c: char| !(c.is_alphanumeric() || is_combining(c) || is_apostrophe(c)))
        .filter_map(ascii_word)
        // A lone letter and a function word are the same kind of nothing.
        .filter(|word| word.chars().count() > 1 && !STOPWORDS.contains(&word.as_str()))
}

/// One word of a title as ASCII, or `None` if it carries a letter that has no
/// ASCII spelling — see [`title_slug`] for why that drops the word whole.
fn ascii_word(word: &str) -> Option<String> {
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if is_combining(c) || is_apostrophe(c) {
            // A decomposed `å` is `a` plus a mark that is not a letter of its
            // own, and a possessive apostrophe is not one either. Neither ends
            // the word, and neither survives into it.
        } else if let Some(ascii) = fold(c) {
            out.push_str(ascii);
        } else if c.is_alphanumeric() {
            return None;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The ASCII a Latin letter is spelled with when ASCII is all there is.
///
/// `å→a`, `ø→o`, `æ→ae` are the reason this exists: this board's tasks are
/// Norwegian about one title in nine, and dropping those words left slugs like
/// `fix-nav-k` on work whose subject was `åpner søket`. That mapping is not a
/// judgement call — it is what every Norwegian keyboard-less system does, and
/// what everybody here already types into a branch name by hand. The rest of
/// the table is the same courtesy for the accents that turn up beside them.
///
/// `None` means "this letter has no ASCII spelling", which is a different
/// statement from "I have not listed it": Cyrillic and CJK belong there, and
/// [`ascii_word`] drops their words rather than guessing.
fn fold(c: char) -> Option<&'static str> {
    Some(match c.to_lowercase().next()? {
        'å' => "a",
        'ø' => "o",
        'æ' => "ae",
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'ā' | 'ă' | 'ą' => "a",
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' => "e",
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' => "i",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ō' => "o",
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' => "u",
        'ý' | 'ÿ' => "y",
        'ñ' | 'ń' => "n",
        'ç' | 'ć' | 'č' => "c",
        'š' | 'ś' => "s",
        'ž' | 'ź' | 'ż' => "z",
        'ł' => "l",
        'đ' | 'ð' => "d",
        'ř' => "r",
        'ť' => "t",
        'œ' => "oe",
        'ß' => "ss",
        'þ' => "th",
        _ => return None,
    })
}

/// A combining diacritical mark — what an `å` decomposes into beside its `a`.
/// Not a letter, and so neither a word boundary nor a character of its own.
fn is_combining(c: char) -> bool {
    matches!(c, '\u{300}'..='\u{36f}')
}

fn is_apostrophe(c: char) -> bool {
    c == '\'' || c == '\u{2019}'
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

    /// Norwegian is about one title in nine on the board this ships to, and
    /// `å→a`, `ø→o`, `æ→ae` is what every Norwegian system already does with
    /// it — including everybody here, by hand, in branch names.
    #[test]
    fn norwegian_letters_fold_to_the_ascii_everyone_already_types() {
        assert_eq!(
            title_slug("Ålesund-integrasjonen feiler").as_deref(),
            Some("alesund-integrasjonen-feiler")
        );
        assert_eq!(
            title_slug("Årsoppgjøret på Nordmøre").as_deref(),
            Some("arsoppgjoret-nordmore")
        );
        // Decomposed input lands on the same answer: the combining ring is not
        // a letter of its own, and must not split the word it sits in.
        assert_eq!(
            title_slug("A\u{30a}lesund-integrasjonen feiler").as_deref(),
            Some("alesund-integrasjonen-feiler")
        );
        // The accents that turn up beside them, and the one-off ligatures.
        for (title, slug) in [
            ("Café tokens expire", "cafe-tokens-expire"),
            ("Größe der Uploads", "grosse-der-uploads"),
            ("Œuvre importer", "oeuvre-importer"),
        ] {
            assert_eq!(title_slug(title).as_deref(), Some(slug), "for `{title}`");
        }
    }

    /// The rule the fold replaced still applies where there is genuinely no
    /// ASCII spelling: half a word would be worse than none.
    #[test]
    fn a_letter_with_no_ascii_spelling_drops_its_word() {
        assert_eq!(
            title_slug("Проверка sync fails").as_deref(),
            Some("sync-fails")
        );
        assert_eq!(title_slug("東京 の テスト"), None);
    }

    /// gh#364 review: `fix(nav): ⌘K åpner søket` slugged to `fix-nav-k`, whose
    /// one distinguishing word is a letter. A lone character says less than
    /// the identifier beside it already does.
    #[test]
    fn a_one_character_word_is_not_a_word() {
        assert_eq!(
            title_slug("fix(nav): ⌘K åpner søket — ikke assistenten").as_deref(),
            Some("fix-nav-apner")
        );
        assert_eq!(title_slug("A/B test the onboarding").as_deref(), Some("test-onboarding"));
    }

    /// The seven titles the gh#364 review pulled off the live board — the
    /// population this rule actually ships to, as against the all-English one
    /// it was first measured on.
    #[test]
    fn the_boards_norwegian_titles_come_out_readable() {
        for (title, was, now) in [
            (
                "fix(nav): ⌘K åpner søket — ikke assistenten",
                "fix-nav-k",
                "fix-nav-apner",
            ),
            (
                "Kjør dryRun av Altinn-innboks-cronen i prod",
                "dryrun-av-altinn",
                "kjor-dryrun-altinn",
            ),
            (
                "Signicat MINT: screening-løkka bruker feil terskel",
                "signicat-mint-screening",
                "signicat-mint-screening",
            ),
            (
                "Leverandørfakturaer kan ikke konteres: feil 18000",
                "kan-ikke-konteres",
                "leverandorfakturaer-ikke",
            ),
            (
                "Fakturahistorikk og kreditering per byrå",
                "fakturahistorikk-og",
                "fakturahistorikk-kreditering",
            ),
            (
                "«Lag tilbud» fra Byrå-detalj med forhåndsvisning",
                "lag-tilbud-fra",
                "lag-tilbud-byra",
            ),
            (
                "perf(etterlevelse): memoisert rad, deferret søk",
                "perf-etterlevelse-memoisert",
                "perf-etterlevelse-memoisert",
            ),
        ] {
            let got = title_slug(title).expect("a Norwegian title has a slug now");
            assert_eq!(got, now, "for `{title}`");
            assert!(got.chars().count() <= SLUG_MAX, "{got} is over the cap");
            if was != now {
                assert_ne!(got, was, "the slug the review measured and rejected");
            }
        }
    }

    /// Norwegian function words spend the three slots the fold just freed, so
    /// the list has to see them. `ikke` is not one of them — a negation is
    /// content, which is this module's own rule about `not`.
    #[test]
    fn the_stopwords_cover_both_languages_the_board_writes_in() {
        assert_eq!(
            title_slug("Rapporten er ikke ferdig").as_deref(),
            Some("rapporten-ikke-ferdig")
        );
        assert_eq!(
            title_slug("Kan vi kreditere fra byrået?").as_deref(),
            Some("vi-kreditere-byraet")
        );
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
