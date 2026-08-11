//! The phone reads from the same system (gh#181).
//!
//! `apps/ios/Comet/Theme/Theme.swift` is a second declaration of the design
//! system this crate owns — it has to be, because no Rust runs on that device.
//! Two declarations of one system is how a phone comes to disagree with a
//! laptop about what colour a working agent is, which is exactly what happened:
//! `boardStateColor` said amber and `ChatIndicator.dotColor` said pink, one
//! screen apart, months after the desktop had settled that argument.
//!
//! So this file reads the Swift as text and holds it to two things: the table
//! in `docs/design/tokens.md`, and this crate's own [`Theme`]. Ten checks,
//! mirroring `text_tones.rs` and `scale.rs` for the other viewport:
//!
//! 0. **The hand-ported table is the table** (gh#279,
//!    `ios_canvas_tokens_are_the_values_tokens_md_declares`). The phone's
//!    third declaration of the palette lives in `Theme/DesignCanvas.swift`
//!    under the canvas's own variable names, and this reads `tokens.md`'s
//!    cells against it — both variants, every row, in both directions (a
//!    variable the doc declares and Swift has not; a variable Swift declares
//!    and the doc does not). It is the check the other parity test cannot
//!    make: that one holds the phone to the DESKTOP, which is a second port of
//!    the same table and could in principle drift with it.
//! 1. **Parity, in both variants.** Shared dark paint and the radii/type scale
//!    stay equal to [`Theme::dark`], and since gh#279 the surfaces stay equal
//!    to [`Theme::light`] too — the two viewports are two spendings of one
//!    palette. Two exceptions are named where they are: the light half of
//!    `surface`, and the light status ramp's anchors
//!    (`ios_theme_declares_the_shared_system_and_ios_reference`).
//! 2. **Every paint token declares both variants**
//!    (`every_paint_token_answers_for_both_variants`), hatch `one-tone-ok:`.
//!    A token that resolves to one colour in both schemes is the crack a
//!    scattered `if colorScheme == .light` grows out of.
//! 3. **No Swift view pins a scheme**
//!    (`no_swift_view_pins_a_colour_scheme`). Before gh#257 four call sites
//!    pinned `.dark` — the scene and three sheets. The separate, ring-fenced
//!    `UIUserInterfaceStyle = Dark` bundle key is still present and is checked
//!    explicitly rather than hidden by this claim.
//! 4. **No text tone is multiplied by an alpha** (gh#172), hatch
//!    `theme-opacity-ok:`.
//! 5. **No literal radius or font size outside `Theme.swift`** (gh#174), hatch
//!    `scale-ok:`.
//! 6. **`Capsule()` / `Circle()` is a dot, a drawn cap or the send button**
//!    (gh#174), hatch `round-ok:`.
//! 7. **The ring-fenced bundle override is handled honestly**
//!    (`bundle_style_dependency_is_explicit`).
//! 8. **The activation observer has a bounded lifetime**
//!    (`window_scheme_observer_has_a_bounded_lifetime`).
//! 9. **No view outside `Theme/` names a colour** (gh#279,
//!    `no_view_outside_the_theme_names_a_colour`), hatch `paint-ok:`. The rule
//!    the token port is for: forty call sites reached for `whiteAlpha(a)` with
//!    twelve different alphas, which is gh#172's "a dozen unnamed greys" one
//!    level down.
//!
//! Why here and not in the phone's own harness: `SpecRunner` needs a simulator,
//! so it runs when somebody remembers to run it and never in CI (see
//! `apps/ios/README.md`). These rules are pure text scans, so putting them in
//! `cargo test` closes that gap for this class of drift — a PR that lets the
//! phone slip off the system fails on the same runner that builds the desktop.
//! What no scan can check is what the paint LOOKS like; that is
//! `scripts/ios-theme-shots.sh` and the claims in `docs/design/ios.md`.
//!
//! It lives in `comet-ui` rather than `comet-proto` because it needs both ends:
//! the status ramp from `comet_proto::view::status`, and `Theme::RADIUS_*` /
//! `Theme::TEXT_*` / shared parts of `Theme::dark()` from here.

