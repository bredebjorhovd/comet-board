//! The rule is the deliverable: **no literal radius and no literal font size
//! outside `theme.rs`** (gh#174).
//!
//! Ten radii were in use — 4.5, 5, 6, 8, 10, 12, 16, 26 and full-round — and
//! roughly as many type sizes, six of them inside the 520px board pane alone.
//! Neither was a scale; both were inventories, assembled one reasonable call
//! site at a time. `theme.rs` now declares three radii
//! (`RADIUS_CHIP`/`RADIUS_ROW`/`RADIUS_CARD`), four type sizes
//! (`TEXT_CAPTION`/`TEXT_DENSE`/`TEXT_BODY`/`TEXT_TITLE`), the reserved prose
//! pair and one figure size — and this test is what keeps the eleventh from
//! arriving.
//!
//! Three things are checked, and each has the same escape hatch:
//!
//! 1. `rounded*(px(<number>))` — a radius that is not one of the three.
//! 2. `text_size(px(<number>))` — a size that is not one of the six.
//! 3. `rounded_full()` — the exception, and it is for status dots and the send
//!    button. One round thing on screen, and it is the one you press.
//!
//! The hatch for 1 and 2 is a `scale-ok: <reason>` comment on the same line or
//! the line above; for 3 it is `round-ok: <reason>`. Both are for marks that
//! are DRAWN rather than boxed — a chart column, a 2px rail marker, the stop
//! glyph inside the send button, a bar's round cap — whose corner relates to
//! their own few pixels of width and not to the box they sit in. Every use
//! must say which.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// A literal that should have been a token.
struct Violation {
    file: PathBuf,
    line: usize,
    text: String,
    why: String,
}

#[test]
fn no_literal_radius_or_font_size_outside_the_theme() {
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
        // theme.rs is where the numbers live — that is the whole point.
        if file.file_name().is_some_and(|n| n == "theme.rs") {
            continue;
        }
        let body = std::fs::read_to_string(file).expect("read source");
        let lines: Vec<&str> = body.lines().collect();
        for (ix, line) in lines.iter().enumerate() {
            if exempt(&lines, ix, "scale-ok:") {
                continue;
            }
            for why in offences(line) {
                violations.push(Violation {
                    file: file.strip_prefix(&src).unwrap_or(file).to_path_buf(),
                    line: ix + 1,
                    text: line.trim().to_string(),
                    why,
                });
            }
        }
    }

    report(
        violations,
        "literal(s) where a scale token belongs. Radii are RADIUS_CHIP (6) / \
         RADIUS_ROW (10) / RADIUS_CARD (14) and type is TEXT_CAPTION (11) / \
         TEXT_DENSE (12) / TEXT_BODY (13) / TEXT_TITLE (15), with TEXT_PROSE \
         reserved for the transcript and TEXT_FIGURE for a number shown as a \
         number. Where a value does not fit, the fix is usually the weight — \
         not an eleventh number:",
    );
}

/// The send button and the dots, and nothing else — every surviving
/// `rounded_full()` says which it is.
#[test]
fn full_round_is_a_dot_or_the_send_button() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect_rs(&src, &mut files);

    let mut violations = Vec::new();
    for file in &files {
        let body = std::fs::read_to_string(file).expect("read source");
        let lines: Vec<&str> = body.lines().collect();
        for (ix, line) in lines.iter().enumerate() {
            if !line.contains(".rounded_full()") || exempt(&lines, ix, "round-ok:") {
                continue;
            }
            violations.push(Violation {
                file: file.strip_prefix(&src).unwrap_or(file).to_path_buf(),
                line: ix + 1,
                text: line.trim().to_string(),
                why: "unmarked".into(),
            });
        }
    }

    report(
        violations,
        "unmarked full-round(s). Roundness that is not a status dot, a drawn \
         cap or the send button belongs on the three-step scale; if it really \
         is one of those, say so with a `round-ok: <reason>` comment:",
    );
}

