//! The composer's `@`-picker: when it opens, what it offers, what Enter does
//! (gh#424).
//!
//! Sibling of [`crate::view::skills`], and here for the same reason: three
//! composers (gpui, terminal, iOS) must agree on when a menu opens and what a
//! completion writes, or the same keystrokes mean different things per surface.
//! Everything below is pure.
//!
//! The trigger rules are deliberately narrower than the slash picker's, because
//! `@` is a character people already type: an email address, a git revision
//! (`HEAD@{2}`), a Rust pattern binding. Opening only at the start of the buffer
//! or after whitespace is what keeps `brede@tally.no` from summoning a file
//! menu, and closing on whitespace is what ends it.

use crate::{ContextMatch, ContextRef, ContextRefKind};

/// The `@…` token the caret is currently inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtToken {
    /// Byte range of the token in the buffer, INCLUDING the leading `@`.
    pub start: usize,
    pub end: usize,
    /// What was typed after the `@` — the picker's filter query.
    pub query: String,
}

/// The token the picker should be filtering on, or `None` when the caret is not
/// in one.
///
/// `cursor` is a byte offset and must sit on a char boundary — both composers
/// keep their carets on grapheme boundaries, which is stronger.
pub fn at_token(text: &str, cursor: usize) -> Option<AtToken> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let head = &text[..cursor];
    let start = head.rfind('@')?;
    // An `@` mid-word is an address, a revision, a decorator — never a
    // reference. Only start-of-buffer or after whitespace opens the picker.
    if let Some(before) = head[..start].chars().next_back()
        && !before.is_whitespace()
    {
        return None;
    }
    let query = &head[start + 1..];
    // Whitespace ends the token: paths with spaces exist, but a picker that
    // stayed open across them could never tell "still typing a path" from "went
    // on with the sentence", and the sentence is the common case.
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some(AtToken {
        start,
        end: cursor,
        query: query.to_string(),
    })
}

/// Replace `token` with the reference's own token text, returning the new
/// buffer and where the caret lands.
///
/// A trailing space follows a file (the reference is complete; the sentence goes
/// on) but never a directory: completing `@crates/` and continuing to type is
/// how you walk into it, so the caret stays inside the token and the menu stays
/// open on the deeper query.
pub fn complete(text: &str, token: &AtToken, reference: &ContextRef) -> (String, usize) {
    let inserted = reference.token();
    let mut out = String::with_capacity(text.len() + inserted.len() + 1);
    out.push_str(&text[..token.start]);
    out.push_str(&inserted);
    if reference.kind == ContextRefKind::File {
        out.push(' ');
    }
    let cursor = out.len();
    out.push_str(&text[token.end.min(text.len())..]);
    (out, cursor)
}

/// Every `@…` token in a buffer, in order, as typed references.
///
/// **This is the composer's reference list.** Not a side map of what the picker
/// minted: the buffer is a text and stays one — somebody can backspace half a
/// path, retype it, paste one that was never picked, or delete a token whose
/// reference a side map would then still be carrying. Reading the references
/// back OUT of the text on send is what keeps the two from ever disagreeing, and
/// it is why a hand-typed `@src/main.rs` works exactly like a picked one (the
/// host checks both against the real checkout either way).
///
/// The trailing slash is what says "directory" — which is why [`complete`]
/// writes one, and why the token form is the same on a surface that draws no
/// chip. A path that escapes the checkout, or is absolute, is not a reference
/// and is left as plain text.
pub fn tokens(text: &str) -> Vec<ContextRef> {
    let mut found: Vec<ContextRef> = Vec::new();
    let mut ix = 0;
    while let Some(offset) = text[ix..].find('@') {
        let at = ix + offset;
        let opens = at == 0 || text[..at].chars().next_back().is_some_and(char::is_whitespace);
        let end = text[at + 1..]
            .find(char::is_whitespace)
            .map(|n| at + 1 + n)
            .unwrap_or(text.len());
        let raw = &text[at + 1..end];
        if opens && let Some(path) = crate::context::normalize(raw) {
            let kind = match raw.ends_with('/') {
                true => ContextRefKind::Directory,
                false => ContextRefKind::File,
            };
            // The same path twice is one reference: repeating it in the prose is
            // a normal thing to do, and resolving it twice would say "2 refs".
            if !found.iter().any(|r| r.path == path) {
                found.push(ContextRef { path, kind });
            }
        }
        ix = end.max(at + 1);
        if ix >= text.len() {
            break;
        }
    }
    found
}

/// Match rank for one row: 0 when the file's own NAME starts with the query, 1
/// when the whole path starts with it, 2 for a name substring, 3 for a path
/// substring. `None` for no match; an empty query matches everything at rank 1.
///
/// Name before path because a query is usually a filename somebody half
/// remembers, and a path prefix would otherwise bury `composer.rs` under every
/// file in the first directory that happens to share the letters.
pub fn rank(query: &str, row: &ContextMatch) -> Option<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(1);
    }
    let path = row.path.to_lowercase();
    let name = row.name().to_lowercase();
    if name.starts_with(&query) {
        return Some(0);
    }
    if path.starts_with(&query) {
        return Some(1);
    }
    if name.contains(&query) {
        return Some(2);
    }
    path.contains(&query).then_some(3)
}

