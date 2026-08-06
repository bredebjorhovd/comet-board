//! The sync cycle: poll sources, reconcile session state, derive state, drain
//! writebacks. Ported from herdr-board's `sync.rs` — the tracker half nearly
//! verbatim, the reconcile half reshaped for comet.
//!
//! ## What changed shape in the port
//!
//! herdr-board reconciled against a pane listing it had to distrust: screen
//! scraping, byte-identical-screen resampling (its gh#32), settle debounce
//! clocks (gh#18/gh#34), nudging (gh#40). comet's engine *runs* the agent, so
//! reconciliation here is a mapping: the engine's session watch streams
//! `Session` rows, [`crate::runtime::agent_status`] turns each into an
//! [`AgentStatus`], and [`SyncEngine::reconcile_sessions`] writes them onto
//! live attempts. No screens, no vitals, no nudges.
//!
//! Settle decisions (§H4) key off run-journal events: a run ending is a
//! recorded fact, so "the turn ended, now check the checkout" needs no
//! debounce clock. The decision itself is [`crate::settled::decide`]; the
//! artifact checks and the wrongly-settled rewatch live here, on both the
//! interval reconcile (catch-up) and the event path (the moment the run ends).
//!
//! ## What is deliberately NOT here yet
//!
//! - **Unadopted detection** — lives in [`crate::adopt`] (H8), walking comet
//!   spaces on demand from `comet-board adopt` / `doctor` rather than on the
//!   sync cycle; H7's board view decides whether a periodic sweep earns a
//!   place here.

use crate::config::{Credentials, Paths, RouteContext, RoutingConfig};
use crate::db::{Db, NewWriteback, Reaped};
use crate::gc;
use crate::log::Logger;
use crate::model::*;
use crate::notify::{self, Signal, Stopped, Webhook};
use crate::overrun;
use crate::runtime::{RunEnd, Runtime};
use crate::settled::{self, Commits, Evidence, Verdict, Why};
use crate::sources::github::{Github, HttpRest, PullRequest, Rest, pr_matches_branch};
use crate::sources::linear::{GraphQl, HttpTransport, Linear};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Command;
use std::sync::Arc;

impl GraphQl for Box<dyn GraphQl> {
    fn query(&self, body: &Value) -> Result<Value> {
        (**self).query(body)
    }
}

impl Rest for Box<dyn Rest> {
    fn get(&self, path: &str) -> Result<Value> {
        (**self).get(path)
    }
    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        (**self).post(path, body)
    }
    fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        (**self).patch(path, body)
    }
    fn put(&self, path: &str, body: &Value) -> Result<Value> {
        (**self).put(path, body)
    }
}

/// Health of one upstream source, rendered in the board header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceHealth {
    /// Not configured — the header omits it entirely.
    Absent,
    Ok,
    Down {
        error: String,
        retry_in: u64,
    },
}

/// How a writeback left the queue — see [`SyncEngine::drain_writebacks`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sent {
    /// Reached the source.
    Upstream,
    /// Never sent, and never will be: there is nothing upstream to send it to.
    Dropped(String),
}

/// Agent statuses for this cycle, keyed by chat id (the id stored in
/// `Attempt::pane_id`). Built by the engine's board service from the session
/// watch through [`crate::runtime::agent_status`]. A chat absent from the map
/// has no session row, which is [`AgentStatus::Missing`].
pub type SessionStatuses = HashMap<String, AgentStatus>;

pub struct SyncEngine {
    pub db: Db,
    pub cfg: RoutingConfig,
    pub credentials: Credentials,
    pub paths: Paths,
    pub log: Arc<Logger>,
    pub linear: Option<Linear<Box<dyn GraphQl>>>,
    pub github: Option<Github<Box<dyn Rest>>>,
    /// Where `[defaults] notify_webhook` is POSTed (gh#71). A field rather than
    /// a call into `notify::HttpWebhook` so a test can watch what the board
    /// would have sent without one listening socket in the suite.
    pub webhook: Arc<dyn Webhook>,
}

/// Meta keys. Kept together so readers and the engine's loop cannot drift.
pub mod meta {
    pub const LAST_SYNC: &str = "last_sync";
    pub const LINEAR_WATERMARK: &str = "linear_watermark";
    pub const LINEAR_STATUS: &str = "linear_status";
    pub const LINEAR_LAST_OK: &str = "linear_last_ok";
    pub const LINEAR_FAILURES: &str = "linear_failures";
    pub const GITHUB_STATUS: &str = "github_status";
    pub const GITHUB_LAST_OK: &str = "github_last_ok";
    pub const GITHUB_FAILURES: &str = "github_failures";
    /// When the last full (unwatermarked) poll ran, which is the only poll that
    /// can tell "deleted upstream" from "unchanged".
    pub const LAST_FULL_SWEEP: &str = "last_full_sweep";
    /// Timestamp of our own last writeback for a task, for the loop guard.
    pub fn writeback_at(task_id: &str) -> String {
        format!("wb_at:{task_id}")
    }
    /// Which review comments have already been delivered into a task's chat —
    /// [`crate::review::Delivered`], as JSON.
    pub fn reviews_for(task_id: &str) -> String {
        format!("reviews:{task_id}")
    }
    /// When an attempt was first found holding unpushed commits (gh#69), so
    /// the log says it once rather than every cycle.
    pub fn unpushed_noted(attempt: i64) -> String {
        format!("unpushed:{attempt}")
    }
}

impl SyncEngine {
    /// Build a live engine from the board's directories: open `board.db`, load
    /// `routing.toml`, read credentials, construct whichever source clients the
    /// credentials allow. What herdr-board's `engine_from_paths` did.
    pub fn from_paths(paths: &Paths, log: Arc<Logger>) -> Result<SyncEngine> {
        let cfg = RoutingConfig::load_or_default(&paths.routing());
        let db = Db::open(&paths.db())?;
        let credentials = Credentials::load(paths);
        Self::build(db, cfg, paths.clone(), log, credentials)
    }

    pub fn build(
        db: Db,
        cfg: RoutingConfig,
        paths: Paths,
        log: Arc<Logger>,
        credentials: Credentials,
    ) -> Result<SyncEngine> {
        let linear = credentials
            .linear_api_key
            .clone()
            .and_then(|k| HttpTransport::new(k).ok())
            .map(|t| Linear::new(Box::new(t) as Box<dyn GraphQl>));
        // GitHub is optional entirely; without repos configured it is never
        // polled.
        let github = if cfg.github.repos.is_empty() {
            None
        } else {
            HttpRest::from_credentials(&credentials)
                .ok()
                .map(|r| Github::new(Box::new(r) as Box<dyn Rest>))
        };
        Ok(SyncEngine {
            db,
            cfg,
            credentials,
            paths,
            log,
            linear,
            github,
            webhook: Arc::new(crate::notify::HttpWebhook),
        })
    }

    /// Rebuild when credentials or source configuration change on disk.
    ///
    /// The loop outlives the setup: adding a key or a repo should take effect
    /// on the next cycle rather than requiring an engine restart nobody would
    /// think to do. Returns `None` when nothing relevant changed.
    pub fn reload_if_configuration_changed(&self) -> Option<SyncEngine> {
        let credentials = Credentials::load(&self.paths);
        let cfg = RoutingConfig::load_or_default(&self.paths.routing());

        let linear_changed = credentials.linear_api_key != self.credentials.linear_api_key;
        // The whole GitHub credential, not just the token: registering an App
        // over a running board is exactly the change this exists to notice, and
        // comparing only `github_token` would leave it polling as the old
        // identity until somebody restarted the engine (gh#58).
        let github_changed = credentials.github_auth() != self.credentials.github_auth();
        let repos_changed = cfg.github.repos != self.cfg.github.repos;
        let routes_changed = cfg.routes.len() != self.cfg.routes.len();
        if !(linear_changed || github_changed || repos_changed || routes_changed) {
            return None;
        }
        self.log.info(format!(
            "configuration changed (linear credential:{linear_changed} \
             github credential:{github_changed} \
             repos:{repos_changed} routes:{routes_changed}) — rebuilding"
        ));
        let rebuilt = Db::open(&self.paths.db())
            .and_then(|db| Self::build(db, cfg, self.paths.clone(), self.log.clone(), credentials));
        match rebuilt {
            Ok(e) => Some(e),
            Err(e) => {
                self.log
                    .error(format!("could not rebuild after a config change: {e}"));
                None
            }
        }
    }

    /// One full cycle. Never returns `Err` for a source outage — a poll failure
    /// marks the header and serves stale data.
    ///
    /// `statuses` is this cycle's view of the session watch; `None` means the
    /// caller has not received a snapshot yet (engine still booting), and
    /// reconciliation is skipped rather than run against a world where every
    /// chat would read as missing.
    /// Returns the pull requests this cycle polled, handed on rather than
    /// refetched: review delivery ([`SyncEngine::deliver_reviews`]) needs each
    /// one's `updated_at` to decide whether asking about its comments is worth
    /// a call at all. The caller runs delivery *after* the cycle, because
    /// `review` is only correct once this cycle's reconciliation has landed.
    pub fn sync_once(&self, statuses: Option<&SessionStatuses>) -> Result<Vec<PullRequest>> {
        self.sync_once_with(statuses, None)
    }

    /// As [`SyncEngine::sync_once`], with the [`Runtime`] the settle logic
    /// consults for run-journal facts (§H4). `None` — the read-only callers —
    /// still settles on `Idle`, but leaves an errored end unrecognised.
    pub fn sync_once_with(
        &self,
        statuses: Option<&SessionStatuses>,
        runtime: Option<&dyn Runtime>,
    ) -> Result<Vec<PullRequest>> {
        self.poll_linear();
        let pulls = self.poll_github();

        if let Some(statuses) = statuses {
            self.reconcile_sessions_with(statuses, runtime)?;
        }
        self.rederive_all()?;
        // After reconciliation, so a checkout freed this cycle starts its
        // retention clock this cycle; on the interval only, like every other
        // clocked decision (gh#72).
        self.collect_worktrees(runtime);
        self.drain_writebacks();
        self.db.meta_set(meta::LAST_SYNC, &crate::db::now())?;
        Ok(pulls)
    }

    // ---- polling --------------------------------------------------------

    /// How often to poll without the watermark. An incremental poll cannot see
    /// a deletion — a deleted issue is simply never returned again, which is
    /// indistinguishable from one that has not changed — so periodically the
    /// whole set is fetched and anything missing is reaped.
    const FULL_SWEEP_SECS: i64 = 120;

    fn due_for_full_sweep(&self) -> bool {
        let Ok(Some(last)) = self.db.meta_get(meta::LAST_FULL_SWEEP) else {
            return true;
        };
        chrono::DateTime::parse_from_rfc3339(&last)
            .map(|t| {
                (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds()
                    >= Self::FULL_SWEEP_SECS
            })
            .unwrap_or(true)
    }

    /// Retire tasks the source no longer returns.
    ///
    /// A task with a live attempt is left alone entirely: an agent is working on
    /// it, and the row vanishing underneath a running chat would be worse than a
    /// stale row. Reconciliation will orphan it if the chat dies.
    ///
    /// Closed attempts are kept too, as a `gone` row rather than a deletion —
    /// see [`Db::reap_task`]. Only a task nobody ever dispatched is forgotten.
    fn reap_missing(&self, source: Source, seen: &std::collections::HashSet<String>) {
        let Ok(known) = self.db.reapable_task_ids(source) else {
            return;
        };
        let live: std::collections::HashSet<String> = self
            .db
            .live_attempts()
            .unwrap_or_default()
            .into_iter()
            .map(|a| a.task_id)
            .collect();
        for id in known {
            if seen.contains(&id) {
                continue;
            }
            if live.contains(&id) {
                self.log.warn(format!(
                    "{id} is gone from {} but still has a live attempt — keeping it",
                    source.as_str()
                ));
                continue;
            }
            match self.db.reap_task(&id) {
                Ok(Reaped::Forgotten) => self.log.info(format!(
                    "{id} no longer exists upstream and was never dispatched — removed"
                )),
                Ok(Reaped::Kept { attempts }) => self.log.info(format!(
                    "{id} no longer exists upstream — marked gone, keeping {attempts} \
                     attempt(s) so `gc` can still collect their worktrees"
                )),
                Err(e) => self.log.error(format!("reaping {id}: {e}")),
            }
        }
    }

    fn poll_linear(&self) {
        let Some(linear) = &self.linear else {
            return;
        };
        let full_sweep = self.due_for_full_sweep();
        let watermark = if full_sweep {
            None
        } else {
            self.db.meta_get(meta::LINEAR_WATERMARK).ok().flatten()
        };

        // Issues we hold live attempts against are fetched regardless of the
        // board filter, so writeback targets stay fresh after they leave the
        // queue.
        let live_ids: Vec<String> = self
            .db
            .live_attempts()
            .unwrap_or_default()
            .iter()
            .filter_map(|a| self.db.get_task(&a.task_id).ok().flatten())
            .filter(|t| t.source == Source::Linear)
            .map(|t| t.source_id)
            .collect();

        // Local midnight, so today's finished work stays on the board and
        // yesterday's falls off by itself.
        let today = chrono::Local::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|d| {
                d.and_local_timezone(chrono::Local)
                    .earliest()
                    .map(|t| crate::db::rfc3339(t.with_timezone(&chrono::Utc)))
            })
            .unwrap_or_default();

        let result = linear
            .fetch_board_issues(
                &self.cfg.sync.labels,
                watermark.as_deref(),
                today.as_deref(),
            )
            .and_then(|mut issues| {
                let extra = linear.fetch_issues_by_id(&live_ids)?;
                let known: std::collections::HashSet<_> =
                    issues.iter().map(|i| i.id.clone()).collect();
                issues.extend(extra.into_iter().filter(|i| !known.contains(&i.id)));
                Ok(issues)
            });

        match result {
            Ok(issues) => {
                let mut high = watermark.clone().unwrap_or_default();
                for i in &issues {
                    if self.is_own_echo(&i.task_id(), &i.updated_at) {
                        self.log.info(format!(
                            "loop guard: ignoring our own update on {}",
                            i.identifier
                        ));
                    }
                    if let Err(e) = self.db.upsert_task(&i.to_upsert()) {
                        self.log.error(format!("upsert {}: {e}", i.identifier));
                        continue;
                    }
                    // A PR attached to the Linear issue is one of the two ways a
                    // task reaches `review`.
                    if let Some(pr) = i.pr_url() {
                        let number = pr.rsplit('/').next().and_then(|n| n.parse::<i64>().ok());
                        let _ = self.db.set_pr(&i.task_id(), Some(pr), number, true);
                    }
                    if i.updated_at > high {
                        high.clone_from(&i.updated_at);
                    }
                }
                if !high.is_empty() {
                    let _ = self.db.meta_set(meta::LINEAR_WATERMARK, &high);
                }
                if full_sweep {
                    // This response is the complete set, so anything of ours
                    // that is missing from it is genuinely gone.
                    let seen: std::collections::HashSet<String> =
                        issues.iter().map(|i| i.task_id()).collect();
                    self.reap_missing(Source::Linear, &seen);
                    let _ = self.db.meta_set(meta::LAST_FULL_SWEEP, &crate::db::now());
                }
                let _ = self.db.meta_set(meta::LINEAR_STATUS, "ok");
                let _ = self.db.meta_set(meta::LINEAR_LAST_OK, &crate::db::now());
                let _ = self.db.meta_set(meta::LINEAR_FAILURES, "0");
                self.log.info(format!("linear: {} issues", issues.len()));
            }
            Err(e) => {
                // Serve stale data and mark the header. Never blank the list.
                let failures = self.bump_failures(meta::LINEAR_FAILURES);
                let _ = self.db.meta_set(meta::LINEAR_STATUS, &format!("error:{e}"));
                self.log
                    .warn(format!("linear poll failed (attempt {failures}): {e}"));
            }
        }
    }

    /// Returns every pull request seen this cycle, so the caller does not have
    /// to ask GitHub for them a second time.
    fn poll_github(&self) -> Vec<PullRequest> {
        let Some(gh) = &self.github else {
            return Vec::new();
        };
        if self.cfg.github.repos.is_empty() {
            return Vec::new();
        }
        let mut all_pulls: Vec<PullRequest> = Vec::new();
        let mut failed: Option<String> = None;
        // GitHub is always polled in full, so every cycle can reap.
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for repo in &self.cfg.github.repos {
            // Per repo, not one filter for all of them: `labels = []` is the
            // right answer for a curated tracker and a backlog dump for a repo
            // that keeps its roadmap as open issues.
            match gh.issues(repo, self.cfg.github.labels_for(repo)) {
                Ok(issues) => {
                    for i in issues {
                        seen.insert(i.task_id());
                        // Two writers on one issue now — the board and the agent
                        // in the chat — so the loop guard carries across.
                        if self.is_own_echo(&i.task_id(), &i.updated_at) {
                            self.log.info(format!(
                                "loop guard: ignoring our own update on {}",
                                i.task_id()
                            ));
                        }
                        if let Err(e) = self.db.upsert_task(&i.to_upsert()) {
                            self.log.error(format!("upsert {}: {e}", i.task_id()));
                        }
                    }
                }
                Err(e) => failed = Some(e.to_string()),
            }
            match gh.pulls(repo) {
                Ok(p) => all_pulls.extend(p),
                Err(e) => failed = Some(e.to_string()),
            }
        }

        // A PR whose branch belongs to an attempt is that task's PR, not a row
        // of its own — otherwise dispatched work would appear twice.
        //
        // Scoped by repository where the task names one: `board/gh-2` in one
        // repo is not another repo's attempt branch merely because the strings
        // match, and suppressing it hides a real pull request behind a
        // coincidence (herdr-board AGE-20). New branches are repo-qualified,
        // but attempts recorded before that still hold the ambiguous name, so
        // the check carries the scope rather than trusting the string. A
        // Linear task names no repo, so its branch is honoured in whichever
        // repo the PR turns up in.
        let attempt_branches = self.attempt_branches();

        if let Err(e) = self.link_pull_requests(&all_pulls) {
            self.log.error(format!("linking PRs: {e}"));
        }

        // Mergeability costs one call per open PR, so it rides the full sweep
        // rather than every poll. It is the fact that matters most about a PR
        // waiting on you when several branches are in flight at once.
        let check_mergeable = self.due_for_full_sweep();

        if self.cfg.github.pull_requests {
            for pr in &all_pulls {
                if attempt_branches.claims(pr) {
                    continue;
                }
                if let Err(e) = self.db.upsert_task(&pr.to_upsert()) {
                    self.log.error(format!("upsert {}: {e}", pr.task_id()));
                    continue;
                }
                seen.insert(pr.task_id());
                // Setting the PR fields is what makes derivation reach
                // `review` rather than `ready`.
                let _ = self
                    .db
                    .set_pr(&pr.task_id(), Some(&pr.url), Some(pr.number), pr.open);
                let _ = self.db.set_pr_merged(&pr.task_id(), pr.merged);
                if check_mergeable && pr.open {
                    let state = gh.mergeable_state(&pr.repo, pr.number);
                    let _ = self.db.set_pr_mergeable(&pr.task_id(), state.as_deref());
                }
            }
        }
        // Only reap when every repo answered: a failed poll would otherwise look
        // like every issue in that repo had been deleted.
        if failed.is_none() {
            self.reap_missing(Source::Github, &seen);
        }

        match failed {
            None => {
                let _ = self.db.meta_set(meta::GITHUB_STATUS, "ok");
                let _ = self.db.meta_set(meta::GITHUB_LAST_OK, &crate::db::now());
                let _ = self.db.meta_set(meta::GITHUB_FAILURES, "0");
            }
            Some(e) => {
                let failures = self.bump_failures(meta::GITHUB_FAILURES);
                let _ = self.db.meta_set(meta::GITHUB_STATUS, &format!("error:{e}"));
                self.log
                    .warn(format!("github poll failed (attempt {failures}): {e}"));
            }
        }
        all_pulls
    }

    /// Every branch the board has dispatched onto, with the repository it was
    /// dispatched for when the task names one.
    fn attempt_branches(&self) -> AttemptBranches {
        let mut set = AttemptBranches::default();
        for task in self.db.load_tasks().unwrap_or_default() {
            for branch in task.attempts.iter().filter_map(|a| a.branch.clone()) {
                match crate::model::gh_repo(&task.id) {
                    Some(repo) => set
                        .in_repo
                        .entry(repo.to_string())
                        .or_default()
                        .insert(branch),
                    None => set.anywhere.insert(branch),
                };
            }
        }
        set
    }

    /// Attach PRs to tasks by attempt branch (`board/<identifier>`), which is
    /// the link the dispatcher creates.
    pub fn link_pull_requests(&self, pulls: &[PullRequest]) -> Result<()> {
        if pulls.is_empty() {
            return Ok(());
        }
        let check_mergeable = self.due_for_full_sweep();
        let gh = self.github.as_ref();
        for task in self.db.load_tasks()? {
            let branches: Vec<String> = task
                .attempts
                .iter()
                .filter_map(|a| a.branch.clone())
                .collect();
            // A GitHub task owns a repo, and only that repo's pull requests can
            // be its own. Branch names are not unique across repos — `gh#2` in
            // two repos both branched to `board/gh-2` — so matching on the
            // branch alone attached another repo's merged PR to this task and
            // derived it straight to review (herdr-board AGE-20). Branches
            // carry their repo now, but the scope stays: attempts recorded
            // before that do not, and `--branch` can name anything at all.
            // Linear identifiers are globally unique, so Linear rows need no
            // such scoping.
            let own_repo = crate::model::gh_repo(&task.id);
            let Some(pr) = pulls.iter().find(|p| {
                own_repo.is_none_or(|r| p.repo == r)
                    && branches.iter().any(|b| pr_matches_branch(p, b))
            }) else {
                continue;
            };
            self.db
                .set_pr(&task.id, Some(&pr.url), Some(pr.number), pr.open)?;
            // Observing the merge is the same fact as performing it. A PR
            // merged with `gh pr merge` or on the web must not leave its
            // ticket in review forever — and not merely unadvanced: nothing
            // but a finished task ends `review`, so the state kept standing
            // and the review writeback kept asserting it, which is what
            // pulled hand-closed tickets back (herdr-board AGE-21/22). The
            // idempotency key stops the next poll re-sending it.
            if pr.merged && !task.pr_merged && !task.state.is_terminal() {
                self.finish_on_merge(&task, &pr.repo, pr.number)?;
            }
            self.db.set_pr_merged(&task.id, pr.merged)?;
            if check_mergeable
                && pr.open
                && let Some(gh) = gh
            {
                let state = gh.mergeable_state(&pr.repo, pr.number);
                self.db.set_pr_mergeable(&task.id, state.as_deref())?;
            }
        }
        Ok(())
    }

