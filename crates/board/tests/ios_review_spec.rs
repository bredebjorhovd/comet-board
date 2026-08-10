//! The cross-language review fixture (gh#256).
//!
//! `apps/ios/Comet/Board/ReviewModels.swift` is a second implementation of the
//! reading in [`comet_board::claims`] and [`comet_board::effects`] — it has to
//! be, because no Rust runs on that device. Two implementations of one rule is
//! how a phone comes to disagree with a laptop about whether an attempt is
//! trustworthy, and the review screen is the worst place in the app for that to
//! happen: its whole job is to be trusted about what a run did.
//!
//! So the cases live outside both. This test builds them, asks the Rust for
//! every answer, and writes the lot to `apps/ios/Comet/Spec/review-spec.json`;
//! `ReviewSpecRunner.swift` asserts the Swift functions against the same file.
//! Whichever side moves is the side that fails — this test on a rule change in
//! the Rust, the runner on a rule change in the Swift.
//!
//! ```sh
//! cargo test -p comet-board --test ios_review_spec      # this half, and the guard
//! UPDATE_REVIEW_SPEC=1 cargo test -p comet-board --test ios_review_spec   # regenerate
//! scripts/ios-review-spec.sh                     # the Swift half, in a simulator
//! ```
//!
//! **Only this half runs in CI, and that asymmetry is the thing to know.** The
//! Swift half needs a simulator, so it runs when somebody runs it — exactly the
//! gap `apps/ios/README.md` describes for the stats fixture (gh#157).
//! Regenerating without running the simulator half turns the build green while
//! leaving the phone wrong about the rule that just changed. Nothing catches
//! that but a person.

use comet_board::claims::{
    AttemptReview, Brief, ChangedFile, ClaimMark, ClaimView, DiffSource, FindingKind, Remainder,
    Tone,
};
use comet_board::effects::{CallSites, Effects, FileKind, FileScan, Ground};
use comet_board::evidence::{Check, RunEvidence, runs_tests};
use comet_board::verdict;
use comet_proto::view::board::{TaskRow, reviewable};
use serde_json::{Value, json};

// ---- the vocabulary, as the fixture spells it ----------------------------
//
// `Tone`, `FindingKind`, `ClaimMark` and `Ground` are meanings rather than wire
// values, so they carry no `Serialize`. Named here in the spelling the Swift
// enums use, which makes this list the one place the two vocabularies are
// pinned to each other.

fn tone(tone: Tone) -> &'static str {
    match tone {
        Tone::Alarm => "alarm",
        Tone::Unknown => "unknown",
        Tone::Settled => "settled",
    }
}

fn finding_kind(kind: FindingKind) -> &'static str {
    match kind {
        FindingKind::Unclaimed => "unclaimed",
        FindingKind::Uncommitted => "uncommitted",
        FindingKind::UnsupportedClaims => "unsupported_claims",
        FindingKind::NeverPassed => "never_passed",
        FindingKind::Unchecked => "unchecked",
        FindingKind::NeverClaimed => "never_claimed",
        FindingKind::MalformedClaims => "malformed_claims",
        FindingKind::NoDiff => "no_diff",
    }
}

fn mark(mark: ClaimMark) -> &'static str {
    match mark {
        ClaimMark::Contradicted => "contradicted",
        ClaimMark::Corroborated => "corroborated",
        ClaimMark::Bare => "bare",
    }
}

fn ground(ground: Ground) -> &'static str {
    match ground {
        Ground::Neutral => "neutral",
        Ground::Settled => "settled",
        Ground::Working => "working",
        Ground::Unknown => "unknown",
    }
}

// ---- builders ------------------------------------------------------------

fn changed(path: &str, status: &str, added: u32, removed: u32) -> ChangedFile {
    ChangedFile {
        path: path.into(),
        status: status.into(),
        added,
        removed,
        binary: false,
        symbols: Vec::new(),
    }
}

fn claim(
    text: &str,
    files: &[&str],
    symbols: &[&str],
    matched: &[&str],
    unmatched: &[&str],
) -> ClaimView {
    ClaimView {
        text: text.into(),
        files: files.iter().map(|s| (*s).to_string()).collect(),
        symbols: symbols.iter().map(|s| (*s).to_string()).collect(),
        matched: matched.iter().map(|s| (*s).to_string()).collect(),
        unmatched: unmatched.iter().map(|s| (*s).to_string()).collect(),
        call_sites: Vec::new(),
    }
}

