//! Workspace doc schema over `loro` — the per-org entity index that replaces comet's
//! residual entity sync (ARCHITECTURE.md §2.2). Lives in its own DO room (same
//! SessionRoom class, doc id `ws/{orgId}`).
//!
//! Container layout — maps keyed by id, NOT lists: entity rows are LWW upserts, and a
//! map-of-maps means concurrent writers to *different* rows never conflict while writes
//! to the *same* row settle field-by-field LWW (exactly right for renames/archives):
//! - `devices`: LoroMap keyed by deviceId → row map {id, name, platform, lastSeenAt}
//! - `spaces`: LoroMap keyed by spaceId → row map {id, deviceId, path, name?,
//!   gitDetected, gitCheckedAt?, checkoutId?, branch?, createdAt}
//! - `chats`: LoroMap keyed by chatId → row map {id, deviceId, title?, archived, cwd?,
//!   branch?, checkoutId?, config?(json), boardPush?(json), lastMessagePreview?, lastMessageAt?, createdAt,
//!   harnessSessionId?, harnessSessionCwd?, spaceId?, lastSeenAt?, forkedFrom?(json)}
//! - `sessions`: LoroMap keyed by chatId → row map {chatId, deviceId, status, startedAt?,
//!   updatedAt}
//! - `meta`: LoroMap {schemaVersion} — in-band detection for future destructive changes
//!
//! Writer discipline (ARCHITECTURE §2.2): each device writes its own device row, its
//! own session rows, and rows for chats it hosts; title/archived renames are LWW map
//! sets from any device — matching comet's Mutate surface. Presence rides the room's
//! `EphemeralStore` under keys `presence/{deviceId}` (an online timestamp), replacing
//! comet's 15s heartbeat writes so liveness never grows the oplog. Since gh#145 those
//! entries are written by the EDGE, derived from the room's socket set: a client
//! writing them every 15s is what stopped the Durable Object ever hibernating.
//!
//! Timestamps are stored as epoch millis (the session-doc convention) and surface as
//! `chrono::DateTime<Utc>` through the `comet_proto` entity types.

use chrono::{DateTime, Utc};
use loro::{ExportMode, LoroDoc, LoroMap, LoroValue, ToJson};
use serde::{Deserialize, Serialize};

use comet_proto::{
    Chat, ChatConfig, Device, GithubPushState, GithubPushStateError, Session, SessionStatus, Space,
};

use crate::schema::DocError;

/// Workspace doc schema version. v2 = the spaces overhaul (spaces container,
/// chat spaceId/lastSeenAt) — a destructive break shipped via a fresh doc/room
/// (`workspace2` / `ws2/{orgId}`), so no v1 reader exists.
pub const WORKSPACE_SCHEMA_VERSION: i64 = 2;

/// An atomic shadow outside the replaceable config map. Remote clients edit
/// `config` as one LWW value; keeping the board-owned tuple beside it means an
/// old client cannot erase both halves and turn a dispatched chat ordinary.
const BOARD_PUSH_KEY: &str = "boardPush";

/// Ephemeral presence key for a device (`presence/{deviceId}` → online timestamp).
pub fn presence_key(device_id: &str) -> String {
    format!("presence/{device_id}")
}

/// Everything in the workspace doc, materialized (`read_all`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceState {
    pub devices: Vec<Device>,
    pub spaces: Vec<Space>,
    pub chats: Vec<Chat>,
    pub sessions: Vec<Session>,
}

/// Result of a `delete_space` cascade — the chat ids removed alongside the
/// space so the engine can drop local run state / doc-host handles.
#[derive(Debug, Clone, PartialEq)]
pub struct DeletedSpace {
    pub existed: bool,
    pub chat_ids: Vec<String>,
}

/// A workspace doc handle: typed access over a LoroDoc with the schema above.
pub struct WorkspaceDoc {
    doc: LoroDoc,
}

impl Default for WorkspaceDoc {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkspaceDoc {
    /// Fresh, empty workspace doc.
    pub fn new() -> Self {
        Self {
            doc: LoroDoc::new(),
        }
    }

    /// Wrap an existing doc (e.g. imported from a snapshot).
    pub fn from_doc(doc: LoroDoc) -> Self {
        Self { doc }
    }

    pub fn doc(&self) -> &LoroDoc {
        &self.doc
    }

    /// Export a snapshot (persistence) — `ExportMode::Snapshot`.
    pub fn export_snapshot(&self) -> Result<Vec<u8>, DocError> {
        self.doc
            .export(ExportMode::Snapshot)
            .map_err(|e| DocError::Schema(e.to_string()))
    }

    // ── devices ─────────────────────────────────────────────────────────────

    /// Upsert a full device row (writer discipline: callers pass their OWN device).
    pub fn upsert_device(&self, device: &Device) -> Result<(), DocError> {
        let row = self.row("devices", &device.id)?;
        row.insert("id", device.id.as_str())?;
        row.insert("name", device.name.as_str())?;
        row.insert("platform", device.platform.as_str())?;
        set_opt_ms(&row, "lastSeenAt", device.last_seen_at)?;
        set_opt_ms(&row, "createdAt", device.created_at)?;
        set_opt_str(&row, "version", device.version.as_deref())?;
        self.doc.commit();
        Ok(())
    }

    /// LWW rename (settings UI; any device may write). `false` when no such row.
    pub fn rename_device(&self, device_id: &str, name: &str) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("devices", device_id) else {
            return Ok(false);
        };
        row.insert("name", name)?;
        self.doc.commit();
        Ok(true)
    }

    /// Stamp `lastSeenAt` on an existing device row (boot/shutdown only — periodic
    /// liveness rides ephemeral presence, never the oplog). `false` when no such row.
    pub fn set_device_last_seen(
        &self,
        device_id: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("devices", device_id) else {
            return Ok(false);
        };
        row.insert("lastSeenAt", at.timestamp_millis())?;
        self.doc.commit();
        Ok(true)
    }

    pub fn read_devices(&self) -> Result<Vec<Device>, DocError> {
        let mut devices: Vec<Device> = self
            .read_rows::<RawDevice>("devices")?
            .into_iter()
            .map(Device::from)
            .collect();
        devices.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(devices)
    }

    // ── spaces ──────────────────────────────────────────────────────────────

    /// Upsert a full space row (creation from any device; owner-only fields are
    /// enforced one layer up, in the engine).
    pub fn upsert_space(&self, space: &Space) -> Result<(), DocError> {
        let row = self.row("spaces", &space.id)?;
        row.insert("id", space.id.as_str())?;
        row.insert("deviceId", space.device_id.as_str())?;
        row.insert("path", space.path.as_str())?;
        set_opt_str(&row, "name", space.name.as_deref())?;
        row.insert("gitDetected", space.git_detected)?;
        set_opt_ms(&row, "gitCheckedAt", space.git_checked_at)?;
        set_opt_str(&row, "checkoutId", space.checkout_id.as_deref())?;
        set_opt_str(&row, "branch", space.branch.as_deref())?;
        row.insert("createdAt", space.created_at.timestamp_millis())?;
        self.doc.commit();
        Ok(())
    }

