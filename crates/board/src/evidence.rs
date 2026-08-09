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

use serde::{Deserialize, Serialize};

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

    #[test]
    fn nothing_at_all_is_empty_evidence_rather_than_absent_evidence() {
        let e = gather(&[]);
        assert_eq!(e, RunEvidence::default());
        assert!(!e.checked());
    }
}
