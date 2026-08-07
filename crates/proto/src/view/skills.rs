//! The composer's `/`-picker: when it opens, what it offers, what Enter does
//! (gh#134).
//!
//! Here rather than in a viewport because comet-native has three composers —
//! the gpui one, the terminal one, and eventually iOS — and a picker that
//! opened on `/` in one and on `src/` in another would be the same class of bug
//! as a sidebar sorted differently per surface. Everything below is pure, so
//! the trigger rules and the completion arithmetic have one test suite.

use crate::{SkillDescriptor, SkillSource};

/// The `/…` token the caret is currently inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashToken {
    /// Byte range of the token in the buffer, INCLUDING the leading slash.
    pub start: usize,
    pub end: usize,
    /// What was typed after the slash — the picker's filter query.
    pub query: String,
}

/// The token the picker should be filtering on, or `None` when the caret is
/// not in one.
///
/// Opens on a `/` at the very start of the buffer or after whitespace, which
/// is what keeps `src/main.rs` and `https://…` from summoning a menu. Closes
/// as soon as the token grows a space (the argument has started) or a second
/// slash (that was a path after all). `cursor` is a byte offset and must sit on
/// a char boundary — both composers keep their carets on grapheme boundaries,
/// which is a stronger guarantee.
pub fn slash_token(text: &str, cursor: usize) -> Option<SlashToken> {
    let cursor = cursor.min(text.len());
    if !text.is_char_boundary(cursor) {
        return None;
    }
    let head = &text[..cursor];
    let start = head.rfind('/')?;
    // A slash mid-word is a path separator, a URL, or a fraction — never a
    // command. Only start-of-buffer or after whitespace opens the picker.
    if let Some(before) = head[..start].chars().next_back()
        && !before.is_whitespace()
    {
        return None;
    }
    let query = &head[start + 1..];
    if query.chars().any(|c| c.is_whitespace() || c == '/') {
        return None;
    }
    Some(SlashToken {
        start,
        end: cursor,
        query: query.to_string(),
    })
}

/// Replace `token` with the completed invocation, returning the new buffer and
/// where the caret lands.
///
/// The trailing space is deliberate: a skill invocation almost always carries
/// arguments, and a completion that left the caret flush against the name
/// would re-open the picker on the very next keystroke.
pub fn complete(text: &str, token: &SlashToken, name: &str) -> (String, usize) {
    let mut out = String::with_capacity(text.len() + name.len() + 2);
    out.push_str(&text[..token.start]);
    out.push('/');
    out.push_str(name);
    out.push(' ');
    let cursor = out.len();
    out.push_str(&text[token.end.min(text.len())..]);
    (out, cursor)
}

/// Match rank for one skill: 0 for a name prefix, 1 for a name substring, 2 for
/// a description substring, `None` for no match. An empty query matches
/// everything at rank 1.
///
/// Descriptions rank last on purpose. Somebody typing `/com` wants
/// `comet-board`, not the four skills whose blurb happens to say "command" —
/// but once the name matches nothing, a description hit is better than an
/// empty menu.
pub fn rank(query: &str, skill: &SkillDescriptor) -> Option<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(1);
    }
    let name = skill.name.to_lowercase();
    if name.starts_with(&query) {
        return Some(0);
    }
    // A plugin/namespaced skill (`vercel:deploy`) should answer to its bare
    // half too: the colon is where it came from, not what it is called.
    if name
        .split_once(':')
        .is_some_and(|(_, bare)| bare.starts_with(&query))
    {
        return Some(0);
    }
    if name.contains(&query) {
        return Some(1);
    }
    let described = skill
        .description
        .as_deref()
        .is_some_and(|d| d.to_lowercase().contains(&query));
    described.then_some(2)
}