    pub fn space(&self, space_id: &str) -> Result<Option<Space>, DocError> {
        Ok(self.read_spaces()?.into_iter().find(|s| s.id == space_id))
    }

    pub fn read_spaces(&self) -> Result<Vec<Space>, DocError> {
        let mut spaces: Vec<Space> = self
            .read_rows::<RawSpace>("spaces")?
            .into_iter()
            .map(Space::from)
            .collect();
        spaces.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(spaces)
    }

    /// LWW display-name set from any device; `None` clears back to the derived
    /// name (basename of path). `false` when no such row.
    pub fn rename_space(&self, space_id: &str, name: Option<&str>) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("spaces", space_id) else {
            return Ok(false);
        };
        set_opt_str(&row, "name", name)?;
        self.doc.commit();
        Ok(true)
    }

    /// Owner-stamped git presence for the space folder — whether it is a work
    /// tree, which checkout it is, and the branch it sits on (SpacesSync;
    /// ownership is asserted by the engine layer, this is mechanism only).
    /// `false` when no such row.
    pub fn set_space_git(
        &self,
        space_id: &str,
        detected: bool,
        checkout_id: Option<&str>,
        branch: Option<&str>,
        checked_at: DateTime<Utc>,
    ) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("spaces", space_id) else {
            return Ok(false);
        };
        row.insert("gitDetected", detected)?;
        set_opt_str(&row, "checkoutId", checkout_id)?;
        set_opt_str(&row, "branch", branch)?;
        row.insert("gitCheckedAt", checked_at.timestamp_millis())?;
        self.doc.commit();
        Ok(true)
    }

    /// Hard-delete a space and cascade to its chats: tombstone every chat row
    /// (and session-status row) whose `spaceId` matches, then the space row —
    /// one commit. Per-chat transcript docs remain (orphaned, accepted).
    /// Returns the removed chat ids so the engine can drop local state.
    pub fn delete_space(&self, space_id: &str) -> Result<DeletedSpace, DocError> {
        let spaces = self.doc.get_map("spaces");
        let existed = spaces.get(space_id).is_some();
        let chat_ids: Vec<String> = self
            .read_chats()?
            .into_iter()
            .filter(|c| c.space_id.as_deref() == Some(space_id))
            .map(|c| c.id)
            .collect();
        let chats = self.doc.get_map("chats");
        let sessions = self.doc.get_map("sessions");
        for chat_id in &chat_ids {
            chats.delete(chat_id)?;
            sessions.delete(chat_id)?;
        }
        spaces.delete(space_id)?;
        self.doc.commit();
        Ok(DeletedSpace { existed, chat_ids })
    }

    // ── chats ───────────────────────────────────────────────────────────────

    /// Upsert a full chat row (host device, or CreateChat targeting a device).
    pub fn upsert_chat(&self, chat: &Chat) -> Result<(), DocError> {
        let row = self.row("chats", &chat.id)?;
        row.insert("id", chat.id.as_str())?;
        row.insert("deviceId", chat.device_id.as_str())?;
        set_opt_str(&row, "title", chat.title.as_deref())?;
        row.insert("archived", chat.archived)?;
        set_opt_str(&row, "cwd", chat.cwd.as_deref())?;
        set_opt_str(&row, "branch", chat.branch.as_deref())?;
        set_opt_str(&row, "checkoutId", chat.checkout_id.as_deref())?;
        match &chat.config {
            Some(config) => {
                let config = reconcile_config_write(&row, config)?;
                row.insert("config", LoroValue::from(serde_json::to_value(config)?))?
            }
            None => row.delete("config")?,
        }
        row.insert(
            "creationState",
            match chat.creation_state {
                comet_proto::ChatCreationState::Ready => "ready",
                comet_proto::ChatCreationState::Draft => "draft",
            },
        )?;
        set_opt_str(
            &row,
            "lastMessagePreview",
            chat.last_message_preview.as_deref(),
        )?;
        set_opt_ms(&row, "lastMessageAt", chat.last_message_at)?;
        row.insert("createdAt", chat.created_at.timestamp_millis())?;
        // Preserved on full-row upserts (set_chat_activity/set_chat_host read →
        // modify → upsert; dropping these here would silently amnesia the chat).
        set_opt_str(&row, "harnessSessionId", chat.harness_session_id.as_deref())?;
        set_opt_str(
            &row,
            "harnessSessionCwd",
            chat.harness_session_cwd.as_deref(),
        )?;
        set_opt_str(&row, "spaceId", chat.space_id.as_deref())?;
        set_opt_ms(&row, "lastSeenAt", chat.last_seen_at)?;
        // Preserved on full-row upserts for the same reason the harness-session
        // pair above is — and one more (gh#425): a lineage is written once, at
        // creation, so a later `set_chat_host`-style read-modify-upsert that
        // dropped it would erase the only record that this chat is a fork.
        match &chat.forked_from {
            Some(lineage) => {
                row.insert("forkedFrom", LoroValue::from(serde_json::to_value(lineage)?))?
            }
            None => row.delete("forkedFrom")?,
        }
        self.doc.commit();
        Ok(())
    }

    /// Synced seen marker (LWW) with a monotonic guard: no oplog write when the
    /// stored stamp is already >= `at` (idempotence backstop — the UI also
    /// guards on "currently unseen" before calling). `false` when no such row.
    pub fn set_chat_seen(&self, chat_id: &str, at: DateTime<Utc>) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(false);
        };
        let current = match row.get("lastSeenAt") {
            Some(loro::ValueOrContainer::Value(LoroValue::I64(ms))) => Some(ms),
            _ => None,
        };
        if current.is_some_and(|ms| ms >= at.timestamp_millis()) {
            return Ok(true);
        }
        row.insert("lastSeenAt", at.timestamp_millis())?;
        self.doc.commit();
        Ok(true)
    }

    pub fn chat(&self, chat_id: &str) -> Result<Option<Chat>, DocError> {
        // Single-row read: this sits on the per-tick `is_host` path, where
        // materializing every chat row per call multiplied with chat count.
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(None);
        };
        let value = row.get_deep_value().to_json_value();
        match serde_json::from_value::<RawChat>(value) {
            Ok(raw) => Ok(Some(raw.into())),
            Err(err) => {
                tracing::warn!(row = %chat_id, error = %err, "skipping malformed chat row");
                Ok(None)
            }
        }
    }

    pub fn read_chats(&self) -> Result<Vec<Chat>, DocError> {
        let mut chats: Vec<Chat> = self
            .read_rows::<RawChat>("chats")?
            .into_iter()
            .map(Chat::from)
            .collect();
        chats.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(chats)
    }

    /// LWW title set from any device. `false` when no such row.
    pub fn rename_chat(&self, chat_id: &str, title: &str) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(false);
        };
        row.insert("title", title)?;
        self.doc.commit();
        Ok(true)
    }

    /// LWW archived flag from any device. `false` when no such row.
    pub fn set_chat_archived(&self, chat_id: &str, archived: bool) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(false);
        };
        row.insert("archived", archived)?;
        self.doc.commit();
        Ok(true)
    }

    /// Host-side git metadata: the branch checked out at the chat's cwd (HEAD
    /// watcher reconciliation). `false` when no such row.
    pub fn set_chat_branch(&self, chat_id: &str, branch: &str) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(false);
        };
        row.insert("branch", branch)?;
        self.doc.commit();
        Ok(true)
    }

    /// Retarget the chat onto another folder — the mid-session "switch to an
    /// existing worktree" move (t3code `reuseExistingWorktree`). LWW set;
    /// `false` when no such row. Harness resume is cwd-scoped, so the next
    /// run in the new folder starts a fresh harness conversation by design.
    pub fn set_chat_cwd(&self, chat_id: &str, cwd: &str) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(false);
        };
        row.insert("cwd", cwd)?;
        self.doc.commit();
        Ok(true)
    }

    /// Host-side checkout identity for the chat's cwd (diff grouping key).
    /// `false` when no such row.
    pub fn set_chat_checkout(&self, chat_id: &str, checkout_id: &str) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(false);
        };
        row.insert("checkoutId", checkout_id)?;
        self.doc.commit();
        Ok(true)
    }

    /// LWW config set. `false` when no such row.
    pub fn set_chat_config(&self, chat_id: &str, config: &ChatConfig) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(false);
        };
        let config = reconcile_config_write(&row, config)?;
        row.insert("config", LoroValue::from(serde_json::to_value(config)?))?;
        self.doc.commit();
        Ok(true)
    }

    /// The authoritative board-owned push tuple for one chat. The shadow also
    /// repairs the predecessor schema's `push_repo`-only config when its repo
    /// agrees. A different repo or any other half-state remains corruption.
    pub fn github_push_state(&self, chat_id: &str) -> Result<Option<GithubPushState>, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(None);
        };
        let value = row.get_deep_value().to_json_value();
        let config = value
            .get("config")
            .cloned()
            .map(serde_json::from_value::<ChatConfig>)
            .transpose()?;
        let configured = config
            .as_ref()
            .map(ChatConfig::github_push_state)
            .unwrap_or(Ok(None));
        let shadow = board_push_from_value(&value)?;
        match (shadow, configured) {
            (Some(shadow), Ok(Some(configured))) if shadow != configured => Err(DocError::Schema(
                "GitHub push config conflicts with the board-owned push tuple".into(),
            )),
            (Some(shadow), Err(GithubPushStateError::RepositoryWithoutContract))
                if config
                    .as_ref()
                    .and_then(|config| config.push_repo.as_deref())
                    == Some(shadow.repo.as_str()) =>
            {
                Ok(Some(shadow))
            }
            (Some(_), Err(GithubPushStateError::RepositoryWithoutContract)) => {
                Err(DocError::Schema(
                    "GitHub push config conflicts with the board-owned push tuple".into(),
                ))
            }
            (Some(shadow), Ok(_)) => Ok(Some(shadow)),
            (_, Err(error)) => Err(DocError::Schema(error.to_string())),
            (None, Ok(configured)) => Ok(configured),
        }
    }

    /// The repository on a pre-contract board chat, when no authoritative
    /// shadow exists yet. Callers must prove a replacement contract before
    /// upgrading it; this method never turns the half-state into ambient auth.
    pub fn legacy_github_push_repo(&self, chat_id: &str) -> Result<Option<String>, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(None);
        };
        let value = row.get_deep_value().to_json_value();
        if board_push_from_value(&value)?.is_some() {
            return Ok(None);
        }
        let Some(config) = value
            .get("config")
            .cloned()
            .map(serde_json::from_value::<ChatConfig>)
            .transpose()?
        else {
            return Ok(None);
        };
        match config.github_push_state() {
            Err(GithubPushStateError::RepositoryWithoutContract) => Ok(config.push_repo),
            _ => Ok(None),
        }
    }

    /// Persist a host-proven contract onto one pre-contract chat. Rechecks the
    /// exact legacy shape so a concurrent config/shadow change cannot be
    /// overwritten by a stale capability probe.
    pub fn upgrade_legacy_github_push_state(
        &self,
        chat_id: &str,
        state: &GithubPushState,
    ) -> Result<bool, DocError> {
        state
            .validate()
            .map_err(|error| DocError::Schema(error.to_string()))?;
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(false);
        };
        let value = row.get_deep_value().to_json_value();
        if board_push_from_value(&value)?.is_some() {
            return Ok(false);
        }
        let Some(mut config) = value
            .get("config")
            .cloned()
            .map(serde_json::from_value::<ChatConfig>)
            .transpose()?
        else {
            return Ok(false);
        };
        if !matches!(
            config.github_push_state(),
            Err(GithubPushStateError::RepositoryWithoutContract)
        ) || config.push_repo.as_deref() != Some(state.repo.as_str())
        {
            return Ok(false);
        }
        config.apply_github_push_state(state);
        write_board_push(&row, state)?;
        row.insert("config", LoroValue::from(serde_json::to_value(config)?))?;
        self.doc.commit();
        Ok(true)
    }

    /// Backfill the atomic shadow and repair config replacements that omitted
    /// push fields. Called on boot and after local or remote CRDT changes. A
    /// known pre-contract repo-only row is left quietly for the host credential
    /// resolver; other malformed states remain fail-closed and visible.
    pub fn reconcile_github_push_states(&self) -> Result<usize, DocError> {
        let chats = self.doc.get_map("chats");
        let ids: Vec<String> = chats.keys().map(|key| key.to_string()).collect();
        let mut repaired = 0usize;
        for chat_id in ids {
            let Some(row) = self.existing_row("chats", &chat_id) else {
                continue;
            };
            let value = row.get_deep_value().to_json_value();
            let Some(config_value) = value.get("config").cloned() else {
                continue;
            };
            let Ok(mut config) = serde_json::from_value::<ChatConfig>(config_value) else {
                continue;
            };
            let shadow = board_push_from_value(&value)?;
            let configured = config.github_push_state();
            match (shadow, configured) {
                (None, Ok(Some(state))) => {
                    write_board_push(&row, &state)?;
                    repaired += 1;
                }
                (Some(shadow), Ok(None)) => {
                    config.apply_github_push_state(&shadow);
                    row.insert("config", LoroValue::from(serde_json::to_value(config)?))?;
                    repaired += 1;
                }
                (Some(shadow), Err(GithubPushStateError::RepositoryWithoutContract))
                    if config.push_repo.as_deref() == Some(shadow.repo.as_str()) =>
                {
                    config.apply_github_push_state(&shadow);
                    row.insert("config", LoroValue::from(serde_json::to_value(config)?))?;
                    repaired += 1;
                }
                // A pre-contract row has no durable permission promise. Leave
                // it for the host credential resolver instead of warning on
                // every 15-second publish tick.
                (None, Err(GithubPushStateError::RepositoryWithoutContract)) => {}
                (Some(shadow), Ok(Some(configured))) if shadow != configured => {
                    tracing::warn!(chat = %chat_id, "refusing to reconcile conflicting GitHub push tuples");
                }
                (_, Err(error)) => {
                    tracing::warn!(chat = %chat_id, error = %error, "refusing to reconcile inconsistent GitHub push state");
                }
                _ => {}
            }
        }
        if repaired > 0 {
            self.doc.commit();
        }
        Ok(repaired)
    }

    /// Stamp where a chat was forked from (gh#425). Written once by the host
    /// that made the fork; `false` when no such row.
    ///
    /// A set and not an upsert field for the same reason the branch and the
    /// harness session are: the row is created first (so the fork exists even
    /// if what follows fails), and this lands on it a moment later.
    pub fn set_chat_forked_from(
        &self,
        chat_id: &str,
        lineage: &comet_proto::ChatLineage,
    ) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(false);
        };
        row.insert("forkedFrom", LoroValue::from(serde_json::to_value(lineage)?))?;
        self.doc.commit();
        Ok(true)
    }

    /// Host-side resume continuity: the harness-native session id of the chat's
    /// latest run and the cwd it was created under (comet stored the same pair
    /// on the chats table). An empty
    /// `session_id` is the explicit "do not resume" tombstone written after a
    /// harness rejects a resume. `false` when no such row.
    pub fn set_chat_harness_session(
        &self,
        chat_id: &str,
        session_id: &str,
        cwd: &str,
    ) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(false);
        };
        row.insert("harnessSessionId", session_id)?;
        row.insert("harnessSessionCwd", cwd)?;
        self.doc.commit();
        Ok(true)
    }

    /// Host-side sidebar freshness: preview + timestamp of the latest message.
    /// `false` when no such row.
    pub fn set_chat_last_message(
        &self,
        chat_id: &str,
        preview: &str,
        at: DateTime<Utc>,
    ) -> Result<bool, DocError> {
        let Some(row) = self.existing_row("chats", chat_id) else {
            return Ok(false);
        };
        row.insert("lastMessagePreview", preview)?;
        row.insert("lastMessageAt", at.timestamp_millis())?;
        self.doc.commit();
        Ok(true)
    }

    /// Tombstone: delete the chat row (and its session-status row). The per-chat
    /// session doc remains — DeleteChat removes the index entry, not the transcript.
    pub fn delete_chat(&self, chat_id: &str) -> Result<bool, DocError> {
        let chats = self.doc.get_map("chats");
        let existed = chats.get(chat_id).is_some();
        chats.delete(chat_id)?;
        self.doc.get_map("sessions").delete(chat_id)?;
        self.doc.commit();
        Ok(existed)
    }

    // ── sessions ────────────────────────────────────────────────────────────

    /// Upsert a session-status row (writer discipline: each device writes only its
    /// own runs' rows). Staleness is checked client-side against `updatedAt`.
    pub fn upsert_session(&self, session: &Session) -> Result<(), DocError> {
        let row = self.row("sessions", &session.chat_id)?;
        row.insert("chatId", session.chat_id.as_str())?;
        row.insert("deviceId", session.device_id.as_str())?;
        row.insert("status", status_str(session.status))?;
        set_opt_ms(&row, "startedAt", session.started_at)?;
        row.insert("updatedAt", session.updated_at.timestamp_millis())?;
        self.doc.commit();
        Ok(())
    }

    pub fn read_sessions(&self) -> Result<Vec<Session>, DocError> {
        let mut sessions: Vec<Session> = self
            .read_rows::<RawSession>("sessions")?
            .into_iter()
            .map(Session::from)
            .collect();
        sessions.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
        Ok(sessions)
    }

    // ── whole-doc read ──────────────────────────────────────────────────────

    pub fn read_all(&self) -> Result<WorkspaceState, DocError> {
        Ok(WorkspaceState {
            devices: self.read_devices()?,
            spaces: self.read_spaces()?,
            chats: self.read_chats()?,
            sessions: self.read_sessions()?,
        })
    }

    // ── meta ────────────────────────────────────────────────────────────────

    /// Stamp `meta.schemaVersion` when absent or lower (idempotent — steady
    /// state adds nothing to the oplog). Returns the version now in the doc.
    pub fn ensure_schema_version(&self) -> Result<i64, DocError> {
        let meta = self.doc.get_map("meta");
        let current = match meta.get("schemaVersion") {
            Some(loro::ValueOrContainer::Value(LoroValue::I64(v))) => Some(v),
            _ => None,
        };
        match current {
            Some(v) if v >= WORKSPACE_SCHEMA_VERSION => Ok(v),
            _ => {
                meta.insert("schemaVersion", WORKSPACE_SCHEMA_VERSION)?;
                self.doc.commit();
                Ok(WORKSPACE_SCHEMA_VERSION)
            }
        }
    }

    // ── row plumbing ────────────────────────────────────────────────────────

    /// The row map for `key`, creating it when absent.
    fn row(&self, container: &str, key: &str) -> Result<LoroMap, DocError> {
        let parent = self.doc.get_map(container);
        match parent.get(key) {
            Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => Ok(map),
            _ => Ok(parent.insert_container(key, LoroMap::new())?),
        }
    }

    /// The row map for `key`, or `None` when the row doesn't exist.
    fn existing_row(&self, container: &str, key: &str) -> Option<LoroMap> {
        match self.doc.get_map(container).get(key) {
            Some(loro::ValueOrContainer::Container(loro::Container::Map(map))) => Some(map),
            _ => None,
        }
    }

    /// All rows of a container as typed values (malformed rows are skipped with a
    /// warning rather than failing the whole read — a bad peer must not blind us).
    fn read_rows<T: serde::de::DeserializeOwned>(
        &self,
        container: &str,
    ) -> Result<Vec<T>, DocError> {
        let value = self.doc.get_map(container).get_deep_value().to_json_value();
        let serde_json::Value::Object(rows) = value else {
            return Ok(Vec::new());
        };
        let mut out = Vec::with_capacity(rows.len());
        for (key, row) in rows {
            match serde_json::from_value::<T>(row) {
                Ok(parsed) => out.push(parsed),
                Err(err) => {
                    tracing::warn!(container, row = %key, error = %err, "skipping malformed workspace row");
                }
            }
        }
        Ok(out)
    }
}

