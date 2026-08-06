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

use crate::config::{Route, RoutingConfig};
use crate::db::Db;
use crate::model::{BoardState, Task, UpstreamState};
use crate::sync::route_context;

/// The row shape itself lives in proto (`comet_proto::view::board`) so the
/// viewports can deserialize `WatchBoard` items without depending on this
/// crate; it is re-exported here because this crate owns the contract.
pub use comet_proto::view::board::TaskRow;

/// One task, in the shape callers are promised.
///
/// Separate from [`board_rows`] because this shape is the published contract
/// and the surrounding function is filtering and database wiring. Takes the
/// whole config rather than only the route because the wall-clock cap is a
/// route-then-`[defaults]` resolution, and half of it is not on the route.
pub fn task_row(task: &Task, route: Option<&Route>, cfg: &RoutingConfig) -> TaskRow {
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
        // Who pressed enter, as their frontend said (gh#74). Unlike the two
        // above it is nobody's address — a name for the row, not somewhere to
        // deliver to. The device id stays off the wire: it identifies a laptop,
        // not a person, and a reader has no way to resolve one.
        dispatched_by_user: live.or(last).and_then(|a| a.dispatched_by_user.clone()),
        last_outcome: closed
            .and_then(|a| a.outcome)
            .map(|o| o.as_str().to_string()),
        last_outcome_at: closed.and_then(|a| a.ended_at.clone()),
        attempts: task.attempts.len(),
        reopened: live.or(last).map(|a| a.reopened).unwrap_or(0),
        updated_at: task.updated_at.clone(),
        started_at: live.map(|a| a.started_at.clone()),
        // Same shape as `runtime` above: what the attempt actually ran under,
        // falling back to what the route would use for a row nothing has run
        // on yet.
        account: live
            .or(last)
            .map(|a| a.account.clone())
            .unwrap_or_else(|| route.and_then(|r| r.account.clone())),
        // What gh#70's clock will hold this row's attempt to. Route-then-
        // defaults, resolved here because the routing config is the host's and
        // a viewport reading a relayed board has never seen it — an elapsed
        // counter with no cap beside it says half of what it knows (gh#103).
        max_duration_secs: cfg.max_duration_secs(route),
    }
}

/// Every task as a row, in board order — what `WatchBoard` streams and what
/// `list` prints. Board order, so every reader agrees on what is most urgent.
pub fn board_rows(db: &Db, cfg: &RoutingConfig) -> anyhow::Result<Vec<TaskRow>> {
    let mut rows: Vec<TaskRow> = Vec::new();
    for task in db.load_tasks()? {
        let route = cfg.resolve(&route_context(&task));
        rows.push(task_row(&task, route, cfg));
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
                account: None,
                repo_path: None,
                dispatched_by_device: None,
                dispatched_by_user: None,
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

    /// gh#74: the row names the human who released it, so a reader of
    /// `list --json` can tell two teammates' work apart.
    #[test]
    fn rows_name_the_human_who_released_the_attempt() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#74", "gh#74");
        db.insert_attempt(&NewAttempt {
            task_id: "gh:o/r#74".into(),
            pane_id: None,
            workspace: "ws".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: None,
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
            account: None,
            repo_path: None,
            dispatched_by_device: Some("laptop-ana".into()),
            dispatched_by_user: Some("ana@example.com".into()),
        })
        .unwrap();

        let rows = board_rows(&db, &RoutingConfig::default()).unwrap();
        assert_eq!(
            rows[0].dispatched_by_user.as_deref(),
            Some("ana@example.com")
        );
        // The device stays off the contract: it names a laptop, not a person,
        // and no reader can resolve one.
        let wire = serde_json::to_string(&rows[0]).unwrap();
        assert!(!wire.contains("laptop-ana"), "{wire}");
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
                account: None,
                repo_path: None,
                dispatched_by_device: None,
                dispatched_by_user: None,
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
            account: None,
            repo_path: None,
            dispatched_by_device: None,
            dispatched_by_user: None,
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

    /// Whose subscription a row is spending: the attempt's recorded account
    /// once there is one, the route's default before that (gh#59). Same shape
    /// as `runtime`, and for the same reason — the row should say what will
    /// happen as well as what did.
    #[test]
    fn rows_report_the_account_the_attempt_ran_under() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#5", "gh#5");
        let cfg: RoutingConfig = toml::from_str(
            r#"
            [[route]]
            workspace = "r"
            repo = "/tmp/r"
            runtime = "claude-code"
            account = "0011223344556677"
            "#,
        )
        .unwrap();

        // Nothing dispatched yet: the route's default is what a dispatch would
        // spend.
        let rows = board_rows(&db, &cfg).unwrap();
        assert_eq!(rows[0].account.as_deref(), Some("0011223344556677"));

        // Once an attempt exists, the row reports what it actually ran under —
        // here a per-dispatch override of the route's default.
        db.insert_attempt(&NewAttempt {
            task_id: "gh:o/r#5".into(),
            pane_id: None,
            workspace: "r".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: None,
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
            account: Some("8f2c1d0a7b6e4539".into()),
            repo_path: None,
            dispatched_by_device: None,
            dispatched_by_user: None,
        })
        .unwrap();
        let rows = board_rows(&db, &cfg).unwrap();
        assert_eq!(rows[0].account.as_deref(), Some("8f2c1d0a7b6e4539"));
    }

    /// The wall-clock cap rides the row (gh#103): the sidebar's elapsed counter
    /// is read against it, and the routing config lives on the board host — a
    /// laptop reading a relayed board cannot resolve one for itself.
    #[test]
    fn rows_carry_the_caps_their_attempts_run_under() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#70", "gh#70");
        let route = |repo: &str| -> RoutingConfig {
            toml::from_str(&format!(
                r#"
                [defaults]
                max_duration = "2h"

                [[route]]
                workspace = "r"
                repo = "/tmp/r"
                runtime = "claude-code"
                max_duration = "6h"
                match = {{ gh_repo = "{repo}" }}
                "#
            ))
            .unwrap()
        };

        // Nothing routes this row, so it reports what `[defaults]` would cap it
        // at — the same shape `runtime` and `account` have.
        let rows = board_rows(&db, &route("someone/else")).unwrap();
        assert_eq!(rows[0].max_duration_secs, Some(7200));

        // A route with its own cap overrides it.
        let rows = board_rows(&db, &route("o/r")).unwrap();
        assert_eq!(rows[0].max_duration_secs, Some(6 * 3600));
    }

    /// An attempt from before the route named an account keeps saying so: the
    /// route's default must not be back-filled onto a run that did not use it.
    #[test]
    fn an_accountless_attempt_is_not_relabelled_by_the_route() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "gh:o/r#6", "gh#6");
        let cfg: RoutingConfig = toml::from_str(
            r#"
            [[route]]
            workspace = "r"
            repo = "/tmp/r"
            runtime = "claude-code"
            account = "0011223344556677"
            "#,
        )
        .unwrap();
        db.insert_attempt(&NewAttempt {
            task_id: "gh:o/r#6".into(),
            pane_id: None,
            workspace: "r".into(),
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
        })
        .unwrap();
        assert_eq!(board_rows(&db, &cfg).unwrap()[0].account, None);
    }
}