    fn bump_failures(&self, key: &str) -> i64 {
        let n = self
            .db
            .meta_get(key)
            .ok()
            .flatten()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        let _ = self.db.meta_set(key, &n.to_string());
        n
    }

    /// Loop guard: an update whose timestamp sits inside the window right after
    /// our own writeback is our own echo. We never dispatch from upstream
    /// events at all, so this only has to stop log churn.
    fn is_own_echo(&self, task_id: &str, updated_at: &str) -> bool {
        let Ok(Some(ours)) = self.db.meta_get(&meta::writeback_at(task_id)) else {
            return false;
        };
        let (Ok(ours), Ok(theirs)) = (
            chrono::DateTime::parse_from_rfc3339(&ours),
            chrono::DateTime::parse_from_rfc3339(updated_at),
        ) else {
            return false;
        };
        let delta = (theirs - ours).num_seconds();
        (0..=5).contains(&delta)
    }

    // ---- reconciliation -------------------------------------------------

    /// Has this attempt produced commits on its branch?
    ///
    /// A pull request is not the only evidence of finished work: an agent that
    /// commits, pushes and stops is done, and waiting for a PR that is never
    /// coming leaves the row `working` forever. Counting commits against the
    /// attempt's own starting commit is the local equivalent of "explicit done
    /// detection". Half the answer: whether those commits are anywhere but
    /// this box is [`SyncEngine::commits_are_on_origin`], and the two together
    /// are [`SyncEngine::attempt_commits`], which [`SyncEngine::maybe_settle`]
    /// consumes (§H4). The ranking of what they measure is
    /// [`crate::settled::decide`].
    pub fn attempt_has_commits(&self, worktree: Option<&str>, base_sha: Option<&str>) -> bool {
        let Some(worktree) = worktree else {
            return false;
        };
        if !std::path::Path::new(worktree).exists() {
            return false;
        }
        // The attempt's own starting commit is the only correct base. Anything
        // else measures the operator's unpushed work as the agent's: a repo
        // whose default branch is one commit ahead of its remote made every
        // dispatch look finished the instant it started (herdr-board AGE-19).
        if let Some(sha) = base_sha {
            let out = Command::new("git")
                .args([
                    "-C",
                    worktree,
                    "rev-list",
                    "--count",
                    &format!("{sha}..HEAD"),
                ])
                .output();
            if let Ok(o) = out
                && o.status.success()
                && let Ok(n) = String::from_utf8_lossy(&o.stdout).trim().parse::<u32>()
            {
                return n > 0;
            }
            // The recorded base is gone (worktree rebuilt, history rewritten).
            // Fall through rather than guess — but the fallback below is the
            // very thing that was wrong, so say so.
            self.log.info(format!(
                "base {sha} unusable in {worktree}; falling back to remote-relative count"
            ));
        }
        // Attempts dispatched before base_sha existed have no starting point
        // recorded, so they keep the old, weaker measurement.
        let base = Command::new("git")
            .args(["-C", worktree, "rev-parse", "--abbrev-ref", "origin/HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "HEAD@{upstream}".to_string());

        let count = |range: &str| -> Option<u32> {
            let out = Command::new("git")
                .args(["-C", worktree, "rev-list", "--count", range])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
                .flatten()
        };

        // Try the remote default branch, then the local one git reports for the
        // main checkout. Either way: commits on this branch that are not on the
        // base mean the agent produced something.
        for range in [
            format!("{base}..HEAD"),
            "master..HEAD".into(),
            "main..HEAD".into(),
        ] {
            if let Some(n) = count(&range) {
                return n > 0;
            }
        }
        false
    }

    /// What this attempt's commits amount to (gh#69): nothing, work only its
    /// own box can see, or work on origin.
    ///
    /// [`SyncEngine::attempt_has_commits`] answers the first half — is there
    /// anything past the base — and this adds the half that decides whether a
    /// settle may call it reviewable. `ask_github` allows the second, remote
    /// tier of that check; see [`SyncEngine::commits_are_on_origin`].
    pub fn attempt_commits(&self, task: &Task, attempt: &Attempt, ask_github: bool) -> Commits {
        if !self.attempt_has_commits(attempt.worktree.as_deref(), attempt.base_sha.as_deref()) {
            return Commits::None;
        }
        if self.commits_are_on_origin(task, attempt, ask_github) {
            Commits::Pushed
        } else {
            Commits::Unpushed
        }
    }

    /// Is this attempt's HEAD on origin?
    ///
    /// The bar is containment, not existence: a retry reuses its predecessor's
    /// branch, so "there is a branch called `board/gh-69-comet-board` on
    /// origin" would settle a retry on the *previous* attempt's push. What
    /// makes the work reviewable is that these commits are there.
    ///
    /// Two tiers, cheapest first:
    ///
    /// 1. **A remote-tracking ref that contains HEAD.** Free, offline, and
    ///    true of every ordinary `git push`: git writes `refs/remotes/origin/…`
    ///    itself when the push succeeds, and a linked worktree shares those
    ///    refs with the checkout it was cut from. It is also the only tier that
    ///    works at all for a remote that is not GitHub.
    /// 2. **GitHub, asked directly** — for a push made straight to a URL
    ///    (`git push https://…`, which updates no tracking ref at all: exactly
    ///    what the credential path of gh#68 hands an agent) or into a checkout
    ///    whose refs have since been pruned. Only on the event path, for the
    ///    reason [`SyncEngine::recheck_pull_request`] gives: a run ending is a
    ///    rare event, whereas the interval reconcile re-asks every cycle for as
    ///    long as the attempt stays live, and one API call per cycle per stuck
    ///    attempt is a poll nobody asked for.
    ///
    /// Unproven reads as unpushed, always. The cost of that is an attempt that
    /// stays live longer than it had to — visible, and bounded by the wall
    /// clock cap — against a row that says `review` about work nobody can
    /// fetch, which is the bug.
    fn commits_are_on_origin(&self, task: &Task, attempt: &Attempt, ask_github: bool) -> bool {
        let Some(worktree) = attempt.worktree.as_deref() else {
            return false;
        };
        let Some(head) = git_out(worktree, &["rev-parse", "HEAD"]) else {
            return false;
        };
        // `--count=1` because the question is whether *any* remote holds it;
        // the walk stops as soon as one does.
        if git_out(
            worktree,
            &[
                "for-each-ref",
                "--count=1",
                "--contains",
                &head,
                "refs/remotes/",
            ],
        )
        .is_some_and(|refs| !refs.is_empty())
        {
            return true;
        }
        if !ask_github {
            return false;
        }
        let (Some(gh), Some(branch)) = (self.github.as_ref(), attempt.branch.as_deref()) else {
            return false;
        };
        // The task's own repo when it has one; otherwise whatever the checkout
        // pushes to, which is all a Linear ticket can offer.
        let Some(repo) = crate::model::gh_repo(&task.id)
            .map(str::to_string)
            .or_else(|| crate::git_credentials::repo_for_checkout(worktree))
        else {
            return false;
        };
        let Some(remote_head) = gh.branch_head(&repo, branch) else {
            return false;
        };
        // Containment against a commit we hold locally. A remote tip we have
        // never fetched cannot be checked, and unproven reads as unpushed.
        git_ok(
            worktree,
            &["merge-base", "--is-ancestor", &head, &remote_head],
        )
    }

    /// Map live attempts onto the sessions the engine currently reports.
    ///
    /// The lifecycle half of what herdr-board's `reconcile_with` did, minus
    /// everything that existed to distrust a terminal. Statuses arrive already
    /// mapped through [`crate::runtime::agent_status`] — the staleness gate has
    /// been applied, `AwaitingInput` is `Blocked`, `Errored` is `Blocked`.
    ///
    /// Lifecycle decisions here are deliberately few:
    ///
    /// - A chat whose session row vanished *after the agent was seen working*
    ///   is a chat that was archived or deleted out from under its attempt —
    ///   session rows persist as `Idle` after a run ends, so absence after
    ///   activity means the chat itself is gone. That orphans on the same
    ///   two-consecutive-ticks rule herdr-board used, so a transient snapshot
    ///   cannot flap an attempt into `failed`.
    /// - A chat that has *never* had a session row is indistinguishable from a
    ///   dispatch whose first run has not started yet (the brief sits in the
    ///   command ledger until the host device executes it). Ticks are counted
    ///   for observability, but the verdict needs `Runtime::chat_alive` — H2.
    ///   Nothing is orphaned on absence-of-evidence alone.
    /// - Settling (§H4): a live attempt whose chat's last run has ended is
    ///   checked for artifacts — see [`SyncEngine::maybe_settle`]. This is the
    ///   catch-up path (the event path settles the moment the run ends); it
    ///   makes no fresh PR lookup because the cycle polled seconds ago.
    /// - Re-opening (§H4's inverse): a settled attempt whose chat is working
    ///   again — [`SyncEngine::rewatch_settled_attempts`].
    ///
    /// Call this on the steady sync interval only. Session-watch *events*
    /// should go through [`SyncEngine::refresh_statuses`] instead, so a burst
    /// of change notifications cannot run the missing-ticks counter faster
    /// than wall clock — the exact flap gh#34 taught herdr-board about.
    pub fn reconcile_sessions(&self, statuses: &SessionStatuses) -> Result<()> {
        self.reconcile_sessions_with(statuses, None)
    }

    /// As [`SyncEngine::reconcile_sessions`], with the [`Runtime`] whose run
    /// journal the settle logic reads. Without one an `Errored` end cannot be
    /// told apart from a question mid-run (both read `Blocked`), so neither is
    /// acted on.
    pub fn reconcile_sessions_with(
        &self,
        statuses: &SessionStatuses,
        runtime: Option<&dyn Runtime>,
    ) -> Result<()> {
        for attempt in self.db.live_attempts()? {
            let Some(chat_id) = attempt.pane_id.as_deref() else {
                // Dispatch is still in flight; nothing to reconcile yet.
                continue;
            };
            let Some(task) = self.db.get_task(&attempt.task_id)? else {
                continue;
            };
            let status = statuses
                .get(chat_id)
                .copied()
                .unwrap_or(AgentStatus::Missing);

            if status == AgentStatus::Missing {
                let ticks = attempt.missing_ticks + 1;
                if !attempt.saw_working {
                    // Possibly a run that has not started yet. Count, log once
                    // it looks wrong, decide nothing — see the doc comment.
                    self.db.set_missing_ticks(attempt.id, ticks)?;
                    if ticks == 2 {
                        self.log.warn(format!(
                            "{} chat {} has no session and has never worked — \
                             leaving it until liveness can be checked (H2)",
                            task.identifier, chat_id
                        ));
                    }
                    continue;
                }
                if ticks >= 2 {
                    self.log.warn(format!(
                        "{} chat {} gone for {} ticks — orphaned",
                        task.identifier, chat_id, ticks
                    ));
                    self.orphan(runtime, &task, &attempt)?;
                } else {
                    self.log.info(format!(
                        "{} chat {} missing (tick {}/2)",
                        task.identifier, chat_id, ticks
                    ));
                    self.db.set_missing_ticks(attempt.id, ticks)?;
                }
                continue;
            }

            if attempt.missing_ticks != 0 {
                // It came back — a sync hiccup, not a death.
                self.db.set_missing_ticks(attempt.id, 0)?;
            }
            // Persist it so readers render state without a session watch of
            // their own.
            let entered_blocked = self.entered_blocked(&attempt, status);
            if attempt.agent_status != Some(status) {
                self.db.set_attempt_status(attempt.id, status)?;
            }
            // Latch that the agent actually got going, so a settled status can
            // be told apart from one that never started.
            if status == AgentStatus::Working && !attempt.saw_working {
                self.db.set_saw_working(attempt.id)?;
            }
            // §H4: the turn ended — check the checkout. No fresh PR lookup on
            // this path: the cycle polled GitHub moments ago, so the recorded
            // PR state is as fresh as a lookup would be.
            let settled = self.maybe_settle(runtime, &task, &attempt, status, false)?;
            // Blocked *and* settled is an errored run whose pull request was
            // already open: the work is reviewable, so it is a settle and not
            // a block, and saying both would be the board contradicting
            // itself in two comments on the same issue.
            if entered_blocked && !settled {
                self.note_blocked(runtime, &task, &attempt, chat_id)?;
            }
        }
        // After settling, so an attempt that just finished is never failed for
        // running long; before the rewatch, which only looks at closed rows.
        self.enforce_duration_cap(runtime)?;
        // Last, because it may re-open an attempt: doing it first would hand
        // the live pass above a row it has already decided about.
        self.rewatch_settled_attempts(statuses)?;
        Ok(())
    }

    // ---- the wall-clock cap (gh#70) --------------------------------------

    /// Bound every live attempt by wall time: warn its chat once past the
    /// route's `max_duration`, then close it `failed` when the grace runs out.
    ///
    /// Nothing else bounds a *running* attempt. The engine's stall watchdog
    /// hard-stops only a run that emits nothing — its silence-after-output tier
    /// is advisory by design — so an agent looping happily keeps running until
    /// somebody looks, and `attempts.started_at` was a column nobody read. The
    /// same clock closes the other end of that gap: an engine crash past its
    /// revival budget settles the chat `Idle`, and with no commits
    /// [`crate::settled::decide`] says `StayLive(NoArtifacts)` — orphaning
    /// fires on a *missing* session row, and that one exists, so without this
    /// the row renders `working` forever.
    ///
    /// Three properties, in the order they matter:
    ///
    /// - **Never silent.** The breach is a warning in the chat and in the log
    ///   first; the close carries an upstream comment naming the timeout. An
    ///   agent that gets its warning can commit what it has and open a pull
    ///   request, and the settle beats the cancel — a run that finishes inside
    ///   the grace closes `done` on its artifacts like any other.
    /// - **Wall time, not event time.** Called from the interval reconcile
    ///   only, exactly as orphaning is, so a burst of session-watch events
    ///   cannot age an attempt faster than the clock.
    /// - **Every live attempt**, whatever its status. `blocked` holds a chat
    ///   and a concurrency slot as surely as `working` does, and a dispatch
    ///   that never got a session row at all is the most stranded of the lot.
    ///   The cap bounds the *attempt*; which of the ways it got stuck it took
    ///   is the log line's business.
    ///
    /// `runtime` is what talks to the chat. Without one — the read-only callers
    /// — the verdict still stands and is still logged and written back; only
    /// the chat-side warning and interrupt are skipped, because there is
    /// nothing to send them through.
    fn enforce_duration_cap(&self, runtime: Option<&dyn Runtime>) -> Result<()> {
        let interval = self.cfg.sync.interval_secs();
        let now = chrono::Utc::now();
        for attempt in self.db.live_attempts()? {
            let Some(task) = self.db.get_task(&attempt.task_id)? else {
                continue;
            };
            // The route the task resolves to *now*: an attempt outlives the
            // config that released it, and `max_duration_secs` treats a route
            // that has since gone away as the default cap rather than as none.
            let route = self.cfg.resolve(&route_context(&task));
            let Some(cap) = self.cfg.max_duration_secs(route) else {
                continue;
            };
            let Some(age) = secs_since(&attempt.started_at, now) else {
                // An unreadable `started_at` is a row we cannot age. Say so
                // rather than silently exempting it from the cap forever.
                self.log.warn(format!(
                    "{}: attempt {} has an unreadable started_at ({}) — \
                     the duration cap cannot see it",
                    task.identifier, attempt.id, attempt.started_at
                ));
                continue;
            };
            // A stamp we cannot parse counts as "warned just now": it means the
            // warning went out, and the grace restarting is the safe direction
            // to be wrong in.
            let warned = attempt
                .overrun_warned_at
                .as_deref()
                .map(|t| secs_since(t, now).unwrap_or(0));
            let grace = overrun::grace_secs(cap, interval);
            match overrun::decide(age, warned, cap, grace) {
                overrun::Verdict::Within => {}
                overrun::Verdict::Warn => {
                    self.warn_overrun(runtime, &task, &attempt, age, cap, grace)?
                }
                overrun::Verdict::Cancel => {
                    self.cancel_overrun(runtime, &task, &attempt, age, cap)?
                }
            }
        }
        Ok(())
    }

    /// The prompt-once warning: into the chat, so the agent can wrap up, and
    /// into the log, so the operator sees it whether or not the chat is alive.
    ///
    /// Stamped **before** the prompt, and stamped whatever the prompt reports.
    /// A chat that refuses delivery is a chat there is no point re-telling
    /// every cycle, and a warning that only counted when it landed would leave
    /// a dead chat's attempt live forever — which is the bug, not the fix.
    fn warn_overrun(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        age: i64,
        cap: u64,
        grace: u64,
    ) -> Result<()> {
        self.db.set_overrun_warned(attempt.id)?;
        self.log.warn(format!(
            "{} has been live for {} — past its {} cap; \
             warning chat {} and cancelling in {}",
            task.identifier,
            overrun::human_secs(age),
            overrun::human_secs(cap as i64),
            attempt.pane_id.as_deref().unwrap_or("(none yet)"),
            overrun::human_secs(grace as i64),
        ));
        let (Some(runtime), Some(chat_id)) = (runtime, attempt.pane_id.as_deref()) else {
            return Ok(());
        };
        let text = format!(
            "comet-board: this attempt has been running for {} — past the {} cap for its route. \
             It will be cancelled in {}. Commit what you have and open a pull request now; \
             if you are looping on something, stop and say so in the PR description.",
            overrun::human_secs(age),
            overrun::human_secs(cap as i64),
            overrun::human_secs(grace as i64),
        );
        if let Err(e) = runtime.prompt(chat_id, &text) {
            self.log.warn(format!(
                "{}: warning chat {chat_id} about the duration cap: {e:#}",
                task.identifier
            ));
        }
        Ok(())
    }

    /// The grace ran out. Interrupt and archive the chat as a cancel does,
    /// close the attempt `failed`, and write back upstream naming the timeout.
    ///
    /// `failed` rather than `cancelled`: nobody chose this, and the two
    /// verdicts derive differently — `cancelled` returns the issue to `ready`
    /// as if the attempt had never been made, while `failed` renders the row
    /// red and keeps it there until somebody decides what to do with it. A run
    /// that hit the ceiling is exactly the second thing.
    fn cancel_overrun(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        age: i64,
        cap: u64,
    ) -> Result<()> {
        if let (Some(runtime), Some(chat_id)) = (runtime, attempt.pane_id.as_deref())
            && let Err(e) = runtime.cancel(chat_id)
        {
            // The attempt closes either way: a chat that cannot be interrupted
            // is a reason to stop counting it as live, not to keep it.
            self.log.warn(format!(
                "{}: cancelling chat {chat_id} at the duration cap: {e:#}",
                task.identifier
            ));
        }
        let note = format!(
            "timed out after {} (cap {})",
            overrun::human_secs(age),
            overrun::human_secs(cap as i64),
        );
        self.db.close_attempt(attempt.id, Outcome::Failed)?;
        self.enqueue_outcome_note(task, Outcome::Failed, None, Some(&note))?;
        self.log
            .warn(format!("{}: attempt failed — {note}", task.identifier));
        Ok(())
    }

    // ---- reclaiming checkouts (gh#72) ------------------------------------

    /// Delete the checkout and local branch of every attempt nobody is coming
    /// back for, once it has sat unclaimed for `[defaults] retain_worktrees`.
    ///
    /// Every dispatch cuts a worktree and a branch, and until this landed
    /// nothing removed either: settle, orphan, cancel and retry-replace all
    /// close the attempt row and leave the checkout, so a box running a few
    /// tasks a day filled up with a full copy of the repo per attempt. The
    /// decisions are [`gc::standing`] and [`gc::decide`]; this is the sweep
    /// around them.
    ///
    /// Three properties, in the order they matter:
    ///
    /// - **Never a live checkout.** A task with any live attempt is skipped
    ///   whole — retries reuse the branch, so a closed attempt's directory is
    ///   very often the live one's — and so is a task in review, whose branch a
    ///   reviewer may check out and whose chat review delivery still matches
    ///   against this path ([`crate::review`]).
    /// - **Wall time from when it was freed**, not from when the attempt ended:
    ///   a pull request open for a fortnight would otherwise be collected the
    ///   instant it merged. The mark is stamped on one sweep and read on a
    ///   later one, and a task that comes back to life clears it.
    /// - **Never silent.** The mark and the collection are both log lines
    ///   naming the path, so the week between them is visible to somebody who
    ///   wants the checkout kept — `retain_worktrees = "off"` is the answer,
    ///   and `doctor` reports the cost of choosing it.
    ///
    /// Collecting cannot strand a re-opened attempt, and the two rules are the
    /// same rule: [`settled::should_reopen`] refuses to re-open on an upstream
    /// that is final or a task marked done, which is exactly what
    /// [`gc::Standing::Spent`] requires. A checkout is only ever deleted from
    /// under an attempt that can no longer come back to life.
    ///
    /// Failure is never fatal to the cycle: an error is logged and the attempt
    /// is left unmarked as collected, so the next sweep tries again.
    fn collect_worktrees(&self, runtime: Option<&dyn Runtime>) {
        let Some(retain) = self.cfg.retain_worktrees_secs() else {
            return;
        };
        let now = chrono::Utc::now();
        let tasks = match self.db.load_tasks() {
            Ok(t) => t,
            Err(e) => {
                self.log
                    .error(format!("worktree gc: reading the board: {e}"));
                return;
            }
        };
        for task in &tasks {
            for attempt in &task.attempts {
                if attempt.collected_at.is_some() || attempt.worktree.is_none() {
                    continue;
                }
                let spent = attempt
                    .collectable_at
                    .as_deref()
                    .map(|t| secs_since(t, now).unwrap_or(0));
                let standing = gc::standing(task, attempt);
                if let Err(e) = match gc::decide(standing, spent, retain) {
                    gc::Verdict::Keep => Ok(()),
                    gc::Verdict::Mark => self.mark_collectable(task, attempt, retain),
                    gc::Verdict::Unmark => self.db.set_attempt_collectable(attempt.id, false),
                    gc::Verdict::Collect => self.collect_one(runtime, task, attempt),
                } {
                    self.log.warn(format!(
                        "{}: worktree gc on attempt {}: {e:#}",
                        task.identifier, attempt.id
                    ));
                }
            }
        }
    }

    /// Start the retention clock, and say so — the one warning anybody gets
    /// before a checkout is deleted a week later.
    fn mark_collectable(&self, task: &Task, attempt: &Attempt, retain: u64) -> Result<()> {
        self.db.set_attempt_collectable(attempt.id, true)?;
        self.log.info(format!(
            "{}: {} is finished with — its checkout {} goes in {}",
            task.identifier,
            attempt
                .branch
                .as_deref()
                .unwrap_or("the attempt's own branch"),
            attempt.worktree.as_deref().unwrap_or("(none)"),
            gc::human_window(retain),
        ));
        Ok(())
    }

    /// The window ran out: hand the checkout back to the engine, which owns
    /// worktrees, and record that it is gone.
    ///
    /// A cycle run without a runtime marks and unmarks but deletes nothing —
    /// the same split the duration cap makes, and the deliberate one here: only
    /// the process that owns the worktrees may remove one, and everything else
    /// is welcome to keep the clock.
    fn collect_one(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
    ) -> Result<()> {
        let Some(worktree) = attempt.worktree.as_deref() else {
            return Ok(());
        };
        let Some(runtime) = runtime else {
            return Ok(());
        };
        runtime.reclaim_worktree(
            attempt.repo_path.as_deref(),
            worktree,
            attempt.branch.as_deref(),
        )?;
        self.db.set_attempt_collected(attempt.id)?;
        self.log.info(format!(
            "{}: reclaimed {} and branch {}",
            task.identifier,
            worktree,
            attempt.branch.as_deref().unwrap_or("(none recorded)"),
        ));
        Ok(())
    }

    /// End an attempt whose chat is gone, and tell everyone reading upstream.
    ///
    /// Notified on the same channels a settle is (gh#71): an attempt ending
    /// because its chat vanished is still the end of the work a dispatcher was
    /// waiting on, and it is the ending nobody would otherwise notice.
    fn orphan(&self, runtime: Option<&dyn Runtime>, task: &Task, attempt: &Attempt) -> Result<()> {
        self.db.close_attempt(attempt.id, Outcome::Orphaned)?;
        self.enqueue_outcome(task, Outcome::Orphaned, None)?;
        self.announce(
            runtime,
            task,
            attempt,
            Signal::Settled {
                outcome: Outcome::Orphaned,
                evidence: None,
                pr_url: None,
            },
        );
        Ok(())
    }

    // ---- settling (§H4) --------------------------------------------------

    /// Has this attempt's chat's last run ended, and if so how did it end?
    ///
    /// The status carries most of the answer: `Idle` is only ever written
    /// after the engine journals a `Done`, and a chat is fresh per attempt, so
    /// `Idle` means *this attempt's* run ended — whether it completed or was
    /// interrupted decides nothing (both settle the same way), so no journal
    /// read is needed. `Blocked` is the one status hiding two different facts
    /// — `Errored` (the run ended, badly) and `AwaitingInput` (the run is
    /// alive, asking) — and only the journal can split them.
    ///
    /// `None` for everything else: `Working` is a live run, `Unknown` is a
    /// crashed engine (absence of evidence, which must never read as
    /// completion), `Missing` is the orphan logic's business.
    fn run_end(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        chat_id: &str,
        status: AgentStatus,
    ) -> Option<RunEnd> {
        match status {
            AgentStatus::Idle | AgentStatus::Done => Some(RunEnd::Completed),
            AgentStatus::Blocked => match runtime?.last_run_end(chat_id) {
                Ok(Some(RunEnd::Errored)) => Some(RunEnd::Errored),
                // A live run mid-question — or a journal that disagrees with
                // the status, where doing nothing is the safe reading.
                Ok(_) => None,
                Err(e) => {
                    self.log.warn(format!(
                        "{}: reading chat {chat_id}'s journal: {e}",
                        task.identifier
                    ));
                    None
                }
            },
            _ => None,
        }
    }

    /// The §H4 settle check: if this attempt's run has ended, weigh the
    /// artifacts and maybe close it. Returns whether it settled.
    ///
    /// `ask_github` allows the two targeted GitHub lookups the decision can
    /// want — an unrecorded pull request, and (gh#69) a branch pushed without
    /// leaving a tracking ref behind. Only the event path passes true: the
    /// interval path polls before it reconciles, so a PR lookup there would
    /// repeat the poll, and it re-asks every cycle for as long as the attempt
    /// is live, so a branch lookup there would be a poll of its own. The PR
    /// lookup is additionally gated on commits existing, because a pull
    /// request cannot exist without them.
    fn maybe_settle(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        status: AgentStatus,
        ask_github: bool,
    ) -> Result<bool> {
        let Some(chat_id) = attempt.pane_id.as_deref() else {
            return Ok(false);
        };
        let Some(end) = self.run_end(runtime, task, chat_id, status) else {
            return Ok(false);
        };
        let mut pr_open = task.pr_open;
        let mut pr_url = task.pr_url.clone().filter(|_| pr_open);
        if !pr_open
            && ask_github
            && self.attempt_has_commits(attempt.worktree.as_deref(), attempt.base_sha.as_deref())
            && let Some(url) = self.recheck_pull_request(task, attempt)
        {
            pr_open = true;
            pr_url = Some(url);
        }
        let verdict = settled::decide(end, pr_open, || {
            self.attempt_commits(task, attempt, ask_github)
        });
        match verdict {
            Verdict::Finished(evidence) => {
                self.settle(runtime, task, attempt, evidence, pr_url.as_deref())?;
                Ok(true)
            }
            // The one StayLive worth a line: a row that stays `working`
            // because its commits never left the box looks identical to one
            // whose agent is still typing, and the difference is the whole of
            // gh#69. Said once per attempt — the interval path asks again
            // every cycle, and a log that repeats it is a log nobody reads.
            Verdict::StayLive(Why::Unpushed) => {
                self.note_unpushed(task, attempt);
                Ok(false)
            }
            // Not logged: the interval path re-asks every cycle, and an
            // errored or artifact-less attempt is already visible on the
            // board as blocked / dim idle. The settle is the event.
            Verdict::StayLive(_) => Ok(false),
        }
    }

    /// Say, once, that an attempt is sitting on work only its own box can see.
    fn note_unpushed(&self, task: &Task, attempt: &Attempt) {
        let key = meta::unpushed_noted(attempt.id);
        if matches!(self.db.meta_get(&key), Ok(Some(_))) {
            return;
        }
        let _ = self.db.meta_set(&key, &crate::db::now());
        self.log.info(format!(
            "{}: run ended with commits that are not on origin{} — attempt stays live \
             (push the branch, or open a pull request, to settle it)",
            task.identifier,
            attempt
                .branch
                .as_deref()
                .map(|b| format!(" ({b})"))
                .unwrap_or_default(),
        ));
    }

    /// Ask GitHub, now, whether this attempt's branch has an open pull
    /// request the board has not recorded yet.
    ///
    /// A run ends the moment the journal says so; the board records pull
    /// requests on its poll cycle; and the two race reliably, because an agent
    /// opens its PR moments before its final turn ends. herdr chose to live
    /// inside that window and made its settle notice "never assert an absence
    /// it has not checked" (its gh#29). Here the check is affordable — run
    /// ends are rare events, where herdr's settles were idle samples — so the
    /// window is closed instead: one `pulls` call per repo the task can own.
    fn recheck_pull_request(&self, task: &Task, attempt: &Attempt) -> Option<String> {
        let gh = self.github.as_ref()?;
        let branch = attempt.branch.as_deref()?;
        // The poll's own scoping rule (see `link_pull_requests`): a GitHub
        // task owns its repo and only that repo's PRs can be its own; a Linear
        // task names none, so its branch is honoured wherever it turns up.
        let repos: Vec<String> = match crate::model::gh_repo(&task.id) {
            Some(r) => vec![r.to_string()],
            None => self.cfg.github.repos.clone(),
        };
        for repo in &repos {
            let pulls = match gh.pulls(repo) {
                Ok(p) => p,
                Err(e) => {
                    // A GitHub outage must not block the settle — the commits
                    // verdict stands, and the next poll links the PR.
                    self.log
                        .warn(format!("{}: PR recheck in {repo}: {e}", task.identifier));
                    continue;
                }
            };
            if let Some(pr) = pulls
                .iter()
                .find(|p| p.open && pr_matches_branch(p, branch))
            {
                // Record it, so the settle carries the URL and the row derives
                // straight to review with its pull request attached.
                let _ = self
                    .db
                    .set_pr(&task.id, Some(&pr.url), Some(pr.number), true);
                return Some(pr.url.clone());
            }
        }
        None
    }

    /// Close an attempt whose evidence cleared the bar, and tell everyone who
    /// was waiting on it: the tracker, the agent that released it, and the
    /// operator's out-of-band channel. The §H4 half of what herdr-board's
    /// `settle` did, now including its AGE-25 dispatcher wake (gh#71).
    fn settle(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        evidence: Evidence,
        pr_url: Option<&str>,
    ) -> Result<()> {
        self.db.close_attempt(attempt.id, Outcome::Done)?;
        self.enqueue_outcome(task, Outcome::Done, pr_url)?;
        self.log.info(format!(
            "{}: run ended with {} — attempt done",
            task.identifier,
            evidence.as_str()
        ));
        self.announce(
            runtime,
            task,
            attempt,
            Signal::Settled {
                outcome: Outcome::Done,
                evidence: Some(evidence),
                pr_url: pr_url.map(str::to_string),
            },
        );
        Ok(())
    }

    // ---- notification (gh#71) --------------------------------------------

    /// Tell whoever is not watching the board that something happened to a
    /// dispatched attempt.
    ///
    /// Two channels, two switches, and they are independent because they are
    /// different audiences (see [`crate::notify`]): the chat of the agent that
    /// released the work (`notify_dispatcher`, off by default) and the
    /// operator's webhook (`notify` + `notify_webhook`). The third channel —
    /// the comment upstream — is not here: outcome comments are queued by
    /// [`SyncEngine::enqueue_outcome`] and the blocked comment by
    /// [`SyncEngine::note_blocked`], because those are retryable writebacks
    /// rather than best-effort notices.
    ///
    /// Nothing here can fail the caller. A settle that has already happened is
    /// not undone because a webhook host is down, and an attempt is not left
    /// open because a dispatcher's chat was archived.
    fn announce(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        signal: Signal,
    ) {
        if let Signal::Settled {
            outcome,
            evidence,
            pr_url,
        } = &signal
        {
            self.wake_dispatcher(
                runtime,
                task,
                attempt,
                *outcome,
                *evidence,
                pr_url.as_deref(),
            );
        }
        self.post_webhook(task, attempt, &signal);
    }

    /// Queue a settle notice into the chat of the agent that released this
    /// work (herdr-board's AGE-25).
    ///
    /// The provenance is already on the attempt row — `dispatched_by_pane` is
    /// the chat the dispatch ran from, recorded for every agent-issued
    /// dispatch whether or not the board started that chat. So the delivery is
    /// the same [`Runtime::prompt`] a review uses: a steer into a live run, a
    /// send otherwise, durable either way.
    ///
    /// Silent for an operator-released task by construction — there is no chat
    /// to prompt — which is why this switch is separate from the operator's.
    fn wake_dispatcher(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        outcome: Outcome,
        evidence: Option<Evidence>,
        pr_url: Option<&str>,
    ) {
        if !self.cfg.defaults.notify_dispatcher {
            return;
        }
        let dispatcher = attempt.dispatcher();
        let Some(chat) = dispatcher.pane() else {
            return;
        };
        // A chat that dispatched into itself would be prompted about its own
        // settle. Not reachable through `comet-board dispatch` (a dispatch
        // makes a fresh chat), but a hand-set `--via` can say anything.
        if Some(chat) == attempt.pane_id.as_deref() {
            return;
        }
        let Some(runtime) = runtime else {
            self.log.warn(format!(
                "{}: settled, but no runtime to notify chat {chat} with",
                task.identifier
            ));
            return;
        };
        // The dispatcher is usually a long-lived orchestrator that outlives
        // many children, but it can have been archived since. Not an error:
        // there is simply nobody to tell.
        match runtime.chat_alive(chat) {
            Ok(false) => {
                self.log.info(format!(
                    "{}: settled, but the chat that released it ({chat}) is gone — \
                     nobody notified",
                    task.identifier
                ));
                return;
            }
            Err(e) => {
                self.log
                    .warn(format!("{}: checking chat {chat}: {e}", task.identifier));
                return;
            }
            Ok(true) => {}
        }
        let text = notify::dispatcher_message(task, attempt, outcome, evidence, pr_url);
        match runtime.prompt(chat, &text) {
            Ok(()) => self.log.info(format!(
                "{}: settle notice queued into chat {chat}",
                task.identifier
            )),
            // Best effort by design: the attempt is closed and the tracker has
            // the trail. Retrying a notice about a thing that already happened
            // is how a dispatcher gets told twice.
            Err(e) => self.log.warn(format!(
                "{}: could not notify chat {chat}: {e}",
                task.identifier
            )),
        }
    }

    /// POST the event at `[defaults] notify_webhook`, if the operator wants it.
    ///
    /// The only channel that reaches somebody who is looking at neither the
    /// board nor the issue tracker — which at 02:00 is everybody.
    fn post_webhook(&self, task: &Task, attempt: &Attempt, signal: &Signal) {
        if !self.cfg.defaults.notify {
            return;
        }
        let Some(url) = self.cfg.defaults.notify_webhook.as_deref() else {
            return;
        };
        if let Some(problem) = notify::webhook_url_problem(url) {
            self.log.warn(format!(
                "[defaults] notify_webhook is unusable ({problem}); {} went unannounced",
                task.identifier
            ));
            return;
        }
        let body = notify::webhook_payload(task, attempt, signal, &crate::db::now());
        match self.webhook.post(url, &body) {
            Ok(()) => self.log.info(format!(
                "{}: {} posted to {}",
                task.identifier,
                signal.event(),
                notify::webhook_host(url)
            )),
            // Not retried, and the reason is in `notify`'s docs: a
            // notification delivered late reads as current, which is worse
            // than one that never came.
            Err(e) => self.log.warn(format!(
                "{}: {} webhook to {} failed: {e}",
                task.identifier,
                signal.event(),
                notify::webhook_host(url)
            )),
        }
    }

    /// An attempt has just *entered* blocked: leave one comment upstream, and
    /// raise the operator's out-of-band notice (gh#71).
    ///
    /// This is the state the board had no signal for at all. A blocked attempt
    /// settles nothing and closes nothing — that is deliberate, the chat holds
    /// the context and the decision is the operator's — so no outcome
    /// writeback fires and the only trace is a row colour nobody is looking at.
    ///
    /// Once per block, not once per tick and not once per attempt: the counter
    /// bumped here goes into the idempotency key, so a question answered at
    /// 09:00 and a second question at 11:00 are two comments, while the
    /// hundreds of reconcile ticks in between are none.
    fn note_blocked(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        chat_id: &str,
    ) -> Result<()> {
        // Which kind of block, straight off the journal — the same read
        // `run_end` makes, and the only thing that tells a question apart from
        // a dead run inside the one `Blocked` status.
        let why = match runtime.map(|r| r.last_run_end(chat_id)) {
            Some(Ok(Some(RunEnd::Errored))) => Stopped::Errored,
            Some(Ok(_)) => Stopped::Asking,
            Some(Err(_)) | None => Stopped::Unknown,
        };
        let block = self.db.bump_blocked_count(attempt.id)?;
        let queued = self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "blocked".into(),
            payload: json!({
                "reason": why.as_str(),
                "block": block,
                "attempt": task.attempt_count(),
                "log": self.paths.logfile().to_string_lossy(),
            })
            .to_string(),
            idem_key: format!("{}:blocked:{}:{}", task.id, task.attempt_count(), block),
        })?;
        self.log.info(format!(
            "{}: blocked ({}) — {}",
            task.identifier,
            why.as_str(),
            if queued {
                "queued a comment upstream"
            } else {
                "already told upstream about this block"
            }
        ));
        // The row the notice describes: the caller's copy predates the bump,
        // and the payload names which block this is.
        let mut with_count = attempt.clone();
        with_count.blocked_count = block;
        self.announce(runtime, task, &with_count, Signal::Blocked(why));
        Ok(())
    }

