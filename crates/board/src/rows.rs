//! The board as callers see it: one row per task, herdr-board's `list --json`
//! contract with the pane→chat rename applied.
//!
//! This shape is a published contract, consumed three ways: `WatchBoard`
//! streams it, the `comet-board` CLI prints it (H6, verbatim), and the agent
//! conventions text teaches orchestrating agents to poll it. Field renames from
//! herdr-board are exactly the two the port dictates — `pane_id` → `chat_id`,
//! `dispatched_by_pane` → `dispatched_by_chat` — because the values *are* chat
//! ids now, and a contract that lies about what its ids address is worse than
//! one that renames.

use serde::{Deserialize, Serialize};

use crate::config::{Route, RoutingConfig};
use crate::db::Db;
use crate::model::{BoardState, Task, UpstreamState};
use crate::sync::route_context;

/// One task, in the shape callers are promised.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRow {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub state: String,
    pub source: String,
    pub url: String,
    pub labels: Vec<String>,
    /// False when no route matches, or when the issue is gone upstream: the
    /// task is on the board but cannot be dispatched, and `dispatch` refuses it.
    pub dispatchable: bool,
    /// The issue behind this row no longer exists upstream. The row is kept for
    /// the attempts on it; there is nothing left to work on.
    pub gone: bool,
    pub route: Option<String>,
    /// The comet space (herdr's workspace — the config key keeps that name).
    pub workspace: Option<String>,
    pub runtime: Option<String>,
    /// The live attempt's chat (herdr-board's `pane_id`).
    pub chat_id: Option<String>,
    /// Set on `review` rows, which is how a PR reaches an orchestrator.
    pub pr_url: Option<String>,
    pub pr_number: Option<i64>,
    pub branch: Option<String>,
    /// Parent task id when the board dispatched the releasing agent too. Null
    /// on its own does **not** mean the operator: an orchestrating chat has no
    /// attempt and so no task id — read `dispatched_by_chat` as well.
    pub dispatched_by: Option<String>,
    /// The chat the dispatch ran from, when an agent was in it (herdr-board's
    /// `dispatched_by_pane`). Set for every agent-released row, and the address
    /// a parent can be reached at. Both this and `dispatched_by` null means the
    /// operator released it.
    pub dispatched_by_chat: Option<String>,
    /// How the most recent *ended* attempt ended: `done`, `failed`, `cancelled`
    /// or `orphaned`. Null means no attempt has ever ended.
    ///
    /// This is what makes cancellation legible to a parent agent. `cancelled`
    /// derives back to `ready` — deliberately, since the issue is still owed —
    /// so without this field a child the operator killed is indistinguishable
    /// from one that was never dispatched. Set even while a *newer* attempt is
    /// live, so a retry does not erase how the previous one went.
    pub last_outcome: Option<String>,
    /// When that attempt ended (RFC 3339). Pairs with `last_outcome` so a
    /// long-lived poller can tell a fresh cancellation from one it already saw.
    pub last_outcome_at: Option<String>,
    pub attempts: usize,
    /// How many times the board closed this row's attempt and then found its
    /// agent still working (herdr-board gh#34's honesty field).
    pub reopened: i64,
}

/// One task, in the shape callers are promised.
///
/// Separate from [`board_rows`] because this shape is the published contract
/// and the surrounding function is filtering and database wiring.
pub fn task_row(task: &Task, route: Option<&Route>) -> TaskRow {
    let live = task.live_attempt();
    let last = task.attempts.last();
    let closed = task.last_closed_attempt();
    let gone = task.upstream == UpstreamState::Gone;
    TaskRow {
        id: task.id.clone(),
        identifier: task.identifier.clone(),
        title: task.title.clone(),
        state: task.state.as_str().to_string(),
        source: task.source.as_str().to_string(),
        url: task.url.clone(),
        labels: task.labels.clone(),
        dispatchable: route.is_some() && !gone,
        gone,
        route: route.map(|r| r.display_name().to_string()),
        workspace: live
            .or(last)
            .map(|a| a.workspace.clone())
            .or_else(|| route.map(|r| r.workspace.clone())),
        runtime: live
            .or(last)
            .map(|a| a.runtime.clone())
            .or_else(|| route.map(|r| r.runtime.clone())),
        chat_id: live.and_then(|a| a.pane_id.clone()),
        pr_url: task.pr_url.clone(),
        pr_number: task.pr_number,
        branch: live.or(last).and_then(|a| a.branch.clone()),
        dispatched_by: live.or(last).and_then(|a| a.dispatched_by.clone()),
        dispatched_by_chat: live.or(last).and_then(|a| a.dispatched_by_pane.clone()),
        last_outcome: closed
            .and_then(|a| a.outcome)
            .map(|o| o.as_str().to_string()),
        last_outcome_at: closed.and_then(|a| a.ended_at.clone()),
        attempts: task.attempts.len(),
        reopened: live.or(last).map(|a| a.reopened).unwrap_or(0),
    }
}

