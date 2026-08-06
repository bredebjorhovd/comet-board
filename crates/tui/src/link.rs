//! The engine link: one supervisor task that owns the RPC connection and all
//! subscriptions, and talks to the render loop over two channels.
//!
//! Why a supervisor and not `RpcClient` calls from the draw loop:
//!
//! - **Decoding is not the renderer's job.** Every watch frame arrives as a
//!   `serde_json::Value` carrying a *full snapshot* (the engine's watch streams
//!   are `tokio::sync::watch`, so a burst of doc commits collapses to the
//!   latest value). Deserializing into typed rows here keeps the render loop's
//!   only work layout and diffing — it never touches serde.
//! - **The daemon is a separate process.** It can be restarted under us
//!   (`comet daemon restart`, an upgrade, a crash). The supervisor notices the
//!   streams ending, reconnects with backoff, and resubscribes; the app just
//!   sees `Connection` updates and keeps its own state. Nothing in the
//!   viewport needs to know a reconnect happened.
//! - **A slow call must not stall input.** Outgoing calls are spawned, so a
//!   `QueueCommand` that takes a second cannot delay a keystroke.

use std::sync::Arc;
use std::time::Duration;

use comet_doc::SessionMessageEntry;
use comet_proto::view::board::{self as board_view, TaskRow};
use comet_proto::view::ConnectionStatus;
use comet_proto::{AuthState, Chat, Device, Session, Space};
use comet_rpc::{RpcClient, methods};
use tokio::sync::mpsc;

use crate::daemon::{Attachment, DaemonConfig};

/// Everything the render loop learns about the world.
#[derive(Debug)]
pub enum Update {
    Connection(ConnectionStatus),
    /// How we reached the engine — decides what quitting means.
    Attached(Attachment),
    Auth(Box<AuthState>),
    Chats(Vec<Chat>),
    Spaces(Vec<Space>),
    Devices(Vec<Device>),
    Sessions(Vec<Session>),
    /// A transcript snapshot. Carries the chat id so a frame that raced a
    /// selection change is dropped rather than rendered under the wrong title.
    Transcript {
        chat_id: String,
        entries: Vec<SessionMessageEntry>,
    },
    /// This engine's device id — the host for spaces we create.
    LocalDevice(String),
    /// The model catalogue for a harness, answering [`Command::ListModels`].
    Models(Vec<comet_proto::Model>),
    /// A space's branches, answering [`Command::ListRefs`].
    Refs(Vec<comet_proto::RepoRef>),
    /// A board snapshot: every task as a row, in board order. Best-effort — an
    /// engine without the board serves nothing and the app just shows an empty
    /// board.
    Board(Vec<TaskRow>),
    /// Which device the board stream is pointed at, and whether it has actually
    /// delivered rows (gh#55). The pane renders this in its header; the
    /// supervisor owns the choice, so the two can never disagree about which
    /// device a dispatch will land on.
    BoardHostChanged {
        device: Option<String>,
        live: bool,
    },
    /// Which chat the board has pinned as its orchestrator (gh#104), or `None`
    /// for a board with no orchestrator. Rides the board host's sweep, so it is
    /// always the pin of the board the pane is actually showing.
    Orchestrator(Option<String>),
    /// The agent logins a pending dispatch could spend, answering
    /// [`Command::ListDispatchAccounts`]. Carries the task id so a reply that
    /// raced a newer pick is dropped rather than shown under the wrong row.
    DispatchAccounts {
        task_id: String,
        accounts: Vec<comet_proto::AgentAccount>,
        /// The harness the row's runtime resolved to, when `ListBoardRuntimes`
        /// answered. What lets the picker say whose subscription row 0 spends:
        /// the box's own login is the *active* account for a harness, and
        /// without one there is no reliable way to pick it out (gh#101).
        harness: Option<comet_proto::HarnessId>,
    },
    /// A drafted session became real: the chat exists and its prompt is queued.
    SessionStarted {
        chat_id: String,
    },
    /// A transient message for the status line.
    Notice(String),
    /// An optimistic send that didn't land: the app drops the echo and hands
    /// the text back to the composer rather than silently losing it.
    SendFailed {
        chat_id: String,
        message_id: String,
        error: String,
    },
}