fn report(violations: Vec<Violation>, headline: &str) {
    if violations.is_empty() {
        return;
    }
    let mut msg = format!("{} {headline}\n", violations.len());
    for v in &violations {
        let _ = writeln!(
            msg,
            "  {}:{} ({}) {}",
            v.file.display(),
            v.line,
            v.why,
            v.text
        );
    }
    panic!("{msg}");
}

/// The radius/size literals on this line, if any.
fn offences(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (call, kind) in [
        ("text_size(px(", "font size"),
        ("rounded(px(", "radius"),
        ("rounded_t(px(", "radius"),
        ("rounded_b(px(", "radius"),
        ("rounded_l(px(", "radius"),
        ("rounded_r(px(", "radius"),
        ("rounded_tl(px(", "radius"),
        ("rounded_tr(px(", "radius"),
        ("rounded_bl(px(", "radius"),
        ("rounded_br(px(", "radius"),
    ] {
        for (at, _) in line.match_indices(call) {
            let arg = &line[at + call.len()..];
            let Some(end) = arg.find(')') else { continue };
            if let Some(n) = as_number(&arg[..end]) {
                out.push(format!("{kind} {n}"));
            }
        }
    }
    out
}

/// The argument as a bare number, or `None` when it is a name or an
/// expression (`Theme::RADIUS_ROW`, `cell_px / 2.0`, `MARK_RADIUS`).
fn as_number(arg: &str) -> Option<f32> {
    let arg = arg.trim();
    (!arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == '_'))
    .then(|| arg.replace('_', "").parse().ok())
    .flatten()
}

/// `<marker> <reason>` on this line, or anywhere in the comment block directly
/// above it — the reasons run to two lines and the marker word should not have
/// to be on the last of them.
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
    fn literals_are_offences_and_tokens_are_not() {
        assert_eq!(offences(".rounded(px(12.0))"), ["radius 12"]);
        assert_eq!(offences(".text_size(px(10.5))"), ["font size 10.5"]);
        assert_eq!(offences(".rounded_tr(px(6.0))"), ["radius 6"]);
        // Tokens, module constants and expressions all pass — the rule is
        // "name the number", not "never write a radius".
        assert!(offences(".rounded(px(Theme::RADIUS_CARD))").is_empty());
        assert!(offences(".text_size(px(Theme::TEXT_BODY))").is_empty());
        assert!(offences(".rounded(px(MARK_RADIUS))").is_empty());
        assert!(offences(".rounded(px(cell_px / 2.0))").is_empty());
        assert!(offences(".rounded(px(16.0 * scale))").is_empty());
        // Other px() calls are layout, not scale — untouched.
        assert!(offences(".size(px(28.0)).px(px(8.0))").is_empty());
        // Two on one line are two findings.
        assert_eq!(
            offences("div().size(px(11.0)).rounded(px(3.0)).text_size(px(9.0))"),
            ["font size 9", "radius 3"]
        );
    }

    #[test]
    fn the_escape_hatch_needs_a_marker() {
        let lines = ["// scale-ok: a drawn cap", ".rounded(px(1.0))", "x"];
        assert!(exempt(&lines, 1, "scale-ok:"));
        assert!(!exempt(&lines, 2, "scale-ok:"));
        // The marker may sit anywhere in the comment block above — a reason
        // worth writing is usually longer than one line.
        let wrapped = [
            "    // scale-ok: because",
            "    // of this",
            "    .rounded(px(1.0))",
        ];
        assert!(exempt(&wrapped, 2, "scale-ok:"));
        // But the block has to be contiguous with the offending line.
        let gapped = ["// scale-ok: because", "", ".rounded(px(1.0))"];
        assert!(!exempt(&gapped, 2, "scale-ok:"));
        // The two hatches are separate: a round-ok does not license a literal.
        assert!(!exempt(
            &["// round-ok: dot", ".rounded(px(3.0))"],
            1,
            "scale-ok:"
        ));
    }
}