/// Filter + rank a skill list for the picker, stable within each rank.
///
/// Description hits are a *fallback*, not a supplement: as soon as any name
/// matches, they are dropped entirely. Keeping them turned `/com` into 44 rows
/// on a real machine — three of them named `com…` and forty whose blurb happens
/// to say "common" — which is a menu that has stopped answering the question.
pub fn filter(query: &str, skills: &[SkillDescriptor]) -> Vec<SkillDescriptor> {
    let mut ranked: Vec<(usize, usize, &SkillDescriptor)> = skills
        .iter()
        .enumerate()
        .filter_map(|(ix, skill)| rank(query, skill).map(|rank| (rank, ix, skill)))
        .collect();
    if ranked.iter().any(|&(rank, _, _)| rank < DESCRIPTION_RANK) {
        ranked.retain(|&(rank, _, _)| rank < DESCRIPTION_RANK);
    }
    ranked.sort_by_key(|&(rank, ix, _)| (rank, ix));
    ranked.into_iter().map(|(_, _, s)| s.clone()).collect()
}

/// The rank [`rank`] gives a description-only match.
const DESCRIPTION_RANK: usize = 2;

/// Catalog order for the picker before any query: name-sorted, project skills
/// ahead of personal ones on a tie.
///
/// Project first because a repo that ships a skill ships it to be used here,
/// and the name it collides with is the generic one. Deduped on name for the
/// same reason — the harness resolves a collision to exactly one of them, and a
/// menu offering the same `/deploy` twice cannot say which.
pub fn catalog(mut skills: Vec<SkillDescriptor>) -> Vec<SkillDescriptor> {
    skills.sort_by(|a, b| {
        let rank = |s: &SkillDescriptor| match s.source {
            SkillSource::Project => 0,
            SkillSource::Personal => 1,
        };
        a.name.cmp(&b.name).then(rank(a).cmp(&rank(b)))
    });
    skills.dedup_by(|a, b| a.name == b.name);
    skills
}

/// How many rows the picker shows at once before scrolling. Small on purpose:
/// the list is a completion aid, not a browser, and a menu taller than this
/// covers the conversation it is being typed into.
pub const PICKER_MAX_ROWS: usize = 7;

/// Index after moving `delta` rows in a list of `len`, wrapping at both ends.
/// Wrapping because the list is short and reversing direction at an edge costs
/// a glance at which edge you are on.
pub fn step(selected: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let len_i = len as isize;
    (((selected as isize + delta) % len_i + len_i) % len_i) as usize
}

