//! comet-engine — the headless backend: sessions engine, doc host + command executor,
//! run journal + crash recovery, and the IPC RPC server.
//!
//! Spec: ARCHITECTURE.md §5 and docs/research/feature-inventory.md §3. M2 surface:
//! sessions + docs + commands + minimal IPC. Terminals, repos/diffs, uploads, auth,
//! agent accounts, and the device-room host land in later milestones.

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use comet_proto::HarnessId;

use comet_sync::DocsStore;

pub mod agent_accounts;
pub mod auth;
pub mod board;
pub mod board_runtime;
pub mod crash_shield;
pub mod diff_sync;
pub mod doc_host;
pub mod instance_lock;
pub mod org_devices;
pub mod push_credentials;
pub mod registry;
pub mod repos;
pub mod rpc;
pub mod run_journal;
pub mod sessions;
pub mod skills;
pub mod spaces;
pub mod terminals;
pub mod titles;
pub mod uploads;
pub mod workspace_host;

pub use agent_accounts::{AgentAccounts, AgentAccountsConfig};
pub use auth::{Auth, AuthConfig, AuthState, AuthUser, OrgMembership};
pub use board::{BoardService, board_enabled_from_env};
pub use board_runtime::CometRuntime;
pub use diff_sync::{CheckoutDiffSync, DiffSidecar, DiffSnapshot, capture_diff};
pub use doc_host::{ChatDocHandle, DocHost, DocHostConfig, EdgeConfig};
pub use instance_lock::InstanceLock;
pub use org_devices::{ORG_DEVICES_DOC_ID, OrgDevices, OrgDevicesConfig};
pub use registry::{HarnessDescriptor, HarnessRegistry, default_registry};
pub use repos::{CheckoutIdentity, Repos, worktree_branch_from_title};
pub use rpc::EngineRpc;
pub use run_journal::{JournalError, RunJournal};
pub use sessions::{JournaledEvent, SessionsEngine, SteerOutcome};
pub use spaces::SpacesSync;
pub use terminals::Terminals;
pub use titles::TitleGenerator;
pub use uploads::{AttachmentChunk, Uploads};
pub use workspace_host::{
    DEFAULT_ORG_ID, DEFAULT_USER_ID, WORKSPACE_DOC_ID, WorkspaceHost, WorkspaceHostConfig,
};

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("doc: {0}")]
    Doc(#[from] comet_doc::DocError),
    #[error("journal: {0}")]
    Journal(#[from] run_journal::JournalError),
    #[error("store: {0}")]
    Store(#[from] comet_sync::StoreError),
    #[error("harness: {0}")]
    Harness(#[from] comet_harness::HarnessError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

/// Epoch millis now — the doc/journal timestamp base.
pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `edge_url` sentinel that disables the edge entirely (`COMET_EDGE_URL=off`,
/// any case): no room joins, no presence, no device room, no release polling,
/// and dev-mode auth. The stated configuration for intentionally local
/// (single-box) deployments, as opposed to offline-tolerant failing.
pub fn edge_url_is_off(url: &str) -> bool {
    url.trim().eq_ignore_ascii_case("off")
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Data directory (default `~/.comet-native`, dev `~/.comet-native-dev`).
    pub data_dir: PathBuf,
    /// Edge base URL, or the [`edge_url_is_off`] sentinel for local mode.
    pub edge_url: String,
    /// Bearer for edge room joins; `None` runs fully offline (sync disabled).
    pub edge_token: Option<String>,
    /// Localhost IPC port for the UI.
    pub ipc_port: u16,
    /// Harness for doc-command runs on chats without a workspace `config` row.
    pub default_harness: HarnessId,
    /// Workspace-doc org (`ws/{orgId}` room). `None` = `$COMET_ORG_ID` or the dev default.
    /// In WorkOS mode the signed-in session's org wins.
    pub org_id: Option<String>,
    /// WorkOS client id — enables real auth; `None` = dev mode (bearer = `edge_token`).
    pub workos_client_id: Option<String>,
    /// Host the board service (docs/BOARD.md §H1). Default on via
    /// [`board_enabled_from_env`] (`COMET_BOARD=0` disables); with no board
    /// config on disk the loop is idle-cheap.
    pub board: bool,
}

impl EngineConfig {
    /// False in local mode ([`edge_url_is_off`]) — every edge transport
    /// (rooms, presence, device room, nudges, release polling) stays unbuilt.
    pub fn edge_enabled(&self) -> bool {
        !edge_url_is_off(&self.edge_url)
    }
}

/// The assembled engine core — also constructible without the IPC server for tests
/// and the in-process (headed) mode.
pub struct EngineCore {
    pub sessions: SessionsEngine,
    pub doc_host: DocHost,
    pub workspace: WorkspaceHost,
    pub registry: Arc<HarnessRegistry>,
    pub repos: Repos,
    pub terminals: Terminals,
    pub diff_sync: CheckoutDiffSync,
    pub spaces_sync: SpacesSync,
    pub uploads: Uploads,
    pub agent_accounts: AgentAccounts,
    pub device_id: String,
    /// The edge this engine syncs against, if any — `None` is local mode, in
    /// which holding zero edge connections is correct rather than a fault.
    edge_url: Option<String>,
    /// Auth service (attached by [`Engine::run`]; a lazy dev-mode instance otherwise).
    auth: std::sync::Mutex<Option<Auth>>,
    /// Peer link cache for `targetDeviceId` routing (attached when edge+auth are ready).
    links: std::sync::Mutex<Option<Arc<comet_rpc::LinkCache>>>,
    /// Release checker (attached by [`Engine::assemble_runtime`]) — the
    /// UpdateStatus stream + ApplyUpdate.
    updater: std::sync::Mutex<Option<comet_update::Updater>>,
    /// Board service (attached by [`Engine::assemble_runtime`] when enabled) —
    /// the sync loop over `board.db`, serving `WatchBoard` / `DispatchTask` /
    /// `CancelTask` through [`rpc::EngineRpc`].
    board: std::sync::Mutex<Option<Arc<board::BoardService>>>,
    /// Liveness of the DeviceRoom host socket, registered by
    /// [`Self::start_host_relay`]. The relay handle itself lives in
    /// [`EngineRuntime`], but the question "can anyone remote reach this box"
    /// has to be answerable from the RPC surface — see [`Self::edge_health`].
    ///
    /// Shared (`Arc`) because the probe handed to `EngineRpc` is built BEFORE
    /// the relay exists — `start_host_relay` builds an RPC service of its own
    /// to serve — so it has to read this slot live rather than snapshot it.
    host_relay: Arc<std::sync::Mutex<Option<tokio::sync::watch::Receiver<bool>>>>,
    /// Exclusive data-dir lock — held for the engine's lifetime (single-instance).
    _instance_lock: InstanceLock,
}

impl EngineCore {
    /// Open stores under `data_dir`, wire sessions ⇄ doc host ⇄ workspace host, and
    /// recover stale journals from a previous crash. Identity comes from
    /// `$COMET_ORG_ID` / `$COMET_USER_ID` (dev defaults `dev-org` / `dev-user`);
    /// use [`Self::assemble_with_identity`] to pass one explicitly.
    pub fn assemble(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
    ) -> Result<Self, EngineError> {
        let org_id = env_or("COMET_ORG_ID", DEFAULT_ORG_ID);
        let user_id = env_or("COMET_USER_ID", DEFAULT_USER_ID);
        Self::assemble_with_identity(data_dir, registry, default_harness, edge, &org_id, &user_id)
    }

    pub fn assemble_with_identity(
        data_dir: &Path,
        registry: Arc<HarnessRegistry>,
        default_harness: HarnessId,
        edge: Option<EdgeConfig>,
        org_id: &str,
        user_id: &str,
    ) -> Result<Self, EngineError> {
        std::fs::create_dir_all(data_dir)?;
        // Single-instance guard: two engines on one data dir would race the
        // SQLite snapshots + journals. Taken before any store opens or the IPC
        // port binds; held (and kernel-released on crash) for the engine's life.
        let lock = InstanceLock::acquire(data_dir)?;
        let device_id = load_or_create_device_id(data_dir)?;
        // Identity-scoped storage: snapshots, the command ledger, and run
        // journals live under `orgs/{orgId}/{userId}/` so switching accounts or
        // orgs on one machine never reuses another identity's cached docs.
        let org_dir = data_dir
            .join("orgs")
            .join(sanitize_path_id(org_id))
            .join(sanitize_path_id(user_id));
        let store = Arc::new(DocsStore::open(&org_dir)?);
        let journal = Arc::new(RunJournal::open(org_dir.join("journals"))?);
        let sessions = SessionsEngine::new(device_id.clone(), journal, registry.clone());
        let doc_host = DocHost::new(
            store.clone(),
            DocHostConfig {
                device_id: device_id.clone(),
                default_harness,
                edge: edge.clone(),
            },
        );
        let workspace = WorkspaceHost::open(
            store,
            WorkspaceHostConfig {
                device_id: device_id.clone(),
                device_name: local_device_name(),
                platform: std::env::consts::OS.to_string(),
                org_id: org_id.to_string(),
                user_id: user_id.to_string(),
                edge: edge.clone(),
            },
        )?;
        doc_host.set_workspace(workspace.clone());
        doc_host.set_sessions(sessions.clone());
        sessions.set_doc_host(doc_host.clone());
        match sessions.recover_stale() {
            Ok(0) => {}
            Ok(recovered) => tracing::info!(recovered, "stale sessions recovered on boot"),
            Err(err) => tracing::error!(error = %err, "stale-session recovery failed"),
        }
        // §3.3 boot warm-open: after recovery (so a revived run owns its chat's
        // handle first) but before we start serving. Needs a runtime — every
        // open spawns the chat task — so a bare synchronous caller skips it
        // rather than panicking.
        if tokio::runtime::Handle::try_current().is_ok() {
            doc_host.warm_open_recent();
        }
        let repos = Repos::new(data_dir, &device_id);
        let terminals = Terminals::new();
        let uploads = Uploads::new(data_dir, edge.clone());
        let agent_accounts = AgentAccounts::new(AgentAccountsConfig::detect(data_dir));
        // Per-run agent accounts (gh#59): a chat naming a slot has it
        // materialized into its own config dir and stamped into the harness
        // child's env, instead of the engine swapping the shared one.
        sessions.set_accounts(agent_accounts.clone());
        sessions.set_titles(TitleGenerator::new(
            workspace.clone(),
            registry.clone(),
            repos.clone(),
        ));
        let edge_url = edge.as_ref().map(|edge| edge.url.clone());
        let diff_sync = CheckoutDiffSync::start(repos.clone(), workspace.clone(), &device_id, edge);
        let spaces_sync = SpacesSync::start(repos.clone(), workspace.clone(), &device_id);
        Ok(Self {
            sessions,
            doc_host,
            workspace,
            registry,
            repos,
            terminals,
            diff_sync,
            spaces_sync,
            uploads,
            agent_accounts,
            device_id,
            edge_url,
            auth: std::sync::Mutex::new(None),
            links: std::sync::Mutex::new(None),
            updater: std::sync::Mutex::new(None),
            board: std::sync::Mutex::new(None),
            host_relay: Arc::new(std::sync::Mutex::new(None)),
            _instance_lock: lock,
        })
    }

    /// Attach the auth service (before building the RPC service / relays).
    pub fn set_auth(&self, auth: Auth) {
        *self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(auth);
    }

    /// The attached auth service, or a lazily-created dev-mode one (in-process embeds
    /// that never wired WorkOS still answer AuthStatus honestly).
    pub fn auth(&self) -> Auth {
        let mut slot = self
            .auth
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slot.get_or_insert_with(|| {
            let dev_user = std::env::var("COMET_EDGE_TOKEN")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "dev-user".into());
            let mut config = AuthConfig::new("http://localhost:27640", std::env::temp_dir());
            config.dev_user_id = dev_user;
            Auth::new(config)
        })
        .clone()
    }

    /// Attach the peer link cache — enables `targetDeviceId` routing and [`Self::dial_device`].
    pub fn set_links(&self, links: Arc<comet_rpc::LinkCache>) {
        *self
            .links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(links);
    }

    pub fn links(&self) -> Option<Arc<comet_rpc::LinkCache>> {
        self.links
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Attach the board service.
    pub fn set_board(&self, board: Arc<board::BoardService>) {
        *self
            .board
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(board);
    }

    pub fn board(&self) -> Option<Arc<board::BoardService>> {
        self.board
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Attach the release checker (before building the RPC service).
    pub fn set_updater(&self, updater: comet_update::Updater) {
        *self
            .updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(updater);
    }

    pub fn updater(&self) -> Option<comet_update::Updater> {
        self.updater
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A live RPC client to another device's engine through its relay DO (the router's
    /// dial seam). Cached per device; invalidated + re-dialed on failure.
    pub async fn dial_device(
        &self,
        device_id: &str,
    ) -> Result<Arc<comet_rpc::RpcClient>, EngineError> {
        let links = self
            .links()
            .ok_or_else(|| EngineError::Other("peer links unavailable (offline)".into()))?;
        links
            .client(device_id)
            .await
            .map_err(|e| EngineError::Other(e.to_string()))
    }

    /// Start hosting our device room: serve the full RPC surface to relay clients and
    /// warm-open chat docs on nudges (§7 cold-chat command delivery). The token source
    /// re-reads auth on every (re)dial, so token refreshes take effect at reconnect.
    pub fn start_host_relay(&self, edge_url: &str) -> comet_rpc::HostRelay {
        let auth = self.auth();
        let config =
            comet_rpc::HostRelayConfig::new(edge_url, self.device_id.clone(), Arc::new(auth));
        let doc_host = self.doc_host.clone();
        let on_nudge: comet_rpc::NudgeHandler = Arc::new(move |chat_id: String| {
            // Opening the doc joins its room + syncs; drain fires on the change
            // subscription — the command executes with no standing per-chat socket.
            match doc_host.open(&chat_id) {
                Ok(_) => tracing::info!(chat = %chat_id, "nudge: chat doc opened"),
                Err(err) => {
                    tracing::warn!(chat = %chat_id, error = %err, "nudge: open failed")
                }
            }
        });
        let relay = comet_rpc::HostRelay::spawn(config, self.rpc_service(), on_nudge);
        *self
            .host_relay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(relay.watch_connected());
        relay
    }

    /// Which edge connections this engine holds RIGHT NOW (gh#116).
    ///
    /// Every field is read live from the thing that owns the socket, never from
    /// a cached "we started it" flag — the whole point is to be able to
    /// contradict the engine's own optimism.
    pub fn edge_health(&self) -> comet_proto::EdgeHealth {
        edge_health(
            &self.edge_url,
            &self.host_relay,
            &self.workspace,
            &self.doc_host,
        )
    }

    /// [`Self::edge_health`] as a closure the RPC service can hold — see
    /// [`rpc::EdgeHealthProbe`].
    fn edge_health_probe(&self) -> rpc::EdgeHealthProbe {
        let edge_url = self.edge_url.clone();
        let host_relay = self.host_relay.clone();
        let workspace = self.workspace.clone();
        let doc_host = self.doc_host.clone();
        Arc::new(move || edge_health(&edge_url, &host_relay, &workspace, &doc_host))
    }

    pub fn rpc_service(&self) -> Arc<EngineRpc> {
        let mut rpc = EngineRpc::new(
            self.sessions.clone(),
            self.doc_host.clone(),
            self.workspace.clone(),
            self.registry.clone(),
            self.repos.clone(),
            self.terminals.clone(),
            self.diff_sync.clone(),
            self.uploads.clone(),
            self.agent_accounts.clone(),
        )
        .with_auth(self.auth());
        if let Some(links) = self.links() {
            rpc = rpc.with_links(links);
        }
        if let Some(updater) = self.updater() {
            rpc = rpc.with_updater(updater);
        }
        if let Some(board) = self.board() {
            rpc = rpc.with_board(board);
        }
        Arc::new(rpc.with_edge_health(self.edge_health_probe()))
    }

    /// Graceful teardown: settle live runs (streaming entries stamped `aborted`),
    /// kill live PTYs, stamp our workspace `lastSeenAt`, and flush every open doc
    /// snapshot.
    pub async fn shutdown(&self) {
        // First, so its final cycle sees live session state and its SQLite
        // writes land before the stores close.
        let board = self
            .board
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(board) = board {
            board.shutdown();
        }
        self.sessions.shutdown().await;
        self.terminals.shutdown();
        self.agent_accounts.shutdown();
        self.doc_host.flush_all();
        self.workspace.shutdown();
    }

    /// A DETACHED copy of [`Self::shutdown`] for the crash shield: the same
    /// teardown, over cloned service handles rather than a borrow of the core.
    /// It has to be detached — the shield runs it after a panic that may well
    /// have happened inside the core, and a `&self` closure would keep the
    /// panicking frame's borrow alive.
    ///
    /// Build it AFTER assembly (the board attaches during
    /// [`Engine::assemble_runtime`]), or the shield will drain a core whose
    /// board loop it never learned about.
    pub fn drain_hook(&self) -> crash_shield::Drain {
        let board = self.board();
        let sessions = self.sessions.clone();
        let terminals = self.terminals.clone();
        let agent_accounts = self.agent_accounts.clone();
        let doc_host = self.doc_host.clone();
        let workspace = self.workspace.clone();
        Arc::new(move || {
            let board = board.clone();
            let sessions = sessions.clone();
            let terminals = terminals.clone();
            let agent_accounts = agent_accounts.clone();
            let doc_host = doc_host.clone();
            let workspace = workspace.clone();
            Box::pin(async move {
                if let Some(board) = board {
                    board.shutdown();
                }
                sessions.shutdown().await;
                terminals.shutdown();
                agent_accounts.shutdown();
                doc_host.flush_all();
                workspace.shutdown();
            })
        })
    }
}

/// The census behind [`EngineCore::edge_health`], over the cloneable pieces so
/// the RPC probe can hold them without holding the core.
fn edge_health(
    edge_url: &Option<String>,
    host_relay: &std::sync::Mutex<Option<tokio::sync::watch::Receiver<bool>>>,
    workspace: &WorkspaceHost,
    doc_host: &DocHost,
) -> comet_proto::EdgeHealth {
    let (chat_rooms_open, chat_rooms_live) = doc_host.room_census();
    let online = edge_url.is_some();
    comet_proto::EdgeHealth {
        edge_url: edge_url.clone(),
        host_relay: host_relay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|state| *state.borrow()),
        workspace_room: online.then(|| workspace.connected()),
        workspace_presence: online.then(|| workspace.presence_connected()),
        org_registry: online.then(|| workspace.org_devices_connected()),
        org_presence: online.then(|| workspace.org_devices().presence_connected()),
        chat_rooms_open,
        chat_rooms_live,
    }
}

pub struct Engine {
    pub config: EngineConfig,
}

/// A fully assembled identity-scoped engine plus the relay handle whose lifetime
/// keeps this device reachable. Used by both the headless server and the headed
/// in-process engine so their production authentication paths cannot diverge.
pub struct EngineRuntime {
    core: EngineCore,
    _host_relay: Option<comet_rpc::HostRelay>,
}

impl EngineRuntime {
    pub fn core(&self) -> &EngineCore {
        &self.core
    }

    pub async fn shutdown(&self) {
        self.core.shutdown().await;
    }
}

impl Engine {
    pub fn new(config: EngineConfig) -> Self {
        Self { config }
    }

    /// Resolve the shared dev/WorkOS auth configuration for headed and headless
    /// modes. Production callers pass the baked WorkOS client id; explicit dev
    /// bearers still opt into the local dev identity.
    pub async fn build_auth(config: &EngineConfig) -> Auth {
        let mut auth_config = AuthConfig::new(config.edge_url.clone(), config.data_dir.clone());
        // Local mode has no edge to authenticate against: force dev auth so
        // neither the sign-in gate nor the `{edge}/health` probe ever fires.
        auth_config.workos_client_id = config
            .edge_enabled()
            .then(|| config.workos_client_id.clone())
            .flatten();
        if let Ok(base) = std::env::var("COMET_WORKOS_API_BASE")
            && !base.trim().is_empty()
        {
            auth_config.workos_api_base = base;
        }
        auth_config.callback_port = Some(
            std::env::var("COMET_CALLBACK_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(27641),
        );
        if let Some(token) = &config.edge_token {
            auth_config.dev_user_id = token.clone();
        }
        Auth::detect(auth_config).await
    }

    /// Open the identity-scoped stores and online transports for an auth session
    /// that is already ready. The headed UI waits behind its sign-in gate before
    /// calling this; headless mode waits on the terminal flow.
    pub async fn assemble_runtime(
        config: &EngineConfig,
        auth: Auth,
    ) -> anyhow::Result<EngineRuntime> {
        // Local mode wins over any token: a dev bearer (or saved session) must
        // not produce connect attempts against an edge configured away.
        let online = config.edge_enabled()
            && (auth.workos_enabled() || config.edge_token.is_some())
            && auth.access_token().await.is_some();
        let edge = online.then(|| EdgeConfig::new(config.edge_url.clone(), Arc::new(auth.clone())));

        let dev_token_org = config
            .edge_token
            .as_deref()
            .and_then(|t| t.split_once('@'))
            .map(|(_, org)| org.to_string())
            .filter(|s| !s.is_empty());
        let org_id = auth
            .state()
            .org_id()
            .map(str::to_string)
            .or(dev_token_org)
            .or(config.org_id.clone())
            .unwrap_or_else(|| env_or("COMET_ORG_ID", DEFAULT_ORG_ID));
        let user_id = auth
            .user_id()
            .unwrap_or_else(|| env_or("COMET_USER_ID", DEFAULT_USER_ID));
        let core = EngineCore::assemble_with_identity(
            &config.data_dir,
            Arc::new(default_registry()),
            config.default_harness,
            edge.clone(),
            &org_id,
            &user_id,
        )?;
        core.set_auth(auth.clone());
        // Release checker: polls {edge}/releases on a 6h cadence; headless
        // installs with COMET_AUTO_UPDATE=1 apply + restart themselves — gated
        // on quiescence so a restart never lands under a live run or open PTY.
        // Releases come from the edge, so local mode skips the poller too.
        if config.edge_enabled() {
            let quiescent: comet_update::QuiescentCheck = {
                let sessions = core.sessions.clone();
                let terminals = core.terminals.clone();
                Arc::new(move || !sessions.any_active() && !terminals.any_open())
            };
            core.set_updater(comet_update::Updater::spawn(
                config.edge_url.clone(),
                Some(quiescent),
            ));
        } else {
            tracing::info!(
                "edge disabled (local mode) — room sync, presence, device room, and release polling are off"
            );
        }
        tracing::info!(device_id = %core.device_id, "engine core assembled");

        // The board service (docs/BOARD.md §H1): the sync loop herdr-board ran
        // as `syncd`, fed by the same merged session stream `WatchSessions`
        // serves. Failure to start is a warning, not fatal — the engine's job
        // is chats, and the board is an addition.
        if config.board {
            let sessions_watch = core
                .workspace
                .merged_sessions_watch(core.sessions.watch_sessions());
            // The runtime reads the same mirror the loop is fed — one stream,
            // so the board and the frontends can never disagree on status.
            let runtime = Arc::new(board_runtime::CometRuntime::new(
                core.repos.clone(),
                core.workspace.clone(),
                core.doc_host.clone(),
                sessions_watch.clone(),
                core.sessions.journal(),
                core.agent_accounts.clone(),
                tokio::runtime::Handle::current(),
            ));
            // A dispatched agent pushes as the board's GitHub App rather than
            // as the box user (gh#68) — wired here rather than in the core
            // because it is the board's credential, and a device with no board
            // has none of this. Resolving the paths is the only fallible part,
            // and it fails the same way the board itself does.
            match comet_board::config::Paths::under(&config.data_dir) {
                Ok(paths) => core.sessions.set_push_credentials(Arc::new(
                    push_credentials::PushCredentials::detect(paths),
                )),
                Err(err) => tracing::warn!(
                    error = %err,
                    "board directories unreadable — dispatched agents will push with this device's git credentials"
                ),
            }
            match board::BoardService::spawn(
                &config.data_dir,
                sessions_watch,
                runtime,
                core.workspace.watch_spaces(),
                tokio::runtime::Handle::current(),
            ) {
                Ok(board) => core.set_board(Arc::new(board)),
                Err(err) => tracing::warn!(error = %err, "board service failed to start"),
            }
        }

        let host_relay = edge.as_ref().map(|edge| {
            let links = comet_rpc::LinkCache::new(comet_rpc::LinkCacheConfig::new(
                edge.url.clone(),
                Arc::new(auth.clone()),
            ));
            let links_for_presence = links.clone();
            core.workspace
                .set_peer_alive_hook(Arc::new(move |device_id: &str| {
                    links_for_presence.reset_cooldown(device_id);
                }));
            core.set_links(links);
            core.start_host_relay(&edge.url)
        });

        Ok(EngineRuntime {
            core,
            _host_relay: host_relay,
        })
    }

    /// Run until ctrl-c: auth (dev or WorkOS), sessions engine + doc host + command
    /// executor, IPC server, and — when edge+auth are ready — the device-room host
    /// relay + peer link cache (targetDeviceId routing).
    pub async fn run(self) -> anyhow::Result<()> {
        let config = self.config;
        tracing::info!(data_dir = %config.data_dir.display(), "engine starting");

        std::fs::create_dir_all(&config.data_dir)?;
        let auth = Self::build_auth(&config).await;
        let _refresh_loop = auth.spawn_refresh_loop();

        // WorkOS mode: gate edge features on a signed-in, org-scoped session. A TTY
        // gets the interactive paste-code flow; a service manager (systemd/launchd)
        // fails fast with a "run `comet login`" error instead of hanging on a prompt.
        if auth.workos_enabled() {
            terminal_sign_in(&auth).await?;
        }

        let runtime = Self::assemble_runtime(&config, auth).await?;

        // Crash shield (§3.1) — headless only, and deliberately so. This is the
        // process that runs unattended on the shared box, where a panicked task
        // leaves a "Working" row nobody will ever come clear. The headed app has
        // a human in front of it and a UI that can say so.
        crash_shield::install(runtime.core().drain_hook());

        // A daemon exists to serve this port, so a bind failure is fatal here —
        // unlike the headed app, which can still work over its in-process
        // transport (see `serve_ipc`).
        let server = serve_ipc(config.ipc_port, runtime.core().rpc_service()).await?;

        shutdown_signal().await?;
        tracing::info!("shutting down");
        server.abort();
        runtime.shutdown().await;
        Ok(())
    }
}

/// Ctrl-C or SIGTERM. systemd/launchd stop (and the auto-updater's service
/// restart) deliver SIGTERM — without catching it the daemon dies mid-write
/// and every stop takes the crash-recovery path instead of the graceful drain.
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = sigterm.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Serve the typed RPC on the localhost IPC port.
///
/// Both engines call this: the headless daemon, and the headed app's embedded
/// engine. That second case is the point — an embedded engine that keeps the
/// port to itself forces anyone wanting a second viewport (the terminal app) to
/// stop the desktop app, start a daemon, and start it again in the right order.
/// Serving here means any viewport can just attach.
///
/// Localhost only, exactly as before: this widens *which process* can serve the
/// port, not who can reach it.
pub async fn serve_ipc(
    port: u16,
    service: std::sync::Arc<dyn comet_rpc::RpcService>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "IPC server listening");
    Ok(tokio::spawn(comet_rpc::serve_ws_listener(
        listener, service,
    )))
}

/// Block until the WorkOS session is signed in AND org-scoped. On a TTY, print the
/// headless (paste-code) sign-in URL, read the pasted `state.code` from stdin, and
/// run workspace onboarding (create / auto-join / numbered picker). Off a TTY this
/// errors immediately — a daemon under systemd/launchd must load the session that
/// `comet login` persisted, never wait on a prompt nobody can see.
pub async fn terminal_sign_in(auth: &Auth) -> Result<(), EngineError> {
    use std::io::IsTerminal;
    let interactive = std::io::stdin().is_terminal();
    let mut state_rx = auth.watch_state();
    let mut stdin_reader: Option<tokio::task::JoinHandle<()>> = None;
    let mut org_reader: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        let state = state_rx.borrow().clone();
        match state {
            AuthState::SignedIn { user, org_id } => {
                tracing::info!(email = %user.email, org = org_id.as_deref().unwrap_or("<none>"),
                    "auth: session ready");
                break;
            }
            AuthState::NeedsOrganization { user } => {
                if !interactive {
                    // No reader tasks have been spawned on this path (both spawns
                    // are TTY-gated), so an early return leaks nothing.
                    return Err(EngineError::Other(format!(
                        "signed in as {} but no workspace is selected — run `comet login` on this machine to pick one",
                        user.email
                    )));
                }
                if org_reader.is_none() {
                    // Workspace onboarding on the TTY (old comet's
                    // `backend login` flow): create if none, auto-join a
                    // single membership, numbered picker otherwise.
                    println!("Signed in as {}.", user.email);
                    org_reader = Some(tokio::spawn(run_org_onboarding(auth.clone())));
                }
            }
            AuthState::SignedOut => {
                if !interactive {
                    return Err(EngineError::Other(
                        "not signed in — run `comet login` on this machine first".into(),
                    ));
                }
                if stdin_reader.is_none() {
                    let url = auth.start_headless_sign_in();
                    println!("Sign in to Comet:\n\n  {url}\n");
                    println!("Then paste the code shown in the browser here and press enter.");
                    let auth = auth.clone();
                    stdin_reader = Some(tokio::spawn(async move {
                        loop {
                            let Some(line) = read_stdin_line().await else {
                                return;
                            };
                            let pasted = line.trim();
                            if pasted.is_empty() {
                                continue;
                            }
                            match auth.complete_sign_in(pasted).await {
                                Ok(()) => return,
                                Err(err) => println!("Sign-in failed: {err}"),
                            }
                        }
                    }));
                }
            }
        }
        if state_rx.changed().await.is_err() {
            break;
        }
    }
    if let Some(reader) = stdin_reader {
        reader.abort();
    }
    if let Some(reader) = org_reader {
        reader.abort();
    }
    Ok(())
}

/// One line from stdin (blocking read off the runtime). `None` = stdin closed.
async fn read_stdin_line() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None, // EOF / error
            Ok(_) => Some(line),
        }
    })
    .await
    .ok()
    .flatten()
}

/// TTY workspace onboarding for an org-less session (ports old comet's
/// `backend login` flow): no memberships → prompt a name and create; exactly
/// one → auto-join; several → numbered picker. Success flips the auth state to
/// `SignedIn`, which ends [`wait_for_sign_in`]'s wait (and aborts this task).
async fn run_org_onboarding(auth: Auth) {
    let orgs = match auth.list_orgs().await {
        Ok(orgs) => orgs,
        Err(err) => {
            println!(
                "Could not list workspaces ({err}) — create or select one from the Comet UI to continue."
            );
            return;
        }
    };
    match orgs.len() {
        0 => {
            println!("No workspaces yet — name your new workspace and press enter:");
            loop {
                let Some(line) = read_stdin_line().await else {
                    return;
                };
                let name = line.trim();
                if name.is_empty() {
                    continue;
                }
                match auth.create_org(name).await {
                    Ok(()) => return,
                    Err(err) => println!("Creating workspace failed: {err}"),
                }
            }
        }
        1 => {
            let only = &orgs[0];
            println!("Joining workspace \"{}\"…", only.name);
            if let Err(err) = auth.select_org(&only.organization_id).await {
                println!("Joining workspace failed: {err}");
            }
        }
        _ => {
            println!("\nYour workspaces:");
            for (index, org) in orgs.iter().enumerate() {
                println!("  {}. {}", index + 1, org.name);
            }
            println!("Pick a workspace [1-{}]:", orgs.len());
            loop {
                let Some(line) = read_stdin_line().await else {
                    return;
                };
                let choice = line
                    .trim()
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .and_then(|index| orgs.get(index));
                let Some(org) = choice else {
                    println!("Pick a workspace [1-{}]:", orgs.len());
                    continue;
                };
                match auth.select_org(&org.organization_id).await {
                    Ok(()) => return,
                    Err(err) => println!("Joining workspace failed: {err}"),
                }
            }
        }
    }
}

/// Best-effort human name for this device's registry row (hostname).
fn local_device_name() -> String {
    std::env::var("COMET_DEVICE_NAME")
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-device".to_string())
}

/// Trimmed env var or the given default.
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Filesystem-safe form of an org/user id (path segments for `orgs/{org}/{user}/`).
fn sanitize_path_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Stable per-installation device id, persisted at `{data_dir}/device-id`.
fn load_or_create_device_id(data_dir: &Path) -> Result<String, EngineError> {
    let path = data_dir.join("device-id");
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => Ok(id.trim().to_string()),
        Ok(_) | Err(_) => {
            let id = new_id();
            std::fs::write(&path, &id)?;
            Ok(id)
        }
    }
}
