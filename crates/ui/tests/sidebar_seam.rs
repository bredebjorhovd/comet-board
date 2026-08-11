//! The rule is the deliverable: **the sidebar takes no tone the shell does not**
//! (gh#304).
//!
//! The canvases separate the sidebar column from the shell with a 1px `--line`
//! hairline and nothing else (claim A3) — one tone on both sides of the seam.
//! The app painted a `wash(0.05)` over the sidebar column on top of the
//! window's frost, which in light read 218 against the shell's 231: a
//! 13-point step where the canvas has none. It went unnoticed for as long as
//! the tab strip started at x=274, entirely right of the boundary; gh#293/#299
//! moved the tabs to x=172, where the canvas puts them, and the strip then
//! crossed the seam and the tabs read as straddling two panels.
//!
//! This is not about the frost, which stays: the window's root paints
//! [`Theme::glass`], both sides of the seam equally, and gh#177's
//! wallpaper-proofing is untouched. What is banned is an *internal* step —
//! a fill on the sidebar column that the shell beside it does not get.
//!
//! A tone here is the kind of thing that creeps back one reasonable-looking
//! call site at a time (it is one `.bg(` on a full-height band), so the two
//! elements that make up the column are read from the source and checked for
//! a fill: the full-height band behind the sidebar, and the sidebar pane
//! itself.

use std::path::{Path, PathBuf};

/// The full-height band that draws the sidebar/shell seam.
const BAND: &str = "let sidebar_column = div()";
/// The sidebar pane, which sits on the frost and paints nothing of its own.
const PANE: &str = "fn render_sidebar(";

#[test]
fn the_sidebar_column_takes_no_fill_the_shell_does_not() {
    let shell = shell_rs();
    let body = std::fs::read_to_string(&shell).expect("read shell.rs");
    let lines: Vec<&str> = body.lines().collect();

    let band = chain(&lines, BAND).unwrap_or_else(|| panic!("no `{BAND}` in {}", shell.display()));
    let fill = band.iter().find(|(_, l)| l.contains(".bg("));
    if let Some((line, text)) = fill {
        panic!(
            "shell.rs:{line} paints the sidebar column: {text}\n\
             The canvas separates the sidebar from the shell with the `--line` \
             hairline and nothing else — one tone on both sides. A fill here \
             composites ON TOP of the window frost both sides already share, \
             so it is an internal step the canvas never had, and the tab strip \
             (x=172) crosses the seam it draws. If the seam needs strengthening, \
             it is the hairline that changes, not the ground (gh#304)."
        );
    }
    // The hairline is the seam, and it is the whole seam: if it goes, the
    // column stops reading as a column at all.
    assert!(
        band.iter().any(|(_, l)| l.contains(".border_r_1()")),
        "the sidebar column lost its `--line` hairline — that is what separates \
         it from the shell now that nothing else does"
    );

    // The pane in front of the band is transparent for the same reason: the
    // sidebar sits directly on the frost.
    let pane = fn_body(&lines, PANE).unwrap_or_else(|| panic!("no `{PANE}` in {}", shell.display()));
    if let Some((line, text)) = pane.iter().find(|(_, l)| l.contains(".bg(")) {
        panic!(
            "shell.rs:{line} fills the sidebar pane: {text}\n\
             The sidebar is transparent — it sits on the window frost the shell \
             beside it sits on, and the card's own border does the separating \
             (gh#304)."
        );
    }
}

/// The `let <binding> = div()` builder chain starting at `head`, up to the
/// line that closes the statement, as `(1-based line, text)` pairs.
fn chain<'a>(lines: &[&'a str], head: &str) -> Option<Vec<(usize, &'a str)>> {
    let start = lines.iter().position(|l| l.contains(head))?;
    let mut out = Vec::new();
    for (ix, line) in lines.iter().enumerate().skip(start) {
        out.push((ix + 1, *line));
        if line.trim_end().ends_with(';') {
            return Some(out);
        }
    }
    None
}

/// The body of the `fn` whose signature starts at `head`, as `(line, text)`
/// pairs — from the signature to the closing brace at the same indent.
fn fn_body<'a>(lines: &[&'a str], head: &str) -> Option<Vec<(usize, &'a str)>> {
    let start = lines.iter().position(|l| l.contains(head))?;
    let indent = lines[start].len() - lines[start].trim_start().len();
    let close = " ".repeat(indent) + "}";
    let mut out = Vec::new();
    for (ix, line) in lines.iter().enumerate().skip(start) {
        out.push((ix + 1, *line));
        if *line == close {
            return Some(out);
        }
    }
    None
}

fn shell_rs() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/shell.rs")
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn a_chain_ends_at_its_semicolon() {
        let lines = [
            "let x = 1;",
            "let sidebar_column = div()",
            "    .absolute()",
            "    .border_r_1();",
            "let after = 2;",
        ];
        let got = chain(&lines, BAND).expect("found");
        assert_eq!(got.first().map(|(n, _)| *n), Some(2));
        assert_eq!(got.last().map(|(n, _)| *n), Some(4));
        assert!(chain(&lines, "let missing = div()").is_none());
    }

    #[test]
    fn a_fn_body_ends_at_its_own_indent() {
        let lines = [
            "impl Shell {",
            "    fn render_sidebar(&mut self) -> AnyElement {",
            "        if x {",
            "        }",
            "        div().into_any_element()",
            "    }",
            "    fn next(&self) {}",
            "}",
        ];
        let got = fn_body(&lines, PANE).expect("found");
        assert_eq!(got.last().map(|(n, _)| *n), Some(6));
    }
}
