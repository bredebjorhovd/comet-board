//! The claim contract, and the remainder it exists to expose (§gh#183).
//!
//! A summary written by the model that wrote the code inherits its blind spots:
//! a misunderstanding comes back described fluently and confidently, and prose
//! marks its own homework. So the board does not ask an agent for a summary. It
//! asks for **claims** — a sentence and the paths it is about — and then
//! computes, from the diff, the set of changes no claim accounts for.
//!
//! That remainder is the product. A dependency nobody mentioned and a function
//! edited in passing are exactly what a reader would have missed, and they are
//! the one part of a review that has to be derived rather than asserted: an
//! agent asked "what did you not account for" answers from the same blind spot
//! it wrote the code with.
//!
//! ## Three rules, and each one is load-bearing
//!
//! 1. **A claim without an anchor is not a claim.** [`parse`] refuses a line
//!    with no `::` and no paths after it, naming the line. The refusal is the
//!    contract: prose is free to exist, it just cannot be submitted here.
//! 2. **The anchors are matched, never trusted.** A path a claim names that the
//!    diff never touched comes back in
//!    [`Remainder::unmatched_anchors`](Remainder) — an anchor that matches
//!    nothing is a claim about work that did not happen, and it is at least as
//!    interesting as an unclaimed file.
//! 3. **The remainder comes off the diff.** Never off the claims, never off
//!    anything the agent said. [`remainder`] takes the changed set as an
//!    argument for exactly that reason: the caller reads it from git.
//!
//! ## The format
//!
//! One claim per line:
//!
//! ```text
//! <what you did> :: <anchor> [<anchor>…]
//! ```
//!
//! `::` is the separator and the **last** one on the line wins, because a
//! sentence about `Db::open` is a sentence somebody will write. Anchors are
//! separated by whitespace or commas.
//!
//! An anchor is a **path** or a **symbol** (§gh#235), told apart by how it is
//! spelled and never by a flag: anything with a `/` or a file extension is a
//! path, everything else is a symbol. Paths are repo-relative and may name a
//! directory — `crates/board/src/` accounts for every changed file under it,
//! which is the honest spelling for "I rewrote this module" and is visibly
//! coarser than naming the files. A symbol — `active_placements`,
//! `shelf_note` — accounts for every changed file whose diff added or removed
//! a line naming it, which is the anchor a reviewer actually has in mind when
//! the claim is about one function rather than one file.
//!
//! A leading `-` or `*` bullet is tolerated: agents write lists, and refusing
//! one over its punctuation teaches nothing.
//!
//! ## Where the block comes from
//!
//! `comet-board claim` is the interactive half — it answers with the
//! remainder, to the one party still able to do something about it. The other
//! half is [`harvest`]: an agent that finished without running the verb still
//! leaves its claims in a fenced ```` ```claims ```` block in what it said,
//! and the board reads them off the finished attempt rather than losing them
//! (§gh#235). Neither is required. An attempt that claimed nothing settles
//! exactly as it did before any of this existed — but a block that *was*
//! written and could not be parsed is reported, never dropped.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::evidence::RunEvidence;
use crate::model::{Attempt, Task};

/// What separates a claim's sentence from the paths it is about.
pub const ANCHOR: &str = "::";

/// How many claims one attempt may submit. Generous — a large branch honestly
/// described is a dozen — and here to bound a column the board reads on every
/// review, not to ration the contract.
pub const MAX_CLAIMS: usize = 100;

/// How long one claim's sentence may be. A claim is a sentence; a paragraph
/// here is prose wearing an anchor.
pub const MAX_TEXT: usize = 400;

/// The shortest thing that can be a symbol anchor (§gh#235). Two characters
/// name half a diff, and an anchor matching everything checks nothing.
pub const MIN_SYMBOL: usize = 3;

/// How many distinct symbols one changed file records. A bound on a column the
/// board writes on every reconcile — and truncation fails safe, because a
/// symbol that fell off the end comes back as an *unmatched* anchor rather than
/// as a silent match.
pub const MAX_FILE_SYMBOLS: usize = 200;

/// How much unified diff [`attach_symbols`] will read. A branch that moved more
/// than this is one where the remainder, not the symbol index, is the thing
/// worth reading.
pub const MAX_DIFF_BYTES: usize = 4 << 20;

/// One claim: a sentence, and the anchors it is about.
///
/// `files` and `symbols` are never *both* empty — [`parse`] is the only way to
/// build one from agent input and it refuses an anchorless claim outright, so
/// anything stored against an attempt is checkable by construction.
///
/// Two fields rather than one list of a tagged enum because the column is
/// already written: every claim recorded before §gh#235 is a `{text, files}`
/// object, and `symbols` defaulting to empty reads those rows back as exactly
/// what they were — path-anchored claims — with no migration and no guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub text: String,
    pub files: Vec<String>,
    /// Symbol anchors: a function, type or constant the change is about
    /// (§gh#235). Matched against the identifiers the diff's own changed lines
    /// name, so this is checked against git like every other anchor here.
    #[serde(default)]
    pub symbols: Vec<String>,
}

impl Claim {
    /// Every anchor this claim carries, in the order it was written. What a
    /// surface counts when it says how many things a claim is anchored to.
    pub fn anchors(&self) -> impl Iterator<Item = &String> {
        self.files.iter().chain(&self.symbols)
    }
}

/// Is an anchor a path or a symbol (§gh#235)?
///
/// Told from the spelling, deliberately, and never from a sigil: the design's
/// own reference claims are written `shell/spaces.rs` and `active_placements`,
/// and a format that made an agent declare which kind it meant would be a
/// format agents get wrong in a way nothing can detect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorKind {
    Path,
    Symbol,
}

/// The longest thing after a final `.` that still reads as a file extension.
/// `db.rs`, `Cargo.toml`, `schema.graphql` — past this it is punctuation in a
/// sentence, not a suffix.
const MAX_EXTENSION: usize = 9;

/// Which kind of anchor this is.
///
/// A `/` settles it. Otherwise a trailing `.ext` — short, alphanumeric, with
/// something in front of it — makes it a path, so `db.rs` stays the path it
/// obviously is (and stays subject to [`accounts_for`]'s refusal to match on a
/// bare filename). Everything else is a symbol.
pub fn anchor_kind(anchor: &str) -> AnchorKind {
    if anchor.contains('/') {
        return AnchorKind::Path;
    }
    match anchor.rsplit_once('.') {
        Some((stem, ext))
            if !stem.is_empty()
                && !ext.is_empty()
                && ext.len() <= MAX_EXTENSION
                && ext.chars().all(|c| c.is_ascii_alphanumeric()) =>
        {
            AnchorKind::Path
        }
        _ => AnchorKind::Symbol,
    }
}

/// One file the attempt's branch touched, as git reports it.
///
/// Renames are read with `--no-renames` on purpose: to a reviewer a rename is
/// two paths that both changed, and collapsing them to one hides the arrival of
/// the new name — which is precisely the kind of thing an unclaimed set is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    /// git's status letter: `A` added, `M` modified, `D` deleted.
    pub status: String,
    /// Lines added and removed. Both zero for a binary file, which git reports
    /// as `-` and this records as nothing rather than as an empty change.
    pub added: u32,
    pub removed: u32,
    /// A binary file, which has no line counts to give.
    #[serde(default)]
    pub binary: bool,
    /// Identifiers this file's diff added or removed a line naming — what a
    /// symbol anchor is matched against (§gh#235).
    ///
    /// Read off the unified diff by [`attach_symbols`], capped, and empty when
    /// the board never read one. Empty is therefore "not known", and it fails
    /// in the direction the rest of this module fails in: an unrecorded symbol
    /// makes an anchor come back *unmatched*, which is loud, rather than
    /// quietly accounting for a file nobody looked at.
    #[serde(default)]
    pub symbols: Vec<String>,
}

impl ChangedFile {
    /// How much moved, as a reader wants it: `+18 −2`, or `binary` for a file
    /// that has no lines to count. One spelling, because the CLI's column and
    /// the desktop's row are the same fact and reading two of them would be
    /// reading two files.
    pub fn counts(&self) -> String {
        if self.binary {
            return "binary".to_string();
        }
        format!("+{} −{}", self.added, self.removed)
    }
}

/// One claim, checked against the diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimView {
    pub text: String,
    /// The paths the claim named, as submitted.
    pub files: Vec<String>,
    /// The symbols the claim named, as submitted (§gh#235).
    #[serde(default)]
    pub symbols: Vec<String>,
    /// Changed files the claim's anchors — of either kind — account for.
    pub matched: Vec<String>,
    /// Anchors the claim named that the diff does not contain: a path nothing
    /// happened to, or a symbol no changed line names. Not an error and not
    /// dropped — a claim that cannot be checked is exactly the thing this
    /// screen exists to say out loud.
    pub unmatched: Vec<String>,
    /// How far each of this claim's *symbol* anchors reaches, counted in both
    /// trees (§gh#236). Empty on a path-anchored claim, and empty when there
    /// was no checkout to count in — which is why the chip built from it says
    /// nothing rather than saying zero.
    ///
    /// Filled after the fact by [`crate::sync::SyncEngine::review`]: counting
    /// call sites needs a repo, and [`remainder`] is pure on purpose.
    #[serde(default)]
    pub call_sites: Vec<crate::effects::CallSites>,
}

impl ClaimView {
    /// Did anything this claim named actually change? A claim where nothing
    /// did is the review's second-loudest row, after the unclaimed set.
    pub fn anchored(&self) -> bool {
        !self.matched.is_empty()
    }
}

/// The claims, and everything they do not account for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remainder {
    pub claims: Vec<ClaimView>,
    /// **The output.** Changed files no claim names, in the diff's own order.
    pub unclaimed: Vec<ChangedFile>,
    /// Paths claims named that the diff never touched, deduplicated across
    /// claims.
    pub unmatched_anchors: Vec<String>,
    /// How many changed files at least one claim accounts for. With
    /// `unclaimed.len()` this is the whole diff, so a surface can render the
    /// proportion without re-deriving it.
    pub claimed: usize,
}

