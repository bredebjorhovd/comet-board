//! Resolving a task + its route into a [`DispatchSpec`] — the planning half of
//! herdr-board's `dispatch.rs`, in comet vocabulary
//! (§runtime-impl/§dispatch-pipeline).
//!
//! Deliberately only the *decisions* live here: task → route → branch → brief,
//! plus the refusals a dispatch owes its caller before anything is created
//! ([`check_capacity`], [`route_for`], [`stack_parent`]) and the provenance
//! verdict ([`dispatcher_for`]) — pure functions over config and stored rows.
//! Executing the spec is [`crate::runtime::Runtime::dispatch`]'s job; the
//! attempt-row lifecycle around it lives with the board loop in the engine.
//!
//! Stacking (gh#285) is the newest of those decisions and the one that broke
//! the shape: where a dispatch branches from used to be a *route* answer, one
//! string for every task under it. Dispatching task B off task A's branch is a
//! fact about a single release, so it arrives as an override
//! ([`DispatchOverrides::base`]) and, in the spelling anybody actually uses, as
//! a task to stack on ([`DispatchOverrides::onto`]) that [`stack_parent`]
//! resolves to the attempt holding the branch.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};
use comet_proto::view::slug::title_slug;

use crate::billing::{Attribution, Billing};
use crate::config::{Route, RoutingConfig, interpolate, slugify};
use crate::db::Db;
use crate::model::{Dispatcher, Task, UpstreamState};
use crate::runtime::{DispatchSpec, harness_for_runtime};
use crate::sources::github::{CapabilityEvidence, PushCapabilities, WriteCapability};
use crate::sync::route_context;

/// The branch slug for a task: `gh#341` titled "The review page loads nothing"
/// → `gh-341-review-page-loads`, `LIN-145` → `lin-145-<its slug>`. The
/// identifier, made branch-safe, and then what the task is *about* (gh#364).
///
/// It used to be the identifier and the *repo* — `gh-341-comet-board`, from
/// herdr-board AGE-20, where another repo's merged pull request had attached
/// itself to an untouched task. The repo appears twice in the result of that
/// and is implicit both times: a branch lives in the repo it was cut in, and
/// the worktree is already under a per-repo directory
/// (`Repos::create_worktree_on`), so
/// `~/.comet-native/worktrees/comet-board/board-gh-341-comet-board` says it
/// three times and tells a reader nothing. Branch namespaces are
/// per-repo, so nothing collides by dropping it.
///
/// AGE-20's fix is unaffected, because the branch was never what fixed it: the
/// scope is the repo the *task id* names ([`crate::sync::link_for`]'s
/// `own_repo`, and `AttemptBranches`' `in_repo`), which is exactly why those
/// two carry the comment that attempts recorded before the qualification do
/// not have it in their branch either. Nothing anywhere parses a repo back out
/// of a branch name — `crate::stacks::layer_of` reads the `-2`/`-3` suffix off
/// whatever the attempt branch is, and `link_pull_requests` matches `head_ref
/// == attempt.branch` whole.
///
/// A title with no slug in it ([`title_slug`]) leaves the identifier standing
/// alone, which is the rule the whole feature keeps: the identifier is always
/// enough.
pub fn branch_slug(task: &Task) -> String {
    match title_slug(&task.title) {
        Some(slug) => slugify(&format!("{}-{slug}", task.identifier)),
        None => slugify(&task.identifier),
    }
}

/// Build the interpolation variables for a task. Shared by the brief and the
/// branch template so what you read is what gets sent.
pub fn prompt_vars<'a>(
    task: &'a Task,
    branch: &'a str,
    workspace: &'a str,
) -> BTreeMap<&'static str, String> {
    let mut v = BTreeMap::new();
    v.insert("title", task.title.clone());
    v.insert("identifier", task.identifier.clone());
    // The board's own name for the task — what every verb takes as `--task`,
    // and a different string from the identifier above (`gh:owner/repo#339`
    // against `gh#339`). A route template that wants to name the verb itself
    // needs this one; nothing else in a dispatched run says it (§gh#339).
    v.insert("task_id", task.id.clone());
    v.insert("identifier_lower", branch_slug(task));
    // The two halves of that on their own, for a template that wants to
    // compose them itself — or to leave the slug out, which a route whose
    // branches are read by something stricter than a human may want.
    v.insert("identifier_slug", slugify(&task.identifier));
    v.insert("title_slug", title_slug(&task.title).unwrap_or_default());
    v.insert("body", task.body.clone().unwrap_or_default());
    v.insert("url", task.url.clone());
    v.insert("branch", branch.to_string());
    v.insert("workspace", workspace.to_string());
    v
}

/// The brief actually sent for a task under a route, fully interpolated —
/// except `{worktree}`, which comet's engine only knows while executing the
/// spec, after this string is built. [`DispatchSpec::prompt_at`] resolves it
/// once the checkout exists; `interpolate` leaves unknown keys visible rather
/// than blanking them, so the seam is legible in between.
///
/// `base` is the dispatch's [`DispatchSpec::base`], and it is appended to the
/// brief rather than interpolated into it — see [`pr_base`] for why the agent
/// has to be told, and why the line lands on a route's own `prompt` too.
///
/// `stack` is [`DispatchOverrides::stack`], and it is appended for the same
/// reason and one more: [`crate::stacks::stack_brief`] answers the base
/// sentence above it, so it has to come after the sentence it answers.
/// `decompose` ([`DispatchOverrides::decompose`]) sits in the same seat — the
/// two never appear together ([`build_spec`] refuses the pair).
///
/// [`crate::claims::brief`] closes it, on the same rule and for the failure
/// gh#339 opened on: a brief that asked for a commit, a push and a pull request
/// and never mentioned the review contract got exactly what it asked for.
pub fn resolve_prompt(
    route: &Route,
    task: &Task,
    branch: &str,
    base: &str,
    stack: bool,
    decompose: bool,
) -> String {
    let vars = prompt_vars(task, branch, &route.workspace);
    let template = route.prompt.clone().unwrap_or_else(|| {
        // A route with no prompt still needs to say something useful.
        // "push" is not padding: the board settles an attempt on a pull
        // request or on commits that reached origin (gh#69), so work that
        // never leaves the worktree leaves the row `working`.
        "You are working on: {title} ({identifier})\n\n{body}\n\n\
         Work in this worktree; the branch {branch} is prepared. \
         Commit and push your work, and open a pull request when done."
            .to_string()
    });
    let mut prompt = interpolate(&template, &vars);
    if let Some(base) = pr_base(base) {
        prompt.push_str(&pr_base_line(base));
    }
    if stack {
        prompt.push_str(&crate::stacks::stack_brief(branch, pr_base(base)));
    }
    if decompose {
        prompt.push_str(&decompose_brief(task));
    }
    // Last, because it is the last thing the agent does — and unconditional,
    // because the review contract is what the board dispatched for (§gh#339).
    // A template that already names the verb still gets it: the paragraph is
    // about being dispatched, not about this task, and the duplicate a route
    // author would create by mentioning claims themselves is cheaper than an
    // attempt that claims nothing.
    prompt.push_str(&crate::claims::brief(&task.id));
    prompt
}

/// The branch a dispatch's pull request must name, or `None` when the repo's
/// own default is already the right answer (gh#284).
///
/// The `base` key decided one thing and told nobody: where
/// `Repos::create_worktree_on` cuts the branch. Opening the pull request is
/// the agent's job, and `gh pr create` with no `--base` targets the repo
/// default — so a route branching from `release-1.x` produced a request to
/// merge `release-1.x` *into `main`*, carrying every commit the release branch
/// was ahead by. The brief is where that gets fixed, because the brief is the
/// only channel the board has to the thing doing the creating.
///
/// Which spellings mean "the repo default", and so say nothing:
///
/// - `origin/HEAD`, the default — the remote's default branch, which is
///   exactly what `gh pr create` picks unasked;
/// - `HEAD` and empty — the space folder's current checkout, the opt-out for a
///   repo with no remote. The board cannot name that branch from here (it is
///   whatever ran in that folder last, which is the reason `origin/HEAD` is
///   the default in the first place), and a guess in the brief would be worse
///   than the silence: `gh`'s own default is at least right about the repo.
///
/// Anything else names a branch, and `origin/` is stripped: `--base
/// origin/release-1.x` is not a base GitHub accepts.
///
/// Brief-only is the first cut, and deliberately so. The board could make this
/// mechanical instead — the `gh` shim ([`crate::git_credentials`]) already
/// wraps every `gh` an agent runs, and could splice `--base` into a bare `gh
/// pr create` — but that is the shim growing opinions about argv (which
/// subcommand, which existing flag wins, what `--repo` elsewhere means) to
/// cover a case the brief states plainly. Worth doing when a told agent is
/// observed getting it wrong; not before.
///
/// The other half of "not before" is being able to *see* it go wrong, and the
/// board cannot yet: nothing it stores about a linked pull request records
/// which branch that request targets. Once the sync carries the PR's own
/// `base` ref, comparing it against this is the check worth having — and the
/// row should say the two disagree rather than quietly following the request,
/// because a request opened against the wrong branch is not a base the
/// operator changed their mind about.
pub fn pr_base(base: &str) -> Option<&str> {
    match base.trim() {
        "" | "HEAD" | "origin/HEAD" => None,
        other => Some(other.strip_prefix("origin/").unwrap_or(other)),
    }
}

/// The sentence [`resolve_prompt`] appends when [`pr_base`] names a branch.
///
/// Appended after interpolation, so it reaches a route's custom `prompt` too:
/// a template is somebody's wording for the *task*, and where the pull request
/// goes is a fact about the dispatch that the same somebody has no way to know
/// when they write the template. It names the flag rather than the intent
/// because the failure mode is an agent that opens the request correctly-minded
/// and forgets the argument.
fn pr_base_line(base: &str) -> String {
    format!(
        "\n\nOpen your pull request against `{base}`, not the repo's default \
         branch: `gh pr create --base {base}`. Your branch was cut from \
         `{base}`, so a request that targets anything else carries commits \
         that are not yours."
    )
}