/// Everything the render loop asks of the engine.
#[derive(Debug)]
pub enum Command {
    /// Retarget the per-chat transcript subscription (`None` unsubscribes).
    /// Only the visible chat's transcript is streamed: an idle engine should
    /// not be serializing docs nobody is looking at.
    WatchTranscript(Option<String>),
    /// Fire-and-forget call; failures surface as [`Update::Notice`].
    Call {
        method: &'static str,
        params: serde_json::Value,
        /// What to say if it fails ("Couldn't archive the session").
        context: &'static str,
    },
    /// A send, tracked so a failure can be attributed to its echo.
    Send {
        chat_id: String,
        message_id: String,
        params: serde_json::Value,
    },
    /// Fetch the model catalogue for a harness. Unlike [`Command::Call`] this
    /// one's *reply* is wanted, so it comes back as [`Update::Models`].
    ListModels { harness: comet_proto::HarnessId },
    /// Fetch a space's branches, for the ref picker.
    ListRefs {
        repo_path: String,
        target_device: Option<String>,
    },
    /// Turn a drafted session into a real one, then send its first prompt.
    ///
    /// This is one command rather than three because the steps are *ordered and
    /// dependent*: a new worktree has to exist before the chat can name it as
    /// its cwd, and the chat has to exist before its prompt can be queued.
    /// Sequencing that in the reducer would mean modelling half-finished
    /// sessions; sequencing it here means a failure at any step leaves nothing
    /// behind but a notice.
    StartSession(Box<StartSession>),
    /// Release a board task from the TUI: the operator is dispatching, so there
    /// is no `via` — provenance is never fabricated. The reply names the chat
    /// that will hold the run; the identifier travels so the notice reads well.
    /// The target device is stamped by the supervisor, which owns the board
    /// host — a dispatch always lands on the device whose rows you are reading.
    ///
    /// `account` is the agent login the picker chose, `None` being the route's
    /// own (gh#74). `via_device`/`via_user` say who released it — recorded on
    /// the attempt, never checked, never authorized on; see the board's
    /// `DispatchOrigin`.
    Dispatch {
        task_id: String,
        identifier: String,
        account: Option<String>,
        via_device: Option<String>,
        via_user: Option<String>,
        /// End the task's live attempt and release a fresh one — `R` on a
        /// blocked row (gh#73), the deliberate exception to the
        /// one-live-attempt rule. Plain dispatches send `false` and are
        /// refused on a live attempt.
        replace: bool,
    },
    /// The logins a dispatch of `task_id` could spend, for the account picker
    /// (gh#74). Answered as [`Update::DispatchAccounts`], filtered to the
    /// harness `runtime` resolves to — a Claude slot cannot pay for a codex run.
    ListDispatchAccounts {
        task_id: String,
        /// The row's runtime, i.e. the route's. `None` (or a name the host does
        /// not know) leaves the list unfiltered rather than empty: an account
        /// the run cannot use is refused by the engine, but a picker with no
        /// rows at all cannot even be argued with.
        runtime: Option<String>,
    },
    /// Point the board pane at a device, or hand the choice back to the sweep
    /// (gh#55). Survives reconnects, so a pinned box stays pinned across a
    /// `comet daemon restart`.
    BoardHost(BoardHost),
    /// Drop this connection and dial again now, skipping the backoff. What `r`
    /// does after the user has fixed whatever was wrong.
    Reconnect,
    /// Drop the connection and stop reconnecting (quit path).
    Shutdown,
}

/// Which device's board the pane reads and drives (gh#55).
///
/// The board store lives on exactly one device — usually the always-on box —
/// and the board RPCs are relay-forwardable, so a laptop with no board of its
/// own is not out of options: it asks the others. [`BoardHost::Auto`] is that
/// sweep, and is right on every install where one box hosts the board;
/// [`BoardHost::Pinned`] is the escape hatch for when two do, or when the guess
/// is wrong.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BoardHost {
    /// Ask each device in turn until one answers with rows.
    #[default]
    Auto,
    /// Exactly this device; `None` is this one (no `targetDeviceId`).
    Pinned(Option<String>),
}

impl BoardHost {
    /// The device this choice starts at. `Auto` starts at this device — a local
    /// board must always win over a remote one.
    fn target(&self) -> Option<String> {
        match self {
            BoardHost::Auto => None,
            BoardHost::Pinned(device) => device.clone(),
        }
    }
}

/// Everything needed to materialize a drafted session.
#[derive(Debug)]
pub struct StartSession {
    pub chat_id: String,
    pub space_id: String,
    pub repo_path: String,
    pub target_device: Option<String>,
    pub plan: comet_proto::view::CheckoutPlan,
    pub config: Option<serde_json::Value>,
    pub message_id: String,
    /// The already-encoded `SessionCommandPayload::Run`.
    pub command: serde_json::Value,
}

pub struct EngineLink {
    pub updates: mpsc::UnboundedReceiver<Update>,
    pub commands: mpsc::UnboundedSender<Command>,
    supervisor: tokio::task::JoinHandle<()>,
}

impl EngineLink {
    /// Ask the supervisor to do something. A closed channel means the
    /// supervisor is gone, which only happens on the quit path — dropping the
    /// command is correct there.
    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

impl Drop for EngineLink {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        self.supervisor.abort();
    }
}

/// Reconnect backoff: quick first retries (a `comet daemon restart` is back in
/// well under a second) flattening to a 5s poll so a long-down engine costs
/// nothing.
const BACKOFF_MS: [u64; 6] = [200, 400, 800, 1_600, 3_200, 5_000];

/// Start the supervisor. It connects immediately (spawning a daemon if needed)
/// and keeps the connection alive for the life of the app.
pub fn spawn(config: DaemonConfig) -> EngineLink {
    let (update_tx, updates) = mpsc::unbounded_channel();
    let (commands, command_rx) = mpsc::unbounded_channel();
    let supervisor = tokio::spawn(supervise(config, update_tx, command_rx));
    EngineLink {
        updates,
        commands,
        supervisor,
    }
}