impl Remainder {
    /// Is every changed file accounted for? The only state in which the claims
    /// list is the whole story.
    pub fn complete(&self) -> bool {
        self.unclaimed.is_empty()
    }
}

/// Where the changed set came from — and, when it could not be had, why.
///
/// A review that quietly rendered an empty diff would say "nothing changed"
/// about a collected checkout, which is the opposite of true. The variant
/// travels with the review so the surface can say which.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum DiffSource {
    /// Read from the attempt's checkout, just now.
    Checkout,
    /// The snapshot the board recorded while the attempt was still live. What
    /// answers after `gc` reclaims the worktree (gh#72) — the reason the
    /// snapshot is taken at all.
    Recorded,
    /// Neither: no checkout on disk and nothing recorded.
    Unavailable { reason: String },
}

/// What the agent was asked to do — the left-hand side of a review.
///
/// The issue, not the interpolated dispatch prompt: the prompt is a template
/// over exactly these fields plus the board's conventions, and it is not stored
/// anywhere an attempt can be read back from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brief {
    pub identifier: String,
    pub title: String,
    pub url: String,
    pub body: Option<String>,
}

/// Everything a review of one attempt is made of (§gh#183).
///
/// Assembled by [`review`] and printed by `comet-board review --json`. The
/// order of the fields is the order the question is asked in: what was asked,
/// what the agent says it did, what the board can see for itself, and what
/// nobody accounted for.
///
/// Serialized `snake_case` throughout, like [`crate::rows::TaskRow`] and unlike
/// the RPC *params* around it: the same orchestrating agents read `list --json`
/// and this, and one object changing case halfway through a CLI is a papercut
/// nobody should have to remember.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptReview {
    pub task_id: String,
    pub attempt: i64,
    /// Which attempt of the task this is, 1-based — a retry makes new claims,
    /// and "attempt 3" is how every other board surface names them.
    pub attempt_number: usize,
    pub state: String,
    pub outcome: Option<String>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub pr_url: Option<String>,
    pub brief: Brief,
    /// When the agent submitted its claims. `None` with an empty claims list
    /// means it never answered the contract at all, which is a different fact
    /// from claiming nothing and must not render as one.
    pub claimed_at: Option<String>,
    /// Why the block this attempt wrote could not be read (§gh#235).
    ///
    /// An agent that answered the contract badly is not the same as one that
    /// never answered it: the first wrote down what it did and the board threw
    /// it away, which is a fact about the board as much as about the agent.
    /// `None` on every attempt that never wrote a block — the ordinary case,
    /// and the quiet one.
    #[serde(default)]
    pub claims_error: Option<String>,
    #[serde(flatten)]
    pub remainder: Remainder,
    /// Every file the branch touched — the denominator behind the remainder.
    pub changed: Vec<ChangedFile>,
    pub diff: DiffSource,
    /// Files changed in the checkout but not committed, right now.
    ///
    /// Not part of the diff and not claimable: the remainder is about the
    /// **branch**, which is what a reviewer can fetch. It is reported because
    /// of what it means at submission time — an agent that claims before it
    /// commits would otherwise be told every one of its changes is accounted
    /// for, having shown the board none of them. `None` when there was no
    /// checkout to ask.
    pub uncommitted: Option<u32>,
    pub evidence: RunEvidence,
    /// What sandbox the run that produced all of the above actually had
    /// (§gh#349) — read off the harness's own report, never off the dispatch.
    ///
    /// `None` is "nobody said": an attempt from before harnesses reported it,
    /// and one whose journal the board could not read. Rendered as unknown, and
    /// never filled in from the level the board *asked* for — a requested level
    /// that turned out not to hold is the entire reason this field exists.
    #[serde(default)]
    pub sandbox: Option<comet_proto::SandboxReport>,
    /// What the board derived from the branch itself (§gh#236): the tests
    /// either side of it, the public surface, the schema, the config keys and
    /// the dependencies.
    ///
    /// Defaulted for every attempt recorded before it existed, and the default
    /// is [`crate::effects::Effects::read`]`= false` — which renders as "not
    /// read" rather than as five clean results.
    #[serde(default)]
    pub effects: crate::effects::Effects,
}

impl AttemptReview {
    /// Did the agent answer the claim contract at all?
    pub fn claimed(&self) -> bool {
        self.claimed_at.is_some()
    }
}

// ---- the format ----------------------------------------------------------

/// Parse a submitted block into claims, refusing anything without an anchor.
///
/// `worktree` is the attempt's checkout, used only to make an absolute path
/// repo-relative — an agent that pastes the path it has been typing all session
/// has anchored its claim correctly, and rejecting it would be pedantry about a
/// prefix the board itself handed out.
///
/// Every refusal names the line. A contract enforced with "invalid input" is a
/// contract nobody can satisfy on the second try.
pub fn parse(input: &str, worktree: Option<&str>) -> Result<Vec<Claim>> {
    let mut claims = Vec::new();
    for raw in input.lines() {
        let line = raw
            .trim()
            .trim_start_matches(['-', '*', '•'])
            .trim_start_matches(char::is_whitespace);
        if line.is_empty() {
            continue;
        }
        // The LAST separator: `Db::open` in the sentence is ordinary, a `::` in
        // a path is not.
        let Some(split) = line.rfind(ANCHOR) else {
            bail!(
                "no `{ANCHOR}` in: {line}\n\
                 A claim is a sentence and the paths it is about — \
                 `what you did {ANCHOR} path/one.rs path/two.rs`. \
                 Without an anchor it is prose, and prose is not checkable."
            );
        };
        let text = line[..split].trim();
        let mut files: Vec<String> = Vec::new();
        let mut symbols: Vec<String> = Vec::new();
        for anchor in line[split + ANCHOR.len()..]
            .split([',', ' ', '\t'])
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            // Normalized first: the quotes and the worktree prefix an agent
            // pastes are noise on both kinds of anchor, and `active_placements`
            // survives it unchanged.
            let anchor = normalize(anchor, worktree);
            if anchor.is_empty() {
                continue;
            }
            match anchor_kind(&anchor) {
                AnchorKind::Path => files.push(anchor),
                AnchorKind::Symbol if anchor.chars().count() < MIN_SYMBOL => bail!(
                    "`{anchor}` is too short to anchor anything (symbols are {MIN_SYMBOL}+ \
                     characters): {line}\n\
                     A one- or two-letter symbol names half the diff, and an anchor that \
                     matches everything checks nothing."
                ),
                AnchorKind::Symbol => symbols.push(anchor),
            }
        }
        if text.is_empty() {
            bail!("a claim with anchors and nothing said about them: {line}");
        }
        if files.is_empty() && symbols.is_empty() {
            bail!(
                "no anchors after `{ANCHOR}` in: {line}\n\
                 Name the files or the symbols the claim is about; a claim nothing \
                 anchors cannot be checked against the diff."
            );
        }
        if text.chars().count() > MAX_TEXT {
            bail!(
                "a claim of {} characters is a paragraph, not a claim (max {MAX_TEXT}): {}…",
                text.chars().count(),
                text.chars().take(60).collect::<String>()
            );
        }
        claims.push(Claim {
            text: text.to_string(),
            files: dedup(files),
            symbols: dedup(symbols),
        });
    }
    if claims.is_empty() {
        bail!(
            "nothing to record. One claim per line: \
             `what you did {ANCHOR} path/one.rs path/two.rs`."
        );
    }
    if claims.len() > MAX_CLAIMS {
        bail!(
            "{} claims is more than one attempt can be about (max {MAX_CLAIMS})",
            claims.len()
        );
    }
    Ok(claims)
}

// ---- the ask, in the brief itself (§gh#339) -------------------------------

/// The paragraph every dispatch brief ends with: say what you did, here is the
/// verb, here is the exact task to name (§gh#339).
///
/// The contract had three channels to reach an agent through and none of them
/// was the one that always arrives. The skill is *discovered* — Claude Code
/// reads it when the agent decides it is relevant, and an agent handed a
/// ticket, a branch and a finish sequence has no reason to decide that. The
/// instruction file (gh#272) is better and still a standing rule in a file,
/// competing with everything else in it. The brief is neither: it is the one
/// text the board hands to every dispatched agent of every runtime, and until
/// this it asked for a commit, a push and a pull request and never once
/// mentioned claims. Twenty-three settled attempts on the box ran the verb
/// zero times and wrote a block zero times, with the skill installed the whole
/// while. So the ask goes where the work is asked for.
///
/// It names `task_id` verbatim because that is the argument, and because the
/// agent has no other way to learn it: nothing exports it into the run's
/// environment, and the identifier the rest of the brief uses (`gh#339`) is a
/// different string from the id (`gh:owner/repo#339`). Both spellings are
/// accepted now ([`crate::dispatch::task_by_reference`]), which is the other
/// half of this fix — but the one printed here is the one that cannot be
/// ambiguous.
///
/// Appended after interpolation by [`crate::dispatch::resolve_prompt`], for the
/// reason the base sentence is: a route's own `prompt` is somebody's wording
/// for the *task*, and the contract is a fact about being dispatched at all.
///
/// The fallback fence is *named* rather than spelled, so this text contains no
/// [`BLOCK_TAG`] fence of its own: an agent that quotes its brief back in a
/// closing message would otherwise hand [`find_block`] the board's own
/// instruction to read as that agent's answer to it.
pub fn brief(task_id: &str) -> String {
    format!(
        "\n\nBefore you say you are done, tell the board what you changed — in \
         **claims**, one per line, each anchored to the files or symbols it is \
         about:\
         \n\n```\
         \ncomet-board claim --task {task_id} <<'EOF'\
         \n<what you did> {ANCHOR} <anchor> [<anchor>…]\
         \nEOF\
         \n```\
         \n\nAn anchor is a **path** — anything with a `/` or a file extension, \
         repo-relative, and a directory accounts for everything under it — or a \
         **symbol**, a function or type or constant the change is about, which \
         accounts for every changed file whose diff names it. A line with no \
         `{ANCHOR}`, or nothing after it, is refused: without an anchor a claim \
         cannot be checked, and an unanchored summary is the thing this replaces.\
         \n\nThe reply is the part worth reading. The board diffs your branch and \
         prints every changed file no claim accounts for — computed from git, not \
         from what you wrote, so it is where the dependency you bumped and the \
         function you edited in passing turn up. Run it after your last commit, \
         read what comes back, and either claim those changes or go and look at \
         them.\
         \n\nIf you finish without running the verb, write the same lines into a \
         fenced block tagged `{BLOCK_TAG}` in your closing message and the board \
         reads them off the attempt. Claiming nothing breaks nothing — the \
         attempt settles and the pull request opens as they would anyway, and \
         the review says the contract went unanswered."
    )
}