/// Every task as a row, in board order — what `WatchBoard` streams and what
/// `list` prints. Board order, so every reader agrees on what is most urgent.
pub fn board_rows(db: &Db, cfg: &RoutingConfig) -> anyhow::Result<Vec<TaskRow>> {
    let mut rows: Vec<TaskRow> = Vec::new();
    for task in db.load_tasks()? {
        let route = cfg.resolve(&route_context(&task));
        rows.push(task_row(&task, route));
    }
    rows.sort_by_key(|r| {
        BoardState::SECTION_ORDER
            .iter()
            .position(|s| s.as_str() == r.state)
            .unwrap_or(usize::MAX)
    });
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{NewAttempt, UpsertTask};
    use crate::model::{Outcome, Source};

    fn seed(db: &Db, id: &str, identifier: &str) {
        db.upsert_task(&UpsertTask {
            id: id.into(),
            source: Source::Github,
            source_id: identifier.into(),
            identifier: identifier.into(),
            title: format!("task {identifier}"),
            body: None,
            url: format!("https://github.com/{id}"),
            labels: vec![],
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
    }

    #[test]
    fn rows_carry_the_chat_id_of_the_live_attempt() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#1", "gh#1");
        let a = db
            .insert_attempt(&NewAttempt {
                task_id: "gh:o/r#1".into(),
                pane_id: None,
                workspace: "ws".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: Some("board/gh-1-r".into()),
                dispatched_by: None,
                dispatched_by_pane: Some("chat-parent".into()),
                base_sha: None,
            })
            .unwrap();
        db.set_attempt_pane(a, "chat-1").unwrap();

        let rows = board_rows(&db, &RoutingConfig::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].chat_id.as_deref(), Some("chat-1"));
        assert_eq!(rows[0].dispatched_by_chat.as_deref(), Some("chat-parent"));
        assert_eq!(rows[0].branch.as_deref(), Some("board/gh-1-r"));
        // No route configured: on the board, not dispatchable.
        assert!(!rows[0].dispatchable);
    }

    #[test]
    fn a_closed_attempt_reports_its_outcome_even_under_a_newer_live_one() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#2", "gh#2");
        let first = db
            .insert_attempt(&NewAttempt {
                task_id: "gh:o/r#2".into(),
                pane_id: None,
                workspace: "ws".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: None,
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: None,
            })
            .unwrap();
        db.close_attempt(first, Outcome::Cancelled).unwrap();
        db.insert_attempt(&NewAttempt {
            task_id: "gh:o/r#2".into(),
            pane_id: None,
            workspace: "ws".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: None,
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
        })
        .unwrap();

        let rows = board_rows(&db, &RoutingConfig::default()).unwrap();
        assert_eq!(rows[0].last_outcome.as_deref(), Some("cancelled"));
        assert_eq!(rows[0].attempts, 2);
    }

    #[test]
    fn rows_sort_in_board_order() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#3", "gh#3"); // ready (default derivation)
        seed(&db, "gh:o/r#4", "gh#4");
        db.set_local_done("gh:o/r#4", true).unwrap();
        db.store_derived_state("gh:o/r#4", BoardState::Done)
            .unwrap();

        let rows = board_rows(&db, &RoutingConfig::default()).unwrap();
        assert_eq!(rows[0].state, "ready");
        assert_eq!(rows[1].state, "done");
    }
}