async fn supervise(
    config: DaemonConfig,
    updates: mpsc::UnboundedSender<Update>,
    mut commands: mpsc::UnboundedReceiver<Command>,
) {
    // The transcript target survives reconnects: after the engine comes back we
    // resubscribe whatever the user is still looking at.
    let mut transcript_target: Option<String> = None;
    // So does the board host: a pinned box stays pinned across a daemon
    // restart, and an unpinned one starts its sweep over (the box may have been
    // what restarted).
    let mut board_pin = BoardHost::default();
    let mut attempt = 0usize;

    loop {
        if updates
            .send(Update::Connection(ConnectionStatus::Connecting))
            .is_err()
        {
            return; // App is gone.
        }

        match crate::daemon::connect(&config).await {
            Ok(connection) => {
                attempt = 0;
                let _ = updates.send(Update::Attached(connection.attachment.clone()));
                let _ = updates.send(Update::Connection(ConnectionStatus::Ready));
                let client = Arc::new(connection.client);
                match session(
                    &client,
                    &updates,
                    &mut commands,
                    &mut transcript_target,
                    &mut board_pin,
                )
                .await
                {
                    SessionEnd::Shutdown => return,
                    SessionEnd::AppGone => return,
                    SessionEnd::ConnectionLost => {
                        let _ = updates.send(Update::Connection(ConnectionStatus::Failed(
                            "engine connection lost".into(),
                        )));
                    }
                }
            }
            Err(err) => {
                if updates
                    .send(Update::Connection(ConnectionStatus::Failed(format!(
                        "{err:#}"
                    ))))
                    .is_err()
                {
                    return;
                }
            }
        }

        // Backoff, but stay responsive: Shutdown ends us now and Reconnect
        // short-circuits the wait. Commands that need a connection are dropped
        // — except the transcript target, which we must remember so the
        // resubscribe after reconnect restores what the user is reading.
        let wait = Duration::from_millis(BACKOFF_MS[attempt.min(BACKOFF_MS.len() - 1)]);
        attempt = attempt.saturating_add(1);
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                command = commands.recv() => match command {
                    None | Some(Command::Shutdown) => return,
                    Some(Command::Reconnect) => {
                        attempt = 0;
                        break;
                    }
                    Some(Command::WatchTranscript(target)) => transcript_target = target,
                    // The board host is a choice, not a request: remember it so
                    // the reconnect resubscribes where the operator pointed.
                    Some(Command::BoardHost(choice)) => board_pin = choice,
                    Some(_) => {}
                },
            }
        }
    }
}

enum SessionEnd {
    /// A stream ended or a subscribe failed — the daemon went away.
    ConnectionLost,
    /// The app asked us to stop.
    Shutdown,
    /// The app dropped its receiver.
    AppGone,
}

