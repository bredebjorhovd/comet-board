//! `CometRuntime` — the board's [`Runtime`] trait implemented against engine
//! internals (§runtime-impl).
//!
//! What herdr-board's `herdr.rs` drove over a mux socket, this does with the
//! primitives directly: worktrees via [`Repos`], chats via [`WorkspaceHost`],
//! prompts via the [`DocHost`] command ledger, agent state off the merged
//! session mirror. Every method is a thin verb; the board core owns the
//! decisions.
//!
//! The trait is sync on purpose (the board loop is a blocking thread), so the
//! async engine calls run through a captured runtime [`Handle`]. The loop enters
//! that handle for its life (`run_loop`'s `handle.enter()`), so `Handle::block_on`
//! and the `tokio::spawn`s inside `doc_host.open` work from its plain thread.
//! Methods must therefore never be called from a tokio worker thread — in the
//! engine they run on the `comet-board-sync` thread only.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use comet_board::evidence::RanCommand;
use comet_board::runtime::{
    DispatchHandle, DispatchSpec, RefusedBeforeCut, ReviewCandidate, RunEnd, RunTokens, Runtime,
    RuntimeUnavailable,
};
use comet_doc::SessionCommandPayload;
use comet_proto::{
    AgentEvent, ChatConfig, DoneStatus, RunRequest, SandboxLevel, Session, SessionStatus,
};
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::agent_accounts::AgentAccounts;
use crate::checkout_prep::CheckoutPrep;
use crate::doc_host::DocHost;
use crate::repos::Repos;
use crate::run_journal::RunJournal;
use crate::workspace_host::WorkspaceHost;

pub struct CometRuntime {
    repos: Repos,
    workspace: WorkspaceHost,
    doc_host: DocHost,
    /// The same merged local+remote session stream `WatchSessions` serves —
    /// one mirror, so the board can never disagree with the frontends.
    sessions: watch::Receiver<Vec<Session>>,
    /// The engine's run journal — the settle authority (§settle-logic):
    /// `last_run_end` reads a chat's final journaled event off it.
    journal: Arc<RunJournal>,
    /// The device's saved agent logins — a dispatch naming one materializes it
    /// into its own config dir here, before the chat exists (gh#59).
    accounts: AgentAccounts,
    /// The exact resolver also wired into `Sessions`, so dispatch preflights
    /// the handoff later runs will receive rather than only GitHub metadata.
    push: OnceLock<Arc<crate::push_credentials::PushCredentials>>,
    /// The repository's own recipe for turning a fresh checkout into an
    /// environment that can build (gh#422). An engine primitive, held rather
    /// than reimplemented: the same object serves the `PrepareCheckout` RPC an
    /// ordinary (undispatched) session uses, so a repo writes one recipe and
    /// both paths obey it.
    prep: CheckoutPrep,
    handle: Handle,
}

impl CometRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repos: Repos,
        workspace: WorkspaceHost,
        doc_host: DocHost,
        sessions: watch::Receiver<Vec<Session>>,
        journal: Arc<RunJournal>,
        accounts: AgentAccounts,
        prep: CheckoutPrep,
        handle: Handle,
    ) -> Self {
        Self {
            repos,
            workspace,
            doc_host,
            sessions,
            journal,
            accounts,
            push: OnceLock::new(),
            prep,
            handle,
        }
    }

    pub fn set_push_credentials(&self, push: Arc<crate::push_credentials::PushCredentials>) {
        let _ = self.push.set(push);
    }

    fn chat_config(&self, chat_id: &str) -> Option<ChatConfig> {
        self.workspace.chat_config(chat_id)
    }

    fn verify_chat_push_credentials(&self, chat_id: &str) -> anyhow::Result<()> {
        let Some(push) = self.workspace.github_push_state(chat_id)? else {
            return Ok(());
        };
        self.verify_push_credentials(&push.repo, push.contract)
    }

    /// Hold the brief until the checkout says it is ready (gh#422).
    ///
    /// Three shapes, and the first is every repository that has not written a
    /// recipe yet: **no recipe, no change** — the brief is queued inline
    /// exactly as it was before this existed, at the same cost.
    ///
    /// With a recipe, the brief is *parked* and preparation is spawned rather
    /// than awaited. Awaited would be simpler and is wrong twice over: this
    /// runs on the board's single loop thread, where a five-minute
    /// `npm install` would stall every other task's status, cancel and sync
    /// behind it; and `handle_dispatch`'s caller is a frontend holding a
    /// button. So `dispatch` returns with a chat that exists, is shared, and
    /// says what it is waiting for — and the run that costs money starts when
    /// (and only when) the checkout is ready.
    ///
    /// A failure queues nothing. The attempt is live with a chat nobody is
    /// billing, which is precisely the state that a `PrepareCheckout` retry
    /// resolves without minting a second attempt: the parked brief is still
    /// there, and the retry releases it.
    fn park_for_preparation(
        &self,
        chat_id: &str,
        cwd: &str,
        brief: SessionCommandPayload,
    ) -> anyhow::Result<Option<String>> {
        let worktree = std::path::PathBuf::from(cwd);
        // The inline path: no recipe, or one that gates nothing — a repo that
        // wrote only `[run]` has said "here is how to drive me", not "let the
        // host engine do something", and its dispatches must not grow a parked
        // brief and a "preparing…" notice for a no-op. `[archive]` does gate:
        // approval before the agent runs prevents a later repository edit from
        // turning reclamation into unreviewed engine-user code. A recipe that does not
        // *parse* is neither: it must reach `prepare`, which writes it down as
        // a failure instead of starting an agent in a checkout whose
        // repository asked for something we could not read.
        let gates = match self.prep.recipe(&worktree) {
            Ok(None) => false,
            Ok(Some(recipe)) => recipe.requires_host_approval(),
            Err(_) => true,
        };
        if !gates {
            return Ok(None);
        }

        let repository_id = self
            .handle
            .block_on(self.repos.repository_identity(&worktree))
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let parked = serde_json::to_string(&ParkedBrief {
            chat_id: chat_id.to_string(),
            command_id: crate::new_id(),
            repository_id: repository_id.clone(),
            command: brief.clone(),
        })?;
        // This is the point of no return: the durable parked command must
        // exist before a chat can be presented as an attempt. A failure here
        // is a dispatch failure, not a warning that silently deletes recovery.
        self.prep
            .park(&worktree, &parked)
            .map_err(|e| anyhow::anyhow!("parking the dispatch before preparation: {e}"))?;
        Ok(Some(repository_id))
    }

    fn start_preparation(&self, chat_id: &str, cwd: &str, repository_id: String) {
        let worktree = std::path::PathBuf::from(cwd);
        notice(
            &self.doc_host,
            chat_id,
            &format!(
                "Preparing this checkout before starting — {}/{}. \
                 The run has not started and nothing is being billed yet.",
                cwd,
                crate::checkout_prep::RECIPE_PATH
            ),
            false,
        );

        let prep = self.prep.clone();
        let doc_host = self.doc_host.clone();
        self.handle.spawn(async move {
            let record = prep
                .prepare(crate::checkout_prep::PrepareRequest {
                    worktree: &worktree,
                    repository_id: &repository_id,
                    force: false,
                    cancel: None,
                })
                .await;
            settle_preparation(&prep, &doc_host, &worktree, &record);
        });
    }
}

