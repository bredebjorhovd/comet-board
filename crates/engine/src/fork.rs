//! Forking a conversation from any message (gh#425): the host-side half.
//!
//! One call does the whole thing — checkout, chat, lineage, carried context,
//! optional first turn — because the pieces are not independently useful. An
//! isolated fork that cut a worktree and then failed to create the chat has
//! leaked a branch and a directory nobody will ever look for, so the failure
//! path here reclaims exactly what this call created and nothing else.
//!
//! Two decisions are made here rather than by the caller, and both are the
//! reason this is not a sequence of existing RPCs:
//!
//! 1. **Why the transcript is copied** ([`comet_proto::ForkContext`]). A
//!    provider resume id owns one mutable session rather than cloning it, so a
//!    fork always starts an independent session with a bounded visible copy.
//! 2. **What a fork inherits from a board attempt.** Nothing that could let it
//!    act as one: no push credential, no git author, no turn guardrails. A fork
//!    is a chat a person opened, and the board's own provenance stays with the
//!    attempt it belongs to (see [`crate::board_runtime`], where a fork is also
//!    excluded from the pull-request authorship sweep).

use std::io::Write;
use std::path::{Path, PathBuf};

use comet_doc::{MessageStatus, SessionCommandPayload, build_handoff};
use comet_proto::{
    ChatConfig, ChatLineage, ForkCheckout, ForkContext, ForkRequest, ForkResult, HarnessId,
    RunRequest, SandboxLevel,
};

use crate::doc_host::DocHost;
use crate::repos::Repos;
use crate::workspace_host::WorkspaceHost;
use crate::{EngineError, new_id, now_ms};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForkIntent {
    chat_id: String,
    #[serde(default)]
    repo: Option<PathBuf>,
    #[serde(default)]
    worktree: Option<PathBuf>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    committed: bool,
}

/// Immutable execution authority for a fork. Unlike the synced chat row this
/// file is host-local and cannot be widened by another viewport.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForkAuthority {
    pub chat_id: String,
    pub device_id: String,
    pub space_id: String,
    pub cwd: String,
    pub checkout_id: String,
    pub config: ChatConfig,
    pub lineage: ChatLineage,
}

impl ForkAuthority {
    pub(crate) fn matches(&self, row: &comet_proto::Chat) -> bool {
        row.id == self.chat_id
            && row.device_id == self.device_id
            && row.space_id.as_deref() == Some(self.space_id.as_str())
            && row.cwd.as_deref() == Some(self.cwd.as_str())
            && row.checkout_id.as_deref() == Some(self.checkout_id.as_str())
            && row.config.as_ref() == Some(&self.config)
            && row.forked_from.as_ref() == Some(&self.lineage)
    }
}

/// The visible transcript a fork carries, held on the host until the fork's
/// first run picks it up (gh#425).
///
/// On disk rather than in memory because the two events are unrelated in time:
/// a fork is made now and its first prompt may be typed tomorrow, or after a
/// restart, or by somebody on another device. One file per chat, deleted the
/// moment it is consumed — a handoff is for the first turn and no other.
#[derive(Debug, Clone)]
pub struct ForkHandoff {
    dir: PathBuf,
}