/// Serve one connection: subscribe everything, then pump until something breaks.
async fn session(
    client: &Arc<RpcClient>,
    updates: &mpsc::UnboundedSender<Update>,
    commands: &mut mpsc::UnboundedReceiver<Command>,
    transcript_target: &mut Option<String>,
    board_pin: &mut BoardHost,
) -> SessionEnd {
    let empty = || serde_json::json!({});

    // The engine's device id is a plain call, not a stream. Best-effort: an
    // engine that doesn't serve it yet just leaves space creation disabled.
    let mut local_device: Option<String> = None;
    match client.call(methods::LOCAL_DEVICE, empty()).await {
        Ok(value) => {
            if let Some(id) = value.get("deviceId").and_then(|v| v.as_str()) {
                local_device = Some(id.to_string());
                if updates.send(Update::LocalDevice(id.to_string())).is_err() {
                    return SessionEnd::AppGone;
                }
            }
        }
        Err(err) => tracing::debug!(error = %err, "LocalDevice unavailable"),
    }

    let (mut chats, mut spaces, mut devices, mut sessions, mut auth) = match tokio::try_join!(
        client.subscribe(methods::WATCH_CHATS, empty()),
        client.subscribe(methods::WATCH_SPACES, empty()),
        client.subscribe(methods::WATCH_DEVICES, empty()),
        client.subscribe(methods::WATCH_SESSIONS, empty()),
        client.subscribe(methods::AUTH_STATUS, empty()),
    ) {
        Ok(streams) => streams,
        Err(err) => {
            tracing::warn!(error = %err, "engine subscribe failed");
            return SessionEnd::ConnectionLost;
        }
    };

    // Resubscribe the visible transcript (first connect: usually None; after a
    // reconnect: whatever the user was reading).
    let mut transcript = match transcript_target.clone() {
        Some(chat_id) => open_transcript(client, chat_id).await,
        None => None,
    };

    // The board is best-effort: an engine built without the board, or with
    // `COMET_BOARD=0`, refuses the stream and the chat viewport must keep
    // working anyway. Which device we ask is [`BoardHost`]'s business (gh#55):
    // `board_host` is where the current subscription points, `board_delivered`
    // records whether that device ever answered — the engine refuses the
    // subscription outright when it hosts no board, so silence IS the answer,
    // and an unpinned host walks on to the next device.
    let mut devices_seen: Vec<Device> = Vec::new();
    let mut board_host = board_pin.target();
    let mut board_delivered = false;
    let (mut board, mut orchestrator) = open_board_pair(client, board_host.as_deref()).await;
    let mut board_retry: Option<tokio::time::Instant> = None;
    if updates
        .send(Update::BoardHostChanged {
            device: board_host.clone(),
            live: false,
        })
        .is_err()
    {
        return SessionEnd::AppGone;
    }

    loop {
        tokio::select! {
            // Biased so a burst of doc frames can never starve a command: the
            // user's keystroke-driven work goes out first.
            biased;

            command = commands.recv() => match command {
                None | Some(Command::Shutdown) => return SessionEnd::Shutdown,
                // A manual reconnect tears this link down; `supervise` dials
                // again immediately.
                Some(Command::Reconnect) => return SessionEnd::ConnectionLost,
                Some(Command::WatchTranscript(target)) => {
                    if *transcript_target != target {
                        *transcript_target = target.clone();
                        // Dropping the receiver cancels the stream server-side
                        // (comet-rpc sends `{id, cancel}` on the next frame),
                        // so the engine stops serializing the old doc.
                        transcript = match target {
                            Some(chat_id) => open_transcript(client, chat_id).await,
                            None => None,
                        };
                    }
                }
                Some(Command::Call { method, params, context }) => {
                    spawn_call(client.clone(), updates.clone(), method, params, context);
                }
                Some(Command::Send { chat_id, message_id, params }) => {
                    spawn_send(client.clone(), updates.clone(), chat_id, message_id, params);
                }
                Some(Command::ListModels { harness }) => {
                    spawn_models(client.clone(), updates.clone(), harness);
                }
                Some(Command::ListRefs { repo_path, target_device }) => {
                    spawn_refs(client.clone(), updates.clone(), repo_path, target_device);
                }
                Some(Command::StartSession(start)) => {
                    spawn_start_session(client.clone(), updates.clone(), *start);
                }
                Some(Command::Dispatch {
                    task_id,
                    identifier,
                    account,
                    via_device,
                    via_user,
                    replace,
                }) => {
                    // The release lands on the device whose board this is.
                    spawn_dispatch(
                        client.clone(),
                        updates.clone(),
                        Dispatch {
                            task_id,
                            identifier,
                            account,
                            via_device,
                            via_user,
                            replace,
                        },
                        board_host.clone(),
                    );
                }
                Some(Command::ListDispatchAccounts { task_id, runtime }) => {
                    // The host's logins, not this laptop's: the run executes
                    // there, and a slot id only means anything on the device
                    // that saved it.
                    spawn_dispatch_accounts(
                        client.clone(),
                        updates.clone(),
                        task_id,
                        runtime,
                        board_host.clone(),
                    );
                }
                Some(Command::BoardHost(choice)) => {
                    *board_pin = choice;
                    board_host = board_pin.target();
                    board_delivered = false;
                    board_retry = None;
                    (board, orchestrator) = open_board_pair(client, board_host.as_deref()).await;
                    // Another device is another board: clear the rows and the
                    // pin rather than leave one box's tasks — and one box's
                    // orchestrator glyph — under the other's name. The pin
                    // especially: the new host answers with its own, or (no
                    // board there) never answers at all, and a stale glyph
                    // would then sit on a row nothing is delivered to.
                    if updates.send(Update::Board(Vec::new())).is_err()
                        || updates.send(Update::Orchestrator(None)).is_err()
                        || updates
                            .send(Update::BoardHostChanged {
                                device: board_host.clone(),
                                live: false,
                            })
                            .is_err()
                    {
                        return SessionEnd::AppGone;
                    }
                }
            },

            // The board's own backoff: a host that has nothing to say (or lost
            // the stream) is re-asked here rather than in a tight loop.
            _ = wait_until(board_retry) => {
                board_retry = None;
                board_delivered = false;
                (board, orchestrator) = open_board_pair(client, board_host.as_deref()).await;
            },

            frame = chats.recv() => match decode::<Vec<Chat>>(frame, "chats") {
                Frame::Value(rows) => if updates.send(Update::Chats(rows)).is_err() { return SessionEnd::AppGone },
                Frame::Skip => {}
                Frame::Ended => return SessionEnd::ConnectionLost,
            },
            frame = spaces.recv() => match decode::<Vec<Space>>(frame, "spaces") {
                Frame::Value(rows) => if updates.send(Update::Spaces(rows)).is_err() { return SessionEnd::AppGone },
                Frame::Skip => {}
                Frame::Ended => return SessionEnd::ConnectionLost,
            },
            frame = devices.recv() => match decode::<Vec<Device>>(frame, "devices") {
                Frame::Value(rows) => {
                    // Kept here too: the board's host sweep walks this list, and
                    // a device that registers mid-session is a candidate.
                    devices_seen.clone_from(&rows);
                    if updates.send(Update::Devices(rows)).is_err() { return SessionEnd::AppGone }
                }
                Frame::Skip => {}
                Frame::Ended => return SessionEnd::ConnectionLost,
            },
            frame = sessions.recv() => match decode::<Vec<Session>>(frame, "sessions") {
                Frame::Value(rows) => if updates.send(Update::Sessions(rows)).is_err() { return SessionEnd::AppGone },
                Frame::Skip => {}
                Frame::Ended => return SessionEnd::ConnectionLost,
            },
            frame = auth.recv() => match frame {
                // Auth is the one frame with two wire shapes in flight; the
                // tolerant parser is shared with the gpui viewport.
                Some(value) => match comet_proto::view::parse_auth_state(&value) {
                    Some(state) => if updates.send(Update::Auth(Box::new(state))).is_err() {
                        return SessionEnd::AppGone;
                    },
                    None => tracing::warn!("dropping unrecognized AuthStatus frame"),
                },
                None => return SessionEnd::ConnectionLost,
            },

            // A transcript stream ending is NOT a lost connection: the engine
            // ends it when the chat is deleted. Drop it and keep going.
            frame = recv_optional(&mut transcript) => {
                let (chat_id, frame) = frame;
                match frame {
                    Some(value) => match serde_json::from_value::<Vec<SessionMessageEntry>>(value) {
                        Ok(entries) => if updates.send(Update::Transcript { chat_id, entries }).is_err() {
                            return SessionEnd::AppGone;
                        },
                        Err(err) => tracing::warn!(error = %err, "dropping malformed transcript frame"),
                    },
                    None => {
                        transcript = None;
                        *transcript_target = None;
                    }
                }
            },

            // The board's pinned orchestrator (gh#104). Additive: an engine too
            // old to serve it never opened the stream, and this arm pends.
            frame = recv_maybe(&mut orchestrator) => match frame {
                Some(value) => {
                    match serde_json::from_value::<board_view::OrchestratorPin>(value) {
                        Ok(pin) => if updates.send(Update::Orchestrator(pin.chat_id)).is_err() {
                            return SessionEnd::AppGone;
                        },
                        Err(err) => tracing::warn!(error = %err, "dropping malformed pin frame"),
                    }
                }
                // Ended without the board's own stream ending — an engine that
                // serves rows and not this. Nothing is pinned as far as this
                // pane can tell, which is the honest thing to draw.
                None => {
                    orchestrator = None;
                    if updates.send(Update::Orchestrator(None)).is_err() {
                        return SessionEnd::AppGone;
                    }
                }
            },

            // An absent board stream pends forever, so it never fires.
            frame = recv_maybe(&mut board) => match frame {
                Some(value) => match decode::<Vec<TaskRow>>(Some(value), "board") {
                    Frame::Value(rows) => {
                        let first = !board_delivered;
                        board_delivered = true;
                        if updates.send(Update::Board(rows)).is_err() {
                            return SessionEnd::AppGone;
                        }
                        // This device really does host the board — say so once,
                        // so the pane's header can stop hedging.
                        if first && updates.send(Update::BoardHostChanged {
                            device: board_host.clone(),
                            live: true,
                        }).is_err() {
                            return SessionEnd::AppGone;
                        }
                    }
                    Frame::Skip => {}
                    Frame::Ended => return SessionEnd::ConnectionLost,
                },
                // The board stream ended. NOT a lost connection: the engine ends
                // it when it hosts no board, and a forwarded one ends when the
                // peer link drops. Drop the slot (a closed receiver would spin
                // this arm) and decide where to look next.
                None => {
                    board = None;
                    // The pin belongs to the board that just went away. Keeping
                    // the stream open would leave the sidebar drawing another
                    // box's glyph the moment the sweep moves on.
                    orchestrator = None;
                    if updates.send(Update::Orchestrator(None)).is_err() {
                        return SessionEnd::AppGone;
                    }
                    let answered = board_delivered;
                    board_delivered = false;
                    // Only an unpinned host that said NOTHING is walked past: a
                    // pinned one is the operator's answer, and one that had been
                    // delivering rows does host the board — it just lost the
                    // stream, and is re-asked in place.
                    let sweep = matches!(board_pin, BoardHost::Auto) && !answered;
                    let next = sweep.then(|| {
                        let candidates = board_view::host_candidates(
                            &devices_seen,
                            local_device.as_deref(),
                        );
                        board_view::next_host_candidate(&candidates, board_host.as_deref())
                    });
                    match next {
                        // Ask the next device now: the sweep already costs one
                        // round-trip each, and a backoff per device would make
                        // finding the box take a visible age.
                        Some(Some(target)) => {
                            board_host = target;
                            (board, orchestrator) =
                                open_board_pair(client, board_host.as_deref()).await;
                        }
                        // Everyone was asked and nobody hosts a board: start the
                        // sweep over after the backoff, since the box may be
                        // booting. A pinned host (or one that dropped mid-
                        // stream) simply waits and is re-asked where it is.
                        Some(None) => {
                            board_host = None;
                            board_retry = Some(tokio::time::Instant::now() + BOARD_RETRY);
                        }
                        None => board_retry = Some(tokio::time::Instant::now() + BOARD_RETRY),
                    }
                    // Reported after the decision, so the header always names
                    // the device actually being asked.
                    if updates.send(Update::BoardHostChanged {
                        device: board_host.clone(),
                        live: false,
                    }).is_err() {
                        return SessionEnd::AppGone;
                    }
                }
            },
        }
    }
}