/// Filter + rank rows for the picker, stable within each rank, shortest path
/// first on a tie.
///
/// Shortest-first is the tiebreak because a shallower path is nearly always the
/// one meant: typing `main` should offer `src/main.rs` before
/// `vendor/x/y/z/main.rs`, and both rank identically on the name.
pub fn filter(query: &str, rows: &[ContextMatch]) -> Vec<ContextMatch> {
    let mut ranked: Vec<(usize, usize, usize, &ContextMatch)> = rows
        .iter()
        .enumerate()
        .filter_map(|(ix, row)| rank(query, row).map(|r| (r, row.path.len(), ix, row)))
        .collect();
    ranked.sort_by_key(|&(rank, len, ix, _)| (rank, len, ix));
    ranked.into_iter().map(|(_, _, _, row)| row.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> ContextMatch {
        ContextMatch {
            path: path.into(),
            kind: ContextRefKind::File,
        }
    }

    #[test]
    fn opens_at_the_start_and_after_whitespace_only() {
        let token = at_token("@src", 4).expect("start of buffer opens");
        assert_eq!(
            (token.start, token.end, token.query.as_str()),
            (0, 4, "src")
        );
        let token = at_token("look at @comp", 13).expect("after a space opens");
        assert_eq!((token.start, token.query.as_str()), (8, "comp"));
        assert!(at_token("first\n@comp", 11).is_some(), "newline is whitespace");

        // The cases that must stay quiet — all of them things people type.
        assert_eq!(at_token("brede@tally.no", 14), None, "an email address");
        assert_eq!(at_token("HEAD@{2}", 8), None, "a git revision");
        assert_eq!(at_token("hello", 5), None, "no at-sign at all");
    }

    #[test]
    fn closes_once_the_path_is_followed_by_prose() {
        assert_eq!(at_token("@src/main.rs and then", 21), None);
        // …but the caret parked back inside the path re-opens on it.
        let token = at_token("@src/main.rs and then", 4).expect("caret inside");
        assert_eq!((token.start, token.query.as_str()), (0, "src"));
        // A bare `@` is a valid (empty) query — that is how the list is browsed.
        assert!(at_token("@", 1).expect("bare at opens").query.is_empty());
        // A path IS allowed to contain slashes, unlike the slash picker's token.
        let token = at_token("@crates/ui/src", 14).expect("deep path");
        assert_eq!(token.query, "crates/ui/src");
    }

    #[test]
    fn completion_finishes_a_file_and_walks_into_a_directory() {
        let text = "look at @comp";
        let token = at_token(text, text.len()).expect("token");
        let (out, cursor) = complete(text, &token, &ContextRef::file("crates/ui/src/composer.rs"));
        assert_eq!(out, "look at @crates/ui/src/composer.rs ");
        assert_eq!(cursor, out.len());
        // The caret sits past the space, so the picker does not re-open.
        assert_eq!(at_token(&out, cursor), None);

        // A directory keeps the menu open on the deeper query.
        let text = "@crat";
        let token = at_token(text, text.len()).expect("token");
        let (out, cursor) = complete(text, &token, &ContextRef::directory("crates/ui"));
        assert_eq!(out, "@crates/ui/");
        let token = at_token(&out, cursor).expect("still inside the token");
        assert_eq!(token.query, "crates/ui/");

        // Mid-buffer completion keeps the tail.
        let text = "@comp then stop";
        let token = at_token(text, 5).expect("token");
        let (out, cursor) = complete(text, &token, &ContextRef::file("src/composer.rs"));
        assert_eq!(out, "@src/composer.rs  then stop");
        assert_eq!(cursor, "@src/composer.rs ".len());
    }

    #[test]
    fn tokens_reads_back_what_the_buffer_actually_says() {
        let text = "compare @src/main.rs with @crates/ui/ — not brede@tally.no";
        assert_eq!(
            tokens(text),
            vec![
                ContextRef::file("src/main.rs"),
                ContextRef::directory("crates/ui"),
            ],
            "the trailing slash is what carries the kind across an editable buffer"
        );
        // An escaping or absolute token is not a reference at all.
        assert!(tokens("@../secrets @/etc/passwd").is_empty());
        assert!(tokens("nothing here").is_empty());
        // A bare `@` contributes nothing rather than an empty path.
        assert!(tokens("@ @").is_empty());
        // The same path twice is one reference.
        assert_eq!(tokens("@a.rs and @a.rs again").len(), 1);
        // A token deleted from the buffer takes its reference with it — which
        // is the whole reason this reads the text instead of a side map.
        assert!(tokens("compare  with @crates/ui/").iter().all(|r| r.path == "crates/ui"));
    }

    #[test]
    fn ranking_prefers_the_filename_then_the_shallower_path() {
        let rows = vec![
            file("vendor/deep/nest/composer.rs"),
            file("crates/ui/src/composer.rs"),
            file("docs/composition.md"),
            file("src/main.rs"),
        ];
        let ranked = filter("composer", &rows);
        assert_eq!(
            ranked.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            ["crates/ui/src/composer.rs", "vendor/deep/nest/composer.rs"],
            "name-prefix hits only, shallower first"
        );
        // A path-only hit still shows when nothing matches by name.
        let ranked = filter("crates/", &rows);
        assert_eq!(
            ranked.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            ["crates/ui/src/composer.rs"]
        );
        // Substrings are a fallback, and the empty query is the whole list.
        assert_eq!(rank("posit", &file("docs/composition.md")), Some(2));
        assert_eq!(rank("zzz", &file("src/main.rs")), None);
        assert_eq!(filter("  ", &rows).len(), 4);
    }
}
