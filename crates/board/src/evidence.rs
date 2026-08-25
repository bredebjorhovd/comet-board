//! What the board can say about a run without taking the agent's word for it
//! (§gh#183) — the commands it executed, and how they exited.
//!
//! A summary written by the model that wrote the code inherits its blind
//! spots, so a claim is only worth reading beside something the agent did not
//! author. Two such things exist today and neither needs new instrumentation:
//! the **diff** knows what moved (that half is [`crate::claims`]), and the
//! **run journal** knows what ran. Every harness emits `ToolCall`/`ToolResult`
//! pairs into the journal already, so "cargo test ran nine times and failed
//! twice" is a fact the board holds and did not have to ask for.
//!
//! Deliberately *not* here, and named as follow-ups rather than half-built:
//!
//! - **Which tests ran, and which of them are new.** That needs the harness to
//!   parse a test runner's output, which is instrumentation this ticket does
//!   not have. A command and its exit status is the honest ceiling of what the
//!   journal already records.
//! - **Call sites that moved, and schema changes.** Those are diff questions
//!   that want a parser per language; the diff half here stops at which files
//!   moved and by how many lines.
//!
//! ## Why the check list is a heuristic, and why that is safe
//!
//! [`is_check`] recognises a fixed list of verification commands by prefix. It
//! will miss a project whose test runner is a script nobody here has heard of,
//! and that is the intended direction of the error: a missed check under-states
//! the evidence, which makes a review *more* suspicious rather than less. The
//! totals beside the list are what keep the miss visible — "0 checks among 214
//! commands" is itself a finding, and it is a finding the list cannot suppress.
//!
//! ## Visual/runtime evidence (§gh#421)
//!
//! The other blind spot: behavior that has to be *seen*. Everything above
//! answers a question about code; nothing here answered "what did it look
//! like when you ran it?". An [`EvidenceArtifact`] is one bounded capture —
//! pixels, a recording, an accessibility tree, an excerpt — published onto an
//! attempt, with provenance split by author: the agent supplies kind, bytes,
//! URL, viewport and description; the **board** stamps task, attempt,
//! producing chat, receive time, and the commit/dirty fingerprint it reads out
//! of the worktree itself. The design is
//! docs/board/gh-421-show-the-ui-it-tested.md.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---- visual/runtime evidence (§gh#421) ------------------------------------

/// One bounded visual/runtime artifact an attempt published (§gh#421).
///
/// Every field the agent could have invented and the board could instead read,
/// the board read: `commit_sha`, `dirty_files`, `bytes`, `sha256` and
/// `attached_at` are never taken from the submitter. That is what makes this
/// evidence rather than illustration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceArtifact {
    /// `<kind>-<first 8 hex of sha256>` — unique within the attempt by
    /// construction, since re-attaching the same bytes of the same kind is the
    /// no-op that returns the artifact already there.
    pub id: String,
    pub kind: ArtifactKind,
    /// What it demonstrates, in the agent's words. Bounded prose with no
    /// checker behind it — unlike a claim, nothing here is matched against the
    /// diff, which is exactly why the board-stamped fields beside it matter.
    pub description: String,
    /// The URL the capture was taken at, query stripped: a URL field that
    /// recorded tokens would be the leak this feature does not get to have.
    pub url: Option<String>,
    /// The viewport, spelled `WxH`.
    pub viewport: Option<String>,
    /// When the **board** received it, its own clock.
    pub attached_at: String,
    /// What the attempt's worktree pointed at when the board fingerprinted it.
    /// `None` when there was no readable checkout — stated, never guessed.
    pub commit_sha: Option<String>,
    /// Uncommitted changed files at fingerprint time. The honest tell for
    /// stale pixels: capture happened before attach, and a large count beside
    /// fresh commits says the tree moved in between.
    pub dirty_files: u32,
    pub bytes: u64,
    /// Over the exact bytes stored — dedupe key within the attempt, and the
    /// integrity check any reader can run.
    pub sha256: String,
    /// File name under the attempt's evidence directory, relative to it.
    pub file: String,
}

impl EvidenceArtifact {
    /// The identity two attachments are compared on: same kind and same bytes
    /// is the same artifact, however many times the call retried.
    pub fn identity(kind: ArtifactKind, sha256: &str) -> String {
        format!("{kind}-{}", &sha256[..8.min(sha256.len())])
    }
}

