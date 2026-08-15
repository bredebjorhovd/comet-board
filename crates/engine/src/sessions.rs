//! SessionsEngine — per-chat agent runs: dispatch, steering, interrupts, input bridging,
//! journal + broadcast fan-out, and 120ms coalesced doc streaming.
//!
//! Pragmatic port of comet's `sessions.ts` (spec: feature-inventory §3.2):
//! - every `AgentEvent` is (a) appended to the on-disk run journal, (b) broadcast to
//!   in-process subscribers, (c) folded via `fold_event_into_parts` and diffed into the
//!   chat's `SessionDoc` through `SegmentWriter` on a coalesced `STREAM_COMMIT_MS` timer;
//! - the user message entry is pushed to the doc immediately on dispatch (id = the
//!   command's client-minted message id, so optimistic echoes never flicker);
//! - a `Steered` event splits the assistant entry at the exact boundary;
//! - recovery (interrupt or a stale journal at boot) stamps the streaming entry `aborted`.
//!
//! Scope notes: sessions are keyed by chat id (one live run per chat). Comet's pulse
//! loop is ported as the 15s liveness heartbeat in `drive_run`, and its `STALL_MS`
//! watchdog is ported in TIERED form (see `drive_run`): terminal only for a run that
//! never produced anything at all, advisory for silence after real output. The
//! blanket "10 minutes quiet = kill it" version stays rejected (review: agents may
//! legitimately wait on something for far longer than any timeout, and a live child
//! that has already spoken IS the working signal). Every dying path must still carry
//! its own visible error (child crash with stderr, spawn failure, stream error,
//! engine-restart recovery, startup stall).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use chrono::Utc;
use futures::StreamExt;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use comet_doc::{
    DocError, MessagePart, MessageRole, MessageStatus, STREAM_COMMIT_MS, SegmentWriter, SessionDoc,
    fold_event_into_parts, sanitize_tool_call,
};
use comet_harness::{CancellationToken, Harness, RunControls, SteerMessage};
use comet_proto::{
    AgentEvent, DoneStatus, HarnessId, RunRequest, Session, SessionStatus, UserInputAnswer,
    UserInputQuestion,
};

use crate::doc_host::{ChatDocHandle, DocHost};
use crate::registry::HarnessRegistry;
use crate::run_journal::RunJournal;
use crate::{EngineError, new_id, now_ms};

/// One journaled event: the durable seq plus the event, as broadcast to subscribers.
#[derive(Debug, Clone)]
pub struct JournaledEvent {
    pub seq: u64,
    pub event: AgentEvent,
}

/// Outcome of a steer attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerOutcome {
    /// Delivered into the live run's steering mailbox.
    Accepted,
    /// No live steerable run — the caller should dispatch the prompt as a new turn.
    NotSteerable,
}

type PendingInputs = Arc<Mutex<HashMap<String, oneshot::Sender<Vec<UserInputAnswer>>>>>;

/// A harness-native session id plus the cwd it was created under. Harness
/// session stores are cwd-scoped (claude keys conversations by project
/// directory — comet sessions.ts:563 "harness session stores are keyed by
/// cwd"), so resume is only injected for runs launched from the same cwd.
#[derive(Debug, Clone)]
struct HarnessSessionRef {
    session_id: String,
    cwd: String,
}

struct RunHandle {
    run_id: String,
    steerable: bool,
    steer_tx: mpsc::Sender<SteerMessage>,
    /// Harness-level cancellation (protocol interrupt + child teardown).
    interrupt_token: CancellationToken,
    /// Engine-level cancel: arms the run task's grace deadline so a harness that
    /// ignores its token can never strand the run.
    cancel: watch::Sender<bool>,
    engine_tx: mpsc::UnboundedSender<AgentEvent>,
    pending_inputs: PendingInputs,
}

struct Inner {
    device_id: String,
    journal: Arc<RunJournal>,
    registry: Arc<HarnessRegistry>,
    doc_host: OnceLock<DocHost>,
    /// chat_id → live run.
    runs: Mutex<HashMap<String, RunHandle>>,
    /// chat_id → broadcast hub (retained across runs so subscribers survive turns).
    hubs: Mutex<HashMap<String, broadcast::Sender<JournaledEvent>>>,
    statuses: Mutex<HashMap<String, Session>>,
    sessions_tx: watch::Sender<Vec<Session>>,
    /// Last dispatched request per chat — the steer→new-turn fallback re-derives its
    /// run config from this (chat config rows land with the workspace doc in M4).
    last_requests: Mutex<HashMap<String, RunRequest>>,
    /// Harness-native session ids per chat (resume continuity across turns) —
    /// the live-process cache over the durable copy on the workspace chat row
    /// (comet kept the same pair on `chats.harness_session_id`). An empty
    /// session id is the "do not resume" tombstone after a rejected resume.
    harness_sessions: Mutex<HashMap<String, HarnessSessionRef>>,
    /// Auto-titler for untitled chats (wired at engine assembly; absent in bare tests).
    titles: OnceLock<crate::titles::TitleGenerator>,
    /// The device's saved agent logins (wired at engine assembly; absent in
    /// bare tests). A chat carrying an `account` has its slot materialized
    /// into a config dir of its own, and the harness child is pointed at that
    /// instead of the shared `~/.claude` / `~/.codex` — gh#59.
    accounts: OnceLock<crate::agent_accounts::AgentAccounts>,
    /// The board's GitHub credential, as something a run can push with (wired
    /// only when the board is enabled; absent everywhere else). A chat the
    /// board dispatched carries the repo it works on, and the harness child is
    /// pointed at `comet-board`'s askpass helper for it instead of at the box
    /// user's git credentials — gh#68.
    push: OnceLock<Arc<crate::push_credentials::PushCredentials>>,
    /// The directory holding the `comet-board` this engine shipped with, put on
    /// every harness child's PATH so an agent can actually run the verbs its
    /// skill hands it — gh#184. Resolved once here rather than per run: it is a
    /// property of how this process was installed, and it cannot change while
    /// the process lives. Empty when there is no `comet-board` beside the
    /// engine, which leaves the child's PATH untouched.
    agent_bin_dirs: Vec<std::path::PathBuf>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct SessionsEngine {
    inner: Arc<Inner>,
}

impl SessionsEngine {
    pub fn new(
        device_id: String,
        journal: Arc<RunJournal>,
        registry: Arc<HarnessRegistry>,
    ) -> Self {
        let (sessions_tx, _) = watch::channel(Vec::new());
        Self {
            inner: Arc::new(Inner {
                device_id,
                journal,
                registry,
                doc_host: OnceLock::new(),
                runs: Mutex::new(HashMap::new()),
                hubs: Mutex::new(HashMap::new()),
                statuses: Mutex::new(HashMap::new()),
                sessions_tx,
                last_requests: Mutex::new(HashMap::new()),
                harness_sessions: Mutex::new(HashMap::new()),
                titles: OnceLock::new(),
                accounts: OnceLock::new(),
                push: OnceLock::new(),
                agent_bin_dirs: comet_board::git_credentials::agent_bin_dir()
                    .into_iter()
                    .collect(),
            }),
        }
    }

    /// The run journal this engine appends to — the board's settle authority
    /// (§settle-logic): `CometRuntime::last_run_end` reads it.
    pub fn journal(&self) -> Arc<RunJournal> {
        self.inner.journal.clone()
    }

    /// Wire the doc host (called once at engine assembly; the two services are mutually
    /// referential by design — sessions stream into docs, docs execute commands here).
    pub fn set_doc_host(&self, host: DocHost) {
        let _ = self.inner.doc_host.set(host);
    }

    /// Wire the chat auto-titler (called once at engine assembly). After each
    /// completed exchange the run task fires it for still-untitled chats.
    pub fn set_titles(&self, titles: crate::titles::TitleGenerator) {
        let _ = self.inner.titles.set(titles);
    }

