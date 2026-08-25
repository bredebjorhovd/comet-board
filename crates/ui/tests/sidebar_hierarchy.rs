//! The rule is the deliverable: **the sidebar reads as two sections and a pin**
//! (gh#547).
//!
//! The strangeness the ticket named was not any one element — it was that
//! nothing on screen said what contains what. Needs you, Spaces and the loose
//! Active group all rendered the same header; the orchestrator lived inside a
//! disclosure it does not belong to; and one blocked attempt could appear as
//! three unrelated-looking rows. The fix is composition, and composition is
//! the kind of thing that drifts back one reasonable-looking edit at a time,
//! so like `sidebar_seam.rs` this test reads the source and checks the shape:
//!
//! - **Order**: Needs you (the projection), then the orchestrator's fixture,
//!   then the section hairline, then Spaces (which owns where things live).
//! - **The pin is global**: its chat is held out of every disclosure
//!   unconditionally — one Orchestrator per sidebar.
//! - **No forced-open coupling survives**: nothing re-hides the fixture by
//!   collapsing a space, because no disclosure holds it.
//! - **No fourth section**: the homeless live runs draw under Spaces' Unfiled
//!   header, and nothing renders an `"Active"` list beside the tree.

use std::path::{Path, PathBuf};

/// The chat-mode sidebar's scroll region: where the sections compose.
const COMPOSITION: &str = "fn render_chat_sidebar(";
/// The space disclosure: where the pinned chat must be held out.
const DISCLOSURE: &str = "fn render_space_disclosure(";
/// The Spaces section: where the Unfiled tail lives.
const SPACES: &str = "fn render_spaces_section(";

#[test]
fn two_sections_and_a_pin_compose_in_that_order() {
    let body = fn_body(&shell_rs(), COMPOSITION).expect("render_chat_sidebar");
    let order = [
        ".child(needs_section)",
        ".children(orchestrator_fixture)",
        "Self::render_sidebar_rule(theme)",
        ".child(spaces_section)",
    ];
    let mut last: usize = 0;
    for marker in order {
        let hit = body
            .iter()
            .position(|(_, l)| l.contains(marker))
            .unwrap_or_else(|| panic!("`{marker}` missing from render_chat_sidebar"));
        let line = body[hit].0;
        assert!(
            line > last,
            "`{marker}` composes out of order (line {line}): the sidebar is \
             Needs you → orchestrator fixture → section hairline → Spaces \
             (gh#547)"
        );
        last = line;
    }
}

#[test]
fn the_pinned_chat_is_held_out_of_every_disclosure_unconditionally() {
    let spaces_rs = spaces_rs();
    let body = fn_body(&spaces_rs, DISCLOSURE).expect("render_space_disclosure");
    assert!(
        body.iter()
            .any(|(_, l)| l.contains("self.state.read(cx).orchestrator.clone()")),
        "the disclosure no longer holds the pinned chat out unconditionally — \
         without that, the fixture above the tree and the chat row in its \
         space are the same task twice"
    );
    // And nothing scopes it to the selected space again: the whole point of
    // moving the fixture out was that it shows from every space.
    assert!(
        !body.iter().any(|(_, l)| l.contains("orchestrator_slot(")),
        "the disclosure consults the slot per-space again; the hold must be \
         unconditional (gh#547)"
    );
    assert!(
        !std::fs::read_to_string(&spaces_rs)
            .expect("read spaces.rs")
            .contains("space_disclosure_forced_open"),
        "`space_disclosure_forced_open` came back: with the fixture outside \
         the tree there is no disclosure to force open, so the coupling it \
         existed for cannot recur"
    );
}

#[test]
fn live_runs_with_no_space_draw_under_spaces_not_beside_it() {
    let body = fn_body(&spaces_rs(), SPACES).expect("render_spaces_section");
    assert!(
        body.iter().any(|(_, l)| l.contains("render_unfiled_header")),
        "Spaces no longer draws the Unfiled tail: live runs whose chat names \
         no space must stay inside the section that owns where things live \
         (gh#547), not in a fourth place beside it"
    );
    let source = std::fs::read_to_string(spaces_rs()).expect("read spaces.rs");
    assert!(
        !source.contains("SharedString::from(\"Active\")"),
        "an \"Active\" header is being rendered again: since gh#258 the \
         desktop has no full Active list, and a header saying Active over a \
         remainder was gh#547's original complaint"
    );
}

fn shell_rs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/shell.rs")
}

fn spaces_rs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/shell/spaces.rs")
}

/// The body of the `fn` whose signature starts at `head`, as `(line, text)`
/// pairs — from the signature to the closing brace at the same indent.
fn fn_body(path: &Path, head: &str) -> Option<Vec<(usize, String)>> {
    let lines: Vec<String> = std::fs::read_to_string(path)
        .expect("read source")
        .lines()
        .map(str::to_string)
        .collect();
    let start = lines.iter().position(|l| l.contains(head))?;
    let indent = lines[start].len() - lines[start].trim_start().len();
    let close = " ".repeat(indent) + "}";
    let mut out = Vec::new();
    for (ix, line) in lines.iter().enumerate().skip(start) {
        out.push((ix + 1, line.clone()));
        if line.as_str() == close {
            return Some(out);
        }
    }
    None
}