/// The work a preparation is holding back, with the chat it belongs to.
///
/// Parked as opaque JSON in the checkout's own state directory rather than
/// held in memory, so that a retry after the engine restarted still knows what
/// it is releasing — a box that reboots mid-`npm install` should not cost an
/// attempt.
#[derive(serde::Serialize, serde::Deserialize)]
struct ParkedBrief {
    chat_id: String,
    command_id: String,
    repository_id: String,
    command: SessionCommandPayload,
}

/// What to do with a finished preparation: release the parked work, or say why
/// it is still parked.
///
/// Shared with the `PrepareCheckout` RPC (`crate::rpc`) on purpose — a retry
/// and a first attempt must end the same way, or the recovery path is a second
/// implementation of the thing it is recovering.
pub(crate) fn settle_preparation(
    prep: &crate::checkout_prep::CheckoutPrep,
    doc_host: &DocHost,
    worktree: &Path,
    record: &crate::checkout_prep::PrepRecord,
) {
    for link in &record.links {
        tracing::info!(
            from = %link.from,
            to = %link.to,
            result = %link.result,
            mode = link.mode.as_deref().unwrap_or("-"),
            "checkout link"
        );
    }
    // Persist the reach in the chat before queueing the run. The board record
    // carries the same structured list, but a transcript is the evidence an
    // operator can read without winning a race against a fast agent start.
    if !record.links.is_empty()
        && let Some(parked) = peek_parked(prep, worktree)
    {
        let projections = record
            .links
            .iter()
            .map(|link| {
                format!(
                    "- `{}` → `{}`: {}{}",
                    link.from,
                    link.to,
                    link.result,
                    link.mode
                        .as_deref()
                        .map(|mode| format!(" (mode {mode})"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        notice(
            doc_host,
            &parked.chat_id,
            &format!(
                "Machine-local files made reachable in this checkout before the run:\n{projections}"
            ),
            false,
        );
    }
    if !record.is_ready() {
        tracing::error!(summary = %record.summary(), "checkout preparation failed");
        // The parked brief stays parked. Whoever reads this can retry the
        // preparation, and the same attempt continues.
        if let Some(parked) = peek_parked(prep, worktree) {
            let log = record
                .log
                .as_deref()
                .map(|p| format!("\n\nOutput: {p}"))
                .unwrap_or_default();
            let head = prep
                .log_head(worktree, 2000)
                .map(|t| format!("\n\n```\n{}\n```", t.trim_end()))
                .unwrap_or_default();
            notice(
                doc_host,
                &parked.chat_id,
                &format!(
                    "This checkout could not be prepared, so the run has not started \
                     and nothing has been billed.\n\n{}{log}{head}\n\nThe checkout is \
                     intact at {}. Fix the cause and prepare it again — the brief is \
                     held and will be sent as soon as preparation succeeds.",
                    record.summary(),
                    worktree.display(),
                ),
                true,
            );
        }
        return;
    }
    tracing::info!(summary = %record.summary(), "checkout ready");
    let Some(claim) = prep.claim_parked(worktree) else {
        return;
    };
    let parked: ParkedBrief = match serde_json::from_str(claim.payload()) {
        Ok(parked) => parked,
        Err(e) => {
            tracing::error!(error = %e, "reading the claimed prepared brief");
            return;
        }
    };
    match claim.deliver(|_| {
        doc_host.queue_command_with_id_durable(
            &parked.chat_id,
            &parked.command_id,
            parked.command.clone(),
        )
    }) {
        Ok(true) => {}
        Ok(false) => tracing::info!(chat = %parked.chat_id, "prepared brief was revoked before release"),
        Err(e) => {
            // Dropping the claim rolls the durable payload back for retry.
            tracing::error!(chat = %parked.chat_id, error = %e, "releasing the prepared brief");
        }
    }
}

/// Complete ready handoffs left across an engine crash. Both crash windows use
/// the same path as the live completion: `ready + parked` queues the stable id;
/// `ready + claimed` queues that same id again, which the durable ledger
/// deduplicates before the state machine acknowledges and removes the payload.
pub(crate) fn reconcile_prepared_checkouts(prep: &CheckoutPrep, doc_host: &DocHost) {
    for (worktree, record) in prep.ready_handoffs() {
        tracing::info!(worktree = %worktree.display(), "reconciling ready checkout handoff");
        settle_preparation(prep, doc_host, &worktree, &record);
    }
}

fn peek_parked(
    prep: &crate::checkout_prep::CheckoutPrep,
    worktree: &Path,
) -> Option<ParkedBrief> {
    let text = prep.parked(worktree)?;
    serde_json::from_str(&text).ok()
}

/// A line in the transcript from the engine rather than from either party.
/// Best-effort throughout: a doc that cannot be opened is not a reason to
/// abandon a checkout that prepared fine.
fn notice(doc_host: &DocHost, chat_id: &str, text: &str, is_error: bool) {
    match doc_host.open(chat_id) {
        Ok(handle) => {
            if let Err(e) = handle.write_notice(&crate::new_id(), text, is_error, crate::now_ms()) {
                tracing::warn!(chat = chat_id, error = %e, "writing a checkout notice");
            }
        }
        Err(e) => tracing::warn!(chat = chat_id, error = %e, "opening a chat for a notice"),
    }
}

impl Runtime for CometRuntime {
    fn verify_push_credentials(
        &self,
        repo: &str,
        contract: comet_proto::GithubPushContract,
    ) -> anyhow::Result<()> {
        self.push
            .get()
            .ok_or_else(|| anyhow::anyhow!("the engine has no board credential handoff resolver"))?
            .verify_for_repo(repo, contract)
    }

    fn dispatch(&self, spec: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
        match (spec.push_repo.as_deref(), spec.push_contract) {
            (None, None) | (Some(_), Some(_)) => {}
            _ => anyhow::bail!(
                "board-dispatched GitHub work has an inconsistent repository/contract tuple"
            ),
        }
        // The checkout first: a failure here leaves nothing behind to clean up.
        // That includes an unreachable origin — `create_worktree_on` fetches
        // `spec.base` before cutting and refuses rather than branching from a
        // stale local HEAD (gh#67). Typed as `RefusedBeforeCut` (gh#410): no
        // branch was cut and no chat made, so the board deletes the attempt
        // row it opened instead of burning an attempt on a pre-flight refusal.
        let cwd = if spec.worktree {
            self.handle
                .block_on(self.repos.create_worktree_on(
                    Path::new(&spec.repo_path),
                    &spec.branch,
                    &spec.base,
                ))
                .map_err(|e| anyhow::Error::new(RefusedBeforeCut(e.to_string())))?
                .path
        } else {
            spec.repo_path.clone()
        };

        // Fail the dispatch here if the account does not resolve, rather than
        // at the first run: an attempt whose chat exists but whose login does
        // not is a row somebody has to clean up, and the operator finds out
        // either way. Materializing now also means the dir is seeded before
        // the brief is queued, so the run never races the seeding.
        let config_dir = match spec.account.as_deref() {
            Some(account) => Some(
                self.accounts
                    .materialize(spec.harness, account)
                    .map_err(|e| anyhow::anyhow!("{e}"))?,
            ),
            // No account named: the child inherits the CLI's own config dir,
            // which on a single-login box is every dispatch there is. That dir
            // is the box user's `~/.claude` / `~/.codex`, so the write into it
            // has to be the marker-managed kind — and it is (gh#272).
            None => self.accounts.default_config_dir(spec.harness),
        };

        // The board's conventions in the file this runtime reads without being
        // asked (gh#272). Beside the skill for Claude, and instead of it for
        // Codex, which has no skill mechanism and until now learned the board
        // "by other means" — that is, not at all.
        //
        // Before the chat exists, like the materialize above, so the brief is
        // never queued against a config dir the run would reach first. Never
        // fatal: an attempt that can run is worth more than a file that could
        // not be written, and the brief itself still carries the essentials.
        if let Some(dir) = &config_dir {
            match comet_board::conventions::apply(dir, spec.harness, spec.agent_instructions) {
                Ok(Some(outcome)) if outcome.changed => {
                    tracing::info!(
                        path = %outcome.path.display(),
                        enabled = spec.agent_instructions,
                        "board conventions written into the runtime's instruction file"
                    );
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(
                    dir = %dir.display(),
                    error = %err,
                    "could not write the board conventions into the instruction file"
                ),
            }
        }

        let chat_id = crate::new_id();
        let config = ChatConfig {
            harness: spec.harness,
            model: spec.model.clone(),
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
            // On the chat, so every later turn in it — steers, review
            // deliveries, the operator typing into the same session — keeps
            // spending the account the dispatch chose.
            account: spec.account.clone(),
            // Likewise for the credential its pushes authenticate with
            // (gh#68): the fix for a review comment three days from now is a
            // new run in this chat, and it has to reach the same branch.
            push_repo: spec.push_repo.clone(),
            // The nonsecret grant promise made in the brief, so every later
            // turn and token mint can reject a weaker replacement credential.
            push_contract: spec.push_contract,
            // And whose name is on what it commits (gh#107) — same reasoning
            // again: that later fix should be by the same person as the first
            // commit, not by whoever the box is.
            git_author: spec.git_author.clone(),
            // And what its turns may spend before the run loop steps in
            // (gh#270). Same reasoning a third time: a later turn in this chat
            // is the same unattended agent, working under the same policy.
            turn_limits: spec.turn_limits,
            // The route's MCP tools (gh#273), on the chat so review follow-ups
            // receive the same capabilities as the original attempt.
            mcp_servers: spec.mcp_servers.clone(),
        };
        self.workspace
            .create_chat(&chat_id, &spec.space_id, Some(config), Some(cwd.clone()))?;
        // Identity a human can read in the sidebar; the branch is the sub-line.
        // Identifier AND title (`gh#25 · D1 Prototype v1`): a shelf of bare
        // identifiers is a list you have to look every row up to read.
        self.workspace.rename_chat(&chat_id, &spec.chat_title())?;
        self.workspace.set_chat_branch(&chat_id, &spec.branch)?;

        // The brief, as a durable first send. `queue_command` returns once the
        // entry is in the session doc — the ledger guarantees delivery, which
        // is the property herdr-board's nudge-and-verify loop approximated.
        // `prompt_at` resolves `{worktree}` now that the checkout exists.
        let brief = SessionCommandPayload::Run {
            request: RunRequest {
                prompt: spec.prompt_at(&cwd),
                model: spec.model.clone(),
                reasoning: None,
                model_options: Default::default(),
                cwd: cwd.clone(),
                sandbox: SandboxLevel::WorkspaceWrite,
                auto_approve: false,
                resume: None,
                attachments: Vec::new(),
            },
            message_id: crate::new_id(),
        };
        // The board dispatches on the TEAM's behalf, so its chats are the one
        // kind that is org-visible (gh#66): every member may open the
        // transcript and steer the agent, while private chats on the same box
        // stay private. Best-effort — a chat that failed to share is still a
        // running attempt, just one only its owner can open.
        //
        // Before the brief since gh#422, because the brief may now be held
        // back for a minute while the checkout prepares, and the transcript
        // saying so is no use to a teammate who cannot open it.
        self.doc_host.share_chat(&chat_id);
        let preparation = match self.park_for_preparation(&chat_id, &cwd, brief.clone()) {
            Ok(preparation) => preparation,
            Err(error) => {
                // Parking is the recovery guarantee. If it cannot be made
                // durable, undo the chat and checkout that were just cut
                // rather than leave an attempt which can neither start nor
                // recover.
                let _ = self.workspace.delete_chat(&chat_id);
                self.doc_host.purge_chat(&chat_id);
                if spec.worktree
                    && let Err(rollback) = self.handle.block_on(self.repos.delete_worktree(
                        Path::new(&spec.repo_path),
                        Path::new(&cwd),
                        Some(&spec.branch),
                    ))
                {
                    return Err(anyhow::anyhow!(
                        "{error}; rolling back {} also failed: {rollback}",
                        cwd
                    ));
                }
                if spec.worktree {
                    self.prep.forget(Path::new(&cwd));
                }
                return Err(error);
            }
        };
        let preparation_view = preparation.as_ref().map(|_| {
            comet_proto::view::board::CheckoutPreparation {
                state: comet_proto::view::board::CheckoutPreparationState::Preparing,
                recipe_digest: None,
                execution_digest: None,
                detail: Some("preparing checkout before the agent starts".to_string()),
                log: None,
                log_excerpt: None,
                run_command: None,
                requires_approval: false,
                projections: Vec::new(),
            }
        });
        if let Some(repository_id) = preparation {
            self.start_preparation(&chat_id, &cwd, repository_id);
        } else {
            self.doc_host.queue_command(&chat_id, brief)?;
        }
        Ok(DispatchHandle {
            chat_id,
            cwd,
            preparation: preparation_view,
        })
    }

    fn checkout_preparation(
        &self,
        worktree: &str,
    ) -> anyhow::Result<Option<comet_proto::view::board::CheckoutPreparation>> {
        Ok(self.prep.status(Path::new(worktree)).map(|record| {
            let excerpt = record
                .log
                .as_ref()
                .and_then(|_| self.prep.log_head(Path::new(worktree), 8 * 1024));
            record.board_view(excerpt)
        }))
    }

    fn retry_checkout_preparation(&self, worktree: &str) -> anyhow::Result<()> {
        let parked = peek_parked(&self.prep, Path::new(worktree))
            .ok_or_else(|| anyhow::anyhow!("{} has no parked dispatch to release", worktree))?;
        let repository_id = self
            .handle
            .block_on(self.repos.repository_identity(Path::new(worktree)))
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        anyhow::ensure!(
            repository_id == parked.repository_id,
            "{} no longer belongs to the repository that parked this dispatch",
            worktree
        );
        self.start_preparation(&parked.chat_id, worktree, repository_id);
        Ok(())
    }

    fn approve_checkout_preparation(&self, worktree: &str) -> anyhow::Result<()> {
        let parked = peek_parked(&self.prep, Path::new(worktree))
            .ok_or_else(|| anyhow::anyhow!("{} has no parked dispatch to approve", worktree))?;
        let repository_id = self
            .handle
            .block_on(self.repos.repository_identity(Path::new(worktree)))
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        anyhow::ensure!(
            repository_id == parked.repository_id,
            "{} no longer belongs to the repository that parked this dispatch",
            worktree
        );
        let digest = self
            .prep
            .approve(Path::new(worktree), &repository_id)
            .map_err(|error| anyhow::anyhow!(error))?;
        notice(
            &self.doc_host,
            &parked.chat_id,
            &format!(
                "Approved repository preparation recipe {} on this host; retrying in the existing checkout.",
                &digest[..digest.len().min(12)]
            ),
            false,
        );
        self.start_preparation(&parked.chat_id, worktree, repository_id);
        Ok(())
    }

    fn prompt(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
        // Refuse before the durable command (and therefore before the user
        // message) exists. The command executor checks again because prompts
        // can also arrive from another viewport without going through board.
        self.verify_chat_push_credentials(chat_id)?;
        // Steer a live run, send otherwise — the same split the composers make.
        // AwaitingInput steers too: the ledger's supersede rules queue it
        // behind the pending question instead of starting a second run.
        let live = matches!(
            self.session(chat_id)?.map(|s| s.status),
            Some(SessionStatus::Working) | Some(SessionStatus::AwaitingInput)
        );
        let payload = if live {
            SessionCommandPayload::Steer {
                prompt: text.to_string(),
                message_id: Some(crate::new_id()),
            }
        } else {
            let chat = self
                .workspace
                .doc()
                .chat(chat_id)
                .ok()
                .flatten()
                .ok_or_else(|| anyhow::anyhow!("chat {chat_id} is gone"))?;
            let config = self.chat_config(chat_id);
            SessionCommandPayload::Run {
                request: RunRequest {
                    prompt: text.to_string(),
                    model: config.as_ref().and_then(|c| c.model.clone()),
                    reasoning: config.as_ref().and_then(|c| c.reasoning),
                    model_options: config
                        .as_ref()
                        .map(|c| c.model_options.clone())
                        .unwrap_or_default(),
                    cwd: chat.cwd.unwrap_or_default(),
                    sandbox: config
                        .as_ref()
                        .map(|c| c.sandbox)
                        .unwrap_or(SandboxLevel::WorkspaceWrite),
                    auto_approve: false,
                    // Resume continuity is engine-owned; guessing here would
                    // fight the `harnessSessionId` it stamps on the chat row.
                    resume: None,
                    attachments: Vec::new(),
                },
                message_id: crate::new_id(),
            }
        };
        self.doc_host.queue_command(chat_id, payload)?;
        Ok(())
    }

    fn cancel(&self, chat_id: &str) -> anyhow::Result<()> {
        // Interrupt is durable and idempotent; an idle chat resolves it as a
        // no-op. Archive regardless — cancel ends the attempt either way.
        self.doc_host
            .queue_command(chat_id, SessionCommandPayload::Interrupt {})?;
        self.workspace.set_chat_archived(chat_id, true)?;
        Ok(())
    }

    fn cancel_checkout_preparation(&self, worktree: &str) -> anyhow::Result<()> {
        let worktree = Path::new(worktree);
        // The tombstone is the fail-closed half: if it cannot be made durable,
        // report failure and leave the attempt live. Closing it while a late
        // setup can still release would start work after cancellation.
        self.prep.revoke_parked(worktree).map_err(|error| {
            anyhow::anyhow!(
                "revoking checkout preparation at {}: {error}",
                worktree.display()
            )
        })?;
        self.prep.cancel(worktree);
        Ok(())
    }

    /// Move a chat on or off its space's shelf (gh#139) — the same workspace
    /// mutation the sidebar's own Archive writes, so every surface watching the
    /// doc updates without the board telling any of them.
    fn set_chat_archived(&self, chat_id: &str, archived: bool) -> anyhow::Result<()> {
        // `false` is a chat the doc no longer has — deleted by hand, or on a
        // device that has gone. It is off every shelf already, so the verb has
        // done what it was asked; only a real mutation failure is an error.
        self.workspace.set_chat_archived(chat_id, archived)?;
        Ok(())
    }

    fn session(&self, chat_id: &str) -> anyhow::Result<Option<Session>> {
        Ok(self
            .sessions
            .borrow()
            .iter()
            .find(|s| s.chat_id == chat_id)
            .cloned())
    }

    fn chat_alive(&self, chat_id: &str) -> anyhow::Result<bool> {
        Ok(self
            .workspace
            .doc()
            .chat(chat_id)?
            .is_some_and(|chat| !chat.archived))
    }

    /// The engine's own revival ledger, read for the board's budget (gh#392).
    ///
    /// Off the same journal directory every other fact here comes from, and
    /// keyed the same way — which is what makes the answer trustworthy: this
    /// process is the one that wrote it, in `Sessions::recover_stale`, on this
    /// device's boot. A chat with no journal here is one this device has never
    /// run, and answers `None` rather than a zero the board would count.
    fn chat_revivals(&self, chat_id: &str) -> anyhow::Result<Option<i64>> {
        Ok(self.journal.revivals(chat_id).map(i64::from))
    }

    fn chat_cwd(&self, chat_id: &str) -> anyhow::Result<Option<String>> {
        Ok(self.workspace.doc().chat(chat_id)?.and_then(|c| c.cwd))
    }

    fn review_candidates(&self) -> anyhow::Result<Vec<ReviewCandidate>> {
        let chats = self.workspace.doc().read_chats()?;
        let spaces = self.workspace.doc().read_spaces()?;
        let local_device = self.workspace.device_id();
        Ok(chats
            .into_iter()
            .filter_map(|chat| {
                // A checkout path belongs to its host device. Keep the remote
                // chat as provenance, but never run local git against its path.
                let worktree = (chat.device_id == local_device)
                    .then(|| chat.cwd.as_deref().map(str::trim).map(str::to_string))
                    .flatten()
                    .filter(|path| !path.is_empty());
                let local = chat.device_id == local_device;
                let mut pull_request_urls = chat
                    .last_message_preview
                    .as_deref()
                    .map(github_pull_request_urls)
                    .unwrap_or_default();
                if local && let Ok(Some(text)) = self.journal.final_text(&chat.id) {
                    pull_request_urls.extend(github_pull_request_urls(&text));
                }
                pull_request_urls.sort();
                pull_request_urls.dedup();
                let branch = chat
                    .branch
                    .as_deref()
                    .map(str::trim)
                    .filter(|branch| !branch.is_empty() && *branch != "HEAD")
                    .map(str::to_string);
                let created_pull_request = local
                    && self
                        .journal
                        .commands(&chat.id)
                        .ok()
                        .flatten()
                        .is_some_and(|commands| commands.iter().any(ran_gh_pr_create));
                if branch.is_none() && pull_request_urls.is_empty() {
                    return None;
                }
                let repo = chat
                    .config
                    .as_ref()
                    .and_then(|config| config.push_repo.clone())
                    .or_else(|| {
                        worktree
                            .as_deref()
                            .and_then(comet_board::git_credentials::repo_for_checkout)
                    });
                let workspace = chat
                    .space_id
                    .as_deref()
                    .and_then(|id| spaces.iter().find(|space| space.id == id))
                    .map(|space| space.display_name().to_string())
                    .or_else(|| {
                        chat.cwd.as_deref().and_then(|path| {
                            std::path::Path::new(path)
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                        })
                    })
                    .unwrap_or_else(|| "Comet".into());
                let runtime = chat
                    .config
                    .as_ref()
                    .map(|config| comet_board::runtime::runtime_name(config.harness).to_string())
                    .unwrap_or_else(|| "agent".into());
                let account = chat.config.and_then(|config| config.account);
                Some(ReviewCandidate {
                    chat_id: chat.id,
                    workspace,
                    runtime,
                    worktree,
                    repo,
                    branch,
                    pull_request_urls,
                    created_pull_request,
                    account,
                    created_at: chat.created_at,
                })
            })
            .collect())
    }

    /// Hand a finished attempt's checkout back (gh#72): remove the worktree,
    /// prune the registration, delete the branch the board cut.
    ///
    /// The repo is what the board recorded at dispatch. An attempt from before
    /// that column existed has none, so it is derived from the checkout's own
    /// common git dir — which works precisely while the checkout is still
    /// there, and that is the case worth reclaiming. A checkout that is gone
    /// *and* whose repo is unknown leaves nothing to do but say so: there is no
    /// repo to run `branch -D` in.
    fn reclaim_worktree(
        &self,
        repo_path: Option<&str>,
        worktree: &str,
        branch: Option<&str>,
    ) -> anyhow::Result<()> {
        let worktree = Path::new(worktree);
        let repo = repo_path
            .map(std::path::PathBuf::from)
            .filter(|p| p.join(".git").exists())
            .or_else(|| repo_of_checkout(worktree))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no repo recorded for {} and none derivable from it — \
                     nothing to remove it from",
                    worktree.display()
                )
            })?;
        self.handle
            .block_on(self.repos.delete_worktree(&repo, worktree, branch))
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        // The preparation record is about a directory. Outliving it costs a
        // stale `ready` that would short-circuit the next checkout to land on
        // the same path — the state dir is keyed by path, and paths are reused
        // (`board/gh-422-…` cut again after a retry).
        self.prep.forget(worktree);
        Ok(())
    }

    /// Sweep the build output out of a finished attempt's checkout (gh#186),
    /// leaving the checkout and its branch exactly where they are.
    ///
    /// No git and no `Repos`: a `target/` is not a worktree registration, and
    /// removing one is a directory delete the board's own
    /// [`comet_board::gc::sweep_build_output`] does — which is where the walk's
    /// refusals (never `.git`, never a symlink, never below the depth bound) are
    /// stated and tested. This is here for the ownership rule rather than for the
    /// mechanism: the process that cut the worktrees is the one allowed to delete
    /// inside them, so a read-only board process keeps the clock and sweeps
    /// nothing.
    fn reclaim_build_output(&self, worktree: &str) -> anyhow::Result<comet_board::gc::Swept> {
        let path = Path::new(worktree);
        // The repository's own `[archive]` first (gh#422), then the sweep.
        //
        // In that order and not instead of it. `BUILD_OUTPUT_DIRS` is a list of
        // names the board knows; `[archive]` is what the *repository* knows —
        // the generated directory nothing outside it could have guessed, the
        // container it should stop, the cache its own tool is authoritative
        // about. Neither subsumes the other, and a repo that runs `cargo clean`
        // has simply left the sweep nothing to find in `target/`.
        //
        // Best-effort, because the caller is reclaiming a disk that is already
        // the problem: a `clean` that fails or hangs must not keep the 36 GB.
        match self.handle.block_on(self.repos.repository_identity(path)) {
            Ok(repository_id) => {
                if let Some(said) = self
                    .handle
                    .block_on(self.prep.archive(path, &repository_id, None))
                {
                    tracing::info!(worktree, outcome = %said, "checkout archive step");
                }
            }
            Err(error) => tracing::warn!(worktree, %error, "checkout archive identity"),
        }
        Ok(comet_board::gc::sweep_build_output(path))
    }

    /// Whose subscription a dispatch would spend (gh#101) — the slot's login,
    /// or this device's own when the dispatch names no slot.
    ///
    /// Infallible in practice: an unsaved slot, an unreadable config file and a
    /// harness with no account concept all answer `None`, which is the guard's
    /// "I cannot say" and never an accusation. The `Result` is the trait's, so
    /// a future runtime that really can fail has somewhere to say so.
    fn account_email(
        &self,
        harness: comet_proto::HarnessId,
        account: Option<&str>,
    ) -> anyhow::Result<Option<String>> {
        Ok(self.accounts.billed_email(harness, account))
    }

    /// What this box can actually run (gh#187) — the same answer
    /// `ListBoardRuntimes` gives its pickers, so what a dispatch refuses and
    /// what a picker warned about cannot drift apart.
    fn harness_availability(
        &self,
        harness: comet_proto::HarnessId,
        account: Option<&str>,
    ) -> anyhow::Result<Option<RuntimeUnavailable>> {
        Ok(crate::runtimes::availability(
            &self.accounts,
            harness,
            account,
        ))
    }

    fn last_run_end(&self, chat_id: &str) -> anyhow::Result<Option<RunEnd>> {
        // The journal's last event is a `Done` exactly when no run is live in
        // the chat — every teardown path writes one (including boot recovery,
        // which stamps a synthetic errored `Done` on a journal a crash left
        // open), and a new run's first events displace it.
        Ok(match self.journal.last_event(chat_id)? {
            Some((_, AgentEvent::Done { status, .. })) => Some(match status {
                DoneStatus::Completed => RunEnd::Completed,
                DoneStatus::Interrupted => RunEnd::Interrupted,
                DoneStatus::Errored => RunEnd::Errored,
            }),
            _ => None,
        })
    }

    /// The chat's tokens, off the same journal `last_run_end` reads (gh#151).
    /// The board calls this on every reconcile of a live attempt, so the scan
    /// filters lines by tag before parsing them — see [`RunJournal::tokens`].
    fn run_tokens(&self, chat_id: &str) -> anyhow::Result<Option<RunTokens>> {
        Ok(self.journal.tokens(chat_id)?.map(|t| RunTokens {
            usage: t.usage,
            model: t.model,
            by_model: t.by_model,
            by_agent: t.by_agent,
        }))
    }

    /// How full the chat's window is, off the same journal (gh#271). The last
    /// reading, never a sum — see [`RunJournal::context`].
    fn run_context(&self, chat_id: &str) -> anyhow::Result<Option<comet_proto::ContextUsage>> {
        Ok(self.journal.context(chat_id)?)
    }

    /// The chat's commands, off the same journal again (§gh#183) — the half of
    /// a review the agent did not write. See [`RunJournal::commands`].
    fn run_commands(&self, chat_id: &str) -> anyhow::Result<Option<Vec<RanCommand>>> {
        Ok(self.journal.commands(chat_id)?)
    }

    /// What sandbox the chat's last run actually got (§gh#349) — the terms the
    /// commands above ran under. See [`RunJournal::sandbox`].
    fn run_sandbox(&self, chat_id: &str) -> anyhow::Result<Option<comet_proto::SandboxReport>> {
        Ok(self.journal.sandbox(chat_id)?)
    }

    /// The tail of what the agent said, off the same journal once more
    /// (§gh#235) — where a finished attempt's claims block is, when it wrote
    /// one instead of running the verb. See [`RunJournal::final_text`].
    fn run_message(&self, chat_id: &str) -> anyhow::Result<Option<String>> {
        Ok(self.journal.final_text(chat_id)?)
    }
}

fn ran_gh_pr_create(command: &RanCommand) -> bool {
    !command.failed
        && command
            .command
            .split_whitespace()
            .collect::<Vec<_>>()
            .windows(3)
            .any(|words| words == ["gh", "pr", "create"])
}

/// Canonical PR URLs mentioned in one message. The workspace row only syncs a
/// bounded preview; the owning host also supplies the journal tail above.
fn github_pull_request_urls(text: &str) -> Vec<String> {
    text.match_indices("https://github.com/")
        .filter_map(|(start, _)| {
            let url = text[start..]
                .split(|ch: char| {
                    ch.is_whitespace() || matches!(ch, ')' | ']' | '>' | ',' | ';' | '"' | '\'')
                })
                .next()?
                .trim_end_matches('.');
            let rest = url.strip_prefix("https://github.com/")?;
            let mut parts = rest.split('/');
            let (Some(owner), Some(repo), Some("pull"), Some(number)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return None;
            };
            if owner.is_empty()
                || repo.is_empty()
                || number.parse::<u64>().is_err()
                || parts.next().is_some()
            {
                return None;
            }
            Some(format!("https://github.com/{owner}/{repo}/pull/{number}"))
        })
        .collect()
}

/// The repo a linked worktree belongs to, asked of the checkout itself.
///
/// The fallback for attempts dispatched before the board recorded the repo
/// (gh#72). A linked worktree's *common* git dir is the primary checkout's
/// `.git`, so its parent is the repo root — the same relationship
/// `crate::adopt`-side code reads to tell a worktree from a project.
pub(crate) fn repo_of_checkout(worktree: &Path) -> Option<std::path::PathBuf> {
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &worktree.to_string_lossy(),
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    common.parent().map(|p| p.to_path_buf())
}

#[cfg(test)]
mod review_candidate_tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn pull_request_provenance_reads_plain_and_markdown_urls_only() {
        assert_eq!(
            github_pull_request_urls(
                "Opened [the PR](https://github.com/o/r/pull/265). Issue: https://github.com/o/r/issues/1"
            ),
            vec!["https://github.com/o/r/pull/265"]
        );
    }

    #[test]
    fn pr_creation_is_a_command_signal_not_arbitrary_prose() {
        let successful = RanCommand {
            command: "env CI=1 gh pr create --fill".into(),
            failed: false,
        };
        assert!(ran_gh_pr_create(&successful));

        let failed = RanCommand {
            command: "gh pr create --fill".into(),
            failed: true,
        };
        assert!(!ran_gh_pr_create(&failed));
    }

    #[tokio::test]
    async fn startup_reconciles_ready_parked_and_abandoned_claimed_handoffs() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(repo.join(".comet")).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["config", "user.email", "prep@test.invalid"]);
        git(&repo, &["config", "user.name", "Checkout Prep Test"]);
        std::fs::write(
            repo.join(crate::checkout_prep::RECIPE_PATH),
            "version = 1\n",
        )
        .unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "fixture"]);

        let core = crate::EngineCore::assemble(
            &dir.path().join("data"),
            Arc::new(crate::HarnessRegistry::new()),
            comet_proto::HarnessId::Mock,
            None,
        )
        .unwrap();
        let prep = core.checkout_prep.clone();
        let repository_id = "startup-reconcile-repository";
        prep.approve(&repo, repository_id).unwrap();
        let record = prep
            .prepare(crate::checkout_prep::PrepareRequest {
                worktree: &repo,
                repository_id,
                force: false,
                cancel: None,
            })
            .await;
        assert!(record.is_ready());

        let payload = |chat: &str, command: &str| {
            serde_json::to_string(&ParkedBrief {
                chat_id: chat.to_string(),
                command_id: command.to_string(),
                repository_id: repository_id.to_string(),
                command: SessionCommandPayload::Interrupt {},
            })
            .unwrap()
        };

        prep.park(&repo, &payload("chat-ready-parked", "ready-parked"))
            .unwrap();
        reconcile_prepared_checkouts(&prep, &core.doc_host);
        assert!(prep.parked(&repo).is_none());
        assert!(core
            .doc_host
            .open("chat-ready-parked")
            .unwrap()
            .doc()
            .read_commands()
            .unwrap()
            .iter()
            .any(|command| command.id == "ready-parked"));

        prep.park(&repo, &payload("chat-ready-claimed", "ready-claimed"))
            .unwrap();
        let abandoned = prep.claim_parked(&repo).expect("claimed before crash");
        std::mem::forget(abandoned);
        let restarted = CheckoutPrep::new(&dir.path().join("data"));
        reconcile_prepared_checkouts(&restarted, &core.doc_host);
        assert!(restarted.parked(&repo).is_none());
        assert!(core
            .doc_host
            .open("chat-ready-claimed")
            .unwrap()
            .doc()
            .read_commands()
            .unwrap()
            .iter()
            .any(|command| command.id == "ready-claimed"));
    }
}