    /// Wire the agent-account store (called once at engine assembly). Without
    /// it every run uses the CLI's own config dir, which is the single-account
    /// behavior that predates gh#59.
    pub fn set_accounts(&self, accounts: crate::agent_accounts::AgentAccounts) {
        let _ = self.inner.accounts.set(accounts);
    }

    /// Wire the board's push credentials (called at engine assembly, only when
    /// the board is on). Without it — and for every chat the board did not
    /// dispatch — an agent pushes with the device's own git credentials, which
    /// is the behavior that predates gh#68.
    pub fn set_push_credentials(&self, push: Arc<crate::push_credentials::PushCredentials>) {
        let _ = self.inner.push.set(push);
    }

    fn doc_handle(&self, chat_id: &str) -> Result<Arc<ChatDocHandle>, EngineError> {
        let host =
            self.inner.doc_host.get().ok_or_else(|| {
                EngineError::Other("doc host not wired into sessions engine".into())
            })?;
        host.open(chat_id)
    }

    /// Status watch: the full session list, re-sent on every transition.
    pub fn watch_sessions(&self) -> watch::Receiver<Vec<Session>> {
        self.inner.sessions_tx.subscribe()
    }

    pub fn session_status(&self, chat_id: &str) -> Option<Session> {
        lock(&self.inner.statuses).get(chat_id).cloned()
    }

    /// Any run currently working or blocked on input — the auto-updater's
    /// "don't restart from under a session" gate.
    pub fn any_active(&self) -> bool {
        lock(&self.inner.statuses).values().any(|s| {
            matches!(
                s.status,
                comet_proto::SessionStatus::Working | comet_proto::SessionStatus::AwaitingInput
            )
        })
    }

    /// Is a run live on this chat — dispatched and not settled, or holding an
    /// open question?
    ///
    /// The doc host's release gate (gh#395): a run streams through an
    /// `Arc<SessionDoc>` of its own rather than the chat handle, so the handle
    /// cache cannot see it, and releasing the chat under it would drop the room
    /// socket the turn is syncing through. The `runs` map first because a
    /// dispatch is live there before its status transition lands.
    pub fn chat_is_busy(&self, chat_id: &str) -> bool {
        if lock(&self.inner.runs).contains_key(chat_id) {
            return true;
        }
        matches!(
            lock(&self.inner.statuses).get(chat_id).map(|s| s.status),
            Some(SessionStatus::Working | SessionStatus::AwaitingInput)
        )
    }

    /// The last request dispatched for a chat (steer→new-turn fallback).
    pub fn last_request(&self, chat_id: &str) -> Option<RunRequest> {
        lock(&self.inner.last_requests).get(chat_id).cloned()
    }

    /// Subscribe to a chat's live event stream: returns the journal replay after
    /// `after_seq` plus a live receiver. Subscribe-then-replay ordering means overlap
    /// (dedupe by seq) rather than gaps.
    pub fn subscribe(
        &self,
        chat_id: &str,
        after_seq: u64,
    ) -> Result<(Vec<JournaledEvent>, broadcast::Receiver<JournaledEvent>), EngineError> {
        let rx = {
            let mut hubs = lock(&self.inner.hubs);
            hubs.entry(chat_id.to_string())
                .or_insert_with(|| broadcast::channel(1024).0)
                .subscribe()
        };
        let replay = self
            .inner
            .journal
            .replay(chat_id, after_seq)?
            .into_iter()
            .map(|(seq, event)| JournaledEvent { seq, event })
            .collect();
        Ok((replay, rx))
    }

    /// Start (or route) a run for `chat_id`.
    ///
    /// - The user message entry is written to the doc immediately (id = `message_id`).
    /// - A live steerable run receives the prompt as its next turn via the mailbox
    ///   (comet's persistent-session routing); otherwise any live run is interrupted
    ///   first — never two runtimes driving one chat.
    pub async fn dispatch(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
    ) -> Result<String, EngineError> {
        self.dispatch_with(chat_id, harness_id, request, message_id, true)
            .await
    }