    /// Has this attempt just entered blocked, for the first time this block?
    ///
    /// A transition is the usual answer. The `blocked_count == 0` arm is for
    /// the attempt that was already sitting blocked when this feature landed:
    /// its status is persisted, so it will never transition again, and without
    /// this the one case gh#71 is about — an agent that stopped in the night —
    /// would stay silent for as long as it stays stuck.
    fn entered_blocked(&self, attempt: &Attempt, status: AgentStatus) -> bool {
        status == AgentStatus::Blocked
            && (attempt.agent_status != Some(AgentStatus::Blocked) || attempt.blocked_count == 0)
    }

    /// Look again at attempts the board has already closed (§H4's inverse,
    /// herdr gh#34: "a settle the board got wrong").
    ///
    /// An attempt settles on evidence, and evidence can be wrong — commits are
    /// routinely there long before the work is done. A settled attempt whose
    /// chat starts working again was settled wrongly, and is **re-opened, not
    /// re-dispatched**: nobody dispatched anything, so a second attempt row
    /// would claim a retry that never happened. The one number worth keeping —
    /// that the board was wrong about *this* attempt — is `reopened`, carried
    /// on the row and in `list --json`.
    ///
    /// herdr needed a screen-moved check here, because its `working` could be
    /// a stale spinner in scrollback flapping every finished row. comet's
    /// `Working` means a run is executing right now — written by the engine,
    /// staleness-gated by [`crate::runtime::agent_status`] — so the status
    /// alone is the whole check. Its pane-holds-somebody-else check has no
    /// equivalent either: chat ids are never reused, and the chat *is* the
    /// attempt's, so whoever prompts it (review feedback, an operator's
    /// follow-up) is continuing this attempt.
    fn rewatch_settled_attempts(&self, statuses: &SessionStatuses) -> Result<bool> {
        let mut changed = false;
        for attempt in self.db.settled_attempts()? {
            let Some(chat_id) = attempt.pane_id.as_deref() else {
                continue;
            };
            let status = statuses
                .get(chat_id)
                .copied()
                .unwrap_or(AgentStatus::Missing);
            // The cheap pre-filter, before a task read: the usual case by far
            // is a finished chat sitting `Idle` forever, and the settle stands.
            if status != AgentStatus::Working {
                continue;
            }
            let Some(task) = self.db.get_task(&attempt.task_id)? else {
                continue;
            };
            if !settled::should_reopen(
                status,
                task.upstream.is_final(),
                task.local_done,
                task.live_attempt().is_some(),
            ) {
                continue;
            }
            if !self.db.reopen_attempt(attempt.id)? {
                // Lost a race with a dispatch between the check above and
                // here. The live attempt is the current one; nothing wrong.
                continue;
            }
            // Both, so this same pass's derivation already puts the row back
            // in WORKING rather than leaving it claiming review for a tick.
            self.db
                .set_attempt_status(attempt.id, AgentStatus::Working)?;
            self.db.set_saw_working(attempt.id)?;
            self.log.warn(format!(
                "{} was closed as {} but chat {chat_id} is working again — \
                 attempt re-opened ({} time(s) now)",
                task.identifier,
                attempt.outcome.map(Outcome::as_str).unwrap_or("finished"),
                attempt.reopened + 1
            ));
            changed = true;
        }
        Ok(changed)
    }

    /// Refresh the board from the session watch, the moment things happen.
    ///
    /// Deliberately not [`SyncEngine::reconcile_sessions`]: that owns the
    /// clocked lifecycle decision — orphaning a vanished chat — and running it
    /// from every watch event would count `missing_ticks` in event time rather
    /// than wall time. What runs here is what event time is *correct* for:
    ///
    /// - **Status display.** A status change is caused by input — an agent
    ///   becomes blocked when it asks something, unblocked the moment it is
    ///   answered — and waiting an interval tick to notice makes the board lie
    ///   about the one thing it exists to show.
    /// - **Settling (§H4).** A transition onto a settled-looking status *is*
    ///   the run-end event arriving; this is the "the turn ended, now check
    ///   the checkout" moment the run journal replaced the 60-second clock
    ///   with. Checked on the transition only — the interval reconcile owns
    ///   the steady re-check, so a burst of unchanged snapshots costs no git.
    /// - **Re-opening (§H4's inverse).** comet's `Working` is written by the
    ///   engine that runs the agent and staleness-gated on the way in, so —
    ///   unlike herdr's screen-sampled `working` — acting on it immediately
    ///   cannot flap a finished row.
    pub fn refresh_statuses(&self, statuses: &SessionStatuses) -> Result<bool> {
        self.refresh_statuses_with(statuses, None)
    }

    /// As [`SyncEngine::refresh_statuses`], with the [`Runtime`] whose run
    /// journal distinguishes an `Errored` end from a question mid-run.
    pub fn refresh_statuses_with(
        &self,
        statuses: &SessionStatuses,
        runtime: Option<&dyn Runtime>,
    ) -> Result<bool> {
        let mut changed = false;
        for attempt in self.db.live_attempts()? {
            let Some(chat_id) = attempt.pane_id.as_deref() else {
                continue;
            };
            let Some(status) = statuses.get(chat_id).copied() else {
                // Missing chats are the interval reconcile's business, not ours.
                continue;
            };
            let transitioned = attempt.agent_status != Some(status);
            let entered_blocked = self.entered_blocked(&attempt, status);
            if transitioned {
                self.db.set_attempt_status(attempt.id, status)?;
                changed = true;
            }
            // Monotonic, so safe from the fast path — and a working phase
            // shorter than the sync interval would otherwise never latch.
            if status == AgentStatus::Working && !attempt.saw_working {
                self.db.set_saw_working(attempt.id)?;
            }
            if transitioned || entered_blocked {
                let Some(task) = self.db.get_task(&attempt.task_id)? else {
                    continue;
                };
                // Fresh PR lookup allowed: the run just ended, and the poll
                // may be a whole cycle behind the agent's own `gh pr create`
                // (herdr's gh#29 window — the reason "finished — committed"
                // notices used to reach dispatchers whose PR already existed).
                let settled = self.maybe_settle(runtime, &task, &attempt, status, true)?;
                if settled {
                    changed = true;
                }
                // This is the event path, so this is where a block is noticed
                // the moment the agent asks rather than up to a poll interval
                // later — which for the 02:00 case is the whole point (gh#71).
                if entered_blocked && !settled {
                    self.note_blocked(runtime, &task, &attempt, chat_id)?;
                    changed = true;
                }
            }
        }
        if self.rewatch_settled_attempts(statuses)? {
            changed = true;
        }
        if changed {
            self.rederive_all()?;
        }
        Ok(changed)
    }

    /// Recompute and persist every task's derived state, using the agent status
    /// reconciliation last stored on each live attempt.
    pub fn rederive_all(&self) -> Result<()> {
        self.rederive_with(&HashMap::new())
    }

