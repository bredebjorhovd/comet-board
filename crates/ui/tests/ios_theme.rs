//! The phone reads from the same system (gh#181).
//!
//! `apps/ios/Comet/Theme/Theme.swift` is a second declaration of the design
//! system this crate owns — it has to be, because no Rust runs on that device.
//! Two declarations of one system is how a phone comes to disagree with a
//! laptop about what colour a working agent is, which is exactly what happened:
//! `boardStateColor` said amber and `ChatIndicator.dotColor` said pink, one
//! screen apart, months after the desktop had settled that argument.
//!
//! So this file reads the Swift as text and holds it to the Rust. Four checks,
//! mirroring `text_tones.rs` and `scale.rs` for the other viewport:
//!
//! 1. **Parity.** The status ramp, the three radii, the type scale and the four
//!    text greys are the same NUMBERS the desktop paints (`ios_theme_declares_the_same_system`).
//! 2. **No text tone is multiplied by an alpha** (gh#172), hatch
//!    `theme-opacity-ok:`.
//! 3. **No literal radius or font size outside `Theme.swift`** (gh#174), hatch
//!    `scale-ok:`.
//! 4. **`Capsule()` / `Circle()` is a dot, a drawn cap or the send button**
//!    (gh#174), hatch `round-ok:`.
//!
//! Why here and not in the phone's own harness: `SpecRunner` needs a simulator,
//! so it runs when somebody remembers to run it and never in CI (see
//! `apps/ios/README.md`). These four rules are pure text scans, so putting them
//! in `cargo test` closes that gap for this class of drift — a PR that lets the
//! phone slip off the system fails on the same runner that builds the desktop.
//!
//! It lives in `comet-ui` rather than `comet-proto` because it needs both ends:
//! the status ramp from `comet_proto::view::status`, and `Theme::RADIUS_*` /
//! `Theme::TEXT_*` / `Theme::dark()` from here.

use comet_proto::view::status;
use comet_ui::theme::{Theme, neutral};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The iOS sources, relative to this crate.
fn ios_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/ios/Comet")
        .canonicalize()
        .expect("apps/ios/Comet must exist — the phone is part of this repo")
}

fn theme_swift() -> String {
    std::fs::read_to_string(ios_root().join("Theme/Theme.swift")).expect("read Theme.swift")
}

// ---------------------------------------------------------------------------
// 1. Parity
// ---------------------------------------------------------------------------

/// The value of `static let <name> = …` / `static let <name>: T = …`, trimmed.
fn declaration<'a>(src: &'a str, name: &str) -> &'a str {
    for line in src.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("static let ") else {
            continue;
        };
        let Some((lhs, rhs)) = rest.split_once('=') else {
            continue;
        };
        // `name` or `name: CGFloat`
        let declared = lhs.split(':').next().unwrap_or("").trim();
        if declared == name {
            return rhs.split("//").next().unwrap_or(rhs).trim();
        }
    }
    panic!("Theme.swift declares no `{name}` — the phone dropped a token the desktop still has");
}

/// A bare number declaration (`static let radiusChip: CGFloat = 6`).
fn number(src: &str, name: &str) -> f32 {
    let raw = declaration(src, name);
    raw.parse().unwrap_or_else(|_| {
        panic!("Theme.swift's `{name}` is `{raw}`, which is not a plain number")
    })
}

/// The argument of a one-argument call (`neutral(0.938)` -> `0.938`).
fn call_arg(src: &str, name: &str, func: &str) -> f32 {
    let raw = declaration(src, name);
    let inner = raw
        .strip_prefix(func)
        .and_then(|r| r.strip_prefix('('))
        .and_then(|r| r.strip_suffix(')'))
        .unwrap_or_else(|| panic!("Theme.swift's `{name}` is `{raw}`, expected `{func}(…)`"));
    inner
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("`{name}`'s argument `{inner}` is not a plain number"))
}