/// Minimal scroll that keeps `selected` visible: given the window currently
/// showing (`first`, up to `max_rows` rows), the first row to draw and how many.
///
/// Minimal rather than derived-from-selection-alone so that arrowing back up
/// through a long list retraces the same rows it came down through — a window
/// computed fresh from the index alone re-anchors on every step and the list
/// appears to jump under a caret that only moved one row.
pub fn window(first: usize, selected: usize, len: usize, max_rows: usize) -> (usize, usize) {
    let visible = len.min(max_rows.max(1));
    if visible == 0 {
        return (0, 0);
    }
    let mut first = first.min(len - visible);
    if selected < first {
        first = selected;
    } else if selected >= first + visible {
        first = selected + 1 - visible;
    }
    (first, visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, description: Option<&str>, source: SkillSource) -> SkillDescriptor {
        SkillDescriptor {
            name: name.into(),
            description: description.map(str::to_owned),
            source,
        }
    }

    #[test]
    fn opens_at_the_start_and_after_whitespace_only() {
        let token = slash_token("/com", 4).expect("start of buffer opens");
        assert_eq!(
            (token.start, token.end, token.query.as_str()),
            (0, 4, "com")
        );

        let token = slash_token("please run /com", 15).expect("after a space opens");
        assert_eq!((token.start, token.query.as_str()), (11, "com"));

        // A newline is whitespace: a second-line command still opens.
        assert!(slash_token("first\n/com", 10).is_some());

        // The cases that must stay quiet.
        assert_eq!(slash_token("src/main.rs", 11), None, "path separator");
        assert_eq!(slash_token("https://x", 9), None, "url");
        assert_eq!(slash_token("//", 2), None, "comment marker");
        assert_eq!(slash_token("hello", 5), None, "no slash at all");
    }

    #[test]
    fn closes_once_the_argument_starts() {
        // Space after the name means the picker's job is done.
        assert_eq!(slash_token("/comet-board list", 17), None);
        // …but the caret parked back inside the name re-opens on it.
        let token = slash_token("/comet-board list", 4).expect("caret inside the name");
        assert_eq!(
            (token.start, token.end, token.query.as_str()),
            (0, 4, "com")
        );
        // A bare slash is a valid (empty) query — that is how the whole list
        // is browsed.
        let token = slash_token("/", 1).expect("bare slash opens");
        assert!(token.query.is_empty());
    }

    #[test]
    fn completion_replaces_the_token_and_leaves_room_for_arguments() {
        let text = "please run /com";
        let token = slash_token(text, text.len()).expect("token");
        let (out, cursor) = complete(text, &token, "comet-board");
        assert_eq!(out, "please run /comet-board ");
        assert_eq!(cursor, out.len());
        // The caret sits past the space, so the picker does not re-open.
        assert_eq!(slash_token(&out, cursor), None);

        // Mid-buffer completion keeps the tail.
        let text = "/com then stop";
        let token = slash_token(text, 4).expect("token");
        let (out, cursor) = complete(text, &token, "comet-board");
        assert_eq!(out, "/comet-board  then stop");
        assert_eq!(cursor, "/comet-board ".len());
    }

    #[test]
    fn ranking_prefers_names_then_descriptions() {
        let board = skill(
            "comet-board",
            Some("drive the board"),
            SkillSource::Personal,
        );
        let deploy = skill("vercel:deploy", Some("ship it"), SkillSource::Personal);
        let debug = skill(
            "debug",
            Some("read the command output"),
            SkillSource::Personal,
        );

        assert_eq!(rank("com", &board), Some(0), "name prefix");
        assert_eq!(rank("board", &board), Some(1), "name substring");
        assert_eq!(rank("dep", &deploy), Some(0), "bare half of a namespace");
        assert_eq!(rank("command", &debug), Some(2), "description only");
        assert_eq!(rank("zzz", &board), None);

        // A name match suppresses the description hits entirely.
        let rows = filter("com", &[debug.clone(), board.clone(), deploy.clone()]);
        assert_eq!(
            rows.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["comet-board"],
            "`debug` only matches on its blurb, and a named hit exists"
        );
        // With nothing named, the blurb is better than an empty menu.
        let rows = filter("command", &[debug.clone(), board.clone()]);
        assert_eq!(
            rows.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["debug"]
        );
        // An empty query is the whole list in catalog order.
        assert_eq!(filter("  ", &[debug, board, deploy]).len(), 3);
    }

    #[test]
    fn catalog_dedupes_collisions_in_favour_of_the_repo() {
        let rows = catalog(vec![
            skill("deploy", Some("mine"), SkillSource::Personal),
            skill("comet-board", None, SkillSource::Personal),
            skill("deploy", Some("this repo's"), SkillSource::Project),
        ]);
        assert_eq!(
            rows.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["comet-board", "deploy"]
        );
        let deploy = rows.iter().find(|s| s.name == "deploy").expect("deploy");
        assert_eq!(deploy.source, SkillSource::Project);
        assert_eq!(deploy.description.as_deref(), Some("this repo's"));
    }

    #[test]
    fn selection_wraps_and_the_window_follows_it() {
        assert_eq!(step(0, -1, 3), 2, "up from the top wraps to the bottom");
        assert_eq!(step(2, 1, 3), 0);
        assert_eq!(step(0, 1, 0), 0, "an empty list has nowhere to go");

        // Short lists show everything, anchored at the top.
        assert_eq!(window(0, 0, 3, 7), (0, 3));
        // Long lists scroll only once the selection leaves the window…
        assert_eq!(window(0, 6, 20, 7), (0, 7));
        assert_eq!(window(0, 7, 20, 7), (1, 7));
        // …and scrolling back up moves it by one, not back to an anchor.
        assert_eq!(window(1, 6, 20, 7), (1, 7));
        assert_eq!(window(1, 0, 20, 7), (0, 7));
        // A wrap to the end lands the selection on the last visible row.
        assert_eq!(window(0, 19, 20, 7), (13, 7));
        // A window left past the end by a shrinking list is pulled back in.
        assert_eq!(window(13, 2, 8, 7), (1, 7));
    }
}