// ---- the block, off the finished attempt (§gh#235) ------------------------

/// The fence an agent writes its claims in when it is finishing rather than
/// running a verb.
pub const BLOCK_TAG: &str = "claims";

/// What a finished attempt's own words amounted to (§gh#235).
///
/// Three answers and not two, because "said nothing" and "said something the
/// board could not read" must not settle to the same row. The first is the
/// behaviour every attempt had before this existed; the second is a fact about
/// this attempt that somebody has to see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Harvest {
    /// No `claims` block anywhere in what the agent said. The attempt is
    /// claimless and settles exactly as it always did.
    None,
    /// A block that parsed.
    Claims(Vec<Claim>),
    /// A block that did not parse, and why — in [`parse`]'s own words, which
    /// name the offending line.
    Malformed(String),
}

/// The body of the last ```` ```claims ```` block in `text`, if there is one.
///
/// The **last**, on [`Db::set_attempt_claims`]'s rule: an agent that wrote the
/// block twice is correcting itself, and reading the superseded one would be
/// reviewing a draft. Anything after the tag on the fence line is ignored, and
/// an unterminated block runs to the end of the text — a message truncated
/// mid-fence still carries claims, and refusing them over a missing ``` would
/// teach nobody anything.
///
/// [`Db::set_attempt_claims`]: crate::db::Db::set_attempt_claims
pub fn find_block(text: &str) -> Option<&str> {
    let mut found: Option<(usize, usize)> = None;
    // `None` outside any fence; `Some(None)` inside somebody else's fenced
    // block — a shell snippet, a diff — which is skipped whole so a `claims`
    // fence quoted inside one is not read as ours; `Some(Some(start))` inside
    // a claims block whose body begins at `start`.
    let mut open: Option<Option<usize>> = None;
    let mut cursor = 0usize;
    for line in text.split_inclusive('\n') {
        let start = cursor;
        cursor += line.len();
        let trimmed = line.trim();
        let Some(rest) = trimmed
            .strip_prefix("```")
            .or_else(|| trimmed.strip_prefix("~~~"))
        else {
            continue;
        };
        match open {
            // A closing fence. Whatever is written on it is decoration.
            Some(body) => {
                if let Some(body) = body {
                    found = Some((body, start));
                }
                open = None;
            }
            None if rest.trim().eq_ignore_ascii_case(BLOCK_TAG) => open = Some(Some(cursor)),
            None => open = Some(None),
        }
    }
    // An unterminated `claims` fence still counts, to the end of the text.
    if let Some(Some(body)) = open {
        found = Some((body, text.len()));
    }
    found.map(|(a, b)| text[a..b].trim_matches('\n'))
}

/// Read an agent's claims off what it said when it finished (§gh#235).
///
/// The non-interactive half of the contract. `comet-board claim` is better —
/// it answers with the remainder while the agent can still act on it — but an
/// agent that finished without running it has still written down what it did,
/// and losing that is a worse outcome than reading it late.
///
/// Never an error to the caller: a claimless attempt is [`Harvest::None`] and
/// settles as it always did, and a block that will not parse comes back as
/// [`Harvest::Malformed`] to be stored and shown. Nothing here may stand
/// between a finished run and its PR.
pub fn harvest(text: &str, worktree: Option<&str>) -> Harvest {
    let Some(block) = find_block(text) else {
        return Harvest::None;
    };
    if block.trim().is_empty() {
        return Harvest::None;
    }
    match parse(block, worktree) {
        Ok(claims) => Harvest::Claims(claims),
        Err(e) => Harvest::Malformed(format!("{e:#}")),
    }
}

/// A submitted path as the diff spells it: repo-relative, forward slashes, no
/// `./` and no trailing separator.
pub fn normalize(path: &str, worktree: Option<&str>) -> String {
    let path = path.trim().trim_matches(['`', '"', '\'']);
    let path = path.replace('\\', "/");
    let relative = worktree
        .map(|w| w.trim_end_matches('/'))
        .filter(|w| !w.is_empty())
        .and_then(|w| path.strip_prefix(w))
        .map(|rest| rest.trim_start_matches('/'))
        .unwrap_or(&path);
    relative
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn dedup(mut paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));
    paths
}

// ---- the remainder -------------------------------------------------------

/// Does this claim path account for this changed file?
///
/// Exact match, or a directory the file sits under. Deliberately **not** a
/// basename match: an agent that writes `db.rs` for `crates/board/src/db.rs`
/// has not anchored anything — three crates here have a `db.rs` — and the
/// generous reading would silently claim files nobody looked at, which is the
/// whole failure this design is built around.
pub fn accounts_for(claim_path: &str, changed: &str) -> bool {
    if claim_path.is_empty() {
        return false;
    }
    if claim_path == changed {
        return true;
    }
    changed
        .strip_prefix(claim_path)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Does this changed file's diff name this symbol (§gh#235)?
///
/// Exact, case-sensitive, over the identifiers [`attach_symbols`] read off the
/// file's own added and removed lines. Not a substring test and not a fuzzy
/// one: `note` must not answer for `shelf_note`, for the same reason `db.rs`
/// does not answer for `crates/board/src/db.rs`.
///
/// A symbol on a changed line is positive evidence about *that* file, which is
/// what a path anchor could never be — and it is evidence the agent did not
/// author, which is the whole rule. What it does not claim to be is a
/// definition: a rename touches every call site, and a review that listed only
/// the file holding the `fn` would be hiding the other eleven. Every file a
/// symbol reaches is printed under the claim, so an anchor that swept more than
/// its author meant is visible in the same place its matches are.
pub fn names_symbol(symbol: &str, file: &ChangedFile) -> bool {
    !symbol.is_empty() && file.symbols.iter().any(|s| s == symbol)
}

/// Record, on each changed file, the identifiers its diff added or removed a
/// line naming — the index a symbol anchor is checked against (§gh#235).
///
/// `diff` is a unified diff of the same range the changed set came from, read
/// from git by the caller. Read with no language knowledge at all, on purpose:
/// a per-language parser is what §gh#183 named as a follow-up rather than
/// half-built, and a lexical scan of the lines that actually moved is both
/// honest about what it knows and the same for Rust, Swift, TOML and the shell
/// script somebody will add next.
///
/// Bounded twice — [`MAX_DIFF_BYTES`] of input, [`MAX_FILE_SYMBOLS`] per file —
/// because this is written into the attempt row on every reconcile. Both bounds
/// lose symbols, and losing one costs an anchor its match, which the review
/// reports. Neither can invent one.
pub fn attach_symbols(files: &mut [ChangedFile], diff: &str) {
    let by_path: std::collections::HashMap<String, usize> = files
        .iter()
        .enumerate()
        .map(|(ix, f)| (f.path.clone(), ix))
        .collect();
    // Deduplicated per file as we go: a diff repeats an identifier on every
    // line it touched, and the column stores the set.
    let mut seen: std::collections::HashSet<(usize, &str)> = Default::default();
    let mut current: Option<usize> = None;
    let mut read = 0usize;
    for line in diff.lines() {
        read += line.len() + 1;
        if read > MAX_DIFF_BYTES {
            break;
        }
        // `+++ b/path` names the file the following hunks are about. A deleted
        // file's is `/dev/null`, so `--- a/path` is the fallback and not the
        // primary: on every other file the two agree and the `+++` is the one
        // that is right about a rename.
        if let Some(path) = line.strip_prefix("+++ ") {
            current = header_path(path).and_then(|p| by_path.get(p).copied());
            continue;
        }
        if let Some(path) = line.strip_prefix("--- ") {
            if let Some(p) = header_path(path) {
                current = by_path.get(p).copied();
            }
            continue;
        }
        let Some(ix) = current else { continue };
        let Some(body) = line
            .strip_prefix('+')
            .or_else(|| line.strip_prefix('-'))
            .filter(|_| !line.starts_with("+++") && !line.starts_with("---"))
        else {
            continue;
        };
        for ident in identifiers(body) {
            if files[ix].symbols.len() >= MAX_FILE_SYMBOLS {
                break;
            }
            if seen.insert((ix, ident)) {
                files[ix].symbols.push(ident.to_string());
            }
        }
    }
}

/// The path out of a `+++ b/crates/board/src/db.rs` header, or `None` for
/// `/dev/null` and for the `+++` of a diff that carries no path.
///
/// Shared with [`crate::effects`], which walks the same `-U0` diff for a
/// different question (§gh#236): two readers of one diff format must agree
/// about which file a hunk belongs to, and the way to guarantee that is one
/// function.
pub(crate) fn header_path(header: &str) -> Option<&str> {
    let path = header.split('\t').next()?.trim();
    if path.is_empty() || path == "/dev/null" {
        return None;
    }
    Some(
        path.strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path),
    )
}

/// The identifiers on one changed line: runs of `[A-Za-z0-9_]` that start with
/// a letter or an underscore and are long enough to anchor anything.
///
/// Not tokenized per language and not filtered by keyword. A keyword anchor is
/// nonsense that matches most of the diff, and the review prints every file an
/// anchor reached — so a useless anchor is visible rather than quietly
/// excluded by a list that would be wrong for the next language anyway.
fn identifiers(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| {
            w.chars().count() >= MIN_SYMBOL
                && w.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        })
}