    /// `override_status` lets a caller (and the tests) supply chat statuses
    /// directly; otherwise the value stored on the attempt is used.
    pub fn rederive_with(&self, override_status: &HashMap<String, AgentStatus>) -> Result<()> {
        for task in self.db.load_tasks()? {
            let state = derive_state(derivation_for(&task, override_status));
            if state != task.state {
                self.log
                    .info(format!("{}: {} → {}", task.identifier, task.state, state));
            }
            // A Linear row that has reached `review` is work that is finished and
            // waiting on a human — and Linear was never told. Dispatch moves the
            // issue to a started-type state and nothing moves it again until a
            // merge, so anyone reading Linear rather than the board sees
            // In Progress for the whole review window.
            //
            // Only when `[linear] review_state` names the state to move to:
            // Linear has no review *type* to resolve, so with nothing configured
            // there is no correct target and the ticket stays where it is.
            if state == BoardState::Review
                && task.source == Source::Linear
                && !task.upstream.is_final()
                && self.cfg.linear.review_state.is_some()
            {
                self.enqueue_review(&task)?;
            }
            // A GitHub row that has reached `done` while its issue is still open
            // upstream needs closing, or the next poll undoes it. `is_final`
            // rather than `!= Terminal`: an issue that is *gone* cannot be
            // closed, and asking would retry against a 404 forever.
            //
            // Per repo, not globally: closing an issue on a repo the board is
            // only reading is the write this setting exists to prevent.
            if state == BoardState::Done
                && task.source == Source::Github
                && !task.upstream.is_final()
                && split_gh_task_id(&task.id)
                    .is_some_and(|(repo, _)| self.cfg.github.writeback_for(&repo))
            {
                self.db.enqueue_writeback(&NewWriteback {
                    task_id: task.id.clone(),
                    kind: "close".into(),
                    payload: "{}".into(),
                    idem_key: format!("{}:close", task.id),
                })?;
            }
            self.db.store_derived_state(&task.id, state)?;
        }
        Ok(())
    }

    // ---- writeback ------------------------------------------------------

    /// `via` is the dispatcher already named for a reader — an issue identifier,
    /// or the chat an orchestrator is running in. `None` is the operator, and
    /// says nothing upstream.
    pub fn enqueue_dispatch(
        &self,
        task: &Task,
        runtime: &str,
        workspace: &str,
        attempt_no: usize,
        via: Option<&str>,
    ) -> Result<()> {
        // Name the parent upstream too: reading the Linear issue should tell you
        // an agent released this, not a person.
        self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "dispatch".into(),
            payload: json!({
                "runtime": runtime,
                "workspace": workspace,
                "attempt": attempt_no,
                "via": via,
            })
            .to_string(),
            idem_key: format!("{}:dispatch:{}", task.id, attempt_no),
        })?;
        Ok(())
    }

    /// Tell Linear the work is finished and waiting on a human.
    ///
    /// Keyed by attempt so a retry can move the ticket back out of review and in
    /// again: dispatch sends it to In Progress, and the attempt that follows has
    /// its own review transition to make.
    fn enqueue_review(&self, task: &Task) -> Result<()> {
        let Some(want) = self.cfg.linear.review_state.as_deref() else {
            return Ok(());
        };
        // Already there — nothing to say. Usually because we moved it on an
        // earlier tick, sometimes because the operator did it by hand; either
        // way a mutation that changes nothing is not worth sending.
        if task
            .source_state
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(want))
        {
            return Ok(());
        }
        self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "review".into(),
            payload: json!({ "state": want }).to_string(),
            idem_key: format!("{}:review:{}", task.id, task.attempt_count()),
        })?;
        Ok(())
    }

    pub fn enqueue_outcome(
        &self,
        task: &Task,
        outcome: Outcome,
        pr_url: Option<&str>,
    ) -> Result<()> {
        self.enqueue_outcome_note(task, outcome, pr_url, None)
    }

    /// As [`SyncEngine::enqueue_outcome`], with a phrase saying *why* — for the
    /// verdicts the board reaches on its own rather than reads off an artifact.
    ///
    /// The duration cap (gh#70) is the first: `failed` alone reads as a
    /// dispatch that never produced an agent, and the operator upstream has no
    /// way to tell that from a run the board stopped at its ceiling. The note
    /// rides the same comment, after the outcome.
    pub fn enqueue_outcome_note(
        &self,
        task: &Task,
        outcome: Outcome,
        pr_url: Option<&str>,
        note: Option<&str>,
    ) -> Result<()> {
        let attempt_no = task.attempt_count();
        self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "outcome".into(),
            payload: json!({
                "outcome": outcome.as_str(),
                "pr_url": pr_url,
                "note": note,
                "log": self.paths.logfile().to_string_lossy(),
                "attempt": attempt_no,
            })
            .to_string(),
            idem_key: format!("{}:outcome:{}:{}", task.id, attempt_no, outcome.as_str()),
        })?;
        Ok(())
    }

    /// Drain the queue. Failures back off exponentially, capped at 5 minutes,
    /// and drain on their own once the source returns.
    ///
    /// A writeback leaves the queue two ways: delivered to the source, or
    /// dropped because there is no longer anything upstream to deliver it to.
    /// Both are final — the difference is only what the log says.
    pub fn drain_writebacks(&self) {
        let pending = match self.db.pending_writebacks(20) {
            Ok(p) => p,
            Err(e) => {
                self.log.error(format!("reading writeback queue: {e}"));
                return;
            }
        };
        for w in pending {
            match self.deliver(&w) {
                Ok(sent) => {
                    let _ = self.db.mark_writeback_done(w.id);
                    let _ = self
                        .db
                        .meta_set(&meta::writeback_at(&w.task_id), &crate::db::now());
                    // Both leave the queue; only one of them reached a source,
                    // and a log that called the other "delivered" would be a
                    // lie an operator has no way to check.
                    match sent {
                        Sent::Upstream => self
                            .log
                            .info(format!("writeback {} delivered ({})", w.idem_key, w.kind)),
                        Sent::Dropped(why) => self.log.info(format!(
                            "writeback {} ({}) dropped: {why}",
                            w.idem_key, w.kind
                        )),
                    }
                }
                Err(e) => {
                    self.log
                        .warn(format!("writeback {} failed: {e}", w.idem_key));
                    let _ = self.db.defer_writeback(w.id, w.attempts, &e.to_string());
                }
            }
        }
    }

    fn deliver(&self, w: &crate::db::Writeback) -> Result<Sent> {
        let Some(task) = self.db.get_task(&w.task_id)? else {
            // Reaping a never-dispatched task drops its queued writebacks in the
            // same transaction, so this is only reachable if the row went some
            // other way. Either way there is nothing left to say it to.
            return Ok(Sent::Dropped(format!(
                "{} is no longer on the board",
                w.task_id
            )));
        };
        if task.upstream == UpstreamState::Gone {
            // The row is kept for its history, but the issue it points at is not
            // there any more. Retrying would only back off forever, so this
            // leaves the queue — as dropped, not as delivered.
            return Ok(Sent::Dropped(format!(
                "{} no longer exists upstream",
                task.identifier
            )));
        }
        let payload: Value = serde_json::from_str(&w.payload).unwrap_or(Value::Null);

        match task.source {
            Source::Linear => {
                let Some(linear) = &self.linear else {
                    anyhow::bail!("no Linear credentials; writeback stays queued");
                };
                match w.kind.as_str() {
                    "dispatch" => {
                        // Move the issue into the team's started-type state.
                        let team = task.identifier.split('-').next().unwrap_or_default();
                        if let Ok(Some(state_id)) = linear.started_state_id(team) {
                            linear.set_state(&task.source_id, &state_id)?;
                        } else {
                            self.log.warn(format!(
                                "no started-type state for team {team}; commenting only"
                            ));
                        }
                        let via = match payload["via"].as_str() {
                            Some(v) => format!(" · dispatched by {v}"),
                            None => String::new(),
                        };
                        linear.comment(
                            &task.source_id,
                            &format!(
                                "Dispatched to comet · {} · space:{} · attempt {}{}",
                                payload["runtime"].as_str().unwrap_or("?"),
                                payload["workspace"].as_str().unwrap_or("?"),
                                payload["attempt"].as_u64().unwrap_or(1),
                                via,
                            ),
                        )?;
                    }
                    "outcome" => {
                        let outcome = payload["outcome"].as_str().unwrap_or("done");
                        let pr = payload["pr_url"].as_str();
                        match (outcome, pr) {
                            ("done", Some(url)) => {
                                linear.attach_link(&task.source_id, url, "Pull request")?;
                                linear.comment(
                                    &task.source_id,
                                    &format!("comet-board: attempt finished · {url}"),
                                )?;
                            }
                            ("done", None) => {
                                linear.comment(
                                    &task.source_id,
                                    "comet-board: attempt finished with no pull request",
                                )?;
                            }
                            (other, _) => {
                                linear.comment(
                                    &task.source_id,
                                    &format!(
                                        "comet-board: attempt {} · {}{} · log: {}",
                                        payload["attempt"].as_u64().unwrap_or(1),
                                        other,
                                        outcome_note(&payload),
                                        payload["log"].as_str().unwrap_or("(none)"),
                                    ),
                                )?;
                            }
                        }
                    }
                    // The agent stopped and cannot go on by itself (gh#71).
                    // Not a state transition: the issue is still In Progress
                    // and that is true — it is in progress and stuck, which is
                    // a thing to say rather than a state to move to.
                    "blocked" => {
                        linear.comment(&task.source_id, &blocked_comment(&payload))?;
                    }
                    // The attempt settled with work waiting on a human. Dispatch
                    // moved this issue to In Progress and, without this, nothing
                    // moved it again until a merge — so Linear read In Progress
                    // for the whole review window.
                    "review" => {
                        // Config decides, at delivery: turning the setting off
                        // must stop a transition still sitting in the queue.
                        let Some(want) = self.cfg.linear.review_state.as_deref() else {
                            return Ok(Sent::Dropped(format!(
                                "no [linear] review_state configured ({})",
                                task.identifier
                            )));
                        };
                        let team = task.identifier.split('-').next().unwrap_or_default();
                        match linear.state_id_named(team, want)? {
                            Ok(state_id) => linear.set_state(&task.source_id, &state_id)?,
                            // A named state that does not exist is a config
                            // mistake, not an outage: retrying it against Linear
                            // forever would only bury the reason. `doctor`
                            // checks this name for exactly this reason.
                            Err(have) => {
                                return Ok(Sent::Dropped(format!(
                                    "team {team} has no state named `{want}` (has: {})",
                                    have.join(", ")
                                )));
                            }
                        }
                    }
                    // Merging its pull request finished the work; the ticket is
                    // what is left.
                    "close" => {
                        let team = task.identifier.split('-').next().unwrap_or_default();
                        match linear.completed_state_id(team) {
                            Ok(Some(state_id)) => {
                                linear.set_state(&task.source_id, &state_id)?;
                                linear
                                    .comment(&task.source_id, "comet-board: pull request merged")?;
                            }
                            _ => self.log.warn(format!(
                                "no completed-type state for team {team}; leaving it open"
                            )),
                        }
                    }
                    other => self.log.warn(format!("unknown writeback kind {other}")),
                }
            }
            Source::Github => {
                let Some(gh) = &self.github else {
                    anyhow::bail!("no GitHub client; writeback stays queued");
                };
                let Some((repo, number)) = split_gh_task_id(&task.id) else {
                    self.log
                        .warn(format!("cannot parse a repo out of {}", task.id));
                    return Ok(Sent::Dropped(format!("no repo in {}", task.id)));
                };
                // Config decides at delivery, and it decides per repo: the
                // queue holds effects aimed at several repos at once, and the
                // repo is what says whether this one may land. Turning it off
                // for a repo must also stop what is already queued for it.
                if !self.cfg.github.writeback_for(&repo) {
                    return Ok(Sent::Dropped(format!(
                        "writeback is off for {repo} in routing.toml ({})",
                        task.identifier
                    )));
                }
                match w.kind.as_str() {
                    "dispatch" => {
                        let via = match payload["via"].as_str() {
                            Some(v) => format!(" · dispatched by {v}"),
                            None => String::new(),
                        };
                        gh.comment(
                            &repo,
                            number,
                            &format!(
                                "Dispatched to comet · {} · space:{} · attempt {}{}",
                                payload["runtime"].as_str().unwrap_or("?"),
                                payload["workspace"].as_str().unwrap_or("?"),
                                payload["attempt"].as_u64().unwrap_or(1),
                                via,
                            ),
                        )?;
                    }
                    "outcome" => {
                        let outcome = payload["outcome"].as_str().unwrap_or("done");
                        let body = match payload["pr_url"].as_str() {
                            Some(url) => {
                                format!("comet-board: attempt finished · {url}")
                            }
                            None => format!(
                                "comet-board: attempt {} · {}{} · log: {}",
                                payload["attempt"].as_u64().unwrap_or(1),
                                outcome,
                                outcome_note(&payload),
                                payload["log"].as_str().unwrap_or("(none)"),
                            ),
                        };
                        gh.comment(&repo, number, &body)?;
                    }
                    "blocked" => {
                        gh.comment(&repo, number, &blocked_comment(&payload))?;
                    }
                    // Close on done. This is what makes "mark done" mean the
                    // same thing on a GitHub row as on a Linear one.
                    "close" => gh.close_issue(&repo, number)?,
                    other => self.log.warn(format!("unknown writeback kind {other}")),
                }
            }
        }
        Ok(Sent::Upstream)
    }

    /// Merge the pull request on a task, if it has one.
    pub fn merge_pull_request(&self, task: &Task) -> Result<String> {
        let Some(number) = task.pr_number else {
            anyhow::bail!("{} has no pull request", task.identifier);
        };
        let Some(gh) = &self.github else {
            anyhow::bail!("no GitHub credentials");
        };
        // The repo comes from the task id for a PR row, or from the PR url for
        // a task whose PR was linked by branch.
        let repo = split_gh_task_id(&task.id)
            .map(|(r, _)| r)
            .or_else(|| {
                let url = task.pr_url.as_deref()?;
                let rest = url.split("github.com/").nth(1)?;
                let mut parts = rest.split('/');
                Some(format!("{}/{}", parts.next()?, parts.next()?))
            })
            .ok_or_else(|| {
                anyhow::anyhow!("cannot tell which repo {} belongs to", task.identifier)
            })?;

        gh.merge_pr(&repo, number)?;
        self.log
            .info(format!("merged {repo}#{number} for {}", task.identifier));

        // Reflect it immediately rather than waiting for a poll: the operator
        // just pressed the key and needs the row to move.
        self.db
            .set_pr(&task.id, task.pr_url.as_deref(), Some(number), false)?;
        self.db.set_pr_merged(&task.id, true)?;
        self.finish_on_merge(task, &repo, number)?;
        self.rederive_all()?;
        Ok(format!("{repo}#{number}"))
    }

    /// What a merged pull request means for the task that owns it.
    ///
    /// A merged PR is finished work. For a PR row that is the whole task; for
    /// an issue whose PR this was, the work is done and the ticket is what
    /// remains. Shared by the merge command and by the poll that notices
    /// someone merged elsewhere, because otherwise the row's state depends on
    /// which route the merge took.
    fn finish_on_merge(&self, task: &Task, repo: &str, number: i64) -> Result<()> {
        self.db.set_local_done(&task.id, true)?;
        let queued = self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "close".into(),
            payload: json!({ "reason": "merged" }).to_string(),
            idem_key: format!("{}:close", task.id),
        })?;
        if queued {
            self.log.info(format!(
                "{}: {repo}#{number} merged; queued a close",
                task.identifier
            ));
        }
        Ok(())
    }

    // ---- health for the header -----------------------------------------

    pub fn health(&self, source: Source) -> SourceHealth {
        let (configured, status_key, fail_key) = match source {
            Source::Linear => (
                self.linear.is_some(),
                meta::LINEAR_STATUS,
                meta::LINEAR_FAILURES,
            ),
            Source::Github => (
                self.github.is_some(),
                meta::GITHUB_STATUS,
                meta::GITHUB_FAILURES,
            ),
        };
        // A recorded status means *something* polled this source, even if this
        // process built no client for it — a reader's own credentials say
        // nothing about whether the engine's loop has any. Gating on
        // `configured` alone made the board omit `linear ✓` while the loop
        // was happily polling Linear.
        match self.db.meta_get(status_key).ok().flatten() {
            None if !configured => SourceHealth::Absent,
            Some(s) if s == "ok" => SourceHealth::Ok,
            Some(s) if s.starts_with("error:") => {
                let failures = self
                    .db
                    .meta_get(fail_key)
                    .ok()
                    .flatten()
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(1);
                SourceHealth::Down {
                    error: s.trim_start_matches("error:").to_string(),
                    retry_in: crate::db::backoff_secs(failures),
                }
            }
            _ if configured => SourceHealth::Ok,
            _ => SourceHealth::Absent,
        }
    }
}

/// What `derive_state` gets to see for a task, read off its stored rows.
///
/// Pulled out of the sync cycle so anything that needs the current state without
/// writing it asks the same question the loop does, rather than a second
/// approximation of it. `override_status` supplies statuses by chat id; empty
/// means "use what reconciliation last stored on the attempt".
pub fn derivation_for(task: &Task, override_status: &HashMap<String, AgentStatus>) -> Derivation {
    let live = task.live_attempt().map(|a| {
        a.pane_id
            .as_deref()
            .and_then(|p| override_status.get(p).copied())
            .or(a.agent_status)
            .unwrap_or(AgentStatus::Unknown)
    });
    let last_outcome = task.last_closed_attempt().and_then(|a| a.outcome);
    Derivation {
        upstream: task.upstream,
        live,
        last_outcome,
        open_pr: task.pr_open,
        local_done: task.local_done,
    }
}

/// The comment a queued `blocked` writeback delivers (gh#71).
///
/// Composed at delivery rather than at enqueue so the wording is one thing in
/// one place for both trackers, exactly as the outcome comment is — but the
/// *facts* come out of the payload, because they were true when the block
/// happened and the task row has moved on since.
fn blocked_comment(payload: &Value) -> String {
    notify::upstream_comment(
        payload["attempt"].as_u64().unwrap_or(1),
        Stopped::parse(payload["reason"].as_str().unwrap_or("")),
        payload["log"].as_str().unwrap_or("(none)"),
    )
}

/// The branches the board has dispatched onto, and where.
///
/// A branch name alone is not an identity: the board watches several repos and
/// `board/gh-2` can exist in all of them. A GitHub task's branch is claimed only
/// within its own repository; a Linear task names no repo, so its branch is
/// claimed wherever the pull request appears.
#[derive(Default)]
struct AttemptBranches {
    in_repo: std::collections::HashMap<String, std::collections::HashSet<String>>,
    anywhere: std::collections::HashSet<String>,
}

impl AttemptBranches {
    /// Is this pull request already some attempt's, rather than a row of its
    /// own?
    fn claims(&self, pr: &PullRequest) -> bool {
        self.anywhere.contains(&pr.head_ref)
            || self
                .in_repo
                .get(&pr.repo)
                .is_some_and(|branches| branches.contains(&pr.head_ref))
    }
}

/// `gh:owner/repo#87` → (`owner/repo`, 87). Also accepts the pull-request form
/// `gh:owner/repo!508` — GitHub's issues endpoints serve pull requests too, so
/// comments and closing work the same for both.
pub fn split_gh_task_id(id: &str) -> Option<(String, i64)> {
    let repo = crate::model::gh_repo(id)?;
    let (_, number) = id.rsplit_once(['#', '!'])?;
    Some((repo.to_string(), number.parse().ok()?))
}

/// The route context for a task, from its stored fields.
pub fn route_context(task: &Task) -> RouteContext {
    match task.source {
        Source::Linear => RouteContext {
            // Prefer what Linear told us; fall back to the identifier prefix
            // (`LIN-142` → team key `LIN`) for rows stored before the team was
            // recorded.
            linear_team: task
                .linear_team
                .clone()
                .or_else(|| task.identifier.split('-').next().map(str::to_string)),
            linear_project: task.linear_project.clone(),
            gh_repo: None,
            labels: task.labels.clone(),
        },
        Source::Github => RouteContext {
            linear_team: None,
            linear_project: None,
            gh_repo: crate::model::gh_repo(&task.id).map(str::to_string),
            labels: task.labels.clone(),
        },
    }
}

/// The ` · why` clause an outcome comment carries when the board reached the
/// verdict itself — empty for the outcomes an artifact decided (gh#70).
fn outcome_note(payload: &Value) -> String {
    match payload["note"].as_str().filter(|n| !n.is_empty()) {
        Some(note) => format!(" · {note}"),
        None => String::new(),
    }
}

