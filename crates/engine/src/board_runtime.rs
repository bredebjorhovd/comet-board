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
use std::sync::Arc;

use comet_board::evidence::RanCommand;
use comet_board::runtime::{
    DispatchHandle, DispatchSpec, ReviewCandidate, RunEnd, RunTokens, Runtime, RuntimeUnavailable,
};
use comet_doc::SessionCommandPayload;
use comet_proto::{
    AgentEvent, ChatConfig, DoneStatus, RunRequest, SandboxLevel, Session, SessionStatus,
};
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::agent_accounts::AgentAccounts;
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
        handle: Handle,
    ) -> Self {
        Self {
            repos,
            workspace,
            doc_host,
            sessions,
            journal,
            accounts,
            handle,
        }
    }

    fn chat_config(&self, chat_id: &str) -> Option<ChatConfig> {
        self.workspace.chat_config(chat_id)
    }
}

impl Runtime for CometRuntime {
    fn dispatch(&self, spec: &DispatchSpec) -> anyhow::Result<DispatchHandle> {
        // The checkout first: a failure here leaves nothing behind to clean up.
        // That includes an unreachable origin — `create_worktree_on` fetches
        // `spec.base` before cutting and refuses rather than branching from a
        // stale local HEAD (gh#67).
        let cwd = if spec.worktree {
            self.handle
                .block_on(self.repos.create_worktree_on(
                    Path::new(&spec.repo_path),
                    &spec.branch,
                    &spec.base,
                ))?
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
            // And whose name is on what it commits (gh#107) — same reasoning
            // again: that later fix should be by the same person as the first
            // commit, not by whoever the box is.
            git_author: spec.git_author.clone(),
            // And what its turns may spend before the run loop steps in
            // (gh#270). Same reasoning a third time: a later turn in this chat
            // is the same unattended agent, working under the same policy.
            turn_limits: spec.turn_limits,
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
        self.doc_host.queue_command(
            &chat_id,
            SessionCommandPayload::Run {
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
            },
        )?;
        // The board dispatches on the TEAM's behalf, so its chats are the one
        // kind that is org-visible (gh#66): every member may open the
        // transcript and steer the agent, while private chats on the same box
        // stay private. Best-effort — a chat that failed to share is still a
        // running attempt, just one only its owner can open.
        self.doc_host.share_chat(&chat_id);
        Ok(DispatchHandle { chat_id, cwd })
    }

    fn prompt(&self, chat_id: &str, text: &str) -> anyhow::Result<()> {
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
            .map_err(|e| anyhow::anyhow!("{e}"))
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
        Ok(comet_board::gc::sweep_build_output(Path::new(worktree)))
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

#[cfg(test)]
mod review_candidate_tests {
    use super::*;

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
}

/// The repo a linked worktree belongs to, asked of the checkout itself.
///
/// The fallback for attempts dispatched before the board recorded the repo
/// (gh#72). A linked worktree's *common* git dir is the primary checkout's
/// `.git`, so its parent is the repo root — the same relationship
/// `crate::adopt`-side code reads to tell a worktree from a project.
fn repo_of_checkout(worktree: &Path) -> Option<std::path::PathBuf> {
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
