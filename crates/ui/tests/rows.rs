//! The rule is the deliverable: **selection's hairline has exactly one
//! source** (gh#175).
//!
//! Hover and selection are two channels now — a flat wash for the pointer, a
//! change of surface plus a 1px edge for the row that is yours — and
//! `Theme::row` is the only place that decides either. The wash half cannot be
//! policed by grep (a wash is just a colour), but the structural half can: the
//! edge is an INSET box shadow, and an inset shadow has no other job in this
//! app. So an inset shadow written anywhere but `theme.rs` is a list quietly
//! inventing its own selection language again — which is how the app arrived
//! at "hover equals active, told apart by a ring at 9% white".
//!
//! Drop shadows are untouched: the presence-dot glow, the popover cards and
//! the drag ghosts are painting depth, not saying "this one".
//!
//! The escape hatch is a `ring-ok: <reason>` comment on the same line or in
//! the comment block directly above, and it is for an edge that is genuinely
//! not about selection. Every use must say which.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// An inset shadow built outside the theme.
struct Violation {
    file: PathBuf,
    line: usize,
    text: String,
}

#[test]
fn the_selected_row_hairline_comes_from_the_theme() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);
    assert!(
        files.len() > 20,
        "expected to walk the whole crate, found {} files under {}",
        files.len(),
        src.display()
    );

    let mut violations = Vec::new();
    for file in &files {
        // theme.rs is where the ring lives — that is the whole point.
        if file.file_name().is_some_and(|n| n == "theme.rs") {
            continue;
        }
        let body = std::fs::read_to_string(file).expect("read source");
        let lines: Vec<&str> = body.lines().collect();
        for (ix, line) in lines.iter().enumerate() {
            if !is_inset(line) || exempt(&lines, ix, "ring-ok:") {
                continue;
            }
            violations.push(Violation {
                file: file.strip_prefix(&src).unwrap_or(file).to_path_buf(),
                line: ix + 1,
                text: line.trim().to_string(),
            });
        }
    }

    if violations.is_empty() {
        return;
    }
    let mut msg = format!(
        "{} hand-built inset shadow(s). A selected row asks `Theme::row(bed, \
         true)` — which answers hover and selection together, and knows which \
         way to move against the surface the row sits on. If the edge is not \
         about selection, say so with a `ring-ok: <reason>` comment:\n",
        violations.len()
    );
    for v in &violations {
        let _ = writeln!(msg, "  {}:{} {}", v.file.display(), v.line, v.text);
    }
    panic!("{msg}");
}

/// `inset: true` on this line — the field literal of a `gpui::BoxShadow`.
fn is_inset(line: &str) -> bool {
    let line = line.trim_start();
    !line.starts_with("//") && line.replace(' ', "").contains("inset:true")
}

/// `<marker> <reason>` on this line, or anywhere in the comment block directly
/// above it (same convention as `tests/scale.rs`).
fn exempt(lines: &[&str], ix: usize, marker: &str) -> bool {
    if lines[ix].contains(marker) {
        return true;
    }
    let mut above = ix;
    while above > 0 && lines[above - 1].trim_start().starts_with("//") {
        above -= 1;
        if lines[above].contains(marker) {
            return true;
        }
    }
    false
}

fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("read crate source dir");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn inset_literals_are_found_and_comments_are_not() {
        assert!(is_inset("            inset: true,"));
        assert!(is_inset("shadow(vec![BoxShadow { inset:true }])"));
        assert!(!is_inset("            inset: false,"));
        assert!(!is_inset("// inset: true is how the ring is drawn"));
    }

    #[test]
    fn the_escape_hatch_needs_a_marker() {
        let lines = vec![
            "// ring-ok: a pressed key cap, not a selected row",
            "inset: true,",
            "inset: true,",
        ];
        assert!(exempt(&lines, 1, "ring-ok:"));
        assert!(!exempt(&lines, 2, "ring-ok:"));
    }
}