impl ForkHandoff {
    /// Open (creating) the handoff directory.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, EngineError> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn safe_id(chat_id: &str) -> String {
        chat_id
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect()
    }

    fn path(&self, chat_id: &str) -> PathBuf {
        // Chat ids are uuids, but this builds a filesystem path out of a value
        // that arrives over the wire: anything that is not id-shaped is
        // flattened rather than trusted.
        let safe = Self::safe_id(chat_id);
        self.dir.join(format!("{safe}.md"))
    }

    fn intent_path(&self, chat_id: &str) -> PathBuf {
        self.dir
            .join(format!("{}.fork.json", Self::safe_id(chat_id)))
    }

    fn authority_path(&self, chat_id: &str) -> PathBuf {
        self.dir
            .join(format!("{}.authority.json", Self::safe_id(chat_id)))
    }

    pub(crate) fn write_authority(&self, authority: &ForkAuthority) -> Result<(), EngineError> {
        let bytes = serde_json::to_vec(authority)
            .map_err(|error| EngineError::Other(format!("encode fork authority: {error}")))?;
        write_durable(&self.authority_path(&authority.chat_id), &bytes)
    }

    pub(crate) fn authority(&self, chat_id: &str) -> Result<Option<ForkAuthority>, EngineError> {
        match std::fs::read(self.authority_path(chat_id)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| EngineError::Other(format!("decode fork authority: {error}"))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// A fork is runnable only after its creation intent has been committed.
    /// A missing intent means finalization already durably removed it.
    pub(crate) fn require_committed(&self, chat_id: &str) -> Result<(), EngineError> {
        match std::fs::read(self.intent_path(chat_id)) {
            Ok(bytes) => {
                let intent: ForkIntent = serde_json::from_slice(&bytes)
                    .map_err(|error| EngineError::Other(format!("decode fork intent: {error}")))?;
                if intent.chat_id != chat_id || !intent.committed {
                    return Err(EngineError::Other(
                        "fork creation has not durably committed".into(),
                    ));
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn is_staged(&self, chat_id: &str) -> Result<bool, EngineError> {
        match std::fs::read(self.intent_path(chat_id)) {
            Ok(bytes) => {
                let intent: ForkIntent = serde_json::from_slice(&bytes)
                    .map_err(|error| EngineError::Other(format!("decode fork intent: {error}")))?;
                Ok(intent.chat_id == chat_id && !intent.committed)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn clear_authority(&self, chat_id: &str) -> Result<(), EngineError> {
        match std::fs::remove_file(self.authority_path(chat_id)) {
            Ok(()) => sync_dir(&self.dir),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn clear_fork_state(&self, chat_id: &str) -> Result<(), EngineError> {
        self.clear_checked(chat_id)?;
        self.clear_authority(chat_id)?;
        self.clear_intent(chat_id)
    }

    pub(crate) fn authorities(&self) -> Result<Vec<ForkAuthority>, EngineError> {
        let mut authorities = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .ends_with(".authority.json")
            {
                authorities.push(
                    serde_json::from_slice(&std::fs::read(entry.path())?).map_err(|error| {
                        EngineError::Other(format!("decode fork authority: {error}"))
                    })?,
                );
            }
        }
        Ok(authorities)
    }

    fn write_intent(&self, intent: &ForkIntent) -> Result<(), EngineError> {
        let path = self.intent_path(&intent.chat_id);
        let bytes = serde_json::to_vec(intent)
            .map_err(|error| EngineError::Other(format!("encode fork intent: {error}")))?;
        write_durable(&path, &bytes)
    }

    fn clear_intent(&self, chat_id: &str) -> Result<(), EngineError> {
        if cfg!(debug_assertions)
            && std::env::var("COMET_TEST_FORK_INTENT_UNLINK_FAIL")
                .ok()
                .is_some_and(|value| value == chat_id || value == "*")
        {
            return Err(EngineError::Other(
                "injected committed-intent unlink failure".into(),
            ));
        }
        match std::fs::remove_file(self.intent_path(chat_id)) {
            Ok(()) => sync_dir(&self.dir),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn intents(&self) -> Result<Vec<ForkIntent>, EngineError> {
        let mut intents = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().ends_with(".fork.json") {
                intents.push(
                    serde_json::from_slice(&std::fs::read(entry.path())?).map_err(|error| {
                        EngineError::Other(format!("decode fork intent: {error}"))
                    })?,
                );
            }
        }
        Ok(intents)
    }

    pub async fn recover_pending(
        &self,
        workspace: &WorkspaceHost,
        doc_host: &DocHost,
        repos: &Repos,
    ) -> Result<usize, EngineError> {
        let mut recovered = 0;
        for intent in self.intents()? {
            if !intent.committed {
                let _ = workspace.delete_chat(&intent.chat_id)?;
                doc_host.purge_chat_checked(&intent.chat_id)?;
                self.clear_checked(&intent.chat_id)?;
                self.clear_authority(&intent.chat_id)?;
                if let (Some(repo), Some(path), Some(branch)) =
                    (&intent.repo, &intent.worktree, intent.branch.as_deref())
                {
                    repos.delete_worktree(repo, path, Some(branch)).await?;
                }
                workspace.flush_checked()?;
                recovered += 1;
            }
            self.clear_intent(&intent.chat_id)?;
        }
        Ok(recovered)
    }

    pub fn write(&self, chat_id: &str, text: &str) -> Result<(), EngineError> {
        write_durable(&self.path(chat_id), text.as_bytes())
    }

    /// Read without consuming. Consumption happens only after SessionStarted
    /// is durably journaled, so a rejected spawn or crash cannot lose context.
    pub fn peek(&self, chat_id: &str) -> Option<String> {
        let path = self.path(chat_id);
        std::fs::read_to_string(&path)
            .ok()
            .filter(|t| !t.trim().is_empty())
    }

    /// Drop a handoff nobody will consume (the fork's chat was deleted, or its
    /// creation failed after the file was written).
    pub fn clear(&self, chat_id: &str) {
        if let Err(error) = self.clear_checked(chat_id) {
            tracing::warn!(chat = %chat_id, %error, "fork handoff cleanup failed");
        }
    }

    pub fn clear_checked(&self, chat_id: &str) -> Result<(), EngineError> {
        match std::fs::remove_file(self.path(chat_id)) {
            Ok(()) => sync_dir(&self.dir),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

fn write_durable(path: &Path, bytes: &[u8]) -> Result<(), EngineError> {
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    sync_dir(
        path.parent().ok_or_else(|| {
            EngineError::Other(format!("{} has no parent directory", path.display()))
        })?,
    )
}

fn sync_dir(path: &Path) -> Result<(), EngineError> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(())
}

/// Everything [`fork_chat`] needs from the engine, borrowed for the call.
pub struct ForkDeps<'a> {
    pub workspace: &'a WorkspaceHost,
    pub doc_host: &'a DocHost,
    pub repos: &'a Repos,
    /// Where the carried transcript waits for the fork's first run. `None`
    /// disables copy-mode forks rather than silently making context-free ones.
    pub handoff: Option<&'a ForkHandoff>,
    /// Harness for a source chat whose row carries no config at all.
    pub default_harness: HarnessId,
}

/// Fork `request.chat_id` at `request.message_id`.
///
/// Refuses (rather than half-succeeds) when: this device does not host the
/// source chat; the fork point is not a completed message in it; an isolated
/// fork is asked for over a folder that is not a git work tree.
pub async fn fork_chat(
    deps: ForkDeps<'_>,
    request: &ForkRequest,
) -> Result<ForkResult, EngineError> {
    let source = deps
        .workspace
        .doc()
        .chat(&request.chat_id)?
        .ok_or_else(|| EngineError::Other(format!("no such chat: {}", request.chat_id)))?;
    if source.device_id != deps.workspace.device_id() {
        return Err(EngineError::Other(format!(
            "chat {} is hosted by {} — fork it there",
            request.chat_id, source.device_id
        )));
    }
    let space_id = source.space_id.clone().ok_or_else(|| {
        EngineError::Other("the source chat is not in a space; nothing to fork it into".into())
    })?;
    let synced_cwd = source
        .cwd
        .clone()
        .filter(|cwd| !cwd.trim().is_empty())
        .ok_or_else(|| EngineError::Other("the source chat has no working directory".into()))?;
    let space = deps
        .workspace
        .doc()
        .space(&space_id)?
        .ok_or_else(|| EngineError::Other(format!("no such space: {space_id}")))?;
    let source_cwd = deps
        .repos
        .validated_checkout_root(&space.path, &synced_cwd)
        .await
        .ok_or_else(|| {
            EngineError::Other(
                "the source checkout is not registered on this host; refusing its synced cwd"
                    .into(),
            )
        })?
        .to_string_lossy()
        .to_string();

    // The transcript, joined the way every viewport reads it: a fork point a
    // reader can see is a fork point this can be asked for.
    let entries = comet_doc::join_continuation_entries(
        deps.doc_host.open(&request.chat_id)?.doc().read_entries()?,
    );
    let fork_point = match request.message_id.as_deref() {
        Some(id) => {
            let entry = entries
                .iter()
                .find(|e| e.id == id)
                .ok_or_else(|| EngineError::Other(format!("no message {id} in this chat")))?;
            if entry.status != Some(MessageStatus::Complete)
                || !matches!(
                    entry.role,
                    comet_doc::MessageRole::User | comet_doc::MessageRole::Assistant
                )
            {
                return Err(EngineError::Other(
                    "only a completed user or assistant message can be forked".into(),
                ));
            }
            Some(id.to_string())
        }
        None => {
            let entry = entries.last().ok_or_else(|| {
                EngineError::Other("an empty transcript has no forkable tail".into())
            })?;
            if entry.status != Some(MessageStatus::Complete)
                || !matches!(
                    entry.role,
                    comet_doc::MessageRole::User | comet_doc::MessageRole::Assistant
                )
            {
                return Err(EngineError::Other(
                    "the transcript tail is not a completed user or assistant message".into(),
                ));
            }
            None
        }
    };
    // "The tail" is about the conversation, not about the id: a fork asked for
    // at the last message is the same fork as one asked for with no message at
    // all, and gets the tail-specific copy reason.
    let at_tail = match (&fork_point, entries.last()) {
        (None, _) => true,
        (Some(id), Some(last)) => &last.id == id,
        (Some(_), None) => false,
    };

    let source_config = source.config.clone();
    let source_harness = source_config
        .as_ref()
        .map(|c| c.harness)
        .unwrap_or(deps.default_harness);
    let harness = request.harness.unwrap_or(source_harness);
    let same_harness = harness == source_harness;

    let transaction = deps.handoff.ok_or_else(|| {
        EngineError::Other("this engine has no durable fork transaction store".into())
    })?;
    let chat_id = new_id();
    let mut intent = ForkIntent {
        chat_id: chat_id.clone(),
        repo: None,
        worktree: None,
        branch: None,
        committed: false,
    };
    transaction.write_intent(&intent)?;

    // ── the checkout ────────────────────────────────────────────────────────
    let mut cut: Option<CutWorktree> = None;
    let (cwd, branch) = match request.checkout {
        ForkCheckout::Shared => (source_cwd.clone(), source.branch.clone()),
        ForkCheckout::Isolated => {
            let source_path = Path::new(&source_cwd);
            if !deps.repos.is_repo(source_path).await {
                transaction.clear_intent(&chat_id)?;
                return Err(EngineError::Other(format!(
                    "{source_cwd} is not a git work tree — an isolated fork needs one to cut a \
                     worktree from"
                )));
            }
            let repo_root = match deps.repos.repo_root(source_path).await {
                Ok(root) => root,
                Err(error) => {
                    transaction.clear_intent(&chat_id)?;
                    return Err(error);
                }
            };
            let head = match deps.repos.head_sha(source_path).await {
                Ok(head) => head,
                Err(error) => {
                    transaction.clear_intent(&chat_id)?;
                    return Err(error);
                }
            };
            let (path, planned_branch, name) = match deps.repos.plan_worktree(&repo_root).await {
                Ok(plan) => plan,
                Err(error) => {
                    transaction.clear_intent(&chat_id)?;
                    return Err(error);
                }
            };
            intent.repo = Some(repo_root.clone());
            intent.worktree = Some(path.clone());
            intent.branch = Some(planned_branch.clone());
            if let Err(error) = transaction.write_intent(&intent) {
                deps.repos.release_planned_worktree(&path);
                return Err(error);
            }
            cut = Some(CutWorktree {
                repo: repo_root.clone(),
                path: path.clone(),
                branch: planned_branch.clone(),
            });
            let worktree = match deps
                .repos
                .create_planned_worktree(&repo_root, &head, path, planned_branch, name)
                .await
            {
                Ok(worktree) => worktree,
                Err(error) => {
                    return rollback_fork(&deps, &chat_id, cut.as_ref(), transaction, error).await;
                }
            };
            let cwd = worktree.path.clone();
            let branch = worktree.branch.clone();
            (cwd, Some(branch))
        }
    };

    // Everything from here can fail, and everything from here is reclaimable
    // — so it runs in one place with one cleanup (see `reclaim` below).
    let result = assemble_fork(
        &deps,
        request,
        AssembleInput {
            source: &source,
            chat_id: &chat_id,
            space_id: &space_id,
            source_cwd: &source_cwd,
            cwd: &cwd,
            branch: branch.clone(),
            harness,
            same_harness,
            at_tail,
            fork_point: fork_point.clone(),
            entries: &entries,
        },
    )
    .await;

    match result {
        Ok(result) => {
            if let Err(error) = precommit_test_gate(&chat_id).await {
                return rollback_fork(&deps, &chat_id, cut.as_ref(), transaction, error).await;
            }
            let authority = match transaction.authority(&chat_id) {
                Ok(Some(authority)) => authority,
                Ok(None) => {
                    return rollback_fork(
                        &deps,
                        &chat_id,
                        cut.as_ref(),
                        transaction,
                        EngineError::Other("staged fork authority disappeared".into()),
                    )
                    .await;
                }
                Err(error) => {
                    return rollback_fork(&deps, &chat_id, cut.as_ref(), transaction, error).await;
                }
            };
            let finalized = deps.workspace.commit_host_owned_chat(
                &request.chat_id,
                deps.workspace.device_id(),
                &chat_id,
                |row| {
                    if authority.matches(row) {
                        Ok(())
                    } else {
                        Err(EngineError::Other(
                            "fork row changed before its creation committed".into(),
                        ))
                    }
                },
                || {
                    deps.doc_host.persist_chat_snapshot(&chat_id)?;
                    intent.committed = true;
                    transaction.write_intent(&intent)
                },
            );
            match finalized {
                Ok(()) => {
                    // committed=true is the point of no return. Failure to
                    // unlink is recoverable housekeeping which startup will
                    // retry; destructive rollback would contradict the
                    // durable committed record startup is required to keep.
                    if let Err(error) = transaction.clear_intent(&chat_id) {
                        tracing::warn!(chat = %chat_id, %error, "committed fork intent cleanup deferred to restart");
                    }
                    if result.prompted {
                        match deps.doc_host.open(&chat_id) {
                            Ok(handle) => deps.doc_host.drain_commands(&handle).await,
                            Err(error) => tracing::warn!(chat = %chat_id, %error,
                                "committed first Run remains durable for restart"),
                        }
                    }
                    Ok(result)
                }
                Err(error) => {
                    rollback_fork(&deps, &chat_id, cut.as_ref(), transaction, error).await
                }
            }
        }
        Err(error) => rollback_fork(&deps, &chat_id, cut.as_ref(), transaction, error).await,
    }
}

async fn precommit_test_gate(chat_id: &str) -> Result<(), EngineError> {
    if !cfg!(debug_assertions) {
        return Ok(());
    }
    let Some(root) = std::env::var_os("COMET_TEST_FORK_PRECOMMIT_GATE") else {
        return Ok(());
    };
    let root = PathBuf::from(root);
    std::fs::create_dir_all(&root)?;
    write_durable(&root.join(format!("{chat_id}.reached")), b"reached")?;
    let release = root.join(format!("{chat_id}.release"));
    while !release.exists() {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    if root.join(format!("{chat_id}.fail")).exists() {
        return Err(EngineError::Other("injected precommit failure".into()));
    }
    Ok(())
}

async fn rollback_fork(
    deps: &ForkDeps<'_>,
    chat_id: &str,
    cut: Option<&CutWorktree>,
    transaction: &ForkHandoff,
    original: EngineError,
) -> Result<ForkResult, EngineError> {
    if cfg!(debug_assertions)
        && std::env::var("COMET_TEST_FORK_ROLLBACK_FAIL")
            .ok()
            .is_some_and(|value| value == chat_id || value == "*")
    {
        return Err(EngineError::Other(format!(
            "{original}; injected rollback failure"
        )));
    }
    let mut failures = Vec::new();
    if let Err(error) = deps.workspace.delete_chat(chat_id) {
        failures.push(format!("workspace chat: {error}"));
    }
    if let Err(error) = deps.doc_host.purge_chat_checked(chat_id) {
        failures.push(format!("session snapshot: {error}"));
    }
    if let Err(error) = transaction.clear_checked(chat_id) {
        failures.push(format!("handoff: {error}"));
    }
    if let Err(error) = transaction.clear_authority(chat_id) {
        failures.push(format!("fork authority: {error}"));
    }
    if let Some(cut) = cut
        && let Err(error) = reclaim(deps.repos, cut).await
    {
        failures.push(format!("worktree: {error}"));
    }
    if let Err(error) = deps.workspace.flush_checked() {
        failures.push(format!("workspace snapshot: {error}"));
    }
    if failures.is_empty()
        && let Err(error) = transaction.clear_intent(chat_id)
    {
        failures.push(format!("intent acknowledgement: {error}"));
    }
    if failures.is_empty() {
        Err(original)
    } else {
        Err(EngineError::Other(format!(
            "{original}; fork rollback also failed: {}",
            failures.join("; ")
        )))
    }
}

/// A worktree this call created — the only thing a failed fork may delete.
struct CutWorktree {
    repo: PathBuf,
    path: PathBuf,
    branch: String,
}

async fn reclaim(repos: &Repos, cut: &CutWorktree) -> Result<(), EngineError> {
    match repos
        .delete_worktree(&cut.repo, &cut.path, Some(&cut.branch))
        .await
    {
        Ok(()) => {
            tracing::info!(
            worktree = %cut.path.display(),
            branch = %cut.branch,
            "fork failed; its worktree was reclaimed"
            );
            Ok(())
        }
        // Said loudly rather than swallowed: an unreclaimed worktree is a
        // directory and a branch somebody now has to find by hand.
        Err(err) => Err(err),
    }
}

struct AssembleInput<'a> {
    source: &'a comet_proto::Chat,
    chat_id: &'a str,
    space_id: &'a str,
    source_cwd: &'a str,
    cwd: &'a str,
    branch: Option<String>,
    harness: HarnessId,
    same_harness: bool,
    at_tail: bool,
    fork_point: Option<String>,
    entries: &'a [comet_doc::SessionMessageEntry],
}

async fn assemble_fork(
    deps: &ForkDeps<'_>,
    request: &ForkRequest,
    input: AssembleInput<'_>,
) -> Result<ForkResult, EngineError> {
    let source = input.source;
    let inherited = input.same_harness.then(|| source.config.clone()).flatten();

    // Why the destination could not resume, or that it could. Every branch is
    // a fact about the source chat a reader could check for themselves.
    let session = source
        .harness_session_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let session_cwd_matches = source
        .harness_session_cwd
        .as_deref()
        .map(str::trim)
        .is_none_or(|cwd| {
            cwd.is_empty()
                || (std::fs::canonicalize(cwd).ok() == std::fs::canonicalize(input.cwd).ok())
        });
    let (context, copy_reason) = comet_proto::decide_context(comet_proto::ForkPoint {
        at_tail: input.at_tail,
        same_harness: input.same_harness,
        same_checkout: input.cwd == input.source_cwd && session_cwd_matches,
        session_id: session,
    });

    let config = ChatConfig {
        harness: input.harness,
        model: request
            .model
            .clone()
            .or_else(|| inherited.as_ref().and_then(|c| c.model.clone())),
        reasoning: inherited.as_ref().and_then(|c| c.reasoning),
        model_options: inherited
            .as_ref()
            .map(|c| c.model_options.clone())
            .unwrap_or_default(),
        sandbox: match request.checkout {
            ForkCheckout::Shared => SandboxLevel::ReadOnly,
            ForkCheckout::Isolated => source
                .config
                .as_ref()
                .map(|c| c.sandbox)
                .unwrap_or(SandboxLevel::WorkspaceWrite),
        },
        // A login is per-harness, so a fork onto a different one starts on the
        // device's own CLI login rather than on a slot that cannot serve it.
        account: request.account.clone(),
        // The three fields a board dispatch sets, and the three a fork must
        // not inherit: the credential its pushes authenticate with, the person
        // its commits are attributed to, and the guardrails an unattended
        // attempt runs under. A fork was opened by a human, in a chat the board
        // never dispatched; giving it the board's GitHub App credential would
        // let a "second opinion" push to the branch under review.
        push_repo: None,
        push_contract: None,
        git_author: None,
        turn_limits: Default::default(),
        mcp_servers: Vec::new(),
    };

    let chat_id = input.chat_id.to_string();

    let checkout_id = deps.repos.checkout_identity(Path::new(input.cwd)).await?.id;

    // Every fork carries a transcript copy. A provider resume id names one
    // mutable conversation; attaching two chats to it is not a clone.
    let (carried, truncated, truncation) = match context {
        ForkContext::NativeResume => unreachable!("fork context never shares a provider session"),
        ForkContext::TranscriptCopy => {
            let handoff_store = deps.handoff.ok_or_else(|| {
                EngineError::Other(
                    "this engine has no handoff store, so it cannot carry a transcript".into(),
                )
            })?;
            let handoff = build_handoff(
                input.entries,
                (!input.at_tail)
                    .then_some(input.fork_point.as_deref())
                    .flatten(),
                source.title.as_deref(),
                comet_doc::HANDOFF_BUDGET_BYTES,
            );
            let carried = Some(handoff.carried);
            let truncated = handoff.truncated;
            let truncation = handoff.truncation;
            // Written where the fork's first run will pick it up…
            handoff_store.write(&chat_id, &handoff.text)?;
            // …and into the transcript, so what the agent was handed is what
            // the reader sees. A copy nobody can read is a copy nobody can
            // check.
            deps.doc_host.open(&chat_id)?.write_system_message(
                &format!("{chat_id}-handoff"),
                &handoff.text,
                now_ms(),
            )?;
            (carried, truncated, truncation)
        }
    };

    let prompt = request
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty());
    if let Some(prompt) = prompt
        && let Some(handoff) = deps.handoff.and_then(|store| store.peek(&chat_id))
    {
        crate::sessions::combine_fork_payload(&handoff, prompt)?;
    }

    let lineage = ChatLineage {
        source_chat_id: request.chat_id.clone(),
        source_message_id: input.fork_point.clone(),
        checkout: request.checkout,
        context,
        copy_reason,
        carried_messages: carried,
        truncated,
        truncation,
        forked_at: chrono::Utc::now(),
    };
    let row = comet_proto::Chat {
        id: chat_id.clone(),
        device_id: deps.workspace.device_id().to_string(),
        title: Some(fork_title(
            request.title.as_deref(),
            source.title.as_deref(),
        )),
        archived: false,
        cwd: Some(input.cwd.to_string()),
        branch: input.branch.clone(),
        checkout_id: Some(checkout_id.clone()),
        config: Some(config.clone()),
        creation_state: comet_proto::ChatCreationState::Ready,
        last_message_preview: None,
        last_message_at: None,
        // Workspace chat rows store millisecond timestamps. Canonicalize
        // before signing the exact row into host authority so verification is
        // byte-for-value stable after the Loro round trip.
        created_at: chrono::DateTime::from_timestamp_millis(now_ms())
            .expect("current timestamp is representable"),
        harness_session_id: None,
        harness_session_cwd: None,
        space_id: Some(input.space_id.to_string()),
        last_seen_at: None,
        forked_from: Some(lineage.clone()),
    };
    let authority = ForkAuthority {
        chat_id: chat_id.clone(),
        device_id: deps.workspace.device_id().to_string(),
        space_id: input.space_id.to_string(),
        cwd: input.cwd.to_string(),
        checkout_id,
        config: config.clone(),
        lineage: lineage.clone(),
    };
    let transaction = deps
        .handoff
        .ok_or_else(|| EngineError::Other("missing fork authority store".into()))?;
    // Authority precedes publication. Once the row can be observed, every Run
    // already has a fail-closed host snapshot to resolve against.
    transaction.write_authority(&authority)?;

    // The first turn, if one was asked for. Last, so a fork whose prompt could
    // not be queued is still a fork somebody can type into rather than a
    // half-made chat.
    if let Some(prompt) = prompt {
        deps.doc_host.queue_command_with_id_durable(
            &chat_id,
            &new_id(),
            SessionCommandPayload::Run {
                request: RunRequest {
                    prompt: prompt.to_string(),
                    model: config.model.clone(),
                    reasoning: config.reasoning,
                    model_options: config.model_options.clone(),
                    cwd: input.cwd.to_string(),
                    sandbox: config.sandbox,
                    auto_approve: false,
                    // Resume continuity is engine-owned: the row this call just
                    // wrote is what the dispatch reads.
                    resume: None,
                    attachments: Vec::new(),
                },
                message_id: new_id(),
            },
        )?;
    }

    // The exact row appears in one commit only after authority, handoff,
    // lineage and optional command are durable. The workspace owner gate also
    // makes a shallow reseed replay this row rather than erase half of it.
    deps.workspace.publish_host_owned_chat(&row)?;

    Ok(ForkResult {
        chat_id,
        cwd: input.cwd.to_string(),
        branch: input.branch,
        lineage,
        prompted: prompt.is_some(),
    })
}

/// The fork's name. Derived from the source's so the sidebar reads as a pair,
/// and never longer than a row can show.
fn fork_title(requested: Option<&str>, source: Option<&str>) -> String {
    if let Some(title) = requested.map(str::trim).filter(|t| !t.is_empty()) {
        return truncate_title(title);
    }
    match source.map(str::trim).filter(|t| !t.is_empty()) {
        Some(source) => truncate_title(&format!("{source} (fork)")),
        None => "Fork".to_string(),
    }
}

fn truncate_title(title: &str) -> String {
    const MAX: usize = 80;
    if title.chars().count() <= MAX {
        return title.to_string();
    }
    let head: String = title.chars().take(MAX - 1).collect();
    format!("{}…", head.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fork_is_named_after_what_it_came_from() {
        assert_eq!(
            fork_title(None, Some("Fix the parser")),
            "Fix the parser (fork)"
        );
        assert_eq!(fork_title(Some(" Review "), Some("Fix")), "Review");
        assert_eq!(fork_title(None, None), "Fork");
        assert_eq!(fork_title(Some("  "), None), "Fork");
        assert!(fork_title(None, Some(&"x".repeat(200))).chars().count() <= 80);
    }

    #[test]
    fn a_handoff_prefixes_exactly_one_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let store = ForkHandoff::open(dir.path().join("handoff")).unwrap();
        store.write("chat-1", "carried context").unwrap();
        assert_eq!(store.peek("chat-1").as_deref(), Some("carried context"));
        assert_eq!(store.peek("chat-1").as_deref(), Some("carried context"));
        store.clear("chat-1");
        assert_eq!(store.peek("chat-1"), None, "explicitly acknowledged");
    }

    #[test]
    fn handoff_and_creation_intent_survive_a_reseed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("handoff");
        let first = ForkHandoff::open(&root).unwrap();
        first.write("chat-1", "carried context").unwrap();
        first
            .write_intent(&ForkIntent {
                chat_id: "chat-1".into(),
                repo: Some(PathBuf::from("/repo")),
                worktree: Some(PathBuf::from("/repo-worktree")),
                branch: Some("comet/fork".into()),
                committed: false,
            })
            .unwrap();
        drop(first);

        let reseeded = ForkHandoff::open(&root).unwrap();
        assert_eq!(reseeded.peek("chat-1").as_deref(), Some("carried context"));
        let intents = reseeded.intents().unwrap();
        assert_eq!(intents.len(), 1);
        assert_eq!(
            intents[0].worktree.as_deref(),
            Some(Path::new("/repo-worktree"))
        );
        assert!(!intents[0].committed);
    }

    #[test]
    fn a_staged_fork_cannot_run_until_its_intent_is_committed() {
        let dir = tempfile::tempdir().unwrap();
        let store = ForkHandoff::open(dir.path()).unwrap();
        let mut intent = ForkIntent {
            chat_id: "chat-staged".into(),
            repo: None,
            worktree: None,
            branch: None,
            committed: false,
        };
        store.write_intent(&intent).unwrap();
        let error = store.require_committed("chat-staged").unwrap_err();
        assert!(error.to_string().contains("not durably committed"));

        intent.committed = true;
        store.write_intent(&intent).unwrap();
        store.require_committed("chat-staged").unwrap();
        store.clear_intent("chat-staged").unwrap();
        store.require_committed("chat-staged").unwrap();
    }

    #[test]
    fn concurrent_intent_cleanup_removes_only_the_failed_fork() {
        let dir = tempfile::tempdir().unwrap();
        let store = ForkHandoff::open(dir.path()).unwrap();
        for chat_id in ["fork-a", "fork-b"] {
            store
                .write_intent(&ForkIntent {
                    chat_id: chat_id.into(),
                    repo: None,
                    worktree: None,
                    branch: None,
                    committed: false,
                })
                .unwrap();
        }
        store.clear_intent("fork-a").unwrap();
        let remaining = store.intents().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].chat_id, "fork-b");
    }

    /// A chat id is a wire value; it must not be able to name a path.
    #[test]
    fn a_handoff_file_is_named_by_the_chat_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let store = ForkHandoff::open(dir.path()).unwrap();
        let path = store.path("../../etc/passwd");
        assert_eq!(path.parent(), Some(dir.path()));
        assert!(!path.to_string_lossy().contains(".."));
    }
}