/// Map claims onto the diff and return what they do not account for.
///
/// `changed` is the branch's diff, read from git by the caller — never from
/// anything the agent submitted. That separation is the point of the whole
/// module: an agent cannot widen its own denominator.
pub fn remainder(claims: &[Claim], changed: &[ChangedFile]) -> Remainder {
    let mut views = Vec::with_capacity(claims.len());
    let mut accounted = vec![false; changed.len()];
    let mut unmatched_anchors: Vec<String> = Vec::new();

    for claim in claims {
        let mut matched = Vec::new();
        let mut unmatched = Vec::new();
        // Both kinds of anchor, resolved the same way: each one either names
        // changed files or names nothing, and naming nothing is reportable
        // rather than fatal.
        for (anchor, is_path) in claim
            .files
            .iter()
            .map(|a| (a, true))
            .chain(claim.symbols.iter().map(|a| (a, false)))
        {
            let hits: Vec<String> = changed
                .iter()
                .enumerate()
                .filter(|(_, f)| {
                    if is_path {
                        accounts_for(anchor, &f.path)
                    } else {
                        names_symbol(anchor, f)
                    }
                })
                .map(|(ix, f)| {
                    accounted[ix] = true;
                    f.path.clone()
                })
                .collect();
            if hits.is_empty() {
                unmatched.push(anchor.clone());
                if !unmatched_anchors.contains(anchor) {
                    unmatched_anchors.push(anchor.clone());
                }
            } else {
                for hit in hits {
                    if !matched.contains(&hit) {
                        matched.push(hit);
                    }
                }
            }
        }
        views.push(ClaimView {
            text: claim.text.clone(),
            files: claim.files.clone(),
            symbols: claim.symbols.clone(),
            matched,
            unmatched,
            call_sites: Vec::new(),
        });
    }

    let unclaimed: Vec<ChangedFile> = changed
        .iter()
        .zip(&accounted)
        .filter(|(_, done)| !**done)
        .map(|(f, _)| f.clone())
        .collect();
    Remainder {
        claims: views,
        claimed: changed.len() - unclaimed.len(),
        unclaimed,
        unmatched_anchors,
    }
}

/// Assemble the whole review from the pieces the caller has gathered.
///
/// Pure, and takes the diff rather than reading it, so the ranking and the
/// remainder are testable without a checkout — the same split
/// [`crate::settled::decide`] makes for the settle hierarchy.
///
/// The parameter list is long because the *review* is: each argument is one
/// independently-sourced fact about the attempt (the branch, the checkout, the
/// journal), and bundling them into a struct here would only move the same list
/// one call up while hiding which caller failed to supply which.
#[allow(clippy::too_many_arguments)]
pub fn review(
    task: &Task,
    attempt: &Attempt,
    changed: Vec<ChangedFile>,
    diff: DiffSource,
    uncommitted: Option<u32>,
    evidence: RunEvidence,
    effects: crate::effects::Effects,
    sandbox: Option<comet_proto::SandboxReport>,
) -> AttemptReview {
    let remainder = remainder(&attempt.claims, &changed);
    AttemptReview {
        task_id: task.id.clone(),
        attempt: attempt.id,
        attempt_number: task
            .attempts
            .iter()
            .position(|a| a.id == attempt.id)
            .map(|ix| ix + 1)
            // An attempt read on its own, outside its task's list. Its own id
            // is not a count, so saying nothing beats saying a wrong number.
            .unwrap_or(0),
        state: task.state.as_str().to_string(),
        outcome: attempt.outcome.map(|o| o.as_str().to_string()),
        branch: attempt.branch.clone(),
        worktree: attempt.worktree.clone(),
        pr_url: task.pr_url.clone(),
        brief: Brief {
            identifier: task.identifier.clone(),
            title: task.title.clone(),
            url: task.url.clone(),
            body: task
                .body
                .as_deref()
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(str::to_string),
        },
        claimed_at: attempt.claims_at.clone(),
        claims_error: attempt.claims_error.clone(),
        remainder,
        changed,
        diff,
        uncommitted,
        evidence,
        sandbox,
        effects,
    }
}

// ---- the reading ---------------------------------------------------------
//
// Everything from here down turns a finished [`AttemptReview`] into the
// sentences a surface says about it. It lives here, beside the thing it reads,
// for the reason every other derivation in this workspace does: the CLI, the
// desktop review screen and whatever comes after it must not each invent their
// own arithmetic for "is this review alarming", or the same attempt would read
// as fine in one window and wrong in another.
//
// The rule these functions encode is one sentence: **a review is alarming when
// the board can see something the agent did not account for.** Everything the
// agent asserted is checked against something it did not author — the diff, the
// run journal — and only the mismatches are loud.

/// How loudly a surface should say what a review amounts to.
///
/// Three states and not two, because "nothing is wrong" and "nothing was
/// established" are the opposite of the same thing. An attempt that never
/// answered the claim contract has no findings against it and has also proved
/// nothing, and a surface that painted that green would be reporting an absence
/// of evidence as evidence of absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// The board can see something nobody accounted for. The one tone this
    /// screen is allowed to shout in.
    Alarm,
    /// Nothing was established: no claims were submitted, or the diff could not
    /// be read at all.
    Unknown,
    /// The claims account for the whole diff and nothing contradicts them.
    Settled,
}

impl Tone {
    /// Should a surface make this loud? Only [`Tone::Alarm`] — an unknown
    /// review is quiet on purpose, because there is nothing in it to point at.
    pub fn loud(self) -> bool {
        matches!(self, Tone::Alarm)
    }
}

/// Which fact a [`Finding`] is. Surfaces switch on this rather than on the
/// sentence, so the wording can change without a renderer changing meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// Changed files no claim names. **The product** — first in the list and
    /// first on the screen, because it is the only finding here that is derived
    /// rather than asserted.
    Unclaimed,
    /// Work in the checkout that is on no branch, and so has been shown to
    /// nobody. Not part of the remainder — the remainder is about the branch —
    /// but it is what would make an empty remainder a lie.
    Uncommitted,
    /// A claim anchored to paths the diff never touched: work described that
    /// did not happen.
    UnsupportedClaims,
    /// A verification command that ran and never once passed.
    NeverPassed,
    /// Commands ran; none of them checked anything.
    Unchecked,
    /// The attempt never answered the claim contract. Distinct from claiming
    /// nothing, and it must never render as one.
    NeverClaimed,
    /// The attempt wrote a claims block the board could not parse (§gh#235).
    /// Louder than [`FindingKind::NeverClaimed`] and deliberately so: silence
    /// is an absence of evidence, and a refused block is a description of the
    /// work that exists and that nobody can check.
    MalformedClaims,
    /// There is no diff to read: no checkout on disk and nothing recorded.
    NoDiff,
}

impl FindingKind {
    /// Does this one mean somebody has to look, or only that nothing is known?
    pub fn tone(self) -> Tone {
        match self {
            FindingKind::Unclaimed
            | FindingKind::Uncommitted
            | FindingKind::UnsupportedClaims
            | FindingKind::NeverPassed
            | FindingKind::MalformedClaims
            | FindingKind::Unchecked => Tone::Alarm,
            FindingKind::NeverClaimed | FindingKind::NoDiff => Tone::Unknown,
        }
    }
}

/// How much a claim is standing on (§gh#236), in one glyph.
///
/// Three, because a claim has three genuinely different states and a screen
/// that drew two of them alike would be doing the flattening this whole design
/// exists to undo. The middle one is the reason it is not a boolean: a claim
/// the diff supports but nothing checks is neither contradicted nor verified,
/// and it must not be allowed to borrow either look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimMark {
    /// Nothing this claim anchored moved. `!`, in the review's one loud hue.
    Contradicted,
    /// Something the agent did not author supports it: a new test that passes,
    /// a call-site count that moved. `✓`.
    Corroborated,
    /// The anchors matched, and nothing corroborates it. `·`, quiet.
    Bare,
}

impl ClaimMark {
    /// The glyph itself, so the CLI and the desktop cannot pick different ones.
    pub fn glyph(self) -> &'static str {
        match self {
            ClaimMark::Contradicted => "!",
            ClaimMark::Corroborated => "✓",
            ClaimMark::Bare => "·",
        }
    }
}

/// One thing a review has to say out loud, already counted and phrased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub kind: FindingKind,
    pub text: String,
}

/// What the whole review amounts to, in one line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub tone: Tone,
    pub text: String,
}

/// The first line of a refusal, for a verdict bar that gets one line.
///
/// [`parse`]'s messages are two paragraphs — the offending line, then what the
/// format is — because the agent reading them is about to try again. A reviewer
/// reading them is not, and the whole text is still on the row underneath.
fn first_line(message: &str) -> &str {
    message.lines().next().unwrap_or(message).trim()
}

/// `1 file` / `2 files` — said the same way everywhere rather than `file(s)`.
fn count(n: usize, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("{n} {singular}")
    } else {
        format!("{n} {plural}")
    }
}

impl AttemptReview {
    /// Everything the board can see that the agent did not account for,
    /// loudest first.
    ///
    /// Ordered, not sorted: the sequence is the argument. The remainder leads
    /// because it is the only entry derived from something the agent never
    /// touched; uncommitted work follows because it is the one fact that can
    /// make an *empty* remainder wrong; then the claims that the diff refuses
    /// to support, then what did and did not get checked.
    ///
    /// Empty is a real answer, and [`Self::verdict`] is what says so.
    pub fn findings(&self) -> Vec<Finding> {
        let mut out = Vec::new();
        // No diff means no denominator: every other finding below would be
        // computed against an empty changed set and would read as good news.
        if let DiffSource::Unavailable { reason } = &self.diff {
            out.push(Finding {
                kind: FindingKind::NoDiff,
                text: format!("there is no diff to check these claims against: {reason}"),
            });
            return out;
        }
        let changed = self.changed.len();
        // Before the never-claimed line, and instead of it: this attempt did
        // answer, and what it wrote is sitting on the row unread. Reported
        // rather than dropped is the whole of §gh#235's second half.
        if let Some(err) = &self.claims_error {
            out.push(Finding {
                kind: FindingKind::MalformedClaims,
                text: format!(
                    "this attempt wrote a claims block the board could not read, \
                     so nothing accounts for its {}: {}",
                    count(changed, "changed file", "changed files"),
                    first_line(err)
                ),
            });
        } else if !self.claimed() {
            out.push(Finding {
                kind: FindingKind::NeverClaimed,
                text: format!(
                    "this attempt never answered the claim contract, so nothing accounts for its {}",
                    count(changed, "changed file", "changed files")
                ),
            });
        } else if !self.remainder.unclaimed.is_empty() {
            // Said as a proportion on purpose: "4 unclaimed" is a number, "4 of
            // 17" is the question of whether the summary covered the work.
            out.push(Finding {
                kind: FindingKind::Unclaimed,
                text: format!(
                    "{} of {changed} changed files are claimed by nobody",
                    self.remainder.unclaimed.len()
                ),
            });
        }
        if let Some(n) = self.uncommitted.filter(|n| *n > 0) {
            out.push(Finding {
                kind: FindingKind::Uncommitted,
                text: format!(
                    "{} changed in the checkout and on no branch, so the diff above has never seen them",
                    count(n as usize, "file is", "files are")
                ),
            });
        }
        let unsupported = self
            .remainder
            .claims
            .iter()
            .filter(|c| !c.anchored())
            .count();
        if unsupported > 0 {
            out.push(Finding {
                kind: FindingKind::UnsupportedClaims,
                text: format!(
                    "{} anchored to files nothing happened to",
                    count(unsupported, "claim is", "claims are")
                ),
            });
        }
        let never = self.evidence.failing().count();
        if never > 0 {
            out.push(Finding {
                kind: FindingKind::NeverPassed,
                text: format!(
                    "{} ran and never once passed",
                    count(never, "check", "checks")
                ),
            });
        } else if !self.evidence.checked() && self.evidence.commands > 0 {
            out.push(Finding {
                kind: FindingKind::Unchecked,
                text: format!(
                    "nothing that verifies anything ran, across {}",
                    count(self.evidence.commands as usize, "command", "commands")
                ),
            });
        }
        out
    }

