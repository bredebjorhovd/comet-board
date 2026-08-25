//! comet-rpc — the typed control plane (UiRpc / ControlRpc) over WebSocket + in-memory
//! transports, plus the device-room relay transport ({s,k,to,from} frames — [`device_room`]).
//!
//! Framing: ndjson envelopes, one JSON object per WebSocket text message (or per line on
//! byte transports), matching the shape of comet's Effect RPC without the Effect runtime:
//!
//! - client → server: `{id, method, params}` to invoke, `{id, cancel: true}` to stop a stream;
//! - server → client: `{id, ok}` / `{id, err}` for unary calls,
//!   `{id, item}`* then `{id, done: true}` (or `{id, err}`) for streams.
//!
//! The server dispatches into an [`RpcService`]; the [`RpcClient`] offers `call` and
//! `subscribe`. Both ends run over any pair of string channels, so the in-memory transport
//! ([`memory_client`]) exercises the exact same code path as the WebSocket one.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

mod client;
pub mod device_room;
mod server;

pub use client::{RpcClient, connect_ws};
pub use device_room::{
    DeviceFrameHeader, DeviceLink, HostRelay, HostRelayConfig, LinkCache, LinkCacheConfig,
    NudgeHandler, StaticToken, TokenSource, decode_device_frame, device_room_ws_url,
    encode_device_frame,
};
pub use server::{serve_connection, serve_connection_as, serve_ws_listener};