fn scan(
    path: &str,
    kind: FileKind,
    tests_added: u32,
    api: u32,
    schema: u32,
    keys: u32,
) -> FileScan {
    FileScan {
        path: path.into(),
        kind: Some(kind),
        tests_added,
        tests_removed: 0,
        api,
        schema,
        config_keys: keys,
    }
}

fn check(command: &str, runs: u32, failed: u32) -> Check {
    Check {
        command: command.into(),
        runs,
        failed,
    }
}

fn blank(task_id: &str, identifier: &str) -> AttemptReview {
    AttemptReview {
        task_id: task_id.into(),
        attempt: 1,
        attempt_number: 1,
        state: "review".into(),
        outcome: Some("done".into()),
        branch: Some("board/gh-1".into()),
        worktree: None,
        pr_url: Some("https://github.com/o/r/pull/9".into()),
        brief: Brief {
            identifier: identifier.into(),
            title: "A title".into(),
            url: "https://github.com/o/r/issues/1".into(),
            body: None,
        },
        claimed_at: Some("2026-08-10T07:41:00Z".into()),
        claims_error: None,
        remainder: Remainder::default(),
        changed: Vec::new(),
        diff: DiffSource::Checkout,
        uncommitted: Some(0),
        evidence: RunEvidence::default(),
        effects: Effects::default(),
    }
}

// ---- the cases -----------------------------------------------------------

/// The screen the design draws: three claims, five effect chips, and two
/// changes nobody named.
fn the_alarming_one() -> AttemptReview {
    AttemptReview {
        changed: vec![
            changed("crates/ui/src/transcript.rs", "M", 64, 12),
            changed("crates/ui/src/tools.rs", "M", 21, 4),
            changed("crates/ui/src/board.rs", "M", 8, 3),
            changed("Cargo.toml", "M", 1, 0),
            changed("crates/ui/src/shell.rs", "M", 6, 4),
        ],
        remainder: Remainder {
            claims: vec![
                claim(
                    "A group's header paints its own state, never a child's.",
                    &["crates/ui/src/transcript.rs"],
                    &[],
                    &["crates/ui/src/transcript.rs"],
                    &[],
                ),
                ClaimView {
                    call_sites: vec![CallSites {
                        symbol: "group_tone".into(),
                        now: 1,
                        before: Some(2),
                    }],
                    ..claim(
                        "Both surfaces read one derivation per frame.",
                        &[],
                        &["group_tone"],
                        &["crates/ui/src/tools.rs"],
                        &[],
                    )
                },
                claim(
                    "A retried child stops recolouring the header it sits under.",
                    &["crates/ui/src/board.rs"],
                    &[],
                    &["crates/ui/src/board.rs"],
                    &[],
                ),
            ],
            unclaimed: vec![
                changed("Cargo.toml", "M", 1, 0),
                changed("crates/ui/src/shell.rs", "M", 6, 4),
            ],
            unmatched_anchors: Vec::new(),
            claimed: 3,
        },
        evidence: RunEvidence {
            commands: 84,
            failed: 6,
            checks: vec![
                check("cargo test -p comet-ui", 7, 1),
                check("cargo clippy --all-targets", 3, 0),
            ],
            truncated: false,
        },
        effects: Effects {
            read: true,
            files: vec![
                scan("crates/ui/src/transcript.rs", FileKind::Rust, 4, 0, 0, 0),
                scan("crates/ui/src/tools.rs", FileKind::Rust, 2, 0, 0, 0),
                scan("crates/ui/src/board.rs", FileKind::Rust, 0, 0, 0, 0),
                scan("Cargo.toml", FileKind::Manifest, 0, 0, 0, 0),
                scan("crates/ui/src/shell.rs", FileKind::Rust, 0, 0, 0, 0),
            ],
            tests_after: Some(47),
            tests_before: Some(41),
            deps_added: vec!["itertools 0.13".into()],
            deps_known: true,
        },
        ..blank("gh:o/r#117", "gh#117")
    }
}