/// What kind of thing an artifact shows. The kind decides the ceiling
/// ([`ArtifactKind::max_bytes`]) and how a review renders it, and it is
/// parsed from the agent's spelling so a typo costs an error naming the valid
/// set — never a silent default to screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Screenshot,
    Recording,
    Accessibility,
    Console,
    Log,
}

impl ArtifactKind {
    /// Parse the agent's spelling. `a11y` is accepted as accessibility because
    /// it is the spelling every frontend agent actually writes.
    pub fn parse(kind: &str) -> Option<ArtifactKind> {
        match kind.trim().to_ascii_lowercase().as_str() {
            "screenshot" | "screen" => Some(ArtifactKind::Screenshot),
            "recording" | "video" => Some(ArtifactKind::Recording),
            "accessibility" | "a11y" | "ax" => Some(ArtifactKind::Accessibility),
            "console" | "network" => Some(ArtifactKind::Console),
            "log" | "logs" => Some(ArtifactKind::Log),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ArtifactKind::Screenshot => "screenshot",
            ArtifactKind::Recording => "recording",
            ArtifactKind::Accessibility => "accessibility",
            ArtifactKind::Console => "console",
            ArtifactKind::Log => "log",
        }
    }

    /// The byte ceiling for one artifact of this kind. A recording may be
    /// twenty-four times a screenshot because moving pixels cost more than
    /// still ones; everything text-shaped is bounded far below both, because
    /// an excerpt nobody scrolls is worth more than a log nobody finishes.
    pub fn max_bytes(self) -> u64 {
        const MIB: u64 = 1024 * 1024;
        match self {
            ArtifactKind::Screenshot => 10 * MIB,
            ArtifactKind::Recording => 24 * MIB,
            ArtifactKind::Accessibility => MIB,
            ArtifactKind::Console => 256 * 1024,
            ArtifactKind::Log => MIB,
        }
    }

    /// The file extension stored bytes get when they are not one of the
    /// formats [`sniff_ext`] recognises — text kinds are text because the
    /// agent said so and the byte ceiling keeps any lie small.
    pub fn default_ext(self) -> &'static str {
        match self {
            ArtifactKind::Accessibility | ArtifactKind::Console | ArtifactKind::Log => "txt",
            ArtifactKind::Screenshot | ArtifactKind::Recording => "bin",
        }
    }
}

impl std::fmt::Display for ArtifactKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How many artifacts one attempt may publish. Eight covers both surfaces of
/// a mid-sized frontend change at desktop and phone widths with room for the
/// console excerpt that explains them; more than that is a gallery, and the
/// review is not one.
pub const MAX_ARTIFACTS: usize = 8;

/// How long one artifact's description may be.
pub const MAX_DESCRIPTION: usize = 300;

/// How long a captured URL may be.
pub const MAX_URL: usize = 2048;

/// How many days an artifact's *bytes* live after receipt. The record is
/// permanent — provenance stays true after the pixels are gone — and expiry
/// is visible wherever the artifact would have rendered.
pub const RETAIN_EVIDENCE_DAYS: u64 = 90;

/// Everything an agent supplies when attaching one artifact, validated as a
/// set by [`validate_artifact`]. The bytes ride separately; they are the bulk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactInput {
    pub kind: ArtifactKind,
    pub description: String,
    pub url: Option<String>,
    pub viewport: Option<String>,
}

/// Validate one attachment the way the board's host does, before anything is
/// written. Every refusal names the bound it hit, so the caller's next
/// attempt can be right rather than merely smaller.
pub fn validate_artifact(
    input: &ArtifactInput,
    existing: usize,
    bytes_len: u64,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        existing < MAX_ARTIFACTS,
        "this attempt already carries {existing} artifacts (max {MAX_ARTIFACTS}) — \
         attach the ones that carry the review"
    );
    let description = input.description.trim();
    anyhow::ensure!(
        !description.is_empty(),
        "say what the artifact demonstrates — an unlabelled screenshot is a puzzle, \
         not evidence"
    );
    anyhow::ensure!(
        description.chars().count() <= MAX_DESCRIPTION,
        "the description is {} characters; keep it under {MAX_DESCRIPTION} — \
         the artifact shows, the sentence points",
        description.chars().count()
    );
    if let Some(url) = &input.url {
        anyhow::ensure!(
            url.chars().count() <= MAX_URL,
            "the URL is {} characters (max {MAX_URL})",
            url.chars().count()
        );
        let stripped = strip_query(url);
        anyhow::ensure!(
            stripped == *url,
            "the URL carries a query string — strip it (`{stripped}`): a stored capture \
             URL must never record a token"
        );
    }
    if let Some(viewport) = &input.viewport {
        anyhow::ensure!(
            is_viewport(viewport),
            "`{viewport}` is not a viewport — spell it `WxH`, e.g. `1440x900`"
        );
    }
    let max = input.kind.max_bytes();
    anyhow::ensure!(bytes_len > 0, "{} is empty — nothing to show", input.kind);
    anyhow::ensure!(
        bytes_len <= max,
        "this {} is {bytes_len} bytes; the cap for {} is {max} — trim the excerpt or \
         shorten the recording",
        input.kind,
        input.kind
    );
    Ok(())
}