    /// [`Self::dispatch`] with resume injection controllable: the failed-resume
    /// retry re-dispatches with `inject_resume = false` so a session id the
    /// harness just rejected can never be re-injected from the journal.
    /// Boxed future: `drive_run` re-enters this for that retry, and the
    /// erasure breaks the opaque-type cycle the recursion would otherwise form.
    fn dispatch_with<'a>(
        &'a self,
        chat_id: &'a str,
        harness_id: HarnessId,
        request: RunRequest,
        message_id: Option<String>,
        inject_resume: bool,
    ) -> futures::future::BoxFuture<'a, Result<String, EngineError>> {
        Box::pin(self.dispatch_inner(chat_id, harness_id, request, message_id, inject_resume))
    }

    async fn dispatch_inner(
        &self,
        chat_id: &str,
        harness_id: HarnessId,
        mut request: RunRequest,
        message_id: Option<String>,
        inject_resume: bool,
    ) -> Result<String, EngineError> {
        let routed = lock(&self.inner.runs)
            .get(chat_id)
            .map(|h| (h.run_id.clone(), h.steerable, h.steer_tx.clone()));
        if let Some((run_id, steerable, steer_tx)) = routed {
            let message = SteerMessage {
                prompt: request.prompt.clone(),
                message_id: message_id.clone(),
            };
            if steerable && steer_tx.try_send(message).is_ok() {
                let user_id = message_id.unwrap_or_else(new_id);
                let handle = self.doc_handle(chat_id)?;
                handle.write_user_message(&user_id, &request.prompt, now_ms())?;
                // Working BEFORE the lastMessageAt bump: both ride the
                // workspace doc from this one peer, so causal order makes it
                // impossible for an observer to hold [new message, old status]
                // — that gap read as unseen-with-no-live-run = a phantom
                // "completed" flash on every remote send (2026-07-31).
                self.set_status(chat_id, SessionStatus::Working, false);
                self.inner.note_message(chat_id, &request.prompt);
                return Ok(run_id);
            }
            // Mailbox closed (runtime mid-teardown / non-steering harness): replace it.
            self.interrupt(chat_id).await?;
        }

        let harness = self.inner.registry.resolve(harness_id)?;

        // Whose subscription this run spends (gh#59). Resolved per run from the
        // chat row, not carried on the request: the account is the agent's, and
        // a steer arriving mid-turn must not be able to change it. A named
        // account that will not resolve REFUSES the run — falling back to the
        // shared login would quietly bill the wrong person. Before the user
        // message is written, alongside harness resolution, so a refusal leaves
        // no prompt sitting in the transcript with nothing answering it.
        let (account, account_lease) = self.inner.account_for(chat_id, harness_id)?;

        let handle = self.doc_handle(chat_id)?;
        let user_id = message_id.unwrap_or_else(new_id);
        handle.write_user_message(&user_id, &request.prompt, now_ms())?;

        // Engine-owned resume (comet sessions.ts:736 — every dispatch read the
        // chat's stored harness session): callers always send `resume: None`;
        // the engine threads the chat's prior harness session back in so a new
        // process (app restart) continues the same harness conversation.
        let mut resume_injected = false;
        if request.resume.is_none() && inject_resume {
            request.resume = self.inner.resume_for(chat_id, &request.cwd);
            resume_injected = request.resume.is_some();
        }
        lock(&self.inner.last_requests).insert(chat_id.to_string(), request.clone());

        let run_id = new_id();
        let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(32);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let (engine_tx, engine_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let pending_inputs: PendingInputs = Arc::new(Mutex::new(HashMap::new()));

        // Input bridge: the harness asks questions; we mint the request id, park the
        // resolver for `respond_input`, and surface the event through the run pipeline.
        let request_input = {
            let pending = pending_inputs.clone();
            let engine_tx = engine_tx.clone();
            Box::new(move |questions: Vec<UserInputQuestion>| {
                let (tx, rx) = oneshot::channel();
                let request_id = new_id();
                lock(&pending).insert(request_id.clone(), tx);
                let _ = engine_tx.send(AgentEvent::InputRequested {
                    request_id,
                    questions,
                });
                rx
            })
        };
        let interrupt_token = CancellationToken::new();
        let controls = RunControls {
            request_input,
            steering: steer_rx,
            interrupt: interrupt_token.clone(),
            chat_id: Some(chat_id.to_string()),
            account,
            push: self.inner.push_for(chat_id),
            bin_dirs: self.inner.agent_bin_dirs.clone(),
        };

        lock(&self.inner.runs).insert(
            chat_id.to_string(),
            RunHandle {
                run_id: run_id.clone(),
                steerable: harness.supports_steering(),
                steer_tx,
                interrupt_token,
                cancel: cancel_tx,
                engine_tx,
                pending_inputs,
            },
        );
        self.set_status(chat_id, SessionStatus::Working, true);
        // AFTER Working (same causal-order guarantee as the steer path): the
        // lastMessageAt bump must never be observable ahead of the live run.
        self.inner.note_message(chat_id, &request.prompt);

        // Name the chat NOW, off the first prompt — not after the first
        // exchange completes ("called New session for a long time for no
        // reason"; the titler only needs the prompt and skips titled chats;
        // the Done-time call below stays as the retry for a failed
        // generation).
        if let Some(titles) = self.inner.titles.get() {
            titles.maybe_generate(chat_id, harness_id, &request.prompt, &request.cwd);
        }

        tokio::spawn(drive_run(
            self.inner.clone(),
            chat_id.to_string(),
            run_id.clone(),
            harness,
            request,
            handle.doc_arc(),
            controls,
            engine_rx,
            cancel_rx,
            RunResumeState {
                user_message_id: user_id,
                resume_injected,
            },
            account_lease,
        ));
        Ok(run_id)
    }

    /// Push a steer prompt into the live run's mailbox. `NotSteerable` when no live
    /// steerable run exists — the caller (command executor) dispatches a new turn.
    pub async fn steer(
        &self,
        chat_id: &str,
        prompt: &str,
        message_id: Option<String>,
    ) -> Result<SteerOutcome, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .filter(|h| h.steerable)
            .map(|h| h.steer_tx.clone());
        let Some(steer_tx) = target else {
            return Ok(SteerOutcome::NotSteerable);
        };
        let message = SteerMessage {
            prompt: prompt.to_string(),
            message_id: message_id.clone(),
        };
        if steer_tx.try_send(message).is_err() {
            return Ok(SteerOutcome::NotSteerable);
        }
        // Accepted: the steer prompt becomes a user entry immediately (client-minted id).
        let user_id = message_id.unwrap_or_else(new_id);
        let handle = self.doc_handle(chat_id)?;
        handle.write_user_message(&user_id, prompt, now_ms())?;
        self.inner.note_message(chat_id, prompt);
        Ok(SteerOutcome::Accepted)
    }

    /// Interrupt the live run, if any. The run settles with a synthetic
    /// `Done{interrupted}` and its streaming entry stamped `aborted`; this waits
    /// (bounded) for that settlement so callers observe a consistent doc.
    pub async fn interrupt(&self, chat_id: &str) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs).get(chat_id).map(|h| {
            (
                h.run_id.clone(),
                h.interrupt_token.clone(),
                h.cancel.clone(),
                h.pending_inputs.clone(),
            )
        });
        let Some((run_id, token, cancel, pending)) = target else {
            return Ok(false);
        };
        // Tell the RUN TASK first (gh#111). Everything below this line — the
        // unparked resolver, the cancelled token — is a way of ending the
        // harness stream, and the run task classifies a stream that ends
        // without a `Done` by whether it knows an interrupt is in flight: it
        // is `Done{interrupted}` when `interrupted` is set and "harness stream
        // ended without Done" (an Errored run, a visible error part) when it
        // is not. A harness that reacts promptly — teardown on the token, or a
        // CLI that exits the moment its parked question resolves empty — can
        // close the stream before this watch is sent, and the run task's own
        // `biased` select then reads a clean stop as a crash. Only the
        // ordering here can close that window; sending first makes the flag
        // visible on the very next poll, ahead of any stream end it causes.
        //
        // This also arms the engine-side grace deadline in the run task, so a
        // harness that ignores its token still settles with a synthesized
        // Done{interrupted}.
        let _ = cancel.send(true);
        // Unpark any blocked question BEFORE the token (mirrors comet: harness teardown
        // can await a parked question callback — a run stuck on a question would
        // deadlock the stop).
        let parked: Vec<_> = lock(&pending).drain().map(|(_, tx)| tx).collect();
        for tx in parked {
            let _ = tx.send(Vec::new());
        }
        // Harness-level interrupt (protocol + child teardown).
        token.cancel();
        // Bounded settle wait (the run task appends Done + stamps `aborted`).
        for _ in 0..500 {
            if !self.is_live(chat_id, &run_id) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Ok(true)
    }

    /// Resolve a pending `request_input` question set. Returns `false` when no such
    /// request is pending (unknown id, or the run already settled).
    pub fn respond_input(
        &self,
        chat_id: &str,
        request_id: &str,
        answers: Vec<UserInputAnswer>,
    ) -> Result<bool, EngineError> {
        let target = lock(&self.inner.runs)
            .get(chat_id)
            .map(|h| (h.pending_inputs.clone(), h.engine_tx.clone()));
        let Some((pending, engine_tx)) = target else {
            return Ok(false);
        };
        let Some(resolver) = lock(&pending).remove(request_id) else {
            return Ok(false);
        };
        let _ = resolver.send(answers);
        let _ = engine_tx.send(AgentEvent::InputResolved {
            request_id: request_id.to_string(),
        });
        Ok(true)
    }

    /// Boot recovery: for every journal whose last event is not `Done` (a run died
    /// mid-stream), stamp this device's abandoned `streaming` doc entries `aborted`
    /// with a VISIBLE "Run interrupted by engine restart" error part, close the
    /// journal with a synthetic `Done{interrupted}` — and then PICK THE RUN BACK
    /// UP: a fresh crashed turn with revival budget left is re-dispatched against
    /// the remembered harness session (comet: "not just eulogized";
    /// `MAX_AUTO_RESUME` = 3 consecutive revivals, fresh = crashed < 12h ago).
    pub fn recover_stale(&self) -> Result<usize, EngineError> {
        const MAX_AUTO_RESUME: u32 = 3;
        const RESUME_FRESH_MS: i64 = 12 * 60 * 60 * 1000;

        let stale = self.inner.journal.stale_sessions()?;
        let mut recovered = 0usize;
        for chat_id in stale {
            if lock(&self.inner.runs).contains_key(&chat_id) {
                continue; // a live run owns this journal
            }
            let handle = self.doc_handle(&chat_id)?;
            // Harness continuity first: the crashed run's session id may only
            // exist in the journal (the debounced workspace-row write may
            // never have landed) — remember it so the revived run resumes the
            // same harness conversation (comet recoverDraft, sessions.ts:538).
            if let Some((session_id, cwd)) = self.inner.journal_harness_session(&chat_id) {
                self.inner
                    .remember_harness_session(&chat_id, &session_id, &cwd);
            }
            // The revival prompt: the last user message (idempotent re-dispatch
            // under the SAME id — `write_user_message` dedupes by id, so the
            // transcript never shows a duplicate).
            let prompt = handle.doc().read_entries().ok().and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|e| e.role == MessageRole::User)
                    .and_then(|e| {
                        e.parts.iter().find_map(|p| match p {
                            MessagePart::Text { text, .. } => Some((e.id.clone(), text.clone())),
                            _ => None,
                        })
                    })
            });
            let attempts = self.inner.journal.resume_attempts(&chat_id);
            let fresh = handle
                .doc()
                .read_entries()
                .ok()
                .and_then(|entries| {
                    entries
                        .iter()
                        .rev()
                        .find(|e| e.status == Some(MessageStatus::Streaming))
                        .map(|e| now_ms() - e.created_at < RESUME_FRESH_MS)
                })
                .unwrap_or(false);
            let will_resume = fresh && prompt.is_some() && attempts < MAX_AUTO_RESUME;

            let note = if will_resume {
                "Run interrupted by engine restart — resuming"
            } else {
                "Run interrupted by engine restart"
            };
            let done = AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: Some(note.into()),
                session_id: None,
            };
            self.inner.publish(&chat_id, &done);
            let stamped = handle.mark_abandoned_streams(note)?.len();
            self.set_status(&chat_id, SessionStatus::Idle, false);
            tracing::info!(chat = %chat_id, stamped, will_resume, attempts, "recovered stale session journal");
            recovered += 1;

            if !will_resume {
                continue;
            }
            let attempt = self.inner.journal.note_resume_attempt(&chat_id);
            let (user_id, prompt_text) = prompt.expect("gated by will_resume");
            let sessions = self.clone();
            tokio::spawn(async move {
                let Some(host) = sessions.inner.doc_host.get().cloned() else {
                    return;
                };
                let request = sessions
                    .last_request(&chat_id)
                    .or_else(|| host.request_from_chat_row(&chat_id, &prompt_text))
                    // Last resort: the journal's own cwd (comet's draft config)
                    // — a crash can predate the debounced workspace-row write.
                    .or_else(|| {
                        let (_, cwd) = sessions.inner.journal_harness_session(&chat_id)?;
                        Some(RunRequest {
                            prompt: String::new(),
                            model: None,
                            reasoning: None,
                            model_options: Default::default(),
                            cwd,
                            sandbox: comet_proto::SandboxLevel::WorkspaceWrite,
                            auto_approve: false,
                            attachments: Vec::new(),
                            resume: None,
                        })
                    });
                let Some(mut request) = request else {
                    tracing::warn!(chat = %chat_id, "auto-resume skipped: no run config");
                    return;
                };
                request.prompt = prompt_text;
                request.resume = None; // dispatch re-injects the remembered session
                request.attachments = Vec::new();
                let harness_id = host.harness_for(&chat_id);
                match sessions
                    .dispatch(&chat_id, harness_id, request, Some(user_id))
                    .await
                {
                    Ok(_) => {
                        tracing::info!(chat = %chat_id, attempt, "auto-resumed crashed run")
                    }
                    Err(err) => {
                        tracing::warn!(chat = %chat_id, error = %err, "auto-resume dispatch failed")
                    }
                }
            });
        }
        Ok(recovered)
    }

    /// Graceful shutdown: interrupt every live run so streaming entries settle.
    pub async fn shutdown(&self) {
        let chats: Vec<String> = lock(&self.inner.runs).keys().cloned().collect();
        for chat_id in chats {
            if let Err(err) = self.interrupt(&chat_id).await {
                tracing::warn!(chat = %chat_id, error = %err, "shutdown interrupt failed");
            }
        }
    }

    fn is_live(&self, chat_id: &str, run_id: &str) -> bool {
        lock(&self.inner.runs)
            .get(chat_id)
            .is_some_and(|h| h.run_id == run_id)
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        self.inner.set_status(chat_id, status, fresh_start);
    }
}