/// Never asked is not claimed nothing (§gh#183): unknown, quiet, never green.
fn the_silent_one() -> AttemptReview {
    AttemptReview {
        state: "failed".into(),
        outcome: Some("failed".into()),
        pr_url: None,
        claimed_at: None,
        changed: vec![
            changed("edge/wrangler.toml", "M", 12, 9),
            changed("edge/src/rooms/session.ts", "M", 3, 1),
        ],
        remainder: Remainder {
            unclaimed: vec![
                changed("edge/wrangler.toml", "M", 12, 9),
                changed("edge/src/rooms/session.ts", "M", 3, 1),
            ],
            ..Remainder::default()
        },
        diff: DiffSource::Recorded,
        uncommitted: None,
        evidence: RunEvidence {
            commands: 19,
            failed: 4,
            checks: Vec::new(),
            truncated: false,
        },
        ..blank("gh:o/edge#39", "gh#39")
    }
}

/// The other side of the distinction: asked, and claimed nothing.
fn the_empty_one() -> AttemptReview {
    AttemptReview {
        changed: vec![changed("src/db.rs", "M", 4, 1)],
        remainder: Remainder {
            unclaimed: vec![changed("src/db.rs", "M", 4, 1)],
            ..Remainder::default()
        },
        ..blank("gh:o/r#183", "gh#183")
    }
}

/// A block the board could not read is louder than silence (§gh#235): the work
/// was described, and nobody can check the description.
fn the_malformed_one() -> AttemptReview {
    AttemptReview {
        claims_error: Some(
            "line 3: a claim needs an anchor\n\nWrite `did the thing :: path/to/file.rs`.".into(),
        ),
        changed: vec![
            changed("src/a.rs", "M", 2, 0),
            changed("src/b.rs", "A", 40, 0),
        ],
        remainder: Remainder {
            unclaimed: vec![
                changed("src/a.rs", "M", 2, 0),
                changed("src/b.rs", "A", 40, 0),
            ],
            ..Remainder::default()
        },
        ..blank("gh:o/r#235", "gh#235")
    }
}

/// No diff means no denominator, so every other finding is suppressed — an
/// empty changed set would otherwise read as good news.
fn the_unreadable_one() -> AttemptReview {
    AttemptReview {
        claimed_at: None,
        diff: DiffSource::Unavailable {
            reason: "the checkout was reclaimed".into(),
        },
        uncommitted: None,
        ..blank("gh:o/r#72", "gh#72")
    }
}

/// A complete remainder that is still wrong: work in the checkout on no branch,
/// a claim anchored to nothing, a suite that never passed, and a shrinking test
/// count — the four facts that each have to survive a clean diff.
fn the_complete_but_wrong_one() -> AttemptReview {
    AttemptReview {
        changed: vec![
            changed("src/db.rs", "M", 4, 1),
            changed("lib/thing.rb", "M", 9, 2),
        ],
        remainder: Remainder {
            claims: vec![
                claim("Did the storage", &["src/db.rs"], &[], &["src/db.rs"], &[]),
                claim("Did the API", &["src/api.rs"], &[], &[], &["src/api.rs"]),
                claim(
                    "Did the ruby",
                    &["lib/thing.rb"],
                    &[],
                    &["lib/thing.rb"],
                    &[],
                ),
            ],
            unclaimed: Vec::new(),
            unmatched_anchors: vec!["src/api.rs".into()],
            claimed: 2,
        },
        uncommitted: Some(3),
        evidence: RunEvidence {
            commands: 12,
            failed: 5,
            checks: vec![check("cargo test -p comet-board", 5, 5)],
            truncated: true,
        },
        effects: Effects {
            read: true,
            files: vec![
                scan("src/db.rs", FileKind::Rust, 0, 2, 1, 0),
                scan("lib/thing.rb", FileKind::OtherCode, 0, 0, 0, 2),
            ],
            tests_after: Some(38),
            tests_before: Some(41),
            deps_added: Vec::new(),
            deps_known: false,
        },
        ..blank("gh:o/r#236", "gh#236")
    }
}

/// A branch the board never read: one chip saying exactly that, and not five
/// chips of reassurance nobody earned.
fn the_unread_one() -> AttemptReview {
    AttemptReview {
        changed: vec![changed("src/db.rs", "M", 4, 1)],
        remainder: Remainder {
            claims: vec![claim("Did it", &["src/db.rs"], &[], &["src/db.rs"], &[])],
            unclaimed: Vec::new(),
            unmatched_anchors: Vec::new(),
            claimed: 1,
        },
        evidence: RunEvidence {
            commands: 4,
            failed: 0,
            checks: Vec::new(),
            truncated: false,
        },
        ..blank("gh:o/r#180", "gh#180")
    }
}