#[test]
fn ios_theme_declares_the_same_system() {
    let src = theme_swift();
    let mut wrong = Vec::new();
    let mut check = |what: &str, phone: f32, desktop: f32| {
        if (phone - desktop).abs() > f32::EPSILON {
            wrong.push(format!("  {what}: phone {phone}, desktop {desktop}"));
        }
    };

    // The status ramp — one lightness, one chroma, four hues (gh#173).
    check("statusL", number(&src, "statusL"), status::L);
    check("statusC", number(&src, "statusC"), status::C);
    check("hueBlocked", number(&src, "hueBlocked"), status::BLOCKED);
    check("hueWorking", number(&src, "hueWorking"), status::WORKING);
    check("hueReview", number(&src, "hueReview"), status::REVIEW);
    check("hueSettled", number(&src, "hueSettled"), status::SETTLED);

    // Three radii, and the gutter that makes the nesting rule true (gh#174).
    check("radiusChip", number(&src, "radiusChip"), Theme::RADIUS_CHIP);
    check("radiusRow", number(&src, "radiusRow"), Theme::RADIUS_ROW);
    check("radiusCard", number(&src, "radiusCard"), Theme::RADIUS_CARD);
    // `nestGutter` is arithmetic on the two above in both languages, so what is
    // asserted is that the arithmetic still lands on SPACE_XS.
    assert_eq!(
        declaration(&src, "nestGutter"),
        "radiusCard - radiusRow",
        "the nesting rule's gutter must stay derived from the scale, not typed in"
    );
    check("nestGutter", Theme::NEST_GUTTER, Theme::SPACE_XS);

    // Four type sizes, plus prose and one figure (gh#174).
    check(
        "textCaption",
        number(&src, "textCaption"),
        Theme::TEXT_CAPTION,
    );
    check("textDense", number(&src, "textDense"), Theme::TEXT_DENSE);
    check("textBody", number(&src, "textBody"), Theme::TEXT_BODY);
    check("textTitle", number(&src, "textTitle"), Theme::TEXT_TITLE);
    check("textProse", number(&src, "textProse"), Theme::TEXT_PROSE);
    check(
        "proseLineHeight",
        number(&src, "proseLineHeight"),
        Theme::PROSE_LINE_HEIGHT,
    );
    check("textFigure", number(&src, "textFigure"), Theme::TEXT_FIGURE);

    // Four text greys (gh#172). Compared as PAINT, not as lightness: both
    // languages run the same oklch→sRGB conversion, so the assertion is that
    // what lands on the phone's screen is the tone that lands on the desktop's.
    let dark = Theme::dark();
    for (name, desktop) in [
        ("text", dark.text),
        ("textMuted", dark.text_muted),
        ("textSubtle", dark.text_subtle),
        ("textFaint", dark.text_faint),
    ] {
        let phone = neutral(call_arg(&src, name, "neutral"));
        if phone != desktop {
            wrong.push(format!("  {name}: phone {phone:?}, desktop {desktop:?}"));
        }
    }

    assert!(
        wrong.is_empty(),
        "{} value(s) where the phone has drifted off the desktop's system. \
         The numbers live in crates/ui/src/theme.rs and comet_proto::view::status; \
         apps/ios/Comet/Theme/Theme.swift restates them because no Rust runs on \
         that device, and restating them differently is the bug this test \
         exists for:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// The status ramp is a vocabulary on both sides, and the phone must translate
/// every state into it exactly once — never answer the question itself.
#[test]
fn ios_maps_every_state_through_the_ramp() {
    let src = theme_swift();
    for f in [
        "static func ofBoard(",
        "static func ofAgent(",
        "static func ofChat(",
    ] {
        assert!(
            src.contains(f),
            "Theme.swift is missing `{f}` — a state type that does not translate into \
             `Status` is a state type that gets to pick its own colour (gh#173)"
        );
    }
    assert!(
        src.contains("static func status(_ status: Status) -> Color"),
        "Theme.swift must keep ONE function that turns a meaning into paint"
    );

    // Nothing outside the theme may reach for a hue by name to mean a STATE.
    // (`Theme.warning` etc. are still legitimate as paint — what is banned is
    // reviving the fifth hue the anchor retired.)
    let mut revived = Vec::new();
    for file in swift_sources() {
        let body = std::fs::read_to_string(&file).expect("read source");
        for (ix, line) in body.lines().enumerate() {
            if line.contains("statusWorking") || line.contains("statusCompleted") {
                revived.push(format!("  {}:{}", rel(&file).display(), ix + 1));
            }
        }
    }
    assert!(
        revived.is_empty(),
        "the retired status tokens are back:\n{}\nA running agent is `Status.working` \
         and a finished chat is `Status.settled`; pink was minted to escape an amber \
         that no longer shouts.",
        revived.join("\n")
    );
}

// ---------------------------------------------------------------------------
// 2–4. The three scans
// ---------------------------------------------------------------------------

/// A literal or an alpha that should have been a token.
struct Violation {
    file: PathBuf,
    line: usize,
    text: String,
    why: String,
}

#[test]
fn no_text_colour_is_multiplied_by_an_alpha_on_the_phone() {
    let mut violations = Vec::new();
    for file in swift_sources() {
        // Theme.swift is where alphas are MADE (`whiteAlpha`, `wash`, the
        // inline-code fill) — that is the point of it.
        if file.file_name().is_some_and(|n| n == "Theme.swift") {
            continue;
        }
        let body = std::fs::read_to_string(&file).expect("read source");
        let lines: Vec<&str> = body.lines().collect();
        for (ix, line) in lines.iter().enumerate() {
            if !line.contains(".opacity(") || exempt(&lines, ix, "theme-opacity-ok:") {
                continue;
            }
            if let Some(why) = tone_offence(line) {
                violations.push(Violation {
                    file: rel(&file),
                    line: ix + 1,
                    text: line.trim().to_string(),
                    why: why.into(),
                });
            }
        }
    }

    report(
        violations,
        "text colour(s) multiplied by an alpha. Text paints in the four theme \
         tones (text / textMuted / textSubtle / textFaint) and nothing in \
         between; a tone that is genuinely missing becomes a token in \
         Theme.swift — and in crates/ui/src/theme.rs, because the two ends of \
         this system are one system:",
    );
}

#[test]
fn no_literal_radius_or_font_size_outside_the_ios_theme() {
    let files = swift_sources();
    assert!(
        files.len() > 20,
        "expected to walk the whole app, found {} .swift files under {}",
        files.len(),
        ios_root().display()
    );

    let mut violations = Vec::new();
    for file in &files {
        if file.file_name().is_some_and(|n| n == "Theme.swift") {
            continue;
        }
        let body = std::fs::read_to_string(file).expect("read source");
        let lines: Vec<&str> = body.lines().collect();
        for (ix, line) in lines.iter().enumerate() {
            if exempt(&lines, ix, "scale-ok:") {
                continue;
            }
            for why in scale_offences(line) {
                violations.push(Violation {
                    file: rel(file),
                    line: ix + 1,
                    text: line.trim().to_string(),
                    why,
                });
            }
        }
    }

    report(
        violations,
        "literal(s) where a scale token belongs. Radii are Theme.radiusChip (6) \
         / radiusRow (10) / radiusCard (14) and type is textCaption (11) / \
         textDense (12) / textBody (13) / textTitle (15), with textProse \
         reserved for the transcript and textFigure for a number shown as a \
         number. Where a value does not fit, the fix is usually the weight — \
         not a sixteenth number:",
    );
}

/// A dot, a drawn cap or the send button, and nothing else — every surviving
/// `Capsule()` / `Circle()` says which it is.
#[test]
fn full_round_is_a_dot_a_cap_or_the_send_button() {
    let mut violations = Vec::new();
    for file in swift_sources() {
        let body = std::fs::read_to_string(&file).expect("read source");
        let lines: Vec<&str> = body.lines().collect();
        for (ix, line) in lines.iter().enumerate() {
            let round = line.contains("Capsule()") || line.contains("Circle()");
            if !round || line.trim_start().starts_with("//") || exempt(&lines, ix, "round-ok:") {
                continue;
            }
            violations.push(Violation {
                file: rel(&file),
                line: ix + 1,
                text: line.trim().to_string(),
                why: "unmarked".into(),
            });
        }
    }

    report(
        violations,
        "unmarked full-round shape(s). Roundness that is not a status dot, a \
         drawn cap or the send button belongs on the three-step scale; if it \
         really is one of those, say so with a `round-ok: <reason>` comment:",
    );
}

// ---------------------------------------------------------------------------
// The scanners
// ---------------------------------------------------------------------------

/// Local names that hold a text colour without saying `text`.
const TEXT_LOCALS: [&str; 3] = ["subline", "tone", "fg"];

/// Why this line multiplies a text tone, or `None` if its alpha is paint.
fn tone_offence(line: &str) -> Option<&'static str> {
    // A text tone, multiplied. The receiver is whatever identifier (or
    // `method()` call) sits immediately left of `.opacity(`.
    for (at, _) in line.match_indices(".opacity(") {
        if is_text_receiver(receiver(&line[..at])) {
            return Some("text tone");
        }
    }
    // Anything at all, multiplied INSIDE a foreground slot — this is what
    // catches the accents painted as text, where the receiver name gives
    // nothing away (`Theme.accent.opacity(0.85)`).
    if let Some(at) = line.find(".foregroundStyle(") {
        let open = at + ".foregroundStyle(".len() - 1;
        if let Some(close) = matching_paren(line, open)
            && line[open..close].contains(".opacity(")
        {
            return Some("inside foregroundStyle");
        }
    }
    // The AttributedString form: everything right of the `=` is the colour.
    if let Some(at) = line.find("foregroundColor =")
        && line[at..].contains(".opacity(")
    {
        return Some("assigned to foregroundColor");
    }
    None
}

/// Index of the `)` closing the `(` at `open`, or `None` if unbalanced.
fn matching_paren(line: &str, open: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    debug_assert_eq!(bytes[open], b'(');
    let mut depth = 0i32;
    for (ix, b) in bytes.iter().enumerate().skip(open) {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(ix);
                }
            }
            _ => {}
        }
    }
    None
}