impl Inner {
    /// Journal + broadcast one event (the two unconditional legs of the pipeline).
    fn publish(&self, chat_id: &str, event: &AgentEvent) -> u64 {
        let seq = match self.journal.append(chat_id, event) {
            Ok(seq) => seq,
            Err(err) => {
                tracing::error!(chat = %chat_id, error = %err, "journal append failed");
                0
            }
        };
        if let Some(hub) = lock(&self.hubs).get(chat_id) {
            let _ = hub.send(JournaledEvent {
                seq,
                event: event.clone(),
            });
        }
        seq
    }

    /// Bump the session's freshness on stream activity WITHOUT a status
    /// transition. Long silent-LOOKING stretches (thinking heartbeats, a big
    /// tool input being generated) still carry events — the UI's 45s
    /// staleness gate must not flip "Working" off mid-run. Throttled: a
    /// workspace-doc mirror per delta would be far too chatty.
    fn touch_session(&self, chat_id: &str) {
        const TOUCH_THROTTLE_MS: i64 = 10_000;
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let Some(entry) = statuses.get_mut(chat_id) else {
                return;
            };
            let age = now
                .signed_duration_since(entry.updated_at)
                .num_milliseconds();
            if age < TOUCH_THROTTLE_MS {
                return;
            }
            entry.updated_at = now;
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            self.sessions_tx.send_replace(list);
            session
        };
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    fn set_status(&self, chat_id: &str, status: SessionStatus, fresh_start: bool) {
        let now = Utc::now();
        let session = {
            let mut statuses = lock(&self.statuses);
            let entry = statuses
                .entry(chat_id.to_string())
                .or_insert_with(|| Session {
                    chat_id: chat_id.to_string(),
                    device_id: self.device_id.clone(),
                    status,
                    started_at: None,
                    updated_at: now,
                });
            entry.status = status;
            entry.updated_at = now;
            if fresh_start {
                entry.started_at = Some(now);
            }
            let session = entry.clone();
            let mut list: Vec<Session> = statuses.values().cloned().collect();
            list.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
            // send_replace: keep the current value fresh even with no receivers,
            // so late WatchSessions subscribers see the last transition.
            self.sessions_tx.send_replace(list);
            session
        };
        // Mirror the transition into the workspace doc's session-status row so
        // remote devices' sidebars show this run (staleness-checked client-side).
        if let Some(ws) = self.workspace() {
            ws.record_session(&session);
        }
    }

    fn workspace(&self) -> Option<&crate::workspace_host::WorkspaceHost> {
        self.doc_host.get().and_then(|host| host.workspace())
    }

    /// The agent account a run in `chat_id` spends: the chat row's slot,
    /// materialized into a config dir the harness child is pointed at, plus the
    /// lease that keeps the usage refresher off its tokens while the run lives
    /// (gh#59).
    ///
    /// `(None, None)` — the shared CLI login — for a chat that names no
    /// account, and for one that does on an engine with no account store
    /// wired (bare tests). A named account that will not materialize is an
    /// error, never a fallback: this run would otherwise bill whoever the
    /// device's own login belongs to.
    #[allow(clippy::type_complexity)]
    fn account_for(
        &self,
        chat_id: &str,
        harness: HarnessId,
    ) -> Result<
        (
            Option<comet_harness::AgentAccount>,
            Option<crate::agent_accounts::AccountLease>,
        ),
        EngineError,
    > {
        let Some(accounts) = self.accounts.get() else {
            return Ok((None, None));
        };
        let account_id = self
            .workspace()
            .and_then(|ws| ws.chat_config(chat_id))
            .and_then(|c| c.account)
            .filter(|a| !a.is_empty());
        let Some(account_id) = account_id else {
            return Ok((None, None));
        };
        let dir = accounts.materialize(harness, &account_id)?;
        let lease = accounts.lease(&account_id);
        Ok((
            Some(comet_harness::AgentAccount {
                id: account_id,
                harness,
                dir,
            }),
            Some(lease),
        ))
    }

