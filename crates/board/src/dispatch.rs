//! Resolving a task + its route into a [`DispatchSpec`] — the planning half of
//! herdr-board's `dispatch.rs`, in comet vocabulary
//! (§runtime-impl/§dispatch-pipeline).
//!
//! Deliberately only the *decisions* live here: task → route → branch → brief,
//! plus the two refusals a dispatch owes its caller before anything is created
//! ([`check_capacity`], [`route_for`]) and the provenance verdict
//! ([`dispatcher_for`]) — pure functions over config and stored rows.
//! Executing the spec is [`crate::runtime::Runtime::dispatch`]'s job; the
//! attempt-row lifecycle around it lives with the board loop in the engine.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};

use crate::billing::{Attribution, Billing};
use crate::config::{Route, RoutingConfig, interpolate, slugify};
use crate::db::Db;
use crate::model::{Dispatcher, Task, UpstreamState, gh_repo_name};
use crate::runtime::{DispatchSpec, harness_for_runtime};
use crate::sync::route_context;

/// The branch slug for a task: `gh#2` in `owner/repo` → `gh-2-repo`, `LIN-145`
/// → `lin-145`. Repo-qualified for GitHub tasks so two repos releasing their
/// issue 2 at once do not collide (herdr-board AGE-20).
pub fn branch_slug(task: &Task) -> String {
    match gh_repo_name(&task.id) {
        Some(repo) => slugify(&format!("{}-{repo}", task.identifier)),
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
    v.insert("identifier_lower", branch_slug(task));
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
pub fn resolve_prompt(route: &Route, task: &Task, branch: &str, base: &str) -> String {
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

/// Resolve the branch name for an attempt.
pub fn resolve_branch(cfg: &RoutingConfig, route: &Route, task: &Task) -> String {
    let vars = prompt_vars(task, "", &route.workspace);
    interpolate(cfg.branch_template(route), &vars)
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
pub fn build_spec(
    cfg: &RoutingConfig,
    route: &Route,
    task: &Task,
    space: &SpaceRef,
    overrides: &DispatchOverrides,
    by_user: Option<&str>,
) -> Result<DispatchSpec> {
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
    let base = cfg.base(route).to_string();
    Ok(DispatchSpec {
        identifier: task.identifier.clone(),
        title: task.title.clone(),
        space_id: space.id.clone(),
        device_id: space.device_id.clone(),
        push_repo,
        git_author,
        repo_path,
        prompt: resolve_prompt(route, task, &branch, &base),
        branch,
        base,
        worktree: true,
        harness,
        model: overrides.model.clone(),
        account: effective_account(route, overrides).map(str::to_string),
        // What the engine's run loop will hold this attempt's turns to
        // (gh#270). Resolved here because the route is here — the loop that
        // enforces it sees events, not config.
        turn_limits: cfg.turn_limits(Some(route)),
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
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            local_done: false,
            pr_url: None,
            pr_number: None,
            pr_open: false,
            pr_merged: false,
            pr_mergeable: None,
            pr_base_ref: None,
            pr_stack: None,
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

    #[test]
    fn branch_comes_from_the_template_repo_qualified() {
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &DispatchOverrides::default(),
            None,
        )
        .unwrap();
        assert_eq!(spec.branch, "board/gh-7-widget");
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
                .contains("the branch board/gh-7-widget is prepared")
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
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Started,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let a = db
            .insert_attempt(&NewAttempt {
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
        let sent = spec.prompt_at("/worktrees/widget/board-gh-7-widget");
        assert!(
            sent.contains("in /worktrees/widget/board-gh-7-widget."),
            "{sent}"
        );
        assert!(!sent.contains('{'), "unresolved placeholder: {sent}");
    }
}