/// The receiver expression immediately left of a `.opacity(`, reduced to its
/// trailing name: `Theme.textMuted` -> `textMuted`. `""` when the receiver is a
/// closing paren or index — those are structural, not named.
fn receiver(before: &str) -> &str {
    let b = before.trim_end();
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
    !name.is_empty()
        && (name.contains("text") || name.contains("Text") || TEXT_LOCALS.contains(&name))
}

/// The radius/size literals on this line, if any.
fn scale_offences(line: &str) -> Vec<String> {
    if line.trim_start().starts_with("//") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (call, kind) in [
        ("Theme.sans(", "font size"),
        ("Theme.mono(", "font size"),
        ("Theme.sansUI(", "font size"),
        ("Theme.monoUI(", "font size"),
        ("cornerRadius: ", "radius"),
    ] {
        for (at, _) in line.match_indices(call) {
            let arg = &line[at + call.len()..];
            let end = arg
                .find([',', ')'])
                .unwrap_or_else(|| arg.len().min(arg.trim_end().len()));
            if let Some(n) = as_number(&arg[..end]) {
                out.push(format!("{kind} {n}"));
            }
        }
    }
    out
}

/// The argument as a bare number, or `None` when it is a name or an expression
/// (`Theme.textBody`, `MD.textSize`, `cellSize * 0.25`).
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

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