fn board_push_from_value(value: &serde_json::Value) -> Result<Option<GithubPushState>, DocError> {
    let state = value
        .get(BOARD_PUSH_KEY)
        .cloned()
        .map(serde_json::from_value::<GithubPushState>)
        .transpose()
        .map_err(DocError::from)?;
    if let Some(state) = state.as_ref() {
        state
            .validate()
            .map_err(|error| DocError::Schema(error.to_string()))?;
    }
    Ok(state)
}

fn write_board_push(row: &LoroMap, state: &GithubPushState) -> Result<(), DocError> {
    row.insert(
        BOARD_PUSH_KEY,
        LoroValue::from(serde_json::to_value(state)?),
    )?;
    Ok(())
}

/// Merge an informed config write with the immutable board-owned shadow. The
/// caller may change model/reasoning/etc.; it may neither erase nor retarget
/// the credential tuple once the board has stamped one.
fn reconcile_config_write(row: &LoroMap, config: &ChatConfig) -> Result<ChatConfig, DocError> {
    let mut config = config.clone();
    let configured = config.github_push_state();
    let value = row.get_deep_value().to_json_value();
    let shadow = board_push_from_value(&value)?;
    match (shadow, configured) {
        (Some(shadow), Ok(Some(configured))) if shadow != configured => {
            return Err(DocError::Schema(
                "a config edit cannot change the board-owned GitHub push tuple".into(),
            ));
        }
        (Some(shadow), Err(GithubPushStateError::RepositoryWithoutContract))
            if config.push_repo.as_deref() == Some(shadow.repo.as_str()) =>
        {
            config.apply_github_push_state(&shadow)
        }
        (Some(_), Err(GithubPushStateError::RepositoryWithoutContract)) => {
            return Err(DocError::Schema(
                "a config edit cannot change the board-owned GitHub push tuple".into(),
            ));
        }
        (Some(shadow), Ok(None)) => config.apply_github_push_state(&shadow),
        (None, Ok(Some(configured))) => write_board_push(row, &configured)?,
        (_, Err(error)) => return Err(DocError::Schema(error.to_string())),
        _ => {}
    }
    Ok(config)
}