    /// What this chat's runs push with (gh#68), and whose name their commits
    /// carry (gh#107).
    ///
    /// Read off the chat row for the same reason the account is: both are
    /// properties of the attempt, and a review comment landing next week starts
    /// a new run in the same chat that has to be able to push the fix — under
    /// the same author as the first commit. A chat nobody dispatched carries
    /// neither and gets nothing, which leaves the agent on the box's own git
    /// credentials and identity.
    ///
    /// The two halves are independent on purpose. A board with an author for
    /// the dispatcher but no App credential still authors correctly (it just
    /// pushes as the box user), and a board with the credential but no `[users]`
    /// entry pushes as the App and commits as the box — which is what every
    /// board did before this.
    ///
    /// Never fatal, unlike a named account that will not resolve: a missing
    /// credential means the push falls back to what it used before, and
    /// refusing the run would take away the agent that could still do the work.
    fn push_for(&self, chat_id: &str) -> Option<comet_harness::PushCredentials> {
        let config = self.workspace().and_then(|ws| ws.chat_config(chat_id))?;
        let mut creds = config
            .push_repo
            .as_deref()
            .and_then(|repo| self.push.get()?.for_repo(repo, Some(chat_id)));
        let Some(author) = config.git_author.as_ref() else {
            return creds;
        };
        let env = comet_board::git_identity::author_env(author);
        match &mut creds {
            Some(creds) => creds.env.extend(env),
            None => {
                creds = Some(comet_harness::PushCredentials { env, bin_dir: None });
            }
        }
        creds
    }

    /// The turn-level guardrails this chat's runs are held to (gh#270).
    ///
    /// Read off the chat row for the reason the account and the credential are:
    /// the board resolved them from the route that released the attempt, and a
    /// steer arriving mid-turn must not be able to change what the run is held
    /// to. A chat nobody dispatched — and every chat that predates the field —
    /// carries [`comet_proto::TurnLimits::default`], which is off.
    fn turn_limits(&self, chat_id: &str) -> comet_proto::TurnLimits {
        self.workspace()
            .and_then(|ws| ws.chat_config(chat_id))
            .map(|c| c.turn_limits)
            .unwrap_or_default()
    }

    /// Sidebar freshness: push a message-persist preview into the chat's workspace row.
    fn note_message(&self, chat_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(ws) = self.workspace() {
            ws.note_message(chat_id, text);
        }
    }

    /// Record the chat's harness-native session id (and its cwd): live-process
    /// cache plus the durable workspace chat row — the row is what survives an
    /// engine restart (comet sessions.ts:1039).
    fn remember_harness_session(&self, chat_id: &str, session_id: &str, cwd: &str) {
        if session_id.is_empty() {
            return;
        }
        lock(&self.harness_sessions).insert(
            chat_id.to_string(),
            HarnessSessionRef {
                session_id: session_id.to_string(),
                cwd: cwd.to_string(),
            },
        );
        if let Some(ws) = self.workspace() {
            ws.set_chat_harness_session(chat_id, session_id, cwd);
        }
    }

    /// A harness rejected the stored session id: tombstone it (empty string on
    /// the row, cleared cache) so no lookup source — including the journal,
    /// which still names the dead id — can re-inject it.
    fn forget_harness_session(&self, chat_id: &str) {
        lock(&self.harness_sessions).insert(
            chat_id.to_string(),
            HarnessSessionRef {
                session_id: String::new(),
                cwd: String::new(),
            },
        );
        if let Some(ws) = self.workspace() {
            ws.set_chat_harness_session(chat_id, "", "");
        }
    }

    /// The session id to resume for a run in `chat_id` launching from `cwd`
    /// (comet sessions.ts:736, looked up on every dispatch):
    /// live-process cache → workspace chat row → journal scan (the crash path
    /// where the debounced row write never landed — SessionStarted/Done events
    /// are journaled per event, flushed immediately). Cwd-gated throughout:
    /// harness session stores are keyed by cwd, so a session created elsewhere
    /// never rides `--resume`. An empty stored id is the explicit tombstone —
    /// no resume, no falling through to staler sources.
    fn resume_for(&self, chat_id: &str, cwd: &str) -> Option<String> {
        let cwd_ok = |session_cwd: &str| session_cwd.is_empty() || session_cwd == cwd;
        if let Some(known) = lock(&self.harness_sessions).get(chat_id).cloned() {
            return (!known.session_id.is_empty() && cwd_ok(&known.cwd))
                .then_some(known.session_id);
        }
        if let Some(ws) = self.workspace()
            && let Some((session_id, session_cwd)) = ws.chat_harness_session(chat_id)
        {
            return (!session_id.is_empty() && cwd_ok(session_cwd.as_deref().unwrap_or("")))
                .then_some(session_id);
        }
        let (session_id, session_cwd) = self.journal_harness_session(chat_id)?;
        // Cache the journal hit (memory + row) so later dispatches skip the scan.
        self.remember_harness_session(chat_id, &session_id, &session_cwd);
        cwd_ok(&session_cwd).then_some(session_id)
    }

    /// The last harness session id named anywhere in the chat's journal, with
    /// the cwd of the `SessionStarted` that governs it. `Done.session_id`
    /// inherits the cwd of the most recent `SessionStarted` (same run).
    fn journal_harness_session(&self, chat_id: &str) -> Option<(String, String)> {
        let events = match self.journal.replay(chat_id, 0) {
            Ok(events) => events,
            Err(err) => {
                tracing::warn!(chat = %chat_id, error = %err, "journal scan for harness session failed");
                return None;
            }
        };
        let mut current_cwd = String::new();
        let mut found: Option<(String, String)> = None;
        for (_, event) in events {
            match event {
                AgentEvent::SessionStarted {
                    session_id, cwd, ..
                } => {
                    current_cwd = cwd;
                    if !session_id.is_empty() {
                        found = Some((session_id, current_cwd.clone()));
                    }
                }
                AgentEvent::Done {
                    session_id: Some(session_id),
                    ..
                } if !session_id.is_empty() => {
                    found = Some((session_id, current_cwd.clone()));
                }
                _ => {}
            }
        }
        found
    }

    fn remove_run(&self, chat_id: &str, run_id: &str) {
        let mut runs = lock(&self.runs);
        if runs.get(chat_id).is_some_and(|h| h.run_id == run_id) {
            runs.remove(chat_id);
        }
    }
}

// ── run task ────────────────────────────────────────────────────────────────

/// Apply the render-parts privacy policy: strip heavy/sensitive tool inputs before doc
/// entry. Full inputs live only in the local run journal.
fn render_parts(parts: &[MessagePart]) -> Vec<MessagePart> {
    parts
        .iter()
        .map(|part| match part {
            MessagePart::Tool {
                id,
                call,
                is_error,
                resolved,
            } => MessagePart::Tool {
                id: id.clone(),
                call: sanitize_tool_call(call),
                is_error: *is_error,
                resolved: *resolved,
            },
            other => other.clone(),
        })
        .collect()
}

/// The persisted assistant text of a folded segment (workspace preview source).
fn folded_text(parts: &[MessagePart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            MessagePart::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn sync_segment<'a>(
    doc: &'a SessionDoc,
    writer: &mut Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
) -> Result<(), DocError> {
    if folded.is_empty() {
        return Ok(());
    }
    let rendered = render_parts(folded);
    if writer.is_none() {
        *writer = Some(SegmentWriter::begin(doc, entry_id, device_id, started_at)?);
    }
    if let Some(w) = writer.as_mut() {
        w.sync(&rendered)?;
    }
    Ok(())
}

fn finish_segment<'a>(
    doc: &'a SessionDoc,
    writer: Option<SegmentWriter<'a>>,
    entry_id: &str,
    device_id: &str,
    started_at: i64,
    folded: &[MessagePart],
    status: MessageStatus,
) -> Result<(), DocError> {
    let rendered = render_parts(folded);
    match writer {
        Some(w) => w.finish(&rendered, status),
        None if !folded.is_empty() => {
            SegmentWriter::begin(doc, entry_id, device_id, started_at)?.finish(&rendered, status)
        }
        None => Ok(()),
    }
}

/// Resume bookkeeping for one run task: which user entry the run answers (so a
/// failed-resume retry re-dispatches idempotently against the same doc entry)
/// and whether `dispatch` injected the resume id itself (only engine-injected
/// resumes are retried fresh — a caller-specified resume fails loudly).
struct RunResumeState {
    user_message_id: String,
    resume_injected: bool,
}

