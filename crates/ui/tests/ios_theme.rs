//! The phone reads from the same system (gh#181).
//!
//! `apps/ios/Comet/Theme/Theme.swift` is a second declaration of the design
//! system this crate owns — it has to be, because no Rust runs on that device.
//! Two declarations of one system is how a phone comes to disagree with a
//! laptop about what colour a working agent is, which is exactly what happened:
//! `boardStateColor` said amber and `ChatIndicator.dotColor` said pink, one
//! screen apart, months after the desktop had settled that argument.
//!
//! So this file reads the Swift as text and holds it to the Rust. Six checks,
//! mirroring `text_tones.rs` and `scale.rs` for the other viewport:
//!
//! 1. **Parity, in both variants.** The status ramp, the three radii, the type
//!    scale and the surfaces/text greys are the same NUMBERS the desktop paints
//!    (`ios_theme_declares_the_same_system`). Since gh#257 the phone has a light
//!    variant, so the paint checks read the `dark:` and `light:` arguments of
//!    each `themed(…)` token and hold them to [`Theme::dark`] and
//!    [`Theme::light`] respectively — a light theme that drifts is the same
//!    class of bug as a dark one that does, and it is the easier one to ship
//!    unnoticed on a device nobody uses in daylight.
//! 2. **Every paint token declares both variants**
//!    (`every_paint_token_answers_for_both_variants`), hatch `one-tone-ok:`.
//!    A token that resolves to one colour in both schemes is the crack a
//!    scattered `if colorScheme == .light` grows out of.
//! 3. **Nothing forces a scheme** (`nothing_in_the_app_forces_a_colour_scheme`).
//!    Before gh#257 four call sites pinned `.dark` — the scene and three
//!    sheets — which is why the light design in the reference could not be
//!    reached at all.
//! 4. **No text tone is multiplied by an alpha** (gh#172), hatch
//!    `theme-opacity-ok:`.
//! 5. **No literal radius or font size outside `Theme.swift`** (gh#174), hatch
//!    `scale-ok:`.
//! 6. **`Capsule()` / `Circle()` is a dot, a drawn cap or the send button**
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
use comet_ui::theme::{Theme, cool, grey, ink, neutral, oklch, wash, white_alpha};
use gpui::Hsla;
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

/// The value of `static let <name> = …` / `static let <name>: T = …`, with
/// trailing `//` comments stripped and continuation lines joined — a `themed()`
/// pair with a note against each half spans two lines, and a declaration that
/// stops at the newline reads as an unbalanced expression.
fn declaration(src: &str, name: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    for (ix, line) in lines.iter().enumerate() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("static let ") else {
            continue;
        };
        let Some((lhs, rhs)) = rest.split_once('=') else {
            continue;
        };
        // `name` or `name: CGFloat`
        if lhs.split(':').next().unwrap_or("").trim() != name {
            continue;
        }
        let mut out = code(rhs);
        let mut at = ix;
        while unclosed(&out) && at + 1 < lines.len() {
            at += 1;
            out.push(' ');
            out.push_str(&code(lines[at]));
        }
        return out.trim().to_string();
    }
    panic!("Theme.swift declares no `{name}` — the phone dropped a token the desktop still has");
}

/// One source line's code half — everything left of a trailing `//`.
fn code(line: &str) -> String {
    line.split("//").next().unwrap_or(line).trim().to_string()
}

/// Whether an expression is still waiting on a `)` — the only reason to read
/// the next line. A declaration with no parens at all (a number) is finished.
fn unclosed(expr: &str) -> bool {
    expr.chars().filter(|c| *c == '(').count() > expr.chars().filter(|c| *c == ')').count()
}

/// A bare number declaration (`static let radiusChip: CGFloat = 6`).
fn number(src: &str, name: &str) -> f32 {
    let raw = declaration(src, name);
    raw.parse().unwrap_or_else(|_| {
        panic!("Theme.swift's `{name}` is `{raw}`, which is not a plain number")
    })
}

// ---------------------------------------------------------------------------
// Reading a Swift paint token as paint (gh#257)
// ---------------------------------------------------------------------------

/// The `dark:` and `light:` halves of a paint token, as source expressions.
///
/// Every colour in `Theme.swift` is `themed(dark: …, light: …)` or one of the
/// two shorthands that expand to it — `ramp(hue)` (the same hue at the two
/// status anchors) and `whiteAlpha(a)` (white over dark, cool ink over light).
/// Anything else is a token that paints one colour in both schemes, which
/// `every_paint_token_answers_for_both_variants` is the test for.
fn variants(src: &str, name: &str) -> Option<(String, String)> {
    split_variants(&declaration(src, name))
}