fn set_opt_str(row: &LoroMap, key: &str, value: Option<&str>) -> Result<(), DocError> {
    match value {
        Some(v) => row.insert(key, v)?,
        None => row.delete(key)?,
    }
    Ok(())
}

fn set_opt_ms(row: &LoroMap, key: &str, value: Option<DateTime<Utc>>) -> Result<(), DocError> {
    match value {
        Some(at) => row.insert(key, at.timestamp_millis())?,
        None => row.delete(key)?,
    }
    Ok(())
}

fn status_str(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Idle => "idle",
        SessionStatus::Working => "working",
        SessionStatus::AwaitingInput => "awaitingInput",
        SessionStatus::Errored => "errored",
    }
}

fn dt(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or(DateTime::UNIX_EPOCH)
}

// ── doc-resident row shapes (epoch-millis timestamps) ───────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDevice {
    id: String,
    name: String,
    platform: String,
    #[serde(default)]
    last_seen_at: Option<i64>,
    #[serde(default)]
    created_at: Option<i64>,
    #[serde(default)]
    version: Option<String>,
}

impl From<RawDevice> for Device {
    fn from(raw: RawDevice) -> Self {
        Device {
            id: raw.id,
            name: raw.name,
            platform: raw.platform,
            last_seen_at: raw.last_seen_at.map(dt),
            created_at: raw.created_at.map(dt),
            version: raw.version,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSpace {
    id: String,
    device_id: String,
    path: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    git_detected: bool,
    #[serde(default)]
    git_checked_at: Option<i64>,
    #[serde(default)]
    checkout_id: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    created_at: i64,
}

impl From<RawSpace> for Space {
    fn from(raw: RawSpace) -> Self {
        Space {
            id: raw.id,
            device_id: raw.device_id,
            path: raw.path,
            name: raw.name,
            git_detected: raw.git_detected,
            git_checked_at: raw.git_checked_at.map(dt),
            checkout_id: raw.checkout_id,
            branch: raw.branch,
            created_at: dt(raw.created_at),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawChat {
    id: String,
    device_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    archived: bool,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    checkout_id: Option<String>,
    #[serde(default)]
    config: Option<ChatConfig>,
    #[serde(default)]
    creation_state: comet_proto::ChatCreationState,
    #[serde(default)]
    board_push: Option<GithubPushState>,
    #[serde(default)]
    last_message_preview: Option<String>,
    #[serde(default)]
    last_message_at: Option<i64>,
    #[serde(default)]
    created_at: i64,
    #[serde(default)]
    harness_session_id: Option<String>,
    #[serde(default)]
    harness_session_cwd: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    last_seen_at: Option<i64>,
    #[serde(default)]
    forked_from: Option<comet_proto::ChatLineage>,
}

impl From<RawChat> for Chat {
    fn from(raw: RawChat) -> Self {
        let mut config = raw.config;
        if let (Some(config), Some(shadow)) = (&mut config, raw.board_push.as_ref())
            && (matches!(config.github_push_state(), Ok(None))
                || (matches!(
                    config.github_push_state(),
                    Err(GithubPushStateError::RepositoryWithoutContract)
                ) && config.push_repo.as_deref() == Some(shadow.repo.as_str())))
        {
            config.apply_github_push_state(shadow);
        }
        Chat {
            id: raw.id,
            device_id: raw.device_id,
            title: raw.title,
            archived: raw.archived,
            cwd: raw.cwd,
            branch: raw.branch,
            checkout_id: raw.checkout_id,
            config,
            creation_state: raw.creation_state,
            last_message_preview: raw.last_message_preview,
            last_message_at: raw.last_message_at.map(dt),
            created_at: dt(raw.created_at),
            harness_session_id: raw.harness_session_id,
            harness_session_cwd: raw.harness_session_cwd,
            space_id: raw.space_id,
            last_seen_at: raw.last_seen_at.map(dt),
            forked_from: raw.forked_from,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawSession {
    chat_id: String,
    device_id: String,
    status: SessionStatus,
    #[serde(default)]
    started_at: Option<i64>,
    #[serde(default)]
    updated_at: i64,
}

impl From<RawSession> for Session {
    fn from(raw: RawSession) -> Self {
        Session {
            chat_id: raw.chat_id,
            device_id: raw.device_id,
            status: raw.status,
            started_at: raw.started_at.map(dt),
            updated_at: dt(raw.updated_at),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use comet_proto::{HarnessId, SandboxLevel};

    fn ts(ms: i64) -> DateTime<Utc> {
        dt(ms)
    }

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "linux".into(),
            last_seen_at: Some(ts(1_000)),
            created_at: Some(ts(500)),
            version: Some("0.1.0".into()),
        }
    }

    fn chat(id: &str, device_id: &str) -> Chat {
        Chat {
            creation_state: comet_proto::ChatCreationState::Ready,
            id: id.into(),
            device_id: device_id.into(),
            title: Some("First chat".into()),
            archived: false,
            cwd: Some("/tmp/repo".into()),
            branch: Some("main".into()),
            checkout_id: None,
            config: Some(ChatConfig {
                harness: HarnessId::Mock,
                model: Some("mock-1".into()),
                reasoning: None,
                model_options: Default::default(),
                sandbox: SandboxLevel::WorkspaceWrite,
                account: None,
                push_repo: None,
                push_contract: None,
                git_author: None,
                turn_limits: Default::default(),
                mcp_servers: Vec::new(),
            }),
            last_message_preview: None,
            last_message_at: None,
            created_at: ts(2_000),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            forked_from: None,
        }
    }

    fn space(id: &str, device_id: &str, path: &str) -> Space {
        Space {
            id: id.into(),
            device_id: device_id.into(),
            path: path.into(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            branch: None,
            created_at: ts(1_500),
        }
    }

    fn session(chat_id: &str, device_id: &str, status: SessionStatus) -> Session {
        Session {
            chat_id: chat_id.into(),
            device_id: device_id.into(),
            status,
            started_at: Some(ts(3_000)),
            updated_at: ts(3_500),
        }
    }

    fn cross_sync(a: &WorkspaceDoc, b: &WorkspaceDoc) {
        let a_update = a
            .doc()
            .export(ExportMode::updates(&b.doc().oplog_vv()))
            .expect("export a");
        let b_update = b
            .doc()
            .export(ExportMode::updates(&a.doc().oplog_vv()))
            .expect("export b");
        b.doc().import(&a_update).expect("import into b");
        a.doc().import(&b_update).expect("import into a");
    }

    #[test]
    fn set_chat_config_round_trips_and_ignores_missing_rows() {
        let ws = WorkspaceDoc::new();
        ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
        let mut options = serde_json::Map::new();
        options.insert("contextWindow".into(), serde_json::Value::String("1m".into()));
        let config = ChatConfig {
            harness: HarnessId::ClaudeCode,
            model: Some("claude-fable-5".into()),
            reasoning: Some(comet_proto::ReasoningLevel::XHigh),
            model_options: options,
            sandbox: SandboxLevel::WorkspaceWrite,
            account: Some("a1b2c3d4e5f60718".into()),
            // The board's dispatch stamps this; a mid-session model change is
            // a full-config replace, so it has to survive the round trip or a
            // dispatched agent silently loses its push credentials (gh#68).
            push_repo: Some("Florin-AS/tripletex-mcp".into()),
            push_contract: Some(comet_proto::GithubPushContract {
                contents_write: true,
                workflows_write: true,
            }),
            git_author: None,
            // Same argument for the turn guardrails (gh#270): a model change
            // that dropped them would leave the run loop watching nothing.
            turn_limits: comet_proto::TurnLimits {
                tool_failures: Some(10),
                tool_calls: Some(2000),
            },
            mcp_servers: vec![comet_proto::McpServer {
                name: "comet-board".into(),
                command: "comet-board".into(),
                args: vec!["mcp".into()],
            }],
        };
        assert!(ws.set_chat_config("chat-1", &config).unwrap());
        let row = ws.chat("chat-1").unwrap().expect("row exists");
        assert_eq!(row.config, Some(config.clone()));
        // No such row: false, nothing created.
        assert!(!ws.set_chat_config("nope", &config).unwrap());
        assert!(ws.chat("nope").unwrap().is_none());
    }

    #[test]
    fn remote_config_replacement_cannot_erase_the_board_push_tuple() {
        let a = WorkspaceDoc::new();
        let b = WorkspaceDoc::new();
        let mut dispatched = chat("chat-1", "dev-a");
        let original = GithubPushState {
            repo: "owner/widget".into(),
            contract: comet_proto::GithubPushContract {
                contents_write: true,
                workflows_write: true,
            },
        };
        dispatched
            .config
            .as_mut()
            .unwrap()
            .apply_github_push_state(&original);
        a.upsert_chat(&dispatched).unwrap();
        cross_sync(&a, &b);

        // Model/reasoning edits are whole-value CRDT replacements. Simulate an
        // older remote client that knows neither push field.
        let chats = b.doc().get_map("chats");
        let row = match chats.get("chat-1") {
            Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) => row,
            other => panic!("missing chat row: {other:?}"),
        };
        let mut replacement = dispatched.config.unwrap();
        replacement.model = Some("new-model".into());
        replacement.push_repo = None;
        replacement.push_contract = None;
        row.insert(
            "config",
            LoroValue::from(serde_json::to_value(replacement).unwrap()),
        )
        .unwrap();
        b.doc().commit();
        cross_sync(&a, &b);

        assert_eq!(
            a.github_push_state("chat-1").unwrap(),
            Some(original.clone())
        );
        assert_eq!(a.reconcile_github_push_states().unwrap(), 1);
        let repaired = a.chat("chat-1").unwrap().unwrap().config.unwrap();
        assert_eq!(
            repaired.github_push_state().unwrap(),
            Some(original.clone())
        );

        // The repair itself converges, so the uninformed device cannot keep
        // propagating an ordinary-looking chat.
        cross_sync(&a, &b);
        assert_eq!(b.github_push_state("chat-1").unwrap(), Some(original));
    }

    #[test]
    fn repo_only_config_is_repaired_from_the_board_push_shadow() {
        let ws = WorkspaceDoc::new();
        let mut dispatched = chat("chat-1", "dev-a");
        let original = GithubPushState {
            repo: "owner/widget".into(),
            contract: comet_proto::GithubPushContract {
                contents_write: true,
                workflows_write: true,
            },
        };
        dispatched
            .config
            .as_mut()
            .unwrap()
            .apply_github_push_state(&original);
        ws.upsert_chat(&dispatched).unwrap();

        // A predecessor schema retained pushRepo but knew no pushContract.
        // The immutable shadow is the authority, so this is recoverable rather
        // than a reason to brick the chat forever.
        let chats = ws.doc().get_map("chats");
        let row = match chats.get("chat-1") {
            Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) => row,
            other => panic!("missing chat row: {other:?}"),
        };
        let mut predecessor = dispatched.config.unwrap();
        predecessor.push_contract = None;
        row.insert(
            "config",
            LoroValue::from(serde_json::to_value(predecessor).unwrap()),
        )
        .unwrap();
        ws.doc().commit();

        assert_eq!(ws.reconcile_github_push_states().unwrap(), 1);
        assert_eq!(ws.github_push_state("chat-1").unwrap(), Some(original));
    }

    #[test]
    fn inconsistent_push_half_state_fails_closed_without_shadow_guessing() {
        let ws = WorkspaceDoc::new();
        let mut dispatched = chat("chat-1", "dev-a");
        let config = dispatched.config.as_mut().unwrap();
        config.push_repo = Some("owner/widget".into());
        config.push_contract = Some(comet_proto::GithubPushContract {
            contents_write: true,
            workflows_write: true,
        });
        ws.upsert_chat(&dispatched).unwrap();

        let chats = ws.doc().get_map("chats");
        let row = match chats.get("chat-1") {
            Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) => row,
            other => panic!("missing chat row: {other:?}"),
        };
        let mut inconsistent = dispatched.config.unwrap();
        inconsistent.push_contract = None;
        inconsistent.push_repo = Some("other/widget".into());
        row.insert(
            "config",
            LoroValue::from(serde_json::to_value(&inconsistent).unwrap()),
        )
        .unwrap();
        ws.doc().commit();

        let error = ws.github_push_state("chat-1").unwrap_err().to_string();
        assert!(
            error.contains("conflicts with the board-owned push tuple"),
            "{error}"
        );
        assert_eq!(ws.reconcile_github_push_states().unwrap(), 0);

        inconsistent.push_repo = None;
        inconsistent.push_contract = Some(comet_proto::GithubPushContract {
            contents_write: true,
            workflows_write: true,
        });
        row.insert(
            "config",
            LoroValue::from(serde_json::to_value(inconsistent).unwrap()),
        )
        .unwrap();
        ws.doc().commit();
        let error = ws.github_push_state("chat-1").unwrap_err().to_string();
        assert!(
            error.contains("capability contract but no repository"),
            "{error}"
        );
        assert_eq!(ws.reconcile_github_push_states().unwrap(), 0);
    }

    #[test]
    fn host_proven_contract_upgrades_a_true_legacy_row_once() {
        let ws = WorkspaceDoc::new();
        ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
        let chats = ws.doc().get_map("chats");
        let row = match chats.get("chat-1") {
            Some(loro::ValueOrContainer::Container(loro::Container::Map(row))) => row,
            other => panic!("missing chat row: {other:?}"),
        };
        let mut legacy = chat("chat-1", "dev-a").config.unwrap();
        legacy.push_repo = Some("owner/widget".into());
        legacy.push_contract = None;
        row.insert(
            "config",
            LoroValue::from(serde_json::to_value(legacy).unwrap()),
        )
        .unwrap();
        ws.doc().commit();

        assert_eq!(
            ws.legacy_github_push_repo("chat-1").unwrap().as_deref(),
            Some("owner/widget")
        );
        assert_eq!(ws.reconcile_github_push_states().unwrap(), 0);

        let recovered = GithubPushState {
            repo: "owner/widget".into(),
            contract: comet_proto::GithubPushContract {
                contents_write: true,
                workflows_write: false,
            },
        };
        assert!(
            ws.upgrade_legacy_github_push_state("chat-1", &recovered)
                .unwrap()
        );
        assert_eq!(ws.github_push_state("chat-1").unwrap(), Some(recovered));
        assert_eq!(ws.legacy_github_push_repo("chat-1").unwrap(), None);
    }

    #[test]
    fn rows_round_trip() {
        let ws = WorkspaceDoc::new();
        ws.upsert_device(&device("dev-a", "laptop")).unwrap();
        let mut expected_chat = chat("chat-1", "dev-a");
        expected_chat.creation_state = comet_proto::ChatCreationState::Draft;
        ws.upsert_chat(&expected_chat).unwrap();
        ws.upsert_session(&session("chat-1", "dev-a", SessionStatus::Working))
            .unwrap();

        let state = ws.read_all().unwrap();
        assert_eq!(state.devices, vec![device("dev-a", "laptop")]);
        assert_eq!(state.chats, vec![expected_chat]);
        assert_eq!(
            state.sessions,
            vec![session("chat-1", "dev-a", SessionStatus::Working)]
        );

        // Upsert refreshes in place — no duplicate rows, cleared options removed.
        let mut updated = chat("chat-1", "dev-a");
        updated.title = None;
        updated.last_message_preview = Some("hello".into());
        updated.last_message_at = Some(ts(9_000));
        ws.upsert_chat(&updated).unwrap();
        let chats = ws.read_chats().unwrap();
        assert_eq!(chats.len(), 1);
        assert_eq!(chats[0].title, None);
        assert_eq!(chats[0].last_message_preview.as_deref(), Some("hello"));
    }

    #[test]
    fn snapshot_round_trips_between_docs() {
        let ws = WorkspaceDoc::new();
        ws.upsert_device(&device("dev-a", "laptop")).unwrap();
        ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
        let bytes = ws.export_snapshot().unwrap();

        let other = LoroDoc::new();
        other.import(&bytes).unwrap();
        let restored = WorkspaceDoc::from_doc(other);
        assert_eq!(restored.read_all().unwrap(), ws.read_all().unwrap());
    }

    #[test]
    fn field_mutators_round_trip() {
        let ws = WorkspaceDoc::new();
        ws.upsert_device(&device("dev-a", "laptop")).unwrap();
        ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();

        assert!(ws.rename_chat("chat-1", "Renamed").unwrap());
        assert!(ws.set_chat_archived("chat-1", true).unwrap());
        assert!(
            ws.set_chat_last_message("chat-1", "preview text", ts(5_000))
                .unwrap()
        );
        assert!(ws.rename_device("dev-a", "workstation").unwrap());
        assert!(ws.set_device_last_seen("dev-a", ts(6_000)).unwrap());
        // Unknown rows report false, never invent rows.
        assert!(!ws.rename_chat("nope", "x").unwrap());
        assert!(!ws.set_chat_archived("nope", true).unwrap());
        assert!(!ws.rename_device("nope", "x").unwrap());

        let chat = ws.chat("chat-1").unwrap().unwrap();
        assert_eq!(chat.title.as_deref(), Some("Renamed"));
        assert!(chat.archived);
        assert_eq!(chat.last_message_preview.as_deref(), Some("preview text"));
        assert_eq!(chat.last_message_at, Some(ts(5_000)));
        let dev = &ws.read_devices().unwrap()[0];
        assert_eq!(dev.name, "workstation");
        assert_eq!(dev.last_seen_at, Some(ts(6_000)));
    }

    #[test]
    fn delete_chat_tombstones_row_and_session() {
        let ws = WorkspaceDoc::new();
        ws.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
        ws.upsert_session(&session("chat-1", "dev-a", SessionStatus::Idle))
            .unwrap();
        assert!(ws.delete_chat("chat-1").unwrap());
        assert!(ws.read_chats().unwrap().is_empty());
        assert!(ws.read_sessions().unwrap().is_empty());
        assert!(!ws.delete_chat("chat-1").unwrap());
    }

    #[test]
    fn two_peers_converge_on_disjoint_rows() {
        let a = WorkspaceDoc::new();
        let b = WorkspaceDoc::new();
        // Writer discipline: each device writes its own rows, concurrently.
        a.upsert_device(&device("dev-a", "laptop")).unwrap();
        a.upsert_chat(&chat("chat-a", "dev-a")).unwrap();
        a.upsert_session(&session("chat-a", "dev-a", SessionStatus::Working))
            .unwrap();
        b.upsert_device(&device("dev-b", "vps")).unwrap();
        b.upsert_chat(&chat("chat-b", "dev-b")).unwrap();

        cross_sync(&a, &b);

        let sa = a.read_all().unwrap();
        let sb = b.read_all().unwrap();
        assert_eq!(sa, sb);
        assert_eq!(
            sa.devices.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(),
            vec!["dev-a", "dev-b"]
        );
        assert_eq!(
            sa.chats.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            vec!["chat-a", "chat-b"]
        );
        assert_eq!(sa.sessions.len(), 1);
    }

    #[test]
    fn spaces_round_trip_and_mutate() {
        let ws = WorkspaceDoc::new();
        ws.upsert_space(&space("sp-1", "dev-a", "/home/u/project"))
            .unwrap();
        let row = ws.space("sp-1").unwrap().expect("row exists");
        assert_eq!(row.display_name(), "project");
        assert!(!row.git_detected);

        assert!(ws.rename_space("sp-1", Some("My Project")).unwrap());
        assert_eq!(
            ws.space("sp-1").unwrap().unwrap().display_name(),
            "My Project"
        );
        assert!(ws.rename_space("sp-1", None).unwrap());
        assert_eq!(ws.space("sp-1").unwrap().unwrap().display_name(), "project");

        assert!(
            ws.set_space_git("sp-1", true, Some("checkout-abc"), Some("main"), ts(4_000))
                .unwrap()
        );
        let row = ws.space("sp-1").unwrap().unwrap();
        assert!(row.git_detected);
        assert_eq!(row.checkout_id.as_deref(), Some("checkout-abc"));
        assert_eq!(row.branch.as_deref(), Some("main"));
        assert_eq!(row.git_checked_at, Some(ts(4_000)));

        // A branch is a fact about a work tree, so de-gitting the folder must
        // clear it rather than leave the sidebar naming a branch that is gone.
        assert!(
            ws.set_space_git("sp-1", false, None, None, ts(5_000))
                .unwrap()
        );
        let row = ws.space("sp-1").unwrap().unwrap();
        assert!(!row.git_detected);
        assert_eq!(row.branch, None);

        // Unknown rows report false, never invent rows.
        assert!(!ws.rename_space("nope", Some("x")).unwrap());
        assert!(!ws.set_space_git("nope", true, None, None, ts(1)).unwrap());
    }

    #[test]
    fn delete_space_cascades_and_converges_across_peers() {
        let a = WorkspaceDoc::new();
        a.upsert_space(&space("sp-1", "dev-a", "/tmp/one")).unwrap();
        a.upsert_space(&space("sp-2", "dev-a", "/tmp/two")).unwrap();
        let mut in_space = chat("chat-1", "dev-a");
        in_space.space_id = Some("sp-1".into());
        let mut other = chat("chat-2", "dev-a");
        other.space_id = Some("sp-2".into());
        a.upsert_chat(&in_space).unwrap();
        a.upsert_chat(&other).unwrap();
        a.upsert_session(&session("chat-1", "dev-a", SessionStatus::Working))
            .unwrap();

        let b = WorkspaceDoc::from_doc({
            let d = LoroDoc::new();
            d.import(&a.export_snapshot().unwrap()).unwrap();
            d
        });

        let deleted = a.delete_space("sp-1").unwrap();
        assert!(deleted.existed);
        assert_eq!(deleted.chat_ids, vec!["chat-1".to_string()]);
        cross_sync(&a, &b);

        for ws in [&a, &b] {
            let state = ws.read_all().unwrap();
            assert_eq!(
                state.spaces.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
                vec!["sp-2"]
            );
            assert_eq!(
                state.chats.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
                vec!["chat-2"]
            );
            assert!(state.sessions.is_empty());
        }
        // Idempotent on a gone space.
        let again = b.delete_space("sp-1").unwrap();
        assert!(!again.existed);
        assert!(again.chat_ids.is_empty());
    }

    #[test]
    fn chat_seen_is_monotonic_and_settles_lww() {
        let a = WorkspaceDoc::new();
        a.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
        assert!(a.set_chat_seen("chat-1", ts(5_000)).unwrap());
        assert_eq!(
            a.chat("chat-1").unwrap().unwrap().last_seen_at,
            Some(ts(5_000))
        );
        // Monotonic guard: older stamps are ignored without an oplog write.
        let before = a.doc().oplog_vv();
        assert!(a.set_chat_seen("chat-1", ts(4_000)).unwrap());
        assert_eq!(a.doc().oplog_vv(), before);
        assert_eq!(
            a.chat("chat-1").unwrap().unwrap().last_seen_at,
            Some(ts(5_000))
        );
        assert!(!a.set_chat_seen("nope", ts(1)).unwrap());

        // Concurrent marks from two peers settle on the same winner.
        let b = WorkspaceDoc::from_doc({
            let d = LoroDoc::new();
            d.import(&a.export_snapshot().unwrap()).unwrap();
            d
        });
        a.set_chat_seen("chat-1", ts(9_000)).unwrap();
        b.set_chat_seen("chat-1", ts(9_001)).unwrap();
        cross_sync(&a, &b);
        let seen_a = a.chat("chat-1").unwrap().unwrap().last_seen_at;
        let seen_b = b.chat("chat-1").unwrap().unwrap().last_seen_at;
        assert_eq!(seen_a, seen_b);
        assert!(matches!(seen_a, Some(t) if t == ts(9_000) || t == ts(9_001)));
    }

    #[test]
    fn schema_version_stamp_is_idempotent() {
        let ws = WorkspaceDoc::new();
        assert_eq!(ws.ensure_schema_version().unwrap(), WORKSPACE_SCHEMA_VERSION);
        let before = ws.doc().oplog_vv();
        assert_eq!(ws.ensure_schema_version().unwrap(), WORKSPACE_SCHEMA_VERSION);
        assert_eq!(ws.doc().oplog_vv(), before);
    }

    #[test]
    fn concurrent_rename_settles_lww_on_both_peers() {
        let a = WorkspaceDoc::new();
        a.upsert_chat(&chat("chat-1", "dev-a")).unwrap();
        let b = WorkspaceDoc::from_doc({
            let d = LoroDoc::new();
            d.import(&a.export_snapshot().unwrap()).unwrap();
            d
        });

        // Concurrent renames of the SAME row field from both peers.
        a.rename_chat("chat-1", "from a").unwrap();
        b.rename_chat("chat-1", "from b").unwrap();
        cross_sync(&a, &b);

        let title_a = a.chat("chat-1").unwrap().unwrap().title;
        let title_b = b.chat("chat-1").unwrap().unwrap().title;
        // LWW: both peers settle on the SAME winner (whichever it is).
        assert_eq!(title_a, title_b);
        assert!(matches!(
            title_a.as_deref(),
            Some("from a") | Some("from b")
        ));
        // Everything else on the row survived the conflict.
        assert_eq!(a.chat("chat-1").unwrap().unwrap().device_id, "dev-a");
    }
}