    /// The terms this attempt ran on, when they are worth a reviewer's
    /// attention (§gh#349): "this agent had full access to the box", and why.
    ///
    /// `None` covers the two quiet cases — the run got the sandbox it was
    /// dispatched with and that sandbox was not full access, or no harness ever
    /// said and there is nothing to report but a guess.
    ///
    /// **Deliberately not a [`Finding`]**, for [`Self::effect_chips`]'s reason
    /// and one that is sharper here. Two of the three runtimes apply no sandbox
    /// at all, so this line is true of most attempts on the board; routed
    /// through `findings` it would raise [`Tone::Alarm`] on nearly every review
    /// and the alarm would stop meaning anything within a week. It is a
    /// standing condition of the run, not something nobody accounted for — a
    /// caveat on how much the rest of the screen is worth, which is exactly
    /// where a reviewer should meet it.
    pub fn sandbox_note(&self) -> Option<String> {
        self.sandbox.as_ref()?.note()
    }

    /// The effects row (§gh#236): what the board found in the branch itself,
    /// beside what the agent said about it.
    ///
    /// Deliberately **not** folded into [`Self::findings`]. A new dependency
    /// and an unrun suite are things to look at, not things nobody accounted
    /// for, and the verdict bar's one loud voice belongs to the remainder — a
    /// screen where three blocks shout has nothing left to shout with.
    pub fn effect_chips(&self) -> Vec<crate::effects::Chip> {
        self.effects.chips(&self.evidence)
    }

    /// The evidence chips under one claim: what the board found in the files
    /// that claim's own anchors reached.
    pub fn claim_chips(&self, claim: &ClaimView) -> Vec<crate::effects::Chip> {
        crate::effects::claim_chips(
            &claim.matched,
            &claim.call_sites,
            &self.effects,
            crate::effects::test_run(&self.evidence),
        )
    }

    /// Which glyph one claim is drawn with, in every surface that draws claims.
    pub fn claim_mark(&self, claim: &ClaimView) -> ClaimMark {
        if !claim.anchored() {
            return ClaimMark::Contradicted;
        }
        if crate::effects::corroborated(&self.claim_chips(claim)) {
            ClaimMark::Corroborated
        } else {
            ClaimMark::Bare
        }
    }

    /// The one line a surface leads with.
    ///
    /// The loudest finding, or — when there is none — the sentence that says
    /// the claims covered the diff. Never silent: a review with nothing to
    /// report still has to say what it checked, or an empty screen reads as a
    /// screen that failed to load.
    pub fn verdict(&self) -> Verdict {
        match self.findings().into_iter().next() {
            Some(finding) => Verdict {
                tone: finding.kind.tone(),
                text: finding.text,
            },
            None if self.changed.is_empty() => Verdict {
                tone: Tone::Unknown,
                text: "this attempt's branch changed nothing".to_string(),
            },
            None if self.changed.len() == 1 => Verdict {
                tone: Tone::Settled,
                text: "the one changed file is accounted for".to_string(),
            },
            None => Verdict {
                tone: Tone::Settled,
                text: format!("all {} changed files are accounted for", self.changed.len()),
            },
        }
    }
}

// ---- reading the diff ----------------------------------------------------