/// The credential preflight every GitHub-backed dispatch receives (gh#440).
///
/// A workflow file is not ordinary repository content to GitHub. A credential
/// with `Contents: write` / `repo` can push a whole completed branch until one
/// commit touches `.github/workflows/**`, at which point GitHub rejects the
/// ref update. This block makes that difference part of the first prompt,
/// before an agent promises or attempts a delivery the credential cannot make.
///
/// Unknown evidence fails closed. For ordinary contents that means stop before
/// editing — the task has no deliverable branch. When only workflow permission
/// is absent, ordinary work remains deliverable and the intended workflow
/// becomes a committed patch artifact outside the protected namespace, so the
/// pull request still carries exact work a human can apply.
pub fn credential_preflight_brief(capabilities: PushCapabilities) -> String {
    let evidence = match capabilities.evidence {
        CapabilityEvidence::AppInstallation => "the GitHub App installation token",
        CapabilityEvidence::ClassicOauthScopes => "GITHUB_TOKEN's OAuth scopes",
        CapabilityEvidence::OpaqueToken => {
            "an opaque/fine-grained GITHUB_TOKEN for which GitHub exposed no OAuth scopes"
        }
        CapabilityEvidence::Anonymous => "the absence of a board GitHub credential",
        CapabilityEvidence::ProbeFailed => "a GitHub capability probe that did not complete",
    };

    if capabilities.contents != WriteCapability::Write {
        let remediation = match capabilities.evidence {
            CapabilityEvidence::AppInstallation => {
                "Have the operator grant the App `Contents: Read and write` and approve the updated permission on this installation."
            }
            CapabilityEvidence::ClassicOauthScopes => {
                "Have the operator ensure the token holder has push access to this repository and refresh or replace GITHUB_TOKEN with `repo` (or `public_repo` for a public repository) scope."
            }
            CapabilityEvidence::OpaqueToken => {
                "Have the operator use a GitHub App whose installation reports `Contents: Read and write`, or a classic token whose `repo` scope GitHub can report."
            }
            CapabilityEvidence::Anonymous => {
                "Have the operator configure GITHUB_TOKEN or the GITHUB_APP_ID/GITHUB_APP_PRIVATE_KEY_PATH pair."
            }
            CapabilityEvidence::ProbeFailed => {
                "Have the operator restore GitHub access and run `comet-board doctor` before retrying."
            }
        };
        return format!(
            "\n\nCredential preflight — **stop before changing files**: ordinary repository \
             content write is {} and workflow-file write is {} according to {evidence}. \
             This attempt cannot establish a deliverable push; do not edit, commit, or \
             promise the requested change. Report the credential preflight as the blocker. \
             {remediation}",
            capabilities.contents.word(),
            capabilities.workflows.word(),
        );
    }

    if !capabilities.can_write_workflows() {
        let remediation = match capabilities.evidence {
            CapabilityEvidence::AppInstallation => {
                "The operator can remove this restriction by granting the App `Workflows: Read and write` and approving the updated permission on this installation."
            }
            CapabilityEvidence::ClassicOauthScopes => {
                "The operator can remove this restriction by refreshing or replacing GITHUB_TOKEN with the classic `workflow` scope in addition to its repository scope."
            }
            CapabilityEvidence::OpaqueToken => {
                "The operator can remove this restriction with a GitHub App installation that reports `Contents` and `Workflows` write, or a classic token whose `repo` and `workflow` scopes GitHub reports."
            }
            CapabilityEvidence::Anonymous | CapabilityEvidence::ProbeFailed => {
                "The operator can remove this restriction by repairing the credential and confirming it with `comet-board doctor`."
            }
        };
        return format!(
            "\n\nCredential preflight — ordinary repository content is writable, but \
             `.github/workflows/**` write is {} according to {evidence}. Do not add, \
             edit, stage, or commit workflow files on this branch: GitHub will reject the \
             entire push. If the task needs a workflow change, develop it only as a patch \
             artifact outside `.github/workflows` (for example under `docs/workflow-patches/`), \
             commit that artifact, and put its path and exact apply command in the pull \
             request description. Say plainly that the workflow itself has not landed. \
             {remediation}",
            capabilities.workflows.word(),
        );
    }

    format!(
        "\n\nCredential preflight — ordinary repository content and \
         `.github/workflows/**` files are both writable according to {evidence}."
    )
}

/// A pre-cut refusal when even an ordinary branch cannot be established as
/// pushable. Missing workflow permission alone is not a refusal — that route
/// has the patch-artifact fallback above — but missing or unknown content write
/// leaves no pull request an agent can deliver at all.
pub fn credential_preflight_refusal(capabilities: PushCapabilities) -> Option<String> {
    if capabilities.contents == WriteCapability::Write {
        return None;
    }
    let remediation = match capabilities.evidence {
        CapabilityEvidence::AppInstallation => {
            "grant the GitHub App `Contents: Read and write` and approve the updated installation permission"
        }
        CapabilityEvidence::ClassicOauthScopes => {
            "ensure the token holder has push access to this repository and refresh or replace GITHUB_TOKEN with `repo` (or `public_repo` for a public repository) scope"
        }
        CapabilityEvidence::OpaqueToken => {
            "GitHub exposed no OAuth-scope evidence for this fine-grained/unknown token; use an App installation reporting Contents write or a classic token reporting `repo`"
        }
        CapabilityEvidence::Anonymous => {
            "configure GITHUB_TOKEN or GITHUB_APP_ID with GITHUB_APP_PRIVATE_KEY_PATH"
        }
        CapabilityEvidence::ProbeFailed => {
            "restore GitHub access and confirm it with `comet-board doctor`"
        }
    };
    Some(format!(
        "ordinary repository content write is {} and cannot be established as deliverable — {remediation}",
        capabilities.contents.word()
    ))
}

/// The block [`resolve_prompt`] appends when a dispatch asks for a
/// decomposition (gh#340).
///
/// The standing rule already reaches every dispatched runtime: the conventions
/// block (gh#272) says work you delegate goes through the board and in-chat
/// subagents are for reading — and, a bullet down, that nothing is dispatched
/// speculatively; an explicit instruction is what releases work. What no
/// channel carried was that instruction for a *specific* task. Nothing in a
/// brief ever said "this one is bigger than one agent", so a well-behaved
/// agent that suspected as much was still bound to do the work alone, and the
/// operator's only way to say otherwise was prompt prose in the ticket —
/// workable for whoever knows the tool, invisible to anyone else. Same
/// resolution as `--stack` (gh#287), for the same reason on its other face: an
/// agent deciding on its own to open five tickets is a surprise worth opting
/// into, so the deciding stays a human's and the flag is how it is said.
///
/// The `--repo` in the example is interpolated when the task names one, and
/// only then: a piece filed into a different repository is a piece the parent
/// issue's tracker never shows, and on a box that watches several repos a bare
/// `new` needs the flag anyway. A Linear task gets the bare command — where a
/// new ticket lands is `[defaults] new_source`'s answer, and guessing a team
/// key here would be worse than the CLI's own refusal naming the keys.
fn decompose_brief(task: &Task) -> String {
    let repo_flag = crate::model::gh_repo(&task.id)
        .map(|repo| format!(" --repo {repo}"))
        .unwrap_or_default();
    let identifier = &task.identifier;
    format!(
        "\n\nThis dispatch asks you to **decompose**: whoever released it \
         judged this task bigger than one agent. Split it into pieces that \
         stand alone, write each piece up as a ticket, and release it to an \
         agent of its own:\
         \n\n```\
         \ncomet-board new \"<piece title>\"{repo_flag} --body - --dispatch <<'EOF'\
         \n<the brief that piece's agent starts from: the goal, the \
         constraints, the seam it must honour. Say it is part of {identifier}, \
         so the tracker shows the fan-out.>\
         \nEOF\
         \n```\
         \n\nThe body is everything its agent will know, so write it for a \
         stranger. Provenance rides along on its own — the board reads your \
         chat id from COMET_BOARD_CHAT_ID and records each release as yours — \
         and this chat is prompted when a piece settles or blocks. After \
         releasing, wait for the pieces (`comet-board wait \
         --blocked-is-settled --timeout <secs>`) or say plainly that you are \
         leaving them running.\
         \n\nKeep for yourself the part that needed the whole picture — the \
         shared foundation, the seam the pieces meet at, the integration — and \
         commit and push it here as usual. Your own attempt still settles the \
         ordinary way, on a pull request or pushed commits: a chat that \
         releases everything and pushes nothing reads as an agent still \
         working, until the clock cap ends it as failed. A piece that builds \
         on what you keep is written without `--dispatch` and released with \
         `comet-board dispatch --task <its id> --onto {identifier}` after you \
         have pushed.\
         \n\nIf the work does not honestly split into pieces a stranger could \
         carry, do it all here and say in the pull request description why it \
         did not."
    )
}

/// Resolve the branch name for an attempt.
///
/// The template's answer, unless this task already holds a branch under the
/// same template — then that one, whatever it is called.
///
/// The reuse is gh#364's cost of admission. A branch built from the identifier
/// and the repo was built from two things that never change; one built from the
/// *title* is built on a field the tracker's owner can edit at any moment, and
/// nothing warns the board when they do. Without this, an issue renamed between
/// two attempts would send the retry to a fresh branch cut from base, leaving
/// the first attempt's commits (and its pull request) on a branch nothing on the
/// board points at any more — which is precisely the promise
/// `Repos::create_worktree_on` makes and keeps: *a retry must land on the
/// previous attempt's commits, never rebase them onto a newer base.*
///
/// Same-template is decided by the stem — the template with the descriptive
/// half emptied, `board/gh-341` — because that half is the only part allowed to
/// move. A branch that does not start with the stem was named by a different
/// template (a route edited between attempts) or by nothing here at all, and
/// reusing it would be a guess. Attempts recorded before this issue land on the
/// same rule for free: `board/gh-341-comet-board` starts with `board/gh-341`,
/// so a task in flight when the box updates keeps the branch it is working on.
pub fn resolve_branch(cfg: &RoutingConfig, route: &Route, task: &Task) -> String {
    let vars = prompt_vars(task, "", &route.workspace);
    let template = cfg.branch_template(route);
    let branch = interpolate(template, &vars);
    let stem = {
        let mut vars = vars.clone();
        vars.insert("identifier_lower", slugify(&task.identifier));
        vars.insert("title_slug", String::new());
        interpolate(template, &vars)
    };
    task.attempts
        .iter()
        .rev()
        .filter_map(|attempt| attempt.branch.as_deref())
        .find(|held| {
            *held == stem || held.strip_prefix(&stem).is_some_and(|t| t.starts_with('-'))
        })
        .map(str::to_string)
        .unwrap_or(branch)
}

/// The space a route's `workspace` names, resolved by the caller — the board
/// core knows comet spaces only as (id, host device, path).
#[derive(Debug, Clone)]
pub struct SpaceRef {
    pub id: String,
    pub device_id: String,
    /// The space's folder on the host device — the dispatch's repo root.
    pub path: String,
}

/// Refuse a dispatch that would exceed the space's concurrency cap.
///
/// `max_concurrent_per_workspace` counts **live attempts per space** — the key
/// keeps herdr-board's spelling, the count is comet's. `blocked` attempts count
/// too: they still hold a chat, and the cap exists to bound simultaneous
/// agents, not simultaneous progress.
pub fn check_capacity(db: &Db, cfg: &RoutingConfig, route: &Route) -> Result<()> {
    let live = db.live_count_in_workspace(&route.workspace)?;
    let cap = cfg.max_concurrent(route);
    if live >= cap {
        bail!(
            "space `{}` is at {live} of {cap} working — cancel one first",
            route.workspace
        );
    }
    Ok(())
}

/// Who a dispatch will bill, and the refusal the route's `billing_guard` owes
/// it (gh#101).
///
/// Sits beside [`check_capacity`] in `handle_dispatch` and for the same reason:
/// both are refusals a dispatch owes its caller *before* anything is created,
/// and a `require-own` refusal that left a `failed` attempt row behind would
/// cost the operator exactly the cleanup this guard exists to spare them.
///
/// `account_email` is the engine's answer to "whose login is this slot" —
/// taken as a closure because which logins a device has saved is engine
/// knowledge and this crate has no view of them. It is asked at most once, and
/// only when the run has a harness to spend a subscription on.
///
/// Returns the verdict even when it releases: the caller records `billed_to` on
/// the attempt and prints [`Billing::warning`] under `warn`.
pub fn check_billing(
    cfg: &RoutingConfig,
    route: &Route,
    origin: &DispatchOrigin,
    overrides: &DispatchOverrides,
    account_email: impl FnOnce(comet_proto::HarnessId, Option<&str>) -> Option<String>,
) -> Result<Option<Billing>> {
    // No harness means a runtime that maps to none, which `build_spec` refuses
    // by name a moment later; there is no subscription to reason about.
    let Some(harness) = effective_harness(route, overrides) else {
        return Ok(None);
    };
    let slot = effective_account(route, overrides);
    let billing = Billing {
        billed_to: account_email(harness, slot),
        dispatcher: origin.attribution(),
        harness,
        // The route's `account` counts as named: it is a decision somebody
        // wrote down, and the silent fall to the box's own login is the case
        // where nothing anywhere said which subscription pays.
        named_slot: slot.is_some(),
        is_owner: origin.is_box_owner(),
    };
    let acknowledged = crate::billing::acknowledges(
        overrides.bill.as_deref(),
        slot,
        billing.billed_to.as_deref(),
    );
    crate::billing::guard(cfg.billing_guard(Some(route)), &billing, acknowledged)?;
    Ok(Some(billing))
}