/// How long before a board host that said nothing is asked again. Long enough
/// that a device with no board costs nothing, short enough that a box finishing
/// its boot is picked up while you are still looking at the pane.
const BOARD_RETRY: Duration = Duration::from_secs(2);

/// Subscribe `WatchBoard` at a device (`None` = this one). A refusal is not
/// fatal — an engine with no board is a normal thing to be pointed at while the
/// sweep is looking, and the caller reads "no stream" the same way it reads "a
/// stream that ended without a frame".
async fn open_board(
    client: &Arc<RpcClient>,
    target: Option<&str>,
) -> Option<mpsc::UnboundedReceiver<serde_json::Value>> {
    subscribe_board(client, target, methods::WATCH_BOARD).await
}

/// The board's rows and its pinned orchestrator, opened at the same device
/// (gh#104).
///
/// One helper so the two can never drift apart: the sweep decides which device
/// hosts the board by whether its row stream answers, and a pin read from
/// anywhere else would be some other box's. Both are best-effort — an older
/// engine that does not serve the pin stream still serves rows, and the pane
/// simply has no glyph to draw.
async fn open_board_pair(
    client: &Arc<RpcClient>,
    target: Option<&str>,
) -> (
    Option<mpsc::UnboundedReceiver<serde_json::Value>>,
    Option<mpsc::UnboundedReceiver<serde_json::Value>>,
) {
    (
        open_board(client, target).await,
        subscribe_board(client, target, methods::WATCH_BOARD_ORCHESTRATOR).await,
    )
}

