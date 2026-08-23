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
//! Settle decisions (§settle-logic) key off run-journal events: a run ending
//! is a recorded fact, so "the turn ended, now check the checkout" needs no
//! debounce clock. The decision itself is [`crate::settled::decide`]; the
//! artifact checks and the wrongly-settled rewatch live here, on both the
//! interval reconcile (catch-up) and the event path (the moment the run ends).
//!
//! ## What is deliberately NOT here yet
//!
//! - **Unadopted detection** — lives in [`crate::adopt`] (§adopt-doctor-init),
//!   walking comet spaces on demand from `comet-board adopt` / `doctor` rather
//!   than on the sync cycle; §board-view decides whether a periodic sweep
//!   earns a place here.

use crate::config::{Credentials, Paths, RouteContext, RoutingConfig};
use crate::credential_ledger;
use crate::db::{Db, NewAttempt, NewWriteback, Reaped};
use crate::dispatch::RanOn;
use crate::gc;
use crate::log::Logger;
use crate::model::*;
use crate::notify::{self, Signal, Stopped, Webhook};
use crate::overrun;
use crate::rebased;
use crate::runs;
use crate::runtime::{RunEnd, Runtime};
use crate::settled::{self, Commits, Evidence, Verdict, Why};
use crate::sources::github::{
    AsUser, Github, HttpAsUser, HttpRest, MergeStatus, PullRequest, PushCapabilities, Rest,
    pr_matches_branch,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;

/// How long the board waits between asking whether an in-flight merge is done.
///
/// GitHub's own advice for the asynchronous merge, and the reason it is a
/// second rather than the poll interval: this is a person standing in front of
/// a confirmation they just gave.
const MERGE_POLL_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// How many of those to spend before answering "still merging".
///
/// A stack merge is documented as taking up to a few minutes, and this is
/// nowhere near that on purpose. The board is not the only thing watching — the
/// sync loop sees the merge on its next pass either way — so the budget is set
/// by how long a keypress may block, not by how long GitHub may take.
const MERGE_POLL_TRIES: usize = 20;

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

/// What became of one agent-facing notice — see [`SyncEngine::announce`],
/// which routes on it.
///
/// Not a bool, because the hop to the next addressee and the log line for an
/// event nobody heard both need to know *why* a channel came up empty
/// (gh#165) — an unpinned board and an unreachable pin are different things to
/// go and fix. Only [`Told::Yes`] stops the hop: a dispatcher that cannot be
/// told is not a dispatcher that was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Told {
    /// Prompted into that chat. Nobody after it in the chain hears this event.
    Yes,
    /// There is no chat in this role: nothing agent-released the work, or no
    /// orchestrator is pinned. Not a failure — an empty address.
    NoOne,
    /// There is a chat and it could not be prompted — archived, gone, or the
    /// runtime refused. The reason is already in the log where it happened.
    Unreachable,
    /// The channel is switched off in config. Only `notify_dispatcher` has a
    /// switch; the pin's absence is [`Told::NoOne`].
    Off,
    /// The chat in this role is the attempt's own, so prompting it would be
    /// the agent being told about itself. Reachable only through a hand-set
    /// `--via`, or a pin on a board-dispatched chat.
    Itself,
}

/// Agent statuses for this cycle, keyed by chat id (the id stored in
/// `Attempt::pane_id`). Built by the engine's board service from the session
/// watch through [`crate::runtime::agent_status`]. A chat absent from the map
/// has no session row, which is [`AgentStatus::Missing`].
pub type SessionStatuses = HashMap<String, AgentStatus>;

/// The credential verdict applied when work is present on origin at settle.
/// Kept as one seam so end-to-end push tests can assert the exact production
/// decision rather than reimplementing Handed/Minted interpretation.
pub fn credential_is_sanctioned_at_settle(paths: &Paths, chat: &str) -> bool {
    !credential_ledger::for_chat(paths, chat).unsanctioned()
}

pub struct SyncEngine {
    pub db: Db,
    pub cfg: RoutingConfig,
    pub credentials: Credentials,
    pub paths: Paths,
    pub log: Arc<Logger>,
    pub github: Option<Github<Box<dyn Rest>>>,
    /// A pre-resolved push capability supplied by an embedding that already
    /// established the credential out of band. `None` is the production path:
    /// dispatch probes the configured GitHub credential immediately before it
    /// creates an attempt. The engine's offline board tests supply the known
    /// capability their fake runtime models, so they do not make network calls
    /// or turn an intentionally credential-free fixture into a false failure.
    pub push_capabilities: Option<PushCapabilities>,
    /// How a verdict is cast under the *reviewer's* identity rather than the
    /// board's (gh#369). A field for the same reason [`Self::webhook`] is one:
    /// which credential the review went out under is exactly what a test of
    /// this needs to watch, and it must not take a socket to see it.
    ///
    /// `Rc` rather than `Arc`, unlike the webhook: what it hands out is a
    /// [`Github`] over a `Box<dyn Rest>`, which is this engine's own and goes
    /// nowhere else.
    pub as_user: Rc<dyn AsUser>,
    /// Where `[defaults] notify_webhook` is POSTed (gh#71). A field rather than
    /// a call into `notify::HttpWebhook` so a test can watch what the board
    /// would have sent without one listening socket in the suite.
    pub webhook: Arc<dyn Webhook>,
}

/// Meta keys. Kept together so readers and the engine's loop cannot drift.
pub mod meta {
    pub const LAST_SYNC: &str = "last_sync";
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
    /// When an attempt's checkout was first kept for a layer stacked on it
    /// (gh#286) — the same once-per-attempt discipline, for the same reason.
    pub fn dependents_noted(attempt: i64) -> String {
        format!("dependents:{attempt}")
    }
    /// When an attempt's chat was first kept on its shelf because it had
    /// released work still running (gh#354) — the same once-per-attempt
    /// discipline, for the same reason.
    pub fn dispatcher_noted(attempt: i64) -> String {
        format!("dispatcher:{attempt}")
    }
    /// The head origin held when the board told an attempt its branch had been
    /// rewritten under it (gh#286). Holds the sha rather than a timestamp: it
    /// is both the "said already" mark and the thing a *second* rewrite is
    /// recognised against.
    pub fn rewritten_noted(attempt: i64) -> String {
        format!("rewritten:{attempt}")
    }
    /// What became of the last stack call the board made (gh#387) —
    /// [`crate::stacks::Asked`], which is both the "asked already" mark and the
    /// retry budget. Keyed by [`crate::stacks::StackWork::signature`] rather
    /// than by an attempt: two chains can share a bottom layer, and what a
    /// budget is spent on is a *request*.
    pub fn stack_asked(signature: &str) -> String {
        format!("stack:{signature}")
    }
    /// What the last settle notice about an attempt asserted (§gh#356) —
    /// [`crate::notify::Signal::settle_print`]. Like `rewritten:`, it holds the
    /// claim rather than a timestamp: it is both the "said already" mark and
    /// the thing the *next* settle is recognised against.
    pub fn settle_announced(attempt: i64) -> String {
        format!("settled:{attempt}")
    }
    /// What origin held for an attempt's branch when the attempt began
    /// (gh#489) — the commit the settle's credential check compares against,
    /// so an attempt that changed nothing on origin is never accused over a
    /// branch a previous attempt pushed. Absent when the branch was not on
    /// origin at dispatch, when nothing could answer, or when the attempt
    /// predates the stamp — all of which read the same way: any work on
    /// origin at settle is new, which is the accusation's old footing.
    pub fn origin_at_start(attempt: i64) -> String {
        format!("origin_start:{attempt}")
    }
}

/// Which `routing.toml` sections differ, for the reload's log line: an
/// operator reading `syncd.log` wants to know *what* moved, and "the file
/// changed" does not say.
///
/// Narration only. The rebuild decision is whole-struct equality, so a section
/// missing from this list costs a vague log line and never a stale loop — the
/// opposite of the hand-maintained comparison gh#189 was about. Returns `none`
/// when only a credential changed.
fn changed_sections(before: &RoutingConfig, after: &RoutingConfig) -> String {
    let mut out = Vec::new();
    if before.sync != after.sync {
        out.push("sync");
    }
    if before.routes != after.routes {
        out.push("routes");
    }
    if before.defaults != after.defaults {
        out.push("defaults");
    }
    if before.github != after.github {
        out.push("github");
    }
    if before.adopt != after.adopt {
        out.push("adopt");
    }
    if before.users != after.users {
        out.push("users");
    }
    if before.automations != after.automations {
        out.push("automations");
    }
    if out.is_empty() {
        // Either nothing in the file moved, or a section was added to the
        // struct and not to this list. Both read the same way on purpose:
        // vague, and never a reason not to rebuild.
        return "none".into();
    }
    out.join("+")
}

/// What re-opening a merged branch produced, when it did (gh#563).
struct Reopened {
    url: String,
    /// The clause the settle notice carries — what happened, how much was
    /// stranded, and where the work lives now.
    clause: String,
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
            github,
            push_capabilities: None,
            as_user: Rc::new(HttpAsUser),
            webhook: Arc::new(crate::notify::HttpWebhook),
        })
    }

    /// Rebuild when credentials or `routing.toml` change on disk.
    ///
    /// The loop outlives the setup: an edit to the file should take effect on
    /// the next cycle rather than requiring an engine restart nobody would
    /// think to do. Returns `None` when nothing changed.
    ///
    /// The config test is the **whole** [`RoutingConfig`], not a list of the
    /// fields worth noticing. It was such a list once — two credentials, the
    /// repo list, and the route *count* — and each comparison was right when it
    /// was written; the set stopped being complete the moment a `[defaults]`
    /// key mattered (gh#189). Every key under `[defaults]` was invisible to a
    /// running board, and so was editing a route in place, because the count
    /// did not move. That is worse than a stale flag: all three surfaces that
    /// write those keys — `comet-board routes defaults`, the desktop's
    /// `WriteBoardConfig`, and the iOS orchestrator pin — report the file, so
    /// the operator was told a change had landed while the loop that had to act
    /// on it kept the config it booted with. A derived `PartialEq` cannot fall
    /// behind the struct it is derived from.
    ///
    /// Parsed-config equality rather than a hash of the bytes, so reformatting
    /// or a changed comment is not a rebuild — and an untouched file is not one
    /// either, which is what keeps an idle board from rebuilding every cycle.
    pub fn reload_if_configuration_changed(&self) -> Option<SyncEngine> {
        let credentials = Credentials::load(&self.paths);
        let cfg = RoutingConfig::load_or_default(&self.paths.routing());

        // The whole GitHub credential, not just the token: registering an App
        // over a running board is exactly the change this exists to notice, and
        // comparing only `github_token` would leave it polling as the old
        // identity until somebody restarted the engine (gh#58).
        let github_changed = credentials.github_auth() != self.credentials.github_auth();
        let config_changed = cfg != self.cfg;
        if !(github_changed || config_changed) {
            return None;
        }
        self.log.info(format!(
            "configuration changed (github credential:{github_changed} \
             routing.toml:{}) — rebuilding",
            changed_sections(&self.cfg, &cfg)
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
    /// consults for run-journal facts (§settle-logic). `None` — the read-only
    /// callers — still settles on `Idle`, but leaves an errored end
    /// unrecognised.
    pub fn sync_once_with(
        &self,
        statuses: Option<&SessionStatuses>,
        runtime: Option<&dyn Runtime>,
    ) -> Result<Vec<PullRequest>> {
        let pulls = self.poll_github();
        // Straight after the poll that linked them: a chain `--onto` cut
        // becomes a stack GitHub will take at the moment its last pull request
        // turns up, and this cycle is the one that just saw it (gh#387).
        self.link_dispatched_stacks();
        if let Some(runtime) = runtime
            && let Err(e) = self.adopt_session_pull_requests(&pulls, runtime)
        {
            self.log
                .warn(format!("linking Comet chats to pull requests: {e:#}"));
        }

        if let Some(statuses) = statuses {
            self.reconcile_sessions_with(statuses, runtime)?;
        }
        self.rederive_all()?;
        // After reconciliation, so a checkout freed this cycle starts its
        // retention clock this cycle; on the interval only, like every other
        // clocked decision (gh#72).
        self.collect_worktrees(runtime);
        // Then the cache *inside* the checkouts, on its own much shorter clock
        // (gh#186): a checkout is 14 MB of evidence and its `target/` is 36 GB
        // of regenerable, and one window for both keeps the expensive thing for
        // as long as the cheap one. After `collect_worktrees` so a checkout
        // reclaimed whole this cycle is not walked for a cache that went with it.
        self.sweep_build_output(runtime);
        // Beside them, on the same clock and the same rule: the three are one
        // attempt's leavings, and a box that reclaimed the checkout while
        // keeping the chat forever would have tidied half the mess (gh#139).
        self.archive_chats(runtime);
        // And the checkouts nobody is reclaiming, because they are stacked on a
        // layer that has just landed: GitHub rewrote their branches on the
        // server and this box holds the history from before it (gh#286). On the
        // interval, after the poll that would have seen the merge.
        self.note_rewritten_branches(runtime);
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
        // legacy Linear task names no repo, so its branch is honoured in
        // whichever repo the PR turns up in.
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
                if !self.should_import_pull_request_row(&attempt_branches, pr) {
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
                // Free: the pull list already carried all of them (gh#282,
                // gh#344).
                let stack = pr.stack.as_ref();
                let _ = self.db.set_pr_topology(
                    &pr.task_id(),
                    Some(&pr.base_ref),
                    Some(&pr.head_ref),
                    stack,
                );
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
            if check_mergeable {
                // The stamp that spaces the expensive per-PR calls out. It
                // lived on the Linear poll until gh#471; the clock is this
                // poll's now.
                let _ = self.db.meta_set(meta::LAST_FULL_SWEEP, &crate::db::now());
            }
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

    /// A dispatched issue owns its PR, so the PR is not duplicated as `gh!…`.
    /// An ordinary Comet chat is different: its adopted attempt lives on the
    /// `gh!…` row itself, and that row must continue to be refreshed and marked
    /// seen on every poll rather than disappearing behind its own branch.
    fn should_import_pull_request_row(
        &self,
        attempt_branches: &AttemptBranches,
        pr: &PullRequest,
    ) -> bool {
        if !attempt_branches.claims(pr) {
            return true;
        }
        self.db
            .get_task(&pr.task_id())
            .ok()
            .flatten()
            .is_some_and(|task| {
                task.attempts
                    .iter()
                    .any(|attempt| attempt.branch.as_deref() == Some(pr.head_ref.as_str()))
            })
    }

    /// Give a directly-created Comet PR the same attempt-backed review model
    /// as a Board dispatch.
    ///
    /// GitHub supplies the immutable comparison base; Comet supplies the chat,
    /// checkout and branch. Repository + branch is the ownership proof already
    /// used for dispatched attempts. If several chats share a checkout, an
    /// exact PR URL or the `gh pr create` command is required; recency alone is
    /// not authorship and must never route feedback into the wrong conversation.
    fn adopt_session_pull_requests(
        &self,
        pulls: &[PullRequest],
        runtime: &dyn Runtime,
    ) -> Result<()> {
        let candidates = runtime.review_candidates()?;
        for pr in pulls.iter().filter(|pr| pr.open) {
            let task_id = pr.task_id();
            let Some(task) = self.db.get_task(&task_id)? else {
                // A dispatched branch is represented by its issue row and is
                // deliberately not imported as a separate PR task.
                continue;
            };
            let branch_is_unique = pulls
                .iter()
                .filter(|other| other.open && other.head_ref == pr.head_ref)
                .count()
                == 1;
            // An exact URL means this conversation adopted this exact PR. It
            // outranks repository inference: a fork's origin is the head repo,
            // and a remote chat may not expose any repo metadata to this host.
            let mut explicit: Vec<_> = candidates
                .iter()
                .filter(|candidate| candidate.pull_request_urls.iter().any(|url| url == &pr.url))
                .collect();
            explicit.sort_by_key(|candidate| candidate.created_at);
            let matching: Vec<_> = candidates
                .iter()
                .filter(|candidate| candidate.branch.as_deref() == Some(pr.head_ref.as_str()))
                .filter(|candidate| {
                    candidate.repo.as_deref().is_some_and(|repo| {
                        repo.eq_ignore_ascii_case(&pr.repo)
                            || pr
                                .head_repo
                                .as_deref()
                                .is_some_and(|head| repo.eq_ignore_ascii_case(head))
                    }) || (candidate.repo.is_none() && branch_is_unique)
                })
                .collect();
            let creators: Vec<_> = matching
                .iter()
                .copied()
                .filter(|candidate| candidate.created_pull_request)
                .collect();
            let author = explicit
                .last()
                .copied()
                .or_else(|| (creators.len() == 1).then(|| creators[0]))
                .or_else(|| (matching.len() == 1).then(|| matching[0]));
            // Review eligibility and author attribution are separate. Several
            // matching chats with no proof still establish that this is Comet
            // work, so create an attempt with no chat rather than guessing.
            let candidate = author.or_else(|| {
                matching
                    .iter()
                    .copied()
                    .max_by_key(|candidate| candidate.created_at)
            });

            if let Some(existing) = task.attempts.iter().rev().find(|attempt| {
                !attempt.board_managed && attempt.branch.as_deref() == Some(pr.head_ref.as_str())
            }) {
                let mut attempt = existing.clone();
                let mut relinked = false;
                if let Some(author) = author {
                    if attempt.pane_id.is_none() {
                        self.db.set_attempt_pane(attempt.id, &author.chat_id)?;
                        relinked = true;
                    }
                    if attempt.worktree.is_none()
                        && author.branch.as_deref() == Some(pr.head_ref.as_str())
                        && let Some(worktree) = author.worktree.as_deref()
                    {
                        self.db.set_attempt_worktree(attempt.id, worktree)?;
                        relinked = true;
                    }
                }
                if relinked {
                    attempt = self.db.get_attempt(attempt.id)?.ok_or_else(|| {
                        anyhow::anyhow!("adopted attempt {} disappeared", attempt.id)
                    })?;
                }
                // Direct agents often open a PR and continue working. Keep the
                // review snapshot current instead of freezing it at first sight.
                self.harvest_claims(Some(runtime), &attempt);
                self.record_tokens(Some(runtime), &attempt);
                self.record_context(Some(runtime), &attempt);
                self.record_review_facts(Some(runtime), &attempt);
                continue;
            }
            if !task.attempts.is_empty() {
                continue;
            }
            let Some(candidate) = candidate else {
                continue;
            };
            let author_chat_id = author.map(|author| author.chat_id.clone());
            let worktree = author
                .filter(|author| author.branch.as_deref() == Some(pr.head_ref.as_str()))
                .and_then(|author| author.worktree.clone());

            let attempt_id = self.db.insert_adopted_attempt(&NewAttempt {
                automation: None,
                automation_owner: None,
                stacked_on: None,
                task_id: task_id.clone(),
                pane_id: author_chat_id.clone(),
                workspace: candidate.workspace.clone(),
                runtime: candidate.runtime.clone(),
                worktree,
                repo_path: None,
                branch: Some(pr.head_ref.clone()),
                dispatched_by: None,
                dispatched_by_pane: None,
                base_sha: pr.base_sha.clone(),
                account: candidate.account.clone(),
                dispatched_by_device: None,
                dispatched_by_user: None,
                dispatched_by_verified: false,
                billed_to: None,
            })?;
            let attempt = self
                .db
                .get_attempt(attempt_id)?
                .ok_or_else(|| anyhow::anyhow!("adopted attempt {attempt_id} disappeared"))?;
            self.harvest_claims(Some(runtime), &attempt);
            self.record_tokens(Some(runtime), &attempt);
            self.record_context(Some(runtime), &attempt);
            self.record_review_facts(Some(runtime), &attempt);
            self.db.close_attempt(attempt_id, Outcome::Done)?;
            match author_chat_id {
                Some(chat_id) => self.log.info(format!(
                    "{}: linked {} to Comet chat {chat_id} — review available",
                    task.identifier, pr.url
                )),
                None => self.log.info(format!(
                    "{}: {} belongs to several possible Comet chats — review available, author unresolved",
                    task.identifier, pr.url
                )),
            }
        }
        Ok(())
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
            // derived it straight to review (herdr-board AGE-20). This scope is
            // the whole of that fix, and it is why gh#364 could spend the
            // branch's repo half on the title: the branch never carried the
            // answer, the task id did. Linear identifiers are globally unique,
            // so Linear rows need no such scoping.
            let own_repo = crate::model::gh_repo(&task.id);
            let Some(link) = link_for(pulls, &branches, own_repo) else {
                continue;
            };
            let pr = link.pr;
            // First sight of this request on this row — the moment the ask in
            // the brief (gh#548) is checkable, because the body exists and the
            // board knows which issue it was supposed to close.
            let fresh_request = task.pr_url.as_deref() != Some(pr.url.as_str());
            self.db
                .set_pr(&task.id, Some(&pr.url), Some(pr.number), link.open)?;
            // A Linear issue's PR is topologically a PR like any other, and 7/9
            // reads the base off the task row whichever source put it there.
            self.db.set_pr_topology(
                &task.id,
                Some(&pr.base_ref),
                Some(&pr.head_ref),
                pr.stack.as_ref(),
            )?;
            // Observing the merge is the same fact as performing it. A PR
            // merged with `gh pr merge` or on the web must not leave its
            // ticket in review forever — and not merely unadvanced: nothing
            // but a finished task ends `review`, so the state kept standing
            // and the review writeback kept asserting it, which is what
            // pulled hand-closed tickets back (herdr-board AGE-21/22). The
            // idempotency key stops the next poll re-sending it.
            //
            // For a stack it is the *stack* that has to have merged (gh#287):
            // the bottom layer merges first and merging it finishes nothing,
            // and a task closed on it would close its issue and let the GC take
            // the checkout the layers above are still being written in.
            if link.merged && !task.pr_merged && !task.state.is_terminal() {
                self.finish_on_merge(&task, &pr.repo, pr.number)?;
            }
            self.db.set_pr_merged(&task.id, link.merged)?;
            // The board's own verification of the ask (gh#548). Writeback
            // closes the issue on settle, so the loop looks whole either way —
            // but that is the board doing it, not GitHub: a request whose body
            // carries nothing GitHub's parser acts on shows no linked issue and
            // closes nothing when merged by a human outside the board. Warned
            // once per request, at first sight, rather than every poll — the
            // log is where an operator or a follow-up dispatch will see it, and
            // a body fixed later is honoured silently by the same check.
            if fresh_request
                && link.open
                && let Some(issue) = crate::model::gh_number(&task.id)
                && !crate::closing::parses_as_closing(pr.body.as_deref(), issue)
            {
                self.log.warn(format!(
                    "{}: {} announces closure in prose GitHub cannot parse — \
                     merging it closes nothing on GitHub. Edit the pull request \
                     body to put `Closes #{issue}` on its own line.",
                    task.identifier, pr.url
                ));
            }
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
    /// consumes (§settle-logic). The ranking of what they measure is
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

    /// The commit every measurement of this attempt's branch is taken from
    /// (gh#286): the stamp while it still describes the branch, and the fork
    /// point when the history has been rewritten under it.
    ///
    /// `base_sha` is stamped once, from the checkout's HEAD at dispatch, and it
    /// is right for as long as nothing rewrites that branch. A stacked layer is
    /// where something does: the layer below lands, GitHub rebases this one
    /// server-side, and the next `gh stack sync` or `git pull --rebase` in the
    /// checkout moves the local branch off the stamp. From that moment
    /// `<stamp>..HEAD` is not this attempt's work — it is the layer below's
    /// commits plus this one's, and every reader of it (the settle's commit
    /// count, the review's changed files, the claims' remainder) is being told
    /// the child wrote the parent's diff.
    ///
    /// So the stamp is checked rather than trusted, and re-stamped from the
    /// checkout when it fails: an ancestry test says whether the branch still
    /// starts there, and a merge base against the branch's *current* base says
    /// where it starts now. See [`crate::rebased`] for why that evidence and
    /// never the observed retarget — a pull request moved on GitHub says
    /// nothing about a local branch that has not moved.
    ///
    /// `None` where there is nothing honest to measure from: no stamp recorded
    /// (attempts from before the column existed), or a rewritten branch whose
    /// footing cannot be recovered. Callers keep whatever weaker fallback they
    /// had for that case — never the stale stamp.
    pub fn attempt_base(&self, attempt: &Attempt) -> Option<String> {
        let stamp = attempt.base_sha.as_deref()?;
        let worktree = attempt.worktree.as_deref()?;
        if !std::path::Path::new(worktree).exists() {
            // Nothing to check it against. The stamp is the record of what the
            // dispatch cut from, and the callers that need a checkout are
            // already refusing.
            return Some(stamp.to_string());
        }
        let on_head = git_ok(worktree, &["merge-base", "--is-ancestor", stamp, "HEAD"]);
        if on_head {
            return Some(stamp.to_string());
        }
        let fork = self.fork_point(worktree, attempt);
        match rebased::footing(stamp, on_head, fork.as_deref()) {
            rebased::Footing::Stands => Some(stamp.to_string()),
            rebased::Footing::Refooted(fork) => {
                if let Err(e) = self.db.set_attempt_base_sha(attempt.id, &fork) {
                    self.log
                        .warn(format!("re-stamping attempt {}: {e:#}", attempt.id));
                }
                self.log.info(format!(
                    "attempt {}: {} was rebased under its recorded base {} — \
                     measuring from {fork} instead",
                    attempt.id,
                    attempt.branch.as_deref().unwrap_or("its branch"),
                    &stamp[..stamp.len().min(12)],
                ));
                Some(fork)
            }
            rebased::Footing::Adrift => {
                self.log.info(format!(
                    "attempt {}: base {} is no longer on {}, and no base branch \
                     names where it starts now",
                    attempt.id,
                    &stamp[..stamp.len().min(12)],
                    attempt.branch.as_deref().unwrap_or("its branch"),
                ));
                None
            }
        }
    }

    /// Where git says this branch forks from the base it is on *now*.
    ///
    /// Three candidates, freshest first, and each one is a remote-tracking ref
    /// because a local branch of the same name is exactly the stale answer
    /// gh#67 refused at dispatch:
    ///
    /// 1. the base GitHub last reported for the pull request — the retarget, if
    ///    one has happened;
    /// 2. the branch of the attempt this one was stacked on (gh#285), which is
    ///    the answer while the layer below is still open;
    /// 3. `origin/HEAD`, the trunk, for everything that is not in a stack.
    fn fork_point(&self, worktree: &str, attempt: &Attempt) -> Option<String> {
        let task = self.db.get_task(&attempt.task_id).ok().flatten();
        let parent_branch = attempt
            .stacked_on
            .and_then(|id| self.db.get_attempt(id).ok().flatten())
            .and_then(|parent| parent.branch);
        let candidates = task
            .and_then(|t| t.pr_base_ref)
            .into_iter()
            .chain(parent_branch)
            .map(|b| format!("origin/{b}"))
            .chain(std::iter::once("origin/HEAD".to_string()));
        for base in candidates {
            if let Some(fork) =
                git_out(worktree, &["merge-base", &base, "HEAD"]).filter(|sha| !sha.is_empty())
            {
                return Some(fork);
            }
        }
        None
    }

    /// What this attempt's commits amount to (gh#69): nothing, work only its
    /// own box can see, or work on origin.
    ///
    /// [`SyncEngine::attempt_has_commits`] answers the first half — is there
    /// anything past the base — and this adds the half that decides whether a
    /// settle may call it reviewable. `ask_github` allows the second, remote
    /// tier of that check; see [`SyncEngine::commits_are_on_origin`].
    pub fn attempt_commits(&self, task: &Task, attempt: &Attempt, ask_github: bool) -> Commits {
        if !self.attempt_has_commits(
            attempt.worktree.as_deref(),
            self.attempt_base(attempt).as_deref(),
        ) {
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

    /// The commit origin's copy of `branch` points at right now, by the
    /// strongest evidence on hand: GitHub asked directly when the board has a
    /// client, else the checkout's remote-tracking ref. GitHub first because
    /// the tracking ref can be stale in exactly the direction that matters —
    /// a push made straight to a URL (what gh#68's credential path can hand
    /// an agent) moves origin and no local ref at all.
    ///
    /// `None` is "no answer": the branch is not on origin, or nothing could
    /// say. The two callers — the start-of-attempt stamp and the settle's
    /// credential check — both treat that as a fact to leave unrecorded
    /// rather than one to invent.
    fn origin_branch_oid(
        &self,
        task: &Task,
        worktree: Option<&str>,
        branch: &str,
    ) -> Option<String> {
        let worktree = worktree.filter(|w| std::path::Path::new(w).exists());
        if let Some(gh) = self.github.as_ref()
            && let Some(repo) = crate::model::gh_repo(&task.id)
                .map(str::to_string)
                .or_else(|| task.pr_url.as_deref().and_then(crate::model::pr_repo))
                .or_else(|| worktree.and_then(crate::git_credentials::repo_for_checkout))
            && let Some(oid) = gh.branch_head(&repo, branch)
        {
            return Some(oid);
        }
        git_out(
            worktree?,
            &["rev-parse", &format!("refs/remotes/origin/{branch}")],
        )
        .filter(|oid| !oid.is_empty())
    }

    /// Record what origin already holds for this attempt's branch, as the
    /// attempt begins (gh#489).
    ///
    /// A retry or repair run lands on a branch a previous attempt pushed, so
    /// "the branch is on origin" is true before its agent types a word.
    /// Without this stamp the settle read that pre-existing state as proof
    /// the run pushed — and, finding no mint in the ledger, accused a
    /// credential path that was rightly never used. The stamp is what lets
    /// [`SyncEngine::note_credential_path`] bind its accusation to an origin
    /// that *moved* during the attempt.
    ///
    /// Best effort, like the ledger itself: a stamp that cannot be taken or
    /// written costs the quiet path, never the dispatch — the settle falls
    /// back to its old, loud footing.
    pub fn stamp_origin_at_start(
        &self,
        task: &Task,
        attempt_id: i64,
        worktree: &str,
        branch: &str,
    ) {
        let Some(oid) = self.origin_branch_oid(task, Some(worktree), branch) else {
            return;
        };
        if let Err(e) = self.db.meta_set(&meta::origin_at_start(attempt_id), &oid) {
            self.log.warn(format!(
                "{}: stamping origin's {branch} at attempt start: {e:#}",
                task.identifier
            ));
        }
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
    ///   has lost its run, and [`Runtime::chat_alive`] says which of the two
    ///   things that is (§gh#390): a chat that is gone orphans the attempt on
    ///   the same two-consecutive-ticks rule herdr-board used, and a chat that
    ///   is still there is an interrupted run, which is resumed in place — same
    ///   attempt, same chat, same branch. The distinction is the whole of
    ///   gh#390: session rows persist as `Idle` after a run ends *within one
    ///   engine process*, and an engine restart rebuilds the mirror empty, so
    ///   the old reading closed every live attempt on the box as `orphaned`
    ///   while every one of their chats sat intact on its shelf.
    /// - A chat that has *never* had a session row is indistinguishable from a
    ///   dispatch whose first run has not started yet (the brief sits in the
    ///   command ledger until the host device executes it). Ticks are counted
    ///   for observability, but the verdict needs `Runtime::chat_alive` —
    ///   §runtime-impl. Nothing is orphaned on absence-of-evidence alone.
    /// - Settling (§settle-logic): a live attempt whose chat's last run has
    ///   ended is checked for artifacts — see [`SyncEngine::maybe_settle`].
    ///   This is the catch-up path (the event path settles the moment the run
    ///   ends); it makes no fresh PR lookup because the cycle polled seconds
    ///   ago.
    /// - Re-opening (§settle-logic's inverse): a settled attempt whose chat is
    ///   working again — [`SyncEngine::rewatch_settled_attempts`].
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
        // Every run this pass found dead in a chat that is still alive
        // (§gh#390). Collected rather than announced one at a time, because
        // what happened to six of them at once is one event — an engine
        // restart — and six separate notices are how that event stayed
        // invisible while every one of its casualties was reported.
        let mut interrupted: Vec<notify::Interrupted> = Vec::new();
        for attempt in self.db.live_attempts()? {
            let Some(chat_id) = attempt.pane_id.as_deref() else {
                // Dispatch is still in flight; nothing to reconcile yet.
                continue;
            };
            let Some(task) = self.db.get_task(&attempt.task_id)? else {
                continue;
            };
            // Before anything can decide the attempt is over: every branch
            // below can be the last time this row is seen live — settled,
            // orphaned, capped — and a total recorded after the close would
            // never be recorded at all (gh#151).
            self.record_tokens(runtime, &attempt);
            // …and for how full its window is (gh#271), which is a live-attempt
            // fact above all: an attempt that ends here keeps the last level
            // it was seen at, and one that keeps running is where somebody
            // can still act on it.
            self.record_context(runtime, &attempt);
            // …and the same argument for the review's evidence (§gh#183): the
            // checkout and the journal both outlive the attempt by less than
            // the review does.
            self.record_review_facts(runtime, &attempt);
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
                             leaving it until liveness can be checked \
                             (§runtime-impl)",
                            task.identifier, chat_id
                        ));
                    }
                    continue;
                }
                // The run is gone; whether the *chat* is gone with it is what
                // decides the attempt's fate (§gh#390), and how often it has
                // been started already — by the board and by the engine's own
                // boot recovery — is what bounds it (§gh#392).
                let restarts = self.restarts(runtime, &task, chat_id, attempt.resumes);
                match runs::decide(self.chat_liveness(runtime, &task, chat_id), ticks, restarts) {
                    runs::Verdict::Wait => {
                        self.log.info(format!(
                            "{} chat {} missing (tick {}/{})",
                            task.identifier,
                            chat_id,
                            ticks,
                            runs::MISSING_TICKS
                        ));
                        self.db.set_missing_ticks(attempt.id, ticks)?;
                    }
                    runs::Verdict::Orphan => {
                        self.log.warn(format!(
                            "{} chat {} gone for {} ticks — orphaned",
                            task.identifier, chat_id, ticks
                        ));
                        self.orphan(runtime, &task, &attempt)?;
                    }
                    runs::Verdict::Resume => {
                        // The run may have died *after* finishing — an engine
                        // restart lands on attempts at every stage. Ask the
                        // settle logic first, with `Idle` standing for the fact
                        // this branch establishes: there is no run any more.
                        // Resuming an agent that already opened its pull
                        // request would spend a turn undoing the work.
                        if !self.maybe_settle(runtime, &task, &attempt, AgentStatus::Idle, false)?
                            && let Some(fate) =
                                self.resume(runtime, &task, &attempt, chat_id, restarts)?
                        {
                            interrupted.push(fate);
                        }
                    }
                    runs::Verdict::GiveUp => {
                        interrupted.push(self.give_up(runtime, &task, &attempt, restarts)?);
                    }
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
            // §settle-logic: the turn ended — check the checkout. No fresh PR
            // lookup on this path: the cycle polled GitHub moments ago, so the
            // recorded PR state is as fresh as a lookup would be.
            let settled = self.maybe_settle(runtime, &task, &attempt, status, false)?;
            // Blocked *and* settled is an errored run whose pull request was
            // already open: the work is reviewable, so it is a settle and not
            // a block, and saying both would be the board contradicting
            // itself in two comments on the same issue.
            if entered_blocked && !settled {
                self.note_blocked(runtime, &task, &attempt, chat_id)?;
            }
        }
        // One notice for the whole incident, before anything else this cycle
        // decides (§gh#390) — a box that just lost every run it had should say
        // that as itself, not leave it to be inferred from the wreckage.
        self.report_interrupted(runtime, &interrupted);
        // After settling, so an attempt that just finished is never failed for
        // running long; before the rewatch, which only looks at closed rows.
        self.enforce_duration_cap(runtime)?;
        // Last, because it may re-open an attempt: doing it first would hand
        // the live pass above a row it has already decided about.
        self.rewatch_settled_attempts(statuses, runtime)?;
        Ok(())
    }

    /// Copy what an attempt's chat has spent onto its row (gh#151).
    ///
    /// The engine's run journal is the source and the attempt row is the
    /// record, because the two have different lifetimes: §gh#144 archives a
    /// chat once nobody is coming back to it, and a journal can be compacted
    /// or lost, while the attempt survives for as long as the board has a
    /// history to report on. So this is a copy taken while the evidence is
    /// still there, not a lookup deferred to whenever somebody opens the page.
    ///
    /// Called on every reconcile of a live attempt rather than once at close:
    /// an attempt can end by being orphaned, capped or cancelled — paths where
    /// the chat may already be gone — and the last tick's figure is a far
    /// better answer for those than none at all.
    ///
    /// Never fatal. A runtime that cannot count tokens, an unreadable journal
    /// and a chat that has reported nothing yet all leave the row exactly as
    /// it was; the page's coverage line is where that shows up, which is where
    /// it belongs.
    pub fn record_tokens(&self, runtime: Option<&dyn Runtime>, attempt: &Attempt) {
        let Some(runtime) = runtime else { return };
        let Some(chat_id) = attempt.pane_id.as_deref() else {
            return;
        };
        let run = match runtime.run_tokens(chat_id) {
            Ok(Some(run)) => run,
            Ok(None) => return,
            Err(e) => {
                self.log
                    .warn(format!("tokens for chat {chat_id} unreadable: {e:#}"));
                return;
            }
        };
        // A write per tick per live attempt is a write nobody needs: the
        // journal only grows between turns, so a chat mid-thought reports the
        // same numbers every time it is asked.
        let model_settled = run.model.is_none() || attempt.model == run.model;
        if attempt.tokens == Some(run.usage)
            && model_settled
            && attempt.token_models == run.by_model
            && attempt.token_agents == run.by_agent
        {
            return;
        }
        if let Err(e) = self.db.set_attempt_tokens(
            attempt.id,
            run.usage,
            run.model.as_deref(),
            run.by_model.as_deref(),
            run.by_agent.as_deref(),
        ) {
            self.log.warn(format!(
                "recording tokens for attempt {}: {e:#}",
                attempt.id
            ));
        }
    }

    /// Copy how full the attempt's context window is onto its row (gh#271).
    ///
    /// Beside [`SyncEngine::record_tokens`] and on the same schedule, for the
    /// same reason — the journal and the attempt have different lifetimes — but
    /// answering a different question. Spend is what the attempt cost;
    /// fullness is whether it is about to lose the context it is working from,
    /// which is only worth anything while somebody can still act on it.
    ///
    /// Overwritten each time rather than added to: this is a level. A row that
    /// stops moving is an attempt whose harness stopped reporting, and the
    /// last reading is kept, which is the useful answer for an attempt that
    /// ends by being orphaned or capped.
    ///
    /// Never fatal, and never noisy: a runtime that cannot answer, an
    /// unreadable journal and a harness that meters no window all leave the
    /// row exactly as it was.
    pub fn record_context(&self, runtime: Option<&dyn Runtime>, attempt: &Attempt) {
        let Some(runtime) = runtime else { return };
        let Some(chat_id) = attempt.pane_id.as_deref() else {
            return;
        };
        let context = match runtime.run_context(chat_id) {
            Ok(Some(context)) => context,
            Ok(None) => return,
            Err(e) => {
                self.log.warn(format!(
                    "context usage for chat {chat_id} unreadable: {e:#}"
                ));
                return;
            }
        };
        // A level that has not moved is a write nobody needs, and a live
        // attempt is reconciled every few seconds.
        if attempt.context == Some(context) {
            return;
        }
        if let Err(e) = self.db.set_attempt_context(attempt.id, context) {
            self.log.warn(format!(
                "recording context usage for attempt {}: {e:#}",
                attempt.id
            ));
        }
    }

    // ---- the review contract (§gh#183) -----------------------------------

    /// Copy the two facts a review needs onto the attempt while they still
    /// exist: what the branch touched, and what the run's commands did.
    ///
    /// A copy for [`SyncEngine::record_tokens`]'s reason, restated: gc reclaims
    /// the checkout (gh#72) and a run journal can be compacted, while the
    /// attempt row survives for as long as the board has a history to report
    /// on. A review taken at read time would be a review that goes blank
    /// exactly when it is most useful — a fortnight after the merge, when
    /// somebody asks what that branch actually changed.
    ///
    /// Called on every reconcile of a live attempt rather than once at close,
    /// again for `record_tokens`'s reason: an attempt can end by being
    /// orphaned, capped or cancelled, and the last tick's snapshot is a far
    /// better answer for those than none at all.
    ///
    /// Never fatal, and never noisy: an unreadable checkout, a runtime that
    /// cannot read journals, and an attempt with no worktree all leave the row
    /// as it was. What that costs is visible in the review itself, as
    /// [`crate::claims::DiffSource::Unavailable`].
    pub fn record_review_facts(&self, runtime: Option<&dyn Runtime>, attempt: &Attempt) {
        if let Some((changed, effects)) = self.branch_facts(attempt) {
            // The board reads this every cycle and the diff only moves when the
            // agent commits, so a write per tick is a write nobody needs.
            let stored = self.db.attempt_changes(attempt.id).unwrap_or_default();
            let moved = stored != changed;
            if moved && let Err(e) = self.db.set_attempt_changes(attempt.id, &changed) {
                self.log.warn(format!(
                    "recording changes for attempt {}: {e:#}",
                    attempt.id
                ));
            }
            // The effects are a function of the diff, so a branch that did not
            // move does not need them derived again — except on a row that has
            // none at all, which is every attempt that was already running when
            // §gh#236 landed and whose branch may never move again.
            if (moved || self.db.attempt_effects(attempt.id).ok().flatten().is_none())
                && let Err(e) = self.db.set_attempt_effects(attempt.id, &effects)
            {
                self.log.warn(format!(
                    "recording effects for attempt {}: {e:#}",
                    attempt.id
                ));
            }
        }
        let Some(runtime) = runtime else { return };
        let Some(chat_id) = attempt.pane_id.as_deref() else {
            return;
        };
        // The terms the commands below ran under (§gh#349). Recorded first and
        // on its own `if`: a chat whose journal has no commands in it yet has
        // still already said what sandbox it got — that line is written before
        // the run does anything — and returning early on the commands would
        // lose it for exactly the runs that end without executing much.
        match runtime.run_sandbox(chat_id) {
            Ok(Some(sandbox)) => {
                if self.db.attempt_sandbox(attempt.id).ok().flatten().as_ref() != Some(&sandbox)
                    && let Err(e) = self.db.set_attempt_sandbox(attempt.id, &sandbox)
                {
                    self.log.warn(format!(
                        "recording sandbox for attempt {}: {e:#}",
                        attempt.id
                    ));
                }
            }
            Ok(None) => {}
            Err(e) => self
                .log
                .warn(format!("sandbox for chat {chat_id} unreadable: {e:#}")),
        }
        let commands = match runtime.run_commands(chat_id) {
            Ok(Some(commands)) => commands,
            Ok(None) => return,
            Err(e) => {
                self.log
                    .warn(format!("commands for chat {chat_id} unreadable: {e:#}"));
                return;
            }
        };
        let evidence = crate::evidence::gather(&commands);
        if self.db.attempt_evidence(attempt.id).ok().flatten().as_ref() == Some(&evidence) {
            return;
        }
        if let Err(e) = self.db.set_attempt_evidence(attempt.id, &evidence) {
            self.log.warn(format!(
                "recording evidence for attempt {}: {e:#}",
                attempt.id
            ));
        }
    }

    /// What this attempt's branch has touched, read from its checkout.
    ///
    /// `None` — not an empty diff — when the question cannot be asked: no
    /// worktree recorded, the checkout is gone, or no base to measure from. The
    /// distinction is the whole of it: an empty list is "this branch changed
    /// nothing", and a review that rendered the two the same would say exactly
    /// the wrong thing about a reclaimed checkout.
    ///
    /// Measured from the attempt's own `base_sha`, like
    /// [`SyncEngine::attempt_has_commits`] and for the same reason (AGE-19):
    /// anything else counts the operator's unpushed work as the agent's. An
    /// attempt with no recorded base is not guessed at — the fallback that
    /// function keeps for pre-`base_sha` rows is a weaker measurement, and a
    /// *remainder* computed against the wrong base would invent unclaimed files
    /// out of somebody else's commits.
    pub fn branch_facts(
        &self,
        attempt: &Attempt,
    ) -> Option<(Vec<crate::claims::ChangedFile>, crate::effects::Effects)> {
        let worktree = attempt.worktree.as_deref()?;
        if !std::path::Path::new(worktree).exists() {
            return None;
        }
        // The base as the checkout has it now, not merely as it was stamped:
        // a branch rebased under its stamp (gh#286) would otherwise be
        // rendered as having changed everything the layer below it changed.
        let base = self.attempt_base(attempt)?;
        let range = format!("{base}..HEAD");
        // `--no-renames`, deliberately: to a reviewer a rename is two paths
        // that both changed, and a claim naming only the old one has not
        // accounted for the new one arriving.
        let numstat = git_out(worktree, &["diff", "--numstat", "--no-renames", &range])?;
        let name_status = git_out(worktree, &["diff", "--name-status", "--no-renames", &range])
            .unwrap_or_default();
        let mut changed = crate::claims::parse_diff(&numstat, &name_status);
        // A third read of the same range, for the symbol anchors (§gh#235) and
        // for the effects (§gh#236). `-U0` because only the lines that actually
        // moved are evidence: a symbol three lines above the edit is context
        // the agent did not touch, and letting it anchor a claim — or count as
        // a public API change — would be exactly the generous reading this
        // module refuses everywhere else.
        let unified = git_out(worktree, &["diff", "-U0", "--no-renames", &range]);
        if let Some(diff) = &unified {
            crate::claims::attach_symbols(&mut changed, diff);
        }
        let effects = self.branch_effects(worktree, &base, &changed, unified.as_deref());
        Some((changed, effects))
    }

    /// What this attempt's branch had as an *effect* (§gh#236): the tests
    /// either side of it, the public surface, the schema, the config keys and
    /// the dependencies.
    ///
    /// Everything here is read from git, and every failure lands as "unknown"
    /// rather than as a clean result — a diff the board could not read at all
    /// returns [`crate::effects::Effects::default`], whose `read` flag is
    /// `false` and whose chip row says so in one line.
    fn branch_effects(
        &self,
        worktree: &str,
        base: &str,
        changed: &[crate::claims::ChangedFile],
        diff: Option<&str>,
    ) -> crate::effects::Effects {
        let Some(diff) = diff else {
            return crate::effects::Effects::default();
        };
        let (deps_added, deps_known) = self.deps_added(worktree, base, changed);
        crate::effects::Effects {
            read: true,
            files: crate::effects::scan(changed, diff),
            // Both counted the same way in the two trees the diff spans, so the
            // pair is comparable even where the rule is coarse. `HEAD` rather
            // than the working tree: the branch is what a reviewer can fetch,
            // and uncommitted work is reported on its own line.
            tests_after: test_total(worktree, "HEAD"),
            tests_before: test_total(worktree, base),
            deps_added,
            deps_known,
        }
    }

    /// Dependencies this branch added, read out of the manifests on both sides.
    ///
    /// Not a diff heuristic: the file as it was and the file as it is, each
    /// parsed for the names it lists. A manifest that will not parse, or a side
    /// git will not hand over, returns `false` for "known" — which makes the
    /// chip say unknown, because "no dependencies added" is the one thing the
    /// board must not say about a manifest it failed to read.
    fn deps_added(
        &self,
        worktree: &str,
        base: &str,
        changed: &[crate::claims::ChangedFile],
    ) -> (Vec<String>, bool) {
        let mut added: Vec<String> = Vec::new();
        let mut known = true;
        for file in changed
            .iter()
            .filter(|f| crate::effects::is_manifest(&f.path))
        {
            // An added file had no `before` and a deleted one has no `after`:
            // git is right to refuse those, and empty is the honest content.
            let before = match file.status.starts_with('A') {
                true => Some(String::new()),
                false => git_out(worktree, &["show", &format!("{base}:{}", file.path)]),
            };
            let after = match file.status.starts_with('D') {
                true => Some(String::new()),
                false => git_out(worktree, &["show", &format!("HEAD:{}", file.path)]),
            };
            match (before, after) {
                (Some(before), Some(after)) => {
                    match crate::effects::deps_added(&file.path, &before, &after) {
                        Some(names) => added.extend(names),
                        None => known = false,
                    }
                }
                _ => known = false,
            }
        }
        added.sort();
        added.dedup();
        (added, known)
    }

    /// Read an agent's claims off the attempt it just finished (§gh#235).
    ///
    /// The fallback half of the contract. `comet-board claim` is the better
    /// path and the one the skill asks for first, because it answers with the
    /// remainder while the agent can still act on it — but an agent that
    /// finished without running it has still written down what it did, in the
    /// fenced block the skill asks for, and reading that late beats losing it.
    ///
    /// Three things this must never do, and each is why it returns nothing:
    ///
    /// - **Overwrite.** An attempt that already answered is left alone; the
    ///   submitted set is the agent's considered answer and this is a scrape.
    /// - **Block.** Every failure here — no runtime, no journal, no block, a
    ///   block that will not parse — leaves the settle to carry on. A claimless
    ///   attempt settles exactly as it did before any of this existed, which is
    ///   the ticket's own exit condition.
    /// - **Drop a malformed block silently.** That one is recorded on the row,
    ///   where the review says it out loud
    ///   ([`crate::claims::FindingKind::MalformedClaims`]).
    fn harvest_claims(&self, runtime: Option<&dyn Runtime>, attempt: &Attempt) {
        if attempt.claims_at.is_some() {
            return;
        }
        let Some(runtime) = runtime else { return };
        let Some(chat_id) = attempt.pane_id.as_deref() else {
            return;
        };
        let text = match runtime.run_message(chat_id) {
            Ok(Some(text)) => text,
            Ok(None) => return,
            Err(e) => {
                self.log
                    .warn(format!("message for chat {chat_id} unreadable: {e:#}"));
                return;
            }
        };
        match crate::claims::harvest(&text, attempt.worktree.as_deref()) {
            crate::claims::Harvest::None => {}
            crate::claims::Harvest::Claims(claims) => {
                match self.db.set_attempt_claims(attempt.id, &claims) {
                    Ok(()) => self.log.info(format!(
                        "attempt {} claimed {} change(s) in its closing message",
                        attempt.id,
                        claims.len()
                    )),
                    Err(e) => self.log.warn(format!(
                        "recording claims for attempt {}: {e:#}",
                        attempt.id
                    )),
                }
            }
            crate::claims::Harvest::Malformed(why) => {
                self.log.warn(format!(
                    "attempt {} wrote a claims block that would not parse: {why}",
                    attempt.id
                ));
                if let Err(e) = self.db.set_attempt_claims_error(attempt.id, &why) {
                    self.log.warn(format!(
                        "recording the claims refusal for attempt {}: {e:#}",
                        attempt.id
                    ));
                }
            }
        }
    }

    /// Record an agent's claims against an attempt, and hand back the review
    /// they land in — including what they did not account for.
    ///
    /// Answering with the remainder is the point of doing it here rather than
    /// in a fire-and-forget write: the agent learns, at the moment it submits,
    /// which of its own changes nothing it wrote covers. That is the earliest
    /// the question can be asked, and it is asked of the one party still able
    /// to answer it.
    ///
    /// The attempt is the task's live one, else its most recent: an agent
    /// submits while its own run is still going, and a retry's claims belong to
    /// the retry.
    pub fn submit_claims(
        &self,
        runtime: Option<&dyn Runtime>,
        task_id: &str,
        text: &str,
    ) -> Result<crate::claims::AttemptReview> {
        let tasks = self.db.load_tasks()?;
        let task = crate::dispatch::task_by_reference(&tasks, task_id)?;
        let attempt = task
            .live_attempt()
            .or_else(|| task.attempts.last())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} has no attempts — claims belong to a run, and nothing has run",
                    task.identifier
                )
            })?
            .clone();
        let claims = crate::claims::parse(text, attempt.worktree.as_deref())?;
        self.db.set_attempt_claims(attempt.id, &claims)?;
        self.log.info(format!(
            "{}: attempt {} claimed {} change(s) across {} file(s)",
            task.identifier,
            attempt.id,
            claims.len(),
            claims.iter().flat_map(|c| &c.files).count()
        ));
        // A fresh snapshot before answering: the agent has just finished
        // committing, and a remainder computed against the last reconcile's
        // diff would report files it has since accounted for.
        self.record_review_facts(runtime, &attempt);
        self.review(task_id, Some(attempt.id))
    }

    /// Everything a review of one attempt is made of (§gh#183): the brief, the
    /// claims, the evidence, and the changes no claim accounts for.
    ///
    /// `attempt` names one; `None` is the task's latest, which is what a
    /// reviewer opening a row means. The diff is read live from the checkout
    /// when there is one and falls back to the snapshot the board took while
    /// the attempt was live — and says which, because "nothing changed" and
    /// "the checkout is gone" must never render the same.
    ///
    /// A row with a pull request and **no attempt at all** is reviewed from the
    /// pull request (§gh#344): [`Self::pull_request_review`]. That is the whole
    /// of what an undispatched pull request used to be missing — it had a row,
    /// it was polled like any other, and every part of review needed an attempt
    /// that nothing had created.
    pub fn review(
        &self,
        task_id: &str,
        attempt: Option<i64>,
    ) -> Result<crate::claims::AttemptReview> {
        use crate::claims::DiffSource;
        let tasks = self.db.load_tasks()?;
        let task = crate::dispatch::task_by_reference(&tasks, task_id)?;
        let attempt = match attempt {
            Some(id) => task
                .attempts
                .iter()
                .find(|a| a.id == id)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!("attempt {id} does not belong to {}", task.identifier)
                })?,
            None => match task.attempts.last().cloned() {
                Some(attempt) => attempt,
                None => {
                    let mut review = self.pull_request_review(task)?;
                    crate::stacks::place_in_stack(&tasks, task, &mut review);
                    return Ok(review);
                }
            },
        };
        let (changed, effects, diff) = match self.branch_facts(&attempt) {
            Some((changed, effects)) => (changed, effects, DiffSource::Checkout),
            None => {
                let recorded = self.db.attempt_changes(attempt.id)?;
                // The snapshot's effects, which are the ones taken against the
                // snapshot's diff. Absent is `read = false`, and the chip row
                // says the board never read this branch rather than saying
                // nothing moved in it.
                let effects = self.db.attempt_effects(attempt.id)?.unwrap_or_default();
                if recorded.is_empty() {
                    (
                        Vec::new(),
                        effects,
                        DiffSource::Unavailable {
                            reason: unreadable_diff(&attempt),
                        },
                    )
                } else {
                    (recorded, effects, DiffSource::Recorded)
                }
            }
        };
        let mut review = crate::claims::review(
            task,
            &attempt,
            changed,
            diff,
            self.uncommitted(&attempt),
            self.db.attempt_evidence(attempt.id)?.unwrap_or_default(),
            effects,
            self.db.attempt_sandbox(attempt.id)?,
        );
        self.count_call_sites(&attempt, &mut review);
        crate::stacks::place_in_stack(&tasks, task, &mut review);
        Ok(review)
    }

    /// A review of a pull request nobody dispatched (§gh#344).
    ///
    /// The row is ordinary — a `gh!<n>` upserted by the poll like any other —
    /// and the only thing it is missing is the attempt every part of review
    /// used to be read from. So the attempt becomes an enrichment: the brief is
    /// the row, the diff is GitHub's file list, and the claims are empty, which
    /// makes the remainder the whole diff. That is the honest reading rather
    /// than a degraded one — nobody was ever told the contract.
    ///
    /// A row with no attempt and no pull request either is still an error. It
    /// is not that its review is unavailable; there is nothing to review, and
    /// answering with an empty screen would be inventing one. That error used
    /// to read `has no attempts to review`, which stopped being the reason the
    /// moment an attempt stopped being the requirement — nothing ran *and*
    /// nothing was pushed is what is left, and it is what the sentence says.
    fn pull_request_review(&self, task: &Task) -> Result<crate::claims::AttemptReview> {
        use crate::claims::DiffSource;
        let Some((repo, number)) = crate::verdict::pr_target(task) else {
            anyhow::bail!(
                "nothing has run on {} and no pull request has been opened for it — \
                 there is nothing to review",
                task.identifier
            );
        };
        // Every failure below is a *source*, never a refusal: the brief, the
        // pull request and the fact that nothing was claimed are worth reading
        // on their own, and a review that erred because GitHub was unreachable
        // would take them away too.
        let (changed, diff) = match self.github.as_ref() {
            None => (
                Vec::new(),
                DiffSource::Unavailable {
                    reason: "nothing ran here and this board has no GitHub credential to \
                             read the pull request's diff with"
                        .to_string(),
                },
            ),
            Some(gh) => match gh.pull_files(&repo, number) {
                Ok(changed) => (changed, DiffSource::PullRequest),
                Err(e) => (
                    Vec::new(),
                    DiffSource::Unavailable {
                        reason: format!(
                            "nothing ran here, and GitHub would not say what {repo}#{number} changed: {e:#}"
                        ),
                    },
                ),
            },
        };
        Ok(crate::claims::pull_request_review(task, changed, diff))
    }

    /// Count, for every symbol anchor on every claim, how many lines name it
    /// now and how many named it before (§gh#236).
    ///
    /// Done here rather than in the snapshot because it needs a checkout and
    /// two greps per symbol: cheap when somebody opens a review, and not
    /// something to run on every attempt on every reconcile. When the checkout
    /// is gone the claims simply carry no call-site chips — the absence of a
    /// count is the honest rendering of a count that was never taken.
    fn count_call_sites(&self, attempt: &Attempt, review: &mut crate::claims::AttemptReview) {
        let (Some(worktree), Some(base)) =
            (attempt.worktree.as_deref(), self.attempt_base(attempt))
        else {
            return;
        };
        if !std::path::Path::new(worktree).exists() {
            return;
        }
        for claim in &mut review.remainder.claims {
            for symbol in claim.symbols.clone() {
                let Some(now) = git_grep(worktree, &symbol, "HEAD") else {
                    continue;
                };
                claim.call_sites.push(crate::effects::CallSites {
                    now: crate::effects::count_call_sites(&symbol, &now),
                    before: git_grep(worktree, &symbol, &base)
                        .map(|before| crate::effects::count_call_sites(&symbol, &before)),
                    symbol,
                });
            }
        }
    }

    /// How many files are changed in this attempt's checkout but not committed.
    ///
    /// Reported rather than folded into the diff, because the remainder is
    /// about the *branch* — what a reviewer can fetch — and uncommitted work is
    /// not on it. It exists for the submission-time case: an agent that claims
    /// before it commits would otherwise be told all of its changes were
    /// accounted for, having shown the board none of them.
    ///
    /// `None` when there is no checkout to ask, which is not zero.
    fn uncommitted(&self, attempt: &Attempt) -> Option<u32> {
        let worktree = attempt.worktree.as_deref()?;
        if !std::path::Path::new(worktree).exists() {
            return None;
        }
        // `--porcelain` is one line per path, untracked files included: an
        // agent that has written a new file and not added it has not shown it
        // to the board either.
        let out = git_out(worktree, &["status", "--porcelain"])?;
        Some(out.lines().filter(|l| !l.trim().is_empty()).count() as u32)
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
            // The pinned orchestrator is supposed to live forever (gh#104), so
            // the clock that exists to stop a looping agent must not stop it.
            // The exemption is on the *chat*, not on how the attempt was
            // created: pinning a board-dispatched chat is a misconfiguration
            // `doctor` names, and killing it at the two-hour mark would be a
            // second, less legible way of saying so.
            // Both sides matched on `None` would exempt every attempt that
            // never got a chat — the most stranded rows on the board, and the
            // ones the cap exists for.
            let exempt = matches!(
                self.cfg.defaults.orchestrator(),
                Some(pinned) if Some(pinned) == attempt.pane_id.as_deref()
            );
            match overrun::decide(age, warned, cap, grace) {
                overrun::Verdict::Within => {}
                // Stamped, so this says itself once rather than every cycle for
                // as long as the orchestrator lives — which is the point of it.
                overrun::Verdict::Warn if exempt => {
                    self.db.set_overrun_warned(attempt.id)?;
                    self.log.info(format!(
                        "{}: past its {} cap, but its chat is the pinned orchestrator — exempt",
                        task.identifier,
                        overrun::human_secs(cap as i64),
                    ));
                }
                overrun::Verdict::Warn => {
                    self.warn_overrun(runtime, &task, &attempt, age, cap, grace)?
                }
                overrun::Verdict::Cancel if exempt => {}
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
        // The orchestrator hears about the cap before the attempt is gone
        // (gh#104) — the one notice it gets about a run that is still going,
        // and the only window in which reading the chat can still change the
        // outcome. Straight here with no dispatcher hop (gh#165): this is not
        // an event about work that has ended, so there is nothing for a
        // dispatcher waiting on the step to do about it, and the party who can
        // still act is one that goes and looks. Outside the `runtime`/`chat_id`
        // guard below on purpose: an attempt whose chat vanished is exactly the
        // kind the orchestrator has no other way to learn about.
        self.wake_orchestrator(
            runtime,
            task,
            attempt,
            &notify::Event::CapWarning {
                age_secs: age,
                cap_secs: cap,
                grace_secs: grace,
            },
        );
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
        if let Some(runtime) = runtime {
            if let Some(chat_id) = attempt.pane_id.as_deref()
                && let Err(e) = runtime.cancel(chat_id)
            {
                // A chat may already be gone; that is not a reason to keep an
                // overrun live.
                self.log.warn(format!(
                    "{}: cancelling chat {chat_id} at the duration cap: {e:#}",
                    task.identifier
                ));
            }
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
        // The cap's *warning* goes straight to the orchestrator, because a run
        // that is still going is nothing for a dispatcher to act on. Its
        // ending is the opposite, and until gh#194 it went to nobody: the
        // attempt closed, the checkout and the chat went on their retention
        // clocks, and the agent whose plan that task was a step in waited for
        // a settle that was never coming. A close the board decided on its own
        // is the *most* owed notice, not the least.
        self.announce(
            runtime,
            task,
            attempt,
            Signal::Settled {
                outcome: Outcome::Failed,
                evidence: None,
                pr_url: None,
                note: Some(note),
            },
        );
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
        // Who is standing on whose branch (gh#286). Built once from the whole
        // board, because no amount of looking at one attempt finds the layer
        // dispatched off it.
        let dependents = rebased::Dependents::of(&tasks);
        for task in &tasks {
            for attempt in &task.attempts {
                if !attempt.board_managed
                    || attempt.collected_at.is_some()
                    || attempt.worktree.is_none()
                {
                    continue;
                }
                let spent = attempt
                    .collectable_at
                    .as_deref()
                    .map(|t| secs_since(t, now).unwrap_or(0));
                let standing = gc::standing(task, attempt, &dependents);
                if standing == gc::Standing::Held && dependents.holds(attempt.id) {
                    self.note_held_by_dependents(task, attempt, &dependents);
                }
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

    /// Say, once, that a checkout is being kept for somebody else's sake
    /// (gh#286) — the answer to "why is this branch still here a month later",
    /// which is otherwise a silence the operator has to reconstruct from the
    /// stacking edge by hand.
    fn note_held_by_dependents(
        &self,
        task: &Task,
        attempt: &Attempt,
        dependents: &rebased::Dependents,
    ) {
        let key = meta::dependents_noted(attempt.id);
        if matches!(self.db.meta_get(&key), Ok(Some(_))) {
            return;
        }
        let _ = self.db.meta_set(&key, &crate::db::now());
        let holders = dependents
            .holders(attempt.id)
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        self.log.info(format!(
            "{}: keeping {} — attempt {} was cut from it and still holds a checkout",
            task.identifier,
            attempt
                .branch
                .as_deref()
                .unwrap_or("the attempt's own branch"),
            holders,
        ));
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

    // ---- sweeping build output (gh#186) ----------------------------------

    /// Delete the build output inside every finished attempt's checkout, on
    /// `[defaults] retain_build_output` — a much shorter clock than the checkout's
    /// own.
    ///
    /// The measurement this exists for, from the box on 2026-08-09: eight
    /// checkouts, 109.5 GiB, of which 44 MiB was checkouts. One of them was 36 GB
    /// with 298 MB outside `target/`. `retain_worktrees` governed both, so the
    /// cheap thing and the expensive thing were kept for the same week, and a week
    /// of Rust checkouts does not fit on a 150 GB disk. Clearing `target/` from
    /// the three whose pull requests had already merged took the box from 36 GB
    /// free to 123 GB.
    ///
    /// What is different from [`SyncEngine::collect_worktrees`], and it is all
    /// one difference — a cache has no reason to be kept:
    ///
    /// - **The clock starts when the attempt ends**, not when the task leaves the
    ///   board. An open pull request holds the *checkout* (review delivery
    ///   resumes an agent in that directory) and holds nothing here: the agent it
    ///   resumes rebuilds. [`gc::cache_standing`] consults one fact — whether
    ///   anybody is building in there — and a live retry on the task counts,
    ///   because retries reuse the branch and therefore the directory.
    /// - **A sweep is not a collection.** `cache_swept_at` says a cache inside a
    ///   checkout that is still there is gone; `collected_at` would say the
    ///   checkout itself is, which would be false and would stop the board ever
    ///   reclaiming it.
    /// - **It can happen twice.** Unlike a deleted worktree or an archived chat,
    ///   a cache comes back: an attempt re-opened to answer review comments
    ///   builds again, so the re-open clears both stamps
    ///   ([`Db::clear_attempt_cache_swept`]) and the next end sweeps the new one.
    ///
    /// Failure is never fatal to the cycle, and never stamps: an error leaves the
    /// attempt unswept so the next sweep tries again over what is left.
    fn sweep_build_output(&self, runtime: Option<&dyn Runtime>) {
        let Some(retain) = self.cfg.retain_build_output_secs() else {
            return;
        };
        let now = chrono::Utc::now();
        let tasks = match self.db.load_tasks() {
            Ok(t) => t,
            Err(e) => {
                self.log
                    .error(format!("build output sweep: reading the board: {e}"));
                return;
            }
        };
        for task in &tasks {
            for attempt in &task.attempts {
                // Nothing to sweep: no checkout recorded, the checkout already
                // reclaimed whole, or this cache already swept. The last is what
                // keeps a box that has been up for months from re-walking every
                // source tree it has ever cut, every cycle.
                if !attempt.board_managed
                    || attempt.worktree.is_none()
                    || attempt.collected_at.is_some()
                    || attempt.cache_swept_at.is_some()
                {
                    continue;
                }
                let spent = attempt
                    .cache_sweepable_at
                    .as_deref()
                    .map(|t| secs_since(t, now).unwrap_or(0));
                let standing = gc::cache_standing(task, attempt);
                if let Err(e) = match gc::decide(standing, spent, retain) {
                    gc::Verdict::Keep => Ok(()),
                    gc::Verdict::Mark => self.mark_cache_sweepable(task, attempt, retain),
                    gc::Verdict::Unmark => self.db.set_attempt_cache_sweepable(attempt.id, false),
                    gc::Verdict::Collect => self.sweep_one_cache(runtime, task, attempt),
                } {
                    self.log.warn(format!(
                        "{}: build output sweep on attempt {}: {e:#}",
                        task.identifier, attempt.id
                    ));
                }
            }
        }
    }

    /// Start the sweep clock on one checkout's build output.
    ///
    /// Silent under `on-settle`, unlike [`SyncEngine::mark_collectable`]: with no
    /// window the mark and the sweep are the same event a cycle apart, and
    /// announcing a deletion thirty seconds before announcing it again is noise
    /// where the checkout's week-long clock genuinely needs a warning. A real
    /// window gets the notice, naming what it is about to remove and when.
    fn mark_cache_sweepable(&self, task: &Task, attempt: &Attempt, retain: u64) -> Result<()> {
        self.db.set_attempt_cache_sweepable(attempt.id, true)?;
        if retain > 0 {
            self.log.info(format!(
                "{}: nothing is building in {} any more — its build output goes in {}",
                task.identifier,
                attempt.worktree.as_deref().unwrap_or("(none)"),
                gc::human_window(retain),
            ));
        }
        Ok(())
    }

    /// The window ran out: hand the cache to the runtime that owns the checkout,
    /// and record that it is gone — as a sweep, never as a collection.
    ///
    /// A cycle run without a runtime marks and unmarks but deletes nothing, the
    /// same split [`SyncEngine::collect_one`] and
    /// [`SyncEngine::archive_one_chat`] make.
    ///
    /// Silent when there was nothing to sweep, which is most attempts: a checkout
    /// nobody built in, or one whose agent only edited docs, has no cache and its
    /// stamp is bookkeeping rather than news. A sweep that freed something says
    /// what and how much, because that number is the whole argument for this
    /// feature.
    fn sweep_one_cache(
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
        let swept = runtime.reclaim_build_output(worktree)?;
        if swept.dirs > 0 {
            self.log.info(format!(
                "{}: swept {} of build output from {} ({} director{}) — the next build \
                 in there writes it again",
                task.identifier,
                gc::human_bytes(swept.bytes),
                worktree,
                swept.dirs,
                if swept.dirs == 1 { "y" } else { "ies" },
            ));
        }
        // Unstamped on a partial failure, so the next cycle retries what is
        // left. The bytes that did go are already reported above.
        if !swept.failed.is_empty() {
            anyhow::bail!("{}", swept.failed.join("; "));
        }
        self.db.set_attempt_cache_swept(attempt.id)?;
        Ok(())
    }

    // ---- telling GitHub about a chain the board cut (gh#387) -------------

    /// Make the chains `--onto` built into GitHub stacks.
    ///
    /// `--onto` does everything it documents — cuts each branch from the one
    /// below, opens each pull request against it, records the edge on the
    /// attempt — and creates no stack, so GitHub's `stack` object is absent,
    /// [`crate::stacks::Stacks`] finds nothing to group, and the dependency
    /// lives in the branch bases and nowhere else. Until somebody ran `gh stack
    /// link` by hand, a board where every stacks feature worked had never once
    /// produced a stack.
    ///
    /// It cannot happen at dispatch, which is the whole reason it is here: a
    /// stack is made of pull requests, and at dispatch there is not one yet.
    /// The chain becomes stackable at some unpredictable point afterwards —
    /// when the last agent opens its pull request — so the board watches for
    /// that the way it watches for everything else, on the cycle that has just
    /// polled the pull requests.
    ///
    /// What keeps it from being a write on a loop:
    ///
    /// - **Nothing to send is the ordinary answer.** [`crate::stacks::unlinked`]
    ///   plans from board rows alone, and a chain already stacked, half-open, or
    ///   not adjacent yet plans to nothing at all.
    /// - **One request is sent once**, and a refused one at most
    ///   [`crate::stacks::LINK_TRIES`] times — [`crate::stacks::Asked`], stored
    ///   under [`meta::stack_asked`]. A chain that grows a layer is a different
    ///   request and gets its own budget.
    /// - **A refusal costs the chain, never the cycle.** GitHub validates the
    ///   base refs itself, and it is right to when the board's picture of them
    ///   is one poll old.
    fn link_dispatched_stacks(&self) {
        let Some(gh) = &self.github else {
            return;
        };
        let tasks = match self.db.load_tasks() {
            Ok(t) => t,
            Err(e) => {
                self.log
                    .error(format!("stack linking: reading the board: {e}"));
                return;
            }
        };
        for work in crate::stacks::unlinked(&tasks) {
            let key = meta::stack_asked(&work.signature());
            let mark = self
                .db
                .meta_get(&key)
                .ok()
                .flatten()
                .as_deref()
                .and_then(crate::stacks::Asked::parse);
            if !crate::stacks::worth_asking(mark) {
                continue;
            }
            let layers = work.layers().join(", ");
            let pulls = work
                .pulls()
                .iter()
                .map(|n| format!("PR #{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sent = match &work {
                crate::stacks::StackWork::Create { repo, pulls, .. } => gh
                    .create_stack(repo, pulls)
                    .map(|stack| format!("{layers} are {repo} stack {stack}")),
                crate::stacks::StackWork::Add {
                    repo, stack, pulls, ..
                } => gh
                    .add_to_stack(repo, *stack, pulls)
                    .map(|()| format!("{layers} joined {repo} stack {stack}")),
            };
            let asked = match sent {
                Ok(said) => {
                    self.log.info(format!(
                        "{said} — {pulls} was dispatched onto the branch below it \
                         and GitHub had never been told",
                    ));
                    crate::stacks::Asked::Linked
                }
                Err(e) => {
                    let asked = crate::stacks::Asked::refused_again(mark);
                    let crate::stacks::Asked::Refused(spent) = asked else {
                        unreachable!("a refusal is a refusal")
                    };
                    self.log.warn(format!(
                        "stacking {layers} in {}: {e:#} (attempt {spent} of {})",
                        work.repo(),
                        crate::stacks::LINK_TRIES,
                    ));
                    asked
                }
            };
            let _ = self.db.meta_set(&key, &asked.render());
        }
    }

    // ---- surviving the parent's merge (gh#286) ---------------------------

    /// Tell a stacked layer, once, that GitHub has rewritten its branch out
    /// from under this checkout.
    ///
    /// When a layer lands, GitHub rebases every layer above it and retargets
    /// their pull requests — on the server, in a repository this box polls. The
    /// checkout does not move ([`crate::rebased`] says why the board leaves it
    /// alone rather than rebasing it), so the agent still standing in it holds
    /// the pre-rebase history, and its next `git push` is a force-push that
    /// puts the old commits back over GitHub's work and undoes the rebase for
    /// the whole stack above it.
    ///
    /// The board cannot stop that push. What it can do is be the one party that
    /// *knows*, and say so where it lands: a prompt into the authoring chat
    /// while that chat is still the checkout's, and a log line either way.
    ///
    /// Three things keep it cheap, in the order they apply:
    ///
    /// - **Only stacked attempts are looked at at all.** `stacked_on` is the
    ///   whole population; an ordinary dispatch pays one field read.
    /// - **Only after the layer below has landed** ([`rebased::parent_landed`]),
    ///   which is the only thing that makes GitHub rewrite a branch.
    /// - **The remote-tracking ref first, GitHub second.** The free answer is
    ///   often enough — anything that fetched in that checkout has already
    ///   brought the rewrite down — and the API call is made only when it is
    ///   not, and only until the notice has been given once.
    ///
    /// One notice per rewrite, keyed on the head origin held when it was given:
    /// an agent told the same thing every cycle learns to skip the message, and
    /// a *second* rewrite is a different sha and says so again.
    ///
    /// With no runtime there is nobody to tell, and the sweep does not run: a
    /// read-only caller must not consume the one notice an agent gets.
    fn note_rewritten_branches(&self, runtime: Option<&dyn Runtime>) {
        let Some(runtime) = runtime else {
            return;
        };
        let tasks = match self.db.load_tasks() {
            Ok(t) => t,
            Err(e) => {
                self.log
                    .error(format!("rewritten branches: reading the board: {e}"));
                return;
            }
        };
        // Every attempt on the board, so a child can find the run it was cut
        // from — and the task that run belongs to, which is what says whether
        // it has landed.
        let parents: HashMap<i64, (&Task, &Attempt)> = tasks
            .iter()
            .flat_map(|t| t.attempts.iter().map(move |a| (a.id, (t, a))))
            .collect();
        for task in &tasks {
            for attempt in &task.attempts {
                let Some(parent_id) = attempt.stacked_on else {
                    continue;
                };
                if !attempt.board_managed || attempt.collected_at.is_some() {
                    continue;
                }
                let (Some(worktree), Some(branch)) =
                    (attempt.worktree.as_deref(), attempt.branch.as_deref())
                else {
                    continue;
                };
                if !std::path::Path::new(worktree).exists() {
                    continue;
                }
                let Some((parent_task, parent)) = parents.get(&parent_id) else {
                    continue;
                };
                if !rebased::parent_landed(
                    parent_task.pr_merged,
                    task.pr_base_ref.as_deref(),
                    parent.branch.as_deref(),
                ) {
                    continue;
                }
                let Some(local) = git_out(worktree, &["rev-parse", "HEAD"]) else {
                    continue;
                };
                let Some(remote) = self.origin_head_of(worktree, task, attempt, branch) else {
                    continue;
                };
                let standing = rebased::against_origin(&local, &remote, |a, b| {
                    git_ok(worktree, &["merge-base", "--is-ancestor", a, b])
                });
                if standing == rebased::Remote::Rewritten {
                    self.note_rewritten(runtime, task, attempt, branch, &remote);
                }
            }
        }
    }

    /// What origin holds for this attempt's branch: the remote-tracking ref if
    /// the checkout has one that has moved, else GitHub asked directly — and
    /// that only while the notice has not been given, which is what bounds the
    /// calls to one per rewrite rather than one per cycle.
    fn origin_head_of(
        &self,
        worktree: &str,
        task: &Task,
        attempt: &Attempt,
        branch: &str,
    ) -> Option<String> {
        let tracking = git_out(
            worktree,
            &["rev-parse", &format!("refs/remotes/origin/{branch}")],
        )
        .filter(|head| !head.is_empty());
        // A tracking ref that has not been fetched since the last push reads as
        // level with HEAD, which is no answer at all — the GitHub tier below is
        // what settles that case.
        if let Some(head) = tracking
            && git_out(worktree, &["rev-parse", "HEAD"]).as_deref() != Some(head.as_str())
        {
            return Some(head);
        }
        if matches!(
            self.db.meta_get(&meta::rewritten_noted(attempt.id)),
            Ok(Some(_))
        ) {
            return None;
        }
        let gh = self.github.as_ref()?;
        let repo = crate::model::gh_repo(&task.id)
            .map(str::to_string)
            .or_else(|| task.pr_url.as_deref().and_then(crate::model::pr_repo))
            .or_else(|| crate::git_credentials::repo_for_checkout(worktree))?;
        gh.branch_head(&repo, branch)
    }

    /// Say it: into the chat that is still standing in the checkout, and into
    /// the log whether or not there was one to reach.
    fn note_rewritten(
        &self,
        runtime: &dyn Runtime,
        task: &Task,
        attempt: &Attempt,
        branch: &str,
        remote_head: &str,
    ) {
        let key = meta::rewritten_noted(attempt.id);
        if matches!(self.db.meta_get(&key), Ok(Some(said)) if said == remote_head) {
            return;
        }
        let _ = self.db.meta_set(&key, remote_head);
        self.log.info(format!(
            "{}: {branch} has been rewritten on origin ({}) — the checkout {} still \
             holds the history from before it, and a push from there would force it back",
            task.identifier,
            &remote_head[..remote_head.len().min(12)],
            attempt.worktree.as_deref().unwrap_or("(none)"),
        ));
        // The same authorship test review delivery applies: the chat is this
        // attempt's only while it is still standing where the work is.
        let Some(chat) = attempt.pane_id.as_deref() else {
            return;
        };
        let reachable = runtime.chat_alive(chat).unwrap_or(false)
            && crate::review::still_the_authors_checkout(
                runtime.chat_cwd(chat).unwrap_or(None).as_deref(),
                attempt,
            );
        if !reachable {
            return;
        }
        let notice = rebased::rewritten_notice(branch, task.pr_base_ref.as_deref());
        match runtime.prompt(chat, &notice) {
            Ok(()) => self.log.info(format!(
                "{}: told chat {chat} that {branch} was rebased on origin",
                task.identifier
            )),
            Err(e) => self.log.warn(format!(
                "{}: could not tell chat {chat} about the rebase of {branch}: {e}",
                task.identifier
            )),
        }
    }
    // ---- clearing the shelf (gh#139) -------------------------------------

    /// Archive the chat of every attempt nobody is coming back to, once it has
    /// sat unclaimed for its route's `archive_chats`.
    ///
    /// Every dispatch creates a chat, and nothing but a hand ever archived one:
    /// at agent throughput a space's shelf is a landfill within days, and the
    /// six chats a person is actually working in are somewhere in it. The
    /// decisions are [`gc::chat_standing`] and [`gc::decide`] — the *same*
    /// decisions the worktree sweep makes, because a chat and a checkout are
    /// the same attempt's leavings and the honest rule is one rule.
    ///
    /// What the sweep will not touch, in the order it matters:
    ///
    /// - **A live or blocked attempt.** Both are open attempts, so
    ///   [`gc::standing`] reads them `Live`. An agent that stopped to ask at
    ///   02:00 is the single worst chat to archive.
    /// - **A task in review.** Review delivery ([`crate::review`]) asks
    ///   `chat_alive` before queueing comments into the author's chat, and an
    ///   archived chat answers no — archiving here would break the delivery
    ///   loop from under itself, silently, for exactly the tasks a human is
    ///   still working on.
    /// - **The pinned orchestrator**, whatever attempt it was dispatched as: it
    ///   is told about every settle on the board, so it is never finished.
    /// - **A chat that has released work the board is not finished with**
    ///   (gh#354). Read off [`gc::Dispatchers`], which is `dispatched_by_pane`
    ///   — the same edge [`SyncEngine::wake_dispatcher`] delivers a settle
    ///   notice on — turned around. A chat is a dispatcher because it
    ///   dispatched, not because somebody pinned it, and the two agents it is
    ///   waiting on are the whole reason somebody is sitting in it. The hold
    ///   runs until the *work* leaves the board rather than until the attempt
    ///   closes: the settle notice is delivered into this chat, so a hold that
    ///   ended at settle would end one sweep before the merge it is there for.
    /// - **A chat with no board attempt.** The sweep walks attempts, so a chat
    ///   somebody made by hand is never even a candidate. Those are theirs.
    ///
    /// Archiving is not deleting. The transcript is untouched, Settings →
    /// Archived puts it back, and a wrongly-settled attempt that goes back to
    /// work un-archives its own chat ([`SyncEngine::rewatch_settled_attempts`]).
    /// That is why this needs no grace beyond the window and no confirmation:
    /// the worst case is a shelf entry somebody restores in one click.
    ///
    /// That argument is about a *spent agent's* chat, and gh#354 is where it
    /// was found being made about something else. One click restores a
    /// transcript; it does not restore the place somebody was working from, and
    /// a person whose window vanished mid-dispatch has no reason to guess that
    /// "archived" is the word for what happened to it. The answer taken here is
    /// to keep that window out of the sweep's reach — the hold above — rather
    /// than to soften the sweep with a confirmation. Everything still archived
    /// is an ended attempt's chat, which is what the reversibility argument was
    /// always about.
    ///
    /// Failure is never fatal to the cycle: an error is logged and the attempt
    /// is left unstamped, so the next sweep tries again.
    fn archive_chats(&self, runtime: Option<&dyn Runtime>) {
        // A board that archives nothing anywhere pays for nothing: `off` on
        // `[defaults]` with no route asking otherwise ends the sweep here,
        // before a task is read.
        if !self.archives_anything() {
            return;
        }
        let now = chrono::Utc::now();
        let orchestrator = self.cfg.defaults.orchestrator();
        let tasks = match self.db.load_tasks() {
            Ok(t) => t,
            Err(e) => {
                self.log
                    .error(format!("chat archiving: reading the board: {e}"));
                return;
            }
        };
        // The same hold the checkouts get (gh#286): a parent whose layer is
        // still being written is not a finished conversation either.
        let dependents = rebased::Dependents::of(&tasks);
        // And who is still waiting on work they released (gh#354). Built once
        // from the whole board, for the same reason: the attempts a chat
        // dispatched sit under other tasks entirely.
        let dispatchers = gc::Dispatchers::of(&tasks, &dependents);
        for task in &tasks {
            // Per route, resolved now rather than stamped at dispatch: the
            // window is a property of the shelf the chat sits on, and an
            // attempt outlives the config that released it. A route since
            // renamed or deleted reads as the board-wide window, never as
            // "never" — see [`RoutingConfig::archive_chats_secs`].
            let route = self.cfg.resolve(&route_context(task));
            let Some(window) = self.cfg.archive_chats_secs(route) else {
                continue;
            };
            for attempt in &task.attempts {
                if !attempt.board_managed
                    || attempt.chat_archived_at.is_some()
                    || attempt.pane_id.is_none()
                {
                    continue;
                }
                let spent = attempt
                    .chat_archivable_at
                    .as_deref()
                    .map(|t| secs_since(t, now).unwrap_or(0));
                let standing =
                    gc::chat_standing(task, attempt, orchestrator, &dispatchers, &dependents);
                // Say why, but only where this hold is the operative one. A
                // chat its own open attempt, its pin or its review already
                // holds is not on the shelf because of what it dispatched, and
                // a line claiming otherwise sends the reader to a fact that is
                // not the one keeping it — the failure gh#194 was about. Asked
                // by re-running the same decision without the hold, so there is
                // no second copy of the rule to drift from the first.
                if let Some(chat) = attempt.pane_id.as_deref().filter(|c| dispatchers.holds(c))
                    && gc::chat_standing(
                        task,
                        attempt,
                        orchestrator,
                        &gc::Dispatchers::default(),
                        &dependents,
                    ) == gc::Standing::Spent
                {
                    self.note_held_as_dispatcher(task, attempt, chat, &dispatchers);
                }
                if let Err(e) = match gc::decide(standing, spent, window) {
                    gc::Verdict::Keep => Ok(()),
                    gc::Verdict::Mark => self.mark_chat_archivable(task, attempt, window),
                    gc::Verdict::Unmark => self.db.set_attempt_chat_archivable(attempt.id, false),
                    gc::Verdict::Collect => self.archive_one_chat(runtime, task, attempt),
                } {
                    self.log.warn(format!(
                        "{}: chat archiving on attempt {}: {e:#}",
                        task.identifier, attempt.id
                    ));
                }
            }
        }
    }

    /// Say, once, that a chat is being kept because it is somebody's dispatcher
    /// (gh#354) — the counterpart to [`SyncEngine::note_held_by_dependents`],
    /// and the answer to "why is this settled attempt's chat still on the
    /// shelf" for a hold nothing else on the row makes visible.
    fn note_held_as_dispatcher(
        &self,
        task: &Task,
        attempt: &Attempt,
        chat: &str,
        dispatchers: &gc::Dispatchers,
    ) {
        let key = meta::dispatcher_noted(attempt.id);
        if matches!(self.db.meta_get(&key), Ok(Some(_))) {
            return;
        }
        let _ = self.db.meta_set(&key, &crate::db::now());
        let released = dispatchers
            .released(chat)
            .iter()
            .map(|id| id.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        self.log.info(format!(
            "{}: keeping chat {chat} on the shelf — it released attempt {released}, \
             which the board is not finished with",
            task.identifier,
        ));
    }

    /// Does any route on this board archive its chats at all?
    ///
    /// The whole-sweep opt-out. `archive_chats` is per route, so `[defaults]
    /// archive_chats = "off"` alone does not settle it — a route may still
    /// name a window of its own, and one that does must still be swept.
    fn archives_anything(&self) -> bool {
        self.cfg.archive_chats_secs(None).is_some()
            || self
                .cfg
                .routes
                .iter()
                .any(|r| self.cfg.archive_chats_secs(Some(r)).is_some())
    }

    /// Start the shelf clock, and say so — the one notice anybody gets before a
    /// chat leaves the sidebar a week later.
    fn mark_chat_archivable(&self, task: &Task, attempt: &Attempt, window: u64) -> Result<()> {
        self.db.set_attempt_chat_archivable(attempt.id, true)?;
        self.log.info(format!(
            "{}: nothing is owed on chat {} any more — it archives in {}",
            task.identifier,
            attempt.pane_id.as_deref().unwrap_or("(none)"),
            gc::human_window(window),
        ));
        Ok(())
    }

    /// The window ran out: archive the chat through the runtime, and record it.
    ///
    /// A cycle run without a runtime marks and unmarks but archives nothing —
    /// the same split [`SyncEngine::collect_one`] makes, and for the same
    /// reason: only the process that hosts the workspace doc may mutate it, and
    /// everything else is welcome to keep the clock.
    fn archive_one_chat(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
    ) -> Result<()> {
        let Some(chat_id) = attempt.pane_id.as_deref() else {
            return Ok(());
        };
        let Some(runtime) = runtime else {
            return Ok(());
        };
        runtime.set_chat_archived(chat_id, true)?;
        self.db.set_attempt_chat_archived(attempt.id)?;
        self.log.info(format!(
            "{}: archived chat {chat_id} — its transcript is in Settings → Archived",
            task.identifier,
        ));
        Ok(())
    }

    // ---- a run that died under a chat that did not (§gh#390) --------------

    /// Is this attempt's chat still there, as [`runs::decide`] wants the
    /// question asked?
    ///
    /// The one call that separates "the chat was deleted" from "the engine
    /// restarted". Both failures answer [`runs::Liveness::Unknown`] — a cycle
    /// with no runtime and a runtime that would not answer — because neither of
    /// them is evidence about the chat, and an attempt must never end on the
    /// board's inability to look.
    fn chat_liveness(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        chat_id: &str,
    ) -> runs::Liveness {
        let Some(runtime) = runtime else {
            return runs::Liveness::Unknown;
        };
        match runtime.chat_alive(chat_id) {
            Ok(true) => runs::Liveness::Alive,
            Ok(false) => runs::Liveness::Gone,
            Err(e) => {
                self.log.warn(format!(
                    "{}: asking whether chat {chat_id} is still there: {e:#} — \
                     deciding nothing about it this tick",
                    task.identifier
                ));
                runs::Liveness::Unknown
            }
        }
    }

    /// How often this attempt's run has been started again already, counting
    /// the restarts the board did not perform (§gh#392).
    ///
    /// The board's own column is the easy half and the engine's ledger is the
    /// half that decides whether the other half means anything: boot recovery
    /// spends three revivals without writing a word to the board, and hands
    /// over a state indistinguishable from a first interruption. Asking is one
    /// call on the runtime that is already being asked whether the chat is
    /// there.
    ///
    /// A runtime that cannot answer leaves [`runs::Restarts::engine`] `None` —
    /// the board then restarts on its own budget exactly as it did before the
    /// question existed, and says nothing about a total it does not have.
    /// Logged at info, not warn: on a box whose chats run on another device
    /// this is the ordinary answer, and a warning per tick per attempt would
    /// bury the ones that mean something.
    fn restarts(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        chat_id: &str,
        board: i64,
    ) -> runs::Restarts {
        let Some(runtime) = runtime else {
            return runs::Restarts::board_only(board);
        };
        match runtime.chat_revivals(chat_id) {
            Ok(engine) => runs::Restarts { board, engine },
            Err(e) => {
                self.log.info(format!(
                    "{}: asking how often the engine revived chat {chat_id}: {e:#} — \
                     counting only the board's own restarts (§gh#392)",
                    task.identifier
                ));
                runs::Restarts::board_only(board)
            }
        }
    }

    /// Restart an interrupted run in the chat it was already in (§gh#390).
    ///
    /// The whole of the fix, and it is deliberately small: the chat holds the
    /// brief, the transcript and whatever the run had already done, and the
    /// checkout still stands on the branch. So there is nothing to re-create —
    /// one prompt puts an agent back in front of the same work. No attempt is
    /// spent, no chat is archived, no worktree is cut, and the row on the board
    /// never leaves `working`.
    ///
    /// Counted **before** the prompt and counted whatever the prompt reports,
    /// for [`SyncEngine::warn_overrun`]'s reason: a chat that will not take a
    /// prompt is a chat there is no point re-telling every cycle, and a resume
    /// that only counted when it landed would let an unreachable chat be
    /// restarted forever. The count is what bounds this
    /// ([`runs::MAX_RESUMES`]), so it must move on every try.
    ///
    /// `Ok(None)` when there was nothing to prompt with — a cycle without a
    /// runtime never reaches here through [`runs::decide`], but the type says
    /// so rather than the reader having to.
    ///
    /// The number this reports is the joined one (§gh#392): the board's column
    /// counts what the board spends, but the budget being spent covers the
    /// engine's own revivals too, so "restart 2 of 3" on an attempt whose
    /// column reads 1 is the sentence that is true.
    fn resume(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        chat_id: &str,
        restarts: runs::Restarts,
    ) -> Result<Option<notify::Interrupted>> {
        let Some(runtime) = runtime else {
            return Ok(None);
        };
        let restarts = runs::Restarts {
            board: self.db.note_resume(attempt.id)?,
            ..restarts
        };
        let resume = restarts.spent();
        let by_engine = match restarts.engine.filter(|e| *e > 0) {
            Some(engine) => format!(", {engine} of them the engine's own"),
            None => String::new(),
        };
        self.log.warn(format!(
            "{}: chat {chat_id} is still there but its run is not — restarting it in place \
             ({resume}/{}{by_engine}), attempt {} unspent",
            task.identifier,
            runs::MAX_RESUMES,
            attempt.id,
        ));
        if let Err(e) = runtime.prompt(chat_id, &runs::resume_prompt(resume)) {
            self.log.warn(format!(
                "{}: could not restart chat {chat_id}: {e:#}",
                task.identifier
            ));
        }
        Ok(Some(notify::Interrupted {
            identifier: task.identifier.clone(),
            chat: Some(chat_id.to_string()),
            fate: notify::Fate::Resumed {
                resume,
                of: runs::MAX_RESUMES,
            },
        }))
    }

    /// Close an attempt whose run has died once too often (§gh#390).
    ///
    /// `failed`, never `orphaned`: nothing vanished. The chat is on its shelf
    /// with the whole conversation in it, the branch is where the last run left
    /// it, and what went wrong is that this box could not keep a run alive —
    /// which is a red row somebody has to look at, not a row that quietly
    /// returns to `ready` for the board to try again on the same broken box.
    ///
    /// `restarts` rather than `attempt.resumes` because the note is evidence
    /// (§gh#392): the engine's boot recovery may have started this run several
    /// times before the board saw it was gone, and a count that omits those is
    /// the board being confidently wrong in the one sentence a person reads.
    fn give_up(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        restarts: runs::Restarts,
    ) -> Result<notify::Interrupted> {
        let note = runs::gave_up_note(restarts);
        self.db.close_attempt(attempt.id, Outcome::Failed)?;
        self.enqueue_outcome_note(task, Outcome::Failed, None, Some(&note))?;
        self.log
            .error(format!("{}: attempt failed — {note}", task.identifier));
        // The dispatcher waiting on this step hears it as the ending it is;
        // the incident notice below adds what it means about the box.
        self.announce(
            runtime,
            task,
            attempt,
            Signal::Settled {
                outcome: Outcome::Failed,
                evidence: None,
                pr_url: None,
                note: Some(note),
            },
        );
        Ok(notify::Interrupted {
            identifier: task.identifier.clone(),
            chat: attempt.pane_id.clone(),
            fate: notify::Fate::Ended,
        })
    }

    /// Say the incident once, as itself (§gh#390).
    ///
    /// The third bug in gh#390, and the one that made the other two hard to
    /// find: a restart that took down six runs surfaced only as six unrelated
    /// settle notices, so the board never said the sentence "the engine
    /// restarted and every live attempt was affected" — the sentence that would
    /// have pointed at the cause instead of at six tasks.
    ///
    /// One notice per cycle covering everything that cycle found, to the
    /// orchestrator and to the operator's webhook. Not to the dispatchers: a
    /// restarted attempt has not ended, and the agents waiting on those steps
    /// have nothing to do until it does.
    fn report_interrupted(&self, runtime: Option<&dyn Runtime>, runs: &[notify::Interrupted]) {
        if runs.is_empty() {
            return;
        }
        self.log.warn(notify::interrupted_summary(runs));
        self.tell_orchestrator(
            "runs interrupted",
            runtime,
            &notify::interrupted_message(runs),
        );
        if let Some(url) = self.webhook_url("the interrupted runs") {
            let body = notify::interrupted_payload(runs, &crate::db::now());
            match self.webhook.post(&url, &body) {
                Ok(()) => self.log.info(format!("interrupted runs posted to {url}")),
                Err(e) => self
                    .log
                    .warn(format!("posting the interrupted runs to {url}: {e:#}")),
            }
        }
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
                note: Some("its chat is gone".into()),
            },
        );
        Ok(())
    }

    // ---- settling (§settle-logic) -----------------------------------------

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

    /// The §settle-logic settle check: if this attempt's run has ended, weigh
    /// the artifacts and maybe close it. Returns whether it settled.
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
            && self.attempt_has_commits(
                attempt.worktree.as_deref(),
                self.attempt_base(attempt).as_deref(),
            )
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
                // gh#563, the orphaned answer: this arm with no open pull
                // request is where a review's answer lands after its pull
                // request merged — pushed to a branch whose door closed under
                // it. On the event path the board can still act instead of
                // narrating: open the pull request the work now needs.
                let reopened = match evidence {
                    Evidence::Commits if ask_github => self.reopen_after_merge(task, attempt),
                    _ => None,
                };
                let (new_url, clause) = match reopened {
                    Some(r) => (Some(r.url), Some(r.clause)),
                    None => (None, None),
                };
                self.settle(
                    runtime,
                    task,
                    attempt,
                    evidence,
                    new_url.as_deref().or(pr_url.as_deref()),
                    clause,
                )?;
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
    /// operator's out-of-band channel. The §settle-logic half of what
    /// herdr-board's `settle` did, now including its AGE-25 dispatcher wake
    /// (gh#71).
    ///
    /// `note` carries a clause the evidence alone does not describe — the
    /// re-opened answer of gh#563. It rides the settle signal's own note slot,
    /// beside whatever [`SyncEngine::note_credential_path`] adds, so every
    /// channel tells one story about one close.
    fn settle(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        evidence: Evidence,
        pr_url: Option<&str>,
        note: Option<String>,
    ) -> Result<()> {
        // The last honest moment to look at the checkout and the journal
        // (§gh#183). The event path settles the instant a run ends, so it can
        // close an attempt no interval reconcile has seen since its final
        // commit — and that commit is the one the review is about.
        //
        // The harvest goes first (§gh#235): the claims it may find are the
        // agent's own, and the snapshot below is what they are checked against.
        // Neither can fail the settle — a claimless attempt reaches its PR the
        // way it always did.
        self.harvest_claims(runtime, attempt);
        self.record_review_facts(runtime, attempt);
        self.db.close_attempt(attempt.id, Outcome::Done)?;
        self.enqueue_outcome(task, Outcome::Done, pr_url)?;
        self.log.info(format!(
            "{}: run ended with {} — attempt done",
            task.identifier,
            evidence.as_str()
        ));
        // Every settle is a settle on work that reached origin — a pull
        // request, or commits the branch check found on the remote — so this
        // is the moment to ask what pushed it (gh#233).
        let credential = self.note_credential_path(task, attempt);
        let note = match (note, credential) {
            (None, None) => None,
            (Some(one), None) => Some(one),
            (None, Some(two)) => Some(two),
            // Both clauses survive a combination: one says where the answer
            // went, the other who pushed it, and dropping either to keep the
            // sentence short would drop exactly what somebody was waiting on.
            (Some(one), Some(two)) => Some(format!("{one}; {two}")),
        };
        self.announce(
            runtime,
            task,
            attempt,
            Signal::Settled {
                outcome: Outcome::Done,
                evidence: Some(evidence),
                pr_url: pr_url.map(str::to_string),
                note,
            },
        );
        Ok(())
    }

    /// An attempt answered its review after the pull request merged; give the
    /// answer its pull request back (gh#563).
    ///
    /// The shape of the bug: review delivery queues a verdict into a live chat
    /// — by design — the agent acts on it and pushes, and by then the dispatcher
    /// has merged. The push lands on a branch whose pull request is closed, so
    /// nothing opens, the settle prints "no pull request was opened" exactly as
    /// if nobody had answered anything, and the work sits on a dead branch until
    /// somebody diffs it against main by hand. Three of those in one session
    /// (gh#527's review finding among them) is what this closes.
    ///
    /// The asymmetry is real — the board cannot know whether a verdict will
    /// produce a push, and holding merges is not a workflow — but the *detection*
    /// was never missing: this is the settle-on-commits path, reached only when
    /// the run genuinely ended on pushed work with no open pull request to show.
    /// What was missing was the action, so this takes the ones a first settle
    /// would have:
    ///
    /// - **a pull request**, from the branch at the base its merged one used,
    ///   recorded immediately (`set_pr`) so the row derives straight to review;
    /// - **the merge undone** as an operator decision — `finish_on_merge` set
    ///   `local_done` as a consequence of merging, and fresh reviewable work is
    ///   the fact that outranks it; cleared only here, only because
    ///   `task.pr_merged` says the done came from a merge rather than a
    ///   deliberate mark;
    /// - **the issue reopened**, through the writeback queue like the close was,
    ///   since a closed upstream otherwise derives `done` over any open PR.
    ///
    /// Guarded to the reported shape, so nothing else changes meaning:
    /// `ask_github` (event path only — no poll pays for this), a recorded
    /// *merged* PR on this attempt's own branch, and a branch GitHub says still
    /// carries commits the base does not (`compare_ahead`; zero means the merge
    /// already took everything — a push that raced the merge — and there is
    /// nothing to reopen). Every failure reads as "no reopen": the plain
    /// commits settle runs as before, which is today's behaviour plus the log
    /// line saying why nothing could be done.
    fn reopen_after_merge(&self, task: &Task, attempt: &Attempt) -> Option<Reopened> {
        let gh = self.github.as_ref()?;
        let branch = attempt.branch.as_deref()?;
        // The recorded pull request must be this attempt's own, closed, and
        // known to have merged — a deliberate `mark done` on a task whose PR is
        // merely closed leaves `local_done` alone (see above), and another
        // branch's PR is not this branch's door.
        if task.pr_number.is_none() || task.pr_open || !task.pr_merged {
            return None;
        }
        // Another branch's PR is not this branch's door.
        if task
            .pr_head_ref
            .as_deref()
            .is_some_and(|head| head != branch)
        {
            return None;
        }
        let repo = split_gh_task_id(&task.id)
            .map(|(r, _)| r)
            .or_else(|| crate::model::pr_repo(task.pr_url.as_deref()?))?;
        // Checked non-none above.
        let old = format!("{repo}#{}", task.pr_number?);
        let base = task.pr_base_ref.clone()?; // set when the PR was linked

        // What the merge left behind, asked of GitHub because the local
        // checkout has not seen the merge commit (no fetch on this path).
        let stranded = match gh.compare_ahead(&repo, &base, branch) {
            Some(ahead) => ahead,
            None => {
                self.log.warn(format!(
                    "{}: {branch} moved past its base after {old} merged, but the compare \
                     could not be read — leaving the commits where they are",
                    task.identifier
                ));
                return None;
            }
        };
        if stranded == 0 {
            // The push raced the merge and lost: everything it made is already
            // in the base. Nothing orphaned, nothing to open.
            self.log.info(format!(
                "{}: {branch} holds nothing that {old}'s merge does not — \
                 nothing to reopen",
                task.identifier
            ));
            return None;
        }

        let title = task.title.trim();
        let body = format!(
            "comet-board opened this pull request: attempt {} answered its review after \
             [{old}](https://github.com/{repo}/pull/{num}) had already merged, which left \
             {stranded} commit(s) on `{branch}` in nothing else.\n\n\
             The branch was cut from `{base}`; these commits were pushed after the merge and \
             are reviewed here as they stand.",
            task.attempt_count().max(1),
            num = task.pr_number.unwrap_or_default(),
        );
        let (number, url) = match gh.create_pull_request(&repo, title, branch, &base, &body) {
            Ok(opened) => opened,
            Err(e) => {
                // "No commits between" is GitHub answering the race question
                // itself, more recently than our compare did — treat it as the
                // zero-ahead case above rather than as a failure.
                if e.to_string().contains("No commits between") {
                    self.log.info(format!(
                        "{}: {branch} holds nothing that {old}'s merge does not — \
                         nothing to reopen",
                        task.identifier
                    ));
                } else {
                    self.log.warn(format!(
                        "{}: could not open a pull request for the answer on {branch}: {e:#}",
                        task.identifier
                    ));
                }
                return None;
            }
        };

        // Record before announcing, so every reader — the row derivation, the
        // notice, the outcome comment — sees the same new door.
        let _ = self.db.set_pr(&task.id, Some(&url), Some(number), true);
        let _ = self
            .db
            .set_pr_topology(&task.id, Some(&base), Some(branch), None);
        // The merge decided this row was over; the answer un-decides it. Only
        // reachable under `pr_merged`, so an operator's own mark stands.
        let _ = self.db.set_local_done(&task.id, false);
        // And upstream: finish_on_merge queued a close, and a closed issue
        // derives `done` over any open pull request. Same queue, same retry
        // guarantees; keyed per attempt, so each reopened answer reopens once.
        let queued = self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "reopen".into(),
            payload: json!({ "reason": "answered its review after the merge" }).to_string(),
            idem_key: format!("{}:reopen:{}", task.id, attempt.id),
        });
        if let Err(e) = queued {
            self.log.error(format!(
                "{}: queueing the issue reopen: {e}",
                task.identifier
            ));
        }

        self.log.warn(format!(
            "{}: the answer to its review arrived after {old} had merged — {stranded} \
             commit(s) were on {branch} and in nothing else; opened {url} to carry them, \
             and the row is back in review",
            task.identifier
        ));
        Some(Reopened {
            clause: format!(
                "pushed {stranded} commit(s) after {old} had merged — reopened as {repo}#{number}"
            ),
            url,
        })
    }

    /// Did the board's own credential push this? (gh#233.)
    ///
    /// The failure this exists for arrived from the inside. The first opencode
    /// dispatch could not exec the askpass helper, wrote a credential wrapper
    /// of its own, pushed with it, opened its pull request and finished green —
    /// and the board recorded a clean attempt, because a board that only
    /// watches outcomes cannot tell a sanctioned push from an improvised one.
    /// gh#68 kept the token out of argv, out of `.git/config` and out of the
    /// environment; none of that survives a wrapper script written under time
    /// pressure by an agent whose push just failed.
    ///
    /// So the ledger is consulted at the one moment there is something to
    /// compare it against. The board handed this run a credential and was never
    /// asked for it, or could not hand one over at all — yet here is work on
    /// origin. That is not proof of wrongdoing and the wording does not claim
    /// it is: it is the board saying, out loud and on the issue, that it cannot
    /// account for the credential that pushed.
    ///
    /// Returns the clause the settle notice carries, so the agent that released
    /// the work hears it too — an orchestrator collecting a finished step is
    /// exactly who needs to know the step's push went around the board.
    fn note_credential_path(&self, task: &Task, attempt: &Attempt) -> Option<String> {
        let chat = attempt.pane_id.as_deref()?;
        let record = credential_ledger::for_chat(&self.paths, chat);
        // Said whether or not the verdict below fires: a helper that failed and
        // then worked is a box on its way to gh#233, and the log is where that
        // is visible before it costs anybody a run.
        for failure in &record.failures {
            self.log.error(format!(
                "{}: the board's credential path failed during this attempt — {}",
                task.identifier,
                failure.summary()
            ));
        }
        if credential_is_sanctioned_at_settle(&self.paths, chat) {
            return None;
        }
        // The accusation binds to an origin that *moved* during this attempt
        // (gh#489). A repair or review-only re-run lands on a branch a
        // previous attempt already pushed, and every artifact the settle
        // rests on — the open pull request, the remote branch — predates it;
        // a run that pushed nothing had no reason to mint, and reading its
        // inherited branch as "work on origin" accused exactly the runs that
        // used the credential path correctly by not using it. Origin still
        // where the attempt found it means nothing was pushed by anybody, so
        // there is no push to account for. A branch that moved — pushed by
        // this run, force-pushed, or changed by another actor entirely —
        // stays on the old, loud footing: the board cannot account for the
        // credential that moved it. So does an attempt with no stamp, or one
        // whose origin cannot be read now: unproven reads as accused, never
        // as excused.
        if let (Ok(Some(start)), Some(branch)) = (
            self.db.meta_get(&meta::origin_at_start(attempt.id)),
            attempt.branch.as_deref(),
        ) && self
            .origin_branch_oid(task, attempt.worktree.as_deref(), branch)
            .is_some_and(|now| now == start)
        {
            self.log.info(format!(
                "{}: {branch} sits exactly where origin held it when this \
                 attempt began — nothing was pushed, so the unused credential \
                 path is in order",
                task.identifier
            ));
            return None;
        }
        let reason = record
            .last_failure()
            .map(|f| f.summary())
            .unwrap_or_else(|| "the helper was never asked".to_string());
        self.log.error(format!(
            "{}: this attempt's work is on origin, but the board's credential never pushed it \
             ({reason}) — the push used a credential the board did not issue",
            task.identifier
        ));
        let queued = self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "credential".into(),
            payload: json!({
                "attempt": task.attempt_count(),
                "branch": attempt.branch,
                "reason": reason,
                "log": self.paths.logfile().to_string_lossy(),
            })
            .to_string(),
            // Per attempt: a re-opened attempt that settles twice has one
            // credential story, and telling it twice on the issue would only
            // make it easier to scroll past.
            idem_key: format!("{}:credential:{}", task.id, attempt.id),
        });
        if let Err(e) = queued {
            self.log.error(format!(
                "{}: queueing the credential notice: {e}",
                task.identifier
            ));
        }
        Some("the board's credential did not push this — see the issue".to_string())
    }

    // ---- notification (gh#71) --------------------------------------------

    /// Tell whoever is not watching the board that something happened to a
    /// dispatched attempt.
    ///
    /// The two agent-facing channels are **one channel with a fallback hop**,
    /// not two switches (gh#165). The dispatcher is tried first because it is
    /// the precise addressee — it prompts the one agent whose plan that task
    /// was a step in — and the orchestrator gets what the dispatcher could not
    /// be given: work no agent released, and work whose dispatcher did not
    /// survive it. Told once or not at all; see [`crate::notify`] for why a
    /// pinned orchestrator only survives a busy board on that rule.
    ///
    /// The operator's webhook (`notify` + `notify_webhook`) is beside both and
    /// unconditional on either: it is not addressed to an agent. The fourth
    /// channel — the comment upstream — is not here at all: outcome comments
    /// are queued by [`SyncEngine::enqueue_outcome`] and the blocked comment by
    /// [`SyncEngine::note_blocked`], because those are retryable writebacks
    /// rather than best-effort notices.
    ///
    /// Nothing here can fail the caller. A settle that has already happened is
    /// not undone because a webhook host is down, and an attempt is not left
    /// open because a dispatcher's chat was archived.
    ///
    /// One announcement per thing that happened, not per time the board
    /// noticed it (§gh#356): a settle whose print matches the last one sent for
    /// this attempt stops here, on every channel.
    fn announce(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        signal: Signal,
    ) {
        if !self.is_news(task, attempt, &signal) {
            return;
        }
        match runtime {
            // The hop, in the order the addresses get more general.
            Some(rt) => {
                let dispatcher = self.wake_dispatcher(rt, task, attempt, &signal);
                if dispatcher != Told::Yes {
                    let orchestrator = self.wake_orchestrator(
                        Some(rt),
                        task,
                        attempt,
                        &notify::Event::Signal(&signal),
                    );
                    if orchestrator != Told::Yes {
                        self.note_unheard(task, &signal, dispatcher, orchestrator);
                    }
                }
            }
            // A cycle run without a runtime can settle rows but cannot prompt
            // anybody. One line rather than one per channel: the reason is the
            // same for both, and it is not about who was addressed.
            None => self.log.warn(format!(
                "{}: {} reached no agent — this cycle has no runtime to prompt a chat with",
                task.identifier,
                signal.event(),
            )),
        }
        self.post_webhook(task, attempt, &signal);
    }

    /// Is there anything in this signal the last one did not already say?
    /// (§gh#356.)
    ///
    /// An attempt settles more than once as a matter of routine. A settled chat
    /// that works again is re-opened rather than re-dispatched (§settle-logic's
    /// inverse), and the re-close settles it again — on a pull request that is
    /// still open, which needs no new work to be evidence. So a task sitting in
    /// `review` with an open PR re-announces its settle every time anything
    /// touches its chat: a review delivered into it, an operator's follow-up,
    /// the agent answering that it already handled the point. Two orion
    /// dispatches did exactly that on the box, and the dispatcher is the party
    /// that pays for it — each repeat is a wake-up and a round of "has anything
    /// actually changed?" against a branch head that had not moved.
    ///
    /// Not deduped by *event*, because some repeats are the feature working:
    /// the review that lands, the fix that is pushed, the second settle that
    /// genuinely follows. What separates them is whether anything the addressee
    /// can act on moved — a new commit, a new pull request, a different outcome
    /// — which is precisely what [`Signal::settle_print`] holds.
    ///
    /// Suppression covers every channel, the webhook included. An operator's
    /// endpoint has the same complaint as an orchestrator: a notification that
    /// arrives is read as something having happened.
    ///
    /// The mark records what was *announced*, not what was delivered. A notice
    /// the dispatcher's chat could not take is not owed a retry — the attempt
    /// is closed, the comment upstream is the durable trail, and re-sending it
    /// on the next reopen is the bug this closes rather than a recovery.
    fn is_news(&self, task: &Task, attempt: &Attempt, signal: &Signal) -> bool {
        let Some(print) = signal.settle_print(self.attempt_head(attempt).as_deref()) else {
            // A block. Told once per block already, counted by `blocked_count`
            // on the attempt, which is a state and not an event.
            return true;
        };
        let key = meta::settle_announced(attempt.id);
        if self.db.meta_get(&key).ok().flatten().as_deref() == Some(print.as_str()) {
            self.log.info(format!(
                "{}: {} says what the last one said — same outcome, same pull request, \
                 no new commit on {} — so nobody is being told twice",
                task.identifier,
                signal.event(),
                attempt.branch.as_deref().unwrap_or("its branch"),
            ));
            return false;
        }
        if let Err(e) = self.db.meta_set(&key, &print) {
            // A mark that could not be written costs a repeat on the next
            // settle, which is the failure this whole guard is about — but
            // dropping a notice over it would be the worse one.
            self.log.warn(format!(
                "{}: recording what {} said: {e}",
                task.identifier,
                signal.event(),
            ));
        }
        true
    }

    /// The commit this attempt's checkout is standing on, for
    /// [`SyncEngine::is_news`].
    ///
    /// Local `HEAD` rather than origin's, and never a fetch: the question is
    /// "did the agent do anything since the last notice?", the checkout answers
    /// it offline, and a settle path that reached across the network to decide
    /// whether to *speak* would be paying a poll for a notice.
    fn attempt_head(&self, attempt: &Attempt) -> Option<String> {
        let worktree = attempt.worktree.as_deref()?;
        git_out(worktree, &["rev-parse", "HEAD"]).filter(|h| !h.is_empty())
    }

    /// Announce a close the board did not settle: an attempt somebody ended
    /// from the panel, the phone or the CLI (gh#194).
    ///
    /// The board's own closes reach [`SyncEngine::announce`] through
    /// [`SyncEngine::settle`] and its neighbours, all of them inside this
    /// crate. A cancel is decided in the engine's board service instead —
    /// it owns the interrupt — and so ended an attempt on exactly the channels
    /// a settle uses minus every one of them. The operator who pressed the key
    /// knows; the agent that released the work and is waiting on that step
    /// does not, and it is the party with something to do about it.
    ///
    /// `note` is what the outcome comment says upstream, so the chat and the
    /// issue describe one close in one wording.
    pub fn announce_ended(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        outcome: Outcome,
        note: &str,
    ) {
        self.announce(
            runtime,
            task,
            attempt,
            Signal::Settled {
                outcome,
                evidence: None,
                pr_url: None,
                note: Some(note.to_string()),
            },
        );
    }

    /// The line that keeps a dropped notice from being a silence.
    ///
    /// Both agent-facing addresses came up empty, so this event is now the row
    /// colour, the webhook and the comment on the issue — which is a legitimate
    /// board (an operator at the panel with nothing pinned is exactly it) and an
    /// invisible one when it is not. Before gh#165 the undeliverable case said
    /// nothing at all, and "the orchestrator never told me" was indistinguishable
    /// from "the orchestrator has nothing to say".
    fn note_unheard(&self, task: &Task, signal: &Signal, dispatcher: Told, orchestrator: Told) {
        let first = match dispatcher {
            Told::Yes => return,
            Told::NoOne => "no chat released it",
            Told::Unreachable => "the chat that released it is gone",
            Told::Off => "`notify_dispatcher` is off",
            Told::Itself => "`--via` named the attempt's own chat",
        };
        let second = match orchestrator {
            Told::Yes => return,
            // The pin has no switch: unset *is* nobody to tell.
            Told::NoOne | Told::Off => "no orchestrator is pinned",
            Told::Unreachable => "the pinned orchestrator could not be told",
            Told::Itself => "the pin is this attempt's own chat",
        };
        self.log.warn(format!(
            "{}: {} reached no agent — {first}, and {second}; the board row and the comment \
             on the issue are the whole trail",
            task.identifier,
            signal.event(),
        ));
    }

    /// Queue a settle or block notice into the chat of the agent that released
    /// this work (herdr-board's AGE-25, made the first hop by gh#165).
    ///
    /// The provenance is already on the attempt row — `dispatched_by_pane` is
    /// the chat the dispatch ran from, recorded for every agent-issued
    /// dispatch whether or not the board started that chat. So the delivery is
    /// the same [`Runtime::prompt`] a review uses: a steer into a live run, a
    /// send otherwise, durable either way.
    ///
    /// The three answers are all meaningful to [`SyncEngine::announce`], which
    /// is why this returns [`Told`] and not a bool: [`Told::NoOne`] is an
    /// operator's dispatch and [`Told::Unreachable`] is a dispatcher that did
    /// not survive its child, and both of those are the orchestrator's to hear.
    ///
    /// **Every exit says why in the log** (gh#194). It used to have three that
    /// did not — the switch, an attempt nobody released, and a chat that
    /// dispatched into itself — and the symptom of any of them was a settle
    /// with no notice line of any kind against it. That is a state a `grep` for
    /// the task cannot tell from a settle path that never ran, and telling
    /// those two apart on the box took a read of the whole log.
    fn wake_dispatcher(
        &self,
        runtime: &dyn Runtime,
        task: &Task,
        attempt: &Attempt,
        signal: &Signal,
    ) -> Told {
        // The address is read before the switch, so the line each exit writes
        // names the thing to go and fix. `notify_dispatcher` off on an attempt
        // nobody released is not a routing decision that lost a notice — it is
        // an empty address, and saying "the switch is off" about it would send
        // an operator to a knob that changes nothing.
        let Some(chat) = attempt.dispatched_by_pane.as_deref() else {
            self.log.info(format!(
                "{}: {} has no chat that released it — an operator's dispatch records \
                 none, so this is the orchestrator's to hear",
                task.identifier,
                signal.event(),
            ));
            return Told::NoOne;
        };
        // A chat that dispatched into itself would be prompted about its own
        // settle. Not reachable through `comet-board dispatch` (a dispatch
        // makes a fresh chat), but a hand-set `--via` can say anything.
        if Some(chat) == attempt.pane_id.as_deref() {
            self.log.warn(format!(
                "{}: {} was not queued into {chat} — that is the attempt's own chat, so \
                 `--via` named the agent rather than whoever released it",
                task.identifier,
                signal.event(),
            ));
            return Told::Itself;
        }
        if !self.cfg.defaults.notify_dispatcher {
            self.log.warn(format!(
                "{}: {} was not queued into the chat that released it ({chat}) — \
                 `[defaults] notify_dispatcher` is off",
                task.identifier,
                signal.event(),
            ));
            return Told::Off;
        }
        // The dispatcher is usually a long-lived orchestrator that outlives
        // many children, but attempts cap at two hours and chats archive as
        // their task settles (§gh#139), so a dispatcher that did not survive
        // its own child is ordinary rather than exceptional. Not an error —
        // just a notice that now needs a different addressee.
        if !self.chat_can_be_told(runtime, &task.identifier, "the chat that released it", chat) {
            return Told::Unreachable;
        }
        let text = notify::dispatcher_message(task, attempt, signal);
        match runtime.prompt(chat, &text) {
            Ok(()) => {
                self.log.info(format!(
                    "{}: {} queued into the chat that released it ({chat})",
                    task.identifier,
                    signal.event(),
                ));
                Told::Yes
            }
            // Best effort by design: the attempt is closed and the tracker has
            // the trail. Retrying a notice about a thing that already happened
            // is how a dispatcher gets told twice — but a hop to the
            // orchestrator is not a retry, it is a different reader.
            Err(e) => {
                self.log.warn(format!(
                    "{}: could not notify chat {chat}: {e}",
                    task.identifier
                ));
                Told::Unreachable
            }
        }
    }

    /// Queue an event into the pinned orchestrator's chat (gh#104).
    ///
    /// The addressee of last resort. `wake_dispatcher` answers "the agent that
    /// released this is waiting on it"; this one answers "somebody has to hear
    /// it and there is nobody else" — work an operator released from the panel
    /// or the phone, work whose dispatcher did not survive it, and the cap
    /// warning, which belongs to no dispatcher at all because the attempt it is
    /// about is still running.
    ///
    /// It is only reached when the dispatcher was not told, which is
    /// [`SyncEngine::announce`]'s rule rather than this function's: an
    /// orchestrator whose context fills with a copy of every child's settle is
    /// one that cannot hold a train of thought, and that was the real content
    /// of the warning gh#71 attached to the *dispatcher* wake.
    ///
    /// Three more things it will not do, each because the alternative is an
    /// agent talking to itself:
    ///
    /// - Prompt the orchestrator about its own chat's attempt. Only reachable
    ///   when somebody pins a board-dispatched chat, which `doctor` names.
    /// - Retry. Same reason the dispatcher notice does not: a notice about a
    ///   thing that already happened, delivered twice, is worse than one that
    ///   was dropped with a line in the log.
    /// - Poll. One prompt per event and no other traffic at all is the whole
    ///   budget — an orchestrator exempt from the duration cap lives forever,
    ///   so its notice volume is the only thing bounding what it costs.
    fn wake_orchestrator(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
        event: &notify::Event,
    ) -> Told {
        let Some(chat) = self.cfg.defaults.orchestrator() else {
            return Told::NoOne;
        };
        if Some(chat) == attempt.pane_id.as_deref() {
            self.log.warn(format!(
                "{}: the pinned orchestrator ({chat}) is this attempt's own chat — it is not \
                 prompted about itself; `doctor` names the pin",
                task.identifier
            ));
            return Told::Itself;
        }
        self.tell_orchestrator(
            &task.identifier,
            runtime,
            &notify::orchestrator_message(task, attempt, event),
        )
    }

    /// Queue one already-composed message into the pinned orchestrator's chat.
    ///
    /// The delivery half of [`SyncEngine::wake_orchestrator`], split out for
    /// the notices that belong to no single attempt (§gh#390's incident line):
    /// everything from "is anybody pinned" down is identical, and the half that
    /// is not — refusing to prompt an orchestrator about its own attempt — is a
    /// question only a per-attempt event can ask.
    ///
    /// `subject` is what the log lines name, so a reader greps a task
    /// identifier or an incident and finds the same shape of line either way.
    fn tell_orchestrator(&self, subject: &str, runtime: Option<&dyn Runtime>, text: &str) -> Told {
        let Some(chat) = self.cfg.defaults.orchestrator() else {
            return Told::NoOne;
        };
        let Some(runtime) = runtime else {
            self.log.warn(format!(
                "{subject}: no runtime to reach the orchestrator ({chat}) with"
            ));
            return Told::Unreachable;
        };
        if !self.chat_can_be_told(runtime, subject, "the pinned orchestrator", chat) {
            return Told::Unreachable;
        }
        match runtime.prompt(chat, text) {
            Ok(()) => {
                self.log.info(format!(
                    "{subject}: queued into the orchestrator's chat {chat}"
                ));
                Told::Yes
            }
            Err(e) => {
                self.log.warn(format!(
                    "{subject}: could not reach the orchestrator ({chat}): {e}"
                ));
                Told::Unreachable
            }
        }
    }

    /// Is this chat still there to be prompted?
    ///
    /// Shared by both agent-facing channels because the answer means the same
    /// thing for both: a chat that has been archived is not an error, it is
    /// simply nobody to tell, and an unreadable answer is a reason to say
    /// nothing rather than to prompt into the dark.
    /// `role` names which of the two channels is asking, so the line reads as
    /// the answer to a question somebody asked rather than as a stray fact
    /// about a chat id (gh#194) — an operator grepping a task's whole life
    /// should not have to hold the routing order in their head to know which
    /// notice went nowhere.
    fn chat_can_be_told(
        &self,
        runtime: &dyn Runtime,
        subject: &str,
        role: &str,
        chat: &str,
    ) -> bool {
        match runtime.chat_alive(chat) {
            Ok(true) => true,
            Ok(false) => {
                self.log.info(format!(
                    "{subject}: {role} ({chat}) is gone — nothing delivered there"
                ));
                false
            }
            Err(e) => {
                self.log
                    .warn(format!("{subject}: checking {role} ({chat}): {e}"));
                false
            }
        }
    }

    /// POST the event at `[defaults] notify_webhook`, if the operator wants it.
    ///
    /// The only channel that reaches somebody who is looking at neither the
    /// board nor the issue tracker — which at 02:00 is everybody.
    fn post_webhook(&self, task: &Task, attempt: &Attempt, signal: &Signal) {
        let Some(url) = self.webhook_url(&task.identifier) else {
            return;
        };
        let url = url.as_str();
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

    /// Where the operator wants to be told, if they want to be told at all.
    ///
    /// Both refusals — the switch off, no URL set — are silent, because neither
    /// is a failure. The third is not: a URL that is set and unusable is a
    /// channel the operator believes they have, so it says so, naming what went
    /// unannounced through it.
    fn webhook_url(&self, subject: &str) -> Option<String> {
        if !self.cfg.defaults.notify {
            return None;
        }
        let url = self.cfg.defaults.notify_webhook.as_deref()?;
        if let Some(problem) = notify::webhook_url_problem(url) {
            self.log.warn(format!(
                "[defaults] notify_webhook is unusable ({problem}); {subject} went unannounced"
            ));
            return None;
        }
        Some(url.to_string())
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

    /// Look again at attempts the board has already closed (§settle-logic's
    /// inverse, herdr gh#34: "a settle the board got wrong").
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
    fn rewatch_settled_attempts(
        &self,
        statuses: &SessionStatuses,
        runtime: Option<&dyn Runtime>,
    ) -> Result<bool> {
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
            // And back onto the shelf, in the same motion (gh#139): the work
            // is live again, so the chat it is happening in belongs in the
            // sidebar rather than in Settings → Archived. Its clock is cleared
            // with it, so the next time this attempt finishes it is owed a
            // whole window.
            self.unarchive_chat(runtime, &task, &attempt)?;
            // And the build output it is about to write again is a fresh cache,
            // not the one this attempt was already swept for (gh#186): the stamp
            // has to go, or the 36 GB the resumed agent builds would sit behind a
            // row the sweep skips forever.
            self.db.clear_attempt_cache_swept(attempt.id)?;
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

    /// Put a re-opened attempt's chat back on its space's shelf (gh#139).
    ///
    /// Only for a chat the *board* archived — `chat_archived_at` is the record
    /// of that. A chat an operator archived by hand is theirs, and un-archiving
    /// it here would be the board arguing with them about their own sidebar.
    ///
    /// The stamps are cleared whether or not the un-archive lands: the board's
    /// claim on this chat is over either way, and a stamp left behind would
    /// keep an attempt out of the sweep forever. A failure is a warning, not an
    /// error — the reopen itself has already happened and is worth more than
    /// where the chat sits.
    fn unarchive_chat(
        &self,
        runtime: Option<&dyn Runtime>,
        task: &Task,
        attempt: &Attempt,
    ) -> Result<()> {
        if attempt.chat_archived_at.is_none() && attempt.chat_archivable_at.is_none() {
            return Ok(());
        }
        if attempt.chat_archived_at.is_some()
            && let (Some(runtime), Some(chat_id)) = (runtime, attempt.pane_id.as_deref())
            && let Err(e) = runtime.set_chat_archived(chat_id, false)
        {
            self.log.warn(format!(
                "{}: chat {chat_id} is working again but could not be un-archived: {e:#} \
                 — Settings → Archived still has it",
                task.identifier
            ));
        }
        self.db.clear_attempt_chat_archived(attempt.id)
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
    /// - **Settling (§settle-logic).** A transition onto a settled-looking
    ///   status *is* the run-end event arriving; this is the "the turn ended,
    ///   now check the checkout" moment the run journal replaced the 60-second
    ///   clock with. Checked on the transition only — the interval reconcile
    ///   owns the steady re-check, so a burst of unchanged snapshots costs no
    ///   git.
    /// - **Re-opening (§settle-logic's inverse).** comet's `Working` is
    ///   written by the engine that runs the agent and staleness-gated on the
    ///   way in, so — unlike herdr's screen-sampled `working` — acting on it
    ///   immediately cannot flap a finished row.
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
        if self.rewatch_settled_attempts(statuses, runtime)? {
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
        let tasks = self.db.load_tasks()?;
        // Built once for the whole board, exactly as `board_rows` builds it: the
        // layer below a row is another row, and asking per task would be a scan
        // per task (gh#289).
        let deps = crate::stacks::Dependents::of(&tasks);
        for task in tasks {
            let state = derive_state(derivation_for(
                &task,
                override_status,
                deps.changes_below(&task.id).is_some(),
            ));
            if state != task.state {
                self.log
                    .info(format!("{}: {} → {}", task.identifier, task.state, state));
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

    /// `ran` is what the attempt actually runs under — the dispatch's overrides
    /// where it made them, not the route's defaults (gh#232). The comment
    /// outlives the chat, the row and the worktree, so it is the one surface
    /// that cannot afford to name the route instead.
    ///
    /// `via` is the dispatcher already named for a reader — an issue identifier,
    /// the human who released it, or the chat an orchestrator is running in.
    /// `None` is a dispatch the board can put no name to, and says nothing
    /// upstream.
    pub fn enqueue_dispatch(
        &self,
        task: &Task,
        ran: RanOn<'_>,
        workspace: &str,
        attempt_no: usize,
        via: Option<&str>,
        billed_to: Option<&str>,
    ) -> Result<()> {
        // Name the parent upstream too: reading the issue should tell you an
        // agent released this, not a person.
        self.db.enqueue_writeback(&NewWriteback {
            task_id: task.id.clone(),
            kind: "dispatch".into(),
            payload: json!({
                "runtime": ran.runtime,
                // Absent when the dispatch named none, which is the harness
                // default and not a fact this board can spell — the comment
                // says the runtime alone rather than guessing at a model id.
                "model": ran.model,
                "workspace": workspace,
                "attempt": attempt_no,
                "via": via,
                // Whose subscription this run spends, when it is not the
                // releaser's (gh#101). Present only then, and on purpose: the
                // trail belongs on the issue where both parties can see it, and
                // a line that named the payer on every dispatch would say
                // nothing on the one dispatch where it matters.
                "billed_to": billed_to,
            })
            .to_string(),
            idem_key: format!("{}:dispatch:{}", task.id, attempt_no),
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
            // Legacy rows (gh#471). The connector is gone, so there is nothing
            // to deliver to — and an existing database may still hold effects
            // queued before the removal. They leave the queue as dropped, not
            // as failed: a retry would back off forever against a source the
            // board can no longer reach.
            Source::Linear => {
                return Ok(Sent::Dropped(format!(
                    "Linear is no longer a connected source ({})",
                    task.identifier
                )));
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
                        gh.comment(&repo, number, &dispatch_comment(&payload))?;
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
                    "credential" => {
                        gh.comment(&repo, number, &credential_comment(&payload))?;
                    }
                    // Close on done, so "mark done" reaches the issue too.
                    "close" => gh.close_issue(&repo, number)?,
                    // The inverse close (gh#563): an answer that arrived after
                    // its merge re-opened the work, and the issue follows.
                    "reopen" => gh.reopen_issue(&repo, number)?,
                    other => self.log.warn(format!("unknown writeback kind {other}")),
                }
            }
        }
        Ok(Sent::Upstream)
    }

    /// Merge the pull request on a task, if it has one.
    ///
    /// Asynchronous end to end (gh#290): the submission comes back `pending` and
    /// the board waits out [`MERGE_POLL_TRIES`] of it before answering. What it
    /// answers is what actually happened — merged, queued, or still running —
    /// because merging a layer of a stack merges every layer beneath it as one
    /// group, and three pull requests take longer than one.
    ///
    /// The row is only marked merged when GitHub says `merged`. A merge that
    /// entered the queue, or that is still running when the wait is over, leaves
    /// the row where it is: `link_pull_requests` reads `merged` off every pull
    /// request it polls and calls the same [`SyncEngine::finish_on_merge`] this
    /// does, so a merge that lands a minute later lands on the board too,
    /// without a marker column holding a fact GitHub is already answering.
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
            .or_else(|| crate::model::pr_repo(task.pr_url.as_deref()?))
            .ok_or_else(|| {
                anyhow::anyhow!("cannot tell which repo {} belongs to", task.identifier)
            })?;

        let submitted = gh.merge_pr(&repo, number)?;
        let status =
            gh.await_merge(&repo, number, submitted, MERGE_POLL_EVERY, MERGE_POLL_TRIES)?;
        self.log.info(format!(
            "{repo}#{number} for {}: {}",
            task.identifier,
            status.note()
        ));
        if status != MergeStatus::Merged {
            // Nothing landed, so nothing on the row may say it did. The next
            // poll is what turns a queued or still-running merge into a done
            // row — and it is the same path a merge made on the web takes.
            return Ok(format!("{repo}#{number} is {}", status.note()));
        }

        // Reflect it immediately rather than waiting for a poll: the operator
        // just pressed the key and needs the row to move.
        self.db
            .set_pr(&task.id, task.pr_url.as_deref(), Some(number), false)?;
        self.db.set_pr_merged(&task.id, true)?;
        self.finish_on_merge(task, &repo, number)?;
        self.rederive_all()?;
        // The other half of gh#563: a review delivered into the agent's chat
        // minutes ago is the one whose answer can land on this now-merged
        // branch. The board reopens such an answer as its own pull request —
        // but the keypress is the moment somebody is still watching, so say it
        // here too rather than leaving the sentence to a settle notice that may
        // be hours away.
        let mut answer = format!("{repo}#{number} merged");
        if let Some(ago) = crate::review::recent_delivery(&self.db, &task.id) {
            self.log.warn(format!(
                "{}: {repo}#{number} merged {} after a review reached the agent's chat — \
                 if it answers with commits, the board will reopen them as a new pull request",
                task.identifier, ago
            ));
            answer.push_str(
                " — a review reached the agent's chat just before this merge; \
                 if it answers with commits, the board will reopen them as a new pull request",
            );
        }
        // A sentence, like every other outcome: the caller shows this to the
        // person who pressed the key, whichever surface that was (gh#408).
        Ok(answer)
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
            // Legacy (gh#471): nothing polls Linear, so it has no health to
            // report — absent, on every board, whatever an old database says.
            Source::Linear => return SourceHealth::Absent,
            Source::Github => (
                self.github.is_some(),
                meta::GITHUB_STATUS,
                meta::GITHUB_FAILURES,
            ),
        };
        // A recorded status means *something* polled this source, even if this
        // process built no client for it — a reader's own credentials say
        // nothing about whether the engine's loop has any. Gating on
        // `configured` alone made the board omit the source's ✓ while the
        // loop was happily polling it.
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
///
/// `changes_below` is the one fact in here that is not on the task's own rows —
/// whether a layer underneath it has been asked to change (gh#289) — so a caller
/// with one task and no board around it passes `false` and gets the answer this
/// row would have had before stacks existed.
pub fn derivation_for(
    task: &Task,
    override_status: &HashMap<String, AgentStatus>,
    changes_below: bool,
) -> Derivation {
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
        changes_below,
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

/// The gh#233 notice, composed at delivery like every other comment.
fn credential_comment(payload: &Value) -> String {
    notify::credential_comment(
        payload["attempt"].as_u64().unwrap_or(1),
        payload["branch"].as_str(),
        payload["reason"]
            .as_str()
            .unwrap_or("the helper was never asked"),
        payload["log"].as_str().unwrap_or("(none)"),
    )
}

/// The line a dispatch leaves on the issue, rendered from the queued payload
/// (gh#232).
///
/// One function because there is one sentence: when the board wrote to two
/// sources it had the same `format!` twice, and a fix to either was a fix to
/// one half of the board's readers.
///
/// The model rides beside the runtime when the dispatch named one. For a board
/// spreading work across harnesses that is the half worth having — `codex` says
/// what ran, `codex · gpt-5.6-luna` says what to compare the next attempt
/// against. Absent means the harness default, which the board does not know and
/// so does not name.
fn dispatch_comment(payload: &Value) -> String {
    let field = |key: &str| payload[key].as_str().filter(|v| !v.trim().is_empty());
    let segment = |prefix: &str, value: Option<&str>| match value {
        Some(v) => format!(" · {prefix}{v}"),
        None => String::new(),
    };
    format!(
        "Dispatched to comet · {}{} · space:{} · attempt {}{}{}",
        field("runtime").unwrap_or("?"),
        segment("", field("model")),
        field("workspace").unwrap_or("?"),
        payload["attempt"].as_u64().unwrap_or(1),
        segment("dispatched by ", field("via")),
        dispatch_billing_suffix(payload),
    )
}

/// What a dispatch comment appends when the run spends somebody else's
/// subscription (gh#101) — empty when it does not, which is most of them.
///
/// The record is written upstream rather than kept on the box on purpose: the
/// two people it concerns are the teammate who released the work and the owner
/// whose plan pays for it, and the issue is the one place both of them look.
fn dispatch_billing_suffix(payload: &Value) -> String {
    match payload["billed_to"].as_str().filter(|b| !b.is_empty()) {
        Some(billed) => comet_proto::view::board::bills_comment_suffix(billed),
        None => String::new(),
    }
}

/// Which pull request a task links to, and what the rest of its stack says
/// about the state of the work (gh#287).
#[derive(Debug)]
struct Linked<'a> {
    /// The pull request the row points at: the *bottom* layer, which for an
    /// unstacked attempt is the only one there is. It is the branch the board
    /// named and recorded on the attempt, so it is the one every other part of
    /// the board can recognise — `authoring_attempt` (review delivery) and
    /// `adopt` both match a pull request to an attempt by that exact name.
    pr: &'a PullRequest,
    /// Is any layer still open? The attempt's work is not over while one is:
    /// the GC holds a checkout for an open pull request, and a stack whose
    /// bottom merged first would otherwise let go of the worktree the layers
    /// above are still being written in.
    open: bool,
    /// Have *all* the layers landed? False while one is open, and false for a
    /// stack that has none — a pull request closed without merging is not a
    /// merge, whether it stood alone or held up four others.
    merged: bool,
}

/// The pull requests among `pulls` that belong to a task with these attempt
/// `branches`, resolved into the one the row links to and the verdict of the
/// stack around it.
///
/// `own_repo` is the repository a GitHub task owns, and it is the scope: branch
/// names are not unique across repositories — `gh#2` in two repos both branch to
/// `board/gh-2` — so matching on the branch alone attached another repo's merged
/// pull request to this task and derived it straight to review (herdr-board
/// AGE-20). Branch names have never had to answer for that and since gh#364 do
/// not pretend to: they carry a slug of the title where the repo used to be.
/// `None` for a Linear task, whose identifier is globally unique and whose pull
/// request is honoured in whichever repo it turns up in.
///
/// Ordering is (layer, then newest first), so the link is the bottom layer, and
/// within one branch the most recent pull request on it — a branch whose first
/// request was closed and reopened as a second links to the second, which is
/// what the poll's own ordering already gave in practice.
fn link_for<'a>(
    pulls: &'a [PullRequest],
    branches: &[String],
    own_repo: Option<&str>,
) -> Option<Linked<'a>> {
    let mut matched: Vec<(i64, &PullRequest)> = pulls
        .iter()
        .filter(|p| own_repo.is_none_or(|r| p.repo == r))
        .filter_map(|p| Some((layer_for(p, branches)?, p)))
        .collect();
    matched.sort_by_key(|(layer, p)| (*layer, std::cmp::Reverse(p.number)));
    // One representative per branch: the newest request on it. Without this a
    // layer that was opened, closed and reopened would be counted twice, and
    // the stale half would hold `merged` down forever.
    let mut layers: Vec<&PullRequest> = Vec::new();
    for (_, pr) in matched {
        if !layers.iter().any(|seen| seen.head_ref == pr.head_ref) {
            layers.push(pr);
        }
    }
    let pr = *layers.first()?;
    let open = layers.iter().any(|p| p.open);
    // Something landed and nothing is still open. A layer closed *without*
    // merging counts as neither: it is work the agent withdrew, and holding
    // `merged` down on it would leave a stack that can never read as finished.
    let merged = layers.iter().any(|p| p.merged) && !open;
    Some(Linked { pr, open, merged })
}

/// Which layer of a task's stack this pull request is, if it is one of theirs:
/// 1 for a request on an attempt branch itself, `n` for the `n`th layer of a
/// stack cut from one (gh#287), `None` for a stranger.
fn layer_for(pr: &PullRequest, branches: &[String]) -> Option<i64> {
    branches
        .iter()
        .filter_map(|branch| {
            if pr_matches_branch(pr, branch) {
                Some(1)
            } else if pr.stack.is_some() {
                // Only a request GitHub itself calls stacked may be read into
                // an attempt on the strength of its name — see
                // `AttemptBranches::claims_as_layer`.
                crate::stacks::layer_of(&pr.head_ref, branch)
            } else {
                None
            }
        })
        .min()
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
            || self.claims_as_layer(pr)
    }

    /// The same question for a layer of an agent-authored stack (gh#287): the
    /// branch was cut *from* an attempt branch and named for it, so the pull
    /// request on it is that attempt's work even though the board never named
    /// the branch. Without this every layer above the first arrives as a row of
    /// its own — dispatchable, reviewed by nobody, and duplicating work that is
    /// already on the board.
    ///
    /// Gated on the pull request carrying GitHub's own `stack` object, which is
    /// the difference between a layer and a branch that merely reads like one.
    /// A scan rather than a lookup, so it runs only for the handful of pull
    /// requests that say they are stacked.
    fn claims_as_layer(&self, pr: &PullRequest) -> bool {
        if pr.stack.is_none() {
            return false;
        }
        let in_repo = self.in_repo.get(&pr.repo).into_iter().flatten();
        self.anywhere
            .iter()
            .chain(in_repo)
            .any(|branch| crate::stacks::layer_of(&pr.head_ref, branch).is_some())
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

/// The route context for a task, from its stored fields. A legacy Linear row
/// names no repo, so only its labels can match a route.
pub fn route_context(task: &Task) -> RouteContext {
    RouteContext {
        gh_repo: crate::model::gh_repo(&task.id).map(str::to_string),
        labels: task.labels.clone(),
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

/// The regexes that find a test declaration in a whole tree (§gh#236).
///
/// The `git grep` half of [`crate::effects::is_test_decl`], and deliberately
/// the same rules: the two numbers on the tests chip are counted by this, in
/// both trees, so a rule that misses a language misses it symmetrically and the
/// *pair* stays honest even where the total is low. Searching every tracked
/// file rather than a pathspec per language costs the odd markdown code fence,
/// on both sides, for the same reason.
const TEST_PATTERNS: &[&str] = &[
    r"^[[:space:]]*#\[(tokio::|async_std::)?test\]",
    r"^[[:space:]]*#\[(rstest|test_case)",
    r"^func Test",
    r"^[[:space:]]*(async )?def test_",
    r"^[[:space:]]*(it|test)\(",
    r"^[[:space:]]*func test",
];

/// How many test declarations one tree holds, or `None` when git could not say.
///
/// `None` is not zero and the chip renders it as neither: a suite the board
/// could not count is a suite it must not report a count for.
fn test_total(worktree: &str, rev: &str) -> Option<u32> {
    let mut args: Vec<&str> = vec!["grep", "-I", "-c", "-E"];
    for pattern in TEST_PATTERNS {
        args.push("-e");
        args.push(pattern);
    }
    args.push(rev);
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(&args)
        .output()
        .ok()?;
    match out.status.code() {
        // `git grep -c` prints `rev:path:count` per file.
        Some(0) => Some(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|line| line.rsplit_once(':'))
                .filter_map(|(_, count)| count.trim().parse::<u32>().ok())
                .sum(),
        ),
        // Exit 1 is git grep's "nothing matched", which is a count.
        Some(1) => Some(0),
        _ => None,
    }
}

/// Every line in one tree that names `symbol` as a whole word.
///
/// `-F` and `-w`: a fixed string on a word boundary, so `note` cannot answer
/// for `shelf_note` — the same exactness [`crate::claims::names_symbol`] holds
/// to, and for the same reason. `-h` drops the `rev:path:` prefix, so what
/// comes back is the lines themselves and [`crate::effects::count_call_sites`]
/// can read them.
fn git_grep(worktree: &str, symbol: &str, rev: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["grep", "-h", "-I", "-w", "-F", "-e", symbol, rev])
        .output()
        .ok()?;
    match out.status.code() {
        Some(0) => Some(String::from_utf8_lossy(&out.stdout).to_string()),
        Some(1) => Some(String::new()),
        _ => None,
    }
}

/// Why an attempt's diff could not be read, in the words a review prints
/// (§gh#183). Specific on purpose: "unavailable" tells a reviewer to go and
/// find out, and each of these tells them what they would find.
fn unreadable_diff(attempt: &Attempt) -> String {
    match (
        attempt.worktree.as_deref(),
        attempt.collected_at.is_some(),
        attempt.base_sha.as_deref(),
    ) {
        (None, _, _) => "this attempt never recorded a checkout".into(),
        (Some(w), true, _) => format!("the checkout was reclaimed ({w})"),
        (Some(w), false, _) if !std::path::Path::new(w).exists() => {
            format!("the checkout is no longer on disk ({w})")
        }
        (_, _, None) => "this attempt recorded no base commit, so there is nothing honest to \
             measure its diff against"
            .into(),
        _ => "the checkout could not be read".into(),
    }
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

    use crate::review::tests::TestEngine;

    fn engine() -> TestEngine {
        engine_with(None)
    }

    fn engine_with(github: Option<Github<Box<dyn Rest>>>) -> TestEngine {
        let mut e = engine_inner();
        e.github = github;
        e
    }

    fn engine_inner() -> TestEngine {
        let tmp = tempfile::Builder::new()
            .prefix("comet-board-test-")
            .tempdir()
            .unwrap();
        let dir = tmp.path().to_path_buf();
        let e = SyncEngine {
            db: Db::open_in_memory().unwrap(),
            cfg: RoutingConfig {
                defaults: Defaults::default(),
                ..Default::default()
            },
            credentials: Default::default(),
            paths: Paths {
                config_dir: dir.clone(),
                state_dir: dir,
            },
            log: Arc::new(Logger::new("", false)),
            github: None,
            push_capabilities: None,
            as_user: Rc::new(crate::sources::github::FixtureAsUser::default()),
            webhook: Arc::new(RecordingWebhook::default()),
        };
        TestEngine { e, _tmp: tmp }
    }

    /// gh#430: the box's /tmp is RAM, and 151,476 leftover test directories
    /// were 6 GB of it. The fixture now owns its directory; this pins that
    /// dropping the fixture is what removes it.
    #[test]
    fn a_dropped_fixture_removes_its_scratch_directory() {
        let e = engine();
        let dir = e.paths.state_dir.clone();
        assert!(dir.exists());
        drop(e);
        assert!(!dir.exists(), "{} survived its fixture", dir.display());
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
    fn engine_with_webhook(url: &str, fail: bool) -> (TestEngine, Arc<RecordingWebhook>) {
        let hook = Arc::new(RecordingWebhook {
            fail,
            ..Default::default()
        });
        let mut e = engine_inner();
        e.cfg.defaults.notify_webhook = Some(url.to_string());
        e.webhook = hook.clone();
        (e, hook)
    }

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
            upstream,
            updated_at: crate::db::now(),
        })
        .unwrap();
    }

    /// One `[[route]]`, so a test can edit it in place without moving the
    /// route count — the thing the old comparison keyed on.
    fn one_route(runtime: &str, base: &str) -> String {
        format!(
            "[[route]]\nworkspace = \"offhand\"\nrepo = \"~/dev/comet\"\n\
             runtime = \"{runtime}\"\nbase = \"{base}\"\n"
        )
    }

    /// Put the engine's config and its `routing.toml` in the same state, which
    /// is what a board that has just booted (or just reloaded) looks like.
    fn write_and_adopt(e: &mut SyncEngine, toml: &str) {
        std::fs::write(e.paths.routing(), toml).unwrap();
        e.cfg = RoutingConfig::load(&e.paths.routing()).unwrap();
    }

    /// gh#189: a `[defaults]`-only edit is a configuration change.
    ///
    /// It was invisible to a running board — the reload compared two
    /// credentials, the repo list and the route *count* — so a key written at
    /// 10:00 did nothing to a loop that had loaded its config at 08:03, and
    /// every surface that writes these keys reported the file as the truth.
    #[test]
    fn reload_notices_a_defaults_only_change() {
        let e = engine();
        std::fs::write(
            e.paths.routing(),
            "[defaults]\nnotify_dispatcher = false\nmax_concurrent_per_workspace = 7\n",
        )
        .unwrap();

        let fresh = e
            .reload_if_configuration_changed()
            .expect("a defaults-only edit is a configuration change");
        assert!(!fresh.cfg.defaults.notify_dispatcher);
        assert_eq!(fresh.cfg.defaults.max_concurrent_per_workspace, 7);
    }

    /// The orchestrator pin (gh#166) is a `[defaults]` key, and it is the one
    /// the box republishes as if the board had agreed. It has to reach the loop
    /// that delivers to it, not just the file.
    #[test]
    fn reload_notices_a_freshly_pinned_orchestrator() {
        let e = engine();
        assert_eq!(e.cfg.defaults.orchestrator(), None);
        std::fs::write(
            e.paths.routing(),
            "[defaults]\norchestrator_chat = \"chat-9\"\n",
        )
        .unwrap();

        let fresh = e
            .reload_if_configuration_changed()
            .expect("pinning an orchestrator is a configuration change");
        assert_eq!(fresh.cfg.defaults.orchestrator(), Some("chat-9"));
    }

    /// Editing a route rather than adding one: same count, different route.
    #[test]
    fn reload_notices_a_route_edited_in_place() {
        let mut e = engine();
        write_and_adopt(&mut e, &one_route("claude-code", "origin/HEAD"));
        std::fs::write(e.paths.routing(), one_route("codex", "origin/main")).unwrap();

        let fresh = e
            .reload_if_configuration_changed()
            .expect("editing a route in place is a configuration change");
        assert_eq!(fresh.cfg.routes.len(), 1);
        assert_eq!(fresh.cfg.routes[0].runtime, "codex");
        assert_eq!(fresh.cfg.routes[0].base.as_deref(), Some("origin/main"));
    }

    /// The other half of the exit: an unchanged board must not rebuild itself
    /// every cycle. Asked twice, because the loop asks every 30 seconds.
    #[test]
    fn reload_is_none_while_the_file_is_untouched() {
        let mut e = engine();
        // No file at all is the empty board, and it is not a change either.
        assert!(e.reload_if_configuration_changed().is_none());

        write_and_adopt(
            &mut e,
            &format!(
                "[defaults]\nnotify_dispatcher = false\n\n[github]\nrepos = [\"o/r\"]\n\n{}",
                one_route("claude-code", "origin/HEAD")
            ),
        );
        assert!(e.reload_if_configuration_changed().is_none());
        assert!(e.reload_if_configuration_changed().is_none());
    }

    /// Comments and formatting are not configuration: the comparison is the
    /// parsed config, so rewriting the file without changing what it says
    /// leaves the engine alone.
    #[test]
    fn reload_is_none_for_a_comment_only_edit() {
        let mut e = engine();
        write_and_adopt(&mut e, "[defaults]\nnotify_dispatcher = false\n");
        std::fs::write(
            e.paths.routing(),
            "# the dispatcher hears nothing on this board\n[defaults]\n\nnotify_dispatcher = false\n",
        )
        .unwrap();

        assert!(e.reload_if_configuration_changed().is_none());
    }

    /// The log line names the section that moved — vague enough to survive a
    /// new section, specific enough to be worth reading.
    #[test]
    fn the_reload_log_names_which_section_changed() {
        let before = RoutingConfig::default();
        let mut after = before.clone();
        after.defaults.notify_dispatcher = false;
        assert_eq!(changed_sections(&before, &after), "defaults");

        after.github.repos.push("o/r".into());
        assert_eq!(changed_sections(&before, &after), "defaults+github");

        assert_eq!(changed_sections(&before, &before), "none");
    }

    /// This cycle's session statuses, as the engine's board service builds
    /// them: chat id → status already mapped through `runtime::agent_status`.
    fn statuses(pairs: &[(&str, AgentStatus)]) -> SessionStatuses {
        pairs.iter().map(|(id, s)| (id.to_string(), *s)).collect()
    }

    fn dispatch(e: &SyncEngine, task: &str, chat_id: &str) -> i64 {
        let a =
            e.db.insert_attempt(&crate::db::NewAttempt {
                automation: None,
                automation_owner: None,
                stacked_on: None,
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
                dispatched_by_verified: false,
                billed_to: None,
            })
            .unwrap();
        e.db.set_attempt_pane(a, chat_id).unwrap();
        a
    }

    /// An attempt on a named branch, for the tests that care which one.
    fn dispatch_on(e: &SyncEngine, task: &str, branch: &str) -> i64 {
        e.db.insert_attempt(&crate::db::NewAttempt {
            automation: None,
            automation_owner: None,
            stacked_on: None,
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
            dispatched_by_verified: false,
            billed_to: None,
        })
        .unwrap()
    }

    fn live(e: &SyncEngine) -> Attempt {
        e.db.attempts_for("linear:LIN-142").unwrap().remove(0)
    }

    /// The one line a dispatch leaves upstream, from the payload the queue
    /// holds — including the model, which for a board spreading work across
    /// harnesses is what makes the next comparison possible (gh#232).
    #[test]
    fn a_dispatch_comment_names_the_runtime_and_the_model_that_ran() {
        let e = engine();
        seed(&e, "linear:LIN-232", "LIN-232", UpstreamState::Started);
        let task = e.db.get_task("linear:LIN-232").unwrap().unwrap();
        e.enqueue_dispatch(
            &task,
            RanOn {
                runtime: "codex",
                model: Some("gpt-5.6-luna"),
            },
            "comet-board",
            1,
            Some("brede@tally.no"),
            None,
        )
        .unwrap();
        let queued = e.db.pending_writebacks(10).unwrap();
        let payload: Value = serde_json::from_str(&queued[0].payload).unwrap();
        assert_eq!(
            dispatch_comment(&payload),
            "Dispatched to comet · codex · gpt-5.6-luna · space:comet-board · attempt 1 · \
             dispatched by brede@tally.no"
        );

        // No model named is the harness default, which the board cannot spell —
        // so it says the runtime alone rather than guessing.
        e.enqueue_dispatch(
            &task,
            RanOn {
                runtime: "claude-code",
                model: None,
            },
            "comet-board",
            2,
            None,
            None,
        )
        .unwrap();
        let queued = e.db.pending_writebacks(10).unwrap();
        let payload: Value = serde_json::from_str(&queued[1].payload).unwrap();
        assert_eq!(
            dispatch_comment(&payload),
            "Dispatched to comet · claude-code · space:comet-board · attempt 2"
        );
    }

    // ---- session reconciliation -----------------------------------------

    #[test]
    fn a_missing_chat_needs_two_ticks_before_it_orphans() {
        // Avoid flapping on a transient snapshot: one absent tick proves
        // nothing, exactly as one missing pane proved nothing in herdr-board.
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        // The chat itself is gone, which since §gh#390 is what orphans — an
        // absent session row on a chat that is still there is a dead run.
        let rt = JournalFact::ending_without(None, &["chat-9"]);
        // The agent got going — absence after activity is what orphans.
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        e.reconcile_sessions_with(&statuses(&[]), Some(&rt))
            .unwrap();
        assert!(
            live(&e).outcome.is_none(),
            "one missing tick must not orphan the attempt"
        );

        e.reconcile_sessions_with(&statuses(&[]), Some(&rt))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Orphaned));
    }

    #[test]
    fn a_chat_that_comes_back_resets_the_missing_counter() {
        let e = engine();
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
        // verdict needs `Runtime::chat_alive` (§runtime-impl); until then
        // absence of evidence must not end an attempt that may not have begun.
        let e = engine();
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
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending_without(None, &["chat-9"]);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        for _ in 0..5 {
            e.reconcile_sessions_with(&statuses(&[]), Some(&rt))
                .unwrap();
        }
        assert_eq!(e.db.pending_writeback_count().unwrap(), 1);
    }

    #[test]
    fn an_idle_status_leaves_the_attempt_live() {
        // "only finalize on explicit done detection or user action". The run
        // ended, and the checkout was checked (§settle-logic) — but this
        // attempt has no worktree, no commits and no PR, and between turns is
        // not finished.
        let e = engine();
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
        let e = engine();
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

    // ---- tokens on the attempt row (gh#151) ------------------------------

    /// A runtime that only meters. Everything else is unreachable: the token
    /// copy must not depend on any other verb, or a reconcile that cannot
    /// reach the chat would stop recording what it already knows.
    struct Meter(
        std::sync::Mutex<Option<crate::runtime::RunTokens>>,
        /// …and how full the window was, the other meter (gh#271).
        std::sync::Mutex<Option<comet_proto::ContextUsage>>,
        /// Chats that have been deleted or archived — what an *orphan* is made
        /// of since §gh#390, as opposed to a chat that simply lost its run.
        std::sync::Mutex<Vec<String>>,
    );

    impl Meter {
        fn saying(usage: comet_proto::TokenUsage, model: Option<&str>) -> Meter {
            Meter(
                std::sync::Mutex::new(Some(crate::runtime::RunTokens {
                    usage,
                    model: model.map(str::to_string),
                    by_model: None,
                    by_agent: None,
                })),
                std::sync::Mutex::new(None),
                std::sync::Mutex::new(Vec::new()),
            )
        }
        fn silent() -> Meter {
            Meter(
                std::sync::Mutex::new(None),
                std::sync::Mutex::new(None),
                std::sync::Mutex::new(Vec::new()),
            )
        }
        /// The chat has been archived out from under its attempt.
        fn lose_chat(&self, chat: &str) {
            self.2.lock().unwrap().push(chat.to_string());
        }
        fn set(&self, usage: comet_proto::TokenUsage, model: Option<&str>) {
            *self.0.lock().unwrap() = Some(crate::runtime::RunTokens {
                usage,
                model: model.map(str::to_string),
                by_model: None,
                by_agent: None,
            });
        }
        fn set_context(&self, context: Option<comet_proto::ContextUsage>) {
            *self.1.lock().unwrap() = context;
        }
        fn set_attribution(
            &self,
            by_model: Vec<comet_proto::ModelTokenUsage>,
            by_agent: Vec<comet_proto::AgentTokenUsage>,
        ) {
            let mut run = self.0.lock().unwrap();
            let run = run.as_mut().expect("meter is saying");
            run.by_model = Some(by_model);
            run.by_agent = Some(by_agent);
        }
    }

    impl Runtime for Meter {
        fn dispatch(&self, _: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
            unreachable!()
        }
        fn prompt(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn cancel(&self, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn session(&self, _: &str) -> anyhow::Result<Option<comet_proto::Session>> {
            Ok(None)
        }
        fn chat_alive(&self, chat: &str) -> anyhow::Result<bool> {
            Ok(!self.2.lock().unwrap().iter().any(|c| c == chat))
        }
        fn chat_cwd(&self, _: &str) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn last_run_end(&self, _: &str) -> anyhow::Result<Option<RunEnd>> {
            Ok(None)
        }
        fn run_tokens(&self, _: &str) -> anyhow::Result<Option<crate::runtime::RunTokens>> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn run_context(&self, _: &str) -> anyhow::Result<Option<comet_proto::ContextUsage>> {
            Ok(*self.1.lock().unwrap())
        }
    }

    fn tokens(input: u64, output: u64) -> comet_proto::TokenUsage {
        comet_proto::TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: input * 4,
            cache_creation_tokens: 7,
        }
    }

    #[test]
    fn reconcile_copies_the_running_total_onto_the_attempt() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = Meter::saying(tokens(100, 10), Some("claude-opus-5"));
        let by_model = vec![comet_proto::ModelTokenUsage {
            model: "claude-haiku-4-5".into(),
            usage: tokens(100, 10),
        }];
        let by_agent = vec![comet_proto::AgentTokenUsage {
            agent: comet_proto::AgentKind::Subagent,
            name: Some("Explore".into()),
            model: "claude-haiku-4-5".into(),
            usage: tokens(100, 10),
        }];
        rt.set_attribution(by_model.clone(), by_agent.clone());

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        let a = live(&e);
        assert_eq!(a.tokens, Some(tokens(100, 10)));
        assert_eq!(a.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(a.token_models, Some(by_model));
        assert_eq!(a.token_agents, Some(by_agent));

        // The next turn's total replaces it — the journal's figure is already
        // a run total, so this is a copy and never an accumulation on top of
        // an accumulation.
        rt.set(tokens(250, 30), Some("claude-opus-5"));
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        assert_eq!(live(&e).tokens, Some(tokens(250, 30)));
    }

    /// The whole point of the `Option`: a chat that has reported nothing
    /// leaves the row blank. A zero would be indistinguishable from an agent
    /// that worked for an hour for free.
    #[test]
    fn a_chat_that_reports_nothing_leaves_the_row_blank_not_zero() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.reconcile_sessions_with(
            &statuses(&[("chat-9", AgentStatus::Working)]),
            Some(&Meter::silent()),
        )
        .unwrap();
        assert_eq!(live(&e).tokens, None);
        // And a reconcile with no runtime at all cannot invent one either.
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        assert_eq!(live(&e).tokens, None);
    }

    /// An attempt orphaned or capped is closed inside the same reconcile that
    /// would record its tokens — so the recording happens first, or the
    /// longest-running attempts are exactly the ones that never report.
    #[test]
    fn an_orphaned_attempt_keeps_what_it_had_spent() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = Meter::saying(tokens(500, 50), Some("claude-opus-5"));
        // Seen working once, then the chat vanishes for two ticks.
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        rt.lose_chat("chat-9");
        e.reconcile_sessions_with(&statuses(&[]), Some(&rt))
            .unwrap();
        e.reconcile_sessions_with(&statuses(&[]), Some(&rt))
            .unwrap();

        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        let a = task.attempts.last().unwrap();
        assert_eq!(a.outcome, Some(Outcome::Orphaned));
        assert_eq!(a.tokens, Some(tokens(500, 50)));
    }

    // ---- how full the window is (gh#271) ---------------------------------

    fn context(used: u64) -> comet_proto::ContextUsage {
        comet_proto::ContextUsage {
            used_tokens: used,
            max_tokens: 200_000,
            compact_at_tokens: Some(167_000),
        }
    }

    /// Fullness is a LEVEL: each reconcile replaces the last reading rather
    /// than adding to it — including when it falls, which is what a compaction
    /// looks like from here. Summing these would put an attempt at 170% of a
    /// window it never filled once.
    #[test]
    fn reconcile_replaces_the_context_level_instead_of_accumulating_it() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = Meter::saying(tokens(100, 10), Some("claude-opus-5"));
        rt.set_context(Some(context(120_000)));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        let a = live(&e);
        assert_eq!(a.context, Some(context(120_000)));
        assert_eq!(a.context.and_then(|c| c.percent()), Some(60));

        rt.set_context(Some(context(170_000)));
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        let a = live(&e);
        assert_eq!(a.context, Some(context(170_000)));
        assert!(a.context.is_some_and(|c| c.is_near_compaction(0.9)));

        // The agent compacted: the level falls, and the row follows it down.
        rt.set_context(Some(context(45_000)));
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        assert_eq!(live(&e).context, Some(context(45_000)));
    }

    /// A harness that meters no window leaves the row blank — never 0%, which
    /// would read as an agent running on an empty context.
    #[test]
    fn a_harness_that_meters_no_window_leaves_the_context_blank() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = Meter::saying(tokens(100, 10), Some("gpt-5.6-terra"));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        let a = live(&e);
        assert_eq!(a.tokens, Some(tokens(100, 10)), "spend is still recorded");
        assert_eq!(a.context, None, "and fullness honestly is not");
    }

    /// The last level survives the close, for `record_tokens`'s reason: an
    /// attempt can end by being orphaned, and the reading it had is a better
    /// answer than none — it is also the one that says *why* it stalled.
    #[test]
    fn an_orphaned_attempt_keeps_the_last_level_it_was_seen_at() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = Meter::saying(tokens(500, 50), Some("claude-opus-5"));
        rt.set_context(Some(context(190_000)));
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        rt.lose_chat("chat-9");
        e.reconcile_sessions_with(&statuses(&[]), Some(&rt))
            .unwrap();
        e.reconcile_sessions_with(&statuses(&[]), Some(&rt))
            .unwrap();

        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        let a = task.attempts.last().unwrap();
        assert_eq!(a.outcome, Some(Outcome::Orphaned));
        assert_eq!(a.context, Some(context(190_000)));
    }

    // ---- the status-only fast path --------------------------------------

    #[test]
    fn a_watch_event_picks_up_a_status_change_without_touching_lifecycle() {
        let e = engine();
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
        let e = engine();
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
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.refresh_statuses(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        assert!(live(&e).saw_working);
    }

    #[test]
    fn rederive_persists_the_matrix_result() {
        let e = engine();
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

    // ---- settling on run events (§settle-logic) ---------------------------

    use crate::runtime::{DispatchHandle, DispatchSpec};

    /// A runtime that answers the journal question and records the prompts the
    /// settle path sends — the two things reachable from settling (gh#71).
    #[derive(Default)]
    struct JournalFact {
        end: Option<RunEnd>,
        queued: std::sync::Mutex<Vec<(String, String)>>,
        /// Chats that have been archived since — the ordinary shape of a
        /// dispatcher that did not survive its child (gh#165).
        gone: Vec<String>,
        /// A runtime that cannot answer whether a chat is there at all
        /// (§gh#390) — the third answer, which must end nothing.
        blind: bool,
        /// What the engine's own boot recovery already spent on every chat
        /// here (§gh#392). `None` is the ledger the board could not read.
        revivals: Option<i64>,
    }

    impl JournalFact {
        fn ending(end: Option<RunEnd>) -> JournalFact {
            JournalFact {
                end,
                queued: Default::default(),
                gone: Vec::new(),
                blind: false,
                revivals: Some(0),
            }
        }
        /// The same, on a box whose engine has already revived these runs
        /// itself — invisibly, before the board ever looked (§gh#392).
        fn revived(end: Option<RunEnd>, times: i64) -> JournalFact {
            JournalFact {
                revivals: Some(times),
                ..JournalFact::ending(end)
            }
        }
        /// The same, with no readable engine ledger at all.
        fn uncounted(end: Option<RunEnd>) -> JournalFact {
            JournalFact {
                revivals: None,
                ..JournalFact::ending(end)
            }
        }
        /// The same, with some chats already archived.
        fn ending_without(end: Option<RunEnd>, gone: &[&str]) -> JournalFact {
            JournalFact {
                gone: gone.iter().map(|c| c.to_string()).collect(),
                ..JournalFact::ending(end)
            }
        }
        /// The same, and unable to say whether any chat is still there.
        fn blind(end: Option<RunEnd>) -> JournalFact {
            JournalFact {
                blind: true,
                ..JournalFact::ending(end)
            }
        }
        /// (chat id, text) for every prompt the board queued.
        fn prompts(&self) -> Vec<(String, String)> {
            self.queued.lock().unwrap().clone()
        }
    }

    impl Runtime for JournalFact {
        fn dispatch(&self, _: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
            unreachable!("settling never dispatches")
        }
        fn prompt(&self, chat: &str, text: &str) -> anyhow::Result<()> {
            self.queued
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
        fn chat_alive(&self, chat: &str) -> anyhow::Result<bool> {
            if self.blind {
                anyhow::bail!("the workspace could not be read");
            }
            Ok(!self.gone.iter().any(|c| c == chat))
        }
        fn chat_revivals(&self, _: &str) -> anyhow::Result<Option<i64>> {
            Ok(self.revivals)
        }
        fn chat_cwd(&self, _: &str) -> anyhow::Result<Option<String>> {
            unreachable!("settling never reads the chat cwd")
        }
        fn last_run_end(&self, _: &str) -> anyhow::Result<Option<RunEnd>> {
            Ok(self.end)
        }
    }

    /// Point an engine's log at a file the test can read back.
    ///
    /// The only way to assert on a silence: the board's answer to "nobody could
    /// be told" is a line in the log, and a line nobody checks is exactly the
    /// silence gh#165 is about.
    fn logging(e: &mut SyncEngine) -> std::path::PathBuf {
        let path = e.paths.state_dir.join("board.log");
        e.log = Arc::new(Logger::new(path.clone(), false));
        path
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
    ///
    /// The `TempDir` owns the checkout — hold it as long as the settle needs
    /// the tree; dropping it is the cleanup.
    fn agent_worked_in(
        e: &SyncEngine,
        attempt: i64,
        did: Work,
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let (repo, work) = repo_ahead_of_its_remote();
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
        (repo, work)
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
        // The whole §settle-logic headline: no clock, no second sample. The
        // run ended, a PR is open — the agent said it is finished, so it is.
        let e = engine();
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
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = agent_worked_in(&e, a, Work::Pushed);

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        // Commits are the evidence, and no PR is claimed — the payload says
        // null, which delivery renders as the log-pointer comment.
        assert!(outcome_payload(&e)["pr_url"].is_null());
    }

    /// gh#69, end to end: the agent committed, `gh pr create` never happened
    /// (no credential on the box — gh#68's whole premise), and the run ended
    /// `Completed`. The old rule read that as a finished attempt and moved the
    /// row to `review` while the work sat in a worktree nobody else can reach.
    #[test]
    fn a_run_that_ends_with_unpushed_commits_does_not_read_as_review() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let (_repo, work) = agent_worked_in(&e, a, Work::Committed);

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
    }

    /// The crash variant. Recovery stamps an aborted run `Interrupted`, so the
    /// errored-runs-never-settle-on-commits guard does not apply to it, and an
    /// agent killed mid-task used to settle as finished on a commit it had
    /// made along the way.
    #[test]
    fn a_recovery_aborted_run_with_unpushed_commits_stays_live() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = agent_worked_in(&e, a, Work::Committed);
        let rt = JournalFact::ending(Some(RunEnd::Interrupted));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();
        assert!(live(&e).outcome.is_none());
    }

    /// A push straight to a URL — what an agent carrying the board's
    /// credential does — updates no remote-tracking ref, so the local tier
    /// proves nothing and the row would sit `working` on work that is in fact
    /// on origin. The event path asks GitHub, once, and settles.
    #[test]
    fn a_branch_pushed_without_a_tracking_ref_is_found_by_asking_github() {
        let (_repo, work_dir) = repo_ahead_of_its_remote();
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
        let e = engine_with(Some(gh));
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
    }

    /// The bar is containment, not a name: a retry reuses its predecessor's
    /// branch, and the push that branch already carries is not this attempt's.
    #[test]
    fn a_retrys_own_commits_are_what_must_be_on_origin() {
        let (_repo, work_dir) = repo_ahead_of_its_remote();
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
        let e = engine_with(Some(gh));
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
    }

    #[test]
    fn a_run_that_ends_with_nothing_new_committed_stays_live() {
        // The checkout exists and holds the operator's own unpushed commit —
        // the AGE-19 trap. Measured from the attempt's base there is nothing,
        // and nothing is what must be found.
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = agent_worked_in(&e, a, Work::None);

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        assert!(live(&e).outcome.is_none());
    }

    #[test]
    fn an_errored_run_keeps_the_attempt_for_the_retry() {
        // §settle-logic's `reopened` contract, first half: an
        // `Errored`→retried run is the same attempt, not a new one. The
        // errored end never closes the row — even over commits — so the retry
        // lands on it, and the clean end that follows settles it with nothing
        // reopened and nothing double-counted.
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = agent_worked_in(&e, a, Work::Pushed);
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
    }

    #[test]
    fn an_errored_run_with_a_pull_request_still_finishes() {
        // A harness that crashes moments after `gh pr create` still finished
        // the work: the PR is the agent's own statement, whatever the exit
        // said.
        let e = engine();
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
        let e = engine();
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
        let e = engine();
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
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = agent_worked_in(&e, a, Work::Pushed);
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Unknown)]))
            .unwrap();
        assert!(live(&e).outcome.is_none());
    }

    // ---- the answer that arrived after its merge closed the branch (gh#563)

    /// A gh task's attempt with a chat and a dispatcher to hear the settle,
    /// standing on a named branch. `settle` tests above reuse the Linear
    /// fixtures; these need a task id that names a repo.
    fn gh_attempt(e: &SyncEngine, task: &str, chat: &str, parent: &str, branch: &str) -> i64 {
        let a =
            e.db.insert_attempt(&crate::db::NewAttempt {
                automation: None,
                automation_owner: None,
                stacked_on: None,
                task_id: task.into(),
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: None,
                branch: Some(branch.into()),
                dispatched_by: None,
                dispatched_by_pane: Some(parent.into()),
                base_sha: None,
                account: None,
                repo_path: None,
                dispatched_by_device: None,
                dispatched_by_user: None,
                dispatched_by_verified: false,
                billed_to: None,
            })
            .unwrap();
        e.db.set_attempt_pane(a, chat).unwrap();
        a
    }

    /// The reported shape (gh#563): review delivery woke a settled agent, the
    /// dispatcher merged while it worked, and its answer landed as commits on
    /// a branch whose pull request had closed. The settle used to print "no
    /// pull request was opened" and strand the work; now it opens the door
    /// again — recorded, derived back into review, upstream reopened, and said
    /// out loud to whoever was waiting.
    #[test]
    fn an_answer_pushed_after_its_pr_merged_reopens_as_a_new_pull_request() {
        let (_root, work) = repo_ahead_of_its_remote();
        let wt = work.to_string_lossy().into_owned();
        let base = git_out(&wt, &["rev-parse", "HEAD"]).unwrap();
        git_in(&work, &["checkout", "-b", "board/gh-87"]);
        std::fs::write(work.join("answer"), "the review's ask").unwrap();
        git_in(&work, &["add", "."]);
        git_in(&work, &["commit", "-m", "answer the review"]);
        git_in(&work, &["push", "origin", "board/gh-87"]);

        // The PR recheck finds nothing open (it merged); the compare says two
        // commits of answer are stranded; the create succeeds.
        let rest = std::rc::Rc::new(FixtureRest::new(vec![
            ("/repos/o/r/pulls?".to_string(), json!([])),
            (
                "/repos/o/r/compare/main...board/gh-87".to_string(),
                json!({ "ahead_by": 2 }),
            ),
            (
                "POST /repos/o/r/pulls".to_string(),
                json!({ "number": 99, "html_url": "https://github.com/o/r/pull/99" }),
            ),
        ]));
        let e = engine_with(Some(Github::new(Box::new(SharedRest(rest)))));
        seed_gh_in(&e, "gh:o/r#87");
        let a = gh_attempt(&e, "gh:o/r#87", "chat-9", "chat-parent", "board/gh-87");
        e.db.set_attempt_worktree(a, &wt).unwrap();
        e.db.set_attempt_base_sha(a, &base).unwrap();
        // The merge happened while the agent worked: PR closed, merged, done.
        e.db.set_pr(
            "gh:o/r#87",
            Some("https://github.com/o/r/pull/50"),
            Some(50),
            false,
        )
        .unwrap();
        e.db.set_pr_topology("gh:o/r#87", Some("main"), Some("board/gh-87"), None)
            .unwrap();
        e.db.set_pr_merged("gh:o/r#87", true).unwrap();
        e.db.set_local_done("gh:o/r#87", true).unwrap();

        let rt = JournalFact::ending(Some(RunEnd::Completed));
        e.refresh_statuses_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        e.refresh_statuses_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        let attempt = e.db.attempts_for("gh:o/r#87").unwrap().remove(0);
        assert_eq!(attempt.outcome, Some(Outcome::Done));
        let t = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        assert_eq!(t.pr_number, Some(99), "the new door is recorded");
        assert_eq!(t.pr_url.as_deref(), Some("https://github.com/o/r/pull/99"));
        assert!(t.pr_open);
        assert!(
            !t.local_done,
            "the merge's `done` does not stand over fresh reviewable work"
        );
        e.rederive_all().unwrap();
        let t = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        assert_eq!(
            t.state,
            BoardState::Review,
            "the answer is reviewable, exactly like a first settle"
        );
        assert!(
            e.db.pending_writebacks(20)
                .unwrap()
                .iter()
                .any(|w| w.kind == "reopen"),
            "the issue follows the work back open"
        );
        assert!(
            rt.prompts().iter().any(|(_, m)| {
                m.contains("pushed 2 commit(s) after o/r#50 had merged")
                    && m.contains("reopened as o/r#99")
                    && m.contains("pull request: https://github.com/o/r/pull/99")
            }),
            "the dispatcher hears what happened and where the work went: {:?}",
            rt.prompts()
        );
    }

    /// A push that raced the merge and lost is not stranded work: GitHub says
    /// the base already holds everything the branch carries, so no pull
    /// request is opened and the merge's `done` stands.
    #[test]
    fn a_branch_the_merge_already_took_is_not_reopened() {
        let (_root, work) = repo_ahead_of_its_remote();
        let wt = work.to_string_lossy().into_owned();
        let base = git_out(&wt, &["rev-parse", "HEAD"]).unwrap();
        git_in(&work, &["checkout", "-b", "board/gh-87"]);
        std::fs::write(work.join("answer"), "raced the merge").unwrap();
        git_in(&work, &["add", "."]);
        git_in(&work, &["commit", "-m", "answer the review"]);
        git_in(&work, &["push", "origin", "board/gh-87"]);

        let rest = std::rc::Rc::new(FixtureRest::new(vec![
            ("/repos/o/r/pulls?".to_string(), json!([])),
            (
                "/repos/o/r/compare/main...board/gh-87".to_string(),
                json!({ "ahead_by": 0 }),
            ),
        ]));
        let e = engine_with(Some(Github::new(Box::new(SharedRest(rest.clone())))));
        seed_gh_in(&e, "gh:o/r#87");
        let a = gh_attempt(&e, "gh:o/r#87", "chat-9", "chat-parent", "board/gh-87");
        e.db.set_attempt_worktree(a, &wt).unwrap();
        e.db.set_attempt_base_sha(a, &base).unwrap();
        e.db.set_pr(
            "gh:o/r#87",
            Some("https://github.com/o/r/pull/50"),
            Some(50),
            false,
        )
        .unwrap();
        e.db.set_pr_merged("gh:o/r#87", true).unwrap();
        e.db.set_local_done("gh:o/r#87", true).unwrap();

        let rt = JournalFact::ending(Some(RunEnd::Completed));
        e.refresh_statuses_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert_eq!(
            e.db.attempts_for("gh:o/r#87").unwrap()[0].outcome,
            Some(Outcome::Done)
        );
        let t = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        assert_eq!(t.pr_number, Some(50), "nothing new was opened");
        assert!(!t.pr_open);
        assert!(t.local_done);
        assert!(
            rest.wrote.borrow().is_empty(),
            "GitHub was asked, never written"
        );
        assert!(
            rt.prompts()
                .iter()
                .any(|(_, m)| m.contains("no pull request was opened")),
        );
    }

    /// GitHub refusing the create must not fail the settle: today's behaviour
    /// — settle on the commits, say there was no PR — is the fallback, with
    /// the refusal in the log.
    #[test]
    fn a_refused_create_falls_back_to_the_plain_commits_settle() {
        let (_root, work) = repo_ahead_of_its_remote();
        let wt = work.to_string_lossy().into_owned();
        let base = git_out(&wt, &["rev-parse", "HEAD"]).unwrap();
        git_in(&work, &["checkout", "-b", "board/gh-87"]);
        std::fs::write(work.join("answer"), "work").unwrap();
        git_in(&work, &["add", "."]);
        git_in(&work, &["commit", "-m", "answer the review"]);
        git_in(&work, &["push", "origin", "board/gh-87"]);

        // No `POST /repos/o/r/pulls` route: the fixture answers Null, which
        // reads as a create without a number in it.
        let rest = std::rc::Rc::new(FixtureRest::new(vec![
            ("/repos/o/r/pulls?".to_string(), json!([])),
            (
                "/repos/o/r/compare/main...board/gh-87".to_string(),
                json!({ "ahead_by": 3 }),
            ),
        ]));
        let e = engine_with(Some(Github::new(Box::new(SharedRest(rest)))));
        seed_gh_in(&e, "gh:o/r#87");
        let a = gh_attempt(&e, "gh:o/r#87", "chat-9", "chat-parent", "board/gh-87");
        e.db.set_attempt_worktree(a, &wt).unwrap();
        e.db.set_attempt_base_sha(a, &base).unwrap();
        e.db.set_pr(
            "gh:o/r#87",
            Some("https://github.com/o/r/pull/50"),
            Some(50),
            false,
        )
        .unwrap();
        e.db.set_pr_merged("gh:o/r#87", true).unwrap();
        e.db.set_local_done("gh:o/r#87", true).unwrap();

        let rt = JournalFact::ending(Some(RunEnd::Completed));
        e.refresh_statuses_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert_eq!(
            e.db.attempts_for("gh:o/r#87").unwrap()[0].outcome,
            Some(Outcome::Done)
        );
        let t = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        assert_eq!(
            t.pr_number,
            Some(50),
            "the merged PR stays the recorded one"
        );
        assert!(t.local_done, "nothing reopened, so nothing un-done");
        assert!(
            !e.db
                .pending_writebacks(20)
                .unwrap()
                .iter()
                .any(|w| w.kind == "reopen")
        );
    }

    // ---- notification (gh#71) --------------------------------------------

    /// An attempt released by an agent in `chat-parent`, as `--via` records it.
    fn dispatch_via(e: &SyncEngine, task: &str, chat_id: &str, parent: &str) -> i64 {
        let a =
            e.db.insert_attempt(&crate::db::NewAttempt {
                automation: None,
                automation_owner: None,
                stacked_on: None,
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
                dispatched_by_device: None,
                dispatched_by_user: None,
                dispatched_by_verified: false,
                billed_to: None,
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
        let e = engine();
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
        let e = engine();
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
        let e = engine();
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
        let e = engine();
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
        let e = engine();
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
        let e = engine();
        assert!(e.cfg.defaults.notify_dispatcher, "on by default (gh#165)");
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

    /// A block is the state where nothing happens until somebody acts, and the
    /// agent whose plan that task was a step in is the one who can act soonest.
    /// Before gh#165 the dispatcher wake fired on settles only.
    #[test]
    fn a_block_wakes_the_chat_that_released_the_work_too() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-parent");
        let rt = JournalFact::ending(None);

        e.refresh_statuses_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();

        let prompts = rt.prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(prompts[0].0, "chat-parent");
        assert!(prompts[0].1.contains("work you released is blocked"));
        assert!(prompts[0].1.contains("chat: chat-9"), "where to answer it");
        assert!(live(&e).outcome.is_none(), "and it still settles nothing");
    }

    /// Turning it off is a routing choice, not a mute: with a pin behind it the
    /// event goes to the orchestrator instead, and with nothing pinned it goes
    /// nowhere — which the log has to say (gh#165).
    #[test]
    fn the_dispatcher_wake_can_still_be_turned_off() {
        let mut e = engine();
        e.cfg.defaults.notify_dispatcher = false;
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

    // ---- the pinned orchestrator (gh#104) --------------------------------

    /// The first of the three cases the pin exists for, and most of a solo
    /// operator's dispatches: released from the panel, the phone, or a bare
    /// `comet-board dispatch`, so there is no dispatching chat at all — and the
    /// agent running the board still has to hear, because reviewing what lands
    /// is its job.
    #[test]
    fn the_orchestrator_hears_about_work_nobody_else_released() {
        let mut e = engine();
        e.cfg.defaults.orchestrator_chat = Some("chat-boss".into());
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
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
        assert_eq!(prompts[0].0, "chat-boss");
        assert!(prompts[0].1.contains("LIN-142"));
        assert!(prompts[0].1.contains("https://github.com/o/r/pull/18"));
        assert!(
            !prompts[0].1.contains("work you released"),
            "it released nothing: {}",
            prompts[0].1
        );
    }

    /// A block settles nothing and closes nothing, so without a notice the
    /// only trace is a row colour. The orchestrator is the party that can
    /// actually go and answer the question.
    #[test]
    fn a_block_reaches_the_orchestrator_too() {
        let mut e = engine();
        e.cfg.defaults.orchestrator_chat = Some("chat-boss".into());
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(None);

        e.refresh_statuses_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();

        let prompts = rt.prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(prompts[0].0, "chat-boss");
        assert!(prompts[0].1.contains("blocked"));
        assert!(prompts[0].1.contains("chat: chat-9"), "where to answer it");
        assert!(live(&e).outcome.is_none(), "and it still settles nothing");
    }

    /// When the orchestrator is also the chat that released the work, both
    /// channels have something to say about the same settle. It is told once —
    /// in the dispatcher's words, which are the more specific truth.
    #[test]
    fn the_orchestrator_is_not_told_twice_about_its_own_dispatch() {
        let mut e = engine();
        e.cfg.defaults.orchestrator_chat = Some("chat-boss".into());
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-boss");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        let prompts = rt.prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert!(prompts[0].1.contains("work you released"));
    }

    /// The rule that makes a pin survivable on a busy board (gh#165): a settle
    /// its dispatcher was told about is not copied to the orchestrator. Before
    /// this, an orchestrator pinned beside dispatching siblings received every
    /// child's settle twice over — once as a sibling's business and once as its
    /// own — and filled with work nobody needed it for.
    #[test]
    fn a_settle_a_live_dispatcher_handled_does_not_reach_the_orchestrator() {
        let mut e = engine();
        e.cfg.defaults.orchestrator_chat = Some("chat-boss".into());
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-sibling");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        let prompts = rt.prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(prompts[0].0, "chat-sibling", "the precise addressee");
    }

    /// The second case the pin exists for, and the one that used to be a
    /// dropped notice: attempts cap at two hours and chats archive as their
    /// task settles (§gh#139), so a dispatcher that did not survive its own
    /// child is ordinary. The event still matters — it just needs another
    /// addressee.
    #[test]
    fn a_settle_whose_dispatcher_is_gone_hops_to_the_orchestrator() {
        let mut e = engine();
        e.cfg.defaults.orchestrator_chat = Some("chat-boss".into());
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-parent");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending_without(None, &["chat-parent"]);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        let prompts = rt.prompts();
        assert_eq!(prompts.len(), 1, "{prompts:?}");
        assert_eq!(prompts[0].0, "chat-boss");
        assert!(prompts[0].1.contains("LIN-142"));
        // And it names the chat that never heard, which is the fact the
        // orchestrator acts on: this is a step in somebody's abandoned plan.
        assert!(
            prompts[0].1.contains("released by: chat chat-parent"),
            "{}",
            prompts[0].1
        );
    }

    /// The exit condition's last clause: an event that reaches neither agent is
    /// not dropped without a line saying so. Both halves of the address are
    /// named, because "no orchestrator is pinned" and "the orchestrator could
    /// not be told" are different things to go and fix.
    #[test]
    fn an_event_that_reaches_nobody_says_so_in_the_log() {
        let mut e = engine();
        let log = logging(&mut e);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-parent");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending_without(None, &["chat-parent"]);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert!(rt.prompts().is_empty(), "nobody to tell");
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(log.contains("on_settled reached no agent"), "{log}");
        assert!(log.contains("the chat that released it is gone"), "{log}");
        assert!(log.contains("no orchestrator is pinned"), "{log}");
    }

    /// The same line for the commonest board of all: a solo operator's
    /// dispatch, with nothing pinned. Silence here is a setting, and it should
    /// read as one in the log rather than as nothing having happened.
    #[test]
    fn a_panel_dispatch_on_an_unpinned_board_says_so_too() {
        let mut e = engine();
        let log = logging(&mut e);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(None);

        e.refresh_statuses_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();

        let log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(log.contains("on_blocked reached no agent"), "{log}");
        assert!(log.contains("no chat released it"), "{log}");
    }

    // ---- every skipped notice leaves a trace (gh#194) --------------------

    /// The report this issue was filed on, re-run against gh#165's rework: a
    /// dispatcher recorded on the attempt, `notify_dispatcher` on, and the
    /// settle prompted into that chat. It works, and it says in the log that it
    /// worked — which is what "no notice line of any kind" was the absence of.
    #[test]
    fn a_settle_with_a_dispatcher_recorded_reaches_it_and_says_so() {
        let mut e = engine();
        let log = logging(&mut e);
        assert!(e.cfg.defaults.notify_dispatcher, "as the box had it");
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-parent");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert_eq!(rt.prompts()[0].0, "chat-parent");
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            log.contains("on_settled queued into the chat that released it (chat-parent)"),
            "one greppable line per delivered notice: {log}"
        );
    }

    /// The switch turned off used to be the first of three exits that returned
    /// without a word, so a board configured to say nothing looked exactly like
    /// a board whose settle path never ran. It is the knob to go and turn, so
    /// the line names it.
    #[test]
    fn a_dispatcher_the_switch_silenced_is_named_in_the_log() {
        let mut e = engine();
        let log = logging(&mut e);
        e.cfg.defaults.notify_dispatcher = false;
        e.cfg.defaults.orchestrator_chat = Some("chat-boss".into());
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-parent");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        // The pin caught it, so `note_unheard` says nothing — which is exactly
        // the case the old code left with no trace at all.
        assert_eq!(rt.prompts()[0].0, "chat-boss");
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(log.contains("chat that released it (chat-parent)"), "{log}");
        assert!(
            log.contains("`[defaults] notify_dispatcher` is off"),
            "{log}"
        );
    }

    /// The second silent exit: an attempt nobody released. Not a fault — most
    /// of a solo operator's board is this — but a settle whose log says nothing
    /// about a dispatcher is a settle an operator cannot tell from one whose
    /// notice was dropped, so it says which.
    #[test]
    fn an_attempt_nobody_released_says_that_much() {
        let mut e = engine();
        let log = logging(&mut e);
        e.cfg.defaults.orchestrator_chat = Some("chat-boss".into());
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert_eq!(rt.prompts()[0].0, "chat-boss");
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(log.contains("has no chat that released it"), "{log}");
    }

    /// The third, and the one that reads as a bug rather than a setting: a
    /// hand-set `--via` naming the attempt's own chat. The board will not
    /// prompt an agent about itself, and the only way anybody learns that the
    /// `--via` was wrong is this line.
    #[test]
    fn a_via_that_names_the_attempts_own_chat_is_not_a_silent_drop() {
        let mut e = engine();
        let log = logging(&mut e);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-9");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert!(rt.prompts().is_empty(), "never prompted about itself");
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(log.contains("that is the attempt's own chat"), "{log}");
        assert!(
            log.contains("`--via` named the attempt's own chat"),
            "and again in the reached-nobody line: {log}"
        );
    }

    /// A chat that is gone is logged by the check both channels share, so the
    /// line has to name which channel was asking — otherwise a task whose
    /// dispatcher *and* pin have both been archived leaves two identical lines
    /// and no way to tell them apart.
    #[test]
    fn a_gone_chat_is_logged_against_the_channel_that_wanted_it() {
        let mut e = engine();
        let log = logging(&mut e);
        e.cfg.defaults.orchestrator_chat = Some("chat-boss".into());
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-parent");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending_without(None, &["chat-parent", "chat-boss"]);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        let log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            log.contains("the chat that released it (chat-parent) is gone"),
            "{log}"
        );
        assert!(
            log.contains("the pinned orchestrator (chat-boss) is gone"),
            "{log}"
        );
    }

    /// The kill switch, and the whole of it: no pin, no notices. The chat that
    /// was pinned goes back to being an ordinary chat with nothing arriving in
    /// it, which is what makes unpinning safe to reach for.
    #[test]
    fn unpinned_is_silent() {
        let e = engine();
        assert!(e.cfg.defaults.orchestrator().is_none(), "unset by default");
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert!(rt.prompts().is_empty());
        assert_eq!(live(&e).outcome, Some(Outcome::Done), "but it settles");
    }

    /// An empty string is what a settings field that has been cleared writes.
    /// Read as a chat id it would fail delivery on every event forever.
    #[test]
    fn a_cleared_pin_reads_as_no_pin() {
        let mut e = engine();
        e.cfg.defaults.orchestrator_chat = Some("   ".into());
        assert!(e.cfg.defaults.orchestrator().is_none());
    }

    /// Operator-released work has no dispatcher chat, so the switch being on
    /// changes nothing about it. That is why it is a separate switch from the
    /// operator's own notice — and why an unpinned board hears nothing at all
    /// about the dispatches a solo operator makes most of.
    #[test]
    fn operator_released_work_has_nobody_to_wake() {
        let e = engine();
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

    // ---- the credential that pushed (gh#233) -----------------------------

    fn credential_writebacks(e: &SyncEngine) -> Vec<Value> {
        e.db.pending_writebacks(50)
            .unwrap()
            .into_iter()
            .filter(|w| w.kind == "credential")
            .map(|w| serde_json::from_str(&w.payload).unwrap())
            .collect()
    }

    /// Settle a dispatched attempt on a pull request, having first written
    /// `ledger` into the board's credential record for its chat.
    fn settle_with_ledger(ledger: impl FnOnce(&Paths)) -> (TestEngine, Arc<RecordingWebhook>) {
        let (e, hook) = engine_with_webhook("https://hooks.example.com/x", false);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        ledger(&e.paths);
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done), "the attempt settled");
        (e, hook)
    }

    /// gh#233, as it actually happened: the board wired the run to its askpass
    /// helper, the helper was never asked, and a pull request exists anyway.
    /// Something pushed that branch and it was not the board's credential.
    #[test]
    fn work_on_origin_that_the_boards_credential_never_pushed_is_said_out_loud() {
        let (e, hook) = settle_with_ledger(|paths| {
            credential_ledger::handed(paths, "owner/widget", Some("chat-9"));
        });

        let notices = credential_writebacks(&e);
        assert_eq!(notices.len(), 1, "one comment upstream: {notices:?}");
        assert_eq!(notices[0]["branch"], json!("board/lin-142"));
        let comment = credential_comment(&notices[0]);
        assert!(comment.contains("never asked"), "{comment}");
        assert!(comment.contains("comet-board doctor"), "{comment}");

        // And the agent that released the work hears it on the settle itself,
        // rather than finding out from the issue next week.
        let posts = hook.posts.lock().unwrap().clone();
        assert!(
            posts[0].1["note"]
                .as_str()
                .is_some_and(|n| n.contains("did not push this")),
            "{:?}",
            posts[0].1
        );
    }

    /// The same settle on a box that could not hand the credential over at
    /// all. The engine refuses a path it cannot exec (that is the gh#233 fix),
    /// so nothing is `handed` — but the refusal is on the record, and work
    /// still reached origin.
    #[test]
    fn a_credential_path_that_was_refused_still_accuses_the_push_that_happened() {
        let (e, _) = settle_with_ledger(|paths| {
            credential_ledger::unusable(
                paths,
                "owner/widget",
                Some("chat-9"),
                "cannot exec the askpass helper",
            );
        });

        let notices = credential_writebacks(&e);
        assert_eq!(notices.len(), 1);
        let comment = credential_comment(&notices[0]);
        assert!(comment.contains("cannot exec"), "{comment}");
    }

    /// The case that must stay quiet, because it is every normal dispatch: the
    /// board handed its credential over and the helper answered a push with it.
    #[test]
    fn a_push_the_boards_credential_made_is_not_worth_a_word() {
        let (e, hook) = settle_with_ledger(|paths| {
            credential_ledger::handed(paths, "owner/widget", Some("chat-9"));
            credential_ledger::minted(paths, "git-askpass", "owner/widget", Some("chat-9"));
        });

        // gh#488: this is also the board-side half of the live-Codex
        // regression. The harness test executes the model-issued push; here
        // the same chat's Handed + Minted pair travels through a dispatched
        // attempt, PR discovery, session reconciliation, and final settle.
        // Do not replace this with a credential-ledger unit assertion: the
        // bug was observable only when an origin update and settle met.
        let record = credential_ledger::for_chat(&e.paths, "chat-9");
        assert!(record.handed && record.minted, "{record:?}");
        assert!(!record.unsanctioned(), "{record:?}");
        assert!(credential_writebacks(&e).is_empty());
        assert_eq!(hook.posts.lock().unwrap()[0].1["note"], Value::Null);
    }

    /// And the case that must stay quiet for a different reason: a box with no
    /// board credential at all never claimed to be the thing that pushes, so
    /// an empty ledger is not an accusation — it is a device pushing the way
    /// every device did before gh#68.
    #[test]
    fn a_box_that_never_issues_credentials_is_not_accused_of_losing_one() {
        let (e, _) = settle_with_ledger(|_| {});
        assert!(credential_writebacks(&e).is_empty());
    }

    // ---- the accusation binds to an origin that moved (gh#489) ------------

    /// Settle a dispatched attempt on a pull request, on a real checkout
    /// whose remote-tracking ref for the attempt's branch sits at the
    /// checkout's HEAD — with the stamp taken at attempt start resolved from
    /// `then_rev` in that same repo. `"HEAD"` is a branch origin has held,
    /// unmoved, since before the attempt; anything older is a branch that
    /// moved while the attempt ran. What the settle then says about the
    /// credential is the whole test.
    fn settle_with_origin_stamp(e: &TestEngine, then_rev: &str) -> tempfile::TempDir {
        seed(e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(e, "linear:LIN-142", "chat-9");
        credential_ledger::handed(&e.paths, "owner/widget", Some("chat-9"));
        let (repo, work) = repo_ahead_of_its_remote();
        let wt = work.to_string_lossy().into_owned();
        e.db.set_attempt_worktree(a, &wt).unwrap();
        let now = git_out(&wt, &["rev-parse", "HEAD"]).unwrap();
        let then = git_out(&wt, &["rev-parse", then_rev]).unwrap();
        git_in(
            &work,
            &["update-ref", "refs/remotes/origin/board/lin-142", &now],
        );
        e.db.meta_set(&meta::origin_at_start(a), &then).unwrap();
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();
        assert_eq!(live(e).outcome, Some(Outcome::Done), "the attempt settled");
        repo
    }

    /// gh#489, as it happened to gh#468's attempt 3: a repair run dispatched
    /// onto a branch a previous attempt had already pushed, whose journal
    /// holds no push at all. The branch being on origin is the *old* push,
    /// not this run's — origin has not moved since the attempt began, so
    /// there is no push to account for and nothing to accuse.
    #[test]
    fn a_repair_run_that_pushed_nothing_is_not_accused_over_the_old_push() {
        let (e, hook) = engine_with_webhook("https://hooks.example.com/x", false);
        let _repo = settle_with_origin_stamp(&e, "HEAD");

        assert!(
            credential_writebacks(&e).is_empty(),
            "no comment lands on the issue"
        );
        assert_eq!(
            hook.posts.lock().unwrap()[0].1["note"],
            Value::Null,
            "and the settle notice carries no accusation"
        );
    }

    /// The inverse must not soften (gh#489's second half): a branch that
    /// *moved* during the attempt still demands a same-chat mint, whoever
    /// moved it. A wrapper-made push and a push by another actor entirely
    /// read the same from here — origin changed on a run whose credential
    /// was never asked — and the board says, honestly, that it cannot
    /// account for the credential that did it.
    #[test]
    fn a_branch_that_moved_without_a_mint_is_still_said_out_loud() {
        let (e, hook) = engine_with_webhook("https://hooks.example.com/x", false);
        let _repo = settle_with_origin_stamp(&e, "HEAD~1");

        let notices = credential_writebacks(&e);
        assert_eq!(notices.len(), 1, "one comment upstream: {notices:?}");
        assert!(
            hook.posts.lock().unwrap()[0].1["note"]
                .as_str()
                .is_some_and(|n| n.contains("did not push this")),
        );
    }

    /// A force-push is a move like any other: the tracking ref pointing at a
    /// commit that is no descendant of the stamp is still `now != then`, and
    /// the check compares identity, not ancestry — a branch rewound and
    /// rewritten under the attempt does not slip past as "already there".
    #[test]
    fn a_force_pushed_branch_reads_as_moved() {
        let (e, _) = engine_with_webhook("https://hooks.example.com/x", false);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        credential_ledger::handed(&e.paths, "owner/widget", Some("chat-9"));
        let (_repo, work) = repo_ahead_of_its_remote();
        let wt = work.to_string_lossy().into_owned();
        let then = git_out(&wt, &["rev-parse", "HEAD"]).unwrap();
        // The rewrite: same tree, different commit — what a force-push leaves.
        git_in(&work, &["commit", "--amend", "-m", "rewritten"]);
        let rewritten = git_out(&wt, &["rev-parse", "HEAD"]).unwrap();
        assert_ne!(then, rewritten);
        e.db.set_attempt_worktree(a, &wt).unwrap();
        git_in(
            &work,
            &[
                "update-ref",
                "refs/remotes/origin/board/lin-142",
                &rewritten,
            ],
        );
        e.db.meta_set(&meta::origin_at_start(a), &then).unwrap();
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert_eq!(credential_writebacks(&e).len(), 1);
    }

    /// A moved branch with the board's own mint beside it is every normal
    /// push, stamp or no stamp: the mint answers the question before the
    /// origin comparison is ever asked.
    #[test]
    fn a_moved_branch_the_boards_credential_pushed_stays_quiet() {
        let (e, hook) = engine_with_webhook("https://hooks.example.com/x", false);
        credential_ledger::minted(&e.paths, "git-askpass", "owner/widget", Some("chat-9"));
        let _repo = settle_with_origin_stamp(&e, "HEAD~1");

        assert!(credential_writebacks(&e).is_empty());
        assert_eq!(hook.posts.lock().unwrap()[0].1["note"], Value::Null);
    }

    /// The stamp itself: at dispatch the engine records the tracking ref's
    /// answer for the attempt's branch, and records nothing for a branch
    /// origin does not hold — absence at settle then reads as "any work on
    /// origin is new", the old footing, rather than as a spent alibi.
    #[test]
    fn the_dispatch_stamp_records_origin_and_only_origin() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let (_repo, work) = repo_ahead_of_its_remote();
        let wt = work.to_string_lossy().into_owned();
        let tip = git_out(&wt, &["rev-parse", "HEAD"]).unwrap();

        // A branch origin never heard of leaves no stamp.
        e.stamp_origin_at_start(&task, a, &wt, "board/lin-142");
        assert_eq!(e.db.meta_get(&meta::origin_at_start(a)).unwrap(), None);

        // One origin holds is stamped exactly as origin holds it.
        git_in(
            &work,
            &["update-ref", "refs/remotes/origin/board/lin-142", &tip],
        );
        e.stamp_origin_at_start(&task, a, &wt, "board/lin-142");
        assert_eq!(
            e.db.meta_get(&meta::origin_at_start(a)).unwrap().as_deref(),
            Some(tip.as_str())
        );
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
        let (e, hook) = engine_with_webhook("https://hooks.example.com/x", false);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-parent");
        let rt = JournalFact::ending_without(None, &["chat-9"]);

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

    // ---- a restart must not eat the attempts (§gh#390) --------------------

    /// Two missing ticks on a chat that is still there.
    fn lose_the_run(e: &SyncEngine, rt: &dyn Runtime) {
        for _ in 0..runs::MISSING_TICKS {
            e.reconcile_sessions_with(&statuses(&[]), Some(rt)).unwrap();
        }
    }

    /// The bug, at its smallest. The engine restarts; the session mirror comes
    /// back empty; the chat is exactly where it was. That is a dead run, not a
    /// dead chat, and burying the attempt over it threw away a live agent's
    /// whole context.
    #[test]
    fn a_chat_that_outlived_its_run_is_restarted_not_orphaned() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(None);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        lose_the_run(&e, &rt);

        let a = live(&e);
        assert!(a.outcome.is_none(), "the attempt is not over");
        assert_eq!(a.resumes, 1, "its run was restarted once");
        assert_eq!(a.missing_ticks, 0, "and the absence is spent");
        let prompts = rt.prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].0, "chat-9", "restarted in its own chat");
        assert!(prompts[0].1.contains("no attempt has been spent"));
        assert_eq!(
            e.db.pending_writeback_count().unwrap(),
            0,
            "nothing is written upstream about a run that is carrying on"
        );
    }

    /// Nothing is re-created: same attempt row, same chat, same branch. A
    /// retry would have made a second attempt in a second chat and thrown the
    /// first one's context away, which is what the orphan sweep forced.
    #[test]
    fn a_restarted_run_keeps_its_attempt_its_chat_and_its_branch() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let id = dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(None);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        lose_the_run(&e, &rt);

        let attempts = e.db.attempts_for("linear:LIN-142").unwrap();
        assert_eq!(attempts.len(), 1, "no second attempt was released");
        assert_eq!(attempts[0].id, id);
        assert_eq!(attempts[0].pane_id.as_deref(), Some("chat-9"));
        assert_eq!(attempts[0].branch.as_deref(), Some("board/lin-142"));
    }

    /// The night this is about: six attempts, six chats, one restart. Six
    /// settle notices said six things about six tasks and nothing at all about
    /// the engine — so the incident is announced once, as itself.
    #[test]
    fn a_box_that_loses_every_run_at_once_says_so_once() {
        let (mut e, hook) = engine_with_webhook("https://hooks.example.com/x", false);
        e.cfg.defaults.orchestrator_chat = Some("chat-boss".into());
        let chats: Vec<String> = (1..=6).map(|n| format!("chat-{n}")).collect();
        for (n, chat) in chats.iter().enumerate() {
            let id = format!("gh:o/r#{}", n + 1);
            seed(&e, &id, &format!("o/r#{}", n + 1), UpstreamState::Started);
            dispatch(&e, &id, chat);
        }
        let rt = JournalFact::ending(None);
        let live_now: Vec<(&str, AgentStatus)> = chats
            .iter()
            .map(|c| (c.as_str(), AgentStatus::Working))
            .collect();
        e.reconcile_sessions_with(&statuses(&live_now), Some(&rt))
            .unwrap();

        lose_the_run(&e, &rt);

        let prompts = rt.prompts();
        let to_boss: Vec<&(String, String)> =
            prompts.iter().filter(|(c, _)| c == "chat-boss").collect();
        assert_eq!(to_boss.len(), 1, "one notice, not six");
        assert!(
            to_boss[0].1.contains("6 live attempts lost their runs"),
            "and it says what happened to the box: {}",
            to_boss[0].1
        );
        assert_eq!(
            prompts.len(),
            7,
            "six restarts and the one notice — no settles at all"
        );
        let posts = hook.posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].1["event"], "on_runs_interrupted");
        assert_eq!(posts[0].1["runs"].as_array().unwrap().len(), 6);
    }

    /// A box that cannot keep a run alive would otherwise be restarted forever.
    /// Past the cap the attempt closes `failed` — not `orphaned`, because
    /// nothing vanished, and a red row is what makes somebody look at the box.
    #[test]
    fn a_run_that_keeps_dying_closes_the_attempt_failed() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(None);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        for _ in 0..runs::MAX_RESUMES {
            lose_the_run(&e, &rt);
            assert!(live(&e).outcome.is_none(), "still worth restarting");
        }
        lose_the_run(&e, &rt);

        let a = live(&e);
        assert_eq!(a.outcome, Some(Outcome::Failed));
        assert_eq!(a.resumes, runs::MAX_RESUMES);
        assert_eq!(
            e.db.pending_writeback_count().unwrap(),
            1,
            "and the issue is told once, when it is actually over"
        );
    }

    // ---- one budget, wherever the restart came from (§gh#392) -------------

    /// The bug. The engine's boot recovery revives a crashed run three times on
    /// its own ledger and then stops — leaving a live chat, a closed journal
    /// and no run, which is exactly the state the board resumes from. It used
    /// to start counting at zero there, so the run was started six times and
    /// the note said three.
    #[test]
    fn the_engines_own_revivals_are_spent_from_the_same_budget() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::revived(None, runs::MAX_RESUMES);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        lose_the_run(&e, &rt);

        let a = live(&e);
        assert_eq!(
            a.outcome,
            Some(Outcome::Failed),
            "the budget was already spent by the engine"
        );
        assert_eq!(a.resumes, 0, "and the board added nothing to the pile");
        assert!(
            rt.prompts().iter().all(|(c, _)| c != "chat-9"),
            "a fourth start is the one this exists to prevent: {:?}",
            rt.prompts()
        );
    }

    /// And what it tells the person who has to act on it counts every start,
    /// including the three nothing on the board recorded.
    #[test]
    fn the_note_counts_the_restarts_the_board_never_made() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::revived(None, 2);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        lose_the_run(&e, &rt); // the board's one remaining restart
        assert!(live(&e).outcome.is_none(), "one left of the three");
        lose_the_run(&e, &rt);

        let a = live(&e);
        assert_eq!(a.outcome, Some(Outcome::Failed));
        assert_eq!(a.resumes, 1, "the board restarted it once");
        let note = outcome_payload(&e)["note"].as_str().unwrap().to_string();
        assert!(note.contains("died 3 times"), "{note}");
        assert!(
            note.contains("2 of those restarts were the engine's own"),
            "{note}"
        );
    }

    /// An engine ledger the board cannot read must cost the attempt nothing —
    /// the board restarts on its own count exactly as before — and must not be
    /// reported as a number. "Restarted until the board gave up" is honest;
    /// "3 times" would not be.
    #[test]
    fn an_unreadable_ledger_changes_nothing_and_claims_nothing() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::uncounted(None);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        for _ in 0..runs::MAX_RESUMES {
            lose_the_run(&e, &rt);
            assert!(live(&e).outcome.is_none(), "still worth restarting");
        }
        lose_the_run(&e, &rt);

        let a = live(&e);
        assert_eq!(a.outcome, Some(Outcome::Failed));
        assert_eq!(a.resumes, runs::MAX_RESUMES);
        let note = outcome_payload(&e)["note"].as_str().unwrap().to_string();
        assert!(!note.contains('3'), "no count it cannot support: {note}");
        assert!(note.contains("the box is not keeping runs alive"), "{note}");
    }

    /// The disjoint path, unchanged: an engine with budget left revives the run
    /// itself, the board sees a chat that is working, and nothing is spent on
    /// either side of the fence.
    #[test]
    fn an_engine_restart_with_budget_left_costs_the_board_nothing() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::revived(None, 1);

        for _ in 0..4 {
            e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
                .unwrap();
        }

        let a = live(&e);
        assert!(a.outcome.is_none(), "the run is going; nothing to decide");
        assert_eq!(a.resumes, 0, "no board resume was spent on it");
        assert!(rt.prompts().is_empty(), "and it was not prompted");
    }

    /// The prompt the agent reads says where in the budget it is, not where in
    /// the board's column — an attempt the engine already revived twice is on
    /// its last restart, and the sentence has to say so.
    #[test]
    fn the_restarted_agent_is_told_the_joined_position() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::revived(None, 2);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        lose_the_run(&e, &rt);

        let prompts = rt.prompts();
        assert_eq!(prompts[0].0, "chat-9");
        assert!(
            prompts[0]
                .1
                .contains(&format!("Restart 3 of {}", runs::MAX_RESUMES)),
            "{}",
            prompts[0].1
        );
    }

    /// The promise a block makes — "the chat still holds the whole task, so it
    /// is a retry or a cancel, not a lost attempt" — was broken by the orphan
    /// sweep taking the same row minutes later. The chat is there; it is still
    /// the retry path it was said to be.
    #[test]
    fn an_errored_block_is_not_swept_away_behind_the_operators_back() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::ending(Some(RunEnd::Errored));
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Blocked)]), Some(&rt))
            .unwrap();
        assert_eq!(live(&e).blocked_count, 1, "the block was announced");

        lose_the_run(&e, &rt);

        let a = live(&e);
        assert!(
            a.outcome.is_none(),
            "the chat is still there, so the attempt is not lost"
        );
        assert_eq!(a.resumes, 1);
    }

    /// A runtime that cannot answer ends nothing — the rule a chat which had
    /// never worked already lived by, now applied to one that had.
    #[test]
    fn a_chat_nobody_can_ask_about_is_left_alone() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        let rt = JournalFact::blind(None);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        for _ in 0..5 {
            e.reconcile_sessions_with(&statuses(&[]), Some(&rt))
                .unwrap();
        }

        let a = live(&e);
        assert!(a.outcome.is_none(), "never ended on a failed lookup");
        assert_eq!(a.resumes, 0, "and never restarted on one either");
        assert_eq!(a.missing_ticks, 5, "but the absence is on the record");
    }

    /// A restart lands on attempts at every stage, including the one that had
    /// just finished. Restarting an agent that already opened its pull request
    /// would spend a turn undoing the work — so the settle check runs first.
    #[test]
    fn a_run_that_died_after_finishing_settles_instead_of_restarting() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr("linear:LIN-142", Some("https://x/pull/1"), Some(1), true)
            .unwrap();
        let rt = JournalFact::ending(None);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        lose_the_run(&e, &rt);

        let a = live(&e);
        assert_eq!(a.outcome, Some(Outcome::Done));
        assert_eq!(a.resumes, 0, "nothing was restarted");
    }

    // ---- the settle the board got wrong (§settle-logic's inverse) ---------

    /// Settle an attempt on commits, returning the checkout's handle — the
    /// tree lives exactly as long as the test holds it.
    fn settled_on_commits(e: &SyncEngine, attempt: i64) -> tempfile::TempDir {
        let (repo, _work) = agent_worked_in(e, attempt, Work::Pushed);
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        assert_eq!(live(e).outcome, Some(Outcome::Done), "fixture must settle");
        repo
    }

    #[test]
    fn a_settled_chat_seen_working_again_is_reopened_not_redispatched() {
        // Commits are routinely there before the work is done, so a settle on
        // them can be wrong. The chat working again is the proof — and it is
        // counted on this attempt as `reopened`, because nobody dispatched
        // anything.
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = settled_on_commits(&e, a);

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        let attempt = live(&e);
        assert!(attempt.outcome.is_none(), "back to work");
        assert_eq!(attempt.reopened, 1);
        assert_eq!(attempt.agent_status, Some(AgentStatus::Working));
        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(task.attempts.len(), 1, "re-opened, not re-dispatched");
    }

    #[test]
    fn a_finished_chat_sitting_idle_leaves_the_settle_standing() {
        // Every finished chat reads Idle forever; only Working reopens.
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = settled_on_commits(&e, a);
        for _ in 0..3 {
            e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
                .unwrap();
        }
        let attempt = live(&e);
        assert_eq!(attempt.outcome, Some(Outcome::Done));
        assert_eq!(attempt.reopened, 0);
    }

    #[test]
    fn reopening_is_refused_once_somebody_redispatched() {
        // The re-dispatch names a decision-maker, and that attempt is the
        // current one — the old chat still typing does not overrule it.
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = settled_on_commits(&e, a);
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
    }

    #[test]
    fn a_task_marked_done_is_not_reopened() {
        // `mark done` is the operator deciding the task is over; an agent
        // still typing does not overrule them.
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = settled_on_commits(&e, a);
        e.db.set_local_done("linear:LIN-142", true).unwrap();

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
    }

    // ---- a settle announced twice with nothing behind it (gh#356) --------

    /// Settle an attempt `chat-parent` released, on a real checkout, so the
    /// notice has a dispatcher to reach. Returns the checkout.
    fn settled_for_its_dispatcher(e: &SyncEngine, rt: &JournalFact) -> tempfile::TempDir {
        let a = dispatch_via(e, "linear:LIN-142", "chat-9", "chat-parent");
        let (repo, _work) = agent_worked_in(e, a, Work::Pushed);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(rt))
            .unwrap();
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(rt))
            .unwrap();
        assert_eq!(live(e).outcome, Some(Outcome::Done), "fixture must settle");
        repo
    }

    /// Wake the settled chat and let its run end again, settling the attempt a
    /// second time — the shape every repeat on the box had.
    fn woke_and_settled_again(e: &SyncEngine, rt: &JournalFact) {
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(rt))
            .unwrap();
        assert_eq!(live(e).reopened, 1, "the reopen is not the thing at fault");
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(rt))
            .unwrap();
        assert_eq!(live(e).outcome, Some(Outcome::Done), "and settles again");
    }

    /// The report, in full. The chat wakes with nothing to do — a review
    /// delivered into it, an operator's follow-up, the agent saying it already
    /// handled the point — the attempt re-opens, and the still-open pull
    /// request settles it again on the spot. The dispatcher hears about it
    /// once, because that is how many things happened.
    #[test]
    fn a_settle_the_dispatcher_already_heard_is_not_sent_twice() {
        let mut e = engine();
        let log = logging(&mut e);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let rt = JournalFact::ending(None);
        let _work = settled_for_its_dispatcher(&e, &rt);
        assert_eq!(rt.prompts().len(), 1, "the first settle is news");

        woke_and_settled_again(&e, &rt);

        assert_eq!(
            rt.prompts().len(),
            1,
            "the same close, the same branch head: {:?}",
            rt.prompts()
        );
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            log.contains("on_settled says what the last one said"),
            "and the log says why nobody was told: {log}"
        );
    }

    /// The repeat that *is* the feature. The review landed, the agent pushed a
    /// fix, and the attempt settled on a commit the dispatcher has never seen —
    /// so it is told, exactly as it was the first time. A guard that suppressed
    /// this would be worse than the bug it closes.
    #[test]
    fn a_reopened_attempt_that_pushes_a_fix_is_announced_again() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let rt = JournalFact::ending(None);
        let repo = settled_for_its_dispatcher(&e, &rt);
        let work = repo.path().join("work");

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        std::fs::write(work.join("fix"), "what the review asked for").unwrap();
        git_in(&work, &["add", "."]);
        git_in(&work, &["commit", "-m", "the fix the review asked for"]);
        git_in(&work, &["push", "origin", "main"]);
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        assert_eq!(
            rt.prompts().len(),
            2,
            "a new commit is news: {:?}",
            rt.prompts()
        );
    }

    /// The operator's endpoint has the same complaint as the orchestrator: a
    /// POST that arrives reads as something having happened. So the suppression
    /// is not per channel — it is the announcement that does not happen.
    #[test]
    fn the_repeat_is_not_posted_to_the_webhook_either() {
        let (e, hook) = engine_with_webhook("https://hook.test/x", false);
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let rt = JournalFact::ending(None);
        let _work = settled_for_its_dispatcher(&e, &rt);
        assert_eq!(hook.posts.lock().unwrap().len(), 1);

        woke_and_settled_again(&e, &rt);

        assert_eq!(hook.posts.lock().unwrap().len(), 1, "one close, one POST");
    }

    /// A cancel on top of a settle is a different fact about the attempt, and
    /// the guard must not swallow it: the outcome is in the print, so an
    /// operator ending an attempt the board had called done still reaches
    /// whoever was waiting on that step (gh#194).
    #[test]
    fn a_different_outcome_on_the_same_attempt_is_still_news() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let rt = JournalFact::ending(None);
        let _work = settled_for_its_dispatcher(&e, &rt);
        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        let attempt = live(&e);

        e.announce_ended(
            Some(&rt),
            &task,
            &attempt,
            Outcome::Cancelled,
            "cancelled from the panel",
        );

        assert_eq!(rt.prompts().len(), 2, "{:?}", rt.prompts());
    }

    // ---- the event path --------------------------------------------------

    #[test]
    fn the_event_path_settles_on_the_transition() {
        // The moment the run ends — not the next interval tick. The 60-second
        // clock this replaces is the whole of §settle-logic.
        let e = engine();
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
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = settled_on_commits(&e, a);

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
        let mut e = engine_with(Some(Github::new(Box::new(fixture) as Box<dyn Rest>)));
        // A Linear task names no repo, so the lookup walks the configured ones.
        e.cfg.github.repos = vec!["o/r".into()];
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = agent_worked_in(&e, a, Work::Pushed);

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
        let mut e = engine_with(Some(Github::new(Box::new(fixture) as Box<dyn Rest>)));
        e.cfg.github.repos = vec!["o/r".into()];
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = agent_worked_in(&e, a, Work::Pushed);

        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Idle)]))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        assert!(outcome_payload(&e)["pr_url"].is_null());
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
match = {{ label = "herd" }}
workspace = "offhand"
repo = "/tmp"
runtime = "claude-code"
max_duration = "{max_duration}"
"#
        ))
        .unwrap();
    }

    /// The cap warning is the one event about a run that is *still going*, and
    /// the only window in which reading the chat can still change how it ends.
    /// So the orchestrator gets it alongside the agent being warned.
    #[test]
    fn the_cap_warning_reaches_the_orchestrator_before_the_kill() {
        let mut e = engine();
        e.cfg.defaults.orchestrator_chat = Some("chat-boss".into());
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        age_attempt(&e, a, 3 * 3600, None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        let prompts = rt.prompts.borrow().clone();
        assert_eq!(
            prompts.len(),
            2,
            "the agent and the orchestrator: {prompts:?}"
        );
        let boss = prompts
            .iter()
            .find(|(chat, _)| chat == "chat-boss")
            .expect("the orchestrator was told");
        assert!(boss.1.contains("past its time cap"));
        assert!(boss.1.contains("cap 2h"), "{}", boss.1);
        assert!(rt.cancels.borrow().is_empty(), "a warning is not a kill");
    }

    /// The orchestrator is supposed to live forever, so the clock that exists
    /// to stop a looping agent must not stop it — and must say so once rather
    /// than every cycle for the rest of the box's uptime.
    #[test]
    fn the_pinned_chat_is_exempt_from_the_duration_cap() {
        let mut e = engine();
        e.cfg.defaults.orchestrator_chat = Some("chat-9".into());
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        age_attempt(&e, a, 3 * 3600, None);

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        assert!(live(&e).outcome.is_none());
        assert!(
            live(&e).overrun_warned_at.is_some(),
            "stamped, so the log line is said once and not every cycle"
        );
        assert!(
            rt.prompts.borrow().is_empty(),
            "and it is not warned about a cap that will never bite it"
        );

        // Well past the grace, and still alive.
        age_attempt(&e, a, 9 * 3600, Some(overrun::MAX_GRACE_SECS as i64 * 3));
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();
        assert!(live(&e).outcome.is_none(), "never cancelled");
        assert!(rt.cancels.borrow().is_empty());
    }

    /// The exit criterion's quiet half: a legit long run is untouched. Two
    /// hours is the default cap and this one is one hour in.
    #[test]
    fn a_run_inside_its_cap_is_left_alone() {
        let e = engine();
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
        let e = engine();
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
        let e = engine();
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

    /// gh#194: the cap's *warning* goes to the orchestrator because a run that
    /// is still going is nothing for a dispatcher to act on. Its ending is the
    /// opposite — the step that agent was waiting on is over and will not
    /// finish — and it used to reach nobody at all: the row closed, the
    /// checkout and the chat started their retention clocks, and the only trace
    /// of the whole event was a colour on a board nothing was watching.
    #[test]
    fn an_attempt_killed_at_the_cap_tells_the_agent_that_released_it() {
        let e = engine();
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch_via(&e, "linear:LIN-142", "chat-9", "chat-parent");
        age_attempt(&e, a, 3 * 3600, Some(overrun::MAX_GRACE_SECS as i64));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        assert_eq!(live(&e).outcome, Some(Outcome::Failed));
        let prompts = rt.prompts.borrow().clone();
        let told = prompts
            .iter()
            .find(|(chat, _)| chat == "chat-parent")
            .unwrap_or_else(|| panic!("the chat that released it: {prompts:?}"));
        assert!(
            told.1.contains("work you released has finished"),
            "{}",
            told.1
        );
        // `failed` alone reads as a dispatch that never produced an agent. The
        // reason the outcome comment carries upstream rides the notice too, so
        // the chat and the issue describe one close in one wording.
        assert!(told.1.contains("failed — timed out after 3h"), "{}", told.1);
        assert!(told.1.contains("cap 2h"), "{}", told.1);
    }

    /// And the same ending with nobody to address it is a logged drop rather
    /// than a silence — the cap is the one close no human asked for, so an
    /// unwitnessed one is the worst kind to leave untraced.
    #[test]
    fn a_cap_kill_nobody_can_be_told_about_says_so() {
        let mut e = engine();
        let log = logging(&mut e);
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        age_attempt(&e, a, 3 * 3600, Some(overrun::MAX_GRACE_SECS as i64));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Working)]), Some(&rt))
            .unwrap();

        assert_eq!(live(&e).outcome, Some(Outcome::Failed));
        let log = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(log.contains("on_settled reached no agent"), "{log}");
    }

    /// gh#70's second hole. The engine crashed past its revival budget, so the
    /// chat settled `Idle`; with no commits `settled::decide` says
    /// `StayLive(NoArtifacts)`, and orphaning fires only on a *missing* session
    /// row — this one exists. Before the cap, that row rendered `working`
    /// forever.
    #[test]
    fn a_stranded_idle_row_with_no_artifacts_is_closed_by_the_clock() {
        let e = engine();
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
        let e = engine();
        let rt = CapWatcher::default();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let _work = agent_worked_in(&e, a, Work::Pushed);
        age_attempt(&e, a, 5 * 3600, Some(overrun::MAX_GRACE_SECS as i64));

        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();
        assert_eq!(live(&e).outcome, Some(Outcome::Done));
        assert!(
            rt.cancels.borrow().is_empty(),
            "a finished run is not killed"
        );
    }

    /// The cap is a property of the work, so it is per route: the refactor
    /// route gets six hours without loosening the one that fixes typos.
    #[test]
    fn a_route_can_raise_the_cap_over_the_default() {
        let mut e = engine();
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
        let mut e = engine();
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
    /// it (§runtime-impl). The clock is what closes it — with nothing to
    /// interrupt, and the reason still written back.
    #[test]
    fn a_dispatch_that_never_got_a_chat_is_closed_by_the_clock_too() {
        let e = engine();
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
        let e = engine();
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
        let e = engine();
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
        let e = engine();
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
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");
        seed(&e, "linear:LIN-999", "LIN-999", UpstreamState::Started);

        let pr = PullRequest {
            repo: "o/r".into(),
            number: 291,
            title: "Add retry".into(),
            body: None,
            url: "https://github.com/o/r/pull/291".into(),
            head_ref: "board/lin-142".into(),
            base_ref: "main".into(),
            head_repo: None,
            base_sha: None,
            open: true,
            merged: false,
            draft: false,
            stack: None,
            updated_at: crate::db::now(),
        };
        let branches = e.attempt_branches();
        assert!(!e.should_import_pull_request_row(&branches, &pr));
        e.link_pull_requests(std::slice::from_ref(&pr)).unwrap();

        assert!(e.db.get_task("linear:LIN-142").unwrap().unwrap().pr_open);
        // The unrelated task must not pick up the PR.
        assert!(!e.db.get_task("linear:LIN-999").unwrap().unwrap().pr_open);
    }

    /// gh#548. The brief asks for a closing reference; this is the board
    /// checking the answer at first sight of the request. A body GitHub cannot
    /// parse — Tally's `**Lukker gh#932.**`, Norwegian keyword, board-flavoured
    /// reference — is warned about once, with the repair in the warning.
    #[test]
    fn a_request_that_announces_closure_in_prose_is_warned_about_once() {
        let mut e = engine();
        let log_path = e.paths.state_dir.join("syncd.log");
        e.log = Arc::new(Logger::new(&log_path, false));
        seed(&e, "gh:o/r#7", "gh#7", UpstreamState::Started);
        dispatch(&e, "gh:o/r#7", "chat-9");

        let pr = PullRequest {
            repo: "o/r".into(),
            number: 291,
            title: "Add retry".into(),
            body: Some("**Lukker gh#7.**".into()),
            url: "https://github.com/o/r/pull/291".into(),
            head_ref: "board/lin-142".into(),
            base_ref: "main".into(),
            head_repo: None,
            base_sha: None,
            open: true,
            merged: false,
            draft: false,
            stack: None,
            updated_at: crate::db::now(),
        };
        e.link_pull_requests(std::slice::from_ref(&pr)).unwrap();
        let logged = |path: &std::path::Path| std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            logged(&log_path).contains("announces closure in prose"),
            "{}",
            logged(&log_path)
        );
        assert!(
            logged(&log_path).contains("`Closes #7`"),
            "{}",
            logged(&log_path)
        );

        // Once per request: the next poll re-links everything it saw, and a
        // warning that repeats every cycle is noise an operator filters out.
        std::fs::write(&log_path, "").unwrap();
        e.link_pull_requests(std::slice::from_ref(&pr)).unwrap();
        assert!(
            !logged(&log_path).contains("announces closure in prose"),
            "{}",
            logged(&log_path)
        );
    }

    /// And the check accepts what GitHub accepts, so a body carrying the real
    /// thing is never warned about.
    #[test]
    fn a_parseable_closing_reference_is_never_warned_about() {
        let mut e = engine();
        let log_path = e.paths.state_dir.join("syncd.log");
        e.log = Arc::new(Logger::new(&log_path, false));
        seed(&e, "gh:o/r#7", "gh#7", UpstreamState::Started);
        dispatch(&e, "gh:o/r#7", "chat-9");

        let pr = PullRequest {
            repo: "o/r".into(),
            number: 291,
            title: "Add retry".into(),
            body: Some("Håndterer tilstanden.\n\nCloses #7.".into()),
            url: "https://github.com/o/r/pull/291".into(),
            head_ref: "board/lin-142".into(),
            base_ref: "main".into(),
            head_repo: None,
            base_sha: None,
            open: true,
            merged: false,
            draft: false,
            stack: None,
            updated_at: crate::db::now(),
        };
        e.link_pull_requests(std::slice::from_ref(&pr)).unwrap();
        // The logger opens its file lazily on first write; silence here means
        // no file at all, which is exactly the absence being asserted.
        assert!(
            !std::fs::read_to_string(&log_path)
                .unwrap_or_default()
                .contains("announces")
        );
    }

    #[test]
    fn linking_a_pull_request_records_its_topology_and_forgets_it_again() {
        // gh#282. The linked PR is the only place a Linear row learns where its
        // branch lands, and a stack a PR has left must not linger on the row —
        // sibling grouping would keep merging it into a stack it is out of.
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        let mut pr = PullRequest {
            repo: "o/r".into(),
            number: 291,
            title: "Add retry".into(),
            body: None,
            url: "https://github.com/o/r/pull/291".into(),
            head_ref: "board/lin-142".into(),
            base_ref: "board/lin-141".into(),
            head_repo: None,
            base_sha: None,
            open: true,
            merged: false,
            draft: false,
            stack: Some(crate::model::PrStack {
                number: 4,
                size: Some(3),
                position: Some(2),
                base_ref: Some("main".into()),
            }),
            updated_at: crate::db::now(),
        };
        e.link_pull_requests(std::slice::from_ref(&pr)).unwrap();

        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(task.pr_base_ref.as_deref(), Some("board/lin-141"));
        assert_eq!(
            task.pr_stack,
            Some(crate::model::PrStack {
                number: 4,
                size: Some(3),
                position: Some(2),
                base_ref: Some("main".into()),
            })
        );

        // The parent merged, the PR was retargeted onto main, and it is no
        // longer stacked.
        pr.base_ref = "main".into();
        pr.stack = None;
        e.link_pull_requests(std::slice::from_ref(&pr)).unwrap();

        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(task.pr_base_ref.as_deref(), Some("main"));
        assert_eq!(task.pr_stack, None);
    }

    // ---- an agent-authored stack (gh#287) --------------------------------

    /// One layer of a stack an agent wrote for its own task, as the poll sees
    /// it. `position` 1 is the attempt branch itself.
    fn layer_pr(number: i64, head_ref: &str, position: i64, merged: bool) -> PullRequest {
        PullRequest {
            repo: "o/r".into(),
            number,
            title: format!("layer {position}"),
            body: None,
            url: format!("https://github.com/o/r/pull/{number}"),
            head_ref: head_ref.into(),
            base_ref: "main".into(),
            head_repo: None,
            base_sha: None,
            open: !merged,
            merged,
            draft: false,
            stack: Some(crate::model::PrStack {
                number: 4,
                size: Some(2),
                position: Some(position),
                base_ref: Some("main".into()),
            }),
            updated_at: crate::db::now(),
        }
    }

    /// The layers above the first are branches the board never named, and the
    /// naming convention is the only thing that says whose they are. Without it
    /// each one is imported as a row of its own — dispatchable, and reviewed by
    /// nobody.
    #[test]
    fn the_upper_layers_of_a_stack_are_the_attempts_work_and_not_rows_of_their_own() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        let bottom = layer_pr(291, "board/lin-142", 1, false);
        let top = layer_pr(292, "board/lin-142-2", 2, false);
        let branches = e.attempt_branches();
        assert!(!e.should_import_pull_request_row(&branches, &bottom));
        assert!(!e.should_import_pull_request_row(&branches, &top));

        e.link_pull_requests(&[top.clone(), bottom.clone()])
            .unwrap();
        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        // The row points at the bottom whichever order the poll returned them:
        // it is the branch the board named and the attempt recorded, and the
        // one review delivery can match an attempt to.
        assert_eq!(task.pr_number, Some(291));
        assert!(task.pr_open);
    }

    /// A branch that merely reads like a layer is not one. GitHub's own `stack`
    /// object is what separates the two, and without it the pull request stays
    /// a row of its own rather than being swallowed by an attempt.
    #[test]
    fn a_branch_that_only_looks_like_a_layer_is_still_a_row_of_its_own() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        let mut unstacked = layer_pr(292, "board/lin-142-2", 2, false);
        unstacked.stack = None;
        assert!(e.should_import_pull_request_row(&e.attempt_branches(), &unstacked));

        e.link_pull_requests(std::slice::from_ref(&unstacked))
            .unwrap();
        assert!(!e.db.get_task("linear:LIN-142").unwrap().unwrap().pr_open);
    }

    /// The bottom of a stack merges first, and merging it finishes nothing. A
    /// task closed on it would close its issue and let the GC take the checkout
    /// the layers above are still being written in.
    #[test]
    fn a_bottom_layer_that_merges_first_does_not_finish_the_task() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        let bottom = layer_pr(291, "board/lin-142", 1, true);
        let top = layer_pr(292, "board/lin-142-2", 2, false);
        e.link_pull_requests(&[bottom, top]).unwrap();

        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert!(!task.pr_merged, "one layer of two is not a merged stack");
        assert!(task.pr_open, "the work is not over while a layer is open");
        assert!(!task.local_done);
    }

    /// And when the last of them lands, the whole thing has: the row merges,
    /// exactly as a single pull request's does.
    #[test]
    fn the_task_finishes_when_the_whole_stack_has_merged() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        let bottom = layer_pr(291, "board/lin-142", 1, true);
        let top = layer_pr(292, "board/lin-142-2", 2, true);
        e.link_pull_requests(&[bottom, top]).unwrap();

        let task = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        assert!(task.pr_merged);
        assert!(!task.pr_open);
        assert!(task.local_done, "the merge finished the task");
    }

    // ---- the first real stack, end to end (gh#388) ------------------------
    //
    // The nine stacks issues landed against fixtures somebody wrote, and gh#337
    // asked that one payload GitHub actually sent be put through the whole path
    // before the vocabulary is trusted. Stack 49 on
    // `Florin-AS/orion-productmapping` is that payload — three layers, filed
    // from the board, merged the same morning — and it enters these tests as
    // the JSON `GET /pulls` returned and leaves as the words a reader reads.
    //
    // The question gh#388 raised is the one below: **every layer of it reports
    // `mergeable_state: clean`**, including the two that cannot reach `main`
    // alone. GitHub is not wrong — each is clean against its own base — and
    // `clean` is the field a reader reaches for. What the board must not do is
    // pass it on.

    /// Stack 49, as GitHub answered it. The fixture's `_provenance` says what
    /// was recorded, what was withheld and which three fields of the open
    /// snapshot were restored to their values inside the window.
    const STACK_49: &str = include_str!("../fixtures/gh-388-stack-49.json");

    const STACK_49_REPO: &str = "Florin-AS/orion-productmapping";

    /// The board after one poll of stack 49 — `snapshot` is `open` for the
    /// three layers as gh#388 found them, `merged` for the same three after
    /// they landed.
    ///
    /// Nothing here is hand-built: the pull requests are parsed by
    /// [`Github::pulls`] out of the recorded payload, linked by the poll's own
    /// [`SyncEngine::link_pull_requests`], and read back as the rows
    /// [`crate::rows::board_rows`] streams to both viewports.
    fn stack_49(snapshot: &str) -> Vec<comet_proto::view::board::TaskRow> {
        stack_49_board(snapshot).1
    }

    fn stack_49_board(snapshot: &str) -> (TestEngine, Vec<comet_proto::view::board::TaskRow>) {
        let fixture: Value = serde_json::from_str(STACK_49).unwrap();
        let repo = STACK_49_REPO;
        // Most specific first: `FixtureRest` answers on a path prefix, and
        // `/repos/{repo}/pulls` is a prefix of `/repos/{repo}/pulls/47`. The
        // per-pull route is the mergeability call the full sweep makes.
        let mut routes: Vec<(String, Value)> = fixture["mergeable_open"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(number, state)| (format!("/repos/{repo}/pulls/{number}"), state.clone()))
            .collect();
        routes.push((format!("/repos/{repo}/pulls?"), fixture[snapshot].clone()));
        let e = engine_with(
            Some(Github::new(
                Box::new(FixtureRest::new(routes)) as Box<dyn Rest>
            )),
        );

        // The three issues the layers were written for, each dispatched onto
        // the branch its own layer heads — this is what makes them three rows
        // of one stack rather than three strangers — and gh#33, whose pull
        // request rides the same poll and is in no stack at all (gh#337's
        // control). gh#33 is merged already: no unstacked pull request was
        // open while stack 49 was, so the nearest real one is the last to
        // close before it.
        for (issue, branch) in [
            (33, "board/gh-33-orion-productmapping"),
            (44, "board/gh-44-packages-kind-decided"),
            (45, "board/gh-45-auto-promote-bug"),
            (46, "board/gh-46-auto-ready-auto"),
        ] {
            let id = format!("gh:{repo}#{issue}");
            seed_gh_in(&e, &id);
            dispatch_on(&e, &id, branch);
        }

        let pulls = e.github.as_ref().unwrap().pulls(repo).unwrap();
        e.link_pull_requests(&pulls).unwrap();
        e.rederive_all().unwrap();
        let rows = crate::rows::board_rows(&e.db, &e.cfg).unwrap();
        (e, rows)
    }

    fn layer_row(
        rows: &[comet_proto::view::board::TaskRow],
        issue: i64,
    ) -> comet_proto::view::board::TaskRow {
        let id = format!("gh:{STACK_49_REPO}#{issue}");
        rows.iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("no row for {id}"))
            .clone()
    }

    /// gh#337 case 1: the three arrive as one thing. Every row carries the
    /// whole chain, its own place in it, and the branch the chain lands on —
    /// off a payload where the only link between the three is GitHub's `stack`
    /// object and a `base` pointing at the branch below.
    #[test]
    fn the_real_stack_arrives_as_one_chain_on_every_row() {
        let rows = stack_49("open");
        for (issue, position) in [(44, 1), (45, 2), (46, 3)] {
            let row = layer_row(&rows, issue);
            let stack = row
                .stack
                .as_ref()
                .unwrap_or_else(|| panic!("gh#{issue} arrived with no stack on it"));
            assert_eq!(stack.number, 49);
            assert_eq!(stack.size, Some(3));
            assert_eq!(stack.position, Some(position));
            assert_eq!(stack.base_ref.as_deref(), Some("main"));
            // The `#47 ↑ #48 ↑ #50` map, bottom first, the same on all three.
            assert_eq!(
                comet_proto::view::board::stack_map(&row)
                    .iter()
                    .map(|l| l.pr_number)
                    .collect::<Vec<_>>(),
                vec![Some(47), Some(48), Some(50)],
            );
            assert_eq!(
                comet_proto::view::board::stack_note(&row).as_deref(),
                Some(format!("{position} of 3").as_str()),
            );
        }
    }

    /// gh#337 case 2 and 3, and the whole of gh#388: what the layers *read* as
    /// when every one of them reports `clean`.
    ///
    /// The raw field is on the row — the board keeps GitHub's answer — and no
    /// surface prints it. What they print is the verdict [`landing`] derived
    /// from it, which for this payload is the one shape where a mid-stack
    /// `clean` may say `ready`: every layer below is clean too, so merging any
    /// of them lands the ones underneath with it, atomically (gh#290). The
    /// count is the part that makes it honest — "ready to land" alone would
    /// claim this pull request lands by itself, and PR 50 lands three.
    ///
    /// The wording gh#337 asked for (`waiting on PR #47`) is the answer to a
    /// *different* payload — a layer below that GitHub objects to — and
    /// `a_clean_child_over_a_dirty_parent_is_not_ready_to_land` in `stacks.rs`
    /// is where that one is held. Stack 49 never entered that state.
    #[test]
    fn a_clean_layer_over_clean_layers_says_what_merging_it_would_do() {
        let rows = stack_49("open");
        let now = chrono::Utc::now();
        let read = |issue: i64| {
            let row = layer_row(&rows, issue);
            (
                row.pr_mergeable.clone(),
                row.landing.clone(),
                comet_proto::view::board::landing_note(&row),
                comet_proto::view::board::row_metadata_line(&row, false, now),
            )
        };

        // The bottom layer: nothing below it, so merging it lands one thing.
        let (mergeable, landing, note, line) = read(44);
        assert_eq!(mergeable.as_deref(), Some("clean"));
        assert_eq!(landing.as_deref(), Some("ready"));
        assert_eq!(note.as_deref(), Some("ready to land"));
        assert_eq!(line, "1 of 3 · ready to land · in PR #47");

        // The middle layer. `clean` here is clean against
        // `board/gh-44-packages-kind-decided`, and the row says so by naming
        // what comes along rather than by repeating GitHub's word.
        let (mergeable, landing, note, line) = read(45);
        assert_eq!(mergeable.as_deref(), Some("clean"));
        assert_eq!(landing.as_deref(), Some("ready"));
        assert_eq!(note.as_deref(), Some("ready to land with 1 below"));
        assert_eq!(line, "2 of 3 · ready to land with 1 below · in PR #48");

        // The top layer, three pull requests deep.
        let (mergeable, landing, note, line) = read(46);
        assert_eq!(mergeable.as_deref(), Some("clean"));
        assert_eq!(landing.as_deref(), Some("ready"));
        assert_eq!(note.as_deref(), Some("ready to land with 2 below"));
        assert_eq!(line, "3 of 3 · ready to land with 2 below · in PR #50");

        // Nowhere on any row does the flat word reach a reader.
        for issue in [44, 45, 46] {
            let row = layer_row(&rows, issue);
            assert!(
                !comet_proto::view::board::row_metadata_line(&row, false, now).contains("clean"),
                "gh#{issue} passed GitHub's `clean` through to the row"
            );
        }
    }

    /// The detail surface, on the same payload: which layer this is, the branch
    /// it sits on, where the chain lands — and, on the one irreversible key,
    /// the pull requests the merge takes with it by number.
    #[test]
    fn the_detail_surface_names_the_branch_below_and_the_merge_it_would_make() {
        let rows = stack_49("open");

        let bottom = layer_row(&rows, 44);
        assert_eq!(
            comet_proto::view::board::stack_line(&bottom).as_deref(),
            Some("stack 1 of 3 · lands on main"),
            "the bottom layer's base *is* the target; saying it twice reads as two branches",
        );
        assert_eq!(
            comet_proto::view::board::merge_confirmation(&bottom),
            "merge gh#44 (PR #47) into main",
        );

        let top = layer_row(&rows, 46);
        assert_eq!(
            comet_proto::view::board::stack_line(&top).as_deref(),
            Some("stack 3 of 3 · onto board/gh-45-auto-promote-bug · lands on main"),
        );
        assert_eq!(
            comet_proto::view::board::merge_confirmation(&top),
            "merge gh#46 (PR #50) into main · this lands PR #47, PR #48 with it \
             — GitHub merges the group or none of it",
        );
    }

    /// gh#337 case 4: an unstacked pull request of the same repository, in the
    /// same poll, is untouched by any of it. PR 42 carries `stack: null`, so it
    /// is grouped into nothing, says nothing about a position, and answers for
    /// itself alone.
    #[test]
    fn an_unstacked_pull_request_in_the_same_repo_is_untouched() {
        let rows = stack_49("open");
        for row in &rows {
            if let Some(stack) = row.stack.as_ref() {
                assert!(
                    stack.layers.iter().all(|l| l.pr_number != Some(42)),
                    "PR 42 was grouped into stack 49",
                );
            }
        }
        // And on its own row: no stack, no position, no map to draw.
        let row = layer_row(&rows, 33);
        assert_eq!(row.pr_number, Some(42), "gh#33's own pull request");
        assert!(row.stack.is_none());
        assert_eq!(comet_proto::view::board::stack_note(&row), None);
        assert!(comet_proto::view::board::stack_map(&row).is_empty());
        assert_eq!(comet_proto::view::board::stack_line(&row), None);
        assert_eq!(
            comet_proto::view::board::landing(&row),
            comet_proto::view::board::Landing::Unknown,
            "a closed pull request nobody asked GitHub about answers nothing",
        );
    }

    /// The same three layers after they landed — 09:15:08, :09 and :11, each
    /// into its own base, which is what a group merge looks like from outside.
    ///
    /// Two things have to hold on the merged payload: no row claims it can land
    /// (there is nothing left to merge, and GitHub answers `unknown` for a
    /// closed pull request rather than `clean`), and each task is finished by
    /// its own layer merging.
    #[test]
    fn the_merged_stack_leaves_nothing_claiming_it_can_land() {
        let (e, rows) = stack_49_board("merged");
        let now = chrono::Utc::now();
        for issue in [44, 45, 46] {
            let task =
                e.db.get_task(&format!("gh:{STACK_49_REPO}#{issue}"))
                    .unwrap()
                    .unwrap();
            assert!(task.pr_merged, "gh#{issue} merged with the group");
            assert!(!task.pr_open);
            assert!(task.local_done, "the merge finished gh#{issue}");
            let row = layer_row(&rows, issue);
            assert_eq!(row.landing, None, "a merged layer has nothing to land");
            let line = comet_proto::view::board::row_metadata_line(&row, false, now);
            assert!(
                !line.contains("ready to land"),
                "gh#{issue} still offers to land: {line}"
            );
        }
    }

    #[derive(Clone)]
    struct DirectReviewChat {
        candidates: Vec<crate::runtime::ReviewCandidate>,
    }

    impl Runtime for DirectReviewChat {
        fn dispatch(&self, _: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
            unreachable!("adoption never dispatches")
        }
        fn prompt(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn cancel(&self, _: &str) -> anyhow::Result<()> {
            unreachable!("adoption never cancels")
        }
        fn session(&self, _: &str) -> anyhow::Result<Option<comet_proto::Session>> {
            Ok(None)
        }
        fn chat_alive(&self, _: &str) -> anyhow::Result<bool> {
            Ok(true)
        }
        fn chat_cwd(&self, _: &str) -> anyhow::Result<Option<String>> {
            Ok(self
                .candidates
                .first()
                .and_then(|candidate| candidate.worktree.clone()))
        }
        fn review_candidates(&self) -> anyhow::Result<Vec<crate::runtime::ReviewCandidate>> {
            Ok(self.candidates.clone())
        }
        fn last_run_end(&self, _: &str) -> anyhow::Result<Option<RunEnd>> {
            Ok(Some(RunEnd::Completed))
        }
        fn run_commands(
            &self,
            _: &str,
        ) -> anyhow::Result<Option<Vec<crate::evidence::RanCommand>>> {
            Ok(Some(vec![crate::evidence::RanCommand {
                command: "cargo test -p comet-board".into(),
                failed: false,
            }]))
        }
    }

    #[test]
    fn a_pr_from_an_ordinary_comet_chat_gets_an_attempt_backed_review() {
        let e = engine();
        let (_repo, work) = repo_ahead_of_its_remote();
        git_in(
            &work,
            &["remote", "set-url", "origin", "https://github.com/o/r.git"],
        );
        let base = git_out(&work.to_string_lossy(), &["rev-parse", "origin/main"]).unwrap();
        git_in(&work, &["switch", "-c", "direct-pr"]);
        std::fs::write(work.join("agent-change"), "made in Comet").unwrap();
        git_in(&work, &["add", "."]);
        git_in(&work, &["commit", "-m", "agent change"]);

        let pr = PullRequest {
            repo: "o/r".into(),
            number: 265,
            title: "Made by a normal Comet agent".into(),
            body: None,
            url: "https://github.com/o/r/pull/265".into(),
            head_ref: "direct-pr".into(),
            base_ref: "main".into(),
            head_repo: None,
            base_sha: Some(base.clone()),
            open: true,
            merged: false,
            draft: false,
            stack: None,
            updated_at: crate::db::now(),
        };
        e.db.upsert_task(&pr.to_upsert()).unwrap();
        e.db.set_pr(&pr.task_id(), Some(&pr.url), Some(pr.number), true)
            .unwrap();
        let runtime = DirectReviewChat {
            candidates: vec![
                crate::runtime::ReviewCandidate {
                    chat_id: "comet-chat".into(),
                    workspace: "comet-board".into(),
                    runtime: "claude-code".into(),
                    worktree: Some(work.to_string_lossy().into_owned()),
                    repo: Some("o/r".into()),
                    branch: Some("direct-pr".into()),
                    pull_request_urls: vec![pr.url.clone()],
                    created_pull_request: false,
                    account: None,
                    created_at: chrono::Utc::now(),
                },
                crate::runtime::ReviewCandidate {
                    chat_id: "newer-unrelated-chat".into(),
                    workspace: "comet-board".into(),
                    runtime: "codex".into(),
                    worktree: Some(work.to_string_lossy().into_owned()),
                    repo: Some("o/r".into()),
                    branch: Some("direct-pr".into()),
                    pull_request_urls: Vec::new(),
                    created_pull_request: false,
                    account: None,
                    created_at: chrono::Utc::now() + chrono::Duration::seconds(1),
                },
            ],
        };

        e.adopt_session_pull_requests(std::slice::from_ref(&pr), &runtime)
            .unwrap();

        let task = e.db.get_task(&pr.task_id()).unwrap().unwrap();
        assert_eq!(task.attempts.len(), 1);
        let attempt = &task.attempts[0];
        assert_eq!(attempt.pane_id.as_deref(), Some("comet-chat"));
        assert_eq!(attempt.outcome, Some(Outcome::Done));
        assert!(!attempt.board_managed, "the person's checkout stays theirs");
        assert_eq!(e.db.live_count_in_workspace("comet-board").unwrap(), 0);
        assert!(e.db.settled_attempts().unwrap().is_empty());
        assert!(e.db.collectable_attempts().unwrap().is_empty());
        assert!(e.db.archivable_chat_attempts().unwrap().is_empty());
        assert!(
            !e.rewatch_settled_attempts(
                &statuses(&[("comet-chat", AgentStatus::Working)]),
                Some(&runtime)
            )
            .unwrap(),
            "a normal chat continuing work is not reopened as a Board run"
        );
        let branches = e.attempt_branches();
        assert!(
            e.should_import_pull_request_row(&branches, &pr),
            "an adopted PR row must not disappear behind its own attempt"
        );
        let review = e.review(&pr.task_id(), None).unwrap();
        assert_eq!(review.diff, crate::claims::DiffSource::Checkout);
        assert!(
            review
                .changed
                .iter()
                .any(|file| file.path == "agent-change"),
            "the direct chat's PR receives the real branch review"
        );
        assert!(
            !review.claimed(),
            "missing claims remain unknown, never clean"
        );

        std::fs::write(work.join("later-change"), "after opening the PR").unwrap();
        git_in(&work, &["add", "."]);
        git_in(&work, &["commit", "-m", "continue after opening PR"]);
        e.adopt_session_pull_requests(std::slice::from_ref(&pr), &runtime)
            .unwrap();
        let refreshed = e.review(&pr.task_id(), None).unwrap();
        assert!(
            refreshed
                .changed
                .iter()
                .any(|file| file.path == "later-change"),
            "the review follows work committed after the PR first appeared"
        );
        assert_eq!(
            e.db.get_task(&pr.task_id())
                .unwrap()
                .unwrap()
                .attempts
                .len(),
            1,
            "refreshing the review does not invent another run"
        );

        e.db.set_pr(&pr.task_id(), Some(&pr.url), Some(pr.number), false)
            .unwrap();
        e.db.set_local_done(&pr.task_id(), true).unwrap();
        e.rederive_all().unwrap();
        e.collect_worktrees(Some(&runtime));
        e.sweep_build_output(Some(&runtime));
        e.archive_chats(Some(&runtime));
        let preserved = e.db.get_attempt(attempt.id).unwrap().unwrap();
        assert_eq!(preserved.collectable_at, None);
        assert_eq!(preserved.cache_sweepable_at, None);
        assert_eq!(preserved.chat_archivable_at, None);
    }

    #[test]
    fn a_remote_comet_chat_can_make_its_unique_pr_reviewable() {
        let e = engine();
        let pr = PullRequest {
            repo: "o/r".into(),
            number: 266,
            title: "Made on the other Mac".into(),
            body: None,
            url: "https://github.com/o/r/pull/266".into(),
            head_ref: "comet/remote-pr".into(),
            base_ref: "main".into(),
            head_repo: None,
            base_sha: Some("base".into()),
            open: true,
            merged: false,
            draft: false,
            stack: None,
            updated_at: crate::db::now(),
        };
        e.db.upsert_task(&pr.to_upsert()).unwrap();
        e.db.set_pr(&pr.task_id(), Some(&pr.url), Some(pr.number), true)
            .unwrap();
        let runtime = DirectReviewChat {
            candidates: vec![crate::runtime::ReviewCandidate {
                chat_id: "remote-chat".into(),
                workspace: "remote-space".into(),
                runtime: "codex".into(),
                // A synced path from another device is deliberately not used
                // as a local checkout. The attempt still carries the chat.
                worktree: None,
                repo: None,
                branch: None,
                pull_request_urls: vec![pr.url.clone()],
                created_pull_request: false,
                account: None,
                created_at: chrono::Utc::now(),
            }],
        };
        let same_branch_elsewhere = PullRequest {
            repo: "another/repo".into(),
            number: 9,
            title: "Same branch name elsewhere".into(),
            body: None,
            url: "https://github.com/another/repo/pull/9".into(),
            head_ref: "comet/remote-pr".into(),
            base_ref: "main".into(),
            head_repo: None,
            base_sha: Some("other-base".into()),
            open: true,
            merged: false,
            draft: false,
            stack: None,
            updated_at: crate::db::now(),
        };

        e.adopt_session_pull_requests(&[pr.clone(), same_branch_elsewhere], &runtime)
            .unwrap();

        let task = e.db.get_task(&pr.task_id()).unwrap().unwrap();
        assert_eq!(task.attempts.len(), 1);
        assert_eq!(task.attempts[0].pane_id.as_deref(), Some("remote-chat"));
        assert_eq!(task.attempts[0].worktree, None);
        assert!(
            matches!(
                e.review(&pr.task_id(), None).unwrap().diff,
                crate::claims::DiffSource::Unavailable { .. }
            ),
            "a remote diff is unknown, never clean"
        );
    }

    #[test]
    fn ambiguous_comet_authorship_keeps_review_and_guesses_no_chat() {
        let e = engine();
        let pr = PullRequest {
            repo: "upstream/r".into(),
            number: 267,
            title: "Forked work".into(),
            body: None,
            url: "https://github.com/upstream/r/pull/267".into(),
            head_ref: "feature".into(),
            base_ref: "main".into(),
            head_repo: Some("person/r".into()),
            base_sha: Some("base".into()),
            open: true,
            merged: false,
            draft: false,
            stack: None,
            updated_at: crate::db::now(),
        };
        e.db.upsert_task(&pr.to_upsert()).unwrap();
        e.db.set_pr(&pr.task_id(), Some(&pr.url), Some(pr.number), true)
            .unwrap();
        let candidate = |chat_id: &str, created_at| crate::runtime::ReviewCandidate {
            chat_id: chat_id.into(),
            workspace: "fork".into(),
            runtime: "codex".into(),
            worktree: None,
            repo: Some("person/r".into()),
            branch: Some("feature".into()),
            pull_request_urls: Vec::new(),
            created_pull_request: false,
            account: None,
            created_at,
        };
        let now = chrono::Utc::now();
        let runtime = DirectReviewChat {
            candidates: vec![
                candidate("older-chat", now),
                candidate("newer-chat", now + chrono::Duration::seconds(1)),
            ],
        };

        e.adopt_session_pull_requests(std::slice::from_ref(&pr), &runtime)
            .unwrap();

        let task = e.db.get_task(&pr.task_id()).unwrap().unwrap();
        assert_eq!(task.attempts.len(), 1, "the review door is present");
        assert_eq!(
            task.attempts[0].pane_id, None,
            "recency is not author proof"
        );
        assert_eq!(task.attempts[0].worktree, None);
    }

    /// The row Brede watched go `review` → `done` unread (§gh#344): a pull
    /// request opened by an agent that never dispatched, with no comet chat
    /// this board can see. There is no attempt, and the review is assembled
    /// from the pull request anyway.
    #[test]
    fn a_pull_request_nobody_dispatched_is_reviewed_from_the_pull_request() {
        let mut e = engine_with(
            Some(Github::new(Box::new(FixtureRest::new(vec![(
                "/repos/b/itsm-agent/pulls/191/files".into(),
                json!([
                    { "filename": "src/approval.rs", "status": "modified",
                      "additions": 44, "deletions": 9, "patch": "@@" },
                    { "filename": ".github/workflows/ci.yml", "status": "modified",
                      "additions": 3, "deletions": 1, "patch": "@@" }
                ]),
            )])) as Box<dyn Rest>)),
        );
        e.cfg.github.repos = vec!["b/itsm-agent".into()];
        let pr = PullRequest {
            repo: "b/itsm-agent".into(),
            number: 191,
            title: "fix: restore approval lifecycle and CI".into(),
            body: None,
            url: "https://github.com/b/itsm-agent/pull/191".into(),
            head_ref: "codex/restore-green-main".into(),
            base_ref: "main".into(),
            head_repo: None,
            base_sha: None,
            open: true,
            merged: false,
            draft: false,
            stack: None,
            updated_at: crate::db::now(),
        };
        e.db.upsert_task(&pr.to_upsert()).unwrap();
        e.db.set_pr(&pr.task_id(), Some(&pr.url), Some(pr.number), true)
            .unwrap();
        e.db.set_pr_topology(&pr.task_id(), Some(&pr.base_ref), Some(&pr.head_ref), None)
            .unwrap();
        let task = e.db.get_task(&pr.task_id()).unwrap().unwrap();
        assert!(task.attempts.is_empty(), "nothing was ever dispatched");
        assert!(
            comet_proto::view::board::reviewable(&crate::rows::task_row(
                &task, None, &e.cfg, None, None
            )),
            "the row has a door to go through"
        );

        let review = e.review(&pr.task_id(), None).unwrap();
        assert!(review.undispatched());
        assert_eq!(review.diff, crate::claims::DiffSource::PullRequest);
        assert_eq!(review.changed.len(), 2, "the diff is GitHub's");
        assert_eq!(
            review.remainder.unclaimed.len(),
            2,
            "with no claims, every changed file is unaccounted for"
        );
        assert_eq!(
            review.branch.as_deref(),
            Some("codex/restore-green-main"),
            "the head ref is the poll's, and the only record of it"
        );
        assert_eq!(review.brief.title, "fix: restore approval lifecycle and CI");
        assert!(review.verdict().text.contains("nothing dispatched"));
    }

    /// GitHub being unreachable takes the diff away, not the review: the brief,
    /// the pull request and "nobody claimed anything" are still worth reading,
    /// and the reason is on the screen rather than in a log (§gh#344).
    #[test]
    fn a_diff_github_will_not_answer_for_leaves_the_review_standing() {
        let e = engine_with(
            Some(Github::new(
                Box::new(FixtureRest::new(vec![])) as Box<dyn Rest>
            )),
        );
        let pr = PullRequest {
            repo: "o/r".into(),
            number: 8,
            title: "Opened by hand".into(),
            body: None,
            url: "https://github.com/o/r/pull/8".into(),
            head_ref: "hand-written".into(),
            base_ref: "main".into(),
            head_repo: None,
            base_sha: None,
            open: true,
            merged: false,
            draft: false,
            stack: None,
            updated_at: crate::db::now(),
        };
        e.db.upsert_task(&pr.to_upsert()).unwrap();
        e.db.set_pr(&pr.task_id(), Some(&pr.url), Some(pr.number), true)
            .unwrap();

        let review = e.review(&pr.task_id(), None).unwrap();
        let crate::claims::DiffSource::Unavailable { reason } = &review.diff else {
            panic!("an unanswerable diff is unknown, never empty: {review:?}");
        };
        assert!(reason.contains("o/r#8"), "{reason}");
        assert_eq!(review.brief.title, "Opened by hand");
        assert!(review.undispatched());
    }

    /// A row with neither an attempt nor a pull request has nothing to review,
    /// and answering with an empty screen would be inventing one.
    #[test]
    fn a_row_that_never_ran_and_never_pushed_has_no_review() {
        let e = engine();
        seed(&e, "gh:o/r#4", "gh#4", UpstreamState::Unstarted);
        let err = e.review("gh:o/r#4", None).unwrap_err().to_string();
        assert!(err.contains("nothing to review"), "{err}");
    }

    #[test]
    fn a_pull_request_never_crosses_into_another_repo() {
        // herdr-board AGE-20, seen live: gh#2 exists in two repos, both branch
        // to `board/gh-2`, and one repo's *merged* PR attached itself to the
        // other's task — deriving it to review with no work done.
        let e = engine();
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
            base_ref: "main".into(),
            head_repo: None,
            base_sha: None,
            open: false,
            merged: true,
            draft: false,
            stack: None,
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
            base_ref: "main".into(),
            head_repo: None,
            base_sha: None,
            open: false,
            merged: true,
            draft: false,
            stack: None,
            updated_at: crate::db::now(),
        }
    }

    #[test]
    fn a_pull_request_merged_outside_the_board_still_closes_the_ticket() {
        // `gh pr merge` and the web UI go nowhere near the board, so nothing
        // told the tracker and tickets sat at In Review until someone closed
        // them by hand (herdr-board AGE-22).
        let e = engine();
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
        let e = engine();
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
        let e = engine();
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
        let e = engine();
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

    /// An engine polling one GitHub repo through a fixture.
    fn gh_poll_engine(routes: Vec<(String, Value)>) -> TestEngine {
        let mut e = engine_with(Some(Github::new(
            Box::new(FixtureRest::new(routes)) as Box<dyn Rest>,
        )));
        e.cfg.github.repos = vec!["o/r".into()];
        e
    }

    /// Routes answering an empty repo: no issues, no pull requests.
    fn empty_repo_routes() -> Vec<(String, Value)> {
        vec![
            ("/repos/o/r/issues".into(), json!([])),
            ("/repos/o/r/pulls".into(), json!([])),
        ]
    }

    #[test]
    fn an_outage_serves_stale_data_and_marks_the_header() {
        // No fixture routes at all: every call fails.
        let e = gh_poll_engine(vec![]);
        seed_gh(&e);

        e.poll_github();

        // The row is still there — never blank the list because a poll failed.
        assert_eq!(e.db.load_tasks().unwrap().len(), 1);
        match e.health(Source::Github) {
            SourceHealth::Down { error, retry_in } => {
                assert!(error.contains("no fixture"));
                assert!(retry_in > 0 && retry_in <= 300);
            }
            other => panic!("expected Down, got {other:?}"),
        }
    }

    #[test]
    fn backoff_grows_across_consecutive_failures_and_caps() {
        let e = gh_poll_engine(vec![]);
        e.poll_github();
        let first = match e.health(Source::Github) {
            SourceHealth::Down { retry_in, .. } => retry_in,
            _ => unreachable!(),
        };
        for _ in 0..10 {
            e.poll_github();
        }
        let later = match e.health(Source::Github) {
            SourceHealth::Down { retry_in, .. } => retry_in,
            _ => unreachable!(),
        };
        assert!(later > first);
        assert_eq!(later, 300, "backoff must cap at 5 minutes");
    }

    #[test]
    fn recovery_clears_the_header_and_the_failure_count() {
        let e = gh_poll_engine(empty_repo_routes());
        e.db.meta_set(meta::GITHUB_STATUS, "error:earlier").unwrap();
        e.db.meta_set(meta::GITHUB_FAILURES, "4").unwrap();
        e.poll_github();
        assert_eq!(e.health(Source::Github), SourceHealth::Ok);
    }

    #[test]
    fn an_unconfigured_source_is_absent_not_down() {
        // The header must not render `gh ✗` for a source nobody configured.
        let e = engine();
        assert_eq!(e.health(Source::Github), SourceHealth::Absent);
    }

    #[test]
    fn a_reader_reports_health_the_loop_recorded() {
        // A reader builds no GitHub client of its own when it starts before
        // the token exists, but it must still show `gh ✓` once the engine's
        // loop is polling successfully.
        let e = engine();
        assert_eq!(e.health(Source::Github), SourceHealth::Absent);
        e.db.meta_set(meta::GITHUB_STATUS, "ok").unwrap();
        assert_eq!(e.health(Source::Github), SourceHealth::Ok);
    }

    /// gh#471: Linear is no longer polled, so it has no health on any board —
    /// and a stale status row a pre-removal engine recorded must not
    /// resurrect a `linear ✓` in the header.
    #[test]
    fn linear_is_always_absent_from_the_header() {
        let e = engine();
        assert_eq!(e.health(Source::Linear), SourceHealth::Absent);
        e.db.meta_set("linear_status", "ok").unwrap();
        assert_eq!(e.health(Source::Linear), SourceHealth::Absent);
    }

    // ---- reaping ---------------------------------------------------------

    #[test]
    fn a_poll_removes_a_task_deleted_upstream() {
        let e = gh_poll_engine(empty_repo_routes());
        seed_gh(&e);
        assert_eq!(e.db.load_tasks().unwrap().len(), 1);

        e.poll_github();
        assert!(
            e.db.load_tasks().unwrap().is_empty(),
            "a task the source no longer returns must not linger forever"
        );
    }

    #[test]
    fn a_poll_never_removes_a_task_with_a_running_agent() {
        let e = gh_poll_engine(empty_repo_routes());
        seed_gh(&e);
        dispatch(&e, "gh:o/r#87", "chat-9");

        e.poll_github();
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
    fn a_poll_keeps_the_attempts_of_a_task_that_was_worked_on() {
        let e = gh_poll_engine(empty_repo_routes());
        seed_gh(&e);
        let a = dispatch(&e, "gh:o/r#87", "chat-9");
        e.db.conn
            .execute(
                "UPDATE attempts SET worktree = '/wt/gh-87-1' WHERE id = ?1",
                rusqlite::params![a],
            )
            .unwrap();
        // Closed, so the task is reapable at all — a live attempt is protected
        // by a rule of its own.
        e.db.close_attempt(a, Outcome::Done).unwrap();

        e.poll_github();

        let t = e.db.get_task("gh:o/r#87").unwrap().expect("row kept");
        assert_eq!(t.upstream, UpstreamState::Gone);
        assert_eq!(t.attempts.len(), 1, "the history is the point");
        assert_eq!(t.attempts[0].worktree.as_deref(), Some("/wt/gh-87-1"));
        // And it derives out of the queue, into `done`.
        e.rederive_all().unwrap();
        assert_eq!(
            e.db.get_task("gh:o/r#87").unwrap().unwrap().state,
            BoardState::Done
        );
    }

    #[test]
    fn a_poll_still_forgets_a_task_nobody_ever_dispatched() {
        // The noise case: created, mislabelled or deleted again, never worked
        // on. Nothing to keep.
        let e = gh_poll_engine(empty_repo_routes());
        seed_gh(&e);

        e.poll_github();
        assert!(e.db.load_tasks().unwrap().is_empty());
    }

    #[test]
    fn a_reaped_row_is_not_reported_again_on_the_next_poll() {
        let e = gh_poll_engine(empty_repo_routes());
        seed_gh(&e);
        let a = dispatch(&e, "gh:o/r#87", "chat-9");
        e.db.close_attempt(a, Outcome::Done).unwrap();

        e.poll_github();
        e.poll_github();
        assert!(
            e.db.reapable_task_ids(Source::Github).unwrap().is_empty(),
            "a gone row has nothing left to reap"
        );
    }

    #[test]
    fn a_failed_poll_does_not_reap() {
        // A failed poll is not the whole set, so absence proves nothing: the
        // issues call answers but the pulls call dies, and the row stays.
        let e = gh_poll_engine(vec![("/repos/o/r/issues".into(), json!([]))]);
        seed_gh(&e);

        e.poll_github();
        assert_eq!(e.db.load_tasks().unwrap().len(), 1, "nothing reaped");
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
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
    }

    /// An engine with GitHub writeback enabled — it is off by default, so a
    /// test about writeback has to ask for it.
    fn engine_with_gh_writeback() -> TestEngine {
        let mut e = engine_with(Some(gh_client()));
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
        let e = engine_with(Some(gh_client()));
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
        let e = engine_with(Some(gh_client()));
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

    /// gh#563's other half: a review delivered into the agent's chat minutes
    /// before the keypress is exactly the one whose answer can land on a branch
    /// this merge has just closed. Whoever pressed the key hears it now, not in
    /// a settle notice that may be hours away — and hears that the board will
    /// reopen such an answer rather than strand it.
    #[test]
    fn a_merge_right_after_a_delivered_review_warns_that_an_answer_may_follow() {
        let e = engine_with(Some(gh_client()));
        seed_gh(&e);
        e.db.set_pr(
            "gh:o/r#87",
            Some("https://github.com/o/r/pull/87"),
            Some(87),
            true,
        )
        .unwrap();
        let fresh = crate::review::Delivered {
            last_delivery: Some(crate::db::now()),
            ..Default::default()
        };
        e.db.meta_set(
            &meta::reviews_for("gh:o/r#87"),
            &serde_json::to_string(&fresh).unwrap(),
        )
        .unwrap();

        let task = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        let said = e.merge_pull_request(&task).unwrap();
        assert!(
            said.contains("a review reached the agent's chat just before this merge")
                && said.contains("reopen them as a new pull request"),
            "{said}"
        );
    }

    /// And the warning is for the window, not for history: a delivery from an
    /// hour ago names no answer still coming, and the reply stays one sentence.
    #[test]
    fn a_merge_long_after_a_delivery_warns_about_nothing() {
        let e = engine_with(Some(gh_client()));
        seed_gh(&e);
        e.db.set_pr(
            "gh:o/r#87",
            Some("https://github.com/o/r/pull/87"),
            Some(87),
            true,
        )
        .unwrap();
        let stale = crate::review::Delivered {
            last_delivery: Some((chrono::Utc::now() - chrono::Duration::minutes(30)).to_rfc3339()),
            ..Default::default()
        };
        e.db.meta_set(
            &meta::reviews_for("gh:o/r#87"),
            &serde_json::to_string(&stale).unwrap(),
        )
        .unwrap();

        let task = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        assert_eq!(e.merge_pull_request(&task).unwrap(), "o/r#87 merged");
    }

    /// The merge queue accepted the stack; nothing has landed yet. A row marked
    /// done here would be the board claiming a merge the queue can still reject
    /// (gh#290) — so it stays in review, and the poll that sees the merge is
    /// what moves it, exactly as for a merge made on the web.
    #[test]
    fn a_queued_merge_leaves_the_row_where_it_is() {
        let e = engine_with(
            Some(Github::new(Box::new(FixtureRest::new(vec![(
                "PUT /repos/o/r/pulls/87/merge-async".into(),
                json!({ "status": "enqueued" }),
            )])) as Box<dyn Rest>)),
        );
        seed_gh(&e);
        e.db.set_pr(
            "gh:o/r#87",
            Some("https://github.com/o/r/pull/87"),
            Some(87),
            true,
        )
        .unwrap();
        e.rederive_all().unwrap();

        let task = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        let said = e.merge_pull_request(&task).unwrap();
        assert!(said.contains("merge queue"), "{said}");

        let after = e.db.get_task("gh:o/r#87").unwrap().unwrap();
        assert!(after.pr_open, "the pull request has not merged");
        assert!(!after.pr_merged);
        assert_eq!(after.state, BoardState::Review);
        assert_eq!(
            e.db.pending_writeback_count().unwrap(),
            0,
            "and nothing was written back about work that has not landed",
        );
    }

    #[test]
    fn a_github_outcome_leaves_the_same_trail_as_linear() {
        let e = engine_with(Some(gh_client()));
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
        let e = engine_with(Some(gh_client()));
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
        let e = engine_with(Some(gh_client()));
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
    fn engine_with_one_read_only_repo() -> TestEngine {
        let mut e = engine_with(Some(gh_client()));
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
        let mut e = engine_with(Some(gh_client()));
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

    // ---- legacy Linear rows (gh#471) -------------------------------------

    /// A legacy linear-sourced row with an open pull request — the board
    /// derives `review` for it exactly as it always did.
    fn seed_legacy_in_review(e: &SyncEngine) {
        seed(e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        e.db.set_pr(
            "linear:LIN-142",
            Some("https://github.com/o/r/pull/291"),
            Some(291),
            true,
        )
        .unwrap();
    }

    /// The history half of the policy: a linear-sourced row written before the
    /// connector was removed still loads, still derives, and still renders —
    /// it just talks to nobody. Reaching `review` queues nothing.
    #[test]
    fn a_legacy_linear_row_still_derives_and_queues_nothing() {
        let e = engine();
        seed_legacy_in_review(&e);
        e.rederive_all().unwrap();

        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Review
        );
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
    }

    /// The queue half: an existing database may still hold writebacks aimed at
    /// Linear from before the removal. They leave the queue as dropped — not
    /// retried, which would back off forever against a source the board can no
    /// longer reach, and not stuck, which would wedge the drain.
    #[test]
    fn a_queued_legacy_linear_writeback_is_dropped_not_retried() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        e.db.enqueue_writeback(&NewWriteback {
            task_id: "linear:LIN-142".into(),
            kind: "dispatch".into(),
            payload: "{}".into(),
            idem_key: "linear:LIN-142:dispatch:1".into(),
        })
        .unwrap();

        let w = e.db.pending_writebacks(10).unwrap().remove(0);
        match e.deliver(&w).unwrap() {
            Sent::Dropped(why) => assert!(why.contains("no longer a connected source")),
            other => panic!("expected Dropped, got {other:?}"),
        }
        e.drain_writebacks();
        assert_eq!(e.db.pending_writeback_count().unwrap(), 0);
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
        let e = engine();
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
            upstream: UpstreamState::Unstarted,
            updated_at: crate::db::now(),
        })
        .unwrap();
        let t = e.db.get_task("gh:owner/repo!508").unwrap().unwrap();
        assert_eq!(route_context(&t).gh_repo.as_deref(), Some("owner/repo"));
    }

    /// gh#471: a legacy Linear row names no repo, so its context carries only
    /// its labels — a `gh_repo` route can never match it, a label route or a
    /// catch-all still can.
    #[test]
    fn route_context_of_a_legacy_linear_row_carries_no_repo() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let t = e.db.get_task("linear:LIN-142").unwrap().unwrap();
        let ctx = route_context(&t);
        assert_eq!(ctx.gh_repo, None);
        assert_eq!(ctx.labels, vec!["herd"]);
    }

    #[test]
    fn route_context_derives_repo_from_a_github_id() {
        let e = engine();
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
    ///
    /// The `TempDir` owns both checkouts — keep it alive as long as the paths
    /// are in use.
    fn repo_ahead_of_its_remote() -> (tempfile::TempDir, std::path::PathBuf) {
        use std::process::Command;
        let root = tempfile::Builder::new()
            .prefix("cb-ahead-")
            .tempdir()
            .unwrap();
        let remote = root.path().join("remote.git");
        let work = root.path().join("work");
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
        (root, work)
    }

    #[test]
    fn a_retry_does_not_inherit_the_cancelled_runs_commits() {
        // herdr-board#10: cancelled after four commits, re-dispatched onto the
        // same branch, marked `done` 62 seconds later while its agent was still
        // working. The base recorded before dispatch is the *repo* HEAD, and
        // the reused branch was already four commits ahead of it.
        let (_repo, work) = repo_ahead_of_its_remote();
        let e = engine();
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
    }

    #[test]
    fn an_operators_unpushed_commit_is_not_the_agents_output() {
        let (_repo, work) = repo_ahead_of_its_remote();
        let e = engine();
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
    }

    #[test]
    fn without_a_base_the_old_remote_relative_count_is_what_misfires() {
        // Pins the reason base_sha exists: the fallback path reports "the agent
        // produced something" for a repo where nothing has been dispatched at
        // all. Attempts predating the column keep this weaker behaviour, so it
        // is documented rather than fixed.
        let (_repo, work) = repo_ahead_of_its_remote();
        let e = engine();
        assert!(
            e.attempt_has_commits(Some(&work.to_string_lossy()), None),
            "the remote-relative count is fooled by unpushed work — this is the bug"
        );
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
        let e = engine();
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
        let e = engine();
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
        let e = engine();
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
        let e = engine();
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
        let mut e = engine();
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
        let e = engine();
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
        let e = engine();
        let a = spent_attempt(&e);
        e.collect_worktrees(None);
        age_mark(&e, a, 30 * 86_400);
        e.collect_worktrees(None);
        assert!(attempt_row(&e, a).collectable_at.is_some());
        assert!(attempt_row(&e, a).collected_at.is_none());
    }

    // ---- sweeping build output (gh#186) ----------------------------------

    /// A runtime that records which checkouts the sweep asked it to clear, and
    /// can be told to report a directory it could not delete.
    #[derive(Default)]
    struct Builder {
        swept: std::cell::RefCell<Vec<String>>,
        /// Bytes each sweep reports having freed. Zero is "nothing was built in
        /// there", which is the quiet majority of attempts.
        bytes: u64,
        stuck: bool,
    }

    impl Runtime for Builder {
        fn dispatch(&self, _: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
            unreachable!("the sweep never dispatches")
        }
        fn prompt(&self, _: &str, _: &str) -> anyhow::Result<()> {
            unreachable!("the sweep never talks to a chat")
        }
        fn cancel(&self, _: &str) -> anyhow::Result<()> {
            unreachable!("the sweep never cancels")
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
        fn reclaim_worktree(
            &self,
            _: Option<&str>,
            _: &str,
            _: Option<&str>,
        ) -> anyhow::Result<()> {
            unreachable!("a cache sweep never removes a worktree — that is the point")
        }
        fn reclaim_build_output(&self, worktree: &str) -> anyhow::Result<gc::Swept> {
            self.swept.borrow_mut().push(worktree.to_string());
            Ok(gc::Swept {
                dirs: if self.bytes > 0 { 1 } else { 0 },
                bytes: self.bytes,
                failed: if self.stuck {
                    vec![format!("{worktree}/target: permission denied")]
                } else {
                    Vec::new()
                },
            })
        }
    }

    /// A closed attempt with a checkout, on a task still very much on the board:
    /// its issue is open and its pull request is in review. Everything gh#72 does
    /// holds this checkout — and nothing holds the build output inside it.
    fn built_attempt(e: &SyncEngine) -> i64 {
        seed(e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(e, "linear:LIN-142", "chat-9");
        e.db.set_attempt_worktree(a, "/wt/board-lin-142").unwrap();
        e.db.conn
            .execute(
                "UPDATE tasks SET pr_open = 1 WHERE id = 'linear:LIN-142'",
                [],
            )
            .unwrap();
        e.db.close_attempt(a, Outcome::Done).unwrap();
        a
    }

    /// Move the sweep clock back, as wall time would have.
    fn age_sweep_mark(e: &SyncEngine, attempt_id: i64, secs: i64) {
        let stamp = crate::db::rfc3339(chrono::Utc::now() - chrono::Duration::seconds(secs));
        e.db.conn
            .execute(
                "UPDATE attempts SET cache_sweepable_at = ?2 WHERE id = ?1",
                rusqlite::params![attempt_id, stamp],
            )
            .unwrap();
    }

    /// gh#186 in one test. The same attempt, on the same cycle: its checkout is
    /// held for review (14 MB, and review delivery resumes an agent in it) and
    /// its build output is swept (36 GB, and nothing reads it).
    #[test]
    fn build_output_goes_when_the_attempt_ends_and_the_checkout_stays() {
        let e = engine();
        let a = built_attempt(&e);
        let build = Builder {
            bytes: 36 * 1024 * 1024 * 1024,
            ..Default::default()
        };
        let checkouts = Collector::default();

        // The first sweep marks — the same one-cycle mark the shelf takes under
        // `on-settle`, and what `Unmark` reverses if a build starts again.
        e.sweep_build_output(Some(&build));
        assert!(attempt_row(&e, a).cache_sweepable_at.is_some());
        assert!(build.swept.borrow().is_empty());

        // The next takes it, with no window to wait out.
        e.sweep_build_output(Some(&build));
        assert_eq!(build.swept.borrow().as_slice(), ["/wt/board-lin-142"]);

        let row = attempt_row(&e, a);
        assert!(row.cache_swept_at.is_some());
        // The three columns that must not have moved. A sweep is not a
        // collection: the checkout is still there, still on its branch, still
        // the directory review delivery would resume an agent in — and the
        // board must still be able to reclaim it when its own week is up.
        assert!(row.collected_at.is_none(), "the worktree is still there");
        assert!(
            row.collectable_at.is_none(),
            "its own clock has not started"
        );
        assert_eq!(row.worktree.as_deref(), Some("/wt/board-lin-142"));

        // The checkout sweep agrees: review holds it, whatever happened inside.
        e.collect_worktrees(Some(&checkouts));
        assert!(checkouts.reclaimed.borrow().is_empty());

        // And the cache is not offered twice — a box up for months does not
        // re-walk every source tree it ever cut, every cycle.
        e.sweep_build_output(Some(&build));
        assert_eq!(build.swept.borrow().len(), 1);
    }

    /// The checkout's own clock still runs, and still ends in a collection: the
    /// sweep must not have made the row look already reclaimed.
    #[test]
    fn a_swept_checkout_is_still_collected_when_its_own_window_runs_out() {
        let e = engine();
        let a = built_attempt(&e);
        let build = Builder {
            bytes: 1024,
            ..Default::default()
        };
        e.sweep_build_output(Some(&build));
        e.sweep_build_output(Some(&build));
        assert!(attempt_row(&e, a).cache_swept_at.is_some());

        // The pull request lands and the issue closes: now the checkout is
        // nobody's either, and a week later it goes as it always did.
        e.db.conn
            .execute(
                "UPDATE tasks SET pr_open = 0, upstream = 'terminal' \
                 WHERE id = 'linear:LIN-142'",
                [],
            )
            .unwrap();
        let checkouts = Collector::default();
        e.collect_worktrees(Some(&checkouts));
        age_mark(&e, a, 7 * 86_400);
        e.collect_worktrees(Some(&checkouts));
        assert_eq!(checkouts.reclaimed.borrow().len(), 1);
        assert!(attempt_row(&e, a).collected_at.is_some());
    }

    /// The one guard. Sweeping a live attempt's checkout would delete a running
    /// `cargo build`'s output from under it — and a retry reuses the branch, so
    /// the closed attempt's directory is very often the live one's.
    #[test]
    fn nothing_is_swept_while_anybody_is_building() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_attempt_worktree(a, "/wt/board-lin-142").unwrap();
        let build = Builder::default();
        for _ in 0..3 {
            // However old a mark left from before, a build in progress stops the
            // clock rather than running it out.
            age_sweep_mark(&e, a, 30 * 86_400);
            e.sweep_build_output(Some(&build));
            assert!(attempt_row(&e, a).cache_sweepable_at.is_none());
        }
        assert!(build.swept.borrow().is_empty());

        // Closed, then a retry dispatched into the same directory: the mark it
        // had taken is cleared, so the retry's own build gets a whole window.
        e.db.close_attempt(a, Outcome::Failed).unwrap();
        e.sweep_build_output(Some(&build));
        assert!(attempt_row(&e, a).cache_sweepable_at.is_some());
        dispatch(&e, "linear:LIN-142", "chat-10");
        e.sweep_build_output(Some(&build));
        assert!(build.swept.borrow().is_empty(), "a live retry holds it");
        assert!(attempt_row(&e, a).cache_sweepable_at.is_none());
    }

    /// A cache is the one thing here that comes back. An attempt re-opened to
    /// answer review comments builds again, so the stamp has to go with the
    /// re-open — or the 36 GB that build writes sits behind a row the sweep
    /// skips forever.
    #[test]
    fn a_reopened_attempt_has_its_next_build_swept_too() {
        let e = engine();
        let a = built_attempt(&e);
        let build = Builder {
            bytes: 1024,
            ..Default::default()
        };
        e.sweep_build_output(Some(&build));
        e.sweep_build_output(Some(&build));
        assert_eq!(build.swept.borrow().len(), 1);

        // The chat starts working again — the settle was wrong, and the agent is
        // building in there right now.
        e.reconcile_sessions(&statuses(&[("chat-9", AgentStatus::Working)]))
            .unwrap();
        let row = attempt_row(&e, a);
        assert!(row.outcome.is_none(), "back to work");
        assert!(row.cache_swept_at.is_none(), "and its cache is a live one");
        assert!(row.cache_sweepable_at.is_none());

        // Nothing while it runs; swept again once it ends.
        e.sweep_build_output(Some(&build));
        assert_eq!(build.swept.borrow().len(), 1, "it is building");
        e.db.close_attempt(a, Outcome::Done).unwrap();
        e.sweep_build_output(Some(&build));
        e.sweep_build_output(Some(&build));
        assert_eq!(build.swept.borrow().len(), 2);
    }

    /// A box with disk to spare can buy the rebuild back. Only the default is
    /// `on-settle`; the window is a setting like every other one here.
    #[test]
    fn a_window_on_the_cache_is_waited_out() {
        let mut e = engine();
        e.cfg.defaults.retain_build_output = "2h".into();
        let a = built_attempt(&e);
        let build = Builder::default();

        e.sweep_build_output(Some(&build));
        e.sweep_build_output(Some(&build));
        assert!(build.swept.borrow().is_empty(), "two hours is two hours");

        age_sweep_mark(&e, a, 2 * 3_600);
        e.sweep_build_output(Some(&build));
        assert_eq!(build.swept.borrow().len(), 1);
    }

    #[test]
    fn sweeping_off_keeps_every_cache() {
        let mut e = engine();
        e.cfg.defaults.retain_build_output = "off".into();
        let a = built_attempt(&e);
        let build = Builder::default();
        e.sweep_build_output(Some(&build));
        assert!(
            attempt_row(&e, a).cache_sweepable_at.is_none(),
            "the clock never starts"
        );
        // Not even one marked before the operator turned it off.
        age_sweep_mark(&e, a, 365 * 86_400);
        e.sweep_build_output(Some(&build));
        assert!(build.swept.borrow().is_empty());
    }

    /// The stamp is what says the disk space is back. A directory that would not
    /// delete leaves the row unstamped, so the next cycle tries again over what
    /// is left — reporting space it never reclaimed is the failure gh#186 is
    /// about, from the other end.
    #[test]
    fn a_cache_that_will_not_delete_is_tried_again_next_cycle() {
        let e = engine();
        let a = built_attempt(&e);
        let build = Builder {
            stuck: true,
            bytes: 1024,
            ..Default::default()
        };
        e.sweep_build_output(Some(&build));
        e.sweep_build_output(Some(&build));
        assert!(attempt_row(&e, a).cache_swept_at.is_none());
        e.sweep_build_output(Some(&build));
        assert_eq!(build.swept.borrow().len(), 2, "and again the cycle after");
    }

    #[test]
    fn a_cycle_without_a_runtime_marks_but_sweeps_nothing() {
        // Only the process that cut the worktrees may delete inside them;
        // anything else running the cycle keeps the clock.
        let e = engine();
        let a = built_attempt(&e);
        e.sweep_build_output(None);
        e.sweep_build_output(None);
        let row = attempt_row(&e, a);
        assert!(row.cache_sweepable_at.is_some());
        assert!(row.cache_swept_at.is_none());
    }

    /// gh#286 through the sweep: the parent's issue is closed and its pull
    /// request is merged, so its checkout is nobody's — except that the layer
    /// dispatched off its branch is still standing on it. Collecting deletes
    /// the local branch, which is the only remaining name for the history the
    /// child sits on, so the clock does not even start.
    #[test]
    fn a_stacked_child_defers_the_collection_of_the_parents_checkout() {
        let e = engine();
        let parent = spent_attempt(&e);
        seed(&e, "linear:LIN-143", "LIN-143", UpstreamState::Started);
        let child = dispatch(&e, "linear:LIN-143", "chat-10");
        e.db.conn
            .execute(
                "UPDATE attempts SET stacked_on = ?2 WHERE id = ?1",
                rusqlite::params![child, parent],
            )
            .unwrap();
        e.db.set_attempt_worktree(child, "/wt/board-lin-143")
            .unwrap();
        let gc = Collector::default();

        e.collect_worktrees(Some(&gc));
        assert!(
            attempt_row(&e, parent).collectable_at.is_none(),
            "held for the layer above, so the retention clock never starts",
        );
        // Even a mark left from before the child existed is cleared, and a
        // month of wall time collects nothing.
        age_mark(&e, parent, 30 * 86_400);
        e.collect_worktrees(Some(&gc));
        assert!(gc.reclaimed.borrow().is_empty());
        assert!(attempt_row(&e, parent).collectable_at.is_none());

        // The child's own checkout is reclaimed — by its own standing, on its
        // own clock — and the branch under it is free to go with the next
        // sweep, on the full window.
        e.db.set_attempt_collected(child).unwrap();
        e.collect_worktrees(Some(&gc));
        assert!(gc.reclaimed.borrow().is_empty(), "one cycle to mark");
        assert!(attempt_row(&e, parent).collectable_at.is_some());
        age_mark(&e, parent, 7 * 86_400);
        e.collect_worktrees(Some(&gc));
        assert_eq!(gc.reclaimed.borrow().len(), 1);
    }
    // ---- clearing the shelf (gh#139) -------------------------------------

    /// A runtime that records what the shelf sweep asked it to archive, and
    /// can be told to refuse — the one thing this sweep does to the world
    /// outside the database.
    #[derive(Default)]
    struct Shelf {
        archived: std::cell::RefCell<Vec<(String, bool)>>,
        refuse: bool,
    }

    impl Runtime for Shelf {
        fn dispatch(&self, _: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
            unreachable!("the shelf sweep never dispatches")
        }
        fn prompt(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn cancel(&self, _: &str) -> anyhow::Result<()> {
            unreachable!("archiving is not cancelling — nothing is interrupted")
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
        fn set_chat_archived(&self, chat_id: &str, archived: bool) -> anyhow::Result<()> {
            self.archived
                .borrow_mut()
                .push((chat_id.to_string(), archived));
            if self.refuse {
                anyhow::bail!("the workspace doc said no");
            }
            Ok(())
        }
    }

    /// A closed attempt with a chat, on a task whose issue is closed upstream.
    fn spent_chat(e: &SyncEngine) -> i64 {
        seed(e, "linear:LIN-142", "LIN-142", UpstreamState::Terminal);
        let a = dispatch(e, "linear:LIN-142", "chat-9");
        e.db.close_attempt(a, Outcome::Done).unwrap();
        a
    }

    /// Move the shelf clock back, as wall time would have.
    fn age_shelf_mark(e: &SyncEngine, attempt_id: i64, secs: i64) {
        let stamp = crate::db::rfc3339(chrono::Utc::now() - chrono::Duration::seconds(secs));
        e.db.conn
            .execute(
                "UPDATE attempts SET chat_archivable_at = ?2 WHERE id = ?1",
                rusqlite::params![attempt_id, stamp],
            )
            .unwrap();
    }

    #[test]
    fn a_settled_attempts_chat_is_marked_and_archived_on_the_next_sweep() {
        let e = engine();
        let a = spent_chat(&e);
        let shelf = Shelf::default();

        // The sweep that finds it finished stamps when it became archivable —
        // the mark is still taken, because it is what `Unmark` reverses when a
        // task comes back to life. It just is not waited on.
        e.archive_chats(Some(&shelf));
        let row = attempt_row(&e, a);
        assert!(row.chat_archivable_at.is_some(), "the mark is still taken");
        assert!(row.chat_archived_at.is_none());
        assert!(
            shelf.archived.borrow().is_empty(),
            "the sweep that marks does not also collect"
        );

        // The next one takes it — no window to wait out.
        e.archive_chats(Some(&shelf));
        assert_eq!(
            shelf.archived.borrow().as_slice(),
            [("chat-9".to_string(), true)]
        );
        assert!(attempt_row(&e, a).chat_archived_at.is_some());

        // And it is not offered again — the row says where it went.
        e.archive_chats(Some(&shelf));
        assert_eq!(shelf.archived.borrow().len(), 1);
    }

    /// A space that wants a grace period still gets one: the window is a
    /// setting, and only its default changed.
    #[test]
    fn a_route_that_asks_for_a_window_still_waits_it_out() {
        let mut e = engine();
        e.cfg.defaults.archive_chats = "2d".into();
        let a = spent_chat(&e);
        let shelf = Shelf::default();

        e.archive_chats(Some(&shelf));
        e.archive_chats(Some(&shelf));
        assert!(shelf.archived.borrow().is_empty(), "two days is two days");

        age_shelf_mark(&e, a, 2 * 86_400);
        e.archive_chats(Some(&shelf));
        assert_eq!(shelf.archived.borrow().len(), 1);
    }

    /// The exit criterion's sharpest edge: review delivery calls `chat_alive`
    /// on this chat, and an archived one answers no. Archiving here would break
    /// the delivery loop from under itself.
    #[test]
    fn a_chat_in_review_is_never_archived_out_from_under_its_delivery_loop() {
        let e = engine();
        let a = spent_chat(&e);
        let shelf = Shelf::default();
        e.archive_chats(Some(&shelf));
        assert!(attempt_row(&e, a).chat_archivable_at.is_some());

        e.db.conn
            .execute(
                "UPDATE tasks SET pr_open = 1 WHERE id = 'linear:LIN-142'",
                [],
            )
            .unwrap();
        age_shelf_mark(&e, a, 30 * 86_400);
        e.archive_chats(Some(&shelf));
        assert!(shelf.archived.borrow().is_empty(), "review holds the chat");
        assert!(
            attempt_row(&e, a).chat_archivable_at.is_none(),
            "and the window starts over when the pull request lands"
        );
    }

    /// An agent that stopped to ask at 02:00 is the single worst chat to file
    /// away: its attempt is still open, so the sweep never considers it.
    #[test]
    fn a_live_or_blocked_attempt_keeps_its_chat_however_finished_the_task_looks() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Terminal);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_attempt_status(a, AgentStatus::Blocked).unwrap();
        let shelf = Shelf::default();
        // However long the board has been up, an open attempt's chat is not a
        // candidate at all: its clock never starts, so it can never run out.
        for _ in 0..3 {
            e.archive_chats(Some(&shelf));
        }
        assert!(shelf.archived.borrow().is_empty());
        assert!(attempt_row(&e, a).chat_archivable_at.is_none());
    }

    /// The orchestrator hears about every settle on the board, so it is never
    /// finished — even though it was dispatched as an attempt like any other.
    #[test]
    fn the_pinned_orchestrator_is_never_archived() {
        let mut e = engine();
        e.cfg.defaults.orchestrator_chat = Some("chat-9".into());
        let a = spent_chat(&e);
        let shelf = Shelf::default();
        e.archive_chats(Some(&shelf));
        assert!(
            attempt_row(&e, a).chat_archivable_at.is_none(),
            "the clock never starts on the board's own agent"
        );
        age_shelf_mark(&e, a, 365 * 86_400);
        e.archive_chats(Some(&shelf));
        assert!(shelf.archived.borrow().is_empty());
        assert!(
            attempt_row(&e, a).chat_archivable_at.is_none(),
            "and a mark from before it was pinned is cleared"
        );
    }

    /// gh#354, end to end through the sweep, replayed against the report it
    /// came from: "I dispatched two agents through another pane; when the PRs
    /// were merged, the thread that I was working from disappeared."
    ///
    /// The hold has to survive the children *settling*, which is the moment the
    /// pull requests open and the notices land in this chat — not the moment
    /// the work is over. With `archive_chats` at its `on-settle` default there
    /// is no window to absorb the difference: a hold that ended at settle would
    /// take the thread one sweep earlier than he described rather than fixing
    /// anything.
    #[test]
    fn a_chat_that_released_unfinished_work_is_never_archived() {
        let e = engine();
        // The pane he dispatched from: a settled attempt on a closed issue,
        // which is every bit of what the sweep used to need.
        let dispatcher = spent_chat(&e);
        seed(&e, "linear:LIN-143", "LIN-143", UpstreamState::Started);
        let child = dispatch_via(&e, "linear:LIN-143", "chat-10", "chat-9");
        let shelf = Shelf::default();

        for _ in 0..3 {
            e.archive_chats(Some(&shelf));
        }
        assert!(
            shelf.archived.borrow().is_empty(),
            "the thread he is working in is not a finished attempt"
        );
        assert!(
            attempt_row(&e, dispatcher).chat_archivable_at.is_none(),
            "and the clock never starts while it is waiting on an agent"
        );

        // The agent finishes and opens its pull request. The task is still on
        // the board — this is the interval he reads the diff and merges in, and
        // the notice that starts it was delivered into this very chat.
        e.db.close_attempt(child, Outcome::Done).unwrap();
        for _ in 0..3 {
            e.archive_chats(Some(&shelf));
        }
        assert!(
            shelf.archived.borrow().is_empty(),
            "a settled child is not a finished one — this is the merge window"
        );
        assert!(attempt_row(&e, dispatcher).chat_archivable_at.is_none());

        // The pull request merges and the issue closes. Now nothing is owed,
        // and the shelf clears on the ordinary terms.
        seed(&e, "linear:LIN-143", "LIN-143", UpstreamState::Terminal);
        e.archive_chats(Some(&shelf));
        assert!(attempt_row(&e, dispatcher).chat_archivable_at.is_some());
        e.archive_chats(Some(&shelf));
        // Both of them: the child's chat is spent for the ordinary reason and
        // the dispatcher's is spent because the child is. A family finishes
        // together, which is the whole of what the shelf sweep is for.
        let mut archived = shelf.archived.borrow().clone();
        archived.sort();
        assert_eq!(
            archived,
            [("chat-10".to_string(), true), ("chat-9".to_string(), true)]
        );
    }

    /// A mark taken before the dispatch is reversed by it, the same way review
    /// reversal works: the sweep may well have found the parent finished in the
    /// minutes between its own settle and the dispatch it made afterwards.
    #[test]
    fn releasing_work_stops_a_shelf_clock_that_had_already_started() {
        let e = engine();
        let dispatcher = spent_chat(&e);
        let shelf = Shelf::default();
        e.archive_chats(Some(&shelf));
        assert!(attempt_row(&e, dispatcher).chat_archivable_at.is_some());

        seed(&e, "linear:LIN-143", "LIN-143", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-143", "chat-10", "chat-9");
        age_shelf_mark(&e, dispatcher, 365 * 86_400);
        e.archive_chats(Some(&shelf));
        assert!(shelf.archived.borrow().is_empty());
        assert!(
            attempt_row(&e, dispatcher).chat_archivable_at.is_none(),
            "the window starts over when the work it released comes back"
        );
    }

    /// The "why is this still here" line claims the hold that is actually
    /// keeping the chat. A pinned orchestrator that also dispatches was held by
    /// its pin before any of this existed, so the dispatcher note must not
    /// speak for it — the reader would go and look at the wrong fact.
    #[test]
    fn the_note_names_the_hold_that_is_doing_the_work() {
        let mut e = engine();
        let dispatcher = spent_chat(&e);
        seed(&e, "linear:LIN-143", "LIN-143", UpstreamState::Started);
        dispatch_via(&e, "linear:LIN-143", "chat-10", "chat-9");
        let shelf = Shelf::default();

        e.cfg.defaults.orchestrator_chat = Some("chat-9".into());
        e.archive_chats(Some(&shelf));
        assert!(
            matches!(e.db.meta_get(&meta::dispatcher_noted(dispatcher)), Ok(None)),
            "the pin was holding this chat with or without what it dispatched"
        );

        // Unpinned, the dispatch is the only thing keeping it, and says so.
        e.cfg.defaults.orchestrator_chat = None;
        e.archive_chats(Some(&shelf));
        assert!(matches!(
            e.db.meta_get(&meta::dispatcher_noted(dispatcher)),
            Ok(Some(_))
        ));
    }

    #[test]
    fn archiving_off_keeps_every_chat_on_the_shelf() {
        let mut e = engine();
        e.cfg.defaults.archive_chats = "off".into();
        let a = spent_chat(&e);
        let shelf = Shelf::default();
        e.archive_chats(Some(&shelf));
        assert!(
            attempt_row(&e, a).chat_archivable_at.is_none(),
            "the clock never starts"
        );
        // Not even one left over from before the operator turned it off.
        age_shelf_mark(&e, a, 365 * 86_400);
        e.archive_chats(Some(&shelf));
        assert!(shelf.archived.borrow().is_empty());
    }

    #[test]
    fn a_route_can_keep_its_own_chats_longer_than_the_board_does() {
        // The window is read per route at sweep time, not stamped at dispatch.
        let mut e = engine();
        e.cfg.defaults.archive_chats = "1d".into();
        e.cfg.routes = vec![
            toml::from_str(
                r#"
                match = { label = "herd" }
                workspace = "offhand"
                repo = "/tmp"
                runtime = "claude-code"
                archive_chats = "30d"
                "#,
            )
            .unwrap(),
        ];
        let a = spent_chat(&e);
        let shelf = Shelf::default();
        e.archive_chats(Some(&shelf));
        age_shelf_mark(&e, a, 7 * 86_400);
        e.archive_chats(Some(&shelf));
        assert!(
            shelf.archived.borrow().is_empty(),
            "a week is nothing to a route that asked for thirty days"
        );
        age_shelf_mark(&e, a, 30 * 86_400);
        e.archive_chats(Some(&shelf));
        assert_eq!(shelf.archived.borrow().len(), 1);
    }

    #[test]
    fn a_cycle_without_a_runtime_marks_but_never_archives() {
        // Only the process hosting the workspace doc may mutate it; anything
        // else running the cycle is welcome to keep the clock.
        let e = engine();
        let a = spent_chat(&e);
        e.archive_chats(None);
        age_shelf_mark(&e, a, 30 * 86_400);
        e.archive_chats(None);
        assert!(attempt_row(&e, a).chat_archivable_at.is_some());
        assert!(attempt_row(&e, a).chat_archived_at.is_none());
    }

    #[test]
    fn an_archive_that_fails_is_tried_again_next_cycle() {
        let e = engine();
        let a = spent_chat(&e);
        let shelf = Shelf {
            refuse: true,
            ..Default::default()
        };
        e.archive_chats(Some(&shelf));
        age_shelf_mark(&e, a, 30 * 86_400);
        e.archive_chats(Some(&shelf));
        assert!(
            attempt_row(&e, a).chat_archived_at.is_none(),
            "the stamp is what says the chat is off the shelf"
        );
        e.archive_chats(Some(&shelf));
        assert_eq!(
            shelf.archived.borrow().len(),
            2,
            "and again the cycle after"
        );
    }

    /// The one way an archived chat comes back by itself: the settle was wrong
    /// and the agent is working in it again (§settle-logic's inverse). Nobody
    /// should have to go to Settings → Archived to find the chat the board
    /// just re-opened.
    #[test]
    fn a_reopened_attempt_gets_its_chat_back_off_the_shelf() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_saw_working(a).unwrap();
        e.db.close_attempt(a, Outcome::Done).unwrap();
        // Archived while the task looked done, then the operator re-opened the
        // issue and the agent picked the work back up.
        e.db.set_attempt_chat_archivable(a, true).unwrap();
        e.db.set_attempt_chat_archived(a).unwrap();

        let shelf = Shelf::default();
        let working = statuses(&[("chat-9", AgentStatus::Working)]);
        assert!(
            e.rewatch_settled_attempts(&working, Some(&shelf)).unwrap(),
            "a working chat re-opens its settled attempt"
        );
        assert_eq!(
            shelf.archived.borrow().as_slice(),
            [("chat-9".to_string(), false)],
            "and the chat goes back on the shelf in the same motion"
        );
        let row = attempt_row(&e, a);
        assert!(row.chat_archived_at.is_none());
        assert!(
            row.chat_archivable_at.is_none(),
            "the next time it finishes it is owed a whole window"
        );
    }

    /// A chat the *operator* archived is theirs. The board only ever un-archives
    /// what its own record says it archived.
    #[test]
    fn a_reopen_does_not_argue_with_a_chat_somebody_archived_by_hand() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_saw_working(a).unwrap();
        e.db.close_attempt(a, Outcome::Done).unwrap();

        let shelf = Shelf::default();
        assert!(
            e.rewatch_settled_attempts(
                &statuses(&[("chat-9", AgentStatus::Working)]),
                Some(&shelf)
            )
            .unwrap()
        );
        assert!(shelf.archived.borrow().is_empty());
    }

    /// gh#139's exit criterion, end to end on one task: a week after it merged,
    /// its chat is archived and its checkout is collected — and both happened
    /// on the same sweep, from the same rule.
    #[test]
    fn a_task_finished_a_week_ago_leaves_neither_a_chat_nor_a_checkout() {
        let e = engine();
        let a = spent_attempt(&e);
        let gc = Collector::default();
        let shelf = Shelf::default();

        e.collect_worktrees(Some(&gc));
        e.archive_chats(Some(&shelf));
        age_mark(&e, a, 7 * 86_400);
        age_shelf_mark(&e, a, 7 * 86_400);
        e.collect_worktrees(Some(&gc));
        e.archive_chats(Some(&shelf));

        let row = attempt_row(&e, a);
        assert!(row.collected_at.is_some(), "the checkout is reclaimed");
        assert!(row.chat_archived_at.is_some(), "and the chat is filed away");
        assert_eq!(gc.reclaimed.borrow().len(), 1);
        assert_eq!(
            shelf.archived.borrow().as_slice(),
            [("chat-9".to_string(), true)]
        );
    }

    // ---- the full cycle --------------------------------------------------

    #[test]
    fn sync_once_without_a_snapshot_skips_reconciliation() {
        // The engine boots before its first session-watch snapshot arrives. A
        // missing snapshot must read as "no information", not "every chat is
        // missing" — the difference between skipping a tick and counting one
        // against every live attempt.
        let e = engine();
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
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        dispatch(&e, "linear:LIN-142", "chat-9");

        e.sync_once(Some(&statuses(&[("chat-9", AgentStatus::Blocked)])))
            .unwrap();
        assert_eq!(
            e.db.get_task("linear:LIN-142").unwrap().unwrap().state,
            BoardState::Blocked
        );
    }

    // ---- the review contract, against a real checkout (§gh#183) ----------

    /// A checkout with one commit on it, and an attempt whose base is that
    /// commit — the shape every dispatched agent starts from.
    ///
    /// The `TempDir` owns the checkout; dropping it is the cleanup (a test
    /// that removes `dir` itself is simulating gc, which the drop tolerates).
    fn checkout_with_a_base() -> (tempfile::TempDir, std::path::PathBuf, String) {
        let tmp = tempfile::Builder::new()
            .prefix("cb-claims-")
            .tempdir()
            .unwrap();
        let dir = tmp.path().to_path_buf();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/db.rs"), "// base\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "base"]);
        let base = git(&["rev-parse", "HEAD"]);
        (tmp, dir, base)
    }

    /// An attempt in `dir` measured from `base`, with a chat to read a journal
    /// for.
    fn attempt_in(e: &SyncEngine, dir: &std::path::Path, base: &str) -> i64 {
        seed(e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(e, "linear:LIN-142", "chat-9");
        e.db.set_attempt_worktree(a, &dir.to_string_lossy())
            .unwrap();
        e.db.set_attempt_base_sha(a, base).unwrap();
        a
    }

    fn commit_all(dir: &std::path::Path, message: &str) {
        for args in [
            ["add", "-A"].as_slice(),
            ["commit", "-m", message].as_slice(),
        ] {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        }
    }

    /// The whole ticket, end to end: an agent claims two files, the branch
    /// touched four, and the board — not the agent — names the other two.
    #[test]
    fn the_board_computes_what_the_agent_did_not_account_for() {
        let (_repo, dir, base) = checkout_with_a_base();
        let e = engine();
        attempt_in(&e, &dir, &base);

        std::fs::write(dir.join("src/db.rs"), "// base\n// claims column\n").unwrap();
        std::fs::write(dir.join("src/claims.rs"), "// the contract\n").unwrap();
        // Nobody is going to mention these two.
        std::fs::write(dir.join("Cargo.lock"), "# a dependency moved\n").unwrap();
        std::fs::write(dir.join("src/gc.rs"), "// edited in passing\n").unwrap();
        commit_all(&dir, "the work");

        let review = e
            .submit_claims(
                None,
                "linear:LIN-142",
                "Claims are stored against the attempt :: src/db.rs\n\
                 The format and the remainder :: src/claims.rs\n",
            )
            .unwrap();

        assert!(review.claimed(), "the contract was answered");
        assert_eq!(review.remainder.claims.len(), 2);
        assert_eq!(review.remainder.claimed, 2);
        assert_eq!(
            review
                .remainder
                .unclaimed
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["Cargo.lock", "src/gc.rs"],
            "the remainder is the product"
        );
        assert!(matches!(review.diff, crate::claims::DiffSource::Checkout));
        // And the line counts a reader needs to judge one at a glance.
        let lock = &review.remainder.unclaimed[0];
        assert_eq!(lock.status, "A");
        assert_eq!(lock.added, 1);
    }

    /// §gh#236, end to end: every chip on the row comes off the checkout and
    /// the journal, and the one thing that arrived is the one thing that is not
    /// neutral.
    ///
    /// The claims in here are deliberately *correct* — the point is not that
    /// the board catches a lie, it is that a true claim reads differently when
    /// something the agent did not write stands behind it.
    #[test]
    fn the_board_derives_the_effects_of_a_branch_without_asking_the_agent() {
        let (_repo, dir, _) = checkout_with_a_base();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub fn active_placements() -> usize { 0 }\n\
             fn a() { let _ = active_placements(); }\n\
             fn b() { let _ = active_placements(); }\n",
        )
        .unwrap();
        std::fs::write(dir.join("app.toml"), "[sync]\n").unwrap();
        commit_all(&dir, "a base with a dependency and a symbol");
        let base = String::from_utf8_lossy(
            &Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        let e = engine();
        let a = attempt_in(&e, &dir, &base);

        // The branch: a dependency arrives, the schema moves, a config key is
        // added, a test is written, and one of the two call sites goes.
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1.2\"\ntoml = \"1.1\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/db.rs"),
            "// base\nconst UP: &str = \"ALTER TABLE attempts ADD COLUMN effects TEXT\";\n",
        )
        .unwrap();
        std::fs::write(dir.join("app.toml"), "[sync]\ninterval = \"5m\"\n").unwrap();
        std::fs::write(
            dir.join("src/effects.rs"),
            "pub fn scan() {}\n#[test]\nfn it_scans() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "pub fn active_placements() -> usize { 0 }\n\
             fn a() { let _ = active_placements(); }\n\
             fn b() {}\n",
        )
        .unwrap();
        commit_all(&dir, "the work");

        // What the harness recorded while the agent worked — the only source
        // for "did the tests pass", and not one the agent wrote.
        e.db.set_attempt_evidence(
            a,
            &crate::evidence::gather(&[crate::evidence::RanCommand {
                command: "cargo test".into(),
                failed: false,
            }]),
        )
        .unwrap();

        let review = e
            .submit_claims(
                None,
                "linear:LIN-142",
                "The scan and its test :: src/effects.rs\n\
                 One call site went :: active_placements\n",
            )
            .unwrap();

        let effects = &review.effects;
        assert!(effects.read);
        assert_eq!(
            (effects.tests_before, effects.tests_after),
            (Some(0), Some(1)),
            "counted in both trees by the same rule"
        );
        assert_eq!(effects.deps_added, vec!["toml".to_string()]);
        assert!(effects.deps_known);
        assert!(matches!(
            effects.public_api(),
            crate::effects::Surface::Changed { .. }
        ));
        assert_eq!(
            effects.schema(),
            crate::effects::Surface::Changed { count: 1 },
            "a migration written in Rust is still a migration"
        );
        assert_eq!(
            effects.config_keys(),
            crate::effects::Surface::Changed { count: 1 }
        );

        let chips = review.effect_chips();
        assert_eq!(chips[0].text, "Tests 0 → 1, all passing");
        assert_eq!(chips[0].ground, crate::effects::Ground::Neutral);
        assert_eq!(chips.last().unwrap().text, "1 dependency added");
        assert_eq!(
            chips.last().unwrap().ground,
            crate::effects::Ground::Working,
            "a new dependency is the one chip on this row that is not neutral"
        );

        // The claim with a passing new test behind it is corroborated; the one
        // about a symbol carries the count from both trees.
        let tested = &review.remainder.claims[0];
        assert_eq!(review.claim_chips(tested)[0].text, "1 new test passes");
        assert_eq!(
            review.claim_mark(tested),
            crate::claims::ClaimMark::Corroborated
        );
        let moved = &review.remainder.claims[1];
        let chips = review.claim_chips(moved);
        assert_eq!(chips[0].text, "1 call site, was 2");
        assert!(
            chips.iter().any(|c| c.text == "no test covers this"),
            "\"somebody calls it\" and \"something checks it\" are different news"
        );
        // Still a tick: a count taken in both trees is evidence the agent did
        // not author, which is the whole bar this glyph is set at.
        assert_eq!(
            review.claim_mark(moved),
            crate::claims::ClaimMark::Corroborated
        );
    }

    /// The one way an empty remainder could mislead: an agent that claims
    /// before it commits has shown the board nothing, and "all accounted for"
    /// would be the friendliest possible lie.
    #[test]
    fn work_that_is_not_committed_yet_is_reported_beside_the_remainder() {
        let (_repo, dir, base) = checkout_with_a_base();
        let e = engine();
        attempt_in(&e, &dir, &base);
        std::fs::write(dir.join("src/db.rs"), "// written, not committed\n").unwrap();
        std::fs::write(dir.join("src/new.rs"), "// not even added\n").unwrap();

        let review = e
            .submit_claims(None, "linear:LIN-142", "Storage :: src/db.rs")
            .unwrap();
        assert!(review.changed.is_empty(), "nothing is on the branch yet");
        assert!(review.remainder.complete(), "vacuously — hence the flag");
        assert_eq!(
            review.uncommitted,
            Some(2),
            "the untracked file counts too: it has been shown to nobody either"
        );

        // Committing turns it into a diff, and the flag goes.
        commit_all(&dir, "the work");
        let review = e.review("linear:LIN-142", None).unwrap();
        assert_eq!(review.changed.len(), 2);
        assert_eq!(review.uncommitted, Some(0));
        assert_eq!(review.remainder.unclaimed[0].path, "src/new.rs");
    }

    /// Submitting again is the agent correcting itself, not adding to a pile —
    /// a review that showed a superseded claim beside its replacement would be
    /// worse than one that showed neither.
    #[test]
    fn a_second_submission_replaces_the_first() {
        let (_repo, dir, base) = checkout_with_a_base();
        let e = engine();
        let a = attempt_in(&e, &dir, &base);
        std::fs::write(dir.join("src/db.rs"), "// changed\n").unwrap();
        commit_all(&dir, "work");

        e.submit_claims(None, "linear:LIN-142", "Wrong :: src/nothing.rs")
            .unwrap();
        let review = e
            .submit_claims(None, "linear:LIN-142", "Right :: src/db.rs")
            .unwrap();
        assert_eq!(review.remainder.claims.len(), 1);
        assert_eq!(review.remainder.claims[0].text, "Right");
        assert!(review.remainder.complete());
        assert_eq!(e.db.get_attempt(a).unwrap().unwrap().claims.len(), 1);
    }

    /// The verb takes the name the brief handed over (§gh#339). A dispatched
    /// agent is greeted with its identifier — `LIN-142`, `gh#339` — and the
    /// claim path compared that against the id column, so an agent that did
    /// exactly what the skill asked was told its own task was not on the
    /// board. Nothing in a dispatched run exports the id, so the refusal had
    /// no repair in it either.
    #[test]
    fn claims_and_the_review_answer_to_the_identifier_the_agent_was_given() {
        let (_repo, dir, base) = checkout_with_a_base();
        let e = engine();
        let a = attempt_in(&e, &dir, &base);
        std::fs::write(dir.join("src/db.rs"), "// changed\n").unwrap();
        commit_all(&dir, "work");

        let review = e
            .submit_claims(None, "LIN-142", "Storage :: src/db.rs")
            .unwrap();
        assert_eq!(review.remainder.claims.len(), 1);
        assert!(review.remainder.complete());
        assert_eq!(e.db.get_attempt(a).unwrap().unwrap().claims.len(), 1);
        // …and reading it back answers to either spelling, while the review it
        // hands back names the canonical id whichever one was typed: the
        // resolution is at the door, not in the payload.
        assert_eq!(review.task_id, "linear:LIN-142");
        for spelling in ["LIN-142", "linear:LIN-142"] {
            assert_eq!(e.review(spelling, None).unwrap().task_id, "linear:LIN-142");
        }
    }

    /// Prose is refused where it is submitted, and nothing is recorded — the
    /// contract is enforced by the board, not by the agent remembering it.
    #[test]
    fn prose_is_refused_and_leaves_the_attempt_unclaimed() {
        let (_repo, dir, base) = checkout_with_a_base();
        let e = engine();
        let a = attempt_in(&e, &dir, &base);
        let err = e
            .submit_claims(None, "linear:LIN-142", "I improved the storage layer.")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no `::`"), "{err}");
        let attempt = e.db.get_attempt(a).unwrap().unwrap();
        assert!(attempt.claims.is_empty());
        assert_eq!(attempt.claims_at, None, "and it still owes an answer");
    }

    /// The reason the snapshot exists: gc reclaims the checkout (gh#72) and the
    /// review still has to be able to name the unclaimed set.
    #[test]
    fn the_remainder_survives_the_checkout_being_reclaimed() {
        let (_repo, dir, base) = checkout_with_a_base();
        let e = engine();
        let a = attempt_in(&e, &dir, &base);
        std::fs::write(dir.join("src/db.rs"), "// changed\n").unwrap();
        std::fs::write(dir.join("Cargo.lock"), "# nobody says\n").unwrap();
        commit_all(&dir, "work");

        // A reconcile snapshots the diff while the attempt is still live.
        let attempt = e.db.get_attempt(a).unwrap().unwrap();
        e.record_review_facts(None, &attempt);
        e.submit_claims(None, "linear:LIN-142", "Storage :: src/db.rs")
            .unwrap();

        // …and then gc takes the checkout away.
        std::fs::remove_dir_all(&dir).unwrap();
        e.db.set_attempt_collected(a).unwrap();

        let review = e.review("linear:LIN-142", None).unwrap();
        assert!(matches!(review.diff, crate::claims::DiffSource::Recorded));
        assert_eq!(review.remainder.unclaimed.len(), 1);
        assert_eq!(review.remainder.unclaimed[0].path, "Cargo.lock");
        // The effects were snapshotted with it (§gh#236) — they are read from
        // the same checkout, so a review that kept the remainder and lost them
        // would go from "the board looked" to "the board never looked" purely
        // because gc ran.
        assert!(review.effects.read, "the snapshot carries what was derived");
        assert_eq!(review.effects.files.len(), 2);
        // …and the call-site counts do NOT survive, because they were never
        // stored: they need a tree to grep. Absent rather than stale.
        assert!(review.remainder.claims[0].call_sites.is_empty());
    }

    /// …and when there is neither a checkout nor a snapshot, the review says
    /// so. An empty diff would read as "this branch changed nothing", which is
    /// the opposite of what happened.
    #[test]
    fn a_review_with_no_diff_to_read_says_why_rather_than_showing_none() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_attempt_worktree(a, "/wt/gone-long-ago").unwrap();
        e.db.set_attempt_collected(a).unwrap();

        let review = e.review("linear:LIN-142", None).unwrap();
        let crate::claims::DiffSource::Unavailable { reason } = &review.diff else {
            panic!("expected the reason, got {:?}", review.diff);
        };
        assert!(reason.contains("reclaimed"), "{reason}");
        assert!(!review.claimed());
    }

    /// The evidence half: what the run's own commands did, off the journal,
    /// recorded onto the row while the journal is still there.
    #[test]
    fn the_runs_commands_are_recorded_as_evidence_against_the_attempt() {
        use crate::evidence::RanCommand;

        struct Journal;
        impl Runtime for Journal {
            fn dispatch(
                &self,
                _spec: &crate::runtime::DispatchSpec,
            ) -> Result<crate::runtime::DispatchHandle> {
                unreachable!()
            }
            fn prompt(&self, _chat: &str, _text: &str) -> Result<()> {
                Ok(())
            }
            fn cancel(&self, _chat: &str) -> Result<()> {
                Ok(())
            }
            fn session(&self, _chat: &str) -> Result<Option<comet_proto::Session>> {
                Ok(None)
            }
            fn chat_alive(&self, _chat: &str) -> Result<bool> {
                Ok(true)
            }
            fn chat_cwd(&self, _chat: &str) -> Result<Option<String>> {
                Ok(None)
            }
            fn last_run_end(&self, _chat: &str) -> Result<Option<RunEnd>> {
                Ok(None)
            }
            fn run_commands(&self, _chat: &str) -> Result<Option<Vec<RanCommand>>> {
                Ok(Some(vec![
                    RanCommand {
                        command: "git status".into(),
                        failed: false,
                    },
                    RanCommand {
                        command: "cargo test -p comet-board".into(),
                        failed: true,
                    },
                    RanCommand {
                        command: "cargo test -p comet-board".into(),
                        failed: false,
                    },
                ]))
            }
        }

        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let attempt = e.db.get_attempt(a).unwrap().unwrap();
        e.record_review_facts(Some(&Journal), &attempt);

        let review = e.review("linear:LIN-142", None).unwrap();
        assert_eq!(review.evidence.commands, 3);
        assert_eq!(review.evidence.failed, 1);
        assert!(review.evidence.checked());
        assert_eq!(
            review.evidence.checks[0].command,
            "cargo test -p comet-board"
        );
        assert_eq!(review.evidence.checks[0].runs, 2);
        assert!(review.evidence.checks[0].ever_passed());
    }

    /// The other half of the same tick (§gh#349): the terms those commands ran
    /// under, recorded off the harness's own report and rendered as a caveat
    /// rather than as a finding.
    #[test]
    fn the_sandbox_the_run_actually_got_is_recorded_and_read_back_on_the_review() {
        use comet_proto::{SandboxLevel, SandboxReport};

        /// A chat whose harness widened the sandbox out from under the
        /// dispatch — and which ran no commands at all, so the recording must
        /// not be behind the commands read.
        struct Widened;
        impl Runtime for Widened {
            fn dispatch(&self, _: &DispatchSpec) -> Result<DispatchHandle> {
                unreachable!()
            }
            fn prompt(&self, _: &str, _: &str) -> Result<()> {
                Ok(())
            }
            fn cancel(&self, _: &str) -> Result<()> {
                Ok(())
            }
            fn session(&self, _: &str) -> Result<Option<comet_proto::Session>> {
                Ok(None)
            }
            fn chat_alive(&self, _: &str) -> Result<bool> {
                Ok(true)
            }
            fn chat_cwd(&self, _: &str) -> Result<Option<String>> {
                Ok(None)
            }
            fn last_run_end(&self, _: &str) -> Result<Option<RunEnd>> {
                Ok(None)
            }
            fn run_sandbox(&self, _: &str) -> Result<Option<SandboxReport>> {
                Ok(Some(SandboxReport::widened(
                    SandboxLevel::WorkspaceWrite,
                    SandboxLevel::DangerFullAccess,
                    "this codex predates the worktree-mount fix",
                )))
            }
        }

        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let attempt = e.db.get_attempt(a).unwrap().unwrap();

        // Before anything is recorded the answer is "nobody said", which is
        // not the same as "it was sandboxed" and must not render as one.
        assert_eq!(e.review("linear:LIN-142", None).unwrap().sandbox, None);
        assert_eq!(
            e.review("linear:LIN-142", None).unwrap().sandbox_note(),
            None
        );

        e.record_review_facts(Some(&Widened), &attempt);

        let review = e.review("linear:LIN-142", None).unwrap();
        let report = review.sandbox.as_ref().expect("recorded");
        assert_eq!(report.effective, SandboxLevel::DangerFullAccess);
        assert_eq!(report.requested, SandboxLevel::WorkspaceWrite);
        let note = review.sandbox_note().expect("worth saying");
        assert!(note.contains("full access to the box"), "{note}");
        assert!(note.contains("workspace-write was requested"), "{note}");

        // A caveat, never a finding: the verdict is about what nobody
        // accounted for, and two of three runtimes run unsandboxed always —
        // routed through `findings` this would shout on nearly every review.
        assert!(
            !review
                .findings()
                .iter()
                .any(|f| f.text.contains("full access")),
            "the sandbox note stays out of the findings: {:?}",
            review.findings()
        );
    }

    // ---- the claims block, off a finished attempt (§gh#235) ---------------

    /// A runtime whose chat said something when it finished — the only new
    /// question the harvest asks of one.
    struct Closing(&'static str);

    impl Runtime for Closing {
        fn dispatch(&self, _: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
            unreachable!("settling never dispatches")
        }
        fn prompt(&self, _: &str, _: &str) -> anyhow::Result<()> {
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
            Ok(None)
        }
        fn last_run_end(&self, _: &str) -> anyhow::Result<Option<RunEnd>> {
            Ok(Some(RunEnd::Completed))
        }
        fn run_message(&self, _: &str) -> anyhow::Result<Option<String>> {
            Ok(Some(self.0.to_string()))
        }
    }

    /// Settle a task whose run just ended cleanly with a PR already open, on
    /// the runtime given — the shortest honest path to `settle`.
    ///
    /// With a real checkout under it, because a review with no diff to read
    /// reports only that (§gh#183) and would drown out every claim finding
    /// these tests are here for.
    fn settled_with(rt: &dyn Runtime) -> (TestEngine, i64, tempfile::TempDir) {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        let (repo, _work) = agent_worked_in(&e, a, Work::Pushed);
        e.db.set_pr(
            "linear:LIN-142",
            Some("https://github.com/o/r/pull/18"),
            Some(18),
            true,
        )
        .unwrap();
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(rt))
            .unwrap();
        (e, a, repo)
    }

    #[test]
    fn a_closing_message_with_a_claims_block_is_read_onto_the_attempt() {
        let (e, a, _work) = settled_with(&Closing(
            "All done, PR is up.\n\n\
             ```claims\n\
             Storage lives on the attempt :: crates/board/src/db.rs\n\
             The remainder comes off the diff :: remainder\n\
             ```\n",
        ));
        let attempt = e.db.get_attempt(a).unwrap().unwrap();
        assert_eq!(attempt.outcome, Some(Outcome::Done), "and it still settles");
        assert!(attempt.claims_at.is_some(), "the contract was answered");
        assert_eq!(attempt.claims.len(), 2);
        assert_eq!(attempt.claims[0].files, ["crates/board/src/db.rs"]);
        assert_eq!(attempt.claims[1].symbols, ["remainder"]);
        assert!(attempt.claims_error.is_none());
    }

    /// The ticket's exit condition: nothing about claims may stand between a
    /// finished run and its pull request.
    #[test]
    fn an_attempt_that_claimed_nothing_settles_exactly_as_it_always_did() {
        let (e, a, _work) = settled_with(&Closing("Done. The tests pass and the PR is open."));
        let attempt = e.db.get_attempt(a).unwrap().unwrap();
        assert_eq!(attempt.outcome, Some(Outcome::Done));
        assert!(attempt.claims.is_empty());
        assert!(attempt.claims_at.is_none(), "never asked, never answered");
        assert!(attempt.claims_error.is_none(), "and nothing to report");

        let review = e.review("linear:LIN-142", None).unwrap();
        assert!(!review.claimed());
        assert_eq!(
            review.findings()[0].kind,
            crate::claims::FindingKind::NeverClaimed
        );
    }

    /// Reported, never dropped — and it still does not hold up the settle.
    #[test]
    fn a_malformed_block_is_recorded_against_the_attempt_rather_than_dropped() {
        let (e, a, _work) = settled_with(&Closing(
            "Finished.\n\n```claims\nI rewrote the storage layer\n```\n",
        ));
        let attempt = e.db.get_attempt(a).unwrap().unwrap();
        assert_eq!(attempt.outcome, Some(Outcome::Done), "and it still settles");
        assert!(attempt.claims.is_empty());
        assert!(attempt.claims_at.is_none());
        let why = attempt.claims_error.expect("the refusal is on the row");
        assert!(why.contains("I rewrote the storage layer"), "{why}");

        let review = e.review("linear:LIN-142", None).unwrap();
        let finding = &review.findings()[0];
        assert_eq!(finding.kind, crate::claims::FindingKind::MalformedClaims);
        assert!(
            finding.kind.tone().loud(),
            "louder than having said nothing"
        );
    }

    /// The harvest is a scrape and the verb is an answer. An attempt that
    /// already submitted keeps what it submitted.
    #[test]
    fn a_harvest_never_overwrites_what_the_agent_submitted() {
        let e = engine();
        seed(&e, "linear:LIN-142", "LIN-142", UpstreamState::Started);
        let a = dispatch(&e, "linear:LIN-142", "chat-9");
        e.db.set_pr(
            "linear:LIN-142",
            Some("https://github.com/o/r/pull/18"),
            Some(18),
            true,
        )
        .unwrap();
        e.submit_claims(None, "linear:LIN-142", "The considered answer :: src/a.rs")
            .unwrap();

        let rt = Closing("```claims\nA later afterthought :: src/b.rs\n```");
        e.reconcile_sessions_with(&statuses(&[("chat-9", AgentStatus::Idle)]), Some(&rt))
            .unwrap();

        let attempt = e.db.get_attempt(a).unwrap().unwrap();
        assert_eq!(attempt.outcome, Some(Outcome::Done));
        assert_eq!(attempt.claims.len(), 1);
        assert_eq!(attempt.claims[0].files, ["src/a.rs"]);
    }

    /// A good set supersedes a bad block: the refusal goes with it, or the
    /// review would show a draft beside its correction.
    #[test]
    fn submitting_claims_clears_an_earlier_refusal() {
        let (e, a, _work) = settled_with(&Closing("```claims\nno anchor here\n```"));
        assert!(e.db.get_attempt(a).unwrap().unwrap().claims_error.is_some());

        e.submit_claims(None, "linear:LIN-142", "Said properly :: src/a.rs")
            .unwrap();
        let attempt = e.db.get_attempt(a).unwrap().unwrap();
        assert!(attempt.claims_error.is_none());
        assert_eq!(attempt.claims.len(), 1);
        assert!(attempt.claims_at.is_some());
    }

    // ---- surviving the parent's merge (gh#286) ---------------------------

    /// A checkout laid out the way a two-layer stack's is: trunk with the
    /// operator's own unpushed commit under it, a parent branch with the layer
    /// below's commit, and a child branch cut from the parent.
    ///
    /// Returns the checkout, the parent's tip (which is what the child's
    /// dispatch stamps as its base) and the branch names.
    struct Stacked {
        work: std::path::PathBuf,
        parent_tip: String,
        /// Owns the checkout `work` points into.
        _repo: tempfile::TempDir,
    }

    impl Stacked {
        const PARENT: &'static str = "board/gh-11-parent";
        const CHILD: &'static str = "board/gh-12-child";

        fn cut() -> Stacked {
            let (_repo, work) = repo_ahead_of_its_remote();
            git_in(&work, &["checkout", "-b", Stacked::PARENT]);
            std::fs::write(work.join("parent.rs"), "the layer below").unwrap();
            git_in(&work, &["add", "."]);
            git_in(&work, &["commit", "-m", "the parent's work"]);
            let parent_tip = Stacked::sha(&work, "HEAD");
            git_in(&work, &["checkout", "-b", Stacked::CHILD]);
            std::fs::write(work.join("child.rs"), "the layer above").unwrap();
            git_in(&work, &["add", "."]);
            git_in(&work, &["commit", "-m", "the child's work"]);
            Stacked {
                work,
                parent_tip,
                _repo,
            }
        }

        fn sha(work: &std::path::Path, rev: &str) -> String {
            String::from_utf8_lossy(
                &std::process::Command::new("git")
                    .arg("-C")
                    .arg(work)
                    .args(["rev-parse", rev])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .trim()
            .to_string()
        }

        /// What GitHub does when the layer below lands: the parent's content
        /// arrives on trunk under a different commit, trunk carries on moving
        /// under everybody else's merges, and the child is rebased onto it.
        /// The rebase itself is done here in the checkout, which is what `gh
        /// stack sync` or `git pull --rebase` does when the agent runs it.
        fn parent_lands_and_the_child_is_rebased(&self) -> String {
            git_in(&self.work, &["checkout", "main"]);
            git_in(&self.work, &["merge", "--squash", Stacked::PARENT]);
            git_in(
                &self.work,
                &["commit", "-m", "the parent, squashed onto trunk"],
            );
            std::fs::write(self.work.join("somebody-else.rs"), "an unrelated merge").unwrap();
            git_in(&self.work, &["add", "."]);
            git_in(&self.work, &["commit", "-m", "somebody else's work"]);
            git_in(&self.work, &["push", "origin", "main"]);
            let trunk = Stacked::sha(&self.work, "HEAD");
            git_in(&self.work, &["checkout", Stacked::CHILD]);
            git_in(
                &self.work,
                &["rebase", "--onto", "main", Stacked::PARENT, Stacked::CHILD],
            );
            trunk
        }

        fn path(&self) -> String {
            self.work.to_string_lossy().into_owned()
        }
    }

    /// Give the engine a child attempt in that checkout, stamped the way a
    /// stacked dispatch stamps one: the parent's tip is the base.
    fn stacked_child(e: &SyncEngine, s: &Stacked) -> i64 {
        seed(e, "gh:o/r#12", "gh#12", UpstreamState::Started);
        let a = dispatch(e, "gh:o/r#12", "chat-12");
        e.db.conn
            .execute(
                "UPDATE attempts SET branch = ?2 WHERE id = ?1",
                rusqlite::params![a, Stacked::CHILD],
            )
            .unwrap();
        e.db.set_attempt_worktree(a, &s.path()).unwrap();
        e.db.set_attempt_base_sha(a, &s.parent_tip).unwrap();
        a
    }

    /// The headline of consequence 1: while nothing has moved the branch, the
    /// stamp is exactly right and is left alone — and the moment the checkout
    /// is rebased out from under it, everything measures from where the branch
    /// starts *now* instead of attributing the layer below to this attempt.
    #[test]
    fn a_rebased_branch_is_re_footed_and_stops_counting_the_layer_below() {
        let s = Stacked::cut();
        let e = engine();
        let a = stacked_child(&e, &s);
        let attempt = |e: &SyncEngine| e.db.get_attempt(a).unwrap().unwrap();

        // Before anything moves: the stamp stands, and the child's diff is the
        // child's one file.
        assert_eq!(
            e.attempt_base(&attempt(&e)).as_deref(),
            Some(s.parent_tip.as_str())
        );
        let (changed, _) = e.branch_facts(&attempt(&e)).unwrap();
        assert_eq!(
            changed.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
            ["child.rs"],
        );

        // The parent lands, GitHub rebases, and the agent brings the checkout
        // across. The stamp is now a commit on no branch at all.
        let trunk = s.parent_lands_and_the_child_is_rebased();
        e.db.set_pr_topology("gh:o/r#12", Some("main"), Some("work"), None)
            .unwrap();

        assert_eq!(
            e.attempt_base(&attempt(&e)).as_deref(),
            Some(trunk.as_str()),
            "the fork point against the base the request now targets"
        );
        // …and it is written back, so every later reader gets it for free.
        assert_eq!(attempt(&e).base_sha.as_deref(), Some(trunk.as_str()));

        // The bug this closes, pinned: measured from the stale stamp the
        // child's branch reads as three commits touching everything trunk has
        // moved by since it was cut.
        let range = format!("{}..HEAD", s.parent_tip);
        assert_eq!(
            git_out(&s.path(), &["rev-list", "--count", &range]).as_deref(),
            Some("3"),
        );
        assert!(
            git_out(&s.path(), &["diff", "--name-only", &range])
                .unwrap()
                .contains("somebody-else.rs"),
            "the stamp measures everybody else's merges as this attempt's work",
        );
        let (changed, _) = e.branch_facts(&attempt(&e)).unwrap();
        assert_eq!(
            changed.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
            ["child.rs"],
            "and the re-footed base is still only this layer's work",
        );
        // The settle's commit count agrees: one commit, not two.
        assert!(e.attempt_has_commits(Some(&s.path()), e.attempt_base(&attempt(&e)).as_deref()));
    }

    /// A runtime that answers the two questions the rewrite notice asks — is
    /// the chat alive, and is it still standing in the checkout — and records
    /// what it was told to say.
    struct InTheCheckout {
        cwd: Option<String>,
        alive: bool,
        said: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl InTheCheckout {
        fn at(cwd: &str) -> InTheCheckout {
            InTheCheckout {
                cwd: Some(cwd.to_string()),
                alive: true,
                said: Default::default(),
            }
        }
        fn said(&self) -> Vec<(String, String)> {
            self.said.lock().unwrap().clone()
        }
    }

    impl Runtime for InTheCheckout {
        fn dispatch(&self, _: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
            unreachable!("noticing a rebase never dispatches")
        }
        fn prompt(&self, chat: &str, text: &str) -> anyhow::Result<()> {
            self.said
                .lock()
                .unwrap()
                .push((chat.to_string(), text.to_string()));
            Ok(())
        }
        fn cancel(&self, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn session(&self, _: &str) -> anyhow::Result<Option<comet_proto::Session>> {
            Ok(None)
        }
        fn chat_alive(&self, _: &str) -> anyhow::Result<bool> {
            Ok(self.alive)
        }
        fn chat_cwd(&self, _: &str) -> anyhow::Result<Option<String>> {
            Ok(self.cwd.clone())
        }
        fn last_run_end(&self, _: &str) -> anyhow::Result<Option<RunEnd>> {
            Ok(None)
        }
    }

    /// Point the child's remote-tracking ref at the rebased history GitHub
    /// wrote, which is what a fetch in that checkout would have done.
    fn origin_rebased_the_child(s: &Stacked) -> String {
        git_in(&s.work, &["checkout", "-b", "as-github-holds-it", "main"]);
        std::fs::write(s.work.join("child.rs"), "the layer above").unwrap();
        git_in(&s.work, &["add", "."]);
        git_in(&s.work, &["commit", "-m", "the child's work, rebased"]);
        let rewritten = Stacked::sha(&s.work, "HEAD");
        git_in(&s.work, &["checkout", Stacked::CHILD]);
        git_in(
            &s.work,
            &[
                "update-ref",
                &format!("refs/remotes/origin/{}", Stacked::CHILD),
                &rewritten,
            ],
        );
        rewritten
    }

    /// Consequence 2: the checkout did not move, so the agent standing in it
    /// is one `git push` away from putting the pre-rebase history back over
    /// GitHub's work. The board cannot stop that push; it can be the one party
    /// that knows, and it says so where the agent will read it.
    #[test]
    fn an_agent_whose_branch_was_rewritten_on_origin_is_told_once() {
        let s = Stacked::cut();
        let e = engine();
        // The parent, and the child cut from it.
        seed(&e, "gh:o/r#11", "gh#11", UpstreamState::Terminal);
        let parent = dispatch(&e, "gh:o/r#11", "chat-11");
        e.db.conn
            .execute(
                "UPDATE attempts SET branch = ?2 WHERE id = ?1",
                rusqlite::params![parent, Stacked::PARENT],
            )
            .unwrap();
        let child = stacked_child(&e, &s);
        e.db.conn
            .execute(
                "UPDATE attempts SET stacked_on = ?2 WHERE id = ?1",
                rusqlite::params![child, parent],
            )
            .unwrap();

        // Trunk moves and GitHub rewrites the child's branch. The checkout
        // stays exactly where it was.
        s.parent_lands_and_the_child_is_rebased();
        git_in(&s.work, &["reset", "--hard", "ORIG_HEAD"]);
        let rewritten = origin_rebased_the_child(&s);
        // What the board has seen so far: the child still targets the parent's
        // branch, and the parent's request is still open.
        e.db.set_pr_topology("gh:o/r#12", Some(Stacked::PARENT), Some("work"), None)
            .unwrap();

        let rt = InTheCheckout::at(&s.path());
        // Nothing has landed as far as the board knows, so nothing is said —
        // and no branch on the box is compared against origin for it.
        e.note_rewritten_branches(Some(&rt));
        assert!(rt.said().is_empty(), "no landing, no rewrite to report");

        // The poll catches up: the parent merged, and the child was retargeted
        // onto trunk in the same motion.
        e.db.set_pr_merged("gh:o/r#11", true).unwrap();
        e.db.set_pr_topology("gh:o/r#12", Some("main"), Some("work"), None)
            .unwrap();
        e.note_rewritten_branches(Some(&rt));
        let said = rt.said();
        assert_eq!(said.len(), 1);
        assert_eq!(said[0].0, "chat-12");
        assert!(said[0].1.contains(Stacked::CHILD));
        assert!(said[0].1.contains("git rebase origin/main"));

        // Once. An agent told the same thing every cycle learns to skip it.
        e.note_rewritten_branches(Some(&rt));
        assert_eq!(rt.said().len(), 1);
        assert_eq!(
            e.db.meta_get(&meta::rewritten_noted(child))
                .unwrap()
                .as_deref(),
            Some(rewritten.as_str()),
            "and what was said about is on the record",
        );
    }

    /// A read-only caller must not consume the one notice the agent gets, and
    /// an unstacked attempt is never looked at in the first place.
    #[test]
    fn nothing_is_consumed_without_a_runtime_to_tell() {
        let e = engine();
        e.note_rewritten_branches(None);
        assert!(e.db.meta_get(&meta::rewritten_noted(1)).unwrap().is_none());
    }

    // ---- telling GitHub about a chain the board cut (gh#387) ------------

    /// A `--onto` dispatch as the board holds it once the agent has opened its
    /// pull request: an attempt cut from `onto`, and a pull request based on
    /// that attempt's branch — and no stack anywhere.
    fn seed_onto(e: &SyncEngine, number: i64, pr: i64, base: &str, onto: Option<i64>) -> i64 {
        let id = format!("gh:o/r#{number}");
        seed_gh_in(e, &id);
        let branch = format!("board/gh-{number}-x");
        let attempt =
            e.db.insert_attempt(&crate::db::NewAttempt {
                automation: None,
                automation_owner: None,
                task_id: id.clone(),
                branch: Some(branch.clone()),
                stacked_on: onto,
                pane_id: None,
                workspace: "offhand".into(),
                runtime: "claude-code".into(),
                worktree: None,
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
        e.db.set_pr(
            &id,
            Some(&format!("https://github.com/o/r/pull/{pr}")),
            Some(pr),
            true,
        )
        .unwrap();
        e.db.set_pr_topology(&id, Some(base), Some(&branch), None)
            .unwrap();
        attempt
    }

    /// An engine whose GitHub answers a stack create, and the fixture the test
    /// reads the request back off.
    fn engine_that_can_stack() -> (TestEngine, std::rc::Rc<FixtureRest>) {
        let rest = std::rc::Rc::new(FixtureRest::new(vec![(
            "POST /repos/o/r/stacks".into(),
            json!({ "id": 334176, "number": 49, "base": { "ref": "main" } }),
        )]));
        let e = engine_with(Some(Github::new(Box::new(SharedRest(rest.clone())))));
        (e, rest)
    }

    /// The fixture, still readable after the engine has boxed it away.
    struct SharedRest(std::rc::Rc<FixtureRest>);

    impl Rest for SharedRest {
        fn get(&self, path: &str) -> Result<Value> {
            self.0.get(path)
        }
        fn post(&self, path: &str, body: &Value) -> Result<Value> {
            self.0.post(path, body)
        }
        fn patch(&self, path: &str, body: &Value) -> Result<Value> {
            self.0.patch(path, body)
        }
        fn put(&self, path: &str, body: &Value) -> Result<Value> {
            self.0.put(path, body)
        }
    }

    /// The headline, end to end: two tasks the board cut from each other, and
    /// the sweep sends GitHub the one call that makes them a stack — bottom
    /// first, in the repository the task ids name.
    #[test]
    fn a_chain_the_board_cut_is_stacked_on_github() {
        let (e, rest) = engine_that_can_stack();
        let bottom = seed_onto(&e, 44, 47, "main", None);
        seed_onto(&e, 45, 48, "board/gh-44-x", Some(bottom));

        e.link_dispatched_stacks();

        let wrote = rest.wrote.borrow().clone();
        assert_eq!(wrote.len(), 1, "{wrote:?}");
        assert_eq!(wrote[0].0, "POST");
        assert_eq!(wrote[0].1, "/repos/o/r/stacks");
        assert_eq!(wrote[0].2, json!({ "pull_requests": [47, 48] }));
    }

    /// And it is sent once. The poll that would prove the stack landed is a
    /// whole cycle away, and a sweep that filled that window with retries would
    /// be a write on a loop.
    #[test]
    fn the_same_stack_is_never_asked_for_twice() {
        let (e, rest) = engine_that_can_stack();
        let bottom = seed_onto(&e, 44, 47, "main", None);
        seed_onto(&e, 45, 48, "board/gh-44-x", Some(bottom));

        e.link_dispatched_stacks();
        e.link_dispatched_stacks();
        e.link_dispatched_stacks();

        assert_eq!(rest.wrote.borrow().len(), 1);
        assert_eq!(
            e.db.meta_get(&meta::stack_asked("create:o/r:47,48"))
                .unwrap()
                .as_deref(),
            Some("linked"),
            "and what was asked for is on the record",
        );
    }

    /// A refusal is retried, and then the chain is left alone — a repository
    /// with stacks switched off must not be asked at the poll interval for as
    /// long as the board is up.
    #[test]
    fn a_refused_stack_stops_after_its_budget() {
        // No `POST /repos/o/r/stacks` route, so the fixture answers `Null`
        // and `create_stack` finds no number in it — a refusal, as far as the
        // sweep is concerned.
        let rest = std::rc::Rc::new(FixtureRest::new(vec![]));
        let e = engine_with(Some(Github::new(Box::new(SharedRest(rest.clone())))));
        let bottom = seed_onto(&e, 44, 47, "main", None);
        seed_onto(&e, 45, 48, "board/gh-44-x", Some(bottom));

        for _ in 0..6 {
            e.link_dispatched_stacks();
        }
        assert_eq!(
            rest.wrote.borrow().len(),
            crate::stacks::LINK_TRIES as usize,
        );

        // The chain growing a layer is a different request, and asks again.
        seed_onto(&e, 46, 50, "board/gh-45-x", Some(bottom + 1));
        e.link_dispatched_stacks();
        assert_eq!(
            rest.wrote.borrow().last().unwrap().2,
            json!({ "pull_requests": [47, 48, 50] }),
        );
    }

    /// A board with no GitHub credential has nobody to tell, and an ordinary
    /// dispatch is not a chain: neither costs a call.
    #[test]
    fn nothing_is_sent_for_a_board_with_no_chain_to_stack() {
        let (e, rest) = engine_that_can_stack();
        seed_onto(&e, 44, 47, "main", None);
        e.link_dispatched_stacks();
        assert!(rest.wrote.borrow().is_empty());

        let e = engine();
        e.link_dispatched_stacks();
    }
}