fn split_variants(raw: &str) -> Option<(String, String)> {
    if let Some(hue) = call_body(raw, "ramp") {
        return Some((
            format!("oklch(statusL, statusC, {hue})"),
            format!("oklch(statusLLight, statusC, {hue})"),
        ));
    }
    if let Some(alpha) = call_body(raw, "whiteAlpha") {
        return Some((
            format!("Color.white.opacity({alpha})"),
            format!("ink({alpha} * lightInkScale)"),
        ));
    }
    let body = call_body(raw, "themed")?;
    let dark = body.strip_prefix("dark:")?;
    let cut = top_level_comma(dark)?;
    let light = dark[cut + 1..].trim().strip_prefix("light:")?.trim();
    Some((dark[..cut].trim().to_string(), light.to_string()))
}

/// The arguments of `name(…)`, when the whole expression IS that call.
fn call_body(expr: &str, name: &str) -> Option<String> {
    let inner = expr
        .trim()
        .strip_prefix(name)?
        .strip_prefix('(')?
        .strip_suffix(')')?;
    // `oklch(…).opacity(…)` also starts with `oklch(` and ends with `)`; what
    // tells the two apart is whether the parens in between still nest.
    well_nested(inner).then(|| inner.trim().to_string())
}

fn well_nested(expr: &str) -> bool {
    let mut depth = 0i32;
    for ch in expr.chars() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// The index of the comma separating a two-argument call's arguments.
fn top_level_comma(args: &str) -> Option<usize> {
    let mut depth = 0i32;
    for (ix, ch) in args.char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => return Some(ix),
            _ => {}
        }
    }
    None
}

/// One Swift paint expression, evaluated with this crate's own primitives — so
/// what is compared is the COLOUR that lands on the phone's screen, not the
/// spelling of it. The four constructors are the ones `Theme.swift` declares;
/// bare identifiers resolve against the numbers it declares above them.
fn paint(src: &str, expr: &str) -> Hsla {
    let expr = expr.trim();
    // A trailing `.opacity(a)` applies to whatever it is hung off — but only
    // when it is the OUTERMOST call, not an alpha inside an argument.
    if let Some(at) = expr.rfind(".opacity(")
        && matching_paren(expr, at + ".opacity(".len() - 1) == Some(expr.len() - 1)
    {
        let base = &expr[..at];
        let alpha = &expr[at + ".opacity(".len()..expr.len() - 1];
        if base == "Color.white" {
            return white_alpha(scalar(src, alpha));
        }
        return paint(src, base).opacity(scalar(src, alpha));
    }
    let args = |name: &str| -> Option<Vec<f32>> {
        Some(
            split_args(&call_body(expr, name)?)
                .iter()
                .map(|a| scalar(src, a))
                .collect(),
        )
    };
    if let Some(a) = args("grey") {
        return grey(a[0] as u8);
    }
    if let Some(a) = args("neutral") {
        return neutral(a[0]);
    }
    if let Some(a) = args("cool") {
        return cool(a[0], a[1]);
    }
    if let Some(a) = args("ink") {
        return ink(a[0]);
    }
    if let Some(a) = args("wash") {
        return wash(a[0]);
    }
    if let Some(a) = args("oklch") {
        return oklch(a[0], a[1], a[2]);
    }
    panic!(
        "Theme.swift paints `{expr}`, which this test cannot evaluate — teach it the constructor or use one it knows"
    );
}

fn split_args(body: &str) -> Vec<String> {
    match top_level_comma(body) {
        Some(cut) => {
            let mut out = vec![body[..cut].trim().to_string()];
            out.extend(split_args(body[cut + 1..].trim()));
            out
        }
        None => vec![body.trim().to_string()],
    }
}