async fn subscribe_board(
    client: &Arc<RpcClient>,
    target: Option<&str>,
    method: &'static str,
) -> Option<mpsc::UnboundedReceiver<serde_json::Value>> {
    let mut params = serde_json::json!({});
    if let (Some(device), Some(object)) = (target, params.as_object_mut()) {
        object.insert(
            "targetDeviceId".into(),
            serde_json::Value::String(device.to_string()),
        );
    }
    match client.subscribe(method, params).await {
        Ok(stream) => Some(stream),
        Err(err) => {
            tracing::debug!(?target, %method, error = %err, "board stream unavailable");
            None
        }
    }
}

/// Sleep until an optional deadline, pending forever when there is none, so a
/// backoff can sit in the `select!` unconditionally.
async fn wait_until(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// `recv` on an optional stream, pending forever when there is none, so it can
/// sit in the `select!` unconditionally. For streams that carry no chat id.
async fn recv_maybe(
    slot: &mut Option<mpsc::UnboundedReceiver<serde_json::Value>>,
) -> Option<serde_json::Value> {
    match slot {
        Some(stream) => stream.recv().await,
        None => std::future::pending().await,
    }
}

/// Subscribe a chat's transcript. A failure here is not fatal to the session —
/// the chat may have just been deleted.
async fn open_transcript(
    client: &Arc<RpcClient>,
    chat_id: String,
) -> Option<(String, mpsc::UnboundedReceiver<serde_json::Value>)> {
    match client
        .subscribe(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({ "chatId": chat_id }),
        )
        .await
    {
        Ok(stream) => Some((chat_id, stream)),
        Err(err) => {
            tracing::warn!(%chat_id, error = %err, "WatchDocMessages failed");
            None
        }
    }
}

/// `recv` on the optional transcript stream, pending forever when there is
/// none, so it can sit in the `select!` unconditionally.
async fn recv_optional(
    slot: &mut Option<(String, mpsc::UnboundedReceiver<serde_json::Value>)>,
) -> (String, Option<serde_json::Value>) {
    match slot {
        Some((chat_id, stream)) => {
            let frame = stream.recv().await;
            (chat_id.clone(), frame)
        }
        None => std::future::pending().await,
    }
}

/// The three things a watch frame can mean. Kept distinct on purpose: a frame
/// we failed to parse must NOT read as a dropped connection, or one schema skew
/// between engine and viewport would put the app into a reconnect loop against
/// a perfectly healthy daemon.
enum Frame<T> {
    Value(T),
    /// Malformed — logged and ignored; the next snapshot supersedes it anyway.
    Skip,
    /// Stream closed: the engine is gone.
    Ended,
}

fn decode<T: serde::de::DeserializeOwned>(
    frame: Option<serde_json::Value>,
    what: &'static str,
) -> Frame<T> {
    let Some(value) = frame else {
        return Frame::Ended;
    };
    match serde_json::from_value(value) {
        Ok(rows) => Frame::Value(rows),
        Err(err) => {
            tracing::warn!(error = %err, what, "dropping malformed watch frame");
            Frame::Skip
        }
    }
}

fn spawn_call(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    method: &'static str,
    params: serde_json::Value,
    context: &'static str,
) {
    tokio::spawn(async move {
        if let Err(err) = client.call(method, params).await {
            let _ = updates.send(Update::Notice(format!("{context}: {err}")));
        }
    });
}

/// Fetch the model catalogue and hand it up. A failure is a notice, not a
/// silent empty list — an empty picker is indistinguishable from a broken one.
fn spawn_models(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    harness: comet_proto::HarnessId,
) {
    tokio::spawn(async move {
        match client
            .call(
                methods::LIST_MODELS,
                serde_json::json!({ "harness": harness }),
            )
            .await
        {
            Ok(value) => match serde_json::from_value::<Vec<comet_proto::Model>>(value) {
                Ok(models) => {
                    let _ = updates.send(Update::Models(models));
                }
                Err(err) => {
                    let _ = updates.send(Update::Notice(format!("Model list malformed: {err}")));
                }
            },
            Err(err) => {
                let _ = updates.send(Update::Notice(format!("Couldn't list models: {err}")));
            }
        }
    });
}

/// Fetch a space's branches for the ref picker.
fn spawn_refs(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    repo_path: String,
    target_device: Option<String>,
) {
    tokio::spawn(async move {
        let mut params = serde_json::json!({ "repoPath": repo_path });
        if let (Some(device), Some(object)) = (target_device, params.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::Value::String(device));
        }
        match client.call(methods::LIST_REFS, params).await {
            Ok(value) => match serde_json::from_value::<Vec<comet_proto::RepoRef>>(value) {
                Ok(refs) => {
                    let _ = updates.send(Update::Refs(refs));
                }
                Err(err) => {
                    let _ = updates.send(Update::Notice(format!("Branch list malformed: {err}")));
                }
            },
            // A non-git space has no refs; that is not an error worth shouting.
            Err(err) => tracing::debug!(error = %err, "ListRefs unavailable"),
        }
    });
}