/// Is this the `WxH` spelling a viewport is kept in?
pub fn is_viewport(viewport: &str) -> bool {
    let Some((w, h)) = viewport.split_once(['x', 'X']) else {
        return false;
    };
    !w.is_empty()
        && !h.is_empty()
        && w.chars().all(|c| c.is_ascii_digit())
        && h.chars().all(|c| c.is_ascii_digit())
        && w.len() <= 5
        && h.len() <= 5
}

/// The URL with its query string removed — the only transformation the URL
/// field gets, applied by the board rather than trusted from the caller.
pub fn strip_query(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => base.to_string(),
        None => url.to_string(),
    }
}

/// What format binary artifact bytes actually are, read off their magic — the
/// extension a file is stored under comes from this and never from the name
/// the agent's tool happened to give it.
pub fn sniff_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("jpg")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"\x1a\x45\xdf\xa3") {
        // Matroska/WebM (EBML) — screencast frames and most recorder output.
        Some("webm")
    } else if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        Some("mp4")
    } else {
        None
    }
}

/// SHA-256 over the exact bytes, hex — the content address everything else
/// (id, file name, dedupe) derives from.
pub fn fingerprint(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// One command a run executed, as the journal recorded it.
///
/// `failed` is the tool result's `is_error`, i.e. the command exited non-zero
/// (or the harness could not run it at all). A call whose result never landed —
/// the run died mid-command — is **not** failed: the board does not know how it
/// ended, and guessing would invent evidence in the one direction this module
/// exists to avoid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RanCommand {
    pub command: String,
    pub failed: bool,
}

/// One verification command, and what it did across the whole run.
///
/// Deduplicated by the exact command text: an agent runs `cargo test -p
/// comet-board` eleven times while it works, and eleven identical rows say
/// nothing that `runs: 11` does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    pub command: String,
    /// How many times it ran.
    pub runs: u32,
    /// How many of those exited non-zero. `runs == failed` is a check that
    /// never once passed, which is the loudest thing this struct can say.
    pub failed: u32,
}

impl Check {
    /// Did this command pass at least once? Not "did it pass last" — the
    /// journal's order is the order calls were *made*, and pairing a retry with
    /// its predecessor is more than the record supports.
    pub fn ever_passed(&self) -> bool {
        self.failed < self.runs
    }
}

/// What a run's journal says it did, summed (§gh#183).
///
/// The totals are over *every* exec call, and the list is the recognised
/// subset. Both, because they answer different questions: the list is "what
/// was checked", and the totals are the denominator that stops an empty list
/// reading as a quiet run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunEvidence {
    /// Every shell command the run executed.
    pub commands: u32,
    /// How many of them exited non-zero. Routine — an agent greps for a symbol
    /// that is not there — so this is context, not a verdict.
    pub failed: u32,
    /// The ones [`is_check`] recognised as verifying something, deduplicated.
    pub checks: Vec<Check>,
    /// Whether [`MAX_CHECKS`] cut the list short. Never silent: a capped list
    /// that did not say so would read as the whole of what ran.
    pub truncated: bool,
}

impl RunEvidence {
    /// Did anything at all check this work? The question the review surface
    /// asks first, and the one an empty `checks` list answers `false` to
    /// however busy `commands` was.
    pub fn checked(&self) -> bool {
        !self.checks.is_empty()
    }

    /// Checks that never passed. What a review should read before any claim.
    pub fn failing(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| !c.ever_passed())
    }
}

/// How many distinct check commands are kept. A run that genuinely used forty
/// different verification commands is already telling a strange story, and the
/// column this lands in is read on every review.
pub const MAX_CHECKS: usize = 40;