/// RPC method names — single source of truth for both ends.
/// Full surface: docs/research/feature-inventory.md §2.
pub mod methods {
    pub const LIST_HARNESSES: &str = "ListHarnesses";
    pub const LIST_MODELS: &str = "ListModels";
    /// Which of these command names resolve on THIS device (gh#606). Params:
    /// `{commands: [..]}` → `{found: {"<command>": "<path>"}}` — a command
    /// with no entry, or `null`, resolves nowhere.
    ///
    /// The MCP editors ask it so "the agent will not find this command" is a
    /// warning beside the field rather than an opaque dispatch failure later.
    /// Forwardable, and it has to be: commands resolve where the harness child
    /// spawns — the route's workspace device — and the device asking is
    /// usually somebody's laptop, whose PATH proves nothing about the box.
    pub const LOCATE_COMMANDS: &str = "LocateCommands";
    /// The skills and slash commands a run in this chat could invoke, for the
    /// composer's `/` picker (gh#134). Params: `{chatId?, cwd?, harness?}` →
    /// `[SkillDescriptor]`.
    ///
    /// Forwardable, and it has to be: skills are files, they are files on the
    /// device that hosts the chat, and the answer depends on which agent
    /// account that chat names — a laptop enumerating its own `~/.claude` for a
    /// chat running on the box would offer skills that run cannot invoke.
    pub const LIST_SKILLS: &str = "ListSkills";
    /// Files and directories under the chat's own checkout, for the composer's
    /// `@` picker (gh#424). Params: `{chatId?, cwd?, query, limit?}` →
    /// [`comet_proto::ContextSearch`].
    ///
    /// Forwardable, and it has to be, for a sharper version of the reason
    /// [`LIST_SKILLS`] is: the checkout is a directory on the device that hosts
    /// the chat — a laptop searching its own disk for a chat running on the box
    /// would offer paths that do not exist where the turn runs. Bounded per
    /// call rather than indexed; the reply says when it was truncated.
    pub const SEARCH_CONTEXT_FILES: &str = "SearchContextFiles";
    pub const QUEUE_COMMAND: &str = "QueueCommand";
    pub const WATCH_DOC_MESSAGES: &str = "WatchDocMessages";
    /// Stream: the chat's follow-up plan ([`comet_doc::QueueView`]), current
    /// value first, then every change (gh#424).
    ///
    /// The projection rather than the raw ledger: the plan is a fold over
    /// append-only entries, and a viewport that folded it itself could show a
    /// different plan for the same doc. One fold, on the host, watched like the
    /// transcript is.
    pub const WATCH_DOC_QUEUE: &str = "WatchDocQueue";
    pub const WATCH_CHATS: &str = "WatchChats";
    pub const WATCH_DEVICES: &str = "WatchDevices";
    pub const WATCH_SESSIONS: &str = "WatchSessions";
    /// Spaces registry (device+folder pairs) from the workspace doc.
    pub const WATCH_SPACES: &str = "WatchSpaces";
    /// Entity mutations against the workspace doc (feature-inventory §2 DataRpc).
    /// Params are tagged `{op: createChat|createSpace|renameSpace|deleteSpace|
    /// renameChat|setChatArchived|deleteChat|renameDevice|markChatSeen, …}`.
    pub const MUTATE: &str = "Mutate";
    /// This engine's identity → `{deviceId, version}` (IPC-only; never
    /// relay-forwarded — the answer is about whichever engine you are directly
    /// connected to).
    ///
    /// `version` is the engine binary's own `CARGO_PKG_VERSION`, added in
    /// gh#156 so a CLI can tell whether it shipped with the engine it is
    /// driving. Absent from engines older than that, which is why every reader
    /// treats it as optional.
    pub const LOCAL_DEVICE: &str = "LocalDevice";
    /// Which edge connections this engine holds right now →
    /// [`comet_proto::EdgeHealth`] (gh#116).
    ///
    /// Never relay-forwarded, for the same reason as [`LOCAL_DEVICE`] and one
    /// more: the answer travels over the very socket it reports on, so a
    /// forwarded "am I reachable" is either true or unanswerable. It is asked
    /// locally — over IPC, by `comet status` and `comet-board doctor` — which
    /// is exactly where the gh#116 box was reachable and invisible at once.
    pub const EDGE_HEALTH: &str = "EdgeHealth";
    pub const AUTH_STATUS: &str = "AuthStatus";
    // AuthRpc mutations (feature-inventory §2 AuthRpc; IPC-only).
    pub const SIGN_IN: &str = "SignIn";
    pub const SIGN_IN_HEADLESS: &str = "SignInHeadless";
    pub const COMPLETE_SIGN_IN: &str = "CompleteSignIn";
    pub const SIGN_OUT: &str = "SignOut";
    pub const LIST_ORGS: &str = "ListOrgs";
    pub const CREATE_ORG: &str = "CreateOrg";
    pub const SELECT_ORG: &str = "SelectOrg";
    // Workspace members and invitations (gh#76; edge /auth/orgs/:id/invites).
    pub const LIST_MEMBERS: &str = "ListMembers";
    pub const LIST_INVITES: &str = "ListInvites";
    pub const INVITE_MEMBER: &str = "InviteMember";
    pub const REVOKE_INVITE: &str = "RevokeInvite";
    pub const ACCEPT_INVITE: &str = "AcceptInvite";
    // Repos / worktrees / folders (ControlRpc, relay-forwardable).
    pub const LIST_REPOS: &str = "ListRepos";
    pub const ADD_REPO: &str = "AddRepo";
    pub const CLONE_REPO: &str = "CloneRepo";
    pub const CREATE_REPO: &str = "CreateRepo";
    pub const LIST_BRANCHES: &str = "ListBranches";
    pub const LIST_REFS: &str = "ListRefs";
    pub const SWITCH_REF: &str = "SwitchRef";
    pub const LIST_FOLDERS: &str = "ListFolders";
    pub const CREATE_WORKTREE: &str = "CreateWorktree";
    /// Cut an attempt's checkout on a device that is not the one hosting the
    /// board (gh#558). Params `{repoPath, branch, base}` → a `Worktree`.
    ///
    /// Distinct from [`CREATE_WORKTREE`], which generates the branch name and
    /// treats its `branch` param as a *start point*. This one is the board's
    /// dispatch path: the branch name comes from `routing.toml`'s
    /// `branch_template` and is exact, and a fresh branch starts at `base`
    /// **fetched from origin** (gh#67) rather than at whatever the space folder
    /// was last left on.
    ///
    /// It exists because a route names a space, a space is a device+folder
    /// pair, and one board is supposed to be able to drive both machines: the
    /// box polls `comet-board` and dispatches into the Mac's checkout, because
    /// comet-board has iOS and macOS targets that cannot build on Linux. The
    /// board's chat, brief and command ledger are all workspace-doc writes that
    /// already land on the space's own device; the checkout was the one step
    /// still being taken on whichever box happened to host the board, against a
    /// path that does not exist there.
    pub const CREATE_BOARD_WORKTREE: &str = "CreateBoardWorktree";
    pub const DELETE_WORKTREE: &str = "DeleteWorktree";
    /// Fork a conversation from one of its messages (gh#425). Params are a
    /// [`comet_proto::ForkRequest`] → [`comet_proto::ForkResult`].
    ///
    /// Forwardable, and it must be: the transcript, the checkout and the
    /// provider session all live on the device that hosts the source chat, and
    /// the person forking is usually at a laptop that is not it.
    ///
    /// One call rather than the `CreateWorktree` + `Mutate createChat` +
    /// `QueueCommand` sequence a new chat is made of, for two reasons. The
    /// engine is the only party that can honestly decide whether the
    /// destination resumes the provider's session or carries a copy of the
    /// visible transcript — it holds the harness session id, the cwd it was
    /// created under, and the transcript itself. And a worktree cut for a fork
    /// that then fails has to be reclaimed by whoever cut it: split across
    /// three calls, a client that dies in the middle leaks a branch and a
    /// directory nobody will look for.
    pub const FORK_CHAT: &str = "ForkChat";
    // Terminals (ControlRpc, relay-forwardable; SubscribeTerminal streams).
    pub const OPEN_TERMINAL: &str = "OpenTerminal";
    pub const SUBSCRIBE_TERMINAL: &str = "SubscribeTerminal";
    pub const WRITE_TERMINAL: &str = "WriteTerminal";
    pub const RESIZE_TERMINAL: &str = "ResizeTerminal";
    pub const CLOSE_TERMINAL: &str = "CloseTerminal";
    /// Checkout-diff stream for the target device's chats (DataRpc,
    /// relay-forwardable — diffs are produced where the checkout lives).
    pub const WATCH_CHECKOUT_DIFFS: &str = "WatchCheckoutDiffs";
    // Agent accounts (ControlRpc, relay-forwardable — CLI logins are per-device).
    pub const LIST_AGENT_ACCOUNTS: &str = "ListAgentAccounts";
    pub const ACTIVATE_AGENT_ACCOUNT: &str = "ActivateAgentAccount";
    pub const FORGET_AGENT_ACCOUNT: &str = "ForgetAgentAccount";
    pub const START_AGENT_LOGIN: &str = "StartAgentLogin";
    pub const COMPLETE_AGENT_LOGIN: &str = "CompleteAgentLogin";
    pub const POLL_AGENT_LOGIN: &str = "PollAgentLogin";
    pub const CANCEL_AGENT_LOGIN: &str = "CancelAgentLogin";
    /// Per-slot credential freshness, verified against the provider where a
    /// timestamp cannot answer (gh#576). Params: `{accountId?}` →
    /// `Vec<comet_proto::AgentAccountHealth>`; omit `accountId` for every
    /// account on the device. What `comet-board doctor`'s per-login lines and
    /// `comet-board relogin`'s final verdict are made of.
    pub const VERIFY_AGENT_ACCOUNTS: &str = "VerifyAgentAccounts";
    // Uploads / attachments (ControlRpc, relay-forwardable — target the chat's host device).
    pub const UPLOAD_CHUNK: &str = "UploadChunk";
    pub const UPLOAD_COMMIT: &str = "UploadCommit";
    pub const READ_ATTACHMENT_CHUNK: &str = "ReadAttachmentChunk";
    // Board (comet-board fork addition, ControlRpc, relay-forwardable — gh#55).
    // The board store lives on ONE device — wherever the board service runs,
    // usually the always-on box — so every one of these targets that device:
    // `targetDeviceId` = the box makes a teammate's laptop drive the same board
    // over the relay instead of needing a board (or an SSH session) of its own.
    /// Stream: current board rows (tasks + attempts, board order), then every
    /// change. The `list --json` shape from herdr-board, one row per task.
    pub const WATCH_BOARD: &str = "WatchBoard";
    /// Release a task: cut the worktree, create the chat on the route's space,
    /// queue the brief. Params: `{taskId, via?, runtime?, model?}` — `via` is
    /// the dispatching chat's id when an agent released it (provenance, never
    /// authority); `runtime`/`model` override the route's configured runtime
    /// and the harness's default model for this dispatch.
    pub const DISPATCH_TASK: &str = "DispatchTask";
    /// The runtimes a dispatch can be pointed at, for pickers: one canonical
    /// name + label per harness, the set the engine validates overrides
    /// against. Params: `{}` → `[{name, label}]`.
    pub const LIST_BOARD_RUNTIMES: &str = "ListBoardRuntimes";
    /// What the board knows about its own throughput (gh#143). Params:
    /// `{sinceDays?}` → `comet_proto::view::stats::BoardStats`; omit
    /// `sinceDays` for all time.
    ///
    /// A call and not a stream, like [`READ_BOARD_TASK`]: these numbers are
    /// read when somebody opens the page and are stale by a poll interval at
    /// worst, and streaming a full aggregate on every board tick would cost
    /// every connected viewport a recompute nobody is looking at.
    pub const BOARD_STATS: &str = "BoardStats";
    /// One board's stats with its durable store identity and the lossless merge
    /// basis used by [`AGGREGATE_BOARD_STATS`]. Relay-forwardable, but normally
    /// only called by another engine's bounded fan-out.
    pub const BOARD_STATS_SNAPSHOT: &str = "BoardStatsSnapshot";
    /// On-demand union of every board host this engine can currently ask.
    /// Params: `{sinceDays?}` →
    /// `comet_proto::view::stats::AggregateBoardStats`.
    ///
    /// Deliberately not relay-forwardable: whichever engine receives it is the
    /// collector and performs one concurrent, five-second-bounded read per
    /// non-iOS device. No background polling or edge persistence is involved.
    pub const AGGREGATE_BOARD_STATS: &str = "AggregateBoardStats";
    /// Stream: which chat takes this board's stray notices (gh#104, renamed in
    /// gh#348), current value first, then every change. Params: `{}` →
    /// `{chatId}`.
    ///
    /// Separate from [`WATCH_BOARD`] because it answers a question about the
    /// board rather than about the work on it, and every surface that renders
    /// the mark — the session list, on both viewports — needs it whether or not
    /// a board panel is open. Written through [`WRITE_BOARD_CONFIG`], like
    /// every other `routing.toml` key: one writer discipline, not two.
    ///
    /// **The wire name is frozen at the pre-gh#348 spelling.** A method name is
    /// an identifier, not a description: a phone installed before the rename
    /// subscribes by this exact string, and renaming it would take the ◆ off
    /// that phone's session list until somebody shipped it a new build — a real
    /// cost for no gain to any reader. The vocabulary the rename is about lives
    /// in the config key, the `doctor` line and every word a person sees.
    pub const WATCH_BOARD_FALLBACK: &str = "WatchBoardOrchestrator";
    /// End a task's live attempt (interrupt + archive the chat). The issue
    /// stays open: cancel ends attempts, never tasks. Params: `{taskId}`.
    pub const CANCEL_TASK: &str = "CancelTask";
    /// The issue text behind one row, for the detail surface (gh#132). Params:
    /// `{taskId}` → `{id, body}`.
    ///
    /// A call rather than a field on the streamed row, deliberately.
    /// [`WATCH_BOARD`] republishes every row on every sync cycle; a hundred
    /// issue bodies riding along would make each frame two orders of magnitude
    /// larger, relayed to a phone, to draw one truncated line. This is read
    /// when somebody opens a row, and only that row's.
    pub const READ_BOARD_TASK: &str = "ReadBoardTask";
    /// Record what a dispatched agent says its attempt did, file-anchored
    /// (§gh#183). Params: `{taskId, text}` → the same `AttemptReview`
    /// [`READ_ATTEMPT_REVIEW`] answers.
    ///
    /// `text` is the raw block, parsed on the board's host and not by the
    /// caller: the refusal — a claim with no file anchor is prose, not a claim
    /// — is the contract, and a contract enforced in the client is a contract
    /// the next client does not have. The reply carries the remainder so the
    /// agent learns what it did not account for while it can still act on it.
    pub const SUBMIT_CLAIMS: &str = "SubmitClaims";
    /// One attempt's review (§gh#183): the brief, the agent's claims, the
    /// evidence the board observed for itself, and the changes no claim
    /// accounts for. Params: `{taskId, attempt?}` — omit `attempt` for the
    /// task's latest.
    ///
    /// A call and not a stream, like [`READ_BOARD_TASK`]: it reads a diff out
    /// of a checkout, and it is read when somebody opens a review.
    pub const READ_ATTEMPT_REVIEW: &str = "ReadAttemptReview";
    /// Submit a verdict on an attempt's pull request (§gh#239): record it, hand
    /// it to the agent still standing in the checkout, *and* project it onto
    /// the pull request. Params: `{taskId, attempt?, kind, comment}` — `kind`
    /// is `comment` | `approve` | `changes_requested` — → a
    /// [`comet_board::verdict::VerdictReceipt`].
    ///
    /// The unclaimed set is not a parameter. It is recomputed on the board's
    /// host from the diff and attached to both copies, because a reviewer
    /// cannot be asked to retype it and a caller that could supply it could
    /// also get it wrong.
    ///
    /// A GitHub that refuses the review is **not** an error here (gh#365). The
    /// verdict is a board fact and stands; the receipt's `projection` says what
    /// became of the copy on the pull request, and `refused` carries GitHub's
    /// words. Only a verdict the board itself will not take — a closed pull
    /// request, an empty `comment` or `changes requested` — fails the call.
    ///
    /// Idempotent on `{attempt, kind, comment}`: a retry finishes whichever
    /// half failed rather than posting a second review. A caller that times out
    /// should re-send exactly what it sent.
    pub const SUBMIT_VERDICT: &str = "SubmitVerdict";
    /// Merge the pull request on a board row (gh#408): submit GitHub's
    /// asynchronous merge, wait out the poll, and answer what actually happened
    /// — merged, queued, or still running (gh#290). Params: `{taskId}` →
    /// `{line}`, one sentence a surface can show verbatim: `o/r#87 merged`,
    /// `o/r#87 is in the merge queue`.
    ///
    /// Only ever called from an explicit keypress with a confirmation. The
    /// confirmation is the *caller's* job —
    /// `comet_proto::view::board::merge_confirmation` is its wording — because
    /// merging a layer of a stack merges every open layer beneath it as one
    /// group, and the reader has to be told which ones before the call, not
    /// after.
    ///
    /// A merge that entered the queue or is still running when the wait is
    /// over leaves the row where it is: the same poll that notices a merge
    /// made on the web is what moves it, so the board never records a merge
    /// GitHub can still reject.
    pub const MERGE_TASK: &str = "MergeTask";
    /// The board's auto-pick rules with their derived health and recent
    /// history (gh#490). Params: `{}` →
    /// `comet_board::autopick::AutomationsView`.
    ///
    /// A call and not a stream, like [`BOARD_STATS`]: the page and the board
    /// header's popover read it when opened, and history rows are not worth a
    /// republish to every viewport per sync tick. The rules themselves are
    /// `routing.toml` — created, edited, paused and deleted through
    /// [`WRITE_BOARD_CONFIG`]'s `automation*` ops, one writer discipline —
    /// so this carries what the config cannot: live counts, budget meters,
    /// health, and why each considered task was or was not dispatched. Never
    /// credentials or webhook material.
    pub const READ_BOARD_AUTOMATIONS: &str = "ReadBoardAutomations";
    /// The board's `routing.toml` as it stands on its host: the text, its
    /// parse, and everything wrong with it — plus the repos that have a space
    /// on that device but nothing on the board watching them (gh#75). Params:
    /// `{}` → `{routing: RoutingView, unadopted: [Unadopted]}`.
    ///
    /// Reading it is why this is not just a file: the config lives on the box,
    /// and everyone who is not the person who set the box up has no shell on
    /// it.
    pub const READ_BOARD_CONFIG: &str = "ReadBoardConfig";
    /// Change `routing.toml`, validated. Params are tagged `{op: text|route|
    /// default|adopt|ignore, …}`; the reply is the same shape
    /// [`READ_BOARD_CONFIG`] returns, so a caller never has to guess what it
    /// just wrote.
    ///
    /// Every op re-parses and re-validates the whole file before it lands and
    /// leaves the previous contents in `routing.toml.bak`. An edit that would
    /// break the config is refused, naming what it would have broken, and the
    /// file is untouched.
    ///
    /// `adopt` additionally asks the other devices on the account what *they*
    /// poll, and refuses a repo one of them already does (gh#343). `force:
    /// true` writes it anyway — two boards over one repo is a choice on a board
    /// where nobody dispatches, and a race everywhere else.
    pub const WRITE_BOARD_CONFIG: &str = "WriteBoardConfig";
    /// Put a repo the board has never seen on the board, in one call (gh#97):
    /// resolve it against the board's GitHub credential, clone it on *this*
    /// device with that credential, create the space, and adopt it. Params:
    /// `{slug, dir?, labels?, force?}` → `Onboarded`.
    ///
    /// A repo another board on this account already polls is refused between
    /// the resolution and the clone, so nothing is left on the disk by a
    /// refusal (gh#343); `force: true` onboards it anyway.
    ///
    /// Forwardable, and that is the whole point: the clone, the space and the
    /// config all belong to the board's host, and the person onboarding a repo
    /// is usually sitting at a laptop that is not it — with no GitHub credential
    /// of its own, which is why even the *resolution* happens over there.
    ///
    /// Idempotent at every step: an existing checkout of the same repo is
    /// reused, an existing space for that path is reused, and a repo already
    /// polled and routed is left alone. A directory holding something else is
    /// refused rather than cloned over.
    pub const ONBOARD_REPO: &str = "OnboardRepo";
    /// The repos the board's GitHub App can see, and which of them are already
    /// on the board (gh#97). Params: `{}` → `[{slug, private, archived,
    /// onBoard, …}]`.
    ///
    /// What an "Onboard a repo…" picker offers. Deliberately the App's grant
    /// rather than the operator's repos: it is exactly the set a clone can
    /// authenticate for and the sync loop can poll, so a repo missing from it is
    /// a repo somebody has to go and install the App on.
    pub const LIST_APP_REPOS: &str = "ListAppRepos";
    /// Everything a repo-first space picker needs, from the board's host, in one
    /// call (gh#118). Params: `{}` → `{deviceId, spaces: [{spaceId, slug}],
    /// repos: [Candidate], reposNote?}`.
    ///
    /// Two halves that only the host can answer, and answering them separately
    /// would mean two round trips from a phone: which of *its* spaces are
    /// checkouts of which GitHub repo (git, on its disk), and which repos its
    /// App can see ([`LIST_APP_REPOS`], its credential). The picker joins them
    /// against the workspace doc it already holds.
    ///
    /// `repos` degrades to empty with a `reposNote` rather than failing the
    /// call: a board on a `GITHUB_TOKEN` has no installations to enumerate, and
    /// its spaces are still spaces. Refused outright by a device that hosts no
    /// board — the same "said nothing at all" contract the host sweep rules
    /// candidates out with.
    pub const LIST_REPO_SPACES: &str = "ListRepoSpaces";
    // Updates (ControlRpc, relay-forwardable — a device reports/applies its own
    // binary's update). Stream: current UpdateStatus, then every change.
    pub const UPDATE_STATUS: &str = "UpdateStatus";
    /// Download + apply the newest release on the target device (symlink-managed
    /// installs; the service restart is scheduled after the reply flushes).
    pub const APPLY_UPDATE: &str = "ApplyUpdate";
}

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("unknown method: {0}")]
    UnknownMethod(String),
    #[error("bad params: {0}")]
    BadParams(String),
    #[error("{0}")]
    Failed(String),
    /// The device answered, and the answer is *no*: it does not host what the
    /// call is about (gh#155). Distinct from [`RpcError::Failed`] because a
    /// caller sweeping devices has to tell "not me" apart from "I never got
    /// asked" — a laptop that hosts no board and a box behind a broken relay
    /// are the same `Err` otherwise, and a picker that treats both as "not a
    /// host" shows a short list as the whole truth.
    #[error("{0}")]
    Refused(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("connection closed")]
    Closed,
}

