//! EngineRpc — the engine-side `RpcService`: sessions + docs + the workspace-doc
//! entity surface.
//!
//! Methods (feature-inventory §2):
//! - `ListHarnesses` → `[HarnessDescriptor]`
//! - `ListModels {harness}` → `[Model]`
//! - `QueueCommand {chatId, command}` → `{commandId}` (durable doc command)
//! - `WatchDocMessages {chatId}` → stream of joined `SessionMessageEntry[]`,
//!   re-emitted on every doc change
//! - `WatchChats` / `WatchDevices` → streams of the workspace doc's entity rows
//! - `WatchSessions` → stream of `Session[]`: this engine's live statuses merged with
//!   remote devices' workspace session rows
//! - `Mutate {op, …}` → `{ok}` — workspace entity mutations (createChat, renameChat,
//!   setChatArchived, deleteChat, renameDevice, markChatSeen)
//! - `LocalDevice` → `{deviceId}` — this engine's identity (never forwarded)
//! - AuthRpc (feature-inventory §2): `AuthStatus` (stream), `SignIn`/`SignInHeadless` →
//!   `{url}`, `CompleteSignIn {code}`, `SignOut`, `ListOrgs`, `CreateOrg {name}`,
//!   `SelectOrg {organizationId}`
//! - Repos (§3.5): `ListRepos`, `AddRepo {path}`, `CloneRepo {url}`,
//!   `CreateRepo {name}`, `ListBranches {repoPath}` (default branch first),
//!   `ListFolders {path?}`, `CreateWorktree {repoPath, branch}`, `DeleteWorktree
//!   {repoPath, worktreePath}`; `WatchCheckoutDiffs` → stream of `CheckoutDiff[]`
//! - Terminals (§3.4): `OpenTerminal {chatId, cols, rows}` → `TerminalSession`,
//!   `SubscribeTerminal {terminalId, afterSeq?}` → stream of `TerminalEvent`
//!   (replay then live tail), `WriteTerminal {terminalId, data}`, `ResizeTerminal`,
//!   `CloseTerminal`. M5 is single-user local: per-user owner checks land with
//!   real multi-account auth in M6.
//! - Agent accounts (§3.7): `ListAgentAccounts {forceUsage?}` →
//!   `AgentAccountsSnapshot`, `ActivateAgentAccount`/`ForgetAgentAccount`
//!   `{harness, accountId}` → snapshot, `StartAgentLogin {harness}` →
//!   `{loginId, url, mode}`, `CompleteAgentLogin {loginId, code}` → snapshot,
//!   `PollAgentLogin {loginId}`, `CancelAgentLogin {loginId}`.
//! - Board (comet-board fork, docs/BOARD.md): `WatchBoard` → stream of board
//!   rows (herdr-board's `list --json` shape), `DispatchTask {taskId, via?,
//!   runtime?, model?, account?, bill?}` → `{chatId, cwd, attempt}` — `bill` is
//!   the acknowledgement `billing_guard = "require-own"` wants for a run that
//!   spends somebody else's subscription (gh#101), `CancelTask {taskId}` →
//!   `{ok}`, `ListBoardRuntimes` → `[{name, label}]`. Served off the
//!   engine-hosted board service, and relay-forwardable (gh#55): one box hosts
//!   the board, every teammate's device drives it with `targetDeviceId`.
//! - Board config (gh#75): `ReadBoardConfig` → `{routing, unadopted}` — the
//!   host's `routing.toml`, its parse, everything wrong with it, and the repos
//!   with a space there that nothing on the board watches; `WriteBoardConfig
//!   {op: text|route|default|adopt|ignore, …}` → the same shape, after a write
//!   that had to parse and validate and left a `.bak`. Forwardable for the
//!   reason the file is worth reaching at all: it lives on the box.
//! - Uploads (§3.7): `UploadChunk {uploadId, data, seq?}`,
//!   `UploadCommit {uploadId, fileName}` → `{path}`,
//!   `ReadAttachmentChunk {path, offset}` → `{name, mimeType, data, nextOffset,
//!   done}` (path-jailed to the uploads dir + workspace-known chat cwds).
//!
//! ## Device-addressed routing (`targetDeviceId`, feature-inventory §2.1)
//!
//! ControlRpc methods are relay-forwardable: params may carry `targetDeviceId`. When it
//! names another device, the call is forwarded verbatim over that device's relay DO via
//! the [`LinkCache`] — the remote engine sees its own id and handles locally, so the
//! forward can never loop. Streaming methods are proxied by re-subscribing remotely and
//! piping items. To make another method device-addressable, nothing per-method is needed
//! beyond listing it in [`forwardable`] (and [`is_stream_method`] if it streams);
//! handlers stay transport-agnostic. Currently routed: `ListHarnesses`, `ListModels`,
//! `QueueCommand`, `WatchDocMessages`, and the board four (`WatchBoard`,
//! `DispatchTask`, `CancelTask`, `ListBoardRuntimes`) — see [`forwardable`] for the
//! full list.

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Deserialize;
use tokio::sync::watch;

use comet_doc::SessionCommandPayload;
use comet_proto::{ChatConfig, HarnessId};
use comet_rpc::{Caller, LinkCache, RpcError, RpcReply, RpcService, methods, parse_params};

use crate::agent_accounts::AgentAccounts;
use crate::auth::Auth;
use crate::diff_sync::CheckoutDiffSync;
use crate::doc_host::DocHost;
use crate::registry::HarnessRegistry;
use crate::repos::{Repos, home_dir};
use crate::sessions::SessionsEngine;
use crate::terminals::Terminals;
use crate::uploads::Uploads;
use crate::workspace_host::WorkspaceHost;

use comet_board::dispatch::{DispatchOrigin, DispatchOverrides, VerifiedCaller};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChatParams {
    chat_id: String,
}

/// `WriteBoardConfig` params (gh#75).
///
/// The file edits are [`comet_board::routes::Edit`] verbatim — one definition,
/// so the CLI and a settings page send what the board crate's tests exercise.
/// The two that cannot live there are here: adopting and ignoring a repo need
/// the space list and a git probe, which are the engine's to supply and only
/// mean anything on the device the board runs on.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum BoardConfigWrite {
    /// Write the `[[route]]` + `[github] repos` halves a repo is missing, with
    /// the same writer `comet-board adopt` uses. `labels` narrows what the repo
    /// contributes (`Some([])` is "every open issue, said out loud").
    Adopt {
        slug: String,
        #[serde(default)]
        labels: Option<Vec<String>>,
    },
    /// Stop offering a repo — you are only reading it.
    Ignore { slug: String },
    /// Map a teammate's sign-in email to their GitHub identity — the `[users]`
    /// entry `comet-board member add` writes (gh#162).
    ///
    /// Here rather than in `routes::Edit` for the same reason `Adopt` is: a
    /// bare `--github ana` has to be resolved into the address GitHub minted
    /// for that login, and the credential that can ask is the *board's*. The
    /// laptop running the command usually has none — which is the whole reason
    /// the map was a hand-edit over ssh.
    ///
    /// A `--github` that is already an address needs no round trip and takes
    /// none. Removal is [`comet_board::routes::Edit::User`] with a null value:
    /// there is nothing to resolve when taking somebody out.
    Member {
        user: String,
        github: String,
        /// The name their commits show. Absent leaves the GitHub login, which
        /// is what the address would have derived anyway.
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(untagged)]
    Edit(comet_board::routes::Edit),
}

/// `OnboardRepo` (gh#97): a repo the board has never seen, in one call.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OnboardParams {
    /// `owner/repo`, or any GitHub URL carrying one.
    slug: String,
    /// Where the checkout goes **on this device**. Absent = the engine's own
    /// clone root; `~` is expanded here, against this device's home.
    #[serde(default)]
    dir: Option<String>,
    /// Poll only these labels. `Some([])` is "every open issue, said out loud";
    /// absent keeps `[github] labels`.
    #[serde(default)]
    labels: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListModelsParams {
    harness: HarnessId,
}