/// How much of one command is kept. Agents pipe heredocs into `python -`; the
/// evidence is *that it ran*, not the script.
pub const MAX_COMMAND: usize = 200;

/// Sum a run's commands into the evidence a review renders.
///
/// Ordering is deliberate and not chronological: failures first, then the
/// most-run, then alphabetical. A review reads the top of this list and stops,
/// so what failed has to be there — and the journal's own order is the order
/// calls were *made*, which puts the first exploratory `ls` above the test run
/// that mattered.
pub fn gather(commands: &[RanCommand]) -> RunEvidence {
    let mut checks: Vec<Check> = Vec::new();
    let mut out = RunEvidence::default();
    for ran in commands {
        out.commands += 1;
        if ran.failed {
            out.failed += 1;
        }
        if !is_check(&ran.command) {
            continue;
        }
        let command = clip(&ran.command);
        match checks.iter_mut().find(|c| c.command == command) {
            Some(seen) => {
                seen.runs += 1;
                seen.failed += u32::from(ran.failed);
            }
            None => checks.push(Check {
                command,
                runs: 1,
                failed: u32::from(ran.failed),
            }),
        }
    }
    checks.sort_by(|a, b| {
        (b.failed > 0)
            .cmp(&(a.failed > 0))
            .then(b.runs.cmp(&a.runs))
            .then(a.command.cmp(&b.command))
    });
    out.truncated = checks.len() > MAX_CHECKS;
    checks.truncate(MAX_CHECKS);
    out.checks = checks;
    out
}

/// Does this command check something?
///
/// True when any segment of the command line starts with one of
/// [`VERIFICATION`]. Segments, not the whole string, because the shape agents
/// actually write is `cd x && cargo test` and `mkdir -p out && pytest -q` —
/// keying on the first token alone would recognise almost nothing.
///
/// A prefix match, so flags and paths ride along: `cargo test -p comet-board
/// claims::` is `cargo test`. The word boundary is checked on the far end too,
/// so `cargo testbed` is not `cargo test`.
pub fn is_check(command: &str) -> bool {
    segments(command).any(|s| {
        VERIFICATION.iter().any(|v| {
            s.strip_prefix(v)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(|c: char| !word_char(c)))
        })
    })
}

/// The command lines inside one command: split on `&&`, `||`, `;` and `|`, each
/// trimmed of leading environment assignments (`RUST_LOG=debug cargo test`) and
/// of the shell noise that can precede a verb.
fn segments(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(['\n', ';'])
        .flat_map(|s| s.split("&&"))
        .flat_map(|s| s.split("||"))
        .flat_map(|s| s.split('|'))
        .map(strip_env)
}

/// Drop leading `KEY=value` assignments and `env`/`time`/`nice` wrappers, so
/// `CI=1 cargo test` and `time cargo test` are both `cargo test`.
fn strip_env(segment: &str) -> &str {
    let mut rest = segment.trim_start_matches(['(', ' ', '\t']);
    loop {
        let Some(token) = rest.split_whitespace().next() else {
            return rest;
        };
        let wrapper = matches!(token, "env" | "time" | "nice" | "sudo" | "exec");
        let assignment = token
            .split_once('=')
            .is_some_and(|(k, _)| !k.is_empty() && k.chars().all(word_char));
        if !wrapper && !assignment {
            return rest;
        }
        let Some(after) = rest[token.len()..].strip_prefix(|c: char| c.is_whitespace()) else {
            return rest;
        };
        rest = after.trim_start();
    }
}

fn word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Does this command run a *test suite*, as opposed to checking something else
/// (§gh#236)?
///
/// A narrower question than [`is_check`] and asked for a narrower purpose: the
/// review's tests chip says whether the tests pass, and a green `cargo build`
/// or `cargo fmt` is not an answer to that. Same segment-and-prefix machinery,
/// over [`TESTS`] instead of [`VERIFICATION`], so `cd web && pnpm test` counts
/// exactly where `cd web && pnpm build` does not.
pub fn runs_tests(command: &str) -> bool {
    segments(command).any(|s| {
        TESTS.iter().any(|v| {
            s.strip_prefix(v)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(|c: char| !word_char(c)))
        })
    })
}