#[allow(clippy::too_many_arguments)]
async fn drive_run(
    inner: Arc<Inner>,
    chat_id: String,
    run_id: String,
    harness: Arc<dyn Harness>,
    request: RunRequest,
    doc: Arc<SessionDoc>,
    controls: RunControls,
    mut engine_rx: mpsc::UnboundedReceiver<AgentEvent>,
    mut cancel_rx: watch::Receiver<bool>,
    resume_state: RunResumeState,
    // Held, not read: dropping it at the end of the run releases the account
    // for the usage refresher (gh#59). Nothing in here needs the value.
    _account_lease: Option<crate::agent_accounts::AccountLease>,
) {
    let device_id = inner.device_id.clone();
    // Captured for post-run auto-titling (the request moves into the harness).
    let harness_id = harness.id();
    let user_prompt = request.prompt.clone();
    let run_cwd = request.cwd.clone();
    // Kept whole for the failed-resume retry (fresh session, same user entry).
    // Option so the retry branch (inside the event loop) can take ownership.
    let mut retry_request = Some(RunRequest {
        resume: None,
        ..request.clone()
    });
    let mut stream = match harness.run(request, controls).await {
        Ok(stream) => stream,
        Err(err) => {
            let message = err.to_string();
            inner.publish(
                &chat_id,
                &AgentEvent::Error {
                    message: message.clone(),
                },
            );
            inner.publish(
                &chat_id,
                &AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(message),
                    session_id: None,
                },
            );
            inner.remove_run(&chat_id, &run_id);
            inner.set_status(&chat_id, SessionStatus::Errored, false);
            return;
        }
    };

    let doc_ref: &SessionDoc = &doc;
    let mut folded: Vec<MessagePart> = Vec::new();
    let mut entry_id = new_id();
    let mut segment_started = now_ms();
    let mut writer: Option<SegmentWriter<'_>> = None;
    let mut dirty = false;
    let mut flush_at = tokio::time::Instant::now();
    // Set when the engine interrupts the run: the harness gets this long to end its own
    // stream (its token was cancelled); past it, a terminal Done is synthesized.
    let mut interrupt_deadline: Option<tokio::time::Instant> = None;
    let mut interrupted = false;
    let mut saw_session_started = false;
    // Liveness heartbeat: this loop RUNNING is proof the harness stream is
    // open, so freshness must not depend on events arriving. Silent stretches
    // are normal and UNBOUNDED — a long tool call, redacted thinking, an
    // agent waiting on an external process, a question parked for an hour —
    // and each starved the UI's 45s staleness gate in turn (working strip /
    // AwaitingInput dot vanishing mid-run, both user-reported). No stall
    // timeout here by design (a first port was rejected — agents may
    // legitimately be quiet for >10min): a live child means Working, dying
    // paths each carry their own error, and engine death stops these ticks
    // so the gate still catches real crashes. touch_session throttles at 10s.
    let mut live_heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
    live_heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // PERSISTENT SESSION (comet runsBySession): a completed turn on a
    // steerable harness parks here instead of ending the run — the child and
    // its steering mailbox stay warm, and the next user message (dispatch
    // routes into a live run) starts the next turn with zero respawn/resume
    // latency. `Some(when)` = idle since then; the 30-min reaper below ends
    // a session nobody comes back to (comet SESSION_IDLE_MS).
    const SESSION_IDLE: std::time::Duration = std::time::Duration::from_secs(30 * 60);
    let mut idle_since: Option<tokio::time::Instant> = None;
    let steerable = harness.supports_steering();
    // STALL WATCHDOG (feature-inventory §3.2 `STALL_MS`), two tiers — because
    // silence on its own is not evidence of death. A healthy agent can be
    // completely quiet for an hour inside one Exec tool call, which is why the
    // blanket version was rejected in review. What IS unambiguous is a run that
    // has produced *nothing* — not even `SessionStarted` — since it started:
    // the CLI wedged at spawn, hung on a credential prompt it can never get
    // answered on a headless box, or left a pipe open with no process behind
    // it. Nothing legitimate is silent from second zero, so that case ends
    // terminally (tier 1) and gives the box back its child, its RAM and its
    // "Working" row. Silence AFTER real output only logs and re-arms (tier 2):
    // the child proved itself, and a live child is the working signal.
    const STALL: std::time::Duration = std::time::Duration::from_secs(10 * 60);
    let mut saw_any_event = false;
    let mut stall_at = tokio::time::Instant::now() + STALL;
    // Set by tier 1 — and by the turn guardrails below — so the failed-resume
    // retry cannot mistake a `Done` this task synthesized for a rejected
    // `--resume` id, and re-dispatch the run it just ended.
    let mut stall_killed = false;
    // TURN GUARDRAILS (gh#270), the fast counterpart to the stall watchdog and
    // to the board's wall clock. Both of those catch a run that has stopped
    // producing or has simply gone on too long; neither catches the run that is
    // producing at full speed and getting nowhere — the same failing command
    // retried forever, or tool calls without end. That is the one failure mode
    // an unattended dispatch has no defence against, and it spends the whole
    // wall-clock budget in a fraction of the time.
    //
    // The counting and the escalation are `comet_board::spin`'s (a pure
    // decision over the normalized event stream); this loop is where the two
    // rungs are *actuated*, because it is the only place that can do either —
    // it owns the run's steering mailbox and its interrupt token. Off unless
    // the chat carries limits, which means off for every chat the board did
    // not dispatch.
    let mut spin = comet_board::spin::Watch::new(inner.turn_limits(&chat_id));

    let final_status = loop {
        let event: AgentEvent = tokio::select! {
            biased;
            changed = cancel_rx.changed(), if !interrupted => {
                let _ = changed;
                interrupted = true;
                interrupt_deadline = Some(
                    tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                );
                continue;
            }
            _ = tokio::time::sleep_until(
                interrupt_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if interrupt_deadline.is_some() => AgentEvent::Done {
                status: DoneStatus::Interrupted,
                result: None,
                error: None,
                session_id: None,
            },
            _ = live_heartbeat.tick() => {
                inner.touch_session(&chat_id);
                continue;
            }
            // Idle reaper (comet SESSION_IDLE_MS): a parked persistent session
            // nobody returned to in 30 minutes releases its child. The turn
            // was finalized at Done, so this end is clean — no aborted stamp.
            _ = tokio::time::sleep_until(
                idle_since.map(|at| at + SESSION_IDLE).unwrap_or_else(tokio::time::Instant::now)
            ), if idle_since.is_some() => {
                tracing::info!(chat = %chat_id, "reaping idle persistent session");
                if let Some(token) = lock(&inner.runs)
                    .get(&chat_id)
                    .filter(|h| h.run_id == run_id)
                    .map(|h| h.interrupt_token.clone())
                {
                    token.cancel();
                }
                break SessionStatus::Idle;
            }
            // Stall watchdog (see STALL above). Disabled while parked — a
            // parked session is silent BY DESIGN and belongs to the idle
            // reaper; `stall_at` is re-armed by the first event of the next
            // turn like any other event.
            _ = tokio::time::sleep_until(stall_at), if idle_since.is_none() => {
                stall_at = tokio::time::Instant::now() + STALL;
                if saw_any_event {
                    // Tier 2, advisory: it spoke once, so it is working until
                    // proven otherwise. Log and keep waiting, forever if need be.
                    tracing::warn!(
                        chat = %chat_id,
                        "harness silent for 10min; child is live, leaving the run alone"
                    );
                    continue;
                }
                // Tier 1, terminal: never produced a single event.
                tracing::warn!(
                    chat = %chat_id,
                    "harness produced no output within 10min of starting; ending the wedged run"
                );
                stall_killed = true;
                // Release the child. Its stream may still emit afterwards; this
                // task settles the run right here, so nothing else observes it.
                if let Some(token) = lock(&inner.runs)
                    .get(&chat_id)
                    .filter(|h| h.run_id == run_id)
                    .map(|h| h.interrupt_token.clone())
                {
                    token.cancel();
                }
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(
                        "Harness produced no output for 10 minutes after starting — \
                         ending the wedged run".into(),
                    ),
                    session_id: None,
                }
            }
            Some(event) = engine_rx.recv() => event,
            next = stream.next() => match next {
                Some(Ok(event)) => event,
                Some(Err(err)) => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some(err.to_string()),
                    session_id: None,
                },
                None if interrupted => AgentEvent::Done {
                    status: DoneStatus::Interrupted,
                    result: None,
                    error: None,
                    session_id: None,
                },
                // Stream end while PARKED idle: a per-turn adapter closing
                // after its final Done — a clean end, not a crash (the turn
                // was already finalized). Persistent adapters keep the
                // stream open and never hit this.
                None if idle_since.is_some() => break SessionStatus::Idle,
                None => AgentEvent::Done {
                    status: DoneStatus::Errored,
                    result: None,
                    error: Some("harness stream ended without Done".into()),
                    session_id: None,
                },
            },
            _ = tokio::time::sleep_until(flush_at), if dirty => {
                // Coalesced STREAM_COMMIT_MS tick: one doc commit per window.
                if let Err(err) = sync_segment(
                    doc_ref, &mut writer, &entry_id, &device_id, segment_started, &folded,
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "segment sync failed");
                }
                dirty = false;
                continue;
            }
        };

        // Any stream activity proves the run is alive — keep the session's
        // freshness inside the UI's 45s staleness window (throttled), and
        // re-arm the stall watchdog. `saw_any_event` is per-RUN, not per-turn:
        // once a harness has spoken it is never tier-1 again, so a steered
        // turn that opens with a long silent tool call is safe.
        saw_any_event = true;
        stall_at = tokio::time::Instant::now() + STALL;
        inner.touch_session(&chat_id);
        // First event after parking idle = the next turn beginning (a routed
        // dispatch steered in): the session is Working again.
        //
        // Since gh#280 a SUBAGENT reaches here too. A `run_in_background` Task
        // returns its tool result the instant it is launched, so the parent
        // can finish its turn and park while the subagent works on — and the
        // frames it goes on to emit now un-park the session instead of falling
        // into the silence that made "still working" and "finished" identical.
        // It also keeps the idle reaper off a child that is doing real work;
        // the CLI re-invokes the model when the background task completes,
        // which ends in a Done that parks the session again.
        if idle_since.take().is_some() {
            inner.set_status(&chat_id, SessionStatus::Working, true);
        }
        // Empty reasoning deltas are PURE heartbeats: redacted thinking and
        // tool-input-generation windows stream them with no text. They fold
        // to nothing, so journaling/publishing them is only noise (hundreds
        // per long turn observed) — the touch above already did their job.
        if matches!(&event, AgentEvent::ReasoningDelta { text } if text.is_empty()) {
            continue;
        }

        // Turn guardrails (gh#270): has this turn started spinning? The event
        // is still processed normally either way — a breach is something the
        // run does *in addition to* what it was already doing, never a reason
        // to drop the frame that revealed it.
        if let Some(verdict) = spin.observe(&event) {
            match verdict.rung {
                // Soft rung: one message into the chat, at the harness's own
                // steering boundary. Most loops are recoverable, and an
                // interrupt throws away a worktree's worth of context — so the
                // agent is told what the board can see and given the chance to
                // change approach, or to stop and say it is blocked.
                //
                // Spawned rather than awaited: it writes the user entry through
                // the same doc this task is streaming into, and the run loop
                // must not stall behind it. A harness that cannot be steered
                // (or a mailbox mid-teardown) reports `NotSteerable`, which is
                // logged and nothing else — the hard rung below is what still
                // bounds that run.
                comet_board::spin::Rung::Steer => {
                    tracing::warn!(
                        chat = %chat_id,
                        reason = %verdict.reason.sentence(),
                        "turn guardrail: steering a run that looks stuck"
                    );
                    let engine = SessionsEngine {
                        inner: inner.clone(),
                    };
                    let chat = chat_id.clone();
                    let text = verdict.steer_text();
                    tokio::spawn(async move {
                        match engine.steer(&chat, &text, None).await {
                            Ok(SteerOutcome::Accepted) => {}
                            Ok(SteerOutcome::NotSteerable) => tracing::warn!(
                                chat = %chat,
                                "turn guardrail: no steerable run to warn; \
                                 the run will be stopped if it keeps going"
                            ),
                            Err(err) => tracing::warn!(
                                chat = %chat, error = %err,
                                "turn guardrail: delivering the warning failed"
                            ),
                        }
                    });
                }
                // Hard rung: end the run. The child is released the way the
                // stall watchdog releases it, and the terminal pair is queued
                // through the engine channel rather than synthesized here — so
                // the loop's own `Done` handling finalizes the segment, resolves
                // dangling input chips and settles the status exactly as it does
                // for every other ending.
                //
                // `Errored`, not `Interrupted`: this is a run that stopped
                // badly, and the board's reconcile reads that off the journal as
                // a *block* — the attempt keeps its chat, its context and its
                // slot, the dispatching agent and the orchestrator are told, and
                // retry-or-cancel is the operator's call. Which is the whole
                // point of stopping it early rather than at the wall clock.
                comet_board::spin::Rung::Stop => {
                    let message = verdict.stop_text();
                    // Same flag the stall watchdog sets, for the same reason:
                    // the failed-resume retry below must never read a `Done`
                    // *we* synthesized as a `--resume` id the harness rejected,
                    // which would re-dispatch the very run this just stopped.
                    stall_killed = true;
                    tracing::warn!(
                        chat = %chat_id,
                        reason = %verdict.reason.sentence(),
                        "turn guardrail: ending a looping run"
                    );
                    if let Some((token, engine_tx)) = lock(&inner.runs)
                        .get(&chat_id)
                        .filter(|h| h.run_id == run_id)
                        .map(|h| (h.interrupt_token.clone(), h.engine_tx.clone()))
                    {
                        token.cancel();
                        // A visible error part first: the transcript has to say
                        // why this stopped, and the `Done` alone leaves an
                        // aborted-looking entry with no explanation in it.
                        let _ = engine_tx.send(AgentEvent::Error {
                            message: message.clone(),
                        });
                        let _ = engine_tx.send(AgentEvent::Done {
                            status: DoneStatus::Errored,
                            result: None,
                            error: Some(message),
                            session_id: None,
                        });
                    }
                }
            }
        }

        // Failed-resume fallback: an engine-injected `--resume` naming a session
        // the harness no longer knows dies before ever starting (claude exits
        // without an init frame; codex falls back internally via thread/start).
        // Signature: errored Done, no SessionStarted, nothing streamed. Retry
        // ONCE as a fresh session against the same user entry — tombstone the
        // dead id first so no lookup source (journal included) re-injects it.
        if resume_state.resume_injected
            && !saw_session_started
            && folded.is_empty()
            && !interrupted
            && !stall_killed
            && matches!(
                &event,
                AgentEvent::Done {
                    status: DoneStatus::Errored,
                    ..
                }
            )
            && let Some(retry) = retry_request.take()
        {
            tracing::warn!(
                chat = %chat_id,
                "harness rejected injected resume id; retrying as a fresh session"
            );
            inner.forget_harness_session(&chat_id);
            inner.remove_run(&chat_id, &run_id);
            let engine = SessionsEngine {
                inner: inner.clone(),
            };
            let chat = chat_id.clone();
            let message_id = resume_state.user_message_id.clone();
            tokio::spawn(async move {
                // `inject_resume = false`: the retry must start fresh. The user
                // entry write inside dispatch is idempotent by message id.
                if let Err(err) = engine
                    .dispatch_with(&chat, harness_id, retry, Some(message_id), false)
                    .await
                {
                    tracing::error!(chat = %chat, error = %err, "fresh-session retry dispatch failed");
                }
            });
            return;
        }

        // A steer boundary splits the assistant entry exactly where the fold resets.
        if let AgentEvent::Steered {
            next_assistant_message_id,
            ..
        } = &event
        {
            inner.publish(&chat_id, &event);
            if let Err(err) = finish_segment(
                doc_ref,
                writer.take(),
                &entry_id,
                &device_id,
                segment_started,
                &folded,
                MessageStatus::Complete,
            ) {
                tracing::warn!(chat = %chat_id, error = %err, "segment finish failed");
            }
            inner.note_message(&chat_id, &folded_text(&folded));
            folded.clear();
            dirty = false;
            entry_id = next_assistant_message_id.clone().unwrap_or_else(new_id);
            segment_started = now_ms();
            continue;
        }

        match &event {
            AgentEvent::SessionStarted {
                session_id, cwd, ..
            } => {
                saw_session_started = true;
                // The event's own cwd (where the harness actually created the
                // session) scopes the stored id, not the request's.
                inner.remember_harness_session(&chat_id, session_id, cwd);
            }
            AgentEvent::Done {
                session_id: Some(session_id),
                ..
            } => {
                inner.remember_harness_session(&chat_id, session_id, &run_cwd);
            }
            AgentEvent::InputRequested { request_id, .. } => {
                // The engine's input bridge is the sole authority on input
                // requests: it mints the id and parks the resolver BEFORE
                // emitting the event, so a legitimate id is always pending
                // here. A harness emitting its own copy (a different id no
                // resolver knows) would fold an unanswerable twin chip into
                // the doc — and answering the twin would never resume the
                // run. Drop such events.
                let pending = lock(&inner.runs)
                    .get(&chat_id)
                    .map(|h| h.pending_inputs.clone());
                let known = pending.is_some_and(|p| lock(&p).contains_key(request_id));
                if !known {
                    tracing::warn!(
                        chat = %chat_id,
                        request = %request_id,
                        "dropping harness-emitted InputRequested (unknown id; \
                         the engine input bridge owns this lifecycle)"
                    );
                    continue;
                }
                inner.set_status(&chat_id, SessionStatus::AwaitingInput, false);
            }
            AgentEvent::InputResolved { .. } => {
                inner.set_status(&chat_id, SessionStatus::Working, false);
            }
            _ => {}
        }

        inner.publish(&chat_id, &event);

        // Defensive rule from comet: a mid-run SessionStarted re-emission (Claude SDK
        // background re-invocations) must not wipe the segment being written.
        let skip_fold = matches!(&event, AgentEvent::SessionStarted { .. }) && !folded.is_empty();
        if !skip_fold {
            fold_event_into_parts(&mut folded, &event);
        }

        if let AgentEvent::Done { status, .. } = &event {
            let message_status = match status {
                DoneStatus::Interrupted => MessageStatus::Aborted,
                DoneStatus::Completed | DoneStatus::Errored => MessageStatus::Complete,
            };
            // No dangling chips: a run that ends for ANY reason (completed,
            // errored, interrupted) terminally resolves its input parts — an
            // unresolved question must not outlive the run that asked it
            // (its resolver died with the run; an answer could never land).
            for part in folded.iter_mut() {
                if let MessagePart::Input { resolved, .. } = part {
                    *resolved = true;
                }
            }
            // A Done landing on a PARKED session with nothing streamed (the
            // idle reaper's or an interrupt's own teardown) has no entry to
            // finalize — writing one would leave an empty aborted stub.
            let nothing_streamed = writer.is_none() && folded.is_empty();
            if !nothing_streamed {
                if let Err(err) = finish_segment(
                    doc_ref,
                    writer.take(),
                    &entry_id,
                    &device_id,
                    segment_started,
                    &folded,
                    message_status,
                ) {
                    tracing::warn!(chat = %chat_id, error = %err, "final segment finish failed");
                }
                inner.note_message(&chat_id, &folded_text(&folded));
            }
            if *status == DoneStatus::Completed {
                // A cleanly completed turn resets the auto-resume revival
                // budget: only consecutive crash-revive-crash cycles spend it.
                inner.journal.clear_resume_attempts(&chat_id);
            }
            // Exchange completed on an untitled chat → name it (fire-and-forget;
            // interrupted/errored turns never trigger naming).
            if *status == DoneStatus::Completed
                && let Some(titles) = inner.titles.get()
            {
                titles.maybe_generate(&chat_id, harness_id, &user_prompt, &run_cwd);
            }
            // PERSISTENT SESSION: a cleanly completed turn on a steerable
            // harness PARKS instead of ending — child + mailbox stay warm for
            // the next routed dispatch; per-turn state resets for it.
            if *status == DoneStatus::Completed && steerable && !interrupted {
                folded.clear();
                dirty = false;
                entry_id = new_id();
                segment_started = now_ms();
                // Resume-retry is strictly a first-turn concern.
                saw_session_started = true;
                idle_since = Some(tokio::time::Instant::now());
                inner.set_status(&chat_id, SessionStatus::Idle, false);
                continue;
            }
            break match status {
                DoneStatus::Errored => SessionStatus::Errored,
                _ => SessionStatus::Idle,
            };
        }

        if !folded.is_empty() && !dirty {
            dirty = true;
            flush_at =
                tokio::time::Instant::now() + std::time::Duration::from_millis(STREAM_COMMIT_MS);
        }
    };

    // Invariant guard: no run may leave a half-open segment. Every terminal
    // path finalizes through the `Done` handling above (each takes the
    // writer), so a writer still held here means a segment was begun but
    // NEVER finalized — a run that died without a terminal event reaching
    // this task (gh#28). The transcript keeps a stale `streaming` row live
    // if the doc entry stays `Streaming`, and its streaming fade veil can
    // freeze a fading chunk, so stamp the dangling segment `aborted` with a
    // visible note before the run is torn down.
    if writer.is_some() {
        let mut dangling = folded.clone();
        dangling.push(MessagePart::Error {
            id: format!("e{}", dangling.len()),
            message: "Run ended without a terminal event".into(),
        });
        tracing::warn!(chat = %chat_id, "run ended with an unfinalized segment; stamping aborted");
        if let Err(err) = finish_segment(
            doc_ref,
            writer.take(),
            &entry_id,
            &device_id,
            segment_started,
            &dangling,
            MessageStatus::Aborted,
        ) {
            tracing::warn!(chat = %chat_id, error = %err, "dangling segment finalize failed");
        }
    }

    inner.remove_run(&chat_id, &run_id);
    inner.set_status(&chat_id, final_status, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gh#395 — the doc host asks this before it releases a chat's room, so
    /// what counts as "busy" is worth pinning down on its own: a settled chat
    /// keeps a status row forever, and reading that row as busy would leave
    /// every chat this engine has ever run permanently un-releasable.
    #[test]
    fn chat_is_busy_only_while_a_run_is_unsettled() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Arc::new(RunJournal::open(dir.path().join("journals")).expect("journal"));
        let sessions = SessionsEngine::new(
            "device-a".into(),
            journal,
            Arc::new(crate::registry::default_registry()),
        );

        assert!(
            !sessions.chat_is_busy("never-ran"),
            "a chat with no status row at all"
        );

        sessions.set_status("chat-a", SessionStatus::Working, true);
        assert!(sessions.chat_is_busy("chat-a"));

        sessions.set_status("chat-a", SessionStatus::AwaitingInput, false);
        assert!(
            sessions.chat_is_busy("chat-a"),
            "a question is a live run waiting for an answer"
        );

        sessions.set_status("chat-a", SessionStatus::Idle, false);
        assert!(!sessions.chat_is_busy("chat-a"), "settled: releasable");

        sessions.set_status("chat-a", SessionStatus::Errored, false);
        assert!(
            !sessions.chat_is_busy("chat-a"),
            "a failed turn is over too — nothing is streaming through this doc"
        );
    }
}
