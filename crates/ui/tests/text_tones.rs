//! The rule is the deliverable: **no `.opacity()` on a text colour, ever** (gh#172).
//!
//! `theme.rs` declares four text tones and the whole crate paints with those
//! four. Before this test the three tones that existed were multiplied at the
//! call site by `.35`, `.45`, `.5`, `.55`, `.6`, `.65`, `.7`, `.75`, `.8`,
//! `.85` and `.9` — a dozen unnamed greys, none contrast-checked, several
//! landing two percent apart and reading as noise rather than as levels. The
//! eleven multipliers each arrived at one reasonable-looking call site, which
//! is exactly why review discipline could not hold the line and a test can.
//!
//! If a tone is genuinely needed, it becomes a token in `theme.rs` (and gets
//! its contrast asserted there) rather than an alpha here. Non-text opacity —
//! washes, scrims, fills, hairlines, whole-element fades, animation alphas —
//! is out of scope and untouched: that is what `Theme::wash` and
//! `Theme::white_alpha` are for.
//!
//! The escape hatch is a `theme-opacity-ok:` comment on the same line or the
//! line above, and it is for animation alphas only — a crossfade between two
//! tokens is a fade, not a twelfth grey. Every use must say why.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Receiver names that hold a text colour. Anything whose name contains `text`
/// is caught by substring (`theme.text`, `text_muted`, `red_text`,
/// `danger_text()`, a local `text_color`); the rest are the crate's text-toned
/// locals that happen not to say so.
const TEXT_LOCALS: [&str; 3] = ["subline", "tone", "fg"];

/// A `.opacity(` we found applied to a text colour.
struct Violation {
    file: PathBuf,
    line: usize,
    text: String,
    why: &'static str,
}

#[test]
fn no_text_colour_is_multiplied_by_an_alpha() {
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
        let body = std::fs::read_to_string(file).expect("read source");
        let lines: Vec<&str> = body.lines().collect();
        for (ix, line) in lines.iter().enumerate() {
            if !line.contains(".opacity(") || exempt(&lines, ix) {
                continue;
            }
            if let Some(why) = offence(line) {
                violations.push(Violation {
                    file: file.strip_prefix(&src).unwrap_or(file).to_path_buf(),
                    line: ix + 1,
                    text: line.trim().to_string(),
                    why,
                });
            }
        }
    }

    if !violations.is_empty() {
        let mut msg = format!(
            "{} text colour(s) multiplied by an alpha. Text paints in the four \
             theme tones (text / text_muted / text_subtle / text_faint) and \
             nothing in between; if a real tone is missing, add a token in \
             theme.rs with its contrast asserted:\n",
            violations.len()
        );
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
}

/// Why this line is an offence, or `None` if its opacity is non-text paint.
fn offence(line: &str) -> Option<&'static str> {
    // A text tone, multiplied. The receiver is whatever identifier (or
    // `method()` call) sits immediately left of `.opacity(`.
    for (at, _) in line.match_indices(".opacity(") {
        if is_text_receiver(receiver(&line[..at])) {
            return Some("text tone");
        }
    }
    // Anything at all, multiplied INSIDE a `.text_color(...)` — catches the
    // accents and brand tints painted as text, where the receiver name gives
    // nothing away (`theme.accent.opacity(0.85)`).
    let paints_text = line.find(".text_color(")?;
    line[paints_text..]
        .contains(".opacity(")
        .then_some("inside text_color")
}

/// The receiver expression immediately left of a `.opacity(`, reduced to its
/// trailing name: `theme.text_muted` -> `text_muted`, `t.danger_text()` ->
/// `danger_text`. Returns `""` when the receiver is a closing paren or index
/// (`unwrap_or(..)`, `tones[0]`) — those are structural, not named.
fn receiver(before: &str) -> &str {
    let b = before.trim_end();
    // Method call: step over an empty trailing `()` to reach the name.
    let b = match b.strip_suffix("()") {
        Some(stripped) => stripped,
        None if b.ends_with(')') || b.ends_with(']') => return "",
        None => b,
    };
    let start = b
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map_or(0, |i| i + 1);
    &b[start..]
}

fn is_text_receiver(name: &str) -> bool {
    !name.is_empty() && (name.contains("text") || TEXT_LOCALS.contains(&name))
}

/// `theme-opacity-ok: <reason>` on this line or the one above.
fn exempt(lines: &[&str], ix: usize) -> bool {
    let marked = |l: &str| l.contains("theme-opacity-ok:");
    marked(lines[ix]) || (ix > 0 && marked(lines[ix - 1]))
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
    fn receiver_is_the_name_left_of_the_call() {
        assert_eq!(receiver("theme.text_muted"), "text_muted");
        assert_eq!(receiver("    .text_color(theme.text"), "text");
        assert_eq!(receiver("theme.danger_text()"), "danger_text");
        assert_eq!(receiver("let x = red_text"), "red_text");
        assert_eq!(receiver("tint.unwrap_or(theme.text)"), "");
    }

    #[test]
    fn offences_are_text_and_non_offences_are_paint() {
        assert!(offence(".text_color(theme.text_muted.opacity(0.6))").is_some());
        assert!(offence("let subline = theme.text_faint.opacity(0.7);").is_some());
        assert!(offence(".text_color(theme.accent.opacity(0.85))").is_some());
        assert!(offence("theme.warning_text().opacity(0.9),").is_some());
        // Non-text paint keeps its alphas: fills, hairlines, element fades.
        assert!(offence(".bg(theme.accent.opacity(0.16))").is_none());
        assert!(offence(".border_color(danger.opacity(0.16))").is_none());
        assert!(offence(".when(busy, |el| el.opacity(0.4))").is_none());
        assert!(offence("grey(0x16).opacity(0.65)").is_none());
        // A text colour landing in a NON-text slot is still an offence: the
        // four tones are text, and a border wants `white_alpha`.
        assert!(offence(".border_color(theme.text.opacity(0.3))").is_some());
    }

    #[test]
    fn the_escape_hatch_needs_a_marker() {
        let lines = ["// theme-opacity-ok: crossfade", "x.text.opacity(a)", "y"];
        assert!(exempt(&lines, 1));
        assert!(!exempt(&lines, 2));
    }
}