use comet_proto::view::status;
use comet_ui::theme::{Theme, grey, hex, ink, neutral, oklch, wash, white_alpha};
use gpui::{Hsla, Rgba};
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

/// The phone's transcription of the canvas table (gh#279).
fn canvas_swift() -> String {
    std::fs::read_to_string(ios_root().join("Theme/DesignCanvas.swift"))
        .expect("read DesignCanvas.swift")
}

/// The table itself — the doc all three declarations are reconciled against.
fn tokens_md() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/design/tokens.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn appearance_swift() -> String {
    std::fs::read_to_string(ios_root().join("Theme/Appearance.swift"))
        .expect("read Appearance.swift")
}

fn info_plist() -> String {
    std::fs::read_to_string(ios_root().join("Info.plist")).expect("read Info.plist")
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
///
/// A declaration that forwards to the canvas (`= DesignCanvas.statusL`) is
/// followed there, so the status anchors are read where they are transcribed
/// rather than being restated for the test's benefit.
fn number(src: &str, name: &str) -> f32 {
    let raw = declaration(src, name);
    if let Some(forwarded) = raw.strip_prefix("DesignCanvas.") {
        return number(&canvas_swift(), forwarded);
    }
    raw.parse().unwrap_or_else(|_| {
        panic!("Theme.swift's `{name}` is `{raw}`, which is not a plain number")
    })
}

// ---------------------------------------------------------------------------
// The canvas table, as Swift (gh#279)
// ---------------------------------------------------------------------------

/// The `dark:` and `light:` expressions of a `DesignCanvas` variable —
/// `CanvasToken(dark: …, light: …)` or `CanvasShadow(dark: …, light: …, …)`.
fn canvas_halves(src: &str, name: &str) -> (String, String) {
    let raw = declaration(src, name);
    // The four status variables are declared through `ramp(hue)` — one hue at
    // the two anchors — which is the canvas's own `oklch(L C H)` written once
    // instead of eight times.
    if let Some(hue) = call_body(&raw, "ramp") {
        return (
            format!("oklch(statusL, statusC, {hue})"),
            format!("oklch(statusLLight, statusCLight, {hue})"),
        );
    }
    let body = call_body(&raw, "CanvasToken")
        .or_else(|| call_body(&raw, "CanvasShadow"))
        .unwrap_or_else(|| {
            panic!("DesignCanvas.{name} is `{raw}`, not a CanvasToken/CanvasShadow pair")
        });
    let args = split_args(&body);
    let arg = |label: &str| -> String {
        args.iter()
            .find_map(|a| a.trim().strip_prefix(label).map(|v| v.trim().to_string()))
            .unwrap_or_else(|| panic!("DesignCanvas.{name} has no `{label}` argument"))
    };
    (arg("dark:"), arg("light:"))
}

/// One half of a canvas variable, as paint.
fn canvas_paint(name: &str, light: bool) -> Hsla {
    let src = canvas_swift();
    let (d, l) = canvas_halves(&src, name);
    paint(&src, if light { &l } else { &d })
}

// ---------------------------------------------------------------------------
// Reading a Swift paint token as paint (gh#257)
// ---------------------------------------------------------------------------

/// The `dark:` and `light:` halves of a paint token, as source expressions.
///
/// Every colour in `Theme.swift` is `themed(dark: …, light: …)` or one of the
/// two shorthands that expand to it — `ramp(hue)` (the same hue at the two
/// status anchors) and `whiteAlpha(a)` (white over dark, black over light).
/// Anything else is a token that paints one colour in both schemes, which
/// `every_paint_token_answers_for_both_variants` is the test for.
fn variants(src: &str, name: &str) -> Option<(String, String)> {
    split_variants(&declaration(src, name))
}

fn split_variants(raw: &str) -> Option<(String, String)> {
    // `DesignCanvas.raised.color` — a whole canvas variable spent under a name
    // for its job. The two halves are named so `paint` can resolve each in the
    // file that transcribes it (gh#279).
    if let Some(name) = raw
        .trim()
        .strip_prefix("DesignCanvas.")
        .and_then(|r| r.strip_suffix(".color"))
        && !name.contains('(')
    {
        return Some((
            format!("DesignCanvas.{name}.dark"),
            format!("DesignCanvas.{name}.light"),
        ));
    }
    if let Some(hue) = call_body(raw, "ramp") {
        return Some((
            format!("oklch(statusL, statusC, {hue})"),
            format!("oklch(statusLLight, statusCLight, {hue})"),
        ));
    }
    if let Some(alpha) = call_body(raw, "whiteAlpha") {
        return Some((
            format!("Color.white.opacity({alpha})"),
            format!("Color.black.opacity({alpha} * lightInkScale)"),
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
    // A reference into the canvas table resolves in the file that transcribes
    // it, not in the one that spends it.
    if let Some(rest) = expr.strip_prefix("DesignCanvas.") {
        if let Some(name) = rest.strip_suffix(".dark") {
            return canvas_paint(name, false);
        }
        if let Some(name) = rest.strip_suffix(".light") {
            return canvas_paint(name, true);
        }
        panic!("`{expr}` names no half of a canvas variable — say `.dark` or `.light`");
    }
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
        if base == "Color.black" {
            return black_alpha(scalar(src, alpha));
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
    if let Some(a) = args("hex") {
        return hex(a[0] as u32);
    }
    if let Some(a) = args("neutral") {
        return neutral(a[0]);
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

fn black_alpha(alpha: f32) -> Hsla {
    Hsla::from(Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: alpha,
    })
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
fn ios_theme_declares_the_shared_system_and_ios_reference() {
    let src = theme_swift();
    let mut wrong = Vec::new();
    let mut check = |what: &str, phone: f32, desktop: f32| {
        if (phone - desktop).abs() > f32::EPSILON {
            wrong.push(format!("  {what}: phone {phone}, desktop {desktop}"));
        }
    };

    // The status ramp — one lightness, one chroma, four hues (gh#173).
    check("statusL", number(&src, "statusL"), status::L);
    check("statusLLight", number(&src, "statusLLight"), 0.52);
    check("statusC", number(&src, "statusC"), status::C);
    check("statusCLight", number(&src, "statusCLight"), 0.16);
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

    // Paint is compared as COLOUR, not spelling.
    //
    // Since gh#279 the phone spends the SAME canvas table the desktop does, so
    // almost every surface token below is written against a `Theme` field in
    // both variants rather than against a restated literal: the two viewports
    // are two spendings of one palette, and that is the claim worth failing
    // on. Four of these used to be phone-only dark values and were derived
    // rather than transcribed — `--raised` at `neutral(0.235)` painted #202020
    // against the canvas's #161616 — which is gh#274's bug on this side.
    //
    // The exceptions are named where they are: the light half of `surface`
    // (the phone draws one page tone, `--card`, where the desktop has a shell
    // beside a panel) and the light status ramp (the canvas anchors it at
    // L 0.52 / C 0.16, where the desktop's light theme wants 0.55 / 0.14).
    let dark = Theme::dark();
    let light = Theme::light();
    for (name, want_dark, want_light) in [
        // --text / --muted / --subtle / --faint
        ("text", dark.text, light.text),
        ("textMuted", dark.text_muted, light.text_muted),
        ("textSubtle", dark.text_subtle, light.text_subtle),
        ("textFaint", dark.text_faint, light.text_faint),
        // --card / --shell / --raised / --sel / --selcard / --hover / --chip
        ("bg", dark.bg, light.bg),
        // ...and the one token whose halves come from two variables.
        ("surface", dark.surface, light.bg),
        ("surfaceRaised", dark.surface_raised, light.surface_raised),
        ("sheetPanel", dark.bg, light.bg),
        ("card", dark.surface_raised, light.surface_raised),
        ("rowSelected", dark.row_selected, light.row_selected),
        (
            "elementActive",
            dark.row_selected_card,
            light.row_selected_card,
        ),
        ("elementHover", dark.element_hover, light.element_hover),
        ("chip", dark.chip, light.chip),
        // --line / --line2 / --sellift
        ("border", dark.border, light.border),
        ("borderStrong", dark.border_strong, light.border_strong),
        ("separator", dark.border, light.border),
        ("rowEdge", dark.row_edge, light.row_edge),
        // --review / --blocked / --working / --settled
        ("accent", dark.accent, oklch(0.52, 0.16, status::REVIEW)),
        ("danger", dark.danger, oklch(0.52, 0.16, status::BLOCKED)),
        ("warning", dark.warning, oklch(0.52, 0.16, status::WORKING)),
        ("settled", dark.settled, oklch(0.52, 0.16, status::SETTLED)),
        // Foregrounds on status washes use that same reference ramp in light.
        (
            "dangerText",
            dark.danger_text(),
            oklch(0.52, 0.16, status::BLOCKED),
        ),
        (
            "warningText",
            dark.warning_text(),
            oklch(0.52, 0.16, status::WORKING),
        ),
        (
            "settledText",
            dark.settled_text(),
            oklch(0.52, 0.16, status::SETTLED),
        ),
        // --claude
        ("claudeBrand", dark.claude, light.claude),
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
                    "  {name} ({scheme}): phone {expr} = {got:?}, expected {want:?}"
                ));
            }
        }
    }

    assert!(
        wrong.is_empty(),
        "{} value(s) where the phone has drifted off its shared dark/scales or \
         supplied iOS references. Shared dark and layout numbers live in \
         crates/ui/src/theme.rs and comet_proto::view::status; the phone-only \
         dark values and light paint values are restated explicitly above:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

// ---------------------------------------------------------------------------
// The phone against the table it was ported from (gh#279)
// ---------------------------------------------------------------------------

/// One colour literal lifted out of a `tokens.md` cell.
fn doc_colour(value: &str) -> Option<Hsla> {
    let v = value.trim();
    if let Some(h) = v.strip_prefix('#') {
        let n = u32::from_str_radix(h, 16).ok()?;
        return Some(hex(n));
    }
    if let Some(args) = v.strip_prefix("rgba(").and_then(|r| r.strip_suffix(')')) {
        let n: Vec<f32> = args
            .split(',')
            .map(|p| p.trim().parse().unwrap_or(f32::NAN))
            .collect();
        if n.len() != 4 || n.iter().any(|x| x.is_nan()) {
            return None;
        }
        return Some(Hsla::from(Rgba {
            r: n[0] / 255.0,
            g: n[1] / 255.0,
            b: n[2] / 255.0,
            a: n[3],
        }));
    }
    if let Some(args) = v.strip_prefix("oklch(").and_then(|r| r.strip_suffix(')')) {
        let n: Vec<f32> = args
            .split_whitespace()
            .filter_map(|p| p.parse().ok())
            .collect();
        return (n.len() == 3).then(|| oklch(n[0], n[1], n[2]));
    }
    None
}

/// Every colour literal in a cell, left to right — a shadow value carries one
/// or two, and which is which is decided per token below.
fn doc_colours(value: &str) -> Vec<Hsla> {
    let mut out = Vec::new();
    let mut rest = value;
    while !rest.is_empty() {
        let next = ["#", "rgba(", "oklch("]
            .iter()
            .filter_map(|p| rest.find(p).map(|at| (at, *p)))
            .min();
        let Some((at, prefix)) = next else { break };
        let tail = &rest[at..];
        let end = if prefix == "#" {
            tail[1..]
                .find(|c: char| !c.is_ascii_hexdigit())
                .map_or(tail.len(), |i| i + 1)
        } else {
            tail.find(')').map_or(tail.len(), |i| i + 1)
        };
        if let Some(c) = doc_colour(&tail[..end]) {
            out.push(c);
        }
        rest = &tail[end..];
    }
    out
}

/// The canvas variables `docs/design/tokens.md` declares, as
/// `(--name, dark cell, light cell)`.
fn tokens_md_table() -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for line in tokens_md().lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<String> = line
            .trim_matches('|')
            .split('|')
            .map(|c| c.trim().trim_matches('`').trim().to_string())
            .collect();
        // A variable, not the `| --- |` rule under a header row.
        if cells.len() < 3 || !cells[0].starts_with("--") || cells[0].chars().all(|c| c == '-') {
            continue;
        }
        // A cell holding two comma-separated values keeps its backticks
        // inside; strip only the outer pair, which `trim_matches` did.
        out.push((cells[0].clone(), cells[1].clone(), cells[2].clone()));
    }
    assert!(
        out.len() >= 18,
        "tokens.md parsed as {} rows — the table's shape changed and this reader did not",
        out.len()
    );
    out
}

/// **The acceptance check for gh#279**: every value the phone hand-ported is
/// the value `docs/design/tokens.md` declares, in both variants.
///
/// The phone cannot reference `crates/ui/src/theme.rs` — no Rust runs on the
/// device — so the palette is typed in twice whatever anyone does about it.
/// What makes a hand-port survivable is that the ported values are NAMED, in
/// the canvas's own names, in one file: `DesignCanvas.swift` transcribes
/// `--raised` as `raised`, and this test reads the doc's cell and the Swift
/// expression and compares the COLOURS. A typo, a dropped digit or a
/// re-derived grey fails here rather than in a screenshot three surfaces later.
///
/// This is the check `ios_theme_declares_the_shared_system_and_ios_reference`
/// cannot make: that one holds the phone to the DESKTOP, which is a second
/// port of the same table and could in principle drift with it. This one holds
/// the phone to the table.
#[test]
fn ios_canvas_tokens_are_the_values_tokens_md_declares() {
    let src = canvas_swift();
    let mut wrong = Vec::new();
    let mut seen = Vec::new();

    // `--name` in the doc, `DesignCanvas.<swift>` in the phone. The two
    // colour-only sections plus the borrowed mark map one-to-one; the shadow
    // rows carry their colours inside a CSS value and are taken apart below.
    let direct = [
        ("--card", "card"),
        ("--shell", "shell"),
        ("--raised", "raised"),
        ("--sel", "sel"),
        ("--selcard", "selcard"),
        ("--chip", "chip"),
        ("--line", "line"),
        ("--line2", "line2"),
        ("--text", "text"),
        ("--muted", "muted"),
        ("--subtle", "subtle"),
        ("--faint", "faint"),
        ("--blocked", "blocked"),
        ("--working", "working"),
        ("--review", "review"),
        ("--settled", "settled"),
        ("--claude", "claude"),
    ];

    for (var, dark_cell, light_cell) in tokens_md_table() {
        seen.push(var.clone());
        /// One half of one variable against one cell of the table.
        fn compare(
            var: &str,
            swift: &str,
            light: bool,
            want: Option<Hsla>,
            what: &str,
        ) -> Option<String> {
            let Some(want) = want else {
                return Some(format!("  {var}: cannot read `{what}` as a colour"));
            };
            let got = canvas_paint(swift, light);
            (got != want).then(|| {
                let scheme = if light { "light" } else { "dark" };
                format!(
                    "  {var} ({scheme}): DesignCanvas.{swift} = {got:?}, tokens.md says `{what}` = {want:?}"
                )
            })
        }
        let check = |swift: &str, light: bool, want: Option<Hsla>, what: &str| {
            compare(&var, swift, light, want, what)
        };

        if let Some((_, swift)) = direct.iter().find(|(name, _)| *name == var) {
            // `--hover` is the palette's one DELIBERATE deviation and is
            // handled below rather than here.
            wrong.extend(check(swift, false, doc_colour(&dark_cell), &dark_cell));
            wrong.extend(check(swift, true, doc_colour(&light_cell), &light_cell));
            continue;
        }

        match var.as_str() {
            // Same alpha, same neutral, one step off pure — see `tokens.md`'s
            // "Deliberate deviations", and `theme.rs`'s half of the same
            // assertion. Checked as an ALPHA rather than a colour so the
            // deviation cannot quietly grow into a second one.
            "--hover" => {
                let want_alpha = doc_colour(&dark_cell).map(|c| c.a);
                let got = canvas_paint("hover", false);
                if want_alpha != Some(got.a) {
                    wrong.push(format!(
                        "  --hover (dark): alpha {} where tokens.md says {want_alpha:?}",
                        got.a
                    ));
                }
                if got != wash(got.a) {
                    wrong.push(
                        "  --hover (dark): no longer soft-white — the deviation tokens.md \
                         records is `wash(a)`, not some third neutral"
                            .into(),
                    );
                }
                wrong.extend(check("hover", true, doc_colour(&light_cell), &light_cell));
            }
            // `none` in dark; `0 1px 2px rgba(0,0,0,.05)` in light.
            "--lift" => {
                wrong.extend(check("lift", false, Some(transparent()), "none"));
                let light = doc_colours(&light_cell);
                wrong.extend(check("lift", true, light.first().copied(), &light_cell));
            }
            // The ring, then light's second layer.
            "--sellift" => {
                let d = doc_colours(&dark_cell);
                wrong.extend(check("sellift", false, d.first().copied(), &dark_cell));
                wrong.extend(check(
                    "selliftShadow",
                    false,
                    Some(transparent()),
                    "no dark shadow",
                ));
                let l = doc_colours(&light_cell);
                wrong.extend(check("sellift", true, l.first().copied(), &light_cell));
                wrong.extend(check("selliftShadow", true, l.get(1).copied(), &light_cell));
            }
            // The key layer, then the ambient one.
            "--cardshadow" => {
                wrong.extend(check("cardshadow", false, Some(transparent()), "none"));
                wrong.extend(check(
                    "cardshadowAmbient",
                    false,
                    Some(transparent()),
                    "none",
                ));
                let l = doc_colours(&light_cell);
                wrong.extend(check("cardshadow", true, l.first().copied(), &light_cell));
                wrong.extend(check(
                    "cardshadowAmbient",
                    true,
                    l.get(1).copied(),
                    &light_cell,
                ));
            }
            _ => wrong.push(format!(
                "  {var}: tokens.md declares it and DesignCanvas.swift has no answer for it"
            )),
        }
    }

    // …and the other direction: a variable the phone declares that the table
    // does not. `noShadow` is this file's own name for `none` and is exempt.
    for line in src.lines() {
        let Some(rest) = line.trim().strip_prefix("static let ") else {
            continue;
        };
        let name = rest.split(['=', ':']).next().unwrap_or("").trim();
        if name.is_empty() || name == "noShadow" {
            continue;
        }
        let raw = declaration(&src, name);
        if !(raw.starts_with("CanvasToken(") || raw.starts_with("CanvasShadow(")) {
            continue; // an anchor, a hue or a tint percentage — checked elsewhere
        }
        let known = direct.iter().any(|(_, swift)| *swift == name)
            || [
                "hover",
                "lift",
                "sellift",
                "selliftShadow",
                "cardshadow",
                "cardshadowAmbient",
            ]
            .contains(&name);
        if !known {
            wrong.push(format!(
                "  DesignCanvas.{name}: a variable the phone declares that tokens.md does not. \
                 Add it to the table or spend an existing one"
            ));
        }
    }

    // The four anchors and four hues, which the table states as `oklch(L C H)`
    // rather than as rows of their own.
    let mut anchor = |what: &str, got: f32, want: f32| {
        if (got - want).abs() > f32::EPSILON {
            wrong.push(format!(
                "  {what}: DesignCanvas says {got}, the ramp says {want}"
            ));
        }
    };
    anchor("statusL", number(&src, "statusL"), status::L);
    anchor("statusC", number(&src, "statusC"), status::C);
    anchor("statusLLight", number(&src, "statusLLight"), 0.52);
    anchor("statusCLight", number(&src, "statusCLight"), 0.16);
    anchor("hueBlocked", number(&src, "hueBlocked"), status::BLOCKED);
    anchor("hueWorking", number(&src, "hueWorking"), status::WORKING);
    anchor("hueReview", number(&src, "hueReview"), status::REVIEW);
    anchor("hueSettled", number(&src, "hueSettled"), status::SETTLED);

    // `--desk` is the fake desktop the canvas draws BEHIND the window so the
    // corner radius reads. tokens.md says in as many words that it must not
    // acquire a counterpart; a phone has no desktop at all.
    assert!(
        !src.contains("static let desk"),
        "DesignCanvas.swift grew a `--desk` — tokens.md forbids it a counterpart"
    );

    assert!(
        wrong.is_empty(),
        "{} value(s) where the phone's hand-ported table has drifted from \
         docs/design/tokens.md. The doc is the source of truth for all three \
         declarations (this one, crates/ui/src/theme.rs, and the canvases in \
         docs/design/canvas/); fix the Swift, or change the doc and the other \
         two with it:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    assert!(
        seen.iter().any(|v| v == "--card"),
        "tokens.md's surfaces table was not read at all"
    );
}

/// Fully transparent — what `none` means once it is a paint.
fn transparent() -> Hsla {
    black_alpha(0.0)
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
        // A canvas variable spent as paint always ends `.color`; a
        // `DesignCanvas.statusL` forwards a NUMBER and is layout, not paint.
        let is_paint = [
            "themed(",
            "ramp(",
            "whiteAlpha(",
            "oklch(",
            "neutral(",
            "grey(",
            "ink(",
            "wash(",
            "Color(",
            "hex(",
        ]
        .iter()
        .any(|c| raw.starts_with(c))
            || (raw.starts_with("DesignCanvas.") && raw.ends_with(".color"));
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
         with the light half taken from the supplied iOS reference where it \
         declares that job. If the colour really \
         is one colour in both (a brand mark is), say so with a \
         `one-tone-ok: <reason>` comment:",
    );
}

/// No view outside `Theme/` names a colour (gh#279).
///
/// The rule the token port is FOR. Before it, forty call sites across fifteen
/// files reached for `whiteAlpha(a)` with twelve different alphas — 0.02,
/// 0.025, 0.03, 0.035, 0.04, 0.045, 0.05, 0.06, 0.07, 0.08, 0.09, 0.10 — which
/// is gh#172's "a dozen unnamed greys" one level down: a dozen unnamed
/// SURFACES, none of them in the canvas, several a percent apart and reading
/// as noise rather than as levels. The canvas declares four translucent beds
/// and the phone now spends exactly those (`chip`, `elementHover`,
/// `elementActive`, plus the opaque `surfaceRaised`).
///
/// `Theme/` is exempt because that is where paint is MADE: `DesignCanvas.swift`
/// transcribes the table and `Theme.swift` maps it onto jobs. Everything else
/// asks for a job by name.
#[test]
fn no_view_outside_the_theme_names_a_colour() {
    let mut violations = Vec::new();
    for file in swift_sources() {
        if rel(&file).starts_with("Theme") {
            continue;
        }
        let body = std::fs::read_to_string(&file).expect("read source");
        let lines: Vec<&str> = body.lines().collect();
        for (ix, line) in lines.iter().enumerate() {
            if line.trim_start().starts_with("//") || exempt(&lines, ix, "paint-ok:") {
                continue;
            }
            for maker in [
                "whiteAlpha(",
                "wash(",
                "hex(0x",
                "grey(",
                "neutral(",
                "oklch(",
                "Color.white",
                "Color.black",
                "Color(red:",
                "UIColor(red:",
            ] {
                if line.contains(maker) {
                    violations.push(Violation {
                        file: rel(&file),
                        line: ix + 1,
                        text: line.trim().to_string(),
                        why: maker.trim_end_matches(['(', ':']).to_string(),
                    });
                }
            }
        }
    }

    report(
        violations,
        "colour(s) mixed outside Theme/. A view asks for a job — Theme.chip, \
         Theme.elementHover, Theme.surfaceRaised, Theme.border — and \
         Theme.swift says which canvas variable answers it. If a screen really \
         does need paint no token names, add the token (and the canvas \
         variable behind it) rather than the literal; a genuine one-off says \
         so with a `paint-ok: <reason>` comment:",
    );
}

/// No Swift view pins a scheme (gh#257).
///
/// Four call sites used to: the scene in `CometApp.swift` and three sheets,
/// each `.preferredColorScheme(.dark)`. That is why the light design in the
/// reference could not be reached. `cometAppearance()` is now the one modifier
/// that translates an explicit preference into the window-level override.
#[test]
fn no_swift_view_pins_a_colour_scheme() {
    let mut violations = Vec::new();
    for file in swift_sources() {
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
         `.cometAppearance()`, which reads the explicit preference. This claim \
         is intentionally limited to Swift views: Info.plist still forces Dark \
         at the bundle level, and `bundle_style_dependency_is_explicit` holds \
         that remaining integration dependency visible:",
    );
}

/// The ticket forbids editing Info.plist, so this PR cannot honestly claim that
/// the app follows the device appearance by default. Keep the forced key and
/// the code that accounts for it visible until a separately-authorized change
/// removes both together.
#[test]
fn bundle_style_dependency_is_explicit() {
    let plist = info_plist();
    assert!(
        plist.contains("<key>UIUserInterfaceStyle</key>")
            && plist.contains("<string>Dark</string>"),
        "gh#257 ring-fences Info.plist; update this contract when the bundle-level Dark override is separately removed"
    );

    let appearance = appearance_swift();
    for contract in [
        "object(forInfoDictionaryKey: \"UIUserInterfaceStyle\")",
        "case .system: return Appearance.forcedByBundle",
        "forcedByBundle == nil ? allCases : [.light, .dark]",
    ] {
        assert!(
            appearance.contains(contract),
            "Appearance.swift no longer accounts for the ring-fenced bundle override: missing `{contract}`"
        );
    }
}

/// A block observer retained by NotificationCenter must not strongly retain its
/// coordinator. That breaks the retain cycle so deinit can stop it.
#[test]
fn window_scheme_observer_has_a_bounded_lifetime() {
    let appearance = appearance_swift();
    assert!(
        appearance.contains(") { [weak self] _ in"),
        "WindowScheme's activation observer must capture its coordinator weakly"
    );
    assert!(
        appearance.contains("deinit { stopObserving() }"),
        "WindowScheme must remove the weak observer when its coordinator deinitializes"
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
                "oklch(statusLLight, statusCLight, hueReview)".into()
            ))
        );
        assert_eq!(
            split_variants("whiteAlpha(0.08)"),
            Some((
                "Color.white.opacity(0.08)".into(),
                "Color.black.opacity(0.08 * lightInkScale)".into()
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
        let src = "static let lightInkScale: Double = 0.714285714\n";
        assert_eq!(paint(src, "grey(0x14)"), grey(0x14));
        assert_eq!(paint(src, "hex(0x171717)"), hex(0x171717));
        assert_eq!(paint(src, "neutral(0.938)"), neutral(0.938));
        assert_eq!(paint(src, "Color.white.opacity(0.08)"), white_alpha(0.08));
        assert_eq!(
            paint(src, "oklch(0.702, 0.183, 293.541).opacity(0.12)"),
            oklch(0.702, 0.183, 293.541).opacity(0.12)
        );
        // A named constant, and arithmetic on one.
        assert_eq!(
            paint(src, "Color.black.opacity(0.07 * lightInkScale)"),
            black_alpha(0.07 * (5.0 / 7.0))
        );
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
