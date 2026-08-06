//! Resolving a task + its route into a [`DispatchSpec`] — the planning half of
//! herdr-board's `dispatch.rs`, in comet vocabulary (docs/BOARD.md §H2/H3).
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
pub fn resolve_prompt(route: &Route, task: &Task, branch: &str) -> String {
    let vars = prompt_vars(task, branch, &route.workspace);
    let template = route.prompt.clone().unwrap_or_else(|| {
        // A route with no prompt still needs to say something useful.
        "You are working on: {title} ({identifier})\n\n{body}\n\n\
         Work in this worktree; the branch {branch} is prepared. \
         Open a pull request when done."
            .to_string()
    });
    interpolate(&template, &vars)
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

/// The short name for a dispatcher: the parent's issue identifier when the
/// board dispatched it too, its chat id otherwise. `None` is the operator, who
/// is named by the surrounding copy rather than by this.
pub fn dispatcher_name(db: &Db, d: &Dispatcher) -> Option<String> {
    match d {
        Dispatcher::Operator => None,
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
pub fn build_spec(
    cfg: &RoutingConfig,
    route: &Route,
    task: &Task,
    space: &SpaceRef,
    overrides: &DispatchOverrides,
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
    Ok(DispatchSpec {
        identifier: task.identifier.clone(),
        space_id: space.id.clone(),
        device_id: space.device_id.clone(),
        push_repo,
        repo_path,
        prompt: resolve_prompt(route, task, &branch),
        branch,
        base: cfg.base(route).to_string(),
        worktree: true,
        harness,
        model: overrides.model.clone(),
        account: overrides
            .account
            .clone()
            .or_else(|| route.account.clone())
            .filter(|a| !a.is_empty()),
    })
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
        let spec = build_spec(&RoutingConfig::default(), &route(), &task(), &space(), &DispatchOverrides::default()).unwrap();
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
        let spec = build_spec(&RoutingConfig::default(), &route(), &t, &s, &DispatchOverrides::default()).unwrap();
        assert_eq!(spec.push_repo, None);
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
        };
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &overrides,
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
        };
        let spec = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &overrides,
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
        };
        let err = build_spec(
            &RoutingConfig::default(),
            &route(),
            &task(),
            &space(),
            &overrides,
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
        )
        .unwrap();
        assert_eq!(from_defaults.base, "origin/develop");

        let mut r = route();
        r.base = Some("release".into());
        let from_route =
            build_spec(&cfg, &r, &task(), &space(), &DispatchOverrides::default()).unwrap();
        assert_eq!(from_route.base, "release");
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

    // ---- concurrency + provenance (H3) ----------------------------------

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
        assert_eq!(dispatcher_name(&db, &d).as_deref(), Some("LIN-138"));
    }

    /// The usual case (AGE-24): a long-lived orchestrator chat the board never
    /// dispatched. Still an agent, recorded by its chat alone.
    #[test]
    fn a_live_chat_without_an_attempt_is_an_agent_by_chat() {
        let db = Db::open_in_memory().unwrap();
        let d = dispatcher_for(&db, Some("chat-orch"), |_| true);
        assert_eq!(d.task(), None);
        assert_eq!(d.pane(), Some("chat-orch"));
        assert_eq!(dispatcher_name(&db, &d).as_deref(), Some("chat-orch"));
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
        assert_eq!(dispatcher_name(&db, &d).as_deref(), Some("linear:LIN-999"));
        assert_eq!(dispatcher_name(&db, &Dispatcher::Operator), None);
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