/// The subset of [`VERIFICATION`] that runs tests.
///
/// A subset rather than a second list: every entry here is spelled exactly as
/// it is spelled there, so a command cannot be a test runner and not a check —
/// which would make the evidence block and the tests chip disagree about the
/// same line of the journal.
pub const TESTS: &[&str] = &[
    "cargo test",
    "cargo nextest",
    "cargo bench",
    "npm test",
    "npm run test",
    "pnpm test",
    "pnpm run test",
    "yarn test",
    "bun test",
    "jest",
    "vitest",
    "mocha",
    "playwright test",
    "pytest",
    "python -m pytest",
    "python -m unittest",
    "tox",
    "nox",
    "go test",
    "make test",
    "swift test",
    "gradle test",
    "./gradlew test",
    "mvn test",
    "dotnet test",
    "rspec",
    "phpunit",
];

/// Commands that check something, longest-prefix spellings included.
///
/// Not a taxonomy of build tools — a list of the verbs that mean "I verified
/// this", which is the only question the review asks. Adding to it is cheap and
/// safe; the totals beside the list are what keep an unrecognised runner
/// visible in the meantime.
pub const VERIFICATION: &[&str] = &[
    // Rust
    "cargo test",
    "cargo nextest",
    "cargo check",
    "cargo clippy",
    "cargo build",
    "cargo fmt",
    "cargo doc",
    "cargo bench",
    // JavaScript / TypeScript
    "npm test",
    "npm run test",
    "npm run lint",
    "npm run build",
    "npm run typecheck",
    "npm run check",
    "pnpm test",
    "pnpm lint",
    "pnpm build",
    "pnpm typecheck",
    "pnpm check",
    "pnpm run test",
    "pnpm run lint",
    "pnpm run build",
    "yarn test",
    "yarn lint",
    "yarn build",
    "bun test",
    "jest",
    "vitest",
    "mocha",
    "playwright test",
    "tsc",
    "eslint",
    "biome check",
    // Python
    "pytest",
    "python -m pytest",
    "python -m unittest",
    "tox",
    "nox",
    "ruff",
    "mypy",
    "flake8",
    // Go
    "go test",
    "go build",
    "go vet",
    // Everything else that says "verified"
    "make test",
    "make check",
    "make lint",
    "make build",
    "swift test",
    "xcodebuild",
    "gradle test",
    "./gradlew test",
    "mvn test",
    "dotnet test",
    "rspec",
    "phpunit",
    "shellcheck",
    "terraform validate",
];