/// Where a dispatch came from (gh#74, gh#161).
///
/// One struct rather than four parameters because they all answer one question
/// — who released this — at four different strengths:
///
/// - `chat` the board can *check*: it looks the chat up in its own records,
///   which is what [`dispatcher_for`] does below;
/// - `verified` is the only *credential* here: the identity the edge Worker
///   checked and the relay stamped onto the frame, which no frontend can write
///   (gh#161). Absent means the call carried no relay stamp, which is a fact
///   too — it came in over this box's own IPC port;
/// - `device` and `user` stay claims, exactly as §gh#74 described them. `user`
///   is the fallback for the local case and the record for everything else.
///
/// Whose subscription a run spends is still the explicit `account` (gh#59),
/// never inferred from any of these. What `verified` buys is the *comparison*:
/// `require-own` can now refuse on an identity the box did not have to take
/// anybody's word for — see [`crate::billing`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchOrigin {
    /// The dispatching chat's id — provenance, never authority.
    pub chat: Option<String>,
    /// The device the dispatch was issued from.
    pub device: Option<String>,
    /// Who the dispatching frontend says is signed in there: an email when it
    /// knows one, else the user id. A claim.
    pub user: Option<String>,
    /// Who the *edge* says made this call, when it came over the relay.
    pub verified: Option<VerifiedCaller>,
}

/// The dispatcher as the edge verified them, resolved by the receiving device
/// (gh#161).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedCaller {
    /// The WorkOS user id the relay stamped on the frame. Never an email: the
    /// edge verifies a JWT `sub`, and putting a name to it is the box's job.
    pub user_id: String,
    /// That id resolved to an address — from the box's own session when the
    /// caller *is* the box's user, from the workspace roster otherwise.
    /// `None` when neither could answer, which is a refusal under `require-own`
    /// rather than a licence to fall back to the claim.
    pub email: Option<String>,
    /// Is this the identity the box itself runs as? What decides whether a
    /// dispatch that names no account is spending its own plan or somebody's.
    pub is_owner: bool,
}

impl DispatchOrigin {
    /// A dispatch on behalf of a chat, with no device or user named — what the
    /// CLI sends, and what the tests mean by "an agent released it".
    pub fn via(chat: impl Into<String>) -> DispatchOrigin {
        DispatchOrigin {
            chat: Some(chat.into()),
            ..DispatchOrigin::default()
        }
    }

    /// Who released this, at the strongest form available — the projection the
    /// billing guard and the attempt row both read.
    ///
    /// A verified stamp wins outright, and **discards the claim** rather than
    /// merging with it: the two are answers to the same question from sources
    /// of different worth, and a frontend that sends a `viaUser` naming
    /// somebody else must not be able to change what happens by sending it.
    /// The claim survives only inside [`Attribution::Unnamed`], where it is
    /// kept as a record and compared against nothing.
    pub fn attribution(&self) -> Attribution {
        let claimed = self.user.clone().filter(|u| !u.trim().is_empty());
        match &self.verified {
            Some(caller) => match caller.email.clone().filter(|e| !e.trim().is_empty()) {
                Some(email) => Attribution::Verified(email),
                None => Attribution::Unnamed {
                    user_id: caller.user_id.clone(),
                    claimed,
                },
            },
            None => claimed.map(Attribution::Claimed).unwrap_or_default(),
        }
    }

    /// Was this dispatch issued by the identity the box runs as?
    ///
    /// True for every call with no relay stamp: nothing but the box's own
    /// processes can reach its IPC port, so the operator at the box *is* the
    /// box. Otherwise it is the edge's answer, not the caller's.
    pub fn is_box_owner(&self) -> bool {
        self.verified.as_ref().is_none_or(|c| c.is_owner)
    }
}

/// Who released a task, resolved from the dispatching chat id (`via`) — the
/// provenance decision, herdr-board's `dispatcher_from` minus panes.
///
/// The chat id is the identity, exactly as the pane id was: every harness run
/// carries `COMET_BOARD_CHAT_ID`, whether or not the board dispatched the chat
/// (see `crate::runtime`'s table), so `comet-board dispatch` passes it as
/// `via` without anyone threading ids by hand. From there:
///
/// - a live attempt owning that chat is a board-dispatched agent, and names
///   its **task** as the parent — the chain keeps the richer `via LIN-138`
///   label rather than dropping to a chat id;
/// - a chat the board never dispatched (the usual long-lived orchestrator, the
///   case AGE-24 existed for) is still an agent, recorded by its chat alone —
///   `chat_alive` is comet's answer to "does the pane hold an agent";
/// - a chat that is archived or gone is not claimed as an agent: recording it
///   would hand any future notifier an address that answers for nobody.
///
/// No `via` is the operator. `chat_alive` is taken as a closure so the lookup
/// runs only when it is the deciding fact — a chat a live attempt owns is
/// settled from the board's own records.
pub fn dispatcher_for(
    db: &Db,
    via: Option<&str>,
    chat_alive: impl FnOnce(&str) -> bool,
) -> Dispatcher {
    let Some(chat) = via.filter(|c| !c.is_empty()) else {
        return Dispatcher::Operator;
    };
    let live = db.live_attempt_for_pane(chat).ok().flatten();
    let is_agent = live.is_some() || chat_alive(chat);
    Dispatcher::agent(live.map(|a| a.task_id), is_agent.then(|| chat.to_string()))
}

/// The short name for a dispatcher, strongest first: the parent's issue
/// identifier when the board dispatched it too, then the human the board
/// recorded behind the release, and only then the raw chat id (gh#232).
///
/// `user` is [`Attribution::name`] — who released this, as the attempt row
/// stores it. It sits *above* the chat id rather than below it because a chat
/// id is legible exactly when it resolves to an issue identifier: when it does
/// not, it is a UUID, and a UUID is the least useful of the three facts on
/// hand. The comment used to print one in place of an address the board was
/// already holding. The id keeps its place as the last resort it was meant to
/// be — an agent-issued dispatch with nobody signed in still names the chain.
///
/// `None` is a dispatch with none of the three, named by the surrounding copy
/// rather than by this.
pub fn dispatcher_name(db: &Db, d: &Dispatcher, user: Option<&str>) -> Option<String> {
    let named = || {
        user.map(str::to_string)
            .filter(|u: &String| !u.trim().is_empty())
    };
    match d {
        Dispatcher::Operator => named(),
        Dispatcher::Agent { task, pane } => task
            .as_deref()
            // A reaped parent leaves an id with no row behind it; the id is
            // still the truth we have, and naming it beats saying nothing.
            .map(|id| {
                db.get_task(id)
                    .ok()
                    .flatten()
                    .map(|t| t.identifier)
                    .unwrap_or_else(|| id.to_string())
            })
            .or_else(named)
            .or_else(|| pane.clone()),
    }
}

/// Resolve the route for a task, with the refusals a dispatch owes its caller
/// spelled out (gone upstream, no matching route).
pub fn route_for<'a>(cfg: &'a RoutingConfig, task: &Task) -> Result<&'a Route> {
    if task.upstream == UpstreamState::Gone {
        bail!(
            "{} no longer exists in {} — its row is kept for the attempts on it, \
             not to dispatch from",
            task.identifier,
            task.source.as_str()
        );
    }
    cfg.resolve(&route_context(task))
        .ok_or_else(|| anyhow::anyhow!("no route for {}", task.identifier))
}

/// Everything the engine needs to release `task` under `route` into `space`.
///
/// The repo root is the *space's* folder, not the route's `repo` key: the route
/// names which space work goes to, and in comet the space owns its path on the
/// host device. `route.repo` is kept as the fallback for a space the workspace
/// doc has not stamped a path for.
///
/// `by_user` is [`DispatchOrigin::user`] — who the dispatching frontend says
/// released this. Provenance, never authority (see [`DispatchOrigin`]): all it
/// decides here is the commit *author*, which is a claim anybody could write by
/// hand anyway, and the alternative is a teammate's work landing under the
/// box's name (gh#107).
///
/// The spec's `base` is the one place a *per-dispatch* answer overrides the
/// route (gh#285): [`DispatchOverrides::base`] when the release named one, the
/// route's `base` key otherwise. It is one value doing two jobs — where the
/// branch is cut and where the pull request is aimed — and it has to stay one,
/// or a layer cut from its sibling would open a request against trunk carrying
/// the sibling's commits (gh#284).
pub fn build_spec(
    cfg: &RoutingConfig,
    route: &Route,
    task: &Task,
    space: &SpaceRef,
    overrides: &DispatchOverrides,
    by_user: Option<&str>,
) -> Result<DispatchSpec> {
    // Two decomposition asks in two different dimensions is not a bigger ask,
    // it is an ambiguous one: `--stack` layers this attempt's own pull
    // requests, `--decompose` releases pieces to other agents, and a brief
    // carrying both blocks reads as "split this twice" with no rule for which
    // cut comes first. Refused like `--base` with `--onto`, and for the same
    // reason: the pair is a contradiction, not a combination.
    if overrides.stack && overrides.decompose {
        bail!(
            "this dispatch asks for a stack and a decomposition at once — \
             `--stack` layers one attempt's own pull requests, `--decompose` \
             releases pieces to other agents; ask for one shape at a time"
        );
    }
    let runtime = overrides.runtime.as_deref().unwrap_or(&route.runtime);
    let harness = harness_for_runtime(runtime).ok_or_else(|| {
        anyhow::anyhow!(
            "runtime `{runtime}` is not a comet harness; expected one of: {}",
            crate::runtime::RUNTIME_NAMES.join(", ")
        )
    })?;
    let branch = resolve_branch(cfg, route, task);
    let repo_path = if space.path.is_empty() {
        route.repo_path().to_string_lossy().into_owned()
    } else {
        space.path.clone()
    };
    // Which repo the agent's pushes authenticate against (gh#68). The task id
    // when it names one; the checkout's remote otherwise, which is the only
    // thing that can answer for a Linear ticket dispatched into a git space —
    // and is where the branch is going either way.
    let push_repo = crate::model::gh_repo(&task.id)
        .map(str::to_string)
        .or_else(|| crate::git_credentials::repo_for_checkout(&repo_path));
    // And whose name goes on the commits (gh#107). Resolved at dispatch time
    // rather than at commit time because that is when the dispatcher is known:
    // the agent that does the committing knows nothing about who released it.
    let git_author = by_user.and_then(|u| cfg.git_author_for(u));
    // Where this one is cut from, and where its pull request goes: the
    // dispatch's own base when it named one (gh#285), else the route's key.
    // One value for both, exactly as before — a branch cut from a sibling whose
    // request targeted trunk would carry the sibling's commits (gh#284).
    let base = overrides
        .base
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| cfg.base(route))
        .to_string();
    Ok(DispatchSpec {
        identifier: task.identifier.clone(),
        title: task.title.clone(),
        space_id: space.id.clone(),
        device_id: space.device_id.clone(),
        push_repo,
        git_author,
        repo_path,
        prompt: resolve_prompt(
            route,
            task,
            &branch,
            &base,
            overrides.stack,
            overrides.decompose,
        ),
        branch,
        base,
        worktree: true,
        harness,
        model: overrides.model.clone(),
        account: effective_account(route, overrides).map(str::to_string),
        push_contract: None,
        // What the engine's run loop will hold this attempt's turns to
        // (gh#270). Resolved here because the route is here — the loop that
        // enforces it sees events, not config.
        turn_limits: cfg.turn_limits(Some(route)),
        // The route's process-local tool servers (gh#273). This owned copy is
        // the seam between routing.toml and the engine; harness adapters never
        // parse board configuration themselves.
        mcp_servers: cfg.mcp_servers(Some(route)).to_vec(),
        // And whether its runtime is handed the board's conventions in the file
        // it reads on its own (gh#272) — same reasoning again: the executor
        // writes the file, and the executor has no `routing.toml`.
        agent_instructions: cfg.agent_instructions(Some(route)),
    })
}