/// Seconds from an RFC-3339 stamp to `now`, or `None` if it will not parse.
/// Negative when the stamp is in the future — a clock that moved, which the
/// duration cap reads as "no time has passed" rather than as a breach.
/// One git command in a checkout: its trimmed stdout, or `None` if it failed.
///
/// Failure and empty output are deliberately different answers — `for-each-ref`
/// succeeding with nothing is "no such ref", and a git that could not run at
/// all is "no idea", and the push check must not read the second as the first.
fn git_out(worktree: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A git command run for its exit status alone (`merge-base --is-ancestor`).
fn git_ok(worktree: &str, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn secs_since(stamp: &str, now: chrono::DateTime<chrono::Utc>) -> Option<i64> {
    let t = chrono::DateTime::parse_from_rfc3339(stamp).ok()?;
    Some((now - t.with_timezone(&chrono::Utc)).num_seconds())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Defaults;
    use crate::db::UpsertTask;
    use crate::sources::github::FixtureRest;
    use crate::sources::linear::FixtureTransport;

    fn engine(linear: Option<Linear<Box<dyn GraphQl>>>) -> SyncEngine {
        engine_with(linear, None)
    }

    fn engine_with(
        linear: Option<Linear<Box<dyn GraphQl>>>,
        github: Option<Github<Box<dyn Rest>>>,
    ) -> SyncEngine {
        let mut e = engine_inner(linear);
        e.github = github;
        e
    }

    fn engine_inner(linear: Option<Linear<Box<dyn GraphQl>>>) -> SyncEngine {
        let tmp = std::env::temp_dir().join(format!(
            "comet-board-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        SyncEngine {
            db: Db::open_in_memory().unwrap(),
            cfg: RoutingConfig {
                defaults: Defaults::default(),
                ..Default::default()
            },
            credentials: Default::default(),
            paths: Paths {
                config_dir: tmp.clone(),
                state_dir: tmp,
            },
            log: Arc::new(Logger::new("", false)),
            linear,
            github: None,
            webhook: Arc::new(RecordingWebhook::default()),
        }
    }

    /// The webhook the tests watch instead of a listening socket. Shared by
    /// `Arc`, so a test can read back exactly what the board would have POSTed.
    #[derive(Default)]
    struct RecordingWebhook {
        posts: std::sync::Mutex<Vec<(String, Value)>>,
        /// Answer every POST with a failure, for the tests that check a dead
        /// endpoint changes nothing about the board.
        fail: bool,
    }

    impl Webhook for RecordingWebhook {
        fn post(&self, url: &str, body: &Value) -> anyhow::Result<()> {
            self.posts
                .lock()
                .unwrap()
                .push((url.to_string(), body.clone()));
            if self.fail {
                anyhow::bail!("the endpoint is down");
            }
            Ok(())
        }
    }

    /// An engine wired to a webhook the test can read back.
    fn engine_with_webhook(url: &str, fail: bool) -> (SyncEngine, Arc<RecordingWebhook>) {
        let hook = Arc::new(RecordingWebhook {
            fail,
            ..Default::default()
        });
        let mut e = engine_inner(None);
        e.cfg.defaults.notify_webhook = Some(url.to_string());
        e.webhook = hook.clone();
        (e, hook)
    }

    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn seed(e: &SyncEngine, id: &str, identifier: &str, upstream: UpstreamState) {
        e.db.upsert_task(&UpsertTask {
            id: id.into(),
            source: Source::Linear,
            source_id: "uuid-1".into(),
            identifier: identifier.into(),
            title: "Add retry".into(),
            body: None,
            url: "https://linear.app/x".into(),
            labels: vec!["herd".into()],
            source_state: None,
            linear_team: identifier.split('-').next().map(str::to_string),
            linear_project: None,
            upstream,
            updated_at: crate::db::now(),
        })
        .unwrap();
    }

    /// This cycle's session statuses, as the engine's board service builds
    /// them: chat id → status already mapped through `runtime::agent_status`.
    fn statuses(pairs: &[(&str, AgentStatus)]) -> SessionStatuses {
        pairs.iter().map(|(id, s)| (id.to_string(), *s)).collect()
    }

    fn dispatch(e: &SyncEngine, task: &str, chat_id: &str) -> i64 {
        let a =
            e.db.insert_attempt(&crate::db::NewAttempt {
                task_id: task.into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: Some("board/lin-142".into()),
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: None,
                account: None,
                repo_path: None,
                dispatched_by_device: None,
                dispatched_by_user: None,
            })
            .unwrap();
        e.db.set_attempt_pane(a, chat_id).unwrap();
        a
    }

    /// An attempt on a named branch, for the tests that care which one.
    fn dispatch_on(e: &SyncEngine, task: &str, branch: &str) -> i64 {
        e.db.insert_attempt(&crate::db::NewAttempt {
            task_id: task.into(),
            pane_id: None,
            workspace: "offhand".into(),
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
        })
        .unwrap()
    }

    fn live(e: &SyncEngine) -> Attempt {
        e.db.attempts_for("linear:LIN-142").unwrap().remove(0)
    }

    // ---- session reconciliation -----------------------------------------

    #[test]
    fn a_missing_chat_needs_two_ticks_before_it_orphans() {
        // Avoid flapping on a transient snapshot: one absent tick proves
        // nothing, exactly as one missing pane proved nothing in herdr-board.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        // The agent got going — absence after activity is what orphans.
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();

        e.reconcile_sessions(&statuses(&[])).unwrap();
        assert!(
            live(&e).outcome.is_none(),
            "one missing tick must not orphan the attempt"
        );

        e.reconcile_sessions(&statuses(&[])).unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Orphaned));
    }

    #[test]
    fn a_chat_that_comes_back_resets_the_missing_counter() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();

        e.reconcile_sessions(&statuses(&[])).unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        assert_eq!(live(&e).missing_ticks, 0);

        // Having survived one absence, it takes two fresh ticks again.
        e.reconcile_sessions(&statuses(&[])).unwrap();
        assert!(live(&e).outcome.is_none());
    }

    #[test]
    fn a_chat_that_never_started_is_not_orphaned_on_absence() {
        // A dispatch whose brief sits in the command ledger has no session row
        // yet — indistinguishable, from here, from a chat that is gone. The
        // verdict needs `Runtime::chat_alive` (H2); until then absence of
        // evidence must not end an attempt that may not have begun.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        for _ in 0..5 {
            e.reconcile_sessions(&statuses(&[])).unwrap();
        }
        let a = live(&e);
        assert!(
            a.outcome.is_none(),
            "never orphaned without evidence of life"
        );
        assert_eq!(a.missing_ticks, 5, "but the absence is on the record");
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
    }

    #[test]
    fn orphaning_queues_exactly_one_writeback_even_across_ticks() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        for _ in 0..5 {
            e.reconcile_sessions(&statuses(&[])).unwrap();
        }
        assert_eq!(e.db.pending_writeback_count().unwrap(), 1);
    }

    #[test]
    fn an_idle_status_leaves_the_attempt_live() {
        // "only finalize on explicit done detection or user action". The run
        // ended, and the checkout was checked (§H4) — but this attempt has no
        // worktree, no commits and no PR, and between turns is not finished.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        let a = live(&e);
        assert!(a.outcome.is_none());
        assert_eq!(a.agent_status, Some(AgentStatus::Idle));
    }

    #[test]
    fn reconcile_latches_saw_working_and_persists_statuses() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        let a = live(&e);
        assert!(a.saw_working);
        assert_eq!(a.agent_status, Some(AgentStatus::Working));

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Blocked)]))
            .unwrap();
        let a = live(&e);
        assert!(a.saw_working, "the latch does not unlatch");
        assert_eq!(a.agent_status, Some(AgentStatus::Blocked));
    }

    // ---- the status-only fast path --------------------------------------

    #[test]
    fn a_watch_event_picks_up_a_status_change_without_touching_lifecycle() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        assert!(
            e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Blocked)]))
                .unwrap()
        );
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Blocked
        );
        // ...and unblocking is picked up the same way.
        assert!(
            e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Working)]))
                .unwrap()
        );
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Working
        );
        // An unchanged status is not a write.
        assert!(
            !e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Working)]))
                .unwrap()
        );
    }

    #[test]
    fn a_watch_event_never_orphans_a_missing_chat() {
        // Orphaning is the interval reconcile's call; watch events can arrive
        // in bursts, and counting ticks in event time would orphan a chat
        // mid-sync-hiccup.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        for _ in 0..10 {
            e.refresh_statuses(&statuses(&[])).unwrap();
        }
        let a = live(&e);
        assert_eq!(a.missing_ticks, 0);
        assert!(a.outcome.is_none());
    }

    #[test]
    fn the_fast_path_latches_saw_working_too() {
        // A working phase shorter than the sync interval must still latch, or
        // the interval reconcile later reads "never started" off an attempt
        // that ran and finished between two of its ticks.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        assert!(live(&e).saw_working);
    }

    #[test]
    fn rederive_persists_the_matrix_result() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let mut status = HashMap::new();
        status.insert("chat-9".to_string(), AgentStatus::Blocked);
        e.rederive_with(&status).unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Blocked
        );
    }

    // ---- settling on run events (§H4) ------------------------------------

    use crate::runtime::{DispatchHandle, DispatchSpec};

    /// A runtime that answers the journal question and records the prompts the
    /// settle path sends — the two things reachable from settling (gh#71).
    #[derive(Default)]
    struct JournalFact(Option<RunEnd>, std::sync::Mutex<Vec<(String, String)>>);

    impl JournalFact {
        fn ending(end: Option<RunEnd>) -> JournalFact {
            JournalFact(end, Default::default())
        }
        /// (chat id, text) for every prompt the board queued.
        fn prompts(&self) -> Vec<(String, String)> {
            self.1.lock().unwrap().clone()
        }
    }

    impl Runtime for JournalFact {
        fn dispatch(&self, _: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
            unreachable!("settling never dispatches")
        }
        fn prompt(&self, chat: &str, text: &str) -> anyhow::Result<()> {
            self.1
                .lock()
                .unwrap()
                .push((chat.to_string(), text.to_string()));
            Ok(())
        }
        fn cancel(&self, _: &str) -> anyhow::Result<()> {
            unreachable!("settling never cancels")
        }
        fn session(&self, _: &str) -> anyhow::Result<Option<comet_proto::Session>> {
            Ok(None)
        }
        fn chat_alive(&self, _: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
        fn chat_cwd(&self, _: &str) -> anyhow::Result<Option<String>> {
            unreachable!("settling never reads the chat cwd")
        }
        fn last_run_end(&self, _: &str) -> anyhow::Result<Option<RunEnd>> {
            Ok(self.0)
        }
    }

    fn git_in(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// What the agent left in the checkout when its run ended.
    #[derive(Clone, Copy, PartialEq)]
    enum Work {
        /// Nothing past the attempt's base.
        None,
        /// A commit, and nowhere but this worktree — gh#69's stranded attempt.
        Committed,
        /// A commit, and on origin: the artifact a settle may call reviewable.
        Pushed,
    }

    /// Give an attempt a real checkout: base recorded at dispatch, and — when
    /// asked — a commit the agent made after it, pushed or not. Built on the
    /// AGE-19 fixture so the operator's own unpushed commit is always present
    /// underneath, proving every settle here measures from the attempt's base.
    fn agent_worked_in(e: &SyncEngine, attempt: i64, did: Work) -> std::path::PathBuf {
        let work = repo_ahead_of_its_remote();
        let head = std::process::Command::new("git")
            .args(["-C", &work.to_string_lossy(), "rev-parse", "HEAD"])
            .output()
            .unwrap();
        let base = String::from_utf8_lossy(&head.stdout).trim().to_string();
        if did != Work::None {
            std::fs::write(work.join("agent"), "work").unwrap();
            git_in(&work, &["add", "."]);
            git_in(&work, &["commit", "-m", "the agent's work"]);
        }
        if did == Work::Pushed {
            // An ordinary push, which is what leaves the remote-tracking ref
            // the settle's first tier reads.
            git_in(&work, &["push", "origin", "main"]);
        }
        e.db.set_attempt_worktree(attempt, &work.to_string_lossy())
            .unwrap();
        e.db.set_attempt_base_sha(attempt, &base).unwrap();
        work
    }

    fn outcome_payload(e: &SyncEngine) -> Value {
        let w =
            e.db.pending_writebacks(20)
                .unwrap()
                .into_iter()
                .find(|w| w.kind == "outcome")
                .expect("an outcome writeback");
        serde_json::from_str(&w.payload).unwrap()
    }

    #[test]
    fn a_pull_request_settles_the_attempt_the_moment_the_run_ends() {
        // The whole §H4 headline: no clock, no second sample. The run ended,
        // a PR is open — the agent said it is finished, so it is.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr(
            "linear:LIN-142",
            Some("https://github.com/o/r/pull/18"),
            Some(18),
            true,
        )
        .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        assert!(live(&e).outcome.is_none(), "a live run settles nothing");

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        // The writeback names the PR — the trail a dispatcher acts on.
        assert_eq!(
            outcome_payload(&e)["pr_url"].as_str(),
            Some("https://github.com/o/r/pull/18")
        );
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Review
        );
    }

    #[test]
    fn pushed_commits_settle_a_run_that_ended_cleanly() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = agent_worked_in(&e, a, Work::Pushed);

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        // Commits are the evidence, and no PR is claimed — the payload says
        // null, which delivery renders as the log-pointer comment.
        assert!(outcome_payload(&e)["pr_url"].is_null());
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    /// gh#69, end to end: the agent committed, `gh pr create` never happened
    /// (no credential on the box — gh#68's whole premise), and the run ended
    /// `Completed`. The old rule read that as a finished attempt and moved the
    /// row to `review` while the work sat in a worktree nobody else can reach.
    #[test]
    fn a_run_that_ends_with_unpushed_commits_does_not_read_as_review() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = agent_worked_in(&e, a, Work::Committed);

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        assert!(
            live(&e).outcome.is_none(),
            "commits only the agent's box can see are not a reviewable attempt"
        );
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Working
        );
        // And the reason is on the record rather than left to be guessed at
        // from a row that looks like an agent still typing.
        assert!(
            e.db.meta_get(&meta::unpushed_noted(a)).unwrap().is_some(),
            "the stranded branch is noted once"
        );

        // The agent (or the operator) pushes; the next run end settles it.
        git_in(&work, &["push", "origin", "main"]);
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    /// The crash variant. Recovery stamps an aborted run `Interrupted`, so the
    /// errored-runs-never-settle-on-commits guard does not apply to it, and an
    /// agent killed mid-task used to settle as finished on a commit it had
    /// made along the way.
    #[test]
    fn a_recovery_aborted_run_with_unpushed_commits_stays_live() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = agent_worked_in(&e, a, Work::Committed);
        let rt = JournalFact::ending(Some(RunEnd::Interrupted));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();
        assert!(live(&e).outcome.is_none());
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    /// A push straight to a URL — what an agent carrying the board's
    /// credential does — updates no remote-tracking ref, so the local tier
    /// proves nothing and the row would sit `working` on work that is in fact
    /// on origin. The event path asks GitHub, once, and settles.
    #[test]
    fn a_branch_pushed_without_a_tracking_ref_is_found_by_asking_github() {
        let work_dir = repo_ahead_of_its_remote();
        let wt = work_dir.to_string_lossy().into_owned();
        let base = git_out(&wt, &["rev-parse", "HEAD"]).unwrap();
        std::fs::write(work_dir.join("agent"), "work").unwrap();
        git_in(&work_dir, &["add", "."]);
        git_in(&work_dir, &["commit", "-m", "the agent's work"]);
        let head = git_out(&wt, &["rev-parse", "HEAD"]).unwrap();
        // Pushed by URL: the remote has it, this checkout has no record of it.
        let remote = work_dir.parent().unwrap().join("remote.git");
        git_in(
            &work_dir,
            &[
                "push",
                &remote.to_string_lossy(),
                "HEAD:refs/heads/board/lin-142",
            ],
        );
        assert!(
            git_out(&wt, &["for-each-ref", "--contains", &head, "refs/remotes/"])
                .is_some_and(|r| r.is_empty()),
            "the fixture must leave no tracking ref, or it proves nothing"
        );

        let gh = Github::new(Box::new(FixtureRest::new(vec![(
            "/repos/o/r/branches/board/lin-142".to_string(),
            json!({ "commit": { "sha": head } }),
        )])) as Box<dyn Rest>);
        let e = engine_with(None, Some(gh));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_attempt_worktree(a, &wt).unwrap();
        e.db.set_attempt_base_sha(a, &base).unwrap();
        // The Linear task names no repo; the checkout's own remote answers.
        git_in(
            &work_dir,
            &["remote", "set-url", "origin", "https://github.com/o/r.git"],
        );

        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        let attempt = live(&e);
        assert_eq!(
            e.attempt_commits(&task, &attempt, false),
            Commits::Unpushed,
            "the interval path stays local: no API call per cycle per attempt"
        );
        assert_eq!(
            e.attempt_commits(&task, &attempt, true),
            Commits::Pushed,
            "the event path asks GitHub and finds the branch"
        );
        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    /// The bar is containment, not a name: a retry reuses its predecessor's
    /// branch, and the push that branch already carries is not this attempt's.
    #[test]
    fn a_retrys_own_commits_are_what_must_be_on_origin() {
        let work_dir = repo_ahead_of_its_remote();
        let wt = work_dir.to_string_lossy().into_owned();
        // The previous attempt's work, pushed under the shared branch name.
        std::fs::write(work_dir.join("first"), "1").unwrap();
        git_in(&work_dir, &["add", "."]);
        git_in(&work_dir, &["commit", "-m", "the cancelled run's work"]);
        let pushed = git_out(&wt, &["rev-parse", "HEAD"]).unwrap();
        let remote = work_dir.parent().unwrap().join("remote.git");
        git_in(
            &work_dir,
            &[
                "push",
                &remote.to_string_lossy(),
                "HEAD:refs/heads/board/lin-142",
            ],
        );
        // The retry, committing on top and pushing nothing.
        std::fs::write(work_dir.join("second"), "2").unwrap();
        git_in(&work_dir, &["add", "."]);
        git_in(&work_dir, &["commit", "-m", "the retry's work"]);

        let gh = Github::new(Box::new(FixtureRest::new(vec![(
            "/repos/o/r/branches/board/lin-142".to_string(),
            json!({ "commit": { "sha": pushed } }),
        )])) as Box<dyn Rest>);
        let e = engine_with(None, Some(gh));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_attempt_worktree(a, &wt).unwrap();
        e.db.set_attempt_base_sha(a, &pushed).unwrap();
        git_in(
            &work_dir,
            &["remote", "set-url", "origin", "https://github.com/o/r.git"],
        );

        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(
            e.attempt_commits(&task, &live(&e), true),
            Commits::Unpushed,
            "a branch on origin at somebody else's commit settles nothing"
        );
        std::fs::remove_dir_all(work_dir.parent().unwrap()).ok();
    }

    #[test]
    fn a_run_that_ends_with_nothing_new_committed_stays_live() {
        // The checkout exists and holds the operator's own unpushed commit —
        // the AGE-19 trap. Measured from the attempt's base there is nothing,
        // and nothing is what must be found.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = agent_worked_in(&e, a, Work::None);

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        assert!(live(&e).outcome.is_none());
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn an_errored_run_keeps_the_attempt_for_the_retry() {
        // §H4's `reopened` contract, first half: an `Errored`→retried run is
        // the same attempt, not a new one. The errored end never closes the
        // row — even over commits — so the retry lands on it, and the clean
        // end that follows settles it with nothing reopened and nothing
        // double-counted.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = agent_worked_in(&e, a, Work::Pushed);
        let rt = JournalFact::ending(Some(RunEnd::Errored));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        // The run dies: the engine maps Errored to blocked, the journal holds
        // the errored Done.
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();
        assert!(
            live(&e).outcome.is_none(),
            "an errored run must not settle on its commits"
        );

        // The operator retries in the same chat; a run is live again.
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(task.attempts.len(), 1, "the retry is the same attempt");

        // And this time it ends cleanly.
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();
        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(task.attempts.len(), 1);
        assert_eq!(task.attempts[0].outcome, Some(Outcome::Done));
        assert_eq!(task.attempts[0].reopened, 0, "a retry is not a reopen");
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn an_errored_run_with_a_pull_request_still_finishes() {
        // A harness that crashes moments after `gh pr create` still finished
        // the work: the PR is the agent's own statement, whatever the exit
        // said.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(Some(RunEnd::Errored));
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
    }

    #[test]
    fn a_question_mid_run_settles_nothing() {
        // `Blocked` hides two facts, and this is the other one: the agent
        // asked something, the run is alive, the journal's last word is not a
        // `Done`. Even an open PR must not settle it — the question may be
        // about that very PR.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();
        assert!(live(&e).outcome.is_none());
    }

    #[test]
    fn without_a_journal_a_blocked_status_decides_nothing() {
        // A caller with no runtime cannot tell an errored end from a pending
        // question, so it must act on neither.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Blocked)]))
            .unwrap();
        assert!(live(&e).outcome.is_none());
    }

    #[test]
    fn a_crashed_engine_reading_unknown_is_not_completion() {
        // Staleness is absence of evidence. Settling on it would close
        // attempts every time an engine wedges with commits on the branch.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = agent_worked_in(&e, a, Work::Pushed);
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Unknown)]))
            .unwrap();
        assert!(live(&e).outcome.is_none());
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    // ---- notification (gh#71) --------------------------------------------

    /// An attempt released by an agent in `chat-parent`, as `--via` records it.
    fn dispatch_via(e: &SyncEngine, task: &str, chat_id: &str, parent: &str) -> i64 {
        let a =
            e.db.insert_attempt(&crate::db::NewAttempt {
                task_id: task.into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: Some("board/lin-142".into()),
                dispatched_by: None,
                dispatched_by_pane: Some(parent.into()),
                base_sha: None,
                account: None,
                repo_path: None,
            })
            .unwrap();
        e.db.set_attempt_pane(a, chat_id).unwrap();
        a
    }

    fn blocked_writebacks(e: &SyncEngine) -> Vec<Value> {
        e.db.pending_writebacks(50)
            .unwrap()
            .into_iter()
            .filter(|w| w.kind == "blocked")
            .map(|w| serde_json::from_str(&w.payload).unwrap())
            .collect()
    }

    /// The exit criterion in one test: an agent that stops to ask produces one
    /// comment, not one per reconcile tick. Before gh#71 it produced none at
    /// all — a blocked attempt settles nothing, so no outcome writeback fires
    /// and the row colour was the entire signal.
    #[test]
    fn a_blocked_agent_comments_upstream_exactly_once() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        for _ in 0..5 {
            e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
                .unwrap();
        }

        let queued = blocked_writebacks(&e);
        assert_eq!(queued.len(), 1, "one block, one comment: {queued:?}");
        assert_eq!(queued[0]["reason"], "asking");
        assert_eq!(queued[0]["block"], 1);
        assert!(live(&e).outcome.is_none(), "a block ends nothing");
    }

    /// Once per *block*, which is not the same as once per attempt: a question
    /// answered at 09:00 and a second one at 11:00 are two things a human has
    /// to hear about.
    #[test]
    fn blocking_again_after_being_answered_is_a_second_comment() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(None);

        for status in [
            AgentStatus::Blocked,
            AgentStatus::Working,
            AgentStatus::Blocked,
        ] {
            e.reconcile_sessions_with(&statuses(&[("chat-9", status)]), Some(&rt))
                .unwrap();
        }

        let queued = blocked_writebacks(&e);
        assert_eq!(queued.len(), 2, "{queued:?}");
        assert_eq!(queued[1]["block"], 2);
        assert_eq!(live(&e).blocked_count, 2);
    }

    /// The two ways of blocking are not the same news: one needs an answer,
    /// the other needs a retry-or-cancel. Only the run journal splits them.
    #[test]
    fn an_errored_run_says_so_in_its_blocked_comment() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(Some(RunEnd::Errored));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();

        let queued = blocked_writebacks(&e);
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0]["reason"], "errored");
        let body = blocked_comment(&queued[0]);
        assert!(body.contains("stopped with an error"), "{body}");
        assert!(body.starts_with("comet-board:"), "{body}");
    }

    /// The board must not say two contradictory things about one event. An
    /// errored run whose pull request is already open *settles* — the work is
    /// reviewable — so it gets the outcome comment and no blocked comment.
    #[test]
    fn a_block_that_settles_in_the_same_pass_comments_only_once() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(Some(RunEnd::Errored));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();

        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        assert!(
            blocked_writebacks(&e).is_empty(),
            "a settled attempt is not also announced as blocked"
        );
    }

    /// The event path is where a block is noticed the moment it happens
    /// rather than up to a poll interval later — which for the 02:00 case is
    /// the whole point.
    #[test]
    fn the_event_path_notices_a_block_immediately() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(None);

        e.refresh_statuses_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();
        assert_eq!(blocked_writebacks(&e).len(), 1);

        // And the interval pass behind it does not say it again.
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();
        assert_eq!(blocked_writebacks(&e).len(), 1);
    }

    /// herdr-board's AGE-25, ported: the agent that released the work is
    /// prompted in its own chat when that work settles. An orchestrator only
    /// gets a turn when something prompts it, so this is the only way it can
    /// hear.
    #[test]
    fn a_settle_wakes_the_chat_that_released_the_work() {
        let mut e = engine(None);
        e.cfg.defaults.notify_dispatcher = true;
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-parent");
        e.db.set_pr(
            "linear:LIN-142",
            Some("https://github.com/o/r/pull/18"),
            Some(18),
            true,
        )
        .unwrap();
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        let prompts = rt.prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(prompts[0].0, "chat-parent", "the dispatcher, not the child");
        assert!(prompts[0].1.contains("LIN-142"));
        assert!(prompts[0].1.contains("https://github.com/o/r/pull/18"));
    }

    /// Off by default, and the default is the design: an orchestrator woken by
    /// every child it released cannot hold a train of thought.
    #[test]
    fn the_dispatcher_is_not_woken_unless_asked_for() {
        let e = engine(None);
        assert!(!e.cfg.defaults.notify_dispatcher, "off by default");
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-parent");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert!(rt.prompts().is_empty());
        assert_eq!(
            live(&e).outcome,
            Some(Outcome::Done),
            "but it still settles"
        );
    }

    /// Operator-released work has no dispatcher chat, so the switch being on
    /// changes nothing about it. That is why it is a separate switch from the
    /// operator's own notice.
    #[test]
    fn operator_released_work_has_nobody_to_wake() {
        let mut e = engine(None);
        e.cfg.defaults.notify_dispatcher = true;
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert!(rt.prompts().is_empty());
    }

    #[test]
    fn the_webhook_gets_both_events() {
        let (e, hook) = engine_with_webhook("https://hooks.example.com/x", false);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        let posts = hook.posts.lock().unwrap().clone();
        let events: Vec<&str> = posts
            .iter()
            .map(|(_, b)| b["event"].as_str().unwrap())
            .collect();
        assert_eq!(events, vec!["on_blocked", "on_settled"]);
        assert_eq!(posts[0].0, "https://hooks.example.com/x");
        assert_eq!(posts[0].1["reason"], "asking");
        assert_eq!(posts[1].1["outcome"], "done");
    }

    /// `notify = false` is the operator saying "not tonight". It silences the
    /// out-of-band channel and nothing else — the comment upstream belongs to
    /// the task, not to whoever is watching.
    #[test]
    fn notify_off_silences_the_webhook_but_not_the_issue() {
        let (mut e, hook) = engine_with_webhook("https://hooks.example.com/x", false);
        e.cfg.defaults.notify = false;
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();

        assert!(hook.posts.lock().unwrap().is_empty());
        assert_eq!(blocked_writebacks(&e).len(), 1);
    }

    /// A notification is best effort by construction. An endpoint that is down
    /// must not hold a settle open, and must not be retried into telling
    /// somebody tomorrow about a thing that happened tonight.
    #[test]
    fn a_dead_webhook_changes_nothing_about_the_board() {
        let (e, hook) = engine_with_webhook("https://hooks.example.com/x", true);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        assert_eq!(
            hook.posts.lock().unwrap().len(),
            1,
            "tried once, not queued"
        );
    }

    /// An orphaned attempt is the other ending nobody would notice: the chat
    /// vanished, so there is no agent left to ask and no run journal to read.
    #[test]
    fn an_orphaned_attempt_is_announced_like_a_settle() {
        let (mut e, hook) = engine_with_webhook("https://hooks.example.com/x", false);
        e.cfg.defaults.notify_dispatcher = true;
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-parent");
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        for _ in 0..2 {
            e.reconcile_sessions_with(&statuses(&[]), Some(&rt))
                .unwrap();
        }

        assert_eq!(live(&e).outcome, Some(Outcome::Orphaned));
        let posts = hook.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].1["event"], "on_settled");
        assert_eq!(posts[0].1["outcome"], "orphaned");
        assert_eq!(rt.prompts().len(), 1, "the dispatcher hears about it too");
    }

    // ---- the settle the board got wrong (§H4's inverse) ------------------

    /// Settle an attempt on commits, returning its checkout for cleanup.
    fn settled_on_commits(e: &SyncEngine, attempt: i64) -> std::path::PathBuf {
        let work = agent_worked_in(e, attempt, Work::Pushed);
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        assert_eq!(live(e).outcome, Some(Outcome::Done), "fixture must settle");
        work
    }

    #[test]
    fn a_settled_chat_seen_working_again_is_reopened_not_redispatched() {
        // Commits are routinely there before the work is done, so a settle on
        // them can be wrong. The chat working again is the proof — and it is
        // counted on this attempt as `reopened`, because nobody dispatched
        // anything.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = settled_on_commits(&e, a);

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        let attempt = live(&e);
        assert!(attempt.outcome.is_none(), "back to work");
        assert_eq!(attempt.reopened, 1);
        assert_eq!(attempt.agent_status, Some(AgentStatus::Working));
        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(task.attempts.len(), 1, "re-opened, not re-dispatched");
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_finished_chat_sitting_idle_leaves_the_settle_standing() {
        // Every finished chat reads Idle forever; only Working reopens.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = settled_on_commits(&e, a);
        for _ in 0..3 {
            e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
                .unwrap();
        }
        let attempt = live(&e);
        assert_eq!(attempt.outcome, Some(Outcome::Done));
        assert_eq!(attempt.reopened, 0);
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn reopening_is_refused_once_somebody_redispatched() {
        // The re-dispatch names a decision-maker, and that attempt is the
        // current one — the old chat still typing does not overrule it.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = settled_on_commits(&e, a);
        dispatch(&e, "linear:LIN-142", "chat-10");

        e.reconcile_sessions(&statuses(&[
            ("chat-9", AgentStatus::Working),
            ("chat-10", AgentStatus::Working),
        ]))
        .unwrap();
        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(task.attempts[0].outcome, Some(Outcome::Done));
        assert_eq!(task.attempts[0].reopened, 0);
        assert!(task.attempts[1].outcome.is_none());
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn a_task_marked_done_is_not_reopened() {
        // `mark done` is the operator deciding the task is over; an agent
        // still typing does not overrule them.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = settled_on_commits(&e, a);
        e.db.set_local_done("linear:LIN-142", true).unwrap();

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    // ---- the event path --------------------------------------------------

    #[test]
    fn the_event_path_settles_on_the_transition() {
        // The moment the run ends — not the next interval tick. The 60-second
        // clock this replaces is the whole of §H4.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr(
            "linear:LIN-142",
            Some("https://github.com/o/r/pull/18"),
            Some(18),
            true,
        )
        .unwrap();
        e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        assert!(
            e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Idle)]))
                .unwrap()
        );
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        // refresh rederives, so the row is already in review.
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Review
        );
    }

    #[test]
    fn the_event_path_reopens_the_moment_a_settled_chat_works() {
        // herdr ran its rewatch on the interval because a screen-sampled
        // `working` could lie. comet's cannot, so the event path acts on it.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = settled_on_commits(&e, a);

        assert!(
            e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Working)]))
                .unwrap()
        );
        let attempt = live(&e);
        assert!(attempt.outcome.is_none());
        assert_eq!(attempt.reopened, 1);
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Working,
            "this same pass's derivation already reads the row as working"
        );
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn the_run_end_rechecks_github_before_settling_on_commits() {
        // herdr's gh#29: an agent opens its PR moments before its final turn
        // ends, and the board's poll is up to a cycle behind — so its settles
        // said "committed" about work whose PR already existed. Run ends are
        // rare events, so here the window is closed with one targeted lookup.
        let fixture = FixtureRest::new(vec![(
            "/repos/o/r/pulls".into(),
            serde_json::json!([{
                "number": 18,
                "title": "Add retry",
                "state": "open",
                "updated_at": "2026-08-01T00:00:00Z",
                "html_url": "https://github.com/o/r/pull/18",
                "head": { "ref": "board/lin-142" },
            }]),
        )]);
        let mut e = engine_with(None, Some(Github::new(Box::new(fixture) as Box<dyn Rest>)));
        // A Linear task names no repo, so the lookup walks the configured ones.
        e.cfg.github.repos = vec!["o/r".into()];
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = agent_worked_in(&e, a, Work::Pushed);

        e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();

        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(task.attempts[0].outcome, Some(Outcome::Done));
        assert!(task.pr_open, "the lookup recorded what the poll had not");
        assert_eq!(
            outcome_payload(&e)["pr_url"].as_str(),
            Some("https://github.com/o/r/pull/18"),
            "the settle carries the PR rather than asserting an absence"
        );
        assert_eq!(task.state, BoardState::Review);
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn the_interval_path_leaves_the_lookup_to_the_poll() {
        // The cycle polls GitHub moments before it reconciles, so a lookup
        // there would repeat the poll. Same fixture, interval path: the settle
        // rests on commits and claims nothing about a PR it did not check.
        let fixture = FixtureRest::new(vec![(
            "/repos/o/r/pulls".into(),
            serde_json::json!([{
                "number": 18,
                "title": "Add retry",
                "state": "open",
                "updated_at": "2026-08-01T00:00:00Z",
                "html_url": "https://github.com/o/r/pull/18",
                "head": { "ref": "board/lin-142" },
            }]),
        )]);
        let mut e = engine_with(None, Some(Github::new(Box::new(fixture) as Box<dyn Rest>)));
        e.cfg.github.repos = vec!["o/r".into()];
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = agent_worked_in(&e, a, Work::Pushed);

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        assert!(outcome_payload(&e)["pr_url"].is_null());
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    // ---- the wall-clock cap (gh#70) --------------------------------------

    /// A runtime that records what the cap said into the chat and whether it
    /// interrupted one — the two effects the check has on the world outside
    /// the database.
    #[derive(Default)]
    struct CapWatcher {
        prompts: std::cell::RefCell<Vec<(String, String)>>,
        cancels: std::cell::RefCell<Vec<String>>,
    }

    impl Runtime for CapWatcher {
        fn dispatch(&self, _: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
            unreachable!("the cap never dispatches")
        }
        fn prompt(&self, chat: &str, text: &str) -> anyhow::Result<()> {
            self.prompts
                .borrow_mut()
                .push((chat.to_string(), text.to_string()));
            Ok(())
        }
        fn cancel(&self, chat: &str) -> anyhow::Result<()> {
            self.cancels.borrow_mut().push(chat.to_string());
            Ok(())
        }
        fn session(&self, _: &str) -> anyhow::Result<Option<comet_proto::Session>> {
            Ok(None)
        }
        fn chat_alive(&self, _: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
        fn chat_cwd(&self, _: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn last_run_end(&self, _: &str) -> anyhow::Result<Option<RunEnd>> {
            Ok(None)
        }
    }

    /// Move an attempt's clocks back, as wall time would have. The cap reads
    /// `started_at` and the grace reads `overrun_warned_at`, so a test that
    /// wants to be an hour late only has to say so.
    fn age_attempt(e: &SyncEngine, attempt_id: i64, started_ago: i64, warned_ago: Option<i64>) {
        let now = chrono::Utc::now();
        let stamp = |secs: i64| crate::db::rfc3339(now - chrono::Duration::seconds(secs));
        e.db.conn
            .execute(
                "UPDATE attempts SET started_at = ?2, overrun_warned_at = ?3 WHERE id = ?1",
                rusqlite::params![attempt_id, stamp(started_ago), warned_ago.map(stamp)],
            )
            .unwrap();
    }

    fn routed(e: &mut SyncEngine, max_duration: &str) {
        e.cfg = toml::from_str(&format!(
            r#"
[[route]]
match = {{ linear_team = "LIN" }}
workspace = "offhand"
repo = "/tmp"
runtime = "claude-code"
max_duration = "{max_duration}"
"#
        ))
        .unwrap();
    }

    /// The exit criterion's quiet half: a legit long run is untouched. Two
    /// hours is the default cap and this one is one hour in.
    #[test]
    fn a_run_inside_its_cap_is_left_alone() {
        let e = engine(None);
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        age_attempt(&e, a, 3600, None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        assert!(live(&e).outcome.is_none());
        assert!(live(&e).overrun_warned_at.is_none());
        assert!(rt.prompts.borrow().is_empty(), "nothing to say yet");
        assert!(rt.cancels.borrow().is_empty());
    }

    /// Past the cap is a warning, not a kill — and exactly one warning,
    /// however many cycles pass while the agent wraps up.
    #[test]
    fn the_cap_warns_the_chat_once_before_it_closes_anything() {
        let e = engine(None);
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        age_attempt(&e, a, 3 * 3600, None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        let warned = live(&e);
        assert!(warned.outcome.is_none(), "a warning is not a cancel");
        assert!(
            warned.overrun_warned_at.is_some(),
            "the grace clock started"
        );
        let prompts = rt.prompts.borrow().clone();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].0, "chat-9");
        assert!(
            prompts[0].1.contains("3h"),
            "it names the age: {}",
            prompts[0].1
        );
        assert!(
            prompts[0].1.contains("2h cap"),
            "and the cap: {}",
            prompts[0].1
        );
        assert!(rt.cancels.borrow().is_empty());

        // Two more cycles inside the grace: no second warning, still live.
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        assert_eq!(rt.prompts.borrow().len(), 1, "prompt-once means once");
        assert!(live(&e).outcome.is_none());
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
    }

    /// The grace ran out: interrupt the chat, close `failed`, and say why
    /// upstream. Never silent — the writeback names the timeout, because
    /// `failed` on its own reads as a dispatch that never produced an agent.
    #[test]
    fn the_grace_running_out_closes_the_attempt_failed_with_a_writeback() {
        let e = engine(None);
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        age_attempt(&e, a, 3 * 3600, Some(overrun::MAX_GRACE_SECS as i64));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Failed));
        assert_eq!(rt.cancels.borrow().clone(), vec!["chat-9".to_string()]);
        let payload = outcome_payload(&e);
        assert_eq!(payload["outcome"].as_str(), Some("failed"));
        let note = payload["note"].as_str().unwrap();
        assert!(note.contains("timed out after 3h"), "{note}");
        assert!(note.contains("cap 2h"), "{note}");
        // The row is red and stays there — `failed`, not `cancelled`, which
        // would send the issue back to `ready` as if nothing had run.
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Failed
        );
    }

    /// gh#70's second hole. The engine crashed past its revival budget, so the
    /// chat settled `Idle`; with no commits `settled::decide` says
    /// `StayLive(NoArtifacts)`, and orphaning fires only on a *missing* session
    /// row — this one exists. Before the cap, that row rendered `working`
    /// forever.
    #[test]
    fn a_stranded_idle_row_with_no_artifacts_is_closed_by_the_clock() {
        let e = engine(None);
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        // The session row is still there, saying Idle, with nothing to settle
        // on. The settle declines every cycle, exactly as designed.
        age_attempt(&e, a, 3 * 3600, None);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();
        assert!(live(&e).outcome.is_none(), "warned first");

        age_attempt(&e, a, 4 * 3600, Some(overrun::MAX_GRACE_SECS as i64));
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Failed));
    }

    /// The settle beats the cap. An agent that took the warning, committed and
    /// finished inside its grace is a finished attempt — closing it `failed`
    /// on the same tick would throw away the work it just did.
    #[test]
    fn work_finished_inside_the_grace_settles_done_not_failed() {
        let e = engine(None);
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let work = agent_worked_in(&e, a, Work::Pushed);
        age_attempt(&e, a, 5 * 3600, Some(overrun::MAX_GRACE_SECS as i64));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        assert!(
            rt.cancels.borrow().is_empty(),
            "a finished run is not killed"
        );
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    /// The cap is a property of the work, so it is per route: the refactor
    /// route gets six hours without loosening the one that fixes typos.
    #[test]
    fn a_route_can_raise_the_cap_over_the_default() {
        let mut e = engine(None);
        routed(&mut e, "6h");
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        age_attempt(&e, a, 5 * 3600, None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        assert!(live(&e).overrun_warned_at.is_none(), "5h is inside 6h");
        assert!(rt.prompts.borrow().is_empty());
    }

    /// `off` is what every board did before this existed, said out loud.
    #[test]
    fn a_route_can_turn_the_cap_off_entirely() {
        let mut e = engine(None);
        routed(&mut e, "off");
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        age_attempt(&e, a, 48 * 3600, None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        assert!(live(&e).outcome.is_none());
        assert!(rt.prompts.borrow().is_empty());
    }

    /// A dispatch whose brief never reached a chat is the most stranded row of
    /// the lot: no session, no `saw_working`, so orphaning deliberately leaves
    /// it (H2). The clock is what closes it — with nothing to interrupt, and
    /// the reason still written back.
    #[test]
    fn a_dispatch_that_never_got_a_chat_is_closed_by_the_clock_too() {
        let e = engine(None);
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch_on(&e, "linear:LIN-142", "board/lin-142");
        age_attempt(&e, a, 3 * 3600, Some(overrun::MAX_GRACE_SECS as i64));

        e.reconcile_sessions_with(&statuses(&[]), Some(&rt))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Failed));
        assert!(rt.cancels.borrow().is_empty(), "there was no chat to end");
        assert!(
            outcome_payload(&e)["note"]
                .as_str()
                .unwrap()
                .contains("timed out")
        );
    }

    /// The watch is a firehose; the cap is a clock. Counting it in event time
    /// is the flap gh#34 taught herdr-board about, and would let a busy chat
    /// talk itself past its own cap.
    #[test]
    fn a_watch_event_never_enforces_the_cap() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        // Nine hours over a two-hour cap: the interval reconcile would warn on
        // its next tick and close it on the one after. Events decide nothing.
        age_attempt(&e, a, 9 * 3600, None);

        for _ in 0..10 {
            e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Working)]))
                .unwrap();
        }
        assert!(live(&e).outcome.is_none());
        assert!(live(&e).overrun_warned_at.is_none());
    }

    /// The warning has to happen even where it cannot be delivered — the log
    /// and the eventual close are what the operator has left. A read-only
    /// caller has no runtime at all, and must not be the reason a runaway
    /// attempt is exempt.
    #[test]
    fn the_cap_still_decides_without_a_runtime_to_talk_through() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        age_attempt(&e, a, 3 * 3600, None);
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        assert!(live(&e).overrun_warned_at.is_some());

        age_attempt(&e, a, 4 * 3600, Some(overrun::MAX_GRACE_SECS as i64));
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Failed));
    }

    /// Re-opening hands the attempt back its warning and its grace. It has
    /// been live the whole time — `started_at` is not reset — but a row that
    /// came back to work being killed on the next tick, by a stamp collected
    /// before the board wrongly called it finished, is the cap punishing the
    /// board's own mistake.
    #[test]
    fn reopening_gives_the_attempt_a_fresh_warning() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        age_attempt(&e, a, 3 * 3600, Some(60));
        e.db.close_attempt(a, Outcome::Done).unwrap();

        assert!(e.db.reopen_attempt(a).unwrap());
        assert!(live(&e).overrun_warned_at.is_none());
    }

    // ---- pull-request linking -------------------------------------------

    #[test]
    fn pull_requests_link_to_tasks_by_attempt_branch() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        seed(&e, "linear:LIN-999", "LIN-999", UpstreamState::Started);

        e.link_pull_requests(&[PullRequest {
            repo: "o/r".into(),
            number: 291,
            title: "Add retry".into(),
            body: None,
            url: "https://github.com/o/r/pull/291".into(),
            head_ref: "board/lin-142".into(),
            open: true,
            merged: false,
            draft: false,
            updated_at: crate::db::now(),
        }])
        .unwrap();

        assert!(e.db.get_task("linear:LIN-142").unwrap().unwrap().pr_open);
        // The unrelated task must not pick up the PR.
        assert!(!e.db.get_task("linear:LIN-999").unwrap().unwrap().pr_open);
    }

    #[test]
    fn a_pull_request_never_crosses_into_another_repo() {
        // herdr-board AGE-20, seen live: gh#2 exists in two repos, both branch
        // to `board/gh-2`, and one repo's *merged* PR attached itself to the
        // other's task — deriving it to review with no work done.
        let e = engine(None);
        seed(
            &e,
            "gh:Florin-AS/tripletex-mcp#2",
            "gh#2",
            UpstreamState::Started,
        );
        dispatch_on(&e, "gh:Florin-AS/tripletex-mcp#2", "board/gh-2");

        e.link_pull_requests(&[PullRequest {
            repo: "bredebjorhovd/OIOS".into(),
            number: 10,
            title: "Row-capture sweeps".into(),
            body: None,
            url: "https://github.com/bredebjorhovd/OIOS/pull/10".into(),
            head_ref: "board/gh-2".into(),
            open: false,
            merged: true,
            draft: false,
            updated_at: crate::db::now(),
        }])
        .unwrap();

        let t =
            e.db.get_task("gh:Florin-AS/tripletex-mcp#2")
                .unwrap()
                .unwrap();
        assert_eq!(t.pr_url, None, "another repo's PR is not this task's PR");
        assert!(!t.pr_merged, "and must not mark it finished");
    }

    #[test]
    fn another_repos_attempt_branch_does_not_swallow_a_pull_request() {
        // The other side of the same ambiguity (AGE-20). A PR on a branch some
        // attempt owns is that attempt's, not a row of its own — but the same
        // branch name in another repo is not this attempt's merely because the
        // strings match, and treating it as one drops a real pull request off
        // the board with no trace.
        let mut e = engine_with(
            None,
            Some(Github::new(Box::new(FixtureRest::new(vec![
                (
                    "/repos/Florin-AS/tripletex-mcp/issues".into(),
                    json!([{ "number": 2, "node_id": "n1", "title": "Ingest sweeps",
                             "html_url": "u", "state": "open", "updated_at": "t",
                             "labels": [] }]),
                ),
                ("/repos/Florin-AS/tripletex-mcp/pulls".into(), json!([])),
                ("/repos/bredebjorhovd/OIOS/issues".into(), json!([])),
                (
                    "/repos/bredebjorhovd/OIOS/pulls".into(),
                    json!([{ "number": 10, "title": "Row-capture sweeps",
                             "html_url": "https://github.com/bredebjorhovd/OIOS/pull/10",
                             "state": "closed", "merged_at": "t", "updated_at": "t",
                             "head": { "ref": "board/gh-2" } }]),
                ),
            ])) as Box<dyn Rest>)),
        );
        e.cfg.github.repos = vec![
            "Florin-AS/tripletex-mcp".into(),
            "bredebjorhovd/OIOS".into(),
        ];
        seed(
            &e,
            "gh:Florin-AS/tripletex-mcp#2",
            "gh#2",
            UpstreamState::Started,
        );
        dispatch_on(&e, "gh:Florin-AS/tripletex-mcp#2", "board/gh-2");

        e.poll_github();

        let pr = e.db.get_task("gh:bredebjorhovd/OIOS!10").unwrap();
        assert!(
            pr.is_some(),
            "the pull request is nobody's attempt and belongs on the board"
        );
        let t =
            e.db.get_task("gh:Florin-AS/tripletex-mcp#2")
                .unwrap()
                .unwrap();
        assert_eq!(t.pr_url, None, "and is still not the other repo's");
    }

    /// A merged pull request that the board is only told about by a poll.
    fn merged_pr() -> PullRequest {
        PullRequest {
            repo: "o/r".into(),
            number: 291,
            title: "Add retry".into(),
            body: None,
            url: "https://github.com/o/r/pull/291".into(),
            head_ref: "board/lin-142".into(),
            open: false,
            merged: true,
            draft: false,
            updated_at: crate::db::now(),
        }
    }

    #[test]
    fn a_pull_request_merged_outside_the_board_still_closes_the_ticket() {
        // `gh pr merge` and the web UI go nowhere near the board, so nothing
        // told the tracker and tickets sat at In Review until someone closed
        // them by hand (herdr-board AGE-22).
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        e.link_pull_requests(&[merged_pr()]).unwrap();

        let t = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert!(t.pr_merged);
        // Same row state as an in-board merge produces — the route the merge
        // took must not decide where the row ends up.
        assert!(t.local_done, "observing a merge finishes the work");
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Done
        );
        assert!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .any(|w| w.kind == "close"),
            "and the ticket is queued to be closed"
        );
    }

    #[test]
    fn a_merge_seen_by_the_poll_lets_the_row_leave_review() {
        // The half that made this more than a missed advance. The attempt
        // settled, so derivation reaches `review` off its outcome and keeps
        // reaching it — the ticket is held in the wrong state rather than
        // merely not advanced. Only something that finishes the task ends that.
        let mut e = engine(None);
        e.cfg.linear.review_state = Some("In Review".into());
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.close_attempt(a, Outcome::Done).unwrap();
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Review
        );

        e.link_pull_requests(&[merged_pr()]).unwrap();
        for _ in 0..3 {
            e.rederive_all().unwrap();
        }
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Done,
            "and it stays out — no tick puts it back in review"
        );
    }

    #[test]
    fn a_merge_observed_on_every_poll_only_closes_the_ticket_once() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        for _ in 0..5 {
            e.link_pull_requests(&[merged_pr()]).unwrap();
            e.rederive_all().unwrap();
        }
        assert_eq!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .filter(|w| w.kind == "close")
                .count(),
            1,
        );
    }

    #[test]
    fn a_task_already_finished_upstream_is_not_reopened_by_a_merge() {
        // The issue was closed upstream; a merged PR turning up afterwards has
        // nothing left to say, and closing an already-closed ticket is noise.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Terminal);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.rederive_all().unwrap();

        e.link_pull_requests(&[merged_pr()]).unwrap();

        assert!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .all(|w| w.kind != "close"),
        );
        // The PR fact itself is still recorded — only the writeback is skipped.
        assert!(e.db.get_task("linear:LIN-142").unwrap().unwrap().pr_merged);
    }

    // ---- source health ---------------------------------------------------

    #[test]
    fn a_linear_outage_serves_stale_data_and_marks_the_header() {
        struct Down;
        impl GraphQl for Down {
            fn query(&self, _: &Value) -> Result<Value> {
                anyhow::bail!("connection refused")
            }
        }
        let e = engine(Some(Linear::new(Box::new(Down) as Box<dyn GraphQl>)));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);

        e.poll_linear();

        // The row is still there — never blank the list because a poll failed.
        assert_eq!(e.db.load_tasks().unwrap().len(), 1);
        match e.health(Source::Linear) {
            SourceHealth::Down { error, retry_in } => {
                assert!(error.contains("connection refused"));
                assert!(retry_in > 0 && retry_in <= 300);
            }
            other => panic!("expected Down, got {other:?}"),
        }
    }

    #[test]
    fn backoff_grows_across_consecutive_failures_and_caps() {
        struct Down;
        impl GraphQl for Down {
            fn query(&self, _: &Value) -> Result<Value> {
                anyhow::bail!("nope")
            }
        }
        let e = engine(Some(Linear::new(Box::new(Down) as Box<dyn GraphQl>)));
        e.poll_linear();
        let first = match e.health(Source::Linear) {
            SourceHealth::Down { retry_in, .. } => retry_in,
            _ => unreachable!(),
        };
        for _ in 0..10 {
            e.poll_linear();
        }
        let later = match e.health(Source::Linear) {
            SourceHealth::Down { retry_in, .. } => retry_in,
            _ => unreachable!(),
        };
        assert!(later > first);
        assert_eq!(later, 300, "backoff must cap at 5 minutes");
    }

    #[test]
    fn recovery_clears_the_header_and_the_failure_count() {
        let page = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(
            Box::new(FixtureTransport::new(vec![page.clone(), page])) as Box<dyn GraphQl>,
        )));
        e.db.meta_set(meta::LINEAR_STATUS, "error:earlier").unwrap();
        e.db.meta_set(meta::LINEAR_FAILURES, "4").unwrap();
        e.poll_linear();
        assert_eq!(e.health(Source::Linear), SourceHealth::Ok);
    }

    #[test]
    fn an_unconfigured_source_is_absent_not_down() {
        // The header must not render `gh ✗` for a source nobody configured.
        let e = engine(None);
        assert_eq!(e.health(Source::Github), SourceHealth::Absent);
    }

    #[test]
    fn a_reader_reports_health_the_loop_recorded() {
        // A reader builds no Linear client of its own when it starts before the
        // key exists, but it must still show `linear ✓` once the engine's loop
        // is polling successfully.
        let e = engine(None);
        assert_eq!(e.health(Source::Linear), SourceHealth::Absent);
        e.db.meta_set(meta::LINEAR_STATUS, "ok").unwrap();
        assert_eq!(e.health(Source::Linear), SourceHealth::Ok);
    }

    // ---- reaping ---------------------------------------------------------

    #[test]
    fn a_full_sweep_removes_a_task_deleted_upstream() {
        let empty = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(
            Box::new(FixtureTransport::new(vec![empty.clone(), empty])) as Box<dyn GraphQl>,
        )));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        assert_eq!(e.db.load_tasks().unwrap().len(), 1);

        // No watermark recorded, so this poll is a full sweep.
        e.poll_linear();
        assert!(
            e.db.load_tasks().unwrap().is_empty(),
            "a task the source no longer returns must not linger forever"
        );
    }

    #[test]
    fn a_sweep_never_removes_a_task_with_a_running_agent() {
        let empty = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(
            Box::new(FixtureTransport::new(vec![empty.clone(), empty])) as Box<dyn GraphQl>,
        )));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        e.poll_linear();
        assert_eq!(
            e.db.load_tasks().unwrap().len(),
            1,
            "a row must not vanish from under a live chat"
        );
    }

    /// Deleting the task would delete the only row that knows where the
    /// attempt's checkout is, and `gc` then refuses to collect a directory it
    /// cannot attribute. The row stays.
    #[test]
    fn a_sweep_keeps_the_attempts_of_a_task_that_was_worked_on() {
        let empty = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(
            Box::new(FixtureTransport::new(vec![empty.clone(), empty])) as Box<dyn GraphQl>,
        )));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.conn
            .execute(
                "UPDATE attempts SET worktree = '/wt/lin-142-1' WHERE id = ?1",
                rusqlite::params![a],
            )
            .unwrap();
        // Closed, so the task is reapable at all — a live attempt is protected
        // by a rule of its own.
        e.db.close_attempt(a, Outcome::Done).unwrap();

        e.poll_linear();

        let t = e.db.get_task("linear:LIN-142").unwrap().expect("row kept");
        assert_eq!(t.upstream, UpstreamState::Gone);
        assert_eq!(t.attempts.len(), 1, "the history is the point");
        assert_eq!(t.attempts[0].worktree.as_deref(), Some("/wt/lin-142-1"));
        // And it derives out of the queue, into `done`.
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Done
        );
    }

    #[test]
    fn a_sweep_still_forgets_a_task_nobody_ever_dispatched() {
        // The noise case: created, mislabelled or deleted again, never worked
        // on. Nothing to keep.
        let empty = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(
            Box::new(FixtureTransport::new(vec![empty.clone(), empty])) as Box<dyn GraphQl>,
        )));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);

        e.poll_linear();
        assert!(e.db.load_tasks().unwrap().is_empty());
    }

    #[test]
    fn a_reaped_row_is_not_reported_again_on_the_next_sweep() {
        let empty = json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } });
        let e = engine(Some(Linear::new(Box::new(FixtureTransport::new(vec![
            empty.clone(),
            empty.clone(),
            empty.clone(),
            empty,
        ])) as Box<dyn GraphQl>)));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.close_attempt(a, Outcome::Done).unwrap();

        e.poll_linear();
        e.db.meta_set(meta::LAST_FULL_SWEEP, "2026-01-01T00:00:00Z")
            .unwrap();
        e.poll_linear();
        assert!(
            e.db.reapable_task_ids(Source::Linear).unwrap().is_empty(),
            "a gone row has nothing left to reap"
        );
    }

    #[test]
    fn an_incremental_poll_does_not_reap() {
        // An incremental response is not the whole set, so absence proves
        // nothing.
        let nodes = json!([{
            "id": "uuid-1", "identifier": "LIN-999", "title": "t", "url": "u",
            "updatedAt": "2026-07-25T18:00:00.000Z",
            "state": { "name": "Todo", "type": "unstarted" },
            "team": { "key": "LIN" }, "labels": { "nodes": [] },
            "attachments": { "nodes": [] }
        }]);
        let e = engine(Some(Linear::new(Box::new(FixtureTransport::new(vec![
            json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": nodes } }),
            json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": [] } }),
        ])) as Box<dyn GraphQl>)));
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        // Mark a sweep as just done, so this poll is incremental.
        e.db.meta_set(meta::LAST_FULL_SWEEP, &crate::db::now())
            .unwrap();
        e.db.meta_set(meta::LINEAR_WATERMARK, "2026-07-01T00:00:00Z")
            .unwrap();

        e.poll_linear();
        assert_eq!(e.db.load_tasks().unwrap().len(), 2, "nothing reaped");
    }

    #[test]
    fn watermark_advances_to_the_newest_issue() {
        let nodes = json!([{
            "id": "uuid-1", "identifier": "LIN-142", "title": "t",
            "url": "u", "updatedAt": "2026-07-25T18:00:00.000Z",
            "state": { "name": "Todo", "type": "unstarted" },
            "team": { "key": "LIN" }, "labels": { "nodes": [] },
            "attachments": { "nodes": [] }
        }]);
        let e = engine(Some(Linear::new(Box::new(FixtureTransport::new(vec![
            json!({ "issues": { "pageInfo": { "hasNextPage": false }, "nodes": nodes } }),
        ])) as Box<dyn GraphQl>)));
        e.poll_linear();
        assert_eq!(
            e.db.meta_get(meta::LINEAR_WATERMARK).unwrap().as_deref(),
            Some("2026-07-25T18:00:00.000Z")
        );
    }

    // ---- GitHub writeback ------------------------------------------------

    fn seed_gh(e: &SyncEngine) {
        seed_gh_in(e, "gh:o/r#87");
    }

    /// A GitHub row whose id names the repo it belongs to — which is what
    /// per-repo settings are looked up by, so a test about them needs to choose.
    fn seed_gh_in(e: &SyncEngine, id: &str) {
        let number = id.rsplit('#').next().unwrap_or("0").to_string();
        e.db.upsert_task(&UpsertTask {
            id: id.into(),
            source: Source::Github,
            source_id: format!("n{number}"),
            identifier: format!("gh#{number}"),
            title: "Bug".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: Some("open".into()),
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
    }

    /// An engine with GitHub writeback enabled — it is off by default, so a
    /// test about writeback has to ask for it.
    fn engine_with_gh_writeback() -> SyncEngine {
        let mut e = engine_with(None, Some(gh_client()));
        e.cfg.github.writeback = true;
        e
    }

    fn gh_client() -> Github<Box<dyn Rest>> {
        Github::new(Box::new(FixtureRest::new(vec![])) as Box<dyn Rest>)
    }

    #[test]
    fn merging_moves_the_row_without_waiting_for_a_poll() {
        // The operator just pressed a key; the row has to move now, not in
        // thirty seconds.
        let e = engine_with(None, Some(gh_client()));
        seed_gh(&e);
        e.db.set_pr(
            "gh:o/r#87",
            Some("https://github.com/o/r/pull/87"),
            Some(87),
            true,
        )
        .unwrap();
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("gh:o/r#87").unwrap().unwrap().state,
            BoardState::Review
        );

        let task = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        e.merge_pull_request(&task).unwrap();

        let after = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        assert!(!after.pr_open, "the PR is no longer open");
        assert_eq!(after.state, BoardState::Done);
    }

    #[test]
    fn merging_queues_the_ticket_to_be_closed() {
        let e = engine_with(None, Some(gh_client()));
        seed_gh(&e);
        e.db.set_pr(
            "gh:o/r#87",
            Some("https://github.com/o/r/pull/87"),
            Some(87),
            true,
        )
        .unwrap();
        let task = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        e.merge_pull_request(&task).unwrap();
        assert!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .any(|w| w.kind == "close"),
            "merging finished the work; the ticket is what is left"
        );
    }

    #[test]
    fn a_github_outcome_leaves_the_same_trail_as_linear() {
        let e = engine_with(None, Some(gh_client()));
        seed_gh(&e);
        let task = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        e.enqueue_outcome(&task, Outcome::Failed, None).unwrap();
        e.drain_writebacks();
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
        // It was actually delivered, not dropped.
        assert!(e.github.as_ref().is_some_and(|_| {
            e.db.meta_get(&meta::writeback_at("gh:o/r#87"))
                .unwrap()
                .is_some()
        }));
    }

    /// Reaping drops the queued writebacks it can see, but one enqueued between
    /// that sweep and the next drain would otherwise sit there failing against
    /// a deleted issue and backing off forever. It leaves the queue — and the
    /// log calls it dropped rather than delivered, because nothing was.
    #[test]
    fn a_writeback_against_a_gone_task_is_dropped_rather_than_retried_forever() {
        let e = engine_with(None, Some(gh_client()));
        seed_gh(&e);
        let task = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        e.enqueue_outcome(&task, Outcome::Done, None).unwrap();
        e.db.conn
            .execute(
                "UPDATE tasks SET upstream = 'gone' WHERE id = 'gh:o/r#87'",
                [],
            )
            .unwrap();

        let w = e.db.pending_writebacks(1).unwrap().remove(0);
        assert!(matches!(e.deliver(&w).unwrap(), Sent::Dropped(_)));

        e.drain_writebacks();
        assert_eq!(
            e.db.pending_writeback_count().unwrap(),
            0,
            "it must leave the queue, not back off against a 404"
        );
        assert!(
            e.db.meta_get(&meta::writeback_at("gh:o/r#87"))
                .unwrap()
                .is_some(),
            "and it is recorded as handled, so nothing re-queues it"
        );
    }

    #[test]
    fn a_github_row_that_reaches_done_queues_a_close() {
        // Otherwise the next poll recomputes `open` upstream and "mark done"
        // undoes itself.
        let e = engine_with_gh_writeback();
        seed_gh(&e);
        e.db.set_local_done("gh:o/r#87", true).unwrap();
        e.rederive_all().unwrap();

        assert_eq!(
            e.db.get_task("gh:o/r#87").unwrap().unwrap().state,
            BoardState::Done
        );
        let pending = e.db.pending_writebacks(10).unwrap();
        assert!(
            pending.iter().any(|w| w.kind == "close"),
            "a close was not queued: {pending:?}"
        );
    }

    #[test]
    fn the_close_is_queued_once_however_many_times_we_rederive() {
        let e = engine_with_gh_writeback();
        seed_gh(&e);
        e.db.set_local_done("gh:o/r#87", true).unwrap();
        for _ in 0..5 {
            e.rederive_all().unwrap();
        }
        assert_eq!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .filter(|w| w.kind == "close")
                .count(),
            1
        );
    }

    #[test]
    fn an_already_closed_issue_is_not_closed_again() {
        let e = engine_with_gh_writeback();
        e.db.upsert_task(&UpsertTask {
            id: "gh:o/r#88".into(),
            source: Source::Github,
            source_id: "n2".into(),
            identifier: "gh#88".into(),
            title: "Bug".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: Some("closed".into()),
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Terminal,
            updated_at: crate::db::now(),
        })
        .unwrap();
        e.rederive_all().unwrap();
        assert!(
            !e.db
                .pending_writebacks(10)
                .unwrap()
                .iter()
                .any(|w| w.kind == "close")
        );
    }

    #[test]
    fn writeback_is_off_unless_asked_for() {
        // Pointing the board at a repo is not the same as asking it to write to
        // your issues.
        let e = engine_with(None, Some(gh_client()));
        assert!(!e.cfg.github.writeback, "writeback must default to off");
        seed_gh(&e);
        e.db.set_local_done("gh:o/r#87", true).unwrap();
        e.rederive_all().unwrap();
        // The row still moves locally; nothing is sent upstream.
        assert_eq!(
            e.db.get_task("gh:o/r#87").unwrap().unwrap().state,
            BoardState::Done
        );
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
    }

    // ---- one writeback flag cannot answer for every repo -----------------

    /// Writeback on globally, off for the production repo.
    fn engine_with_one_read_only_repo() -> SyncEngine {
        let mut e = engine_with(None, Some(gh_client()));
        e.cfg.github.repos = vec!["bredebjorhovd/OIOS".into(), "Florin-AS/Tally".into()];
        e.cfg.github.writeback = true;
        e.cfg.github.per_repo = vec![crate::config::RepoConfig {
            name: "Florin-AS/Tally".into(),
            labels: None,
            writeback: Some(false),
        }];
        e.cfg
            .check()
            .expect("the config the operator would write must validate");
        e
    }

    #[test]
    fn a_read_only_repo_queues_no_close_while_the_others_still_do() {
        // The bug: the flag was set by its riskiest repo, so wanting the trail
        // on one repo meant accepting it on the other, and refusing it there
        // meant the board could close nothing anywhere.
        let e = engine_with_one_read_only_repo();
        seed_gh_in(&e, "gh:bredebjorhovd/OIOS#12");
        seed_gh_in(&e, "gh:Florin-AS/Tally#34");
        e.db.set_local_done("gh:bredebjorhovd/OIOS#12", true)
            .unwrap();
        e.db.set_local_done("gh:Florin-AS/Tally#34", true).unwrap();
        e.rederive_all().unwrap();

        // Both rows still move locally: the board's own view is not what the
        // setting is about.
        for id in ["gh:bredebjorhovd/OIOS#12", "gh:Florin-AS/Tally#34"] {
            assert_eq!(
                e.db.get_task(id).unwrap().unwrap().state,
                BoardState::Done,
                "{id} did not reach done locally"
            );
        }
        let queued: Vec<String> =
            e.db.pending_writebacks(10)
                .unwrap()
                .into_iter()
                .filter(|w| w.kind == "close")
                .map(|w| w.task_id)
                .collect();
        assert_eq!(queued, ["gh:bredebjorhovd/OIOS#12"], "{queued:?}");
    }

    #[test]
    fn a_comment_aimed_at_a_read_only_repo_is_dropped_at_delivery() {
        // Config decides at delivery, and it decides per repo: a comment queued
        // while the flag was global must not land once the repo says otherwise.
        let e = engine_with_one_read_only_repo();
        seed_gh_in(&e, "gh:Florin-AS/Tally#34");
        let task = e.db.get_task("gh:Florin-AS/Tally#34").unwrap().unwrap();
        e.enqueue_outcome(&task, Outcome::Done, None).unwrap();

        let w = e.db.pending_writebacks(1).unwrap().remove(0);
        match e.deliver(&w).unwrap() {
            Sent::Dropped(why) => assert!(
                why.contains("Florin-AS/Tally"),
                "the reason has to name the repo that refused it: {why}"
            ),
            other => panic!("it was not dropped: {other:?}"),
        }

        e.drain_writebacks();
        assert_eq!(
            e.db.pending_writeback_count().unwrap(),
            0,
            "a dropped writeback leaves the queue rather than backing off"
        );
    }

    #[test]
    fn a_repo_can_be_written_to_while_the_global_flag_stays_off() {
        // The override goes both ways. Otherwise the safe global default can
        // only be escaped by turning every repo on at once — the same bug.
        let mut e = engine_with(None, Some(gh_client()));
        e.cfg.github.repos = vec!["bredebjorhovd/herdr-board".into()];
        e.cfg.github.per_repo = vec![crate::config::RepoConfig {
            name: "bredebjorhovd/herdr-board".into(),
            labels: None,
            writeback: Some(true),
        }];
        e.cfg.check().unwrap();
        assert!(!e.cfg.github.writeback, "the global flag is still off");

        seed_gh_in(&e, "gh:bredebjorhovd/herdr-board#7");
        e.db.set_local_done("gh:bredebjorhovd/herdr-board#7", true)
            .unwrap();
        e.rederive_all().unwrap();
        assert!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .any(|w| w.kind == "close"),
            "the repo asked for the trail and did not get it"
        );
    }

    // ---- one label filter cannot answer for every repo -------------------

    /// Records the paths asked for, from outside the `Box<dyn Rest>` the engine
    /// holds — which is the only place the label filter is observable.
    struct Recorder(Arc<std::sync::Mutex<Vec<String>>>);

    impl Rest for Recorder {
        fn get(&self, path: &str) -> Result<Value> {
            self.0.lock().unwrap().push(path.to_string());
            Ok(json!([]))
        }
        fn post(&self, _: &str, _: &Value) -> Result<Value> {
            Ok(Value::Null)
        }
        fn patch(&self, _: &str, _: &Value) -> Result<Value> {
            Ok(Value::Null)
        }
        fn put(&self, _: &str, _: &Value) -> Result<Value> {
            Ok(Value::Null)
        }
    }

    #[test]
    fn each_repo_is_polled_for_its_own_labels() {
        // The bug: `[github] labels = []` is right for a curated tracker and a
        // backlog dump for the repo next to it, and there was no way to say so.
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut e = engine_with(
            None,
            Some(Github::new(
                Box::new(Recorder(asked.clone())) as Box<dyn Rest>
            )),
        );
        e.cfg.github.repos = vec!["Florin-AS/Tally".into(), "b/itsm-agent".into()];
        e.cfg.github.labels = vec![];
        e.cfg.github.per_repo = vec![crate::config::RepoConfig {
            name: "b/itsm-agent".into(),
            labels: Some(vec!["release-a".into()]),
            writeback: None,
        }];
        e.cfg
            .check()
            .expect("the config the operator would write must validate");

        e.poll_github();

        let asked = asked.lock().unwrap().clone();
        let queries: Vec<&String> = asked.iter().filter(|p| p.contains("/issues?")).collect();
        assert_eq!(queries.len(), 2, "{asked:?}");
        assert!(
            !queries[0].contains("labels="),
            "Tally asked for a filter it never configured: {}",
            queries[0]
        );
        assert!(
            queries[1].contains("labels=release-a"),
            "itsm-agent's whole backlog would arrive: {}",
            queries[1]
        );
    }

    // ---- Linear has to be told the work is waiting on a human ------------

    /// One page of workflow states, as `state_id_named` reads them. `In Review`
    /// and `In Progress` are both `started`, which is the whole problem.
    fn states_page() -> Value {
        json!({
            "teams": { "nodes": [ { "id": "team-1", "states": { "nodes": [
                { "id": "s-rev",  "name": "In Review",   "type": "started",   "position": 2.0 },
                { "id": "s-prog", "name": "In Progress", "type": "started",   "position": 1.0 }
            ] } } ] }
        })
    }

    /// A fixture transport the test can still read after the engine has boxed
    /// it away, so what was actually sent to Linear can be asserted on.
    struct Shared(std::rc::Rc<FixtureTransport>);

    impl GraphQl for Shared {
        fn query(&self, body: &Value) -> Result<Value> {
            self.0.query(body)
        }
    }

    /// A Linear engine that has been told which state means review.
    fn engine_with_review_state(responses: Vec<Value>) -> SyncEngine {
        recording_engine(responses).0
    }

    fn recording_engine(responses: Vec<Value>) -> (SyncEngine, std::rc::Rc<FixtureTransport>) {
        let transport = std::rc::Rc::new(FixtureTransport::new(responses));
        let mut e = engine(Some(Linear::new(
            Box::new(Shared(transport.clone())) as Box<dyn GraphQl>
        )));
        e.cfg.linear.review_state = Some("In Review".into());
        (e, transport)
    }

    /// A Linear row with an open pull request — the board derives `review`.
    fn seed_in_review(e: &SyncEngine) {
        seed(e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        e.db.set_pr(
            "linear:LIN-142",
            Some("https://github.com/o/r/pull/291"),
            Some(291),
            true,
        )
        .unwrap();
    }

    #[test]
    fn a_row_that_reaches_review_queues_the_linear_transition() {
        // Tickets read In Progress in Linear for the whole time their PRs sat
        // waiting: dispatch moved them, and nothing moved them again until a
        // merge.
        let e = engine_with_review_state(vec![]);
        seed_in_review(&e);
        e.rederive_all().unwrap();

        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Review
        );
        assert!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .any(|w| w.kind == "review"),
            "reaching review must tell Linear, not only the board"
        );
    }

    #[test]
    fn with_no_review_state_configured_linear_is_left_where_it_was() {
        // The default. Linear has no review state *type*, so with nothing named
        // there is no correct target — and a workspace without such a state must
        // keep behaving exactly as it did.
        let e = engine(None);
        assert!(e.cfg.linear.review_state.is_none(), "unset by default");
        seed_in_review(&e);
        e.rederive_all().unwrap();

        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Review
        );
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
    }

    #[test]
    fn the_transition_is_queued_once_however_many_times_we_rederive() {
        let e = engine_with_review_state(vec![]);
        seed_in_review(&e);
        for _ in 0..5 {
            e.rederive_all().unwrap();
        }
        assert_eq!(
            e.db.pending_writebacks(10)
                .unwrap()
                .iter()
                .filter(|w| w.kind == "review")
                .count(),
            1
        );
    }

    #[test]
    fn a_retry_gets_a_transition_of_its_own() {
        // Dispatching again moves the ticket back to In Progress, so the attempt
        // that follows has its own review to announce.
        let e = engine_with_review_state(vec![]);
        seed_in_review(&e);
        e.rederive_all().unwrap();
        let first = e.db.pending_writebacks(10).unwrap();

        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.close_attempt(a, Outcome::Done).unwrap();
        e.rederive_all().unwrap();

        let after = e.db.pending_writebacks(10).unwrap();
        assert_eq!(
            after.iter().filter(|w| w.kind == "review").count(),
            2,
            "queued: {:?} then {:?}",
            first.iter().map(|w| &w.idem_key).collect::<Vec<_>>(),
            after.iter().map(|w| &w.idem_key).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_ticket_already_in_the_review_state_is_not_moved_again() {
        // The operator dragged it there themselves, or an earlier tick did. A
        // mutation that changes nothing is still a write to somebody's tracker.
        let e = engine_with_review_state(vec![]);
        seed_in_review(&e);
        e.db.conn
            .execute(
                "UPDATE tasks SET source_state = 'In Review' WHERE id = 'linear:LIN-142'",
                [],
            )
            .unwrap();
        e.rederive_all().unwrap();
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
    }

    #[test]
    fn a_closed_issue_is_never_dragged_back_into_review() {
        // "mark done" derives Done, not Review, so the case that matters is an
        // issue closed upstream while its PR is still open.
        let e = engine_with_review_state(vec![]);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Terminal);
        e.db.set_pr("linear:LIN-142", Some("u"), Some(291), true)
            .unwrap();
        e.rederive_all().unwrap();
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
    }

    #[test]
    fn delivering_the_transition_sets_the_named_state() {
        let (e, transport) = recording_engine(vec![
            states_page(),
            json!({ "issueUpdate": { "success": true } }),
        ]);
        seed_in_review(&e);
        e.rederive_all().unwrap();
        e.drain_writebacks();

        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
        let sent = transport.sent.borrow();
        // Not the lowest-position `started` state, which is In Progress and is
        // where dispatch already put it.
        assert_eq!(sent[1]["variables"]["stateId"], json!("s-rev"));
        assert_eq!(sent[1]["variables"]["id"], json!("uuid-1"));
    }

    #[test]
    fn a_review_state_that_does_not_exist_is_dropped_rather_than_retried() {
        // A name that resolves to nothing is a config mistake, and doctor is
        // where it gets reported. Backing off against Linear forever would only
        // bury it.
        let e = engine_with_review_state(vec![json!({
            "teams": { "nodes": [ { "id": "team-1", "states": { "nodes": [
                { "id": "s-prog", "name": "Pågår", "type": "started", "position": 1.0 }
            ] } } ] }
        })]);
        seed_in_review(&e);
        e.rederive_all().unwrap();

        let w = e.db.pending_writebacks(10).unwrap();
        let w = w.iter().find(|w| w.kind == "review").unwrap();
        assert!(matches!(e.deliver(w).unwrap(), Sent::Dropped(_)));
    }

    #[test]
    fn turning_the_setting_off_stops_a_transition_still_in_the_queue() {
        let mut e = engine_with_review_state(vec![]);
        seed_in_review(&e);
        e.rederive_all().unwrap();
        e.cfg.linear.review_state = None;

        let w = e.db.pending_writebacks(10).unwrap();
        let w = w.iter().find(|w| w.kind == "review").unwrap();
        assert!(matches!(e.deliver(w).unwrap(), Sent::Dropped(_)));
    }

    /// GitHub has no equivalent gap to close. An issue there is open or closed —
    /// there is no state between the two to advance to — and the `outcome`
    /// writeback already comments the PR link on the issue when an attempt
    /// settles, which is the whole of what GitHub can be told.
    #[test]
    fn a_github_row_reaching_review_has_nothing_to_transition() {
        let mut e = engine_with_gh_writeback();
        e.cfg.linear.review_state = Some("In Review".into());
        seed_gh(&e);
        e.db.set_pr(
            "gh:o/r#87",
            Some("https://github.com/o/r/pull/87"),
            Some(87),
            true,
        )
        .unwrap();
        e.rederive_all().unwrap();

        assert_eq!(
            e.db.get_task("gh:o/r#87").unwrap().unwrap().state,
            BoardState::Review
        );
        assert_eq!(
            e.db.pending_writeback_count().unwrap(),
            0,
            "a Linear state name says nothing about a GitHub issue"
        );
    }

    // ---- ids and routing -------------------------------------------------

    #[test]
    fn github_task_ids_split_into_repo_and_number() {
        assert_eq!(
            split_gh_task_id("gh:offhand/tally#87"),
            Some(("offhand/tally".into(), 87))
        );
        // Pull requests too: GitHub's issues endpoints serve both.
        assert_eq!(
            split_gh_task_id("gh:offhand/tally!508"),
            Some(("offhand/tally".into(), 508))
        );
        assert_eq!(split_gh_task_id("linear:LIN-142"), None);
    }

    #[test]
    fn a_pull_request_row_routes_like_its_repo() {
        let e = engine(None);
        e.db.upsert_task(&UpsertTask {
            id: "gh:owner/repo!508".into(),
            source: Source::Github,
            source_id: "u".into(),
            identifier: "gh!508".into(),
            title: "t".into(),
            body: None,
            url: "u".into(),
            labels: vec![],
            source_state: Some("open".into()),
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let t = e.db.get_task("gh:owner/repo!508").unwrap().unwrap();
        assert_eq!(route_context(&t).gh_repo.as_deref(), Some("owner/repo"));
    }

    #[test]
    fn route_context_derives_team_from_a_linear_identifier() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let t = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        let ctx = route_context(&t);
        assert_eq!(ctx.linear_team.as_deref(), Some("LIN"));
        assert_eq!(ctx.labels, vec!["herd"]);
    }

    #[test]
    fn route_context_derives_repo_from_a_github_id() {
        let e = engine(None);
        seed_gh_in(&e, "gh:owner/repo#87");
        let t = e.db.get_task("gh:owner/repo#87").unwrap().unwrap();
        assert_eq!(route_context(&t).gh_repo.as_deref(), Some("owner/repo"));
    }

    // ---- attempt_has_commits ---------------------------------------------

    /// A repo whose default branch is ahead of its remote, which is what made
    /// every dispatch look complete instantly (herdr-board AGE-19).
    ///
    /// Builds a real repo with an "origin" it is one commit ahead of, then asks
    /// the same question two ways: against the remote, and against the commit
    /// the attempt actually started from.
    fn repo_ahead_of_its_remote() -> std::path::PathBuf {
        use std::process::Command;
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "cb-ahead-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let remote = root.join("remote.git");
        let work = root.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let git = |dir: &std::path::Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(&remote)
            .output()
            .unwrap();
        git(&work, &["init", "-b", "main"]);
        git(&work, &["config", "user.email", "t@t"]);
        git(&work, &["config", "user.name", "t"]);
        std::fs::write(work.join("a"), "1").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "base"]);
        git(
            &work,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        git(&work, &["push", "-u", "origin", "main"]);
        // The operator's own unpushed commit — the whole point.
        std::fs::write(work.join("b"), "2").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-m", "operator's unpushed work"]);
        work
    }

    #[test]
    fn a_retry_does_not_inherit_the_cancelled_runs_commits() {
        // herdr-board#10: cancelled after four commits, re-dispatched onto the
        // same branch, marked `done` 62 seconds later while its agent was still
        // working. The base recorded before dispatch is the *repo* HEAD, and
        // the reused branch was already four commits ahead of it.
        let work = repo_ahead_of_its_remote();
        let e = engine(None);
        let wt = work.to_string_lossy().into_owned();
        let sha = |r: &str| {
            String::from_utf8_lossy(
                &std::process::Command::new("git")
                    .args(["-C", &wt, "rev-parse", r])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .trim()
            .to_string()
        };

        // What the first attempt started from, and what it left behind.
        let repo_head_at_first_dispatch = sha("HEAD");
        std::fs::write(work.join("first"), "1").unwrap();
        for args in [
            ["-C", &wt, "add", "."].as_slice(),
            ["-C", &wt, "commit", "-m", "the cancelled run's work"].as_slice(),
        ] {
            std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
        }

        // The retry: same checkout, and the branch tip is now the honest base.
        let base_for_retry = sha("HEAD");
        assert_ne!(base_for_retry, repo_head_at_first_dispatch);
        assert!(
            !e.attempt_has_commits(Some(&wt), Some(&base_for_retry)),
            "a retry that has committed nothing yet must not look finished"
        );
        assert!(
            e.attempt_has_commits(Some(&wt), Some(&repo_head_at_first_dispatch)),
            "measuring from the repo HEAD is what made it look finished — pinned \
             so the regression is recognisable if it returns"
        );
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn an_operators_unpushed_commit_is_not_the_agents_output() {
        let work = repo_ahead_of_its_remote();
        let e = engine(None);
        let wt = work.to_string_lossy().into_owned();

        // Where a dispatch would start from: the repo's HEAD right now.
        let base = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["-C", &wt, "rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        assert!(
            !e.attempt_has_commits(Some(&wt), Some(&base)),
            "an agent that has committed nothing since it started must not look finished"
        );

        // And it still notices real work.
        std::fs::write(work.join("c"), "3").unwrap();
        for args in [
            ["-C", &wt, "add", "."].as_slice(),
            ["-C", &wt, "commit", "-m", "the agent's work"].as_slice(),
        ] {
            std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
        }
        assert!(
            e.attempt_has_commits(Some(&wt), Some(&base)),
            "a commit made after the attempt started is the agent's output"
        );
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    #[test]
    fn without_a_base_the_old_remote_relative_count_is_what_misfires() {
        // Pins the reason base_sha exists: the fallback path reports "the agent
        // produced something" for a repo where nothing has been dispatched at
        // all. Attempts predating the column keep this weaker behaviour, so it
        // is documented rather than fixed.
        let work = repo_ahead_of_its_remote();
        let e = engine(None);
        assert!(
            e.attempt_has_commits(Some(&work.to_string_lossy()), None),
            "the remote-relative count is fooled by unpushed work — this is the bug"
        );
        std::fs::remove_dir_all(work.parent().unwrap()).ok();
    }

    // ---- reclaiming checkouts (gh#72) ------------------------------------

    /// A runtime that records what the sweep asked it to delete, and can be
    /// told to refuse — the two things the gc does to the world outside the
    /// database.
    /// One reclaim call, as the sweep made it: repo, checkout, branch.
    type Reclaimed = (Option<String>, String, Option<String>);

    #[derive(Default)]
    struct Collector {
        reclaimed: std::cell::RefCell<Vec<Reclaimed>>,
        refuse: bool,
    }

    impl Runtime for Collector {
        fn dispatch(&self, _: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
            unreachable!("the gc never dispatches")
        }
        fn prompt(&self, _: &str, _: &str) -> anyhow::Result<()> {
            unreachable!("the gc never talks to a chat")
        }
        fn cancel(&self, _: &str) -> anyhow::Result<()> {
            unreachable!("the gc never cancels")
        }
        fn session(&self, _: &str) -> anyhow::Result<Option<comet_proto::Session>> {
            Ok(None)
        }
        fn chat_alive(&self, _: &str) -> anyhow::Result<bool> {
            Ok(false)
        }
        fn chat_cwd(&self, _: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn last_run_end(&self, _: &str) -> anyhow::Result<Option<RunEnd>> {
            Ok(None)
        }
        fn reclaim_worktree(
            &self,
            repo_path: Option<&str>,
            worktree: &str,
            branch: Option<&str>,
        ) -> anyhow::Result<()> {
            self.reclaimed.borrow_mut().push((
                repo_path.map(str::to_string),
                worktree.to_string(),
                branch.map(str::to_string),
            ));
            if self.refuse {
                anyhow::bail!("git said no");
            }
            Ok(())
        }
    }

    /// A closed attempt with a checkout, on a task whose issue is closed
    /// upstream — the shape everything below varies from.
    fn spent_attempt(e: &SyncEngine) -> i64 {
        seed(e, "linear:LIN-142", "LIN-142", UpstreamState::Terminal);
        let a = dispatch(e, "linear:LIN-142", "chat-9");
        e.db.set_attempt_worktree(a, "/wt/board-lin-142").unwrap();
        e.db.conn
            .execute(
                "UPDATE attempts SET repo_path = '/repo/widget' WHERE id = ?1",
                rusqlite::params![a],
            )
            .unwrap();
        e.db.close_attempt(a, Outcome::Done).unwrap();
        a
    }

    /// Move the retention clock back, as wall time would have.
    fn age_mark(e: &SyncEngine, attempt_id: i64, secs: i64) {
        let stamp = crate::db::rfc3339(chrono::Utc::now() - chrono::Duration::seconds(secs));
        e.db.conn
            .execute(
                "UPDATE attempts SET collectable_at = ?2 WHERE id = ?1",
                rusqlite::params![attempt_id, stamp],
            )
            .unwrap();
    }

    fn attempt_row(e: &SyncEngine, id: i64) -> Attempt {
        e.db.attempts_for("linear:LIN-142")
            .unwrap()
            .into_iter()
            .find(|a| a.id == id)
            .unwrap()
    }

    #[test]
    fn a_finished_attempts_checkout_is_marked_then_collected_a_week_later() {
        let e = engine(None);
        let a = spent_attempt(&e);
        let gc = Collector::default();

        // The sweep that finds it finished only starts the clock.
        e.collect_worktrees(Some(&gc));
        let row = attempt_row(&e, a);
        assert!(row.collectable_at.is_some(), "the clock has to start");
        assert!(row.collected_at.is_none());
        assert!(gc.reclaimed.borrow().is_empty(), "nothing goes on day one");

        // Six days in, still nothing.
        age_mark(&e, a, 6 * 86_400);
        e.collect_worktrees(Some(&gc));
        assert!(gc.reclaimed.borrow().is_empty(), "the window is a week");

        age_mark(&e, a, 7 * 86_400);
        e.collect_worktrees(Some(&gc));
        assert_eq!(
            gc.reclaimed.borrow().as_slice(),
            [(
                Some("/repo/widget".to_string()),
                "/wt/board-lin-142".to_string(),
                Some("board/lin-142".to_string()),
            )],
            "the repo, the checkout and the branch the board cut"
        );
        assert!(attempt_row(&e, a).collected_at.is_some());

        // And it is not offered again — the row says it is gone.
        e.collect_worktrees(Some(&gc));
        assert_eq!(gc.reclaimed.borrow().len(), 1);
    }

    #[test]
    fn a_live_attempt_is_never_collected_however_finished_the_task_looks() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Terminal);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_attempt_worktree(a, "/wt/board-lin-142").unwrap();
        let gc = Collector::default();

        // However long the board has been up, an agent's checkout is untouched
        // and its clock never even starts.
        for _ in 0..3 {
            e.collect_worktrees(Some(&gc));
            age_mark(&e, a, 30 * 86_400);
        }
        assert!(gc.reclaimed.borrow().is_empty());
    }

    #[test]
    fn a_retry_dispatched_beside_a_marked_attempt_stops_the_clock() {
        // The retry reuses the branch and lands in the same directory, so the
        // closed attempt's checkout is the live one's. Deleting it would throw
        // away the commits the retry is meant to continue.
        let e = engine(None);
        let a = spent_attempt(&e);
        let gc = Collector::default();
        e.collect_worktrees(Some(&gc));
        age_mark(&e, a, 30 * 86_400);

        dispatch(&e, "linear:LIN-142", "chat-10");
        e.collect_worktrees(Some(&gc));
        assert!(gc.reclaimed.borrow().is_empty(), "a live retry holds it");
        assert!(
            attempt_row(&e, a).collectable_at.is_none(),
            "and the window starts over when the retry ends"
        );
    }

    #[test]
    fn a_pull_request_back_in_review_stops_the_clock() {
        let e = engine(None);
        let a = spent_attempt(&e);
        let gc = Collector::default();
        e.collect_worktrees(Some(&gc));
        assert!(attempt_row(&e, a).collectable_at.is_some());

        e.db.conn
            .execute(
                "UPDATE tasks SET pr_open = 1 WHERE id = 'linear:LIN-142'",
                [],
            )
            .unwrap();
        age_mark(&e, a, 30 * 86_400);
        e.collect_worktrees(Some(&gc));
        assert!(
            gc.reclaimed.borrow().is_empty(),
            "review holds the checkout"
        );
        assert!(attempt_row(&e, a).collectable_at.is_none());
    }

    #[test]
    fn retention_off_keeps_every_checkout() {
        let mut e = engine(None);
        e.cfg.defaults.retain_worktrees = "off".into();
        let a = spent_attempt(&e);
        let gc = Collector::default();
        e.collect_worktrees(Some(&gc));
        assert!(
            attempt_row(&e, a).collectable_at.is_none(),
            "the clock never starts"
        );
        // Not even one left over from before the operator turned it off.
        age_mark(&e, a, 365 * 86_400);
        e.collect_worktrees(Some(&gc));
        assert!(gc.reclaimed.borrow().is_empty());
    }

    #[test]
    fn a_removal_that_fails_is_tried_again_next_cycle() {
        // The stamp is what says the disk space is back. A `git worktree
        // remove` that failed must leave the row uncollected, or the board
        // would report space it never reclaimed.
        let e = engine(None);
        let a = spent_attempt(&e);
        let gc = Collector {
            refuse: true,
            ..Default::default()
        };
        e.collect_worktrees(Some(&gc));
        age_mark(&e, a, 30 * 86_400);
        e.collect_worktrees(Some(&gc));
        assert!(attempt_row(&e, a).collected_at.is_none());
        e.collect_worktrees(Some(&gc));
        assert_eq!(gc.reclaimed.borrow().len(), 2, "and again the cycle after");
    }

    #[test]
    fn a_cycle_without_a_runtime_marks_but_never_deletes() {
        // Only the process that owns the worktrees may remove one; anything
        // else running the cycle is welcome to keep the clock.
        let e = engine(None);
        let a = spent_attempt(&e);
        e.collect_worktrees(None);
        age_mark(&e, a, 30 * 86_400);
        e.collect_worktrees(None);
        assert!(attempt_row(&e, a).collectable_at.is_some());
        assert!(attempt_row(&e, a).collected_at.is_none());
    }

    // ---- the full cycle --------------------------------------------------

    #[test]
    fn sync_once_without_a_snapshot_skips_reconciliation() {
        // The engine boots before its first session-watch snapshot arrives. A
        // missing snapshot must read as "no information", not "every chat is
        // missing" — the difference between skipping a tick and counting one
        // against every live attempt.
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();

        for _ in 0..5 {
            e.sync_once(None).unwrap();
        }
        let a = live(&e);
        assert!(a.outcome.is_none());
        assert_eq!(a.missing_ticks, 0, "no snapshot, no ticks");
        assert!(
            e.db.meta_get(meta::LAST_SYNC).unwrap().is_some(),
            "the cycle itself still ran"
        );
    }

    #[test]
    fn sync_once_with_a_snapshot_reconciles_and_derives() {
        let e = engine(None);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        e.sync_once(Some(&statuses(&[("chat-9", AgentStatus::Blocked)])))
            .unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Blocked
        );
    }
}