fn swift_sources() -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_swift(&ios_root(), &mut out);
    out.sort();
    out
}

fn collect_swift(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).expect("read iOS source dir");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_swift(&path, out);
        } else if path.extension().is_some_and(|e| e == "swift") {
            out.push(path);
        }
    }
}

fn rel(file: &Path) -> PathBuf {
    file.strip_prefix(ios_root()).unwrap_or(file).to_path_buf()
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

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn literals_are_offences_and_tokens_are_not() {
        assert_eq!(scale_offences(".font(Theme.sans(13))"), ["font size 13"]);
        assert_eq!(
            scale_offences("RoundedRectangle(cornerRadius: 12)"),
            ["radius 12"]
        );
        assert_eq!(
            scale_offences(".font(Theme.mono(10.5, weight: .medium))"),
            ["font size 10.5"]
        );
        // Tokens, other constants and expressions all pass — the rule is "name
        // the number", not "never write a size".
        assert!(scale_offences(".font(Theme.sans(Theme.textBody))").is_empty());
        assert!(scale_offences(".font(Theme.mono(MD.codeTextSize))").is_empty());
        assert!(scale_offences("RoundedRectangle(cornerRadius: Theme.radiusRow)").is_empty());
        assert!(scale_offences("RoundedRectangle(cornerRadius: cellSize * 0.25)").is_empty());
        // Symbol and frame sizes are icon geometry, not type.
        assert!(scale_offences(".font(.system(size: 12)).frame(width: 22)").is_empty());
        // A commented-out line is not code.
        assert!(scale_offences("// .font(Theme.sans(13))").is_empty());
    }

    #[test]
    fn tone_offences_are_text_and_non_offences_are_paint() {
        assert!(tone_offence(".foregroundStyle(Theme.textMuted.opacity(0.6))").is_some());
        assert!(tone_offence("private var subline: Color { Theme.text.opacity(0.5) }").is_some());
        assert!(tone_offence(".foregroundStyle(Theme.accent.opacity(0.85))").is_some());
        assert!(tone_offence("piece.foregroundColor = base.opacity(alpha)").is_some());
        // Non-text paint keeps its alphas: fills, hairlines, whole-element fades.
        assert!(tone_offence(".background(Theme.accent.opacity(0.16), in: shape)").is_none());
        assert!(tone_offence(".strokeBorder(Theme.danger.opacity(0.16), lineWidth: 1)").is_none());
        assert!(tone_offence(".fill(Theme.accent.opacity(0.22))").is_none());
        assert!(tone_offence("oklch(0.7, 0.18, 293).opacity(0.12)").is_none());
        // A whole-element fade AFTER the foreground slot has closed is a fade,
        // not a twelfth grey — this is why the parens are matched.
        assert!(tone_offence(".foregroundStyle(Theme.text).opacity(selected ? 1 : 0)").is_none());
        // A text colour landing in a NON-text slot is still an offence.
        assert!(tone_offence(".strokeBorder(Theme.text.opacity(0.3))").is_some());
    }

    #[test]
    fn the_escape_hatch_needs_a_marker() {
        let lines = ["// scale-ok: a drawn cap", "cornerRadius: 3", "x"];
        assert!(exempt(&lines, 1, "scale-ok:"));
        assert!(!exempt(&lines, 2, "scale-ok:"));
        // The marker may sit anywhere in the comment block above.
        let wrapped = ["// scale-ok: because", "// of this", "cornerRadius: 3"];
        assert!(exempt(&wrapped, 2, "scale-ok:"));
        // But the block has to be contiguous with the offending line.
        let gapped = ["// scale-ok: because", "", "cornerRadius: 3"];
        assert!(!exempt(&gapped, 2, "scale-ok:"));
        // The hatches are separate: a round-ok does not license a literal.
        assert!(!exempt(
            &["// round-ok: dot", "cornerRadius: 3"],
            1,
            "scale-ok:"
        ));
    }
}
