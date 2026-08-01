//! `CometRuntime` — the board's [`Runtime`] trait implemented against engine
//! internals (docs/BOARD.md §H2).
//!
//! What herdr-board's `herdr.rs` drove over a mux socket, this does with the
//! primitives directly: worktrees via [`Repos`], chats via [`WorkspaceHost`],
//! prompts via the [`DocHost`] command ledger, agent state off the merged
//! session mirror. Every method is a thin verb; the board core owns the
//! decisions.
//!
//! The trait is sync on purpose (the board loop is a blocking thread), so the
//! async engine calls run through a captured runtime [`Handle`]. Methods must
//! therefore never be called from a tokio worker thread — in the engine they
//! run on the `comet-board-sync` thread only.

use std::path::Path;
use std::sync::Arc;

use comet_board::runtime::{DispatchHandle, DispatchSpec, RunEnd, Runtime};
use comet_doc::SessionCommandPayload;
use comet_proto::{
    AgentEvent, ChatConfig, DoneStatus, RunRequest, SandboxLevel, Session, SessionStatus,
};
use tokio::runtime::Handle;
use tokio::sync::watch;

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
    /// The engine's run journal — the settle authority (docs/BOARD.md §H4):
    /// `last_run_end` reads a chat's final journaled event off it.
    journal: Arc<RunJournal>,
    handle: Handle,
}

impl CometRuntime {
    pub fn new(
        repos: Repos,
        workspace: WorkspaceHost,
        doc_host: DocHost,
        sessions: watch::Receiver<Vec<Session>>,
        journal: Arc<RunJournal>,
        handle: Handle,
    ) -> Self {
        Self {
            repos,
            workspace,
            doc_host,
            sessions,
            journal,
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
        let cwd = if spec.worktree {
            self.handle
                .block_on(
                    self.repos
                        .create_worktree_on(Path::new(&spec.repo_path), &spec.branch),
                )?
                .path
        } else {
            spec.repo_path.clone()
        };

        let chat_id = crate::new_id();
        let config = ChatConfig {
            harness: spec.harness,
            model: spec.model.clone(),
            reasoning: None,
            model_options: Default::default(),
            sandbox: SandboxLevel::WorkspaceWrite,
        };
        self.workspace
            .create_chat(&chat_id, &spec.space_id, Some(config), Some(cwd.clone()))?;
        // Identity a human can read in the sidebar; the branch is the sub-line.
        self.workspace.rename_chat(&chat_id, &spec.identifier)?;
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
}