/// Materialize a drafted session: worktree (if the plan calls for one), then
/// the chat row, then its first command — in that order, because each step
/// depends on the last.
fn spawn_start_session(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    start: StartSession,
) {
    use comet_proto::view::CheckoutPlan;
    tokio::spawn(async move {
        let mut cwd: Option<String> = None;
        let mut branch: Option<String> = None;

        match &start.plan {
            CheckoutPlan::CurrentCheckout => {}
            CheckoutPlan::ReuseWorktree { path, branch: name } => {
                cwd = Some(path.clone());
                branch = Some(name.clone());
            }
            CheckoutPlan::NewWorktree { base } => {
                branch.clone_from(base);
                if let Some(base) = base {
                    let mut params = serde_json::json!({
                        "repoPath": start.repo_path,
                        "branch": base,
                    });
                    if let (Some(device), Some(object)) =
                        (start.target_device.clone(), params.as_object_mut())
                    {
                        object.insert("targetDeviceId".into(), serde_json::Value::String(device));
                    }
                    match client.call(methods::CREATE_WORKTREE, params).await {
                        Ok(value) => match serde_json::from_value::<comet_proto::Worktree>(value) {
                            Ok(worktree) => cwd = Some(worktree.path),
                            Err(err) => {
                                let _ = updates.send(Update::Notice(format!(
                                    "Worktree reply malformed: {err}"
                                )));
                                return;
                            }
                        },
                        Err(err) => {
                            let _ = updates.send(Update::Notice(format!("Worktree failed: {err}")));
                            return;
                        }
                    }
                }
            }
        }

        let mut mutate = serde_json::json!({
            "op": "createChat",
            "chatId": start.chat_id,
            "spaceId": start.space_id,
        });
        if let Some(object) = mutate.as_object_mut() {
            if let Some(cwd) = &cwd {
                object.insert("cwd".into(), serde_json::Value::String(cwd.clone()));
            }
            if let Some(branch) = &branch {
                object.insert("branch".into(), serde_json::Value::String(branch.clone()));
            }
            if let Some(config) = start.config {
                object.insert("config".into(), config);
            }
        }
        if let Err(err) = client.call(methods::MUTATE, mutate).await {
            let _ = updates.send(Update::Notice(format!(
                "Couldn't create the session: {err}"
            )));
            return;
        }

        // The run's cwd must match what the chat was created with.
        let mut command = start.command;
        if let (Some(cwd), Some(request)) = (
            &cwd,
            command.get_mut("request").and_then(|r| r.as_object_mut()),
        ) {
            request.insert("cwd".into(), serde_json::Value::String(cwd.clone()));
        }
        let params = serde_json::json!({ "chatId": start.chat_id, "command": command });
        match client.call(methods::QUEUE_COMMAND, params).await {
            Ok(_) => {
                let _ = updates.send(Update::SessionStarted {
                    chat_id: start.chat_id,
                });
            }
            Err(err) => {
                let _ = updates.send(Update::SendFailed {
                    chat_id: start.chat_id,
                    message_id: start.message_id,
                    error: err.to_string(),
                });
            }
        }
    });
}