/// What an attempt actually ran under, as against what its route would have run
/// (gh#232).
///
/// One value rather than two parameters because the pair is one answer, and
/// splitting it is how the runtime got separated from the override in the first
/// place: the writeback took a `&str` that the route could satisfy, so a
/// dispatch under `--runtime opencode` typechecked while reading `claude-code`
/// upstream. `model` is `None` for the harness default, which the board cannot
/// spell and so does not name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RanOn<'a> {
    pub runtime: &'a str,
    pub model: Option<&'a str>,
}

/// Per-dispatch deviations from the route's defaults — what the operator (or an
/// orchestrating agent) chooses at release time over what `routing.toml` says.
///
/// `runtime` is validated against the same [`harness_for_runtime`] mapping as
/// the route's own `runtime` key; the pickers surface exactly the canonical
/// names [`crate::runtime::runtime_options`] offers, and the engine refuses a
/// name that maps to no harness the way it refuses a bad route.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DispatchOverrides {
    /// Runtime name (e.g. `opencode`). `None` = the route's configured runtime.
    pub runtime: Option<String>,
    /// Model id for the chosen harness. `None` = the harness default.
    pub model: Option<String>,
    /// Agent-account slot id to spend. `None` = the route's `account`, and
    /// failing that the device's own CLI login (gh#59). Validated by the
    /// executor rather than here: the board core has no view of which logins
    /// this device has saved, so a wrong id fails the dispatch with the
    /// engine's message instead of a guess from this crate.
    pub account: Option<String>,
    /// An explicit "yes, bill that account" — `comet-board dispatch --bill`,
    /// or a frontend's confirm dialog (gh#101). What `billing_guard =
    /// "require-own"` accepts instead of refusing a cross-billed release.
    ///
    /// Does double duty, because a consent that does not say *what* it
    /// consents to is a flag people set once: a slot id here also selects the
    /// account (ahead of [`account`](Self::account) and the route's), and an
    /// email acknowledges the login the run was going to reach anyway — the
    /// only spelling available when that login is the box's own and has no slot
    /// id. Either way it has to name the account actually billed, or it is a
    /// typo rather than consent (see [`crate::billing::acknowledges`]).
    pub bill: Option<String>,
    /// `--stack`: decompose this task into a stack of layered pull requests
    /// (gh#287). All it does is add [`crate::stacks::stack_brief`] to the
    /// brief — the layers are the agent's to design and `gh stack`'s to create.
    ///
    /// Per dispatch rather than per route, and off by default, because an agent
    /// deciding on its own to open five pull requests where one was expected is
    /// a surprise worth opting into, and because how many concerns a task holds
    /// is a property of the *work* rather than of the class of work a route
    /// describes. A size threshold could come later; it would need a board that
    /// has watched this work first.
    pub stack: bool,
    /// `--decompose`: split this task into tickets and release each to an
    /// agent of its own (gh#340). All it does is add [`decompose_brief`] to
    /// the brief — the pieces are the agent's to design and `comet-board new
    /// --dispatch`'s to release.
    ///
    /// [`stack`](Self::stack)'s twin, one level up: a stack is one attempt's
    /// own pull requests layered, a decomposition is other agents' tickets.
    /// Off by default on the stack flag's own reasoning — five tickets where
    /// one agent was expected is a surprise worth opting into — and per
    /// dispatch because task size is a property of the work. The conventions
    /// block (gh#272) already tells every runtime that delegation goes through
    /// the board *and* that nothing is dispatched without explicit
    /// instruction; this is that instruction, as a flag instead of prompt
    /// prose. Asking for both shapes at once is refused ([`build_spec`]).
    pub decompose: bool,
    /// Cut this dispatch from that branch instead of the route's `base`, and
    /// point its pull request at it (gh#285).
    ///
    /// The route's `base` key was the only way to say where work branches from,
    /// and it is a *route* answer: every task under it gets the same one.
    /// Stacking is the case that does not fit — task B is cut from task A's
    /// branch, which is a fact about one dispatch and about nothing the config
    /// could have known when it was written.
    ///
    /// Read exactly as the route's key is (`Repos::resolve_base`): `origin/` is
    /// stripped, the branch is fetched from origin before the cut, and a fetch
    /// that fails refuses the dispatch rather than falling back to whatever the
    /// space folder happens to be sitting on. So the branch has to be **on
    /// origin** — which, for a sibling's branch, means that sibling has pushed.
    ///
    /// Usually filled in from [`onto`](Self::onto) rather than typed: naming
    /// the parent task is the gesture, and the branch is what it resolves to.
    /// Kept as its own field because a base no task on the board holds — a
    /// release branch, a colleague's branch — is a real thing to want, and
    /// needs no board row to exist.
    pub base: Option<String>,
    /// Stack this dispatch onto that task: cut it from the branch that task's
    /// attempt holds, and record the edge between the two attempts (gh#285).
    ///
    /// A task reference rather than a branch, because that is the gesture —
    /// "a follow-up on this" — and because a branch alone loses the thing the
    /// dependents need: which *attempt* the child was cut from. Resolved by
    /// [`stack_parent`], which is also where a reference naming no task, no
    /// attempt or no branch is refused.
    ///
    /// The recorded edge is also what turns the chain into a **GitHub stack**
    /// (gh#387), on a later cycle rather than here: a stack is made of pull
    /// requests, and a dispatch that has not run yet has not opened one. See
    /// [`crate::stacks::unlinked`] for the request and
    /// [`crate::sync::SyncEngine::link_dispatched_stacks`] for the sweep that
    /// sends it — which is why this field is not merely a nicer spelling of
    /// [`base`](Self::base): a raw base leaves no edge, and a chain with no edge
    /// is one nobody ever tells GitHub about.
    pub onto: Option<String>,
}

/// The attempt a stacked dispatch cuts from (gh#285) — what [`onto`] resolves
/// to, and the edge the child's row records.
///
/// [`onto`]: DispatchOverrides::onto
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackParent {
    /// The attempt row itself. This is the edge worth keeping: a branch name
    /// stops being true when the parent merges and its branch is deleted, and
    /// an attempt id does not.
    pub attempt: i64,
    /// The task that attempt belongs to.
    pub task_id: String,
    /// …and its identifier, for the messages and the log line. A reader chasing
    /// a chain wants `gh#12`, not a row id.
    pub identifier: String,
    /// The branch that attempt holds — this dispatch's [`base`].
    ///
    /// [`base`]: DispatchOverrides::base
    pub branch: String,
}