/// A binary file, a call site the board could not count in the base tree, and a
/// suite nobody ran — the three "not zero, unknown" cases in one review.
fn the_unknowable_one() -> AttemptReview {
    AttemptReview {
        changed: vec![ChangedFile {
            binary: true,
            ..changed("assets/logo.png", "A", 0, 0)
        }],
        remainder: Remainder {
            claims: vec![ClaimView {
                call_sites: vec![
                    CallSites {
                        symbol: "render_logo".into(),
                        now: 3,
                        before: None,
                    },
                    CallSites {
                        symbol: "gone".into(),
                        now: 2,
                        before: Some(0),
                    },
                    CallSites {
                        symbol: "same".into(),
                        now: 1,
                        before: Some(1),
                    },
                ],
                ..claim(
                    "Swapped the mark.",
                    &["assets/logo.png"],
                    &[],
                    &["assets/logo.png"],
                    &[],
                )
            }],
            unclaimed: Vec::new(),
            unmatched_anchors: Vec::new(),
            claimed: 1,
        },
        evidence: RunEvidence {
            commands: 2,
            failed: 0,
            checks: vec![check("cargo build", 1, 0)],
            truncated: false,
        },
        effects: Effects {
            read: true,
            files: vec![scan("assets/logo.png", FileKind::Prose, 0, 0, 0, 0)],
            tests_after: None,
            tests_before: None,
            deps_added: vec!["a".into(), "b".into()],
            deps_known: true,
        },
        ..blank("gh:o/r#238", "gh#238")
    }
}

// ---- the fixture ---------------------------------------------------------

