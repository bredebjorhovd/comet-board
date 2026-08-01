//! Resolving a task + its route into a [`DispatchSpec`] — the planning half of
//! herdr-board's `dispatch.rs`, in comet vocabulary (docs/BOARD.md §H2).
//!
//! Deliberately only the *resolution* lives here: task → route → branch →
//! brief, pure functions over config and stored rows. Executing the spec is
//! [`crate::runtime::Runtime::dispatch`]'s job, and the surrounding pipeline —
//! concurrency caps, dispatcher provenance, the attempt-row lifecycle around a
//! failed dispatch — is H3's port of the rest of herdr-board's `dispatch.rs`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Result, bail};

use crate::config::{Route, RoutingConfig, interpolate, slugify};
use crate::model::{Task, UpstreamState, gh_repo_name};
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

/// The brief actually sent for a task under a route, fully interpolated.
///
/// `{worktree}` stays unresolved here: comet's engine picks the checkout path
/// while executing the spec, after this string is built. `interpolate` leaves
/// unknown keys visible rather than blanking them, so a route prompt using it
/// degrades legibly until H3 threads the real path through.
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
) -> Result<DispatchSpec> {
    let harness = harness_for_runtime(&route.runtime).ok_or_else(|| {
        anyhow::anyhow!(
            "runtime `{}` is not a comet harness; expected one of: {}",
            route.runtime,
            crate::runtime::RUNTIME_NAMES.join(", ")
        )
    })?;
    let branch = resolve_branch(cfg, route, task);
    let repo_path = if space.path.is_empty() {
        route.repo_path().to_string_lossy().into_owned()
    } else {
        space.path.clone()
    };
    Ok(DispatchSpec {
        identifier: task.identifier.clone(),
        space_id: space.id.clone(),
        device_id: space.device_id.clone(),
        repo_path,
        prompt: resolve_prompt(route, task, &branch),
        branch,
        worktree: true,
        harness,
        model: None,
    })
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
        let spec = build_spec(&RoutingConfig::default(), &route(), &task(), &space()).unwrap();
        assert_eq!(spec.branch, "board/gh-7-widget");
    }

    #[test]
    fn the_brief_names_task_and_branch() {
        let spec = build_spec(&RoutingConfig::default(), &route(), &task(), &space()).unwrap();
        assert!(spec.prompt.contains("Fix the flaky retry (gh#7)"));
        assert!(
            spec.prompt
                .contains("the branch board/gh-7-widget is prepared")
        );
        assert!(spec.prompt.contains("It flakes."));
    }

    #[test]
    fn the_space_path_is_the_repo_root() {
        let spec = build_spec(&RoutingConfig::default(), &route(), &task(), &space()).unwrap();
        assert_eq!(spec.repo_path, "/home/x/dev/widget");
        assert_eq!(spec.space_id, "space-1");
        assert_eq!(spec.harness, comet_proto::HarnessId::ClaudeCode);
    }

    #[test]
    fn an_unknown_runtime_is_refused_by_name() {
        let mut r = route();
        r.runtime = "gemini".into();
        let err = build_spec(&RoutingConfig::default(), &r, &task(), &space())
            .unwrap_err()
            .to_string();
        assert!(err.contains("gemini"), "{err}");
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
}