impl RpcError {
    /// The wire tag for the variants a caller distinguishes across the
    /// connection. `None` = the ordinary failure, which needs no tag.
    ///
    /// Only refusals are tagged: transport-shaped errors are produced by
    /// whichever hop noticed, never carried, so they have no wire form to keep.
    pub fn code(&self) -> Option<&'static str> {
        match self {
            RpcError::Refused(_) => Some(codes::REFUSED),
            _ => None,
        }
    }

    /// Rebuild an error from a peer's `{err, code}` — the inverse of
    /// [`RpcError::code`]. An unknown (or absent) code is an ordinary failure,
    /// so an older peer degrades to today's behaviour rather than breaking.
    pub fn from_wire(code: Option<&str>, message: String) -> Self {
        match code {
            Some(codes::REFUSED) => RpcError::Refused(message),
            _ => RpcError::Failed(message),
        }
    }
}

/// Wire tags for [`RpcError::code`].
pub mod codes {
    /// [`RpcError::Refused`]: the device answered, and the answer is "not me".
    pub const REFUSED: &str = "refused";
}

/// A client-originated frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientFrame {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub params: serde_json::Value,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancel: bool,
}

/// A server-originated frame. Exactly one of `ok` / `err` / `item` / `done` is meaningful.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerFrame {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub err: Option<String>,
    /// Which *kind* of error `err` is, when the kind is one a caller acts on
    /// ([`RpcError::code`], gh#155). Absent on ordinary failures and on frames
    /// from peers that predate the tag, both of which read as
    /// [`RpcError::Failed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