fn case(name: &str, review: &AttemptReview) -> Value {
    let (label, added, removed) = diff_totals(review);
    json!({
        "name": name,
        "review": review,
        "expect": {
            "claimed": review.claimed(),
            "complete": review.remainder.complete(),
            "verdict": {
                "tone": tone(review.verdict().tone),
                "text": review.verdict().text,
            },
            "findings": review.findings().iter().map(|f| json!({
                "kind": finding_kind(f.kind),
                "text": f.text,
            })).collect::<Vec<_>>(),
            "effectChips": review.effect_chips().iter().map(|c| json!({
                "text": c.text,
                "ground": ground(c.ground),
            })).collect::<Vec<_>>(),
            "claims": review.remainder.claims.iter().map(|claim| json!({
                "mark": mark(review.claim_mark(claim)),
                "chips": review.claim_chips(claim).iter().map(|c| json!({
                    "text": c.text,
                    "ground": ground(c.ground),
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "diffLabel": label,
            "diffAdded": added,
            "diffRemoved": removed,
            "counts": review.changed.iter().map(|f| f.counts()).collect::<Vec<_>>(),
            // Both halves of the sentence the bar states before it sends, so
            // the phone cannot promise a delivery the board would decline.
            "contractDelivering": verdict::contract_line(review, true),
            "contractPostedOnly": verdict::contract_line(review, false),
        }
    })
}

/// The strip that closes the body (§gh#238). A three-line rule that lives in
/// `comet_ui::review` where the Swift cannot see it, restated here so the
/// fixture still pins it — if it ever moves into `comet_board`, this call site
/// is the one to change.
fn diff_totals(review: &AttemptReview) -> (String, u32, u32) {
    let added = review.changed.iter().map(|f| f.added).sum();
    let removed = review.changed.iter().map(|f| f.removed).sum();
    let label = match review.changed.len() {
        1 => "1 file changed".to_string(),
        n => format!("{n} files changed"),
    };
    (label, added, removed)
}

/// One row for the shared review-door rule. Decoded from the published board
/// wire shape so this fixture checks the same input the phone receives.
fn reviewable_case(state: &str, attempts: usize) -> Value {
    let row: TaskRow = serde_json::from_value(json!({
        "id": "gh:o/r#1",
        "identifier": "gh#1",
        "title": "A task",
        "state": state,
        "source": "github",
        "url": "https://github.com/o/r/issues/1",
        "labels": [],
        "dispatchable": true,
        "gone": false,
        "route": "r",
        "workspace": "r",
        "runtime": null,
        "chat_id": null,
        "pr_url": null,
        "pr_number": null,
        "branch": null,
        "dispatched_by": null,
        "dispatched_by_chat": null,
        "last_outcome": null,
        "last_outcome_at": null,
        "attempts": attempts,
        "reopened": 0
    }))
    .expect("published task row");
    json!({ "state": state, "attempts": attempts, "expect": reviewable(&row) })
}

fn build() -> Value {
    let tests_cases: Vec<Value> = [
        "cargo test -p comet-ui",
        "cargo testbed",
        "cd web && pnpm test",
        "cd web && pnpm build",
        "CI=1 time cargo nextest run",
        "grep -r cargo test src/",
        "python -m pytest -q",
        "make test",
        "make testify",
        "npm run lint",
        "./gradlew test --info",
        "echo cargo test",
    ]
    .iter()
    .map(|command| json!({ "command": command, "expect": runs_tests(command) }))
    .collect();

    json!({
        "note": "Generated by crates/board/tests/ios_review_spec.rs. \
                 Do not hand-edit — run `UPDATE_REVIEW_SPEC=1 cargo test -p comet-board --test ios_review_spec`, \
                 then scripts/ios-review-spec.sh to find out what the Swift disagrees about.",
        "reviews": [
            case("the design's screen: two changes nobody claimed", &the_alarming_one()),
            case("never answered the contract", &the_silent_one()),
            case("asked, and claimed nothing", &the_empty_one()),
            case("a claims block the board could not read", &the_malformed_one()),
            case("no diff to check against", &the_unreadable_one()),
            case("a clean remainder that is still wrong", &the_complete_but_wrong_one()),
            case("a branch the board never read", &the_unread_one()),
            case("binary, uncounted call sites, and an unrun suite", &the_unknowable_one()),
        ],
        "reviewable": [
            reviewable_case("blocked", 1),
            reviewable_case("working", 1),
            reviewable_case("ready", 1),
            reviewable_case("review", 1),
            reviewable_case("failed", 1),
            reviewable_case("done", 1),
            reviewable_case("ready", 0),
            reviewable_case("review", 0),
        ],
        "runsTests": tests_cases,
    })
}

fn spec_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../apps/ios/Comet/Spec/review-spec.json")
}

#[test]
fn the_cross_language_fixture_matches_this_reading() {
    let built = build();
    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(&built).expect("serializes")
    );
    let path = spec_path();
    if std::env::var("UPDATE_REVIEW_SPEC").is_ok() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("fixture directory");
        }
        std::fs::write(&path, &rendered).expect("writes the fixture");
        return;
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "cannot read {}: {err}. Run `UPDATE_REVIEW_SPEC=1 cargo test -p comet-board --test ios_review_spec`",
            path.display()
        )
    });
    // Compared as JSON, not as text: reformatting the file is not a behaviour
    // change, and a rule moving is.
    let disk: Value = serde_json::from_str(&on_disk).expect("the fixture is JSON");
    if disk == built {
        return;
    }
    // Report the case that moved rather than the whole document — a diff of
    // four hundred lines is a diff nobody reads.
    let mut moved = Vec::new();
    for key in ["note", "reviewable", "runsTests"] {
        if disk.get(key) != built.get(key) {
            moved.push(format!("  {key} changed"));
        }
    }
    let (disk_reviews, built_reviews) = (
        disk["reviews"].as_array().cloned().unwrap_or_default(),
        built["reviews"].as_array().expect("reviews").clone(),
    );
    for (ix, case) in built_reviews.iter().enumerate() {
        let name = case["name"].as_str().unwrap_or("?");
        match disk_reviews.get(ix) {
            Some(old) if old == case => {}
            Some(old) => moved.push(format!(
                "  {name}:\n    fixture: {}\n    now:     {}",
                old["expect"], case["expect"]
            )),
            None => moved.push(format!("  {name}: new case, not in the fixture")),
        }
    }
    if disk_reviews.len() > built_reviews.len() {
        moved.push(format!(
            "  {} case(s) in the fixture this test no longer builds",
            disk_reviews.len() - built_reviews.len()
        ));
    }
    panic!(
        "the review reading has moved and apps/ios/Comet/Spec/review-spec.json has not.\n{}\n\n\
         Regenerate with `UPDATE_REVIEW_SPEC=1 cargo test -p comet-board --test ios_review_spec`, then \
         run `scripts/ios-review-spec.sh` — the phone reimplements this reading and only the \
         simulator half will tell you whether it still agrees.",
        moved.join("\n")
    );
}