/// The task a caller named, by id (`gh:owner/repo#12`) or by identifier
/// (`gh#12`).
///
/// Both, because both are things a caller has to hand: the RPC callers and the
/// board's own surfaces hold ids, and anybody *typing* one holds the identifier
/// they read off the board — or, for a dispatched agent, the identifier its
/// brief opened with. That second case is gh#339's other half: the brief said
/// `gh#339`, `claim --task` compared strings against the id column, and an
/// agent that did everything the skill asked was answered "gh#339 is not on the
/// board". A contract whose one verb refuses the name it handed you is not a
/// contract anybody can keep.
///
/// The id is tried first and whole. An identifier that matches more than one
/// row is refused rather than guessed — two sources can spell the same name,
/// and the point of the exact id is to be the thing that says which. The
/// refusal names **every** matching id, because naming one of them would be a
/// disambiguator picked by iteration order: right half the time, and wrong in
/// a way the reader cannot see.
pub fn task_by_reference<'a>(tasks: &'a [Task], reference: &str) -> Result<&'a Task> {
    let reference = reference.trim();
    if let Some(task) = tasks.iter().find(|t| t.id == reference) {
        return Ok(task);
    }
    let named: Vec<&Task> = tasks
        .iter()
        .filter(|t| t.identifier.eq_ignore_ascii_case(reference))
        .collect();
    match named.as_slice() {
        [] => bail!("`{reference}` is not a task on the board"),
        [only] => Ok(only),
        // Every id, never one of them. A message that hands the reader a
        // single id and calls it the disambiguator is a message that can be
        // wrong: it would be whichever row the scan reached first, and a
        // reviewer who pastes it lands on the other task's review believing
        // they were told which to use.
        several => bail!(
            "`{reference}` names {} tasks on the board — pass the task id to \
             say which: {}",
            several.len(),
            several
                .iter()
                .map(|t| format!("`{}`", t.id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Resolve a dispatch's `onto` to the attempt it stacks on.
///
/// `reference` is a task id or an identifier, per [`task_by_reference`].
///
/// Which attempt: the **live** one if there is one, else the most recent
/// attempt holding a branch. Both are the same answer to "the branch this task's
/// work is on" — the difference is only whether the agent is still in it, and a
/// task in review is exactly the case the gesture was named for. An attempt
/// with no branch (dispatched into the space folder rather than a worktree) is
/// skipped: there is nothing to cut from.
///
/// Refusals, all of them before anything is created:
/// - the reference names no row, or names several;
/// - the task has no attempt at all, or none that holds a branch — nothing has
///   been cut yet, so there is no sibling to stack on;
/// - the task is the one being dispatched, which would have the branch be its
///   own base.
pub fn stack_parent(db: &Db, reference: &str, dispatching: &str) -> Result<StackParent> {
    let reference = reference.trim();
    if reference.is_empty() {
        bail!("--onto needs a task to stack on");
    }
    let tasks = db.load_tasks()?;
    let task = task_by_reference(&tasks, reference)?;
    if task.id == dispatching {
        bail!(
            "{} cannot stack on itself — `--onto` names the *other* task whose \
             branch this one is cut from",
            task.identifier
        );
    }
    let attempt = task
        .live_attempt()
        .filter(|a| a.branch.is_some())
        .or_else(|| task.attempts.iter().rev().find(|a| a.branch.is_some()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "{} has no attempt holding a branch — dispatch it first, and \
                 stack on it once it has one",
                task.identifier
            )
        })?;
    Ok(StackParent {
        attempt: attempt.id,
        task_id: task.id.clone(),
        identifier: task.identifier.clone(),
        branch: attempt
            .branch
            .clone()
            .expect("filtered to attempts holding a branch"),
    })
}

/// The parent a dispatch stacks on, resolved from its overrides — `None` for
/// the ordinary dispatch off the route's `base`, and for one that names a raw
/// [`base`](DispatchOverrides::base) with no task behind it.
///
/// Naming both is refused rather than ranked. They are two spellings of the
/// same decision, and a caller that sent both has said two things: quietly
/// obeying one of them is how a dispatch ends up cut from a branch nobody
/// asked for.
pub fn stack_parent_for(
    db: &Db,
    task_id: &str,
    overrides: &DispatchOverrides,
) -> Result<Option<StackParent>> {
    let base = overrides.base.as_deref().filter(|b| !b.trim().is_empty());
    let onto = overrides.onto.as_deref().filter(|o| !o.trim().is_empty());
    match (base, onto) {
        (Some(base), Some(onto)) => bail!(
            "this dispatch names both a base (`{base}`) and a task to stack on \
             (`{onto}`) — say one or the other"
        ),
        (_, Some(onto)) => stack_parent(db, onto, task_id).map(Some),
        _ => Ok(None),
    }
}

impl DispatchOverrides {
    /// The agent-account slot this dispatch will spend, before the route is
    /// consulted: an explicit `--bill <slot>` first (it names the payer, which
    /// is the strongest statement available), then `--account`. An email in
    /// `bill` selects nothing — it acknowledges a login the dispatch was
    /// already headed for.
    pub fn account_override(&self) -> Option<&str> {
        self.bill
            .as_deref()
            .filter(|b| crate::billing::bill_names_a_slot(b))
            .or(self.account.as_deref())
            .filter(|a| !a.is_empty())
    }
}

/// The agent-account slot a dispatch of `route` under `overrides` will spend —
/// the same fallback chain [`build_spec`] applies, available before a space has
/// been resolved so the billing guard can run beside the concurrency cap.
///
/// `None` is the device's own CLI login.
pub fn effective_account<'a>(
    route: &'a Route,
    overrides: &'a DispatchOverrides,
) -> Option<&'a str> {
    overrides
        .account_override()
        .or(route.account.as_deref())
        .filter(|a| !a.is_empty())
}

/// The harness a dispatch of `route` under `overrides` will run on, and so
/// which subscription it spends. `None` for a runtime that maps to no harness,
/// which [`build_spec`] refuses by name a moment later.
pub fn effective_harness(
    route: &Route,
    overrides: &DispatchOverrides,
) -> Option<comet_proto::HarnessId> {
    harness_for_runtime(overrides.runtime.as_deref().unwrap_or(&route.runtime))
}

/// Does `space` answer to the name a route's `workspace` key uses? Comet spaces
/// display as their explicit name when renamed, else basename(path) — match
/// both, so routing.toml can say either.
pub fn space_matches(name: Option<&str>, path: &str, workspace: &str) -> bool {
    if name.is_some_and(|n| n == workspace) {
        return true;
    }
    Path::new(path)
        .file_name()
        .is_some_and(|base| base.to_string_lossy() == workspace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Source;

    fn task() -> Task {
        Task {
            id: "gh:owner/widget#7".into(),
            source: Source::Github,
            source_id: "7".into(),
            identifier: "gh#7".into(),
            title: "Fix the flaky retry".into(),
            body: Some("It flakes.".into()),
            url: "https://github.com/owner/widget/issues/7".into(),
            labels: vec![],
            state: crate::model::BoardState::Ready,
            source_state: None,
            upstream: UpstreamState::Unstarted,
            local_done: false,
            pr_url: None,
            pr_number: None,
            pr_open: false,
            pr_merged: false,
            pr_mergeable: None,
            pr_base_ref: None,
            pr_head_ref: None,
            pr_stack: None,
            pr_changes_requested: None,
            updated_at: String::new(),
            synced_at: String::new(),
            attempts: vec![],
        }
    }

    fn route() -> Route {
        toml::from_str(
            r#"
            workspace = "widget"
            repo = "~/dev/widget"
            runtime = "claude-code"
            "#,
        )
        .unwrap()
    }

    fn space() -> SpaceRef {
        SpaceRef {
            id: "space-1".into(),
            device_id: "dev-1".into(),
            path: "/home/x/dev/widget".into(),
        }
    }

    /// gh#287. The flag's whole effect is the brief: the layers are the agent's
    /// to design, and the branch names in front of it are the board's.
    #[test]
    fn a_dispatch_that_asks_for_a_stack_says_so_in_the_brief() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides {
                stack: true,
                ..DispatchOverrides::default()
            },
            None,
        )
        .unwrap();
        assert!(spec.prompt.contains("gh stack init board/gh-7-fix-flaky-retry"));
        assert!(spec.prompt.contains("gh stack add board/gh-7-fix-flaky-retry-2"));
        // And the task's own brief is still in front of it.
        assert!(spec.prompt.contains("Fix the flaky retry (gh#7)"));
    }

    /// And an ordinary dispatch pays nothing for the feature — not a sentence,
    /// which is the point of asking rather than of a threshold.
    #[test]
    fn an_ordinary_dispatch_never_mentions_stacks() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert!(!spec.prompt.contains("gh stack"));
    }

    // ---- the brief authorizes a fan-out only when asked (§gh#340) ---------

    /// gh#340. Same shape as `--stack`: the flag's whole effect is the brief.
    /// The command it hands over names this task's own repo and identifier,
    /// because those are the two things a piece's ticket must carry that the
    /// decomposing agent cannot be trusted to guess.
    #[test]
    fn a_dispatch_that_asks_for_decomposition_says_so_in_the_brief() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides {
                decompose: true,
                ..DispatchOverrides::default()
            },
            None,
        )
        .unwrap();
        assert!(
            spec.prompt.contains("asks you to **decompose**"),
            "{}",
            spec.prompt
        );
        // The command, with the task's own repo interpolated so the pieces
        // land beside the parent issue on a box that watches several.
        assert!(
            spec.prompt
                .contains("--repo owner/widget --body - --dispatch"),
            "{}",
            spec.prompt
        );
        // The follow-up spelling names this task, so a piece that builds on
        // the kept slice stacks on it rather than on a guess.
        assert!(spec.prompt.contains("--onto gh#7"), "{}", spec.prompt);
        // And the task's own brief is still in front of it.
        assert!(spec.prompt.contains("Fix the flaky retry (gh#7)"));
    }

    /// An ordinary dispatch never carries the authorization: the conventions'
    /// standing rule — nothing is dispatched without explicit instruction —
    /// stays in force, and this brief is silent rather than half-permitting.
    #[test]
    fn an_ordinary_dispatch_never_authorizes_a_fan_out() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert!(!spec.prompt.contains("decompose"), "{}", spec.prompt);
        assert!(!spec.prompt.contains("--dispatch"), "{}", spec.prompt);
    }

    /// A Linear task names no repo, so the command stays bare — where a new
    /// ticket lands is `[defaults] new_source`'s answer, not a guess here.
    #[test]
    fn a_linear_decomposition_gets_the_bare_command() {
        let mut task = task();
        task.id = "linear:AGE-14".into();
        task.identifier = "AGE-14".into();
        let prompt = resolve_prompt(&route(), &task, "board/age-14", "origin/HEAD", false, true);
        assert!(
            prompt.contains("comet-board new \"<piece title>\" --body - --dispatch"),
            "{prompt}"
        );
        assert!(!prompt.contains("--repo"), "{prompt}");
    }

    /// Both shapes at once is an ambiguous ask, not a bigger one — refused
    /// before anything is created, like `--base` with `--onto`.
    #[test]
    fn a_stack_and_a_decomposition_at_once_are_refused() {
        let err = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides {
                stack: true,
                decompose: true,
                ..DispatchOverrides::default()
            },
            None,
        )
        .unwrap_err();
        assert!(err.to_string().contains("one shape at a time"), "{err}");
    }

    // ---- the review contract reaches the agent (§gh#339) -----------------

    /// The headline. Twenty-three settled attempts on the box claimed nothing,
    /// with the skill installed the whole time, because the one text that
    /// always arrives asked for a commit, a push and a pull request and never
    /// mentioned the contract the review screen is built on.
    #[test]
    fn every_brief_asks_for_the_claims_the_review_is_made_of() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert!(
            spec.prompt
                .contains("comet-board claim --task gh:owner/widget#7"),
            "{}",
            spec.prompt
        );
        // The fallback too: an agent that finishes without running the verb
        // has still been told where to write the block.
        assert!(
            spec.prompt.contains("fenced block tagged `claims`"),
            "{}",
            spec.prompt
        );
        // …and it *names* that fence rather than spelling one, so a brief
        // quoted back in a closing message cannot be harvested as the agent's
        // own claims. The board must never read its own instruction as an
        // answer to it.
        assert_eq!(crate::claims::find_block(&spec.prompt), None);
    }

    /// And it reaches a route that wrote its own brief, on `pr_base_line`'s
    /// rule: a template is somebody's wording for the *task*, and the contract
    /// is a fact about being dispatched at all. This is the case that produced
    /// the bug — the box's own routes carry custom prompts.
    #[test]
    fn a_route_with_its_own_prompt_is_still_asked() {
        let mut route = route();
        route.prompt = Some("Do {title}. Commit, push, open a PR.".into());
        let prompt = resolve_prompt(&route, &task(), "board/gh-7", "origin/HEAD", false, false);
        assert!(prompt.starts_with("Do Fix the flaky retry."), "{prompt}");
        assert!(
            prompt.contains("comet-board claim --task gh:owner/widget#7"),
            "{prompt}"
        );
    }

    // ---- the credential preflight reaches the first prompt (gh#440) ------

    #[test]
    fn a_workflow_capable_credential_says_both_writes_are_deliverable() {
        let brief = credential_preflight_brief(PushCapabilities {
            contents: WriteCapability::Write,
            workflows: WriteCapability::Write,
            evidence: CapabilityEvidence::AppInstallation,
        });
        assert!(brief.contains("ordinary repository content"), "{brief}");
        assert!(brief.contains("both writable"), "{brief}");
        assert!(brief.contains("installation token"), "{brief}");
    }

    #[test]
    fn missing_workflow_permission_keeps_ordinary_work_and_requires_a_patch_artifact() {
        let brief = credential_preflight_brief(PushCapabilities {
            contents: WriteCapability::Write,
            workflows: WriteCapability::Missing,
            evidence: CapabilityEvidence::ClassicOauthScopes,
        });
        assert!(!brief.contains("stop before changing files"), "{brief}");
        assert!(brief.contains("Do not add, edit, stage, or commit workflow files"));
        assert!(brief.contains("docs/workflow-patches/"), "{brief}");
        assert!(brief.contains("exact apply command"), "{brief}");
        assert!(brief.contains("`workflow` scope"), "{brief}");
    }

    #[test]
    fn unknown_or_absent_content_write_fails_closed_before_work_starts() {
        for capabilities in [
            PushCapabilities {
                contents: WriteCapability::Unknown,
                workflows: WriteCapability::Unknown,
                evidence: CapabilityEvidence::OpaqueToken,
            },
            PushCapabilities::anonymous(),
        ] {
            let brief = credential_preflight_brief(capabilities);
            assert!(brief.contains("stop before changing files"), "{brief}");
            assert!(brief.contains("do not edit, commit, or promise"), "{brief}");
            assert!(brief.contains("Report the credential preflight"), "{brief}");
            let refusal = credential_preflight_refusal(capabilities).expect("must refuse");
            assert!(refusal.contains("cannot be established as deliverable"));
        }
        assert_eq!(
            credential_preflight_refusal(PushCapabilities {
                contents: WriteCapability::Write,
                workflows: WriteCapability::Missing,
                evidence: CapabilityEvidence::AppInstallation,
            }),
            None,
            "missing workflow permission takes the patch-artifact path"
        );
    }

    /// The id is in the brief because nothing else in a dispatched run says
    /// it: no environment variable carries it, and the identifier the brief
    /// opens with is a different string.
    #[test]
    fn the_brief_names_the_id_the_verb_takes_and_not_the_identifier() {
        let prompt = resolve_prompt(&route(), &task(), "board/gh-7", "origin/HEAD", false, false);
        let asked = prompt.split("--task ").nth(1).expect("the verb");
        assert!(asked.starts_with("gh:owner/widget#7"), "{asked}");
    }

    /// A route that wants to place the id itself can: same var, same value.
    #[test]
    fn a_template_can_name_the_task_id() {
        let mut route = route();
        route.prompt = Some("claim against {task_id} when done".into());
        let prompt = resolve_prompt(&route, &task(), "board/gh-7", "origin/HEAD", false, false);
        assert!(
            prompt.starts_with("claim against gh:owner/widget#7 when done"),
            "{prompt}"
        );
    }

    /// The other half of gh#339: the verbs take the name the brief handed
    /// over. An agent that reached for the identifier it was greeted with was
    /// told its own task was not on the board — which reads as the board
    /// having lost the row, not as the wrong spelling.
    #[test]
    fn a_task_answers_to_its_id_and_to_its_identifier() {
        let tasks = vec![task()];
        assert_eq!(
            task_by_reference(&tasks, "gh:owner/widget#7").unwrap().id,
            "gh:owner/widget#7"
        );
        // Typed by hand, so spelled by hand — either case, either spelling.
        for typed in ["gh#7", " GH#7 "] {
            assert_eq!(
                task_by_reference(&tasks, typed).unwrap().id,
                "gh:owner/widget#7",
                "{typed}"
            );
        }
        assert!(task_by_reference(&tasks, "gh#8").is_err());
    }

    /// Two sources can spell one name. Guessing which row a reviewer meant is
    /// how a verdict lands on somebody else's attempt, so the ambiguity is
    /// refused — and the refusal names **every** candidate.
    ///
    /// Naming one would be a disambiguator chosen by iteration order. A reader
    /// told `pass the task id (gh:owner/widget#7)` has been handed something
    /// shaped like the answer to their question, and if theirs was the other
    /// row they paste it and read somebody else's review believing the board
    /// told them to.
    #[test]
    fn an_identifier_two_tasks_share_is_refused_and_every_candidate_is_named() {
        let mut other = task();
        other.id = "gh:owner/gadget#7".into();
        let tasks = vec![task(), other];
        let err = task_by_reference(&tasks, "gh#7").unwrap_err().to_string();
        assert!(err.contains("names 2 tasks"), "{err}");
        for id in ["gh:owner/widget#7", "gh:owner/gadget#7"] {
            assert!(err.contains(id), "{id} missing from: {err}");
        }
        // The exact id is never ambiguous — that is what it is for.
        assert_eq!(
            task_by_reference(&tasks, "gh:owner/gadget#7").unwrap().id,
            "gh:owner/gadget#7"
        );
    }

    #[test]
    fn branch_comes_from_the_template_and_says_what_the_task_is() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(spec.branch, "board/gh-7-fix-flaky-retry");
    }

    /// gh#364. The half a branch used to spend on the repo — which a branch
    /// and its worktree path both already say — now says what the task is.
    #[test]
    fn the_branch_spends_its_descriptive_half_on_the_title() {
        let mut task = task();
        assert_eq!(branch_slug(&task), "gh-7-fix-flaky-retry");

        // A Linear task is qualified the same way; it never carried a repo to
        // lose, and it gains the same description.
        task.id = "linear:LIN-145".into();
        task.identifier = "LIN-145".into();
        task.title = "Altinn retry fails on the second attempt".into();
        assert_eq!(branch_slug(&task), "lin-145-altinn-retry-fails");

        // A title with no content words in it leaves the identifier standing
        // alone — the guarantee gh#357's rule rests on, and cheaper than
        // inventing something.
        task.title = "It is what it is".into();
        assert_eq!(branch_slug(&task), "lin-145");
    }

    /// The cost of admission, paid: the branch now depends on a field the
    /// tracker's owner can edit mid-flight, so a retry has to find the branch
    /// the first attempt is actually on rather than cut a fresh one and orphan
    /// its commits.
    #[test]
    fn a_retry_lands_on_the_branch_the_task_already_holds() {
        let db = Db::open_in_memory().unwrap();
        let cfg = RoutingConfig::default();
        sibling(&db, "gh:owner/widget#7", "gh#7", "board/gh-7-fix-flaky-retry", None);
        let reload = |db: &Db| db.get_task("gh:owner/widget#7").unwrap().unwrap();

        // Renamed upstream after the first attempt: the slug the template
        // would render has moved, and the branch has not.
        let mut task = reload(&db);
        task.title = "Fix the retry that flakes under load".into();
        assert_eq!(
            resolve_branch(&cfg, &route(), &task),
            "board/gh-7-fix-flaky-retry",
            "a renamed issue must not orphan the branch its attempt is on"
        );

        // The same rule carries an attempt made before gh#364 — the old
        // repo-qualified branch shares the identifier stem, so a box that
        // updates mid-flight keeps working where it was working.
        let db = Db::open_in_memory().unwrap();
        sibling(&db, "gh:owner/widget#7", "gh#7", "board/gh-7-widget", None);
        let mut task = reload(&db);
        task.title = "Fix the flaky retry".into();
        assert_eq!(resolve_branch(&cfg, &route(), &task), "board/gh-7-widget");

        // A branch under a different stem is a different template's, and
        // guessing at it would be worse than naming the branch this template
        // asks for.
        let db = Db::open_in_memory().unwrap();
        sibling(&db, "gh:owner/widget#7", "gh#7", "wip/hand-cut", None);
        let mut task = reload(&db);
        task.title = "Fix the flaky retry".into();
        assert_eq!(
            resolve_branch(&cfg, &route(), &task),
            "board/gh-7-fix-flaky-retry"
        );
    }

    #[test]
    fn the_brief_names_task_and_branch() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert!(spec.prompt.contains("Fix the flaky retry (gh#7)"));
        assert!(
            spec.prompt
                .contains("the branch board/gh-7-fix-flaky-retry is prepared")
        );
        assert!(spec.prompt.contains("It flakes."));
    }

    #[test]
    fn the_space_path_is_the_repo_root() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(spec.repo_path, "/home/x/dev/widget");
        assert_eq!(spec.space_id, "space-1");
        assert_eq!(spec.harness, comet_proto::HarnessId::ClaudeCode);
    }

    /// What the agent's `git push` and `gh pr create` authenticate for
    /// (gh#68). A GitHub ticket carries its repo in its own id; nothing has to
    /// touch the checkout to know it.
    #[test]
    fn a_github_task_names_the_repo_its_agent_pushes_to() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(spec.push_repo.as_deref(), Some("owner/widget"));
    }

    /// A Linear ticket names no repo, and the space it dispatches into is the
    /// only thing that can answer. A path that is not a git checkout answers
    /// nothing, which leaves the agent on the box's own credentials.
    #[test]
    fn a_linear_task_falls_back_to_the_checkout_and_tolerates_having_none() {
        let mut t = task();
        t.id = "linear:LIN-142".into();
        t.identifier = "LIN-142".into();
        t.source = Source::Linear;
        let mut s = space();
        s.path = "/nonexistent/not-a-checkout".into();
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &t,
            &s,
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(spec.push_repo, None);
    }

    /// gh#270: the guardrails the engine's run loop enforces are resolved from
    /// the route *here*, because that loop sees events and has no board to ask.
    #[test]
    fn a_dispatch_carries_its_routes_turn_guardrails() {
        let mut cfg = RoutingConfig::default();
        let mut r = route();
        r.max_tool_failures = Some("25".into());
        cfg.defaults.max_tool_calls = "off".into();
        let spec = build_spec(
            &cfg,
            &r,
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(spec.turn_limits.tool_failures, Some(25));
        assert_eq!(spec.turn_limits.tool_calls, None);
    }

    /// gh#272: and so is whether its runtime is handed the conventions, for
    /// the same reason — the executor writes the file beside the config dir it
    /// materialized, with no `routing.toml` anywhere near it.
    #[test]
    fn a_dispatch_carries_whether_its_runtime_is_handed_the_conventions() {
        let cfg = RoutingConfig::default();
        let spec = |r: &crate::config::Route| {
            build_spec(
                &cfg,
                r,
                &task(),
                &space(),
                &DispatchOverrides::default(),
                None,
            )
            .unwrap()
            .agent_instructions
        };
        assert!(spec(&route()), "on unless somebody says otherwise");
        let mut opted_out = route();
        opted_out.agent_instructions = Some(false);
        assert!(!spec(&opted_out));
    }

    /// gh#273: the engine and harnesses have no routing.toml to consult, so the
    /// effective list crosses the same dispatch seam as the turn limits.
    #[test]
    fn a_dispatch_carries_its_routes_mcp_servers() {
        let cfg = RoutingConfig::default();
        let mut r = route();
        r.mcp_servers = Some(vec![comet_proto::McpServer {
            name: "repo".into(),
            command: "repo-mcp".into(),
            args: vec!["--stdio".into()],
        }]);
        let spec = build_spec(
            &cfg,
            &r,
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(spec.mcp_servers, r.mcp_servers.unwrap());
    }

    /// gh#107: the teammate who released the work is the author of what comes
    /// back. Their sign-in email is the key; the case it arrives in is the
    /// frontend's business, not the map's.
    #[test]
    fn a_mapped_dispatcher_becomes_the_author_of_the_attempts_commits() {
        let mut cfg = RoutingConfig::default();
        cfg.users.insert(
            "ana@example.com".into(),
            "22494697+ana@users.noreply.github.com".into(),
        );
        let author = |user: Option<&str>| {
            build_spec(
                &cfg,
                &route(),
                &task(),
                &space(),
                &DispatchOverrides::default(),
                user,
            )
            .unwrap()
            .git_author
        };

        let ana = author(Some("Ana@Example.com")).expect("the map names her");
        assert_eq!(ana.email, "22494697+ana@users.noreply.github.com");
        assert_eq!(ana.name, "ana");

        // Everyone else authors as the box, which is what every dispatch did
        // before the map existed — including a dispatch that named nobody.
        assert_eq!(author(Some("sam@example.com")), None);
        assert_eq!(author(Some("")), None);
        assert_eq!(author(None), None);
    }

    #[test]
    fn an_unknown_runtime_is_refused_by_name() {
        let mut r = route();
        r.runtime = "gemini".into();
        let err = build_spec(
            &RoutingConfig::default(),
            &r,
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("gemini"), "{err}");
    }

    #[test]
    fn a_dispatch_runtime_override_wins_over_the_route() {
        // The route says claude-code; the release says opencode — the harness
        // follows the override, the brief is unchanged.
        let overrides = DispatchOverrides {
            runtime: Some("opencode".into()),
            model: None,
            account: None,
            bill: None,
            stack: false,
            ..DispatchOverrides::default()
        };
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &overrides,
            None,
        )
        .unwrap();
        assert_eq!(spec.harness, comet_proto::HarnessId::Opencode);
        assert_eq!(spec.model, None);
        assert!(spec.prompt.contains("Fix the flaky retry (gh#7)"));
    }

    #[test]
    fn a_dispatch_model_override_is_carried_into_the_spec() {
        let overrides = DispatchOverrides {
            runtime: None,
            model: Some("sonnet-4".into()),
            account: None,
            bill: None,
            stack: false,
            ..DispatchOverrides::default()
        };
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &overrides,
            None,
        )
        .unwrap();
        assert_eq!(spec.model.as_deref(), Some("sonnet-4"));
        // No runtime override: the route's claude-code stays.
        assert_eq!(spec.harness, comet_proto::HarnessId::ClaudeCode);
    }

    #[test]
    fn a_bad_dispatch_runtime_override_is_refused_by_name() {
        let overrides = DispatchOverrides {
            runtime: Some("nonesuch".into()),
            model: None,
            account: None,
            bill: None,
            stack: false,
            ..DispatchOverrides::default()
        };
        let err = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &overrides,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("nonesuch"), "{err}");
        assert!(
            err.contains("claude-code"),
            "the known list is named: {err}"
        );
    }

    /// Whose subscription pays: the dispatch's choice beats the route's, and
    /// a route with none leaves the run on the device's own CLI login.
    #[test]
    fn the_account_falls_back_from_the_dispatch_to_the_route_to_nothing() {
        let plain = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(plain.account, None);

        let mut routed = route();
        routed.account = Some("8f2c1d0a7b6e4539".into());
        let from_route = build_spec(
            &RoutingConfig::default(),
            &routed,
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(from_route.account.as_deref(), Some("8f2c1d0a7b6e4539"));

        let overridden = build_spec(
            &RoutingConfig::default(),
            &routed,
            &task(),
            &space(),
            &DispatchOverrides {
                account: Some("0011223344556677".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap();
        assert_eq!(overridden.account.as_deref(), Some("0011223344556677"));
    }

    /// `account = ""` is somebody clearing the key, not naming a login called
    /// empty string — and an empty id would fail every dispatch on the route.
    #[test]
    fn an_empty_account_is_no_account() {
        let mut routed = route();
        routed.account = Some(String::new());
        let spec = build_spec(
            &RoutingConfig::default(),
            &routed,
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(spec.account, None);
    }

    /// Where a dispatch's branch is cut from (gh#67): the remote's default
    /// branch unless somebody says otherwise, the route's `base` over the
    /// defaults' — and never the space folder's HEAD, which on an always-on box
    /// is whatever ran there last.
    #[test]
    fn the_base_ref_comes_from_the_route_then_the_defaults() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(spec.base, "origin/HEAD");

        let cfg: RoutingConfig = toml::from_str(
            r#"
            [defaults]
            base = "origin/develop"
            "#,
        )
        .unwrap();
        let from_defaults = build_spec(
            &cfg,
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(from_defaults.base, "origin/develop");

        let mut r = route();
        r.base = Some("release".into());
        let from_route = build_spec(
            &cfg,
            &r,
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(from_route.base, "release");
    }

    /// gh#284: which spellings of `base` name a branch the agent has to be
    /// told about, and which are the repo's own default under another name.
    #[test]
    fn only_a_named_base_is_a_pull_request_base() {
        assert_eq!(pr_base("origin/HEAD"), None);
        assert_eq!(pr_base("HEAD"), None);
        assert_eq!(pr_base(""), None);
        assert_eq!(pr_base("  "), None);
        // `--base origin/release-1.x` is not a base GitHub accepts.
        assert_eq!(pr_base("origin/release-1.x"), Some("release-1.x"));
        assert_eq!(pr_base("release-1.x"), Some("release-1.x"));
        assert_eq!(pr_base(" develop "), Some("develop"));
    }

    /// gh#284: the whole point. A route branching from a release branch used
    /// to say nothing about it, and `gh pr create` targets the repo default
    /// unasked — a request to merge someone else's commits into `main`.
    #[test]
    fn a_brief_on_a_non_default_base_names_the_pull_request_base() {
        let cfg: RoutingConfig = toml::from_str(
            r#"
            [defaults]
            base = "origin/release-1.x"
            "#,
        )
        .unwrap();
        let spec = build_spec(
            &cfg,
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert!(
            spec.prompt.contains("gh pr create --base release-1.x"),
            "brief must name the base: {}",
            spec.prompt
        );
        // The stripped name, never the remote-qualified one.
        assert!(!spec.prompt.contains("origin/release-1.x"));
        // And it is an addition, not a replacement.
        assert!(spec.prompt.contains("Fix the flaky retry (gh#7)"));
    }

    /// The default base *is* what `gh pr create` picks on its own, so saying
    /// so would be a line of brief spent on nothing.
    #[test]
    fn a_brief_on_the_repo_default_says_nothing_about_a_base() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert!(!spec.prompt.contains("--base"));
    }

    /// A route's own `prompt` is somebody's wording for the task; where the
    /// pull request goes is a fact about the dispatch they could not have
    /// known when they wrote it. So the line lands on custom briefs too.
    #[test]
    fn a_custom_brief_still_learns_its_base() {
        let cfg: RoutingConfig = toml::from_str(
            r#"
            [defaults]
            base = "develop"
            "#,
        )
        .unwrap();
        let mut r = route();
        r.prompt = Some("Do {identifier}, please.".into());
        let spec = build_spec(
            &cfg,
            &r,
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert!(spec.prompt.starts_with("Do gh#7, please."));
        assert!(spec.prompt.contains("gh pr create --base develop"));
    }

    #[test]
    fn a_gone_task_cannot_resolve_a_route() {
        let mut t = task();
        t.upstream = UpstreamState::Gone;
        assert!(route_for(&RoutingConfig::default(), &t).is_err());
    }

    #[test]
    fn spaces_match_by_name_or_basename() {
        assert!(space_matches(None, "/home/x/dev/widget", "widget"));
        assert!(space_matches(Some("widget"), "/anything", "widget"));
        assert!(!space_matches(None, "/home/x/dev/other", "widget"));
    }

    // ---- concurrency + provenance (§dispatch-pipeline) --------------------

    use crate::db::{Db, NewAttempt, UpsertTask};
    use crate::model::{Dispatcher, Outcome};

    /// A task row plus a live attempt holding `chat` in `workspace` — a
    /// board-dispatched agent, as far as the records go.
    fn working_agent(db: &Db, task_id: &str, chat: &str, workspace: &str) {
        db.upsert_task(&UpsertTask {
            id: task_id.into(),
            source: Source::Linear,
            source_id: "u".into(),
            identifier: task_id.trim_start_matches("linear:").into(),
            title: "parent".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: None,
            upstream: UpstreamState::Started,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let a = db
            .insert_attempt(&NewAttempt {
                automation: None,
                automation_owner: None,
                stacked_on: None,
                task_id: task_id.into(),
                pane_id: None,
                workspace: workspace.into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: None,
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: None,
                account: None,
                repo_path: None,
                dispatched_by_device: None,
                dispatched_by_user: None,
                dispatched_by_verified: false,
                billed_to: None,
            })
            .unwrap();
        db.set_attempt_pane(a, chat).unwrap();
    }

    #[test]
    fn capacity_counts_live_attempts_in_the_route_space() {
        let db = Db::open_in_memory().unwrap();
        let cfg: RoutingConfig = toml::from_str(
            r#"
            [defaults]
            max_concurrent_per_workspace = 2
            "#,
        )
        .unwrap();
        let r = route();
        working_agent(&db, "linear:LIN-1", "chat-1", "widget");
        working_agent(&db, "linear:LIN-2", "chat-2", "widget");
        // Another space's attempts do not count against this route.
        working_agent(&db, "linear:LIN-3", "chat-3", "other");

        let err = check_capacity(&db, &cfg, &r).unwrap_err().to_string();
        assert!(err.contains("2 of 2"), "{err}");
        assert!(err.contains("widget"), "{err}");

        // A closed attempt frees its slot.
        let live = db.live_attempt_for_pane("chat-1").unwrap().unwrap();
        db.close_attempt(live.id, Outcome::Done).unwrap();
        assert!(check_capacity(&db, &cfg, &r).is_ok());
    }

    /// gh#161: where the claim stops mattering. A relayed frame carries an
    /// identity the frontend could not write, and `attribution()` is the one
    /// place that decides which of the two the rest of the board sees.
    #[test]
    fn a_verified_stamp_replaces_the_claim_rather_than_joining_it() {
        let lying = DispatchOrigin {
            // What a frontend willing to misreport its user would send.
            user: Some("brede@tally.no".into()),
            verified: Some(VerifiedCaller {
                user_id: "user_ana".into(),
                email: Some("ana@example.com".into()),
                is_owner: false,
            }),
            ..DispatchOrigin::default()
        };
        assert_eq!(
            lying.attribution(),
            Attribution::Verified("ana@example.com".into())
        );
        assert_eq!(lying.attribution().email(), Some("ana@example.com"));
        assert!(!lying.is_box_owner());

        // Verified, but the box could not put an email to the id: the claim is
        // kept as the record and compared against nothing.
        let unresolved = DispatchOrigin {
            verified: Some(VerifiedCaller {
                email: None,
                ..lying.verified.clone().unwrap()
            }),
            ..lying.clone()
        };
        assert_eq!(unresolved.attribution().email(), None);
        assert_eq!(unresolved.attribution().name(), Some("brede@tally.no"));
        assert!(!unresolved.attribution().is_verified());

        // No stamp at all: the local box, where the claim is all there is and
        // whoever sent it is already the identity the box runs as.
        let local = DispatchOrigin {
            verified: None,
            ..lying
        };
        assert_eq!(
            local.attribution(),
            Attribution::Claimed("brede@tally.no".into())
        );
        assert!(local.is_box_owner());
        assert_eq!(DispatchOrigin::default().attribution(), Attribution::Nobody);
        assert!(DispatchOrigin::default().is_box_owner());
    }

    #[test]
    fn no_via_is_the_operator() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(
            dispatcher_for(&db, None, |_| unreachable!("no chat to ask about")),
            Dispatcher::Operator
        );
        assert_eq!(
            dispatcher_for(&db, Some(""), |_| true),
            Dispatcher::Operator
        );
    }

    /// A `via` chat a live attempt owns names its task as the parent — the
    /// board-dispatched chain keeps the `via LIN-138` label. The liveness
    /// lookup must not run: the board's own records already settled it.
    #[test]
    fn a_via_chat_with_a_live_attempt_names_the_parent_task() {
        let db = Db::open_in_memory().unwrap();
        working_agent(&db, "linear:LIN-138", "chat-p", "widget");
        let d = dispatcher_for(&db, Some("chat-p"), |_| {
            unreachable!("a chat a live attempt owns is settled without asking")
        });
        assert_eq!(d.task(), Some("linear:LIN-138"));
        assert_eq!(d.pane(), Some("chat-p"));
        assert_eq!(dispatcher_name(&db, &d, None).as_deref(), Some("LIN-138"));
        // …and it stays the identifier even with a human on hand: the parent
        // issue is the more useful of the two, and the chain is what a reader
        // follows.
        assert_eq!(
            dispatcher_name(&db, &d, Some("brede@tally.no")).as_deref(),
            Some("LIN-138")
        );
    }

    /// The usual case (AGE-24): a long-lived orchestrator chat the board never
    /// dispatched. Still an agent, recorded by its chat alone.
    #[test]
    fn a_live_chat_without_an_attempt_is_an_agent_by_chat() {
        let db = Db::open_in_memory().unwrap();
        let d = dispatcher_for(&db, Some("chat-orch"), |_| true);
        assert_eq!(d.task(), None);
        assert_eq!(d.pane(), Some("chat-orch"));
        assert_eq!(dispatcher_name(&db, &d, None).as_deref(), Some("chat-orch"));
    }

    /// gh#232: an orchestrator chat the board never dispatched resolves to no
    /// identifier, and the chat id is a UUID on a public comment. The human the
    /// board already recorded beats it; the id survives only when there is
    /// nobody to name, and a blank claim is nobody.
    #[test]
    fn a_person_outranks_a_bare_chat_id() {
        let db = Db::open_in_memory().unwrap();
        let d = dispatcher_for(&db, Some("f31135c6-92d2-4efa-a0c1-1c740170f4c7"), |_| true);
        assert_eq!(
            dispatcher_name(&db, &d, Some("brede@tally.no")).as_deref(),
            Some("brede@tally.no")
        );
        assert_eq!(
            dispatcher_name(&db, &d, Some("   ")).as_deref(),
            Some("f31135c6-92d2-4efa-a0c1-1c740170f4c7")
        );
        // The operator's dispatch is named by the same human, and by nothing
        // when the frontend sent none.
        assert_eq!(
            dispatcher_name(&db, &Dispatcher::Operator, Some("brede@tally.no")).as_deref(),
            Some("brede@tally.no")
        );
        assert_eq!(dispatcher_name(&db, &Dispatcher::Operator, None), None);
    }

    /// An archived or gone chat is not claimed as an agent — recording it
    /// would hand a future notifier an address that answers for nobody.
    #[test]
    fn a_dead_via_chat_is_the_operators_dispatch() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(
            dispatcher_for(&db, Some("chat-gone"), |_| false),
            Dispatcher::Operator
        );
    }

    /// A parent whose attempt has ended is no longer named by its task, but
    /// its chat can still be an agent (an orchestrator waiting on children).
    #[test]
    fn a_finished_parents_chat_is_still_an_agent_while_alive() {
        let db = Db::open_in_memory().unwrap();
        working_agent(&db, "linear:LIN-138", "chat-p", "widget");
        let live = db.live_attempt_for_pane("chat-p").unwrap().unwrap();
        db.close_attempt(live.id, Outcome::Done).unwrap();
        let d = dispatcher_for(&db, Some("chat-p"), |_| true);
        assert_eq!(d.task(), None);
        assert_eq!(d.pane(), Some("chat-p"));
        // ...and once the chat is gone too, it is the operator's.
        assert_eq!(
            dispatcher_for(&db, Some("chat-p"), |_| false),
            Dispatcher::Operator
        );
    }

    /// A reaped parent leaves an id with no row behind it. The id is still the
    /// truth we have, and naming it beats saying nothing.
    #[test]
    fn a_parent_whose_row_is_gone_is_named_by_its_id() {
        let db = Db::open_in_memory().unwrap();
        let d = Dispatcher::agent(Some("linear:LIN-999".into()), None);
        assert_eq!(
            dispatcher_name(&db, &d, None).as_deref(),
            Some("linear:LIN-999")
        );
        assert_eq!(dispatcher_name(&db, &Dispatcher::Operator, None), None);
    }

    #[test]
    fn prompt_at_resolves_the_worktree_late() {
        let mut r = route();
        r.prompt = Some("Work on {title} in {worktree}.".into());
        let spec = build_spec(
            &RoutingConfig::default(),
            &r,
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        // Unresolved (and legible) until the executor knows the checkout…
        assert!(spec.prompt.contains("{worktree}"), "{}", spec.prompt);
        // …then resolved with the real path.
        let sent = spec.prompt_at("/worktrees/widget/board-gh-7-fix-flaky-retry");
        assert!(
            sent.contains("in /worktrees/widget/board-gh-7-fix-flaky-retry."),
            "{sent}"
        );
        assert!(!sent.contains('{'), "unresolved placeholder: {sent}");
    }

    // ---- stacking: a dispatch cut from a sibling (gh#285) ------------------

    /// A task row with one attempt on `branch`, live unless `outcome` says
    /// otherwise — the shape `--onto` looks for.
    fn sibling(db: &Db, task_id: &str, identifier: &str, branch: &str, outcome: Option<Outcome>) {
        db.upsert_task(&UpsertTask {
            id: task_id.into(),
            source: Source::Github,
            source_id: "1".into(),
            identifier: identifier.into(),
            title: "the layer below".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: None,
            upstream: UpstreamState::Started,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let id = db
            .insert_attempt(&NewAttempt {
                automation: None,
                automation_owner: None,
                stacked_on: None,
                task_id: task_id.into(),
                pane_id: None,
                workspace: "widget".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: Some(branch.into()),
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: None,
                account: None,
                repo_path: None,
                dispatched_by_device: None,
                dispatched_by_user: None,
                dispatched_by_verified: false,
                billed_to: None,
            })
            .unwrap();
        if let Some(outcome) = outcome {
            db.close_attempt(id, outcome).unwrap();
        }
    }

    /// The headline: a dispatch cut from a sibling's branch, which nothing in
    /// routing.toml could have said. The base decides both halves — where the
    /// branch is cut and where the pull request goes (gh#284) — so a stacked
    /// layer never opens a request that carries its parent's commits.
    #[test]
    fn a_per_dispatch_base_replaces_the_routes_own() {
        let overrides = DispatchOverrides {
            base: Some("board/gh-12-parser".into()),
            ..DispatchOverrides::default()
        };
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &overrides,
            None,
        )
        .unwrap();
        assert_eq!(spec.base, "board/gh-12-parser");
        assert!(
            spec.prompt
                .contains("gh pr create --base board/gh-12-parser"),
            "the agent has to be told where the request goes: {}",
            spec.prompt
        );
        // Without one, the route's key is untouched.
        let plain = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(plain.base, "origin/HEAD");
        assert!(!plain.prompt.contains("--base"), "{}", plain.prompt);
    }

    /// The gesture: name the task, not the branch. What comes back is the
    /// *attempt*, because that is the edge worth keeping once the parent
    /// merges and its branch is deleted.
    #[test]
    fn onto_resolves_a_task_to_the_attempt_holding_its_branch() {
        let db = Db::open_in_memory().unwrap();
        sibling(
            &db,
            "gh:owner/widget#12",
            "gh#12",
            "board/gh-12-parser",
            None,
        );

        for reference in ["gh:owner/widget#12", "gh#12", "GH#12"] {
            let parent = stack_parent(&db, reference, "gh:owner/widget#7").unwrap();
            assert_eq!(parent.branch, "board/gh-12-parser", "via `{reference}`");
            assert_eq!(parent.identifier, "gh#12");
            assert_eq!(parent.task_id, "gh:owner/widget#12");
            assert_eq!(parent.attempt, 1);
        }
    }

    /// A task in review is exactly what the gesture was named for: its agent is
    /// gone, its branch is pushed, and a follow-up on it is the whole point.
    #[test]
    fn a_closed_attempt_is_still_something_to_stack_on() {
        let db = Db::open_in_memory().unwrap();
        sibling(
            &db,
            "gh:owner/widget#12",
            "gh#12",
            "board/gh-12-parser",
            Some(Outcome::Done),
        );
        let parent = stack_parent(&db, "gh#12", "gh:owner/widget#7").unwrap();
        assert_eq!(parent.branch, "board/gh-12-parser");
    }

    /// Every refusal happens before anything is created, and each says which
    /// of the four things went wrong.
    #[test]
    fn a_parent_that_cannot_be_stacked_on_is_refused_by_name() {
        let db = Db::open_in_memory().unwrap();
        sibling(
            &db,
            "gh:owner/widget#12",
            "gh#12",
            "board/gh-12-parser",
            None,
        );
        // A task with an attempt that never held a branch — nothing was cut, so
        // there is nothing to cut from.
        working_agent(&db, "linear:LIN-3", "chat-3", "widget");

        let err = |reference: &str| {
            stack_parent(&db, reference, "gh:owner/widget#7")
                .unwrap_err()
                .to_string()
        };
        assert!(err("gh#404").contains("not a task on the board"));
        assert!(err("LIN-3").contains("no attempt holding a branch"));
        assert!(err("  ").contains("needs a task"));
        // Its own branch cannot be its own base.
        assert!(
            stack_parent(&db, "gh#12", "gh:owner/widget#12")
                .unwrap_err()
                .to_string()
                .contains("cannot stack on itself")
        );
    }

    /// Two rows can spell the same identifier — two sources, or two repos the
    /// board watches. Picking one silently would stack the work on somebody
    /// else's branch, which is the failure the whole refusal exists for.
    #[test]
    fn an_identifier_two_rows_answer_to_is_refused_rather_than_guessed() {
        let db = Db::open_in_memory().unwrap();
        sibling(
            &db,
            "gh:owner/widget#12",
            "gh#12",
            "board/gh-12-widget",
            None,
        );
        sibling(&db, "gh:owner/other#12", "gh#12", "board/gh-12-other", None);
        let err = stack_parent(&db, "gh#12", "gh:owner/widget#7")
            .unwrap_err()
            .to_string();
        assert!(err.contains("names 2 tasks"), "{err}");
        // Both candidates by name, so `--onto` is as repairable as `--task`:
        // the reader picks, and the message never picks for them.
        for id in ["gh:owner/widget#12", "gh:owner/other#12"] {
            assert!(err.contains(id), "{id} missing from: {err}");
        }
        // The unambiguous id still works.
        let parent = stack_parent(&db, "gh:owner/other#12", "gh:owner/widget#7").unwrap();
        assert_eq!(parent.branch, "board/gh-12-other");
    }

    /// `--base` and `--onto` are two spellings of one decision. A caller that
    /// sent both said two things, and obeying one of them quietly is how a
    /// dispatch ends up cut from a branch nobody asked for.
    #[test]
    fn naming_both_a_base_and_a_parent_is_refused() {
        let db = Db::open_in_memory().unwrap();
        sibling(
            &db,
            "gh:owner/widget#12",
            "gh#12",
            "board/gh-12-parser",
            None,
        );

        let both = DispatchOverrides {
            base: Some("release-1.x".into()),
            onto: Some("gh#12".into()),
            ..DispatchOverrides::default()
        };
        let err = stack_parent_for(&db, "gh:owner/widget#7", &both)
            .unwrap_err()
            .to_string();
        assert!(err.contains("say one or the other"), "{err}");

        // Either alone is fine, and only `onto` yields a parent: a raw base
        // names a branch, and a branch is not an attempt.
        let onto = DispatchOverrides {
            onto: Some("gh#12".into()),
            ..DispatchOverrides::default()
        };
        assert_eq!(
            stack_parent_for(&db, "gh:owner/widget#7", &onto)
                .unwrap()
                .map(|p| p.branch),
            Some("board/gh-12-parser".into()),
        );
        let base = DispatchOverrides {
            base: Some("release-1.x".into()),
            ..DispatchOverrides::default()
        };
        assert_eq!(
            stack_parent_for(&db, "gh:owner/widget#7", &base).unwrap(),
            None
        );
        assert_eq!(
            stack_parent_for(&db, "gh:owner/widget#7", &DispatchOverrides::default()).unwrap(),
            None
        );
    }
}