/// What a service returns for one invocation.
pub enum RpcReply {
    /// Unary response — sent as `{id, ok}`.
    Value(serde_json::Value),
    /// Stream — each item sent as `{id, item}`, then `{id, done: true}` when it ends.
    Stream(BoxStream<'static, serde_json::Value>),
}

impl RpcReply {
    /// Serialize a value into a unary reply.
    pub fn value<T: Serialize>(value: &T) -> Result<Self, RpcError> {
        serde_json::to_value(value)
            .map(RpcReply::Value)
            .map_err(|e| RpcError::Failed(format!("serialize response: {e}")))
    }
}

/// Who is on the other end of a connection, as far as the *transport* can
/// prove it (gh#161).
///
/// Not a claim and not a parameter: the relay stamps this onto every
/// client→host frame from the identity the edge Worker verified before the
/// frame reached the Durable Object (`edge/src/device-room.ts`,
/// `AUTH_USER_HEADER`), and nothing a client puts in a frame header or in
/// `params` can reach it. A handler may therefore compare it against its own
/// records; a handler may not accept a substitute for it.
///
/// [`Caller::LOCAL`] — both identity fields unset — is the *absence* of a relay
/// stamp. It says the call reached the device over localhost IPC; it does not
/// say a person pressed the button. A dispatched agent and repository setup
/// process can both reach localhost, so security-sensitive host approval needs
/// the separate, in-process [`Caller::OPERATOR`] authority.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caller {
    /// The verified WorkOS user id the relay stamped, if this call came over
    /// one. Never an email — the edge verifies a JWT `sub`, and putting a name
    /// to it is the receiving device's job.
    pub user: Option<String>,
    /// The verified org claim that rode with it, when the caller's session
    /// carried one.
    pub org: Option<String>,
    /// True only for the embedded desktop's in-process transport. Never read
    /// from a frame or request parameter, and never set by localhost or relay
    /// transports: code running on the box cannot mint an operator click by
    /// opening the public IPC port.
    operator: bool,
}