/// `ListSkills` (gh#134). Every field optional: the composer on the new-chat
/// canvas has no chat yet and asks with the space's folder as `cwd`, while an
/// open chat names itself and lets the host resolve both halves.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListSkillsParams {
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    /// Which harness the run will use, when no chat pins one.
    #[serde(default)]
    harness: Option<HarnessId>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct QueueCommandParams {
    chat_id: String,
    command: SessionCommandPayload,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepoPathParams {
    /// `repoPath` per §3.5 (the §2.1 shorthand `repo` is accepted as an alias).
    #[serde(alias = "repo")]
    repo_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SwitchRefParams {
    /// The checkout to switch — a session's cwd (main folder or worktree).
    repo_path: String,
    ref_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateWorktreeParams {
    #[serde(alias = "repo")]
    repo_path: String,
    branch: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteWorktreeParams {
    #[serde(alias = "repo")]
    repo_path: String,
    #[serde(alias = "path")]
    worktree_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListFoldersParams {
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenTerminalParams {
    chat_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TerminalIdParams {
    terminal_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscribeTerminalParams {
    terminal_id: String,
    #[serde(default)]
    after_seq: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteTerminalParams {
    terminal_id: String,
    /// Base64 input bytes (plain UTF-8 accepted leniently).
    data: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResizeTerminalParams {
    terminal_id: String,
    cols: u16,
    rows: u16,
}

/// `DispatchTask`'s params. At module scope rather than inside the handler
/// because these key names are a contract three callers write by hand — the
/// desktop panel, the TUI and the `comet-board` CLI — and a camelCase slip
/// would drop an override or an attribution silently, with a successful
/// dispatch to show for it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DispatchTaskParams {
    task_id: String,
    /// The dispatching chat's id when an agent released it — provenance, never
    /// authority.
    #[serde(default)]
    via: Option<String>,
    /// The device the dispatch was issued from, and who its frontend says is
    /// signed in there (gh#74). Both are the caller's word.
    ///
    /// `via_user` is the **fallback**, and only for a call that arrived with no
    /// relay stamp — from this box's own IPC port (gh#161). A forwarded call
    /// resolves its dispatcher from the identity the edge verified and the
    /// relay stamped on the frame, and this key is then recorded and never
    /// compared, so a frontend that writes somebody else's address here changes
    /// nothing about what gets spent. Neither is ever authorization: which
    /// account a run spends is `account` below, always explicit.
    #[serde(default)]
    via_device: Option<String>,
    #[serde(default)]
    via_user: Option<String>,
    /// Runtime override (e.g. `opencode`); `None` = the route's.
    #[serde(default)]
    runtime: Option<String>,
    /// Model override for the chosen harness.
    #[serde(default)]
    model: Option<String>,
    /// Agent-account slot id to spend — whose Claude/Codex subscription pays
    /// for this run (gh#59). `None` = the route's `account`, and failing that
    /// the device's own CLI login. Explicit on purpose: the board does not
    /// infer an account from the WorkOS user who dispatched, it records `via`
    /// and does what it was told.
    #[serde(default)]
    account: Option<String>,
    /// "Bill this account, I know whose it is" — the acknowledgement
    /// `[defaults] billing_guard = "require-own"` accepts instead of refusing a
    /// cross-billed release (gh#101). An agent-account slot id, which also
    /// selects the account, or the email on the login being spent (the only
    /// spelling there is when that login is the box's own).
    ///
    /// A separate key from `account` on purpose: `account` says which
    /// subscription, this says that somebody meant to spend one that is not
    /// theirs. Ignored entirely under `warn` and `off`.
    #[serde(default)]
    bill: Option<String>,
    /// End the task's live attempt and release a fresh one — the blocked row's
    /// Retry (gh#49). Off for ordinary dispatches, which are refused on a live
    /// attempt.
    #[serde(default)]
    replace: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListAgentAccountsParams {
    #[serde(default)]
    force_usage: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentAccountParams {
    harness: HarnessId,
    account_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartAgentLoginParams {
    harness: HarnessId,
    /// Normally routing metadata, read here as meaning (gh#193): the accounts
    /// page sends it only when the switcher points at a device that is not the
    /// one the operator is looking at, so by the time it arrives *here* — the
    /// engine it names — it says the browser that will finish this login is on
    /// some other machine. Codex needs to know: its default flow waits on a
    /// callback to its own loopback, which that browser can never reach.
    #[serde(default)]
    target_device_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginIdParams {
    login_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompleteAgentLoginParams {
    login_id: String,
    code: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadChunkParams {
    upload_id: String,
    /// Base64 payload chunk.
    data: String,
    #[serde(default)]
    seq: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadCommitParams {
    upload_id: String,
    file_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadAttachmentChunkParams {
    path: String,
    #[serde(default)]
    offset: u64,
}

/// The Mutate surface (feature-inventory §2 DataRpc), tagged by `op`.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
enum MutateParams {
    #[serde(rename_all = "camelCase")]
    CreateChat {
        chat_id: String,
        /// The space the chat is created in — fixes host device + base cwd.
        space_id: String,
        #[serde(default)]
        config: Option<ChatConfig>,
        /// The picked ref, named on the row from the first frame (the footer
        /// read "Select ref" until the diff reconciler stamped it).
        #[serde(default)]
        branch: Option<String>,
        /// Cwd override (isolated-worktree path); default = the space's folder.
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Create a space (device + folder pair). Idempotent by id; a live
    /// duplicate `(deviceId, path)` no-ops. `gitDetected` is seeded from the
    /// picker's FolderEntry — the owning device's SpacesSync re-verifies.
    #[serde(rename_all = "camelCase")]
    CreateSpace {
        space_id: String,
        device_id: String,
        path: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        git_detected: bool,
    },
    /// LWW display-name set; `name: None` clears back to basename(path).
    #[serde(rename_all = "camelCase")]
    RenameSpace {
        space_id: String,
        #[serde(default)]
        name: Option<String>,
    },
    /// Hard delete: cascades to every chat (and session row) in the space.
    /// Live runs hosted here are interrupted best-effort.
    #[serde(rename_all = "camelCase")]
    DeleteSpace { space_id: String },
    #[serde(rename_all = "camelCase")]
    RenameChat { chat_id: String, title: String },
    /// Set the chat's checkout branch label — the sidebar's
    /// "project · branch" sub-line.
    #[serde(rename_all = "camelCase")]
    SetChatBranch { chat_id: String, branch: String },
    /// Retarget a chat onto another folder — mid-session switch to an
    /// EXISTING worktree (the picked ref's checkout). Next run starts a
    /// fresh harness conversation there (resume is cwd-scoped).
    #[serde(rename_all = "camelCase")]
    SetChatCwd { chat_id: String, cwd: String },
    /// Backdate a chat's activity timestamps (epoch ms) — the sidebar's
    /// relative-time column. Used by tooling/seeds; the doc fold sets these on
    /// real message traffic.
    #[serde(rename_all = "camelCase")]
    SetChatActivity {
        chat_id: String,
        #[serde(default)]
        last_message_at: Option<i64>,
        #[serde(default)]
        created_at: Option<i64>,
    },
    /// Re-home a chat to another device (tooling/seeds; device migration later).
    #[serde(rename_all = "camelCase")]
    SetChatHost { chat_id: String, device_id: String },
    #[serde(rename_all = "camelCase")]
    SetChatArchived { chat_id: String, archived: bool },
    /// Full-config replace on the chat row (comet `SetChatConfig`): the
    /// composer's mid-session model / reasoning / options changes, LWW-synced
    /// so they survive restarts and reach every device.
    #[serde(rename_all = "camelCase")]
    SetChatConfig { chat_id: String, config: ChatConfig },
    /// Tombstone: removes the chats-map row; the session doc remains.
    #[serde(rename_all = "camelCase")]
    DeleteChat { chat_id: String },
    #[serde(rename_all = "camelCase")]
    RenameDevice { device_id: String, name: String },
    /// Synced seen marker (LWW + monotonic guard): clears the "completed"
    /// badge on every device. `at` is epoch ms; default = now.
    #[serde(rename_all = "camelCase")]
    MarkChatSeen {
        chat_id: String,
        #[serde(default)]
        at: Option<i64>,
    },
}

pub struct EngineRpc {
    sessions: SessionsEngine,
    doc_host: DocHost,
    workspace: WorkspaceHost,
    registry: std::sync::Arc<HarnessRegistry>,
    repos: Repos,
    terminals: Terminals,
    diff_sync: CheckoutDiffSync,
    uploads: Uploads,
    agent_accounts: AgentAccounts,
    auth: Option<Auth>,
    links: Option<std::sync::Arc<LinkCache>>,
    updater: Option<comet_update::Updater>,
    board: Option<std::sync::Arc<crate::board::BoardService>>,
    /// Live edge-connection census (gh#116). A closure rather than the pieces,
    /// because the host-relay socket is owned by the core, not by anything this
    /// struct holds — and the answer must be recomputed per call, never cached.
    edge_health: Option<EdgeHealthProbe>,
}

/// Reads [`crate::EngineCore::edge_health`] on demand.
pub type EdgeHealthProbe = std::sync::Arc<dyn Fn() -> comet_proto::EdgeHealth + Send + Sync>;

impl EngineRpc {
    #[allow(clippy::too_many_arguments)] // engine assembly seam, not a public API
    pub fn new(
        sessions: SessionsEngine,
        doc_host: DocHost,
        workspace: WorkspaceHost,
        registry: std::sync::Arc<HarnessRegistry>,
        repos: Repos,
        terminals: Terminals,
        diff_sync: CheckoutDiffSync,
        uploads: Uploads,
        agent_accounts: AgentAccounts,
    ) -> Self {
        Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            diff_sync,
            uploads,
            agent_accounts,
            auth: None,
            links: None,
            updater: None,
            board: None,
            edge_health: None,
        }
    }

    /// Attach the auth service (AuthStatus + AuthRpc mutations).
    pub fn with_auth(mut self, auth: Auth) -> Self {
        self.auth = Some(auth);
        self
    }

    /// Attach the peer link cache — enables `targetDeviceId` relay forwarding.
    pub fn with_links(mut self, links: std::sync::Arc<LinkCache>) -> Self {
        self.links = Some(links);
        self
    }

    /// Attach the release checker (UpdateStatus stream + ApplyUpdate).
    pub fn with_updater(mut self, updater: comet_update::Updater) -> Self {
        self.updater = Some(updater);
        self
    }

    /// Attach the board service (WatchBoard / DispatchTask / CancelTask).
    pub fn with_board(mut self, board: std::sync::Arc<crate::board::BoardService>) -> Self {
        self.board = Some(board);
        self
    }

    /// Attach the edge-connection census (the `EdgeHealth` method, gh#116).
    pub fn with_edge_health(mut self, probe: EdgeHealthProbe) -> Self {
        self.edge_health = Some(probe);
        self
    }

    /// The skills a run in this chat could invoke (gh#134).
    ///
    /// The config dir is resolved the way the harness child's env will be, not
    /// the way this device's shell is: a chat naming an agent account reads
    /// that account's dir, because a slot overrides `CLAUDE_CONFIG_DIR` and
    /// listing the box user's skills for it would be listing skills the run
    /// cannot invoke. A chat that names none reads the CLI's own dir, which is
    /// every chat on a single-account box.
    ///
    /// Never fails: an unknown chat, a harness with no skills, an unreadable
    /// dir all produce an empty list. The picker's honest answer to "nothing is
    /// installed" and to "I could not look" is the same menu, and an error
    /// would only put a red box under a composer mid-keystroke.
    fn list_skills(&self, params: ListSkillsParams) -> Vec<comet_proto::SkillDescriptor> {
        let chat = params
            .chat_id
            .as_deref()
            .and_then(|id| self.workspace.chat(id));
        let config = chat.as_ref().and_then(|c| c.config.clone());
        let harness = config
            .as_ref()
            .map(|c| c.harness)
            .or(params.harness)
            .unwrap_or(HarnessId::ClaudeCode);
        if !crate::skills::harness_has_skills(harness) {
            return Vec::new();
        }
        let config_dir = match config.as_ref().and_then(|c| c.account.as_deref()) {
            Some(account) => self.agent_accounts.config_dir_for(account),
            None => self.agent_accounts.default_config_dir(harness),
        };
        // The chat's own cwd is the worktree the agent works in, which is where
        // its repo-level skills are. `params.cwd` covers the new-chat canvas,
        // where the space's folder is all there is.
        let cwd = chat
            .and_then(|c| c.cwd)
            .or(params.cwd)
            .map(std::path::PathBuf::from);
        crate::skills::list(&crate::skills::SkillRoots { config_dir, cwd })
    }

    fn auth(&self) -> Result<&Auth, RpcError> {
        self.auth
            .as_ref()
            .ok_or_else(|| RpcError::Failed("auth unavailable".into()))
    }

    /// Who released a dispatch, from the transport's own answer rather than the
    /// caller's (gh#161).
    ///
    /// The relay stamped [`Caller::user`] on the frame from an identity the
    /// edge Worker verified, so a forwarded call resolves its dispatcher here;
    /// `via_user` is the fallback for the local case *only*, where there is no
    /// relay to have stamped anything and the process on the other end is the
    /// box's own. Both land on the attempt; only one of them is ever compared
    /// against an account (see [`comet_board::billing`]).
    async fn dispatch_origin(
        &self,
        caller: &Caller,
        p_via: Option<String>,
        p_device: Option<String>,
        p_user: Option<String>,
    ) -> DispatchOrigin {
        let verified = match caller.user() {
            None => None,
            Some(user_id) => {
                let auth = self.auth.as_ref();
                Some(VerifiedCaller {
                    user_id: user_id.to_string(),
                    // A box with no auth service at all (bare-core test
                    // assemblies) can name nobody, including itself — which is
                    // the honest answer and the conservative one.
                    email: match auth {
                        Some(auth) => auth.email_for_user(user_id).await,
                        None => None,
                    },
                    is_owner: auth.and_then(|a| a.user_id()).as_deref() == Some(user_id),
                })
            }
        };
        DispatchOrigin {
            chat: p_via,
            device: p_device,
            user: p_user,
            verified,
        }
    }

    fn updater(&self) -> Result<&comet_update::Updater, RpcError> {
        self.updater
            .as_ref()
            .ok_or_else(|| RpcError::Failed("updates unavailable".into()))
    }

    /// This device's board, or the refusal that means "I host none".
    ///
    /// [`RpcError::Refused`] rather than `Failed` on purpose (gh#155): every
    /// board-addressed method funnels through here, and a sweeping caller has
    /// to be able to tell this device's honest "not me" from a call that never
    /// arrived. Same message, tagged so the answer survives the relay.
    fn board(&self) -> Result<&crate::board::BoardService, RpcError> {
        self.board.as_deref().ok_or_else(|| {
            RpcError::Refused(
                "board unavailable (COMET_BOARD=0, or the service failed to start — \
                 see the engine log)"
                    .into(),
            )
        })
    }

    /// Forward a device-addressed call over the target device's relay. On transport
    /// failure the cached link is invalidated so the next call re-dials.
    async fn forward(
        &self,
        target: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<RpcReply, RpcError> {
        let Some(links) = &self.links else {
            return Err(RpcError::Failed(format!(
                "cannot reach device {target}: remote routing unavailable (offline)"
            )));
        };
        let client = links.client(target).await?;
        if is_stream_method(method) {
            let rx = match client.subscribe(method, params).await {
                Ok(rx) => rx,
                Err(err) => {
                    links.invalidate(target);
                    return Err(err);
                }
            };
            // Pipe remote items; the held client keeps the link's RpcClient alive for
            // the stream's lifetime. A remote error just ends the stream (the relay
            // link-down path fails pending calls; stream receivers close).
            let stream = futures::stream::unfold((rx, client), |(mut rx, client)| async move {
                rx.recv().await.map(|item| (item, (rx, client)))
            });
            return Ok(RpcReply::Stream(stream.boxed()));
        }
        match client.call(method, params).await {
            Ok(value) => Ok(RpcReply::Value(value)),
            Err(err) => {
                if matches!(err, RpcError::Closed | RpcError::Transport(_)) {
                    links.invalidate(target);
                }
                Err(err)
            }
        }
    }

    // ---- the routing surface (gh#75) ----

    /// The reply both config methods answer with: the file, and the repos on
    /// this device that have a space but nothing on the board watching them.
    ///
    /// The unadopted list rides along because it is the same question asked
    /// from the other side — "what is not routed yet" is the reason somebody
    /// opened the config — and because detecting it needs the space list and a
    /// git probe, neither of which a remote caller has for *this* device.
    async fn config_reply(
        &self,
        routing: comet_board::routes::RoutingView,
    ) -> Result<serde_json::Value, RpcError> {
        let unadopted = match &routing.config {
            // Nothing to compare against: a file that does not parse cannot say
            // which repos it already covers, and offering to "adopt" every one
            // of them would be wrong in both directions.
            None => Vec::new(),
            Some(cfg) => {
                let device = self.doc_host.device_id().to_string();
                let spaces: Vec<comet_proto::Space> = self
                    .workspace
                    .watch_spaces()
                    .borrow()
                    .iter()
                    .filter(|s| s.device_id == device)
                    .cloned()
                    .collect();
                let cfg = cfg.clone();
                // `probe` shells out to git once per space. Off the reactor.
                tokio::task::spawn_blocking(move || {
                    comet_board::adopt::detect(&spaces, &cfg, comet_board::adopt::probe)
                })
                .await
                .map_err(|e| RpcError::Failed(format!("detecting unadopted repos: {e}")))?
            }
        };
        Ok(serde_json::json!({ "routing": routing, "unadopted": unadopted }))
    }

    /// Perform one config write. Everything lands through
    /// [`comet_board::routes::edit`] or [`comet_board::adopt`], which is where
    /// the parse + validate + `.bak` discipline lives.
    async fn write_board_config(
        &self,
        paths: &comet_board::config::Paths,
        write: BoardConfigWrite,
    ) -> anyhow::Result<comet_board::routes::RoutingView> {
        use comet_board::adopt;
        match write {
            BoardConfigWrite::Edit(edit) => comet_board::routes::edit(paths, &edit),
            // Resolve first, write second: a login GitHub has never heard of
            // must cost an error naming it, not a half-written map.
            BoardConfigWrite::Member { user, github, name } => {
                let author = match comet_board::members::parse_github(&github)? {
                    comet_board::members::GithubRef::Author(a) => a,
                    comet_board::members::GithubRef::Login(login) => {
                        let paths = paths.clone();
                        // Blocking: the GitHub clients are `reqwest::blocking`
                        // and hold `Rc`s, exactly as `onboard_repo`'s phase one
                        // does.
                        tokio::task::spawn_blocking(move || {
                            let rest = comet_board::sources::github::HttpRest::from_paths(&paths)?;
                            comet_board::members::resolve_login(
                                &comet_board::sources::github::Github::new(rest),
                                &login,
                            )
                        })
                        .await
                        .map_err(|e| anyhow::anyhow!("resolving `{github}`: {e}"))??
                    }
                };
                let author = comet_board::members::named(author, name.as_deref());
                comet_board::routes::edit(
                    paths,
                    &comet_board::routes::Edit::User {
                        user,
                        value: Some(comet_board::members::users_value(&author)),
                    },
                )
            }
            BoardConfigWrite::Ignore { slug } => {
                adopt::ignore(&paths.routing(), &slug)?;
                comet_board::routes::read(paths)
            }
            BoardConfigWrite::Adopt { slug, labels } => {
                // The offer has to be current: adopting a repo the board is
                // already watching would write a second route for it, and
                // `adopt_with` computes its insertion point from a config it
                // parses itself.
                let routing = comet_board::routes::read(paths)?;
                let cfg = routing.config.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "routing.toml does not parse, so nothing can be adopted into it — \
                         fix it first ({})",
                        routing.problems.join("; ")
                    )
                })?;
                let device = self.doc_host.device_id().to_string();
                let spaces: Vec<comet_proto::Space> = self
                    .workspace
                    .watch_spaces()
                    .borrow()
                    .iter()
                    .filter(|s| s.device_id == device)
                    .cloned()
                    .collect();
                let found = adopt::detect(&spaces, &cfg, adopt::probe);
                let Some(u) = found.iter().find(|u| u.slug.eq_ignore_ascii_case(&slug)) else {
                    anyhow::bail!(
                        "`{slug}` is not on this board's unadopted list — already-adopted \
                         and ignored repos are not offered, and a repo needs a space on \
                         this device to be adopted at all"
                    );
                };
                adopt::adopt_with(&paths.routing(), u, labels.as_deref())?;
                comet_board::routes::read(paths)
            }
        }
    }

    // ---- onboarding a repo the board has never seen (gh#97) ----

    /// Clone, space, adopt — everything on the device that hosts the board.
    ///
    /// The order is the one that fails cheapest first. Resolution is a single
    /// round trip and it is the step most likely to say no (a repo the App is
    /// not installed on), so it happens before anything touches the disk; the
    /// clone is what leaves a directory behind, so it happens before the space
    /// that would otherwise point at nothing.
    ///
    /// Two blocking phases rather than one: the GitHub clients hold `Rc`s and
    /// cannot cross an await, and the clone in the middle is async because
    /// `repos.rs` is. Each phase builds its own client — that costs one extra
    /// installation-token mint per onboard, which is the right price for not
    /// holding a `!Send` value across the clone.
    async fn onboard_repo(
        &self,
        p: OnboardParams,
    ) -> anyhow::Result<comet_board::onboard::Onboarded> {
        use comet_board::onboard::{AtPath, Step, at_path, checkout_path, unreachable};

        let slug_typed = comet_board::onboard::parse_slug(&p.slug)?;
        let paths = self.board()?.paths().clone();

        // ── phase 1: what GitHub says, under the board's own credential ──
        //
        // On the board's device on purpose: the laptop that ran the command
        // usually has no GitHub credential at all, and the credential that
        // decides whether this repo is reachable is the one that will poll it.
        let want_preview = p.labels.is_none();
        let ask = {
            let slug = slug_typed.clone();
            let paths = paths.clone();
            tokio::task::spawn_blocking(move || {
                let rest = comet_board::sources::github::HttpRest::from_paths(&paths)?;
                let gh = comet_board::sources::github::Github::new(rest);
                let facts = match gh.repo(&slug) {
                    Ok(facts) => facts,
                    Err(e) => return Err(unreachable(&slug, gh.rest.auth(), &e)),
                };
                // Best-effort, exactly as `adopt`'s is: no network degrades to
                // onboarding without the numbers rather than to not onboarding.
                let preview = want_preview
                    .then(|| comet_board::adopt::preview(&gh, &facts.slug).ok())
                    .flatten();
                Ok((facts, preview))
            })
            .await
        };
        let (facts, preview) =
            ask.map_err(|e| anyhow::anyhow!("resolving {slug_typed}: {e}"))??;
        // GitHub's casing from here on: `routing.toml` should spell the repo the
        // way the repo is spelled.
        let slug = facts.slug.clone();

        // ── the clone ──
        let target = checkout_path(p.dir.as_deref(), &slug, &self.repos.clone_root())?;
        let exists = target.exists();
        let origin = if exists {
            self.repos.origin_url(&target).await
        } else {
            None
        };
        let clone = match at_path(&target, exists, origin.as_deref(), &slug) {
            AtPath::Occupied(why) => anyhow::bail!("{why}"),
            AtPath::SameRepo => Step::Reused,
            AtPath::Vacant => {
                self.repos
                    .clone_to(
                        &comet_board::git_credentials::push_url(&slug),
                        &facts.clone_url,
                        &target,
                        &clone_env(&slug, &paths),
                    )
                    .await?;
                Step::Created
            }
        };
        let path = target.to_string_lossy().to_string();

        // ── the space ──
        //
        // Reused by (device, path) rather than re-created: the workspace host
        // dedupes that pair too, but silently, which would leave the minted id
        // in the reply naming a space that does not exist.
        let device_id = self.doc_host.device_id().to_string();
        let existing = self
            .workspace
            .read_spaces()?
            .into_iter()
            .find(|s| s.device_id == device_id && s.path == path);
        let (space, space_id, space_name) = match existing {
            Some(s) => (Step::Reused, s.id.clone(), s.display_name().to_string()),
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                // `git_detected` is not a guess here: we just cloned it. The
                // owning device's SpacesSync re-checks it either way.
                self.workspace
                    .create_space(&id, &device_id, &path, None, true)?;
                let name = comet_proto::Space {
                    id: id.clone(),
                    device_id: device_id.clone(),
                    path: path.clone(),
                    name: None,
                    git_detected: true,
                    git_checked_at: None,
                    checkout_id: None,
                    created_at: chrono::Utc::now(),
                }
                .display_name()
                .to_string();
                (Step::Created, id, name)
            }
        };

        // ── the routing halves ──
        //
        // Straight to `adopt_with` rather than through `detect`: detection reads
        // the spaces *watch*, which has not necessarily observed the row created
        // three lines ago, and an onboard that raced its own space would report
        // "not on the unadopted list" about a repo it had just cloned.
        let routing = comet_board::routes::read(&paths)?;
        let cfg = routing.config.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "routing.toml does not parse, so nothing can be adopted into it — the \
                 clone and the space are in place; fix the file and re-run ({})",
                routing.problems.join("; ")
            )
        })?;
        let adopted = match comet_board::onboard::routing_gap(&cfg, &slug) {
            None => None,
            Some(missing) => Some(comet_board::adopt::adopt_with(
                &paths.routing(),
                &comet_board::adopt::Unadopted {
                    label: space_name.clone(),
                    slug: slug.clone(),
                    repo_root: path.clone(),
                    missing,
                },
                p.labels.as_deref(),
            )?),
        };
        // Read *after* the write: whether the board will comment on this repo is
        // a question about the file as it now stands.
        let writeback = comet_board::routes::read(&paths)?
            .config
            .map(|c| c.github.writeback_for(&slug))
            .unwrap_or(false);

        Ok(comet_board::onboard::Onboarded {
            slug,
            device_id,
            path,
            clone,
            space,
            space_id,
            space_name,
            adopted,
            preview,
            writeback,
            issues_disabled: !facts.has_issues,
            archived: facts.archived,
        })
    }

    /// The repos the App can see, crossed with what the board already watches.
    async fn list_app_repos(&self) -> anyhow::Result<Vec<comet_board::onboard::Candidate>> {
        let paths = self.board()?.paths().clone();
        tokio::task::spawn_blocking(move || {
            let rest = comet_board::sources::github::HttpRest::from_paths(&paths)?;
            let app = rest.auth().app().ok_or_else(|| {
                // Not a failure of the board — a different credential shape. A
                // PAT has no installations to enumerate, and guessing at
                // `/user/repos` would offer repos the board cannot poll.
                anyhow::anyhow!(
                    "this board authenticates with GITHUB_TOKEN, which has no App \
                     installations to list — name the repo directly instead"
                )
            })?;
            let repos = app.accessible_repos()?;
            let cfg = comet_board::routes::read(&paths)?
                .config
                .unwrap_or_default();
            Ok(comet_board::onboard::candidates(&repos, &cfg))
        })
        .await
        .map_err(|e| anyhow::anyhow!("listing the App's repos: {e}"))?
    }

    /// Everything a repo-first space picker needs from this host (gh#118).
    ///
    /// The board check comes first and costs nothing, which is what makes the
    /// picker's sweep affordable: a device hosting no board refuses here before
    /// any git or GitHub work, exactly as it refuses `WatchBoard`, so asking
    /// every device "are you the box?" is one cheap round trip each.
    ///
    /// The App's grant is best-effort. A board on a `GITHUB_TOKEN` has no
    /// installations to enumerate (gh#97) — that is a supported board, not a
    /// broken one, and its spaces are still the spaces a picker should offer. So
    /// the failure becomes a note beside a list that is otherwise complete.
    async fn list_repo_spaces(&self) -> anyhow::Result<serde_json::Value> {
        let _board = self.board()?;
        let device = self.doc_host.device_id().to_string();
        let spaces: Vec<comet_proto::Space> = self
            .workspace
            .watch_spaces()
            .borrow()
            .iter()
            .filter(|s| s.device_id == device)
            .cloned()
            .collect();
        // `probe` shells out to git once per space. Off the reactor.
        let links = tokio::task::spawn_blocking(move || {
            comet_board::onboard::space_links(&spaces, comet_board::adopt::probe)
        })
        .await
        .map_err(|e| anyhow::anyhow!("resolving spaces to repos: {e}"))?;
        let (repos, note) = match self.list_app_repos().await {
            Ok(repos) => (repos, None),
            Err(err) => (Vec::new(), Some(format!("{err:#}"))),
        };
        Ok(serde_json::json!({
            "deviceId": device,
            "spaces": links,
            "repos": repos,
            "reposNote": note,
        }))
    }

    fn mutate(&self, params: MutateParams) -> Result<(), RpcError> {
        let failed = |e: crate::EngineError| RpcError::Failed(e.to_string());
        match params {
            MutateParams::CreateChat {
                chat_id,
                space_id,
                config,
                branch,
                cwd,
            } => {
                self.workspace
                    .create_chat(&chat_id, &space_id, config, cwd)
                    .map_err(failed)?;
                if let Some(branch) = branch.as_deref().filter(|b| !b.is_empty()) {
                    self.workspace
                        .set_chat_branch(&chat_id, branch)
                        .map_err(failed)?;
                }
                Ok(())
            }
            MutateParams::CreateSpace {
                space_id,
                device_id,
                path,
                name,
                git_detected,
            } => self
                .workspace
                .create_space(&space_id, &device_id, &path, name, git_detected)
                .map_err(failed),
            MutateParams::RenameSpace { space_id, name } => self
                .workspace
                .rename_space(&space_id, name.as_deref())
                .map_err(failed)
                .map(drop),
            MutateParams::DeleteSpace { space_id } => {
                let deleted = self.workspace.delete_space(&space_id).map_err(failed)?;
                // Best-effort teardown of live runs we host for the deleted chats
                // (the doc rows are already tombstoned; a straggler run would only
                // write into an orphaned session doc).
                let sessions = self.sessions.clone();
                let chat_ids = deleted.chat_ids;
                tokio::spawn(async move {
                    for chat_id in chat_ids {
                        if let Err(err) = sessions.interrupt(&chat_id).await {
                            tracing::debug!(chat = %chat_id, error = %err, "deleteSpace interrupt skipped");
                        }
                    }
                });
                Ok(())
            }
            MutateParams::RenameChat { chat_id, title } => self
                .workspace
                .rename_chat(&chat_id, &title)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatBranch { chat_id, branch } => self
                .workspace
                .set_chat_branch(&chat_id, &branch)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatCwd { chat_id, cwd } => self
                .workspace
                .set_chat_cwd(&chat_id, &cwd)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatActivity {
                chat_id,
                last_message_at,
                created_at,
            } => self
                .workspace
                .set_chat_activity(&chat_id, last_message_at, created_at)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatHost { chat_id, device_id } => self
                .workspace
                .set_chat_host(&chat_id, &device_id)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatArchived { chat_id, archived } => self
                .workspace
                .set_chat_archived(&chat_id, archived)
                .map_err(failed)
                .map(drop),
            MutateParams::SetChatConfig { chat_id, config } => self
                .workspace
                .set_chat_config(&chat_id, &config)
                .map_err(failed)
                .map(drop),
            MutateParams::DeleteChat { chat_id } => self
                .workspace
                .delete_chat(&chat_id)
                .map_err(failed)
                .map(drop),
            MutateParams::RenameDevice { device_id, name } => self
                .workspace
                .rename_device(&device_id, &name)
                .map_err(failed)
                .map(drop),
            MutateParams::MarkChatSeen { chat_id, at } => {
                let at = at
                    .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                    .unwrap_or_else(chrono::Utc::now);
                self.workspace
                    .mark_chat_seen(&chat_id, at)
                    .map_err(failed)
                    .map(drop)
            }
        }
    }
}

/// The environment an onboarding clone authenticates with (gh#97).
///
/// The same pairs a dispatched agent's `git push` gets, for the same reason: the
/// board's App is the credential that will push this repo's branches, so it is
/// the one that should fetch it. Nothing secret is in them — they name the
/// askpass helper, which mints onto git's own pipe.
///
/// Without a resolvable helper binary there is nothing to authenticate *with*,
/// and a public repo still clones anonymously — so the credential half is
/// dropped and the "never block on a terminal" half is kept. A private repo then
/// fails on git's own 403, naming the URL, which is a truer error than one this
/// could invent.
fn clone_env(slug: &str, paths: &comet_board::config::Paths) -> Vec<(String, String)> {
    match comet_board::git_credentials::resolve_board_exe() {
        Some(exe) => comet_board::git_credentials::agent_env(&exe, slug, paths),
        None => {
            tracing::warn!(
                "onboard: no comet-board binary found for the askpass helper — cloning \
                 {slug} without the board's credential"
            );
            vec![("GIT_TERMINAL_PROMPT".into(), "0".into())]
        }
    }
}

/// ControlRpc methods that honor `targetDeviceId` (feature-inventory §2.1). Extend this
/// list (plus [`is_stream_method`] for streams) to make more of the surface
/// device-addressable — the handlers themselves need no changes.
fn forwardable(method: &str) -> bool {
    matches!(
        method,
        methods::LIST_HARNESSES
            | methods::LIST_MODELS
            // Skills are files on the chat's host, and which files depends on
            // the agent account that chat names (gh#134) — a laptop answering
            // from its own `~/.claude` would offer what the run cannot invoke.
            | methods::LIST_SKILLS
            | methods::QUEUE_COMMAND
            | methods::WATCH_DOC_MESSAGES
            // Repos/worktrees/folders are device-local filesystem state.
            | methods::LIST_REPOS
            | methods::ADD_REPO
            | methods::CLONE_REPO
            | methods::CREATE_REPO
            | methods::LIST_BRANCHES
            | methods::LIST_REFS
            | methods::SWITCH_REF
            | methods::LIST_FOLDERS
            | methods::CREATE_WORKTREE
            | methods::DELETE_WORKTREE
            // Checkout diffs are produced on the device holding the checkout.
            | methods::WATCH_CHECKOUT_DIFFS
            // Terminals live on the chat's host device.
            | methods::OPEN_TERMINAL
            | methods::SUBSCRIBE_TERMINAL
            | methods::WRITE_TERMINAL
            | methods::RESIZE_TERMINAL
            | methods::CLOSE_TERMINAL
            // Agent accounts are per-device CLI logins (the device switcher
            // retargets which device's logins are shown). `StartAgentLogin`
            // additionally *reads* the passthrough: a forwarded login is one
            // whose browser is on another machine (gh#193).
            | methods::LIST_AGENT_ACCOUNTS
            | methods::ACTIVATE_AGENT_ACCOUNT
            | methods::FORGET_AGENT_ACCOUNT
            | methods::START_AGENT_LOGIN
            | methods::COMPLETE_AGENT_LOGIN
            | methods::POLL_AGENT_LOGIN
            | methods::CANCEL_AGENT_LOGIN
            // Uploads/attachments target the chat's host device (the agent reads
            // the committed file from that device's disk).
            | methods::UPLOAD_CHUNK
            | methods::UPLOAD_COMMIT
            | methods::READ_ATTACHMENT_CHUNK
            // Updates report/apply on the device whose binary they concern.
            | methods::UPDATE_STATUS
            | methods::APPLY_UPDATE
            // The board lives on whichever device hosts its store — usually the
            // always-on box (gh#55). Forwarding is what turns "one box" into
            // "one box, many users": teammates read and drive the same board
            // from their own laptops instead of SSHing in. Authorization needs
            // nothing per-method — the relay carries frames only between
            // devices of one org (the DeviceRoom's org gate, gh#66; it really
            // was per-USER until then, which is what kept a second teammate
            // out), exactly as for every other forwardable call.
            | methods::WATCH_BOARD
            | methods::WATCH_BOARD_ORCHESTRATOR
            | methods::DISPATCH_TASK
            | methods::CANCEL_TASK
            | methods::READ_BOARD_TASK
            // The review contract (§gh#183). Forwarded for the same reason
            // every other board verb is: the attempt row, the checkout the
            // diff comes out of, and the run journal all live on the box, and
            // an agent submitting claims is usually not sitting on it.
            | methods::SUBMIT_CLAIMS
            | methods::READ_ATTEMPT_REVIEW
            | methods::LIST_BOARD_RUNTIMES
            // Throughput is read off `board.db`, which only the box has
            // (gh#143) — a laptop's stats page is asking about the board, not
            // about itself.
            | methods::BOARD_STATS
            // `routing.toml` is a file on the board's device, which is exactly
            // why it is forwarded (gh#75): the alternative for a teammate who
            // needs a repo routed is an ssh account on the box.
            | methods::READ_BOARD_CONFIG
            | methods::WRITE_BOARD_CONFIG
            // Onboarding is three device-local effects behind one verb — a
            // clone on the box's disk, a space owned by the box, and the box's
            // routing.toml — plus a GitHub round trip that has to happen with
            // the box's credential, because the laptop asking usually has none
            // (gh#97).
            | methods::ONBOARD_REPO
            | methods::LIST_APP_REPOS
            // Which of the host's spaces are checkouts of which repo is a
            // question about the host's disk, asked by a picker that is
            // usually running on a phone (gh#118).
            | methods::LIST_REPO_SPACES
    )
}

/// Forwardable methods whose reply is a stream (proxied item-by-item).
fn is_stream_method(method: &str) -> bool {
    matches!(
        method,
        methods::WATCH_DOC_MESSAGES
            | methods::SUBSCRIBE_TERMINAL
            | methods::WATCH_CHECKOUT_DIFFS
            | methods::UPDATE_STATUS
            | methods::WATCH_BOARD
            | methods::WATCH_BOARD_ORCHESTRATOR
    )
}

/// A watch receiver as a stream: current value first, then every change.
fn watch_stream<T>(rx: watch::Receiver<T>) -> BoxStream<'static, serde_json::Value>
where
    T: serde::Serialize + Clone + Send + Sync + 'static,
{
    futures::stream::unfold((rx, false), |(mut rx, emitted)| async move {
        if emitted {
            rx.changed().await.ok()?;
        }
        let value = {
            let borrowed = rx.borrow_and_update();
            serde_json::to_value(&*borrowed).ok()?
        };
        Some((value, (rx, true)))
    })
    .boxed()
}

/// Authentication-only RPC surface used while the headed app is waiting for a
/// production WorkOS session. Keeping this independent from [`EngineRpc`] lets
/// the UI show its sign-in and organization gates before identity-scoped Loro
/// stores are opened.
#[derive(Clone)]
pub struct AuthRpc {
    auth: Auth,
}

impl AuthRpc {
    pub fn new(auth: Auth) -> Self {
        Self { auth }
    }

    pub fn handles(method: &str) -> bool {
        matches!(
            method,
            methods::AUTH_STATUS
                | methods::SIGN_IN
                | methods::SIGN_IN_HEADLESS
                | methods::COMPLETE_SIGN_IN
                | methods::SIGN_OUT
                | methods::LIST_ORGS
                | methods::CREATE_ORG
                | methods::SELECT_ORG
                | methods::LIST_MEMBERS
                | methods::LIST_INVITES
                | methods::INVITE_MEMBER
                | methods::REVOKE_INVITE
                | methods::ACCEPT_INVITE
        )
    }
}

#[async_trait]
impl RpcService for AuthRpc {
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        match method {
            methods::AUTH_STATUS => Ok(RpcReply::Stream(watch_stream(self.auth.watch_state()))),
            methods::SIGN_IN => {
                let url = self
                    .auth
                    .start_sign_in()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "url": url }))
            }
            methods::SIGN_IN_HEADLESS => {
                let url = self.auth.start_headless_sign_in();
                RpcReply::value(&serde_json::json!({ "url": url }))
            }
            methods::COMPLETE_SIGN_IN => {
                #[derive(Deserialize)]
                struct P {
                    code: String,
                }
                let p: P = parse_params(params)?;
                self.auth
                    .complete_sign_in(&p.code)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::SIGN_OUT => {
                self.auth.sign_out();
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::LIST_ORGS => {
                let orgs = self
                    .auth
                    .list_orgs()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "orgs": orgs }))
            }
            methods::CREATE_ORG => {
                #[derive(Deserialize)]
                struct P {
                    name: String,
                }
                let p: P = parse_params(params)?;
                self.auth
                    .create_org(&p.name)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::SELECT_ORG => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct P {
                    organization_id: String,
                }
                let p: P = parse_params(params)?;
                self.auth
                    .select_org(&p.organization_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::LIST_MEMBERS => {
                let view = self
                    .auth
                    .list_members()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&view)
            }
            methods::LIST_INVITES => {
                let invites = self
                    .auth
                    .list_invites()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "invites": invites }))
            }
            methods::INVITE_MEMBER => {
                #[derive(Deserialize)]
                struct P {
                    email: String,
                    #[serde(default)]
                    role: Option<String>,
                }
                let p: P = parse_params(params)?;
                let invitation = self
                    .auth
                    .invite_member(&p.email, p.role.as_deref())
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "invitation": invitation }))
            }
            methods::REVOKE_INVITE => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct P {
                    invitation_id: String,
                }
                let p: P = parse_params(params)?;
                self.auth
                    .revoke_invite(&p.invitation_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::ACCEPT_INVITE => {
                #[derive(Deserialize)]
                struct P {
                    token: String,
                }
                let p: P = parse_params(params)?;
                let organization_id = self
                    .auth
                    .accept_invite(&p.token)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "organizationId": organization_id }))
            }
            _ => Err(RpcError::UnknownMethod(method.to_string())),
        }
    }
}