/// A number, a `Theme` constant by name, or a product of the two.
fn scalar(src: &str, expr: &str) -> f32 {
    let expr = expr.trim();
    if let Some((a, b)) = expr.split_once('*') {
        return scalar(src, a) * scalar(src, b);
    }
    if let Some(hex) = expr.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16).expect("hex literal") as f32;
    }
    expr.parse().unwrap_or_else(|_| number(src, expr))
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
    check(
        "statusLLight",
        number(&src, "statusLLight"),
        status::L_LIGHT,
    );
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

    // The paint, in BOTH variants (gh#257). Compared as COLOUR, not as
    // lightness: both languages run the same oklch→sRGB conversion, so the
    // assertion is that what lands on the phone's screen is what lands on the
    // desktop's — in the scheme the reader is actually in.
    //
    // The pairs are (phone token, desktop's dark, desktop's light). Where the
    // phone's job for a token differs from the desktop's, the LIGHT column says
    // so by naming a different desktop token — see the notes.
    let dark = Theme::dark();
    let light = Theme::light();
    for (name, want_dark, want_light) in [
        // Four text greys (gh#172), four contrast steps, twice.
        ("text", dark.text, light.text),
        ("textMuted", dark.text_muted, light.text_muted),
        ("textSubtle", dark.text_subtle, light.text_subtle),
        ("textFaint", dark.text_faint, light.text_faint),
        // Surfaces. `surface` is the shell on a desktop and the whole screen on
        // a phone, which is why its light half is the desktop's PAPER: a full
        // screen of the shell's ground reads as a page that failed to load.
        ("bg", dark.bg, light.bg),
        ("surface", dark.surface, light.bg),
        // `surfaceRaised` is an inline tile here (a meter track, a tool card),
        // so light steps DOWN into the paper — the desktop's `bubble`, its
        // token for a well rather than a float.
        ("surfaceRaised", dark.surface_raised, light.bubble),
        // A sheet IS the one surface with something floating on it, so its page
        // is the ground and its cards are the raised white object.
        ("sheetPanel", grey(0x14), light.surface),
        ("card", white_alpha(0.045), light.surface_raised),
        ("elementHover", dark.element_hover, light.element_hover),
        ("border", dark.border, light.border),
        ("borderStrong", dark.border_strong, light.border_strong),
        // The status ramp, at its two anchors.
        ("accent", dark.accent, light.accent),
        ("accentStrong", dark.accent_strong, light.accent_strong),
        ("danger", dark.danger, light.danger),
        ("warning", dark.warning, light.warning),
        ("settled", dark.settled, light.settled),
        // The foregrounds that sit ON a fill of their own colour, and so invert
        // rather than following the ramp.
        ("dangerText", dark.danger_text(), light.danger_text()),
        ("warningText", dark.warning_text(), light.warning_text()),
        ("settledText", dark.settled_text(), light.settled_text()),
    ] {
        let Some((dark_expr, light_expr)) = variants(&src, name) else {
            wrong.push(format!(
                "  {name}: not a `themed(dark:light:)` token — it paints one colour in both schemes"
            ));
            continue;
        };
        for (scheme, expr, want) in [
            ("dark", dark_expr, want_dark),
            ("light", light_expr, want_light),
        ] {
            let got = paint(&src, &expr);
            if got != want {
                wrong.push(format!(
                    "  {name} ({scheme}): phone {expr} = {got:?}, desktop {want:?}"
                ));
            }
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

/// Every colour token answers for both schemes (gh#257).
///
/// The whole design of the phone's light mode is that a token knows its two
/// values and no view ever asks which scheme it is in. The failure mode this
/// guards is the cheap one: somebody adds a token, gives it the dark value that
/// was in front of them, and the light screens grow a `colorScheme ==` check at
/// the call site to work round it. One `themed(…)` here costs less than that
/// check, and unlike the check it can be held to the desktop above.
///
/// A colour that genuinely IS one colour says so with a `one-tone-ok:` marker.
#[test]
fn every_paint_token_answers_for_both_variants() {
    let src = theme_swift();
    let lines: Vec<&str> = src.lines().collect();
    let mut violations = Vec::new();
    for (ix, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("static let ") else {
            continue;
        };
        let Some((lhs, _)) = rest.split_once('=') else {
            continue;
        };
        let name = lhs.split(':').next().unwrap_or("").trim();
        // Only the paint: a token whose value is a number is a layout constant,
        // and layout does not change with the light (see the file header).
        let raw = declaration(&src, name);
        let is_paint = [
            "themed(",
            "ramp(",
            "whiteAlpha(",
            "oklch(",
            "neutral(",
            "cool(",
            "grey(",
            "ink(",
            "wash(",
            "Color(",
        ]
        .iter()
        .any(|c| raw.starts_with(c));
        if !is_paint || exempt(&lines, ix, "one-tone-ok:") {
            continue;
        }
        if split_variants(&raw).is_none() {
            violations.push(Violation {
                file: PathBuf::from("Theme/Theme.swift"),
                line: ix + 1,
                text: format!("static let {name} = {raw}"),
                why: "one tone".into(),
            });
        }
    }

    report(
        violations,
        "paint token(s) that paint the same colour in light and dark. Declare \
         both — `themed(dark: …, light: …)`, or `ramp(hue)` for a status hue — \
         with the light half taken from `Theme::light()` in \
         crates/ui/src/theme.rs rather than invented here. If the colour really \
         is one colour in both (a brand mark is), say so with a \
         `one-tone-ok: <reason>` comment:",
    );
}

/// Nothing pins a scheme (gh#257).
///
/// Four call sites used to: the scene in `CometApp.swift` and three sheets,
/// each `.preferredColorScheme(.dark)`. That is why the light design in the
/// reference could not be reached — and why the sheets needed their own line in
/// the first place is worth remembering, because it is the trap a future
/// "just set it at the root" would fall into: a sheet is its own presentation
/// host. `cometAppearance()` is the one modifier that answers for all four, and
/// it answers with a preference.
#[test]
fn nothing_in_the_app_forces_a_colour_scheme() {
    let mut violations = Vec::new();
    for file in swift_sources() {
        // Appearance.swift is where the preference is TURNED INTO a scheme —
        // that is the point of it.
        if file.file_name().is_some_and(|n| n == "Appearance.swift") {
            continue;
        }
        let body = std::fs::read_to_string(&file).expect("read source");
        for (ix, line) in body.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            if line.contains(".preferredColorScheme(.dark)")
                || line.contains(".preferredColorScheme(.light)")
            {
                violations.push(Violation {
                    file: rel(&file),
                    line: ix + 1,
                    text: line.trim().to_string(),
                    why: "forced".into(),
                });
            }
        }
    }

    report(
        violations,
        "call site(s) pinning a colour scheme. The scheme is a preference — \
         `.cometAppearance()`, which reads it and defaults to the system's \
         answer. A constant here is how the phone spent gh#181 through gh#254 \
         unable to show the light design it had tokens for:",
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

    /// The reader that makes the two-variant parity check possible: it has to
    /// tell a pair from a single tone, survive a note against each half, and
    /// not mistake a trailing `.opacity()` for the call it hangs off.
    #[test]
    fn a_token_is_read_as_its_two_halves() {
        assert_eq!(
            split_variants("themed(dark: grey(6), light: cool(0.986, 0.003))"),
            Some(("grey(6)".into(), "cool(0.986, 0.003)".into()))
        );
        // The shorthands expand to the same pair.
        assert_eq!(
            split_variants("ramp(hueReview)"),
            Some((
                "oklch(statusL, statusC, hueReview)".into(),
                "oklch(statusLLight, statusC, hueReview)".into()
            ))
        );
        assert_eq!(
            split_variants("whiteAlpha(0.08)"),
            Some((
                "Color.white.opacity(0.08)".into(),
                "ink(0.08 * lightInkScale)".into()
            ))
        );
        // One tone in both schemes — the thing the scan is looking for.
        assert_eq!(split_variants("oklch(0.62, 0.19, 265)"), None);
        assert_eq!(split_variants("Color(red: 1, green: 0, blue: 0)"), None);
        // An alpha hung off the whole expression is not a second argument.
        assert_eq!(
            split_variants("themed(dark: oklch(0.7, 0.18, 293).opacity(0.12), light: ink(0.2))"),
            Some((
                "oklch(0.7, 0.18, 293).opacity(0.12)".into(),
                "ink(0.2)".into()
            ))
        );
    }

    #[test]
    fn a_declaration_spans_its_continuation_lines() {
        let src = "\
enum Theme {
    static let statusL: Double = 0.74
    static let dangerText = themed(dark: oklch(0.8, 0.1, 19.5),   // red-300
                                   light: oklch(0.5, 0.2, 27.5))  // red-700
    static let nestGutter: CGFloat = radiusCard - radiusRow
}";
        assert_eq!(declaration(src, "statusL"), "0.74");
        assert_eq!(declaration(src, "nestGutter"), "radiusCard - radiusRow");
        assert_eq!(
            declaration(src, "dangerText"),
            "themed(dark: oklch(0.8, 0.1, 19.5), light: oklch(0.5, 0.2, 27.5))"
        );
    }

    /// Expressions are compared as PAINT, so the evaluator has to agree with
    /// this crate's own primitives — including the alpha and the scale.
    #[test]
    fn paint_evaluates_to_this_crates_colours() {
        let src = "static let lightInkScale: Double = 1.625\n";
        assert_eq!(paint(src, "grey(0x14)"), grey(0x14));
        assert_eq!(paint(src, "neutral(0.938)"), neutral(0.938));
        assert_eq!(paint(src, "cool(0.986, 0.003)"), cool(0.986, 0.003));
        assert_eq!(paint(src, "Color.white.opacity(0.08)"), white_alpha(0.08));
        assert_eq!(
            paint(src, "oklch(0.702, 0.183, 293.541).opacity(0.12)"),
            oklch(0.702, 0.183, 293.541).opacity(0.12)
        );
        // A named constant, and arithmetic on one.
        assert_eq!(paint(src, "ink(0.08 * lightInkScale)"), ink(0.08 * 1.625));
    }

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