/// One command, clipped to [`MAX_COMMAND`] on a char boundary.
fn clip(command: &str) -> String {
    let command = command.trim();
    if command.chars().count() <= MAX_COMMAND {
        return command.to_string();
    }
    let head: String = command.chars().take(MAX_COMMAND).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ran(command: &str, failed: bool) -> RanCommand {
        RanCommand {
            command: command.into(),
            failed,
        }
    }

    #[test]
    fn a_verification_command_is_recognised_through_the_shell_around_it() {
        assert!(is_check("cargo test -p comet-board"));
        // The shape agents actually write.
        assert!(is_check("cd crates/board && cargo test"));
        assert!(is_check("RUST_LOG=debug cargo test claims"));
        assert!(is_check("time cargo clippy --all-targets"));
        assert!(is_check("cargo test 2>&1 | tee .dev.log"));
        assert!(is_check("pnpm build; pnpm test"));
        // …and what is not a check, however much it looks like work.
        assert!(!is_check("git status"));
        assert!(!is_check("rg 'cargo test' docs/"));
        assert!(!is_check("ls target/debug"));
    }

    /// The far word boundary: a prefix match must not turn a neighbouring
    /// command into a check because it happens to start with the same letters.
    #[test]
    fn a_prefix_match_stops_at_a_word_boundary() {
        assert!(!is_check("cargo testbed --list"));
        assert!(!is_check("gopher build"));
        assert!(is_check("cargo test"));
    }

    #[test]
    fn identical_runs_of_one_command_are_one_row_with_a_count() {
        let e = gather(&[
            ran("cargo test -p comet-board", true),
            ran("cargo test -p comet-board", true),
            ran("cargo test -p comet-board", false),
            ran("git status", false),
        ]);
        assert_eq!(e.commands, 4, "the totals count everything");
        assert_eq!(e.failed, 2);
        assert_eq!(e.checks.len(), 1);
        assert_eq!(e.checks[0].runs, 3);
        assert_eq!(e.checks[0].failed, 2);
        assert!(e.checks[0].ever_passed(), "the third one passed");
        assert!(e.checked());
        assert_eq!(e.failing().count(), 0);
    }

    /// The list a review reads top-down: what failed is at the top, whatever
    /// order it ran in.
    #[test]
    fn failures_sort_above_everything_that_passed() {
        let e = gather(&[
            ran("cargo build", false),
            ran("cargo build", false),
            ran("cargo build", false),
            ran("cargo clippy", true),
        ]);
        assert_eq!(e.checks[0].command, "cargo clippy");
        assert_eq!(e.checks[1].command, "cargo build");
        assert_eq!(e.failing().count(), 1);
    }

    /// A busy run that never checked anything. The point of keeping the totals
    /// beside the list: the emptiness is a finding, not a gap in the record.
    #[test]
    fn a_run_that_checked_nothing_says_how_busy_it_was_anyway() {
        let e = gather(&[ran("git add -A", false), ran("rg foo", true)]);
        assert!(!e.checked());
        assert_eq!(e.commands, 2);
        assert_eq!(e.failed, 1);
    }

    #[test]
    fn a_capped_list_says_that_it_was_capped() {
        let many: Vec<RanCommand> = (0..MAX_CHECKS + 5)
            .map(|n| ran(&format!("cargo test -p crate{n}"), false))
            .collect();
        let e = gather(&many);
        assert_eq!(e.checks.len(), MAX_CHECKS);
        assert!(e.truncated);
        assert_eq!(e.commands as usize, MAX_CHECKS + 5, "the total is honest");
    }

    #[test]
    fn a_heredoc_sized_command_is_clipped_not_stored_whole() {
        let long = format!("pytest -k \"{}\"", "x".repeat(400));
        let e = gather(&[ran(&long, false)]);
        assert!(e.checks[0].command.chars().count() <= MAX_COMMAND + 1);
        assert!(e.checks[0].command.ends_with('…'));
    }

    /// The tests chip asks a narrower question than the evidence block, and
    /// every command that answers it must also be a check — or the two would
    /// read the same journal and disagree (§gh#236).
    #[test]
    fn a_test_runner_is_a_check_and_a_build_is_not_a_test_runner() {
        for command in TESTS {
            assert!(is_check(command), "{command} must be a check");
        }
        assert!(runs_tests("cd web && pnpm test -- --run"));
        assert!(runs_tests("RUST_LOG=debug cargo test claims"));
        assert!(!runs_tests("cargo build"));
        assert!(!runs_tests("pnpm build"));
        assert!(!runs_tests("cargo clippy --all-targets"));
        assert!(!runs_tests("git status"));
    }

    #[test]
    fn nothing_at_all_is_empty_evidence_rather_than_absent_evidence() {
        let e = gather(&[]);
        assert_eq!(e, RunEvidence::default());
        assert!(!e.checked());
    }

    // ---- artifacts (§gh#421) ----

    fn input(kind: ArtifactKind) -> ArtifactInput {
        ArtifactInput {
            kind,
            description: "signed-in dashboard, empty state".into(),
            url: Some("http://localhost:5173/".into()),
            viewport: Some("1440x900".into()),
        }
    }

    #[test]
    fn kinds_parse_from_the_spellings_agents_write() {
        for (spelling, kind) in [
            ("screenshot", ArtifactKind::Screenshot),
            ("SCREENSHOT", ArtifactKind::Screenshot),
            ("recording", ArtifactKind::Recording),
            ("a11y", ArtifactKind::Accessibility),
            ("accessibility", ArtifactKind::Accessibility),
            ("console", ArtifactKind::Console),
            ("log", ArtifactKind::Log),
        ] {
            assert_eq!(ArtifactKind::parse(spelling), Some(kind), "{spelling}");
        }
        assert_eq!(ArtifactKind::parse("png"), None);
        assert_eq!(ArtifactKind::parse(""), None);
    }

    #[test]
    fn a_valid_attachment_validates_and_the_refusals_name_their_bound() {
        assert!(validate_artifact(&input(ArtifactKind::Screenshot), 0, 1024).is_ok());

        let err = validate_artifact(&input(ArtifactKind::Screenshot), MAX_ARTIFACTS, 10)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max 8"), "{err}");

        let mut unlabelled = input(ArtifactKind::Screenshot);
        unlabelled.description = "   ".into();
        assert!(validate_artifact(&unlabelled, 0, 10).is_err());

        let mut long = input(ArtifactKind::Console);
        long.description = "x".repeat(MAX_DESCRIPTION + 1);
        let err = validate_artifact(&long, 0, 10).unwrap_err().to_string();
        assert!(err.contains("characters"), "{err}");

        let mut query = input(ArtifactKind::Screenshot);
        query.url = Some("http://localhost:5173/?token=secret".into());
        let err = validate_artifact(&query, 0, 10).unwrap_err().to_string();
        assert!(
            err.contains("query string") && err.contains("strip it"),
            "the refusal names the leak: {err}"
        );

        let mut bad_viewport = input(ArtifactKind::Screenshot);
        bad_viewport.viewport = Some("big".into());
        let err = validate_artifact(&bad_viewport, 0, 10)
            .unwrap_err()
            .to_string();
        assert!(err.contains("WxH"), "{err}");

        let err = validate_artifact(&input(ArtifactKind::Log), 0, 2 * 1024 * 1024)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cap for log"), "{err}");

        assert!(validate_artifact(&input(ArtifactKind::Recording), 0, 24 * 1024 * 1024).is_ok());
    }

    /// The URL field's one rule, applied by the board and not trusted from the
    /// caller: `strip_query` is what stores, so even if validation were
    /// bypassed the stored value cannot carry the token.
    #[test]
    fn the_stored_url_never_carries_a_query() {
        assert_eq!(
            strip_query("http://localhost:5173/settings?tab=billing"),
            "http://localhost:5173/settings"
        );
        assert_eq!(
            strip_query("http://localhost:5173/"),
            "http://localhost:5173/"
        );
    }

    #[test]
    fn identity_is_kind_plus_content_so_a_retry_dedupes() {
        let sha = fingerprint(b"pixels");
        assert_eq!(
            EvidenceArtifact::identity(ArtifactKind::Screenshot, &sha),
            format!("screenshot-{}", &sha[..8])
        );
        // Same bytes under another kind are a different artifact: they say
        // different things.
        assert_ne!(
            EvidenceArtifact::identity(ArtifactKind::Recording, &sha),
            EvidenceArtifact::identity(ArtifactKind::Screenshot, &sha)
        );
    }

    #[test]
    fn binary_formats_are_sniffed_not_asserted() {
        let png = b"\x89PNG\r\n\x1a\nrest".as_slice();
        assert_eq!(sniff_ext(png), Some("png"));
        assert_eq!(sniff_ext(b"\xff\xd8\xff\xe0jpeg"), Some("jpg"));
        assert_eq!(sniff_ext(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some("webp"));
        assert_eq!(sniff_ext(b"\x1a\x45\xdf\xa3matroska"), Some("webm"));
        assert_eq!(sniff_ext(b"\x00\x00\x00\x18ftypmp42"), Some("mp4"));
        assert_eq!(sniff_ext(b"GET /index.html 200"), None);
        // An unrecognized binary screenshot is stored as `.bin` rather than
        // being renamed to something it may not be.
        assert_eq!(ArtifactKind::Screenshot.default_ext(), "bin");
        assert_eq!(ArtifactKind::Console.default_ext(), "txt");
    }

    /// The provenance split, at the type level: everything an agent could
    /// invent round-trips through serde unchanged, and every field the board
    /// stamps is plain data on the struct — there is no `AgentReport` wrapper
    /// the board's copy could be confused with.
    #[test]
    fn an_artifact_round_trips_through_its_wire_shape() {
        let sha = fingerprint(b"pixels");
        let artifact = EvidenceArtifact {
            id: EvidenceArtifact::identity(ArtifactKind::Screenshot, &sha),
            kind: ArtifactKind::Screenshot,
            description: "signed-in dashboard".into(),
            url: Some(strip_query("http://localhost:5173/?token=x")),
            viewport: Some("1440x900".into()),
            attached_at: "2026-08-25T10:00:00Z".into(),
            commit_sha: Some("a1b2c3d4e5f6a7b8".into()),
            dirty_files: 3,
            bytes: 2048,
            sha256: sha,
            file: "screenshot-a1b2c3d4.png".into(),
        };
        let json = serde_json::to_string(&artifact).unwrap();
        assert!(json.contains("\"kind\":\"screenshot\""), "{json}");
        let back: EvidenceArtifact = serde_json::from_str(&json).unwrap();
        assert_eq!(back, artifact);
    }
}