#[async_trait]
impl RpcService for EngineRpc {
    /// A call with no transport identity on it: this engine's own IPC port or
    /// its in-process transport, which is what [`Caller::LOCAL`] means.
    async fn handle(&self, method: &str, params: serde_json::Value) -> Result<RpcReply, RpcError> {
        self.handle_as(method, params, &Caller::LOCAL).await
    }

    async fn handle_as(
        &self,
        method: &str,
        params: serde_json::Value,
        caller: &Caller,
    ) -> Result<RpcReply, RpcError> {
        // Device-addressed routing: forward calls that target another device over its
        // relay. The target compares the id to its own, so forwards cannot loop.
        //
        // The caller is deliberately NOT passed on: the next hop's relay stamps
        // the identity *this* device dials with, which is the truth about who
        // is asking it. An identity forwarded as a parameter would be a claim
        // again, one hop later.
        if forwardable(method)
            && let Some(target) = params.get("targetDeviceId").and_then(|v| v.as_str())
            && target != self.doc_host.device_id()
        {
            let target = target.to_string();
            return self.forward(&target, method, params).await;
        }
        if AuthRpc::handles(method) {
            return AuthRpc::new(self.auth()?.clone())
                .handle(method, params)
                .await;
        }
        match method {
            methods::LIST_HARNESSES => RpcReply::value(&self.registry.descriptors()),
            methods::LIST_MODELS => {
                let p: ListModelsParams = parse_params(params)?;
                let harness = self
                    .registry
                    .resolve(p.harness)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let models = harness
                    .models()
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&models)
            }
            methods::LIST_SKILLS => RpcReply::value(&self.list_skills(parse_params(params)?)),
            methods::QUEUE_COMMAND => {
                let p: QueueCommandParams = parse_params(params)?;
                let command_id = self
                    .doc_host
                    .queue_command(&p.chat_id, p.command)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "commandId": command_id }))
            }
            methods::WATCH_DOC_MESSAGES => {
                let p: ChatParams = parse_params(params)?;
                let handle = self
                    .doc_host
                    .open(&p.chat_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                Ok(RpcReply::Stream(watch_stream(handle.watch_messages())))
            }
            methods::WATCH_CHATS => {
                Ok(RpcReply::Stream(watch_stream(self.workspace.watch_chats())))
            }
            methods::WATCH_DEVICES => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_devices(),
            ))),
            methods::WATCH_SPACES => Ok(RpcReply::Stream(watch_stream(
                self.workspace.watch_spaces(),
            ))),
            methods::WATCH_SESSIONS => {
                // Local live statuses merged with remote devices' workspace rows.
                let merged = self
                    .workspace
                    .merged_sessions_watch(self.sessions.watch_sessions());
                Ok(RpcReply::Stream(watch_stream(merged)))
            }
            methods::LOCAL_DEVICE => {
                // The version travels with the identity because the two are
                // asked together and answered by the same process: `comet-board
                // doctor` compares it against its own build to catch a CLI that
                // stopped being upgraded alongside the engine (gh#156).
                RpcReply::value(&serde_json::json!({
                    "deviceId": self.doc_host.device_id(),
                    "version": env!("CARGO_PKG_VERSION"),
                }))
            }
            methods::EDGE_HEALTH => {
                // Absent only in bare-core test assemblies; an engine that
                // cannot answer reports an unwired edge rather than an error,
                // so a status line never fails over a missing probe.
                let health = self
                    .edge_health
                    .as_ref()
                    .map(|probe| probe())
                    .unwrap_or_default();
                RpcReply::value(&health)
            }
            methods::UPDATE_STATUS => Ok(RpcReply::Stream(watch_stream(self.updater()?.watch()))),
            methods::APPLY_UPDATE => {
                let version = self
                    .updater()?
                    .apply()
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&serde_json::json!({ "ok": true, "version": version }))
            }
            methods::MUTATE => {
                let p: MutateParams = parse_params(params)?;
                self.mutate(p)?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            // Board surface (comet-board fork, §runtime-impl). Served off
            // the board service's loop; absent when the board is disabled.
            methods::WATCH_BOARD => Ok(RpcReply::Stream(watch_stream(self.board()?.watch_rows()))),
            methods::WATCH_BOARD_ORCHESTRATOR => Ok(RpcReply::Stream(watch_stream(
                self.board()?.watch_orchestrator(),
            ))),
            // The runtimes a dispatch can be pointed at, and whether each could
            // actually start *here* (gh#187) — the board core's names, stamped
            // with this device's answer, so the pickers and the engine validate
            // against the same set and warn about the same gaps. Forwardable,
            // which is the point: a laptop's picker is asking about the box the
            // work would land on, not about the laptop. Not board-loop state:
            // served regardless of the service's health.
            methods::LIST_BOARD_RUNTIMES => {
                RpcReply::value(&crate::runtimes::options(&self.agent_accounts))
            }
            methods::DISPATCH_TASK => {
                let p: DispatchTaskParams = parse_params(params)?;
                let board = self.board()?;
                let overrides = DispatchOverrides {
                    runtime: p.runtime,
                    model: p.model,
                    account: p.account,
                    bill: p.bill,
                };
                let origin = self
                    .dispatch_origin(caller, p.via, p.via_device, p.via_user)
                    .await;
                let dispatched = if p.replace {
                    board.retry_task(&p.task_id, origin, overrides).await
                } else {
                    board.dispatch_task(&p.task_id, origin, overrides).await
                }
                .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&dispatched)
            }
            methods::CANCEL_TASK => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct P {
                    task_id: String,
                }
                let p: P = parse_params(params)?;
                self.board()?
                    .cancel_task(&p.task_id)
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            // Throughput over a window (gh#143) — read when the page opens,
            // never streamed: a full aggregate on every board tick would cost
            // every connected viewport a recompute nobody is looking at.
            methods::BOARD_STATS => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct P {
                    #[serde(default)]
                    since_days: Option<i64>,
                }
                let p: P = parse_params(params)?;
                let stats = self
                    .board()?
                    .stats(p.since_days)
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&stats)
            }
            // The issue text behind one row (gh#132) — read when a detail
            // surface opens it, never streamed with the rows.
            methods::READ_BOARD_TASK => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct P {
                    task_id: String,
                }
                let p: P = parse_params(params)?;
                let detail = self
                    .board()?
                    .task_detail(&p.task_id)
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&detail)
            }
            // The claim contract (§gh#183). The refusal a malformed claim gets
            // comes back as the call's error, because the format is enforced
            // here and not in whatever wrote the params.
            methods::SUBMIT_CLAIMS => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct P {
                    task_id: String,
                    text: String,
                }
                let p: P = parse_params(params)?;
                let review = self
                    .board()?
                    .submit_claims(&p.task_id, &p.text)
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&review)
            }
            methods::READ_ATTEMPT_REVIEW => {
                #[derive(Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct P {
                    task_id: String,
                    #[serde(default)]
                    attempt: Option<i64>,
                }
                let p: P = parse_params(params)?;
                let review = self
                    .board()?
                    .attempt_review(&p.task_id, p.attempt)
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&review)
            }
            // The routing surface (gh#75). Served off the board's own paths, so
            // this answers about the config the running loop reads — and keeps
            // answering when that config is what stopped the loop.
            methods::READ_BOARD_CONFIG => {
                let view = comet_board::routes::read(self.board()?.paths())
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&self.config_reply(view).await?)
            }
            methods::WRITE_BOARD_CONFIG => {
                let p: BoardConfigWrite = parse_params(params)?;
                let paths = self.board()?.paths().clone();
                let view = self
                    .write_board_config(&paths, p)
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                // Republish what a frontend is watching off this file before
                // replying (gh#104): pinning a chat is a direct action, and the
                // sidebar glyph should be there when the click returns rather
                // than up to a sync interval later.
                self.board()?.note_config(&view);
                RpcReply::value(&self.config_reply(view).await?)
            }
            // Clone + space + adopt, all on this device (gh#97). Slow by
            // nature — it is a `git clone` — and unary, so a caller that gives
            // up leaves the clone running rather than half-written.
            methods::ONBOARD_REPO => {
                let p: OnboardParams = parse_params(params)?;
                let done = self
                    .onboard_repo(p)
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&done)
            }
            methods::LIST_APP_REPOS => {
                let repos = self
                    .list_app_repos()
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                RpcReply::value(&repos)
            }
            // The repo-first picker's whole world, from the box (gh#118).
            methods::LIST_REPO_SPACES => {
                let view = self
                    .list_repo_spaces()
                    .await
                    .map_err(|e| RpcError::Failed(format!("{e:#}")))?;
                Ok(RpcReply::Value(view))
            }
            methods::WATCH_CHECKOUT_DIFFS => {
                Ok(RpcReply::Stream(watch_stream(self.diff_sync.watch_diffs())))
            }
            methods::LIST_REPOS => RpcReply::value(&self.repos.list().await),
            methods::ADD_REPO => {
                #[derive(Deserialize)]
                struct P {
                    path: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .add(&p.path)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::CLONE_REPO => {
                #[derive(Deserialize)]
                struct P {
                    url: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .clone_repo(&p.url)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::CREATE_REPO => {
                #[derive(Deserialize)]
                struct P {
                    name: String,
                }
                let p: P = parse_params(params)?;
                let repo = self
                    .repos
                    .create(&p.name)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&repo)
            }
            methods::LIST_BRANCHES => {
                let p: RepoPathParams = parse_params(params)?;
                let branches = self
                    .repos
                    .branches(std::path::Path::new(&p.repo_path))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&branches)
            }
            methods::LIST_REFS => {
                let p: RepoPathParams = parse_params(params)?;
                let refs = self
                    .repos
                    .refs(std::path::Path::new(&p.repo_path))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&refs)
            }
            methods::SWITCH_REF => {
                let p: SwitchRefParams = parse_params(params)?;
                let branch = self
                    .repos
                    .switch_ref(std::path::Path::new(&p.repo_path), &p.ref_name)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "branch": branch }))
            }
            methods::LIST_FOLDERS => {
                let p: ListFoldersParams = parse_params(params)?;
                let listing = self
                    .repos
                    .list_folders(p.path)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&listing)
            }
            methods::CREATE_WORKTREE => {
                let p: CreateWorktreeParams = parse_params(params)?;
                let worktree = self
                    .repos
                    .create_worktree(std::path::Path::new(&p.repo_path), &p.branch)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&worktree)
            }
            methods::DELETE_WORKTREE => {
                let p: DeleteWorktreeParams = parse_params(params)?;
                self.repos
                    .delete_worktree(
                        std::path::Path::new(&p.repo_path),
                        std::path::Path::new(&p.worktree_path),
                        // The frontend deletes a worktree it did not name a
                        // branch for, so only comet's own `comet/…` branch is
                        // ours to remove here.
                        None,
                    )
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::OPEN_TERMINAL => {
                let p: OpenTerminalParams = parse_params(params)?;
                // The terminal runs in the chat's checkout; a chat with no cwd (or
                // no row yet) gets the home directory.
                let cwd = self
                    .workspace
                    .doc()
                    .chat(&p.chat_id)
                    .ok()
                    .flatten()
                    .and_then(|chat| chat.cwd)
                    .unwrap_or_else(|| home_dir().to_string_lossy().to_string());
                let session = self
                    .terminals
                    .open(&cwd, p.cols, p.rows)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&session)
            }
            methods::SUBSCRIBE_TERMINAL => {
                let p: SubscribeTerminalParams = parse_params(params)?;
                let rx = self
                    .terminals
                    .subscribe(&p.terminal_id, p.after_seq)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                let stream = futures::stream::unfold(rx, |mut rx| async move {
                    let event = rx.recv().await?;
                    let value = serde_json::to_value(&event).ok()?;
                    Some((value, rx))
                });
                Ok(RpcReply::Stream(stream.boxed()))
            }
            methods::WRITE_TERMINAL => {
                let p: WriteTerminalParams = parse_params(params)?;
                self.terminals
                    .write(&p.terminal_id, &p.data)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::RESIZE_TERMINAL => {
                let p: ResizeTerminalParams = parse_params(params)?;
                self.terminals
                    .resize(&p.terminal_id, p.cols, p.rows)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::CLOSE_TERMINAL => {
                let p: TerminalIdParams = parse_params(params)?;
                self.terminals
                    .close(&p.terminal_id)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::LIST_AGENT_ACCOUNTS => {
                let p: ListAgentAccountsParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .list(p.force_usage.unwrap_or(false))
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::ACTIVATE_AGENT_ACCOUNT => {
                let p: AgentAccountParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .activate(p.harness, &p.account_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::FORGET_AGENT_ACCOUNT => {
                let p: AgentAccountParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .forget(p.harness, &p.account_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::START_AGENT_LOGIN => {
                let p: StartAgentLoginParams = parse_params(params)?;
                // Two independent tells that nobody is sitting at this device:
                // the caller named it by id (the switcher only does that for a
                // device that is not its own), or the call came in over the
                // relay instead of this device's IPC. Either is enough.
                let remote = p.target_device_id.is_some() || caller.is_verified();
                let start = self
                    .agent_accounts
                    .start_login(p.harness, remote)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&start)
            }
            methods::COMPLETE_AGENT_LOGIN => {
                let p: CompleteAgentLoginParams = parse_params(params)?;
                let snapshot = self
                    .agent_accounts
                    .complete_login(&p.login_id, &p.code)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&snapshot)
            }
            methods::POLL_AGENT_LOGIN => {
                let p: LoginIdParams = parse_params(params)?;
                let poll = self
                    .agent_accounts
                    .poll_login(&p.login_id)
                    .await
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&poll)
            }
            methods::CANCEL_AGENT_LOGIN => {
                let p: LoginIdParams = parse_params(params)?;
                self.agent_accounts.cancel_login(&p.login_id);
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::UPLOAD_CHUNK => {
                let p: UploadChunkParams = parse_params(params)?;
                self.uploads
                    .append(&p.upload_id, &p.data, p.seq)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "ok": true }))
            }
            methods::UPLOAD_COMMIT => {
                let p: UploadCommitParams = parse_params(params)?;
                let path = self
                    .uploads
                    .commit(&p.upload_id, &p.file_name)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&serde_json::json!({ "path": path }))
            }
            methods::READ_ATTACHMENT_CHUNK => {
                let p: ReadAttachmentChunkParams = parse_params(params)?;
                // Path jail: the uploads dir plus every workspace-known chat cwd.
                let roots: Vec<std::path::PathBuf> = self
                    .workspace
                    .doc()
                    .read_chats()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|chat| chat.cwd)
                    .map(std::path::PathBuf::from)
                    .collect();
                let chunk = self
                    .uploads
                    .read_chunk(&p.path, p.offset, &roots)
                    .map_err(|e| RpcError::Failed(e.to_string()))?;
                RpcReply::value(&chunk)
            }
            other => Err(RpcError::UnknownMethod(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The UI's Switch/Forget calls send `{id, accountId, harness}` (+ optional
    /// `targetDeviceId`); the extra fields must be tolerated, `accountId` wins.
    #[test]
    fn agent_account_params_accept_ui_shape() {
        let p: AgentAccountParams = parse_params(serde_json::json!({
            "id": "acct-1",
            "accountId": "acct-1",
            "harness": "claude-code",
            "targetDeviceId": "dev-2",
        }))
        .expect("ui param shape");
        assert_eq!(p.account_id, "acct-1");
        assert_eq!(p.harness, HarnessId::ClaudeCode);
    }

    /// gh#193: `StartAgentLogin` reads the routing passthrough as meaning. The
    /// accounts page omits it for its own device and sends it for every other,
    /// which is exactly the question Codex has to answer before it picks a
    /// flow — so the binding is the whole feature, not a detail of it.
    #[test]
    fn start_agent_login_params_carry_the_remote_signal() {
        let local: StartAgentLoginParams =
            parse_params(serde_json::json!({ "harness": "codex" })).expect("local shape");
        assert_eq!(local.harness, HarnessId::Codex);
        assert_eq!(local.target_device_id, None);

        let remote: StartAgentLoginParams =
            parse_params(serde_json::json!({ "harness": "codex", "targetDeviceId": "device-box" }))
                .expect("relayed shape");
        assert_eq!(remote.target_device_id.as_deref(), Some("device-box"));
    }

    /// gh#74: the panel and the TUI hand-write these keys. `account` decides
    /// whose subscription a run burns and `viaDevice`/`viaUser` are the only
    /// record of who released it, so a name that fails to bind is a silent
    /// wrong answer, not an error.
    #[test]
    fn dispatch_task_params_bind_the_names_the_frontends_send() {
        let p: DispatchTaskParams = parse_params(serde_json::json!({
            "taskId": "gh:owner/widget#74",
            "targetDeviceId": "device-box",
            "runtime": "opencode",
            "model": "gpt-5.2",
            "account": "slot-ana",
            "viaDevice": "laptop-ana",
            "viaUser": "ana@example.com",
            "replace": true,
        }))
        .expect("the shape both frontends send");
        assert_eq!(p.task_id, "gh:owner/widget#74");
        assert_eq!(p.runtime.as_deref(), Some("opencode"));
        assert_eq!(p.model.as_deref(), Some("gpt-5.2"));
        assert_eq!(p.account.as_deref(), Some("slot-ana"));
        assert_eq!(p.via_device.as_deref(), Some("laptop-ana"));
        assert_eq!(p.via_user.as_deref(), Some("ana@example.com"));
        assert!(p.replace);

        // The CLI's shape: a task and its provenance, nothing else.
        let p: DispatchTaskParams =
            parse_params(serde_json::json!({ "taskId": "t", "via": "chat-parent" }))
                .expect("the CLI's shape");
        assert_eq!(p.via.as_deref(), Some("chat-parent"));
        assert_eq!(p.via_device, None);
        assert_eq!(p.via_user, None);
        assert!(!p.replace);
    }

    /// gh#134: both composers ask with only what they know. An open chat sends
    /// its id and lets the host resolve the rest; the new-chat canvas has no
    /// chat and sends the space's folder instead.
    #[test]
    fn list_skills_params_accept_both_composer_shapes() {
        let p: ListSkillsParams =
            parse_params(serde_json::json!({ "chatId": "c1", "targetDeviceId": "box" }))
                .expect("open chat");
        assert_eq!(p.chat_id.as_deref(), Some("c1"));
        assert_eq!(p.cwd, None);
        assert_eq!(p.harness, None);

        let p: ListSkillsParams =
            parse_params(serde_json::json!({ "cwd": "/dev/comet", "harness": "claude-code" }))
                .expect("new-chat canvas");
        assert_eq!(p.cwd.as_deref(), Some("/dev/comet"));
        assert_eq!(p.harness, Some(HarnessId::ClaudeCode));

        // And an empty ask is legal: nothing known yet is an empty menu, not
        // a bad-params error under a composer mid-keystroke.
        assert!(parse_params::<ListSkillsParams>(serde_json::json!({})).is_ok());
    }

    #[test]
    fn local_device_is_not_forwardable() {
        assert!(!forwardable(methods::LOCAL_DEVICE));
        assert!(forwardable(methods::QUEUE_COMMAND));
        // Skills are files on the chat's host, and which files depends on the
        // agent account that chat names (gh#134).
        assert!(forwardable(methods::LIST_SKILLS));
    }

    /// gh#97: onboarding is three device-local effects behind one verb — a
    /// clone on the box's disk, a space the box owns, and the box's
    /// routing.toml — and the laptop asking for it usually has none of the
    /// three, nor a GitHub credential to resolve the repo with. Unforwardable,
    /// it would only ever work from a shell on the box, which is the thing it
    /// exists to stop needing.
    #[test]
    fn onboarding_reaches_the_box_the_way_the_rest_of_the_board_does() {
        assert!(forwardable(methods::ONBOARD_REPO));
        assert!(forwardable(methods::LIST_APP_REPOS));
        // Unary, both of them: a clone that streamed progress would need the
        // relay's stream proxy, and neither has progress to report yet.
        assert!(!is_stream_method(methods::ONBOARD_REPO));
        assert!(!is_stream_method(methods::LIST_APP_REPOS));
    }

    /// gh#118: the repo-first picker asks the box which of *its* spaces are
    /// which repo. Same reason as the rest — the disk being asked about is the
    /// box's, and the picker is usually a phone.
    #[test]
    fn the_repo_pickers_list_reaches_the_box_too() {
        assert!(forwardable(methods::LIST_REPO_SPACES));
        assert!(!is_stream_method(methods::LIST_REPO_SPACES));
    }

    /// The params a picker and the CLI both send. `dir` and `labels` are absent
    /// by default and mean different things from empty ones: no `dir` is the
    /// engine's own clone root, and no `labels` keeps `[github] labels` where
    /// `[]` deliberately overrides it.
    #[test]
    fn onboard_params_default_to_absent_rather_than_empty() {
        let bare: OnboardParams = serde_json::from_value(serde_json::json!({
            "slug": "o/r",
            "targetDeviceId": "dev-box",
        }))
        .expect("the host passthrough must be tolerated like every other call");
        assert_eq!(bare.slug, "o/r");
        assert_eq!(bare.dir, None);
        assert_eq!(bare.labels, None);

        let every: OnboardParams = serde_json::from_value(serde_json::json!({
            "slug": "o/r",
            "dir": "~/dev/r",
            "labels": [],
        }))
        .unwrap();
        assert_eq!(every.dir.as_deref(), Some("~/dev/r"));
        assert_eq!(every.labels, Some(Vec::new()), "`[]` is not `null`");
    }
}