/// The changed set from `git diff --numstat` + `--name-status` output.
///
/// Two commands rather than one because git will not print both at once, and
/// the review wants both: the status letter says whether a file arrived or
/// left, and the line counts are what make an unclaimed row readable at a
/// glance. The numstat is authoritative for membership — a path only
/// name-status knows about is still listed, with no counts, rather than
/// dropped.
pub fn parse_diff(numstat: &str, name_status: &str) -> Vec<ChangedFile> {
    let mut statuses: std::collections::HashMap<&str, String> = Default::default();
    for line in name_status.lines() {
        let mut parts = line.split('\t');
        let (Some(status), Some(path)) = (parts.next(), parts.next_back()) else {
            continue;
        };
        let status = status.trim();
        if status.is_empty() || path.is_empty() {
            continue;
        }
        statuses.insert(path, status.to_string());
    }
    let mut out = Vec::new();
    for line in numstat.lines() {
        let mut parts = line.split('\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        // git prints `-` for both counts on a binary file.
        let binary = added == "-" || removed == "-";
        out.push(ChangedFile {
            path: path.to_string(),
            status: statuses
                .get(path)
                .cloned()
                .unwrap_or_else(|| "M".to_string()),
            added: added.parse().unwrap_or(0),
            removed: removed.parse().unwrap_or(0),
            binary,
            // Filled by `attach_symbols` from a unified diff of the same range,
            // which is a third `git diff` and so the caller's to run.
            symbols: Vec::new(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{Check, RanCommand};
    use crate::model::{BoardState, Outcome, Source, UpstreamState};

    fn changed(path: &str) -> ChangedFile {
        ChangedFile {
            path: path.into(),
            status: "M".into(),
            added: 10,
            removed: 2,
            binary: false,
            symbols: Vec::new(),
        }
    }

    /// The same file, with the identifiers a symbol anchor is matched against.
    fn changed_naming(path: &str, symbols: &[&str]) -> ChangedFile {
        ChangedFile {
            symbols: symbols.iter().map(|s| s.to_string()).collect(),
            ..changed(path)
        }
    }

    // ---- the format ------------------------------------------------------

    #[test]
    fn a_claim_is_a_sentence_and_the_paths_it_is_about() {
        let claims = parse(
            "Claims are stored against the attempt :: crates/board/src/db.rs, \
             crates/board/src/model.rs",
            None,
        )
        .unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].text, "Claims are stored against the attempt");
        assert_eq!(
            claims[0].files,
            ["crates/board/src/db.rs", "crates/board/src/model.rs"]
        );
    }

    /// The refusal *is* the contract: anything without an anchor is prose, and
    /// prose is what this design exists to stop taking at face value.
    #[test]
    fn prose_without_an_anchor_is_refused_by_line() {
        let err = parse(
            "I refactored the settle logic and it is much nicer now.",
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no `::`"), "{err}");
        assert!(err.contains("much nicer now"), "the line is named: {err}");

        let err = parse("A claim with an anchor and no paths ::", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no paths"), "{err}");

        // And an anchor with nothing said about it is not a claim either.
        assert!(parse(":: crates/board/src/db.rs", None).is_err());
    }

    /// `Db::open` in a sentence is ordinary prose; a `::` in a path is not. So
    /// the last separator wins.
    #[test]
    fn the_last_separator_wins_so_rust_paths_can_be_written_about() {
        let claims = parse("Db::open now migrates :: crates/board/src/db.rs", None).unwrap();
        assert_eq!(claims[0].text, "Db::open now migrates");
        assert_eq!(claims[0].files, ["crates/board/src/db.rs"]);
    }

    #[test]
    fn agents_write_bullet_lists_and_that_is_fine() {
        let claims = parse(
            "- First thing :: a.rs\n\
             * Second thing :: b.rs\n\
             \n\
             Third thing :: c.rs\n",
            None,
        )
        .unwrap();
        assert_eq!(claims.len(), 3);
        assert_eq!(claims[2].text, "Third thing");
    }

    /// The board handed the agent the absolute path; refusing it back would be
    /// pedantry about a prefix the board itself chose.
    #[test]
    fn an_absolute_path_inside_the_checkout_is_made_relative() {
        let claims = parse(
            "Did a thing :: /wt/gh-183-1/crates/board/src/db.rs ./docs/BOARD.md `Cargo.toml`",
            Some("/wt/gh-183-1"),
        )
        .unwrap();
        assert_eq!(
            claims[0].files,
            ["crates/board/src/db.rs", "docs/BOARD.md", "Cargo.toml"]
        );
    }

    #[test]
    fn a_path_named_twice_in_one_claim_is_named_once() {
        let claims = parse("Thing :: a.rs a.rs ./a.rs", None).unwrap();
        assert_eq!(claims[0].files, ["a.rs"]);
    }

    #[test]
    fn an_empty_submission_says_what_the_format_is() {
        let err = parse("   \n\n", None).unwrap_err().to_string();
        assert!(err.contains("One claim per line"), "{err}");
    }

    #[test]
    fn a_paragraph_wearing_an_anchor_is_refused() {
        let long = "x".repeat(MAX_TEXT + 1);
        let err = parse(&format!("{long} :: a.rs"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a claim"), "{err}");
    }

    // ---- the remainder ---------------------------------------------------

    /// The acceptance criterion of the whole ticket: the interesting set is the
    /// one the claims do not account for.
    #[test]
    fn the_unclaimed_set_is_what_the_claims_do_not_reach() {
        let claims = parse(
            "Stores claims on the attempt :: crates/board/src/db.rs crates/board/src/model.rs",
            None,
        )
        .unwrap();
        let diff = [
            changed("crates/board/src/db.rs"),
            changed("crates/board/src/model.rs"),
            // Nobody mentioned these two — a dependency and a function edited
            // in passing, which is exactly the pair this exists to surface.
            changed("Cargo.lock"),
            changed("crates/board/src/gc.rs"),
        ];
        let r = remainder(&claims, &diff);
        assert_eq!(r.claimed, 2);
        assert_eq!(
            r.unclaimed
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["Cargo.lock", "crates/board/src/gc.rs"]
        );
        assert!(!r.complete());
        assert!(r.claims[0].anchored());
    }

    #[test]
    fn a_diff_every_claim_reaches_is_a_complete_one() {
        let claims = parse("All of it :: src/a.rs src/b.rs", None).unwrap();
        let r = remainder(&claims, &[changed("src/a.rs"), changed("src/b.rs")]);
        assert!(r.complete());
        assert!(r.unclaimed.is_empty());
        assert_eq!(r.claimed, 2);
    }

    /// The honest spelling of "I rewrote this module", and visibly coarser than
    /// naming the files.
    #[test]
    fn a_directory_anchor_accounts_for_what_is_under_it() {
        let claims = parse("Rewrote the module :: crates/board/src/", None).unwrap();
        let r = remainder(
            &claims,
            &[
                changed("crates/board/src/db.rs"),
                changed("crates/board/src/sync.rs"),
                changed("crates/engine/src/board.rs"),
            ],
        );
        assert_eq!(r.unclaimed.len(), 1);
        assert_eq!(r.unclaimed[0].path, "crates/engine/src/board.rs");
        // A sibling whose name merely starts the same way is not underneath it.
        assert!(!accounts_for("crates/board", "crates/board-cli/x.rs"));
        assert!(accounts_for("crates/board", "crates/board/x.rs"));
    }

    /// Three crates here have a `db.rs`. A basename match would claim files
    /// nobody looked at — the exact failure this design is built around.
    #[test]
    fn a_bare_filename_anchors_nothing() {
        let claims = parse("Touched the database :: db.rs", None).unwrap();
        let r = remainder(&claims, &[changed("crates/board/src/db.rs")]);
        assert_eq!(r.unclaimed.len(), 1, "the change is still unaccounted for");
        assert_eq!(r.unmatched_anchors, ["db.rs"]);
        assert!(
            !r.claims[0].anchored(),
            "and the claim is checkable as wrong"
        );
    }

    /// A claim about a file that did not change is at least as interesting as
    /// an unclaimed file, and is never quietly dropped.
    #[test]
    fn an_anchor_the_diff_never_touched_is_reported_not_dropped() {
        let claims = parse(
            "Fixed the retry path :: src/retry.rs src/imaginary.rs",
            None,
        )
        .unwrap();
        let r = remainder(&claims, &[changed("src/retry.rs")]);
        assert_eq!(r.claims[0].matched, ["src/retry.rs"]);
        assert_eq!(r.claims[0].unmatched, ["src/imaginary.rs"]);
        assert_eq!(r.unmatched_anchors, ["src/imaginary.rs"]);
        assert!(r.complete(), "the diff itself is fully accounted for");
    }

    /// The state a review must be able to shout about: an agent that never
    /// answered the contract accounts for nothing at all.
    #[test]
    fn no_claims_means_the_whole_diff_is_the_remainder() {
        let diff = [changed("a.rs"), changed("b.rs")];
        let r = remainder(&[], &diff);
        assert_eq!(r.unclaimed.len(), 2);
        assert_eq!(r.claimed, 0);
        assert!(!r.complete());
    }

    #[test]
    fn two_claims_may_share_a_file_without_double_counting_it() {
        let claims = parse("One :: src/a.rs\nTwo :: src/a.rs src/b.rs", None).unwrap();
        let r = remainder(&claims, &[changed("src/a.rs"), changed("src/b.rs")]);
        assert_eq!(r.claimed, 2);
        assert!(r.complete());
    }

    // ---- reading git -----------------------------------------------------

    #[test]
    fn the_diff_is_read_from_numstat_and_name_status_together() {
        let files = parse_diff(
            "12\t3\tcrates/board/src/db.rs\n\
             40\t0\tcrates/board/src/claims.rs\n\
             -\t-\tdocs/screenshot.png\n",
            "M\tcrates/board/src/db.rs\n\
             A\tcrates/board/src/claims.rs\n\
             A\tdocs/screenshot.png\n",
        );
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].status, "M");
        assert_eq!((files[0].added, files[0].removed), (12, 3));
        assert_eq!(files[1].status, "A");
        assert!(files[2].binary);
        assert_eq!((files[2].added, files[2].removed), (0, 0));
    }

    /// A path only one of the two commands reported is still a changed file.
    /// Membership follows the numstat; the letter defaults to modified.
    #[test]
    fn a_file_name_status_did_not_mention_still_counts_as_changed() {
        let files = parse_diff("1\t1\tsrc/a.rs\n", "");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, "M");
    }

    // ---- the assembled review --------------------------------------------

    fn task_with(attempt: Attempt) -> Task {
        Task {
            id: "gh:o/r#183".into(),
            source: Source::Github,
            source_id: "183".into(),
            identifier: "gh#183".into(),
            title: "review backend".into(),
            body: Some("  ".into()),
            url: "https://github.com/o/r/issues/183".into(),
            labels: vec![],
            state: BoardState::Review,
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Started,
            local_done: false,
            pr_url: Some("https://github.com/o/r/pull/200".into()),
            pr_number: Some(200),
            pr_open: true,
            pr_merged: false,
            pr_mergeable: None,
            pr_base_ref: None,
            pr_stack: None,
            pr_changes_requested: None,
            updated_at: "t".into(),
            synced_at: "t".into(),
            attempts: vec![attempt],
        }
    }

    fn attempt_with(claims: Vec<Claim>, claims_at: Option<&str>) -> Attempt {
        Attempt {
            claims,
            claims_at: claims_at.map(str::to_string),
            outcome: Some(Outcome::Done),
            ..crate::model::tests::blank_attempt()
        }
    }

    #[test]
    fn a_review_carries_the_brief_the_claims_and_what_they_missed() {
        let claims = parse("Did the storage :: crates/board/src/db.rs", None).unwrap();
        let attempt = attempt_with(claims, Some("2026-08-09T10:00:00Z"));
        let task = task_with(attempt.clone());
        let r = review(
            &task,
            &attempt,
            vec![changed("crates/board/src/db.rs"), changed("Cargo.lock")],
            DiffSource::Checkout,
            Some(0),
            RunEvidence::default(),
            crate::effects::Effects::default(),
            None,
        );
        assert_eq!(r.brief.identifier, "gh#183");
        // An issue whose body is whitespace has no description; a surface must
        // not have to know which of the two the tracker sent.
        assert_eq!(r.brief.body, None);
        assert_eq!(r.attempt_number, 1);
        assert!(r.claimed());
        assert_eq!(r.remainder.unclaimed.len(), 1);
        assert_eq!(r.remainder.unclaimed[0].path, "Cargo.lock");
        assert_eq!(r.pr_url.as_deref(), Some("https://github.com/o/r/pull/200"));
    }

    /// "Never answered the contract" and "claimed nothing" are different facts,
    /// and the review keeps them apart all the way out.
    #[test]
    fn an_attempt_that_never_claimed_is_not_an_attempt_that_claimed_nothing() {
        let silent = attempt_with(vec![], None);
        let task = task_with(silent.clone());
        let r = review(
            &task,
            &silent,
            vec![changed("a.rs")],
            DiffSource::Recorded,
            None,
            RunEvidence::default(),
            crate::effects::Effects::default(),
            None,
        );
        assert!(!r.claimed());
        assert_eq!(r.remainder.unclaimed.len(), 1);
    }

    /// The published shape (`comet-board review --json`, `ReadAttemptReview`).
    /// Pinned because orchestrating agents read it beside `list --json`, and a
    /// key that quietly changes case is a key that quietly stops being read.
    #[test]
    fn the_json_is_snake_case_and_flattens_the_remainder_into_the_review() {
        let claims = parse("Did it :: src/a.rs src/nope.rs", None).unwrap();
        let attempt = attempt_with(claims, Some("2026-08-09T10:00:00Z"));
        let task = task_with(attempt.clone());
        let json = serde_json::to_value(review(
            &task,
            &attempt,
            vec![changed("src/a.rs"), changed("src/b.rs")],
            DiffSource::Checkout,
            Some(0),
            RunEvidence::default(),
            crate::effects::Effects::default(),
            None,
        ))
        .unwrap();

        assert_eq!(json["task_id"], "gh:o/r#183");
        assert_eq!(json["attempt_number"], 1);
        assert_eq!(json["claimed_at"], "2026-08-09T10:00:00Z");
        assert_eq!(json["brief"]["identifier"], "gh#183");
        assert_eq!(json["diff"]["source"], "checkout");
        assert_eq!(json["uncommitted"], 0);
        // The remainder is flattened in, so the answer is at the top level
        // where a reader (and a jq one-liner) looks for it.
        assert_eq!(json["unclaimed"][0]["path"], "src/b.rs");
        assert_eq!(json["unclaimed"][0]["added"], 10);
        assert_eq!(json["claimed"], 1);
        assert_eq!(json["unmatched_anchors"][0], "src/nope.rs");
        assert_eq!(json["claims"][0]["matched"][0], "src/a.rs");
        assert_eq!(json["evidence"]["commands"], 0);
        assert!(json["evidence"]["checks"].as_array().unwrap().is_empty());
        // The effects ride in the same object (§gh#236), and a review nobody
        // took them for carries `read: false` rather than dropping the key —
        // a reader that saw no `effects` at all could not tell "unread" from
        // "this build of the board does not derive them".
        assert_eq!(json["effects"]["read"], false);
        assert!(json["effects"]["files"].as_array().unwrap().is_empty());
        assert!(
            json["claims"][0]["call_sites"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    /// A collected checkout with nothing recorded must not render as "nothing
    /// changed" — the opposite of true, and the reason the variant travels.
    #[test]
    fn an_unreadable_diff_says_so_rather_than_reading_as_an_empty_one() {
        let attempt = attempt_with(vec![], None);
        let task = task_with(attempt.clone());
        let r = review(
            &task,
            &attempt,
            vec![],
            DiffSource::Unavailable {
                reason: "the checkout was reclaimed".into(),
            },
            None,
            RunEvidence::default(),
            crate::effects::Effects::default(),
            None,
        );
        assert!(matches!(r.diff, DiffSource::Unavailable { .. }));
        assert!(
            r.remainder.complete(),
            "vacuously, and the reader is told why"
        );
    }

    // ---- the reading -----------------------------------------------------

    fn reviewed(
        claims: &str,
        claimed_at: Option<&str>,
        files: Vec<ChangedFile>,
        uncommitted: Option<u32>,
        evidence: RunEvidence,
    ) -> AttemptReview {
        let parsed = if claims.is_empty() {
            vec![]
        } else {
            parse(claims, None).unwrap()
        };
        let attempt = attempt_with(parsed, claimed_at);
        let task = task_with(attempt.clone());
        review(
            &task,
            &attempt,
            files,
            DiffSource::Checkout,
            uncommitted,
            evidence,
            crate::effects::Effects::default(),
            None,
        )
    }

    fn ran(command: &str, failed: bool) -> RanCommand {
        RanCommand {
            command: command.into(),
            failed,
        }
    }

    /// The whole point of the screen, said in one line: the remainder leads,
    /// and it leads as a proportion rather than a bare count.
    #[test]
    fn the_verdict_leads_with_the_changes_nobody_claimed() {
        let r = reviewed(
            "Did the storage :: src/db.rs",
            Some("2026-08-09T10:00:00Z"),
            vec![changed("src/db.rs"), changed("Cargo.lock")],
            Some(0),
            RunEvidence::default(),
        );
        let verdict = r.verdict();
        assert_eq!(verdict.tone, Tone::Alarm);
        assert_eq!(verdict.text, "1 of 2 changed files are claimed by nobody");
        assert_eq!(r.findings()[0].kind, FindingKind::Unclaimed);
    }

    /// A claim that covers the diff is not the same as an unexamined one, so
    /// the settled verdict names the denominator it checked.
    #[test]
    fn a_diff_the_claims_cover_reads_settled_and_says_what_it_covered() {
        let r = reviewed(
            "Did the storage :: src/db.rs",
            Some("2026-08-09T10:00:00Z"),
            vec![changed("src/db.rs")],
            Some(0),
            RunEvidence {
                commands: 3,
                failed: 0,
                checks: vec![Check {
                    command: "cargo test".into(),
                    runs: 2,
                    failed: 1,
                }],
                truncated: false,
            },
        );
        assert_eq!(r.findings(), vec![]);
        let verdict = r.verdict();
        assert_eq!(verdict.tone, Tone::Settled);
        assert_eq!(verdict.text, "the one changed file is accounted for");
    }

    /// Never asked is not claimed nothing (§gh#183), and the reading is where
    /// that distinction either survives to a screen or dies. A silent attempt
    /// reads as *unknown*, never as an alarm about work it was never asked to
    /// describe — and never, ever, as green.
    #[test]
    fn an_attempt_that_never_claimed_reads_as_unknown_rather_than_as_a_remainder() {
        let r = reviewed(
            "",
            None,
            vec![changed("src/db.rs"), changed("Cargo.lock")],
            Some(0),
            RunEvidence::default(),
        );
        let verdict = r.verdict();
        assert_eq!(verdict.tone, Tone::Unknown);
        assert!(verdict.text.contains("never answered the claim contract"));
        assert!(verdict.text.ends_with("its 2 changed files"));
        // Not ALSO reported as an unclaimed remainder: "2 of 2" adds a number
        // to a sentence that already carries it, and dresses a question the
        // agent was never asked as a failure to answer it.
        assert!(
            !r.findings()
                .iter()
                .any(|f| f.kind == FindingKind::Unclaimed)
        );
    }

    /// Claiming nothing, having been asked, IS the remainder — the other side
    /// of the distinction above.
    #[test]
    fn claiming_nothing_after_being_asked_is_the_whole_diff_unclaimed() {
        let mut r = reviewed(
            "",
            None,
            vec![changed("src/db.rs")],
            Some(0),
            RunEvidence::default(),
        );
        r.claimed_at = Some("2026-08-09T10:00:00Z".into());
        assert_eq!(r.verdict().tone, Tone::Alarm);
        assert_eq!(r.findings()[0].kind, FindingKind::Unclaimed);
    }

    /// The one fact that can make an EMPTY remainder wrong: an agent that
    /// claims before it commits has shown the board none of its work, and
    /// would otherwise be told all of it was accounted for.
    #[test]
    fn uncommitted_work_is_reported_even_when_the_branch_is_fully_claimed() {
        let r = reviewed(
            "Did the storage :: src/db.rs",
            Some("2026-08-09T10:00:00Z"),
            vec![changed("src/db.rs")],
            Some(3),
            RunEvidence::default(),
        );
        assert_eq!(r.verdict().tone, Tone::Alarm);
        assert_eq!(r.findings()[0].kind, FindingKind::Uncommitted);
        assert!(r.findings()[0].text.starts_with("3 files are changed"));
    }

    /// Work described that did not happen is as interesting as work nobody
    /// described, and the reading says so in the same voice.
    #[test]
    fn a_claim_the_diff_refuses_to_support_is_its_own_finding() {
        let r = reviewed(
            "Rewrote the sync loop :: src/sync.rs",
            Some("2026-08-09T10:00:00Z"),
            vec![],
            Some(0),
            RunEvidence::default(),
        );
        let kinds: Vec<_> = r.findings().iter().map(|f| f.kind).collect();
        assert_eq!(kinds, vec![FindingKind::UnsupportedClaims]);
        assert!(r.findings()[0].text.starts_with("1 claim is anchored"));
    }

    /// Evidence the agent did not author, read the way the module intends it:
    /// a check that never passed is louder than no checks at all, and a busy
    /// run that verified nothing still says so.
    #[test]
    fn the_evidence_findings_rank_a_failing_check_over_a_missing_one() {
        let never = reviewed(
            "Did it :: src/db.rs",
            Some("2026-08-09T10:00:00Z"),
            vec![changed("src/db.rs")],
            Some(0),
            crate::evidence::gather(&[ran("cargo test -p comet-board", true)]),
        );
        assert_eq!(never.findings()[0].kind, FindingKind::NeverPassed);
        assert_eq!(
            never.findings()[0].text,
            "1 check ran and never once passed"
        );

        let quiet = reviewed(
            "Did it :: src/db.rs",
            Some("2026-08-09T10:00:00Z"),
            vec![changed("src/db.rs")],
            Some(0),
            crate::evidence::gather(&[ran("ls", false), ran("grep -rn foo .", false)]),
        );
        assert_eq!(quiet.findings()[0].kind, FindingKind::Unchecked);
        assert!(quiet.findings()[0].text.ends_with("across 2 commands"));
    }

    /// No diff means no denominator. Every other finding would be computed
    /// against an empty changed set and would read as good news, so the
    /// reading stops at the reason and says only that.
    #[test]
    fn an_unreadable_diff_suppresses_every_finding_computed_against_it() {
        let attempt = attempt_with(vec![], None);
        let task = task_with(attempt.clone());
        let r = review(
            &task,
            &attempt,
            vec![],
            DiffSource::Unavailable {
                reason: "the checkout was reclaimed".into(),
            },
            Some(4),
            crate::evidence::gather(&[ran("cargo test", true)]),
            crate::effects::Effects::default(),
            None,
        );
        let findings = r.findings();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, FindingKind::NoDiff);
        assert_eq!(r.verdict().tone, Tone::Unknown);
        assert!(r.verdict().text.contains("the checkout was reclaimed"));
    }

    /// A branch that changed nothing is not a branch whose changes were all
    /// accounted for. Green there would be a lie about an empty set.
    #[test]
    fn a_branch_that_changed_nothing_is_unknown_not_settled() {
        let r = reviewed(
            "",
            Some("2026-08-09T10:00:00Z"),
            vec![],
            Some(0),
            RunEvidence::default(),
        );
        assert_eq!(r.verdict().tone, Tone::Unknown);
        assert_eq!(r.verdict().text, "this attempt's branch changed nothing");
    }

    // ---- anchors: a path or a symbol (§gh#235) ---------------------------

    /// The whole rule, and it is only a rule about spelling.
    #[test]
    fn an_anchor_is_a_path_or_a_symbol_by_how_it_is_written() {
        for path in [
            "crates/board/src/db.rs",
            "shell/spaces.rs",
            "db.rs",
            "Cargo.toml",
            "crates/board/src/",
            "docs/board",
        ] {
            assert_eq!(anchor_kind(path), AnchorKind::Path, "{path}");
        }
        for symbol in [
            "active_placements",
            "shelf_note",
            "AttemptReview",
            "set_attempt_claims",
        ] {
            assert_eq!(anchor_kind(symbol), AnchorKind::Symbol, "{symbol}");
        }
    }

    /// The design's own reference claims, which under §gh#183 parsed and then
    /// matched nothing.
    #[test]
    fn the_designs_reference_claims_parse_into_both_kinds() {
        let claims = parse(
            "A live chat's row is drawn by Active :: shell/spaces.rs view/spaces.rs\n\
             Both surfaces read one derivation per frame :: active_placements\n\
             An expanded space says so :: shelf_note",
            None,
        )
        .unwrap();
        assert_eq!(claims[0].files, ["shell/spaces.rs", "view/spaces.rs"]);
        assert!(claims[0].symbols.is_empty());
        assert!(claims[1].files.is_empty());
        assert_eq!(claims[1].symbols, ["active_placements"]);
        assert_eq!(claims[2].symbols, ["shelf_note"]);
    }

    /// A claim may carry both kinds at once, and the anchorless refusal is
    /// unchanged — a symbol is a second way to anchor, never a way out of it.
    #[test]
    fn a_claim_can_mix_anchors_but_still_needs_one() {
        let claims = parse("Rewrote it :: crates/ui/src/review.rs claim_row", None).unwrap();
        assert_eq!(claims[0].files, ["crates/ui/src/review.rs"]);
        assert_eq!(claims[0].symbols, ["claim_row"]);
        assert_eq!(claims[0].anchors().count(), 2);
        assert!(parse("Just prose about the work", None).is_err());
        assert!(parse("Said something ::", None).is_err());
    }

    /// Two characters name half a diff, so they are refused by name rather
    /// than accepted and silently matched against everything.
    #[test]
    fn a_symbol_too_short_to_anchor_anything_is_refused() {
        let err = parse("Renamed it :: id", None).unwrap_err().to_string();
        assert!(err.contains("too short to anchor"), "{err}");
    }

    #[test]
    fn a_symbol_accounts_for_the_changed_files_whose_diff_names_it() {
        let claims = parse("One derivation per frame :: active_placements", None).unwrap();
        let r = remainder(
            &claims,
            &[
                changed_naming(
                    "crates/ui/src/shell/spaces.rs",
                    &["active_placements", "row"],
                ),
                changed_naming("crates/proto/src/view/spaces.rs", &["active_placements"]),
                changed_naming("Cargo.lock", &["comet_board"]),
            ],
        );
        assert_eq!(
            r.claims[0].matched,
            [
                "crates/ui/src/shell/spaces.rs",
                "crates/proto/src/view/spaces.rs"
            ]
        );
        assert_eq!(
            r.unclaimed.iter().map(|f| &f.path).collect::<Vec<_>>(),
            ["Cargo.lock"]
        );
    }

    /// `accounts_for`'s rule, applied to the other kind of anchor: a prefix is
    /// not a match, or the generous reading claims files nobody looked at.
    #[test]
    fn a_symbol_anchor_is_matched_whole_and_reported_when_it_is_not() {
        let claims = parse("Touched the note :: note", None).unwrap();
        let file = changed_naming("crates/ui/src/shelf.rs", &["shelf_note"]);
        let r = remainder(&claims, &[file]);
        assert!(!r.claims[0].anchored());
        assert_eq!(r.unmatched_anchors, ["note"]);
        assert_eq!(r.unclaimed.len(), 1);
    }

    /// A symbol nothing changed names is the second-loudest row on the screen,
    /// exactly like a path nothing happened to — one list, both kinds.
    #[test]
    fn an_unmatched_symbol_lands_beside_an_unmatched_path() {
        let claims = parse("Did two things :: src/gone.rs never_written", None).unwrap();
        let r = remainder(&claims, &[changed_naming("src/here.rs", &["something"])]);
        assert_eq!(r.claims[0].unmatched, ["src/gone.rs", "never_written"]);
        assert_eq!(r.unmatched_anchors, ["src/gone.rs", "never_written"]);
    }

    // ---- the symbol index, off the diff -----------------------------------

    #[test]
    fn the_symbol_index_is_read_off_the_lines_that_moved() {
        let mut files = vec![changed("crates/board/src/claims.rs")];
        attach_symbols(
            &mut files,
            "diff --git a/crates/board/src/claims.rs b/crates/board/src/claims.rs\n\
             --- a/crates/board/src/claims.rs\n\
             +++ b/crates/board/src/claims.rs\n\
             @@ -1,2 +1,3 @@\n\
             +pub fn names_symbol(symbol: &str) -> bool {\n\
             -fn untouched_context() {}\n\
             \x20fn context_line_nobody_edited() {}\n",
        );
        assert!(files[0].symbols.iter().any(|s| s == "names_symbol"));
        // A removed line moved too: deleting a function is a change to it.
        assert!(files[0].symbols.iter().any(|s| s == "untouched_context"));
        // Context is not evidence — that is the whole reason for `-U0`.
        assert!(
            !files[0]
                .symbols
                .iter()
                .any(|s| s == "context_line_nobody_edited")
        );
    }

    /// The `+++`/`---` headers are not changed lines, and a file the diff names
    /// that the changed set does not know about is skipped rather than guessed.
    #[test]
    fn the_symbol_index_ignores_headers_and_unknown_files() {
        let mut files = vec![changed("src/a.rs")];
        attach_symbols(
            &mut files,
            "--- a/src/a.rs\n\
             +++ b/src/a.rs\n\
             +let alpha = 1;\n\
             --- a/src/unknown.rs\n\
             +++ b/src/unknown.rs\n\
             +let beta = 2;\n",
        );
        // `let` is in there too: no keyword list, on purpose — an anchor on a
        // keyword matches most of the diff and the review prints what it
        // reached, which is a better answer than a list that is wrong for the
        // next language.
        assert!(files[0].symbols.iter().any(|s| s == "alpha"));
        assert!(!files[0].symbols.iter().any(|s| s == "beta"));
    }

    /// A new file's `---` is `/dev/null`; the `+++` is what names it.
    #[test]
    fn a_new_file_is_named_by_the_plus_header() {
        let mut files = vec![changed("src/new.rs")];
        attach_symbols(
            &mut files,
            "--- /dev/null\n+++ b/src/new.rs\n+pub const SHELF_NOTE: u8 = 1;\n",
        );
        assert!(files[0].symbols.iter().any(|s| s == "SHELF_NOTE"));
    }

    #[test]
    fn the_symbol_index_is_capped_per_file() {
        let mut files = vec![changed("src/wide.rs")];
        let mut diff = String::from("+++ b/src/wide.rs\n");
        for i in 0..(MAX_FILE_SYMBOLS + 50) {
            diff.push_str(&format!("+let sym_{i} = {i};\n"));
        }
        attach_symbols(&mut files, &diff);
        assert_eq!(files[0].symbols.len(), MAX_FILE_SYMBOLS);
    }

    // ---- the block, off a finished attempt (§gh#235) ----------------------

    #[test]
    fn a_claims_block_is_read_out_of_a_closing_message() {
        let said = "I have finished and pushed the branch.\n\n\
                    ```claims\n\
                    Storage lives on the attempt :: crates/board/src/db.rs\n\
                    The remainder comes off the diff :: remainder\n\
                    ```\n\n\
                    The PR is open.";
        let Harvest::Claims(claims) = harvest(said, None) else {
            panic!("expected claims");
        };
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0].files, ["crates/board/src/db.rs"]);
        assert_eq!(claims[1].symbols, ["remainder"]);
    }

    /// The exit condition: an attempt that said nothing is not an attempt the
    /// harvest has an opinion about.
    #[test]
    fn a_message_with_no_block_harvests_nothing() {
        assert_eq!(harvest("Done. The tests pass.", None), Harvest::None);
        assert_eq!(harvest("", None), Harvest::None);
        assert_eq!(harvest("```claims\n\n```\n", None), Harvest::None);
    }

    /// Reported, never dropped — and in `parse`'s own words, which name the
    /// line that did it.
    #[test]
    fn a_malformed_block_comes_back_as_a_refusal() {
        let Harvest::Malformed(why) = harvest("```claims\nI did some work\n```", None) else {
            panic!("expected a refusal");
        };
        assert!(why.contains("I did some work"), "{why}");
        assert!(why.contains("::"), "{why}");
    }

    /// `set_attempt_claims`'s rule, applied to the block: an agent that wrote
    /// it twice is correcting itself.
    #[test]
    fn the_last_block_wins() {
        let Harvest::Claims(claims) = harvest(
            "```claims\nFirst pass :: src/a.rs\n```\n\
             then I found more.\n\
             ```claims\nSecond pass :: src/b.rs\n```\n",
            None,
        ) else {
            panic!("expected claims");
        };
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].files, ["src/b.rs"]);
    }

    /// A `claims` fence quoted inside a shell block is documentation, not a
    /// submission — the outer fence is skipped whole.
    #[test]
    fn a_block_quoted_inside_another_fence_is_not_read() {
        let said = "Here is how it works:\n\
                    ```bash\n\
                    cat <<'EOF'\n\
                    ```claims\n\
                    Not mine :: src/nope.rs\n\
                    EOF\n\
                    ```\n";
        assert_eq!(harvest(said, None), Harvest::None);
    }

    /// A message cut off mid-block still carries claims. Refusing them over a
    /// missing fence would teach nobody anything.
    #[test]
    fn an_unterminated_block_still_counts() {
        let Harvest::Claims(claims) = harvest("```claims\nDid it :: src/a.rs\n", None) else {
            panic!("expected claims");
        };
        assert_eq!(claims[0].files, ["src/a.rs"]);
    }

    // ---- a refused block, read (§gh#235) ----------------------------------

    /// Louder than silence, and instead of it: this attempt answered, and the
    /// answer is what went missing.
    #[test]
    fn a_refused_block_is_alarming_where_saying_nothing_is_merely_unknown() {
        let quiet = reviewed(
            "",
            None,
            vec![changed("src/a.rs")],
            Some(0),
            RunEvidence::default(),
        );
        assert_eq!(quiet.findings()[0].kind, FindingKind::NeverClaimed);
        assert_eq!(quiet.verdict().tone, Tone::Unknown);

        let attempt = Attempt {
            claims_error: Some("no `::` in: I did some work\nA claim is a sentence…".into()),
            ..attempt_with(vec![], None)
        };
        let task = task_with(attempt.clone());
        let loud = review(
            &task,
            &attempt,
            vec![changed("src/a.rs")],
            DiffSource::Checkout,
            Some(0),
            RunEvidence::default(),
            crate::effects::Effects::default(),
            None,
        );
        assert_eq!(loud.findings()[0].kind, FindingKind::MalformedClaims);
        assert_eq!(loud.verdict().tone, Tone::Alarm);
        // One line in the verdict; the whole refusal is still on the row.
        assert!(loud.verdict().text.contains("I did some work"));
        assert!(!loud.verdict().text.contains('\n'));
    }

    #[test]
    fn a_binary_file_has_no_lines_to_count() {
        let mut file = changed("logo.png");
        file.binary = true;
        file.added = 0;
        file.removed = 0;
        assert_eq!(file.counts(), "binary");
        assert_eq!(changed("src/db.rs").counts(), "+10 −2");
    }
}