impl Caller {
    /// No relay stamp: this device's own IPC or in-process transport.
    pub const LOCAL: Caller = Caller {
        user: None,
        org: None,
        operator: false,
    };

    /// A person operating the embedded desktop on the checkout host. This is
    /// deliberately not the caller used by [`memory_client`]; tests and other
    /// in-process adapters remain ordinary local callers unless they opt into
    /// the operator transport explicitly.
    pub const OPERATOR: Caller = Caller {
        user: None,
        org: None,
        operator: true,
    };

    /// The identity the edge verified, or `None` for a local call.
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref().filter(|u| !u.is_empty())
    }

    /// Did this call arrive over the relay with a verified identity on it?
    pub fn is_verified(&self) -> bool {
        self.user().is_some()
    }

    /// Can this caller make a host trust decision which repository code must
    /// not be able to relay through localhost?
    pub fn is_operator(&self) -> bool {
        self.operator
    }

    /// A relay-authenticated caller. The transport owns construction so a
    /// request body can never smuggle either identity or operator authority.
    pub fn relayed(user: Option<String>, org: Option<String>) -> Caller {
        Caller {
            user,
            org,
            operator: false,
        }
    }
}

/// Server-side dispatch: one implementation serves every transport.
#[async_trait]
pub trait RpcService: Send + Sync + 'static {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError>;

    /// Same, told who the transport verified the caller to be.
    ///
    /// A second method rather than a changed signature because almost nothing
    /// wants this: the default drops the identity and calls [`handle`], which
    /// is exactly right for every handler whose answer does not depend on who
    /// asked. A service that *does* care (the engine, for `DispatchTask`)
    /// overrides this and implements `handle` as `handle_as(…, &Caller::LOCAL)`
    /// — so the local path keeps its meaning instead of being an unstamped
    /// remote one.
    ///
    /// [`handle`]: RpcService::handle
    async fn handle_as(
        &self,
        method: &str,
        params: serde_json::Value,
        caller: &Caller,
    ) -> Result<RpcReply, RpcError> {
        let _ = caller;
        self.handle(method, params).await
    }
}