fn spawn_send(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    chat_id: String,
    message_id: String,
    params: serde_json::Value,
) {
    tokio::spawn(async move {
        if let Err(err) = client.call(methods::QUEUE_COMMAND, params).await {
            let _ = updates.send(Update::SendFailed {
                chat_id,
                message_id,
                error: err.to_string(),
            });
        }
    });
}

/// What a release carries beyond the task: the account it spends and who
/// released it (gh#74). A struct rather than five positional strings — they are
/// all `Option<String>` and swapping two would be a silent, wrong dispatch.
struct Dispatch {
    task_id: String,
    identifier: String,
    account: Option<String>,
    via_device: Option<String>,
    via_user: Option<String>,
    replace: bool,
}

/// Release a board task. The reply names the chat the run landed in; the row
/// flips to `working` on the next board frame either way.
///
/// `replace` is the blocked row's retry: the engine ends the live attempt the
/// row is stuck on before releasing, which is the one thing cancel-then-
/// dispatch cannot do atomically.
fn spawn_dispatch(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    dispatch: Dispatch,
    target_device: Option<String>,
) {
    tokio::spawn(async move {
        let Dispatch {
            task_id,
            identifier,
            account,
            via_device,
            via_user,
            replace,
        } = dispatch;
        let mut params = serde_json::json!({ "taskId": task_id });
        if replace && let Some(object) = params.as_object_mut() {
            object.insert("replace".into(), serde_json::Value::Bool(true));
        }
        for (key, value) in [
            ("targetDeviceId", target_device),
            ("account", account),
            ("viaDevice", via_device),
            ("viaUser", via_user),
        ] {
            if let (Some(value), Some(object)) = (value, params.as_object_mut()) {
                object.insert(key.into(), serde_json::Value::String(value));
            }
        }
        // The verb the notice uses, so a retry that ended a live agent does not
        // read like a first release.
        let verb = if replace { "Retried" } else { "Dispatched" };
        match client.call(methods::DISPATCH_TASK, params).await {
            Ok(value) => {
                let chat = value
                    .get("chatId")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .unwrap_or_default();
                let _ = updates.send(Update::Notice(format!(
                    "{verb} {identifier} — chat {chat} is on it"
                )));
            }
            Err(err) => {
                let _ = updates.send(Update::Notice(format!(
                    "Couldn't {} {identifier}: {err}",
                    if replace { "retry" } else { "dispatch" }
                )));
            }
        }
    });
}

/// Fill the dispatch account picker (gh#74): the host's saved logins, narrowed
/// to the ones the row's runtime can actually spend.
///
/// Two calls, because the mapping from a runtime name to a harness is the
/// engine's — `ListBoardRuntimes` carries it, so neither viewport re-implements
/// `harness_for_runtime` and drifts from what the dispatch will validate. A
/// runtime catalog that fails to load is not fatal: the picker then offers every
/// login and the engine refuses a slot the harness cannot use, which is a worse
/// message but a live one.
fn spawn_dispatch_accounts(
    client: Arc<RpcClient>,
    updates: mpsc::UnboundedSender<Update>,
    task_id: String,
    runtime: Option<String>,
    target_device: Option<String>,
) {
    tokio::spawn(async move {
        let params = |value: serde_json::Value| {
            let mut value = value;
            if let (Some(device), Some(object)) = (&target_device, value.as_object_mut()) {
                object.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(device.clone()),
                );
            }
            value
        };
        let snapshot = match client
            .call(methods::LIST_AGENT_ACCOUNTS, params(serde_json::json!({})))
            .await
        {
            Ok(value) => match serde_json::from_value::<comet_proto::AgentAccountsSnapshot>(value) {
                Ok(snapshot) => snapshot,
                Err(err) => {
                    let _ =
                        updates.send(Update::Notice(format!("Account list malformed: {err}")));
                    return;
                }
            },
            Err(err) => {
                let _ = updates.send(Update::Notice(format!("Couldn't list accounts: {err}")));
                return;
            }
        };
        let harness = match runtime {
            Some(runtime) => client
                .call(methods::LIST_BOARD_RUNTIMES, params(serde_json::json!({})))
                .await
                .ok()
                .and_then(|value| {
                    serde_json::from_value::<Vec<board_view::RuntimeOption>>(value).ok()
                })
                .and_then(|options| {
                    options
                        .into_iter()
                        .find(|option| option.name == runtime)
                        .map(|option| option.harness)
                }),
            None => None,
        };
        let accounts = match harness {
            Some(harness) => snapshot
                .accounts
                .into_iter()
                .filter(|account| account.harness == harness)
                .collect(),
            None => snapshot.accounts,
        };
        let _ = updates.send(Update::DispatchAccounts {
            task_id,
            accounts,
            harness,
        });
    });
}