/// Deserialize typed params out of the envelope's `params` value.
pub fn parse_params<T: serde::de::DeserializeOwned>(
    params: serde_json::Value,
) -> Result<T, RpcError> {
    serde_json::from_value(params).map_err(|e| RpcError::BadParams(e.to_string()))
}

/// Spawn an in-memory server for `service` and return a connected client.
/// Same envelopes, same dispatch loop as the WebSocket path — the in-process UI
/// transport (ARCHITECTURE §1 "zero serialization shortcuts").
pub fn memory_client(service: Arc<dyn RpcService>) -> RpcClient {
    let (client_out, server_in) = tokio::sync::mpsc::channel::<String>(256);
    let (server_out, client_in) = tokio::sync::mpsc::channel::<String>(256);
    tokio::spawn(serve_connection(service, server_out, server_in));
    RpcClient::new(client_out, client_in)
}

/// The embedded desktop's transport. It speaks the same envelopes as every
/// other client, but the server stamps the connection as a physical operator
/// surface. Keeping this constructor separate makes the authority visible at
/// the one place it is granted.
pub fn operator_memory_client(service: Arc<dyn RpcService>) -> RpcClient {
    let (client_out, server_in) = tokio::sync::mpsc::channel::<String>(256);
    let (server_out, client_in) = tokio::sync::mpsc::channel::<String>(256);
    tokio::spawn(serve_connection_as(
        service,
        server_out,
        server_in,
        Caller::OPERATOR,
    ));
    RpcClient::new(client_out, client_in)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    struct TestService;

    #[async_trait]
    impl RpcService for TestService {
        async fn handle(
            &self,
            method: &str,
            params: serde_json::Value,
        ) -> Result<RpcReply, RpcError> {
            match method {
                "Echo" => Ok(RpcReply::Value(params)),
                "Count" => {
                    let n = params.get("n").and_then(|v| v.as_u64()).unwrap_or(0);
                    Ok(RpcReply::Stream(
                        futures::stream::iter((0..n).map(|i| serde_json::json!(i))).boxed(),
                    ))
                }
                "Never" => Ok(RpcReply::Stream(futures::stream::pending().boxed())),
                "Boom" => Err(RpcError::Failed("boom".into())),
                "NotMe" => Err(RpcError::Refused("not me".into())),
                other => Err(RpcError::UnknownMethod(other.into())),
            }
        }
    }

    #[tokio::test]
    async fn memory_call_stream_and_error() {
        let client = memory_client(Arc::new(TestService));

        let echoed = client
            .call("Echo", serde_json::json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(echoed, serde_json::json!({"x": 1}));

        let mut items = client
            .subscribe("Count", serde_json::json!({"n": 3}))
            .await
            .unwrap();
        let mut seen = Vec::new();
        while let Some(v) = items.recv().await {
            seen.push(v);
        }
        assert_eq!(
            seen,
            vec![
                serde_json::json!(0),
                serde_json::json!(1),
                serde_json::json!(2)
            ]
        );

        let err = client
            .call("Boom", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Failed(m) if m == "boom"));
    }

    #[tokio::test]
    async fn websocket_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(serve_ws_listener(listener, Arc::new(TestService)));

        let client = connect_ws(&format!("ws://127.0.0.1:{port}")).await.unwrap();
        let echoed = client
            .call("Echo", serde_json::json!("hello"))
            .await
            .unwrap();
        assert_eq!(echoed, serde_json::json!("hello"));

        let mut items = client
            .subscribe("Count", serde_json::json!({"n": 2}))
            .await
            .unwrap();
        assert_eq!(items.recv().await, Some(serde_json::json!(0)));
        assert_eq!(items.recv().await, Some(serde_json::json!(1)));
        assert_eq!(items.recv().await, None);
    }

    /// gh#155: a device sweep has to tell "I host no board" from "the call
    /// never landed", and both cross the wire as a string today. The kind rides
    /// along on the frame, so a refusal arrives as a refusal — and an ordinary
    /// failure does not become one.
    #[tokio::test]
    async fn a_refusal_stays_a_refusal_across_the_wire() {
        let client = memory_client(Arc::new(TestService));
        let err = client
            .call("NotMe", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Refused(m) if m == "not me"));

        let err = client
            .call("Boom", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, RpcError::Failed(_)));

        // An older peer sends no code at all; its errors read as ordinary
        // failures rather than as refusals.
        assert!(matches!(
            RpcError::from_wire(None, "whatever".into()),
            RpcError::Failed(_)
        ));
        assert!(matches!(
            RpcError::from_wire(Some("something-newer"), "whatever".into()),
            RpcError::Failed(_)
        ));
    }

    #[tokio::test]
    async fn dropping_stream_receiver_cancels_server_side() {
        let client = memory_client(Arc::new(TestService));
        let items = client
            .subscribe("Never", serde_json::Value::Null)
            .await
            .unwrap();
        drop(items);
        // The next unary call still works — the dead stream didn't wedge the connection.
        let echoed = client.call("Echo", serde_json::json!(2)).await.unwrap();
        assert_eq!(echoed, serde_json::json!(2));
    }
}
