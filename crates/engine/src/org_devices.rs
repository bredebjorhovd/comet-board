//! OrgDevices — the org-wide device registry (gh#66).
//!
//! The workspace doc is deliberately per-user (`ws4/{orgId}/{userId}`): spaces,
//! chats and sessions are private to the person who made them. Devices are the
//! one thing that cannot be, because a device is how a *team* reaches shared
//! work. A teammate signing in on their own laptop starts with an empty
//! workspace doc, so before this existed they saw no devices at all — no box to
//! address with `targetDeviceId`, nothing for the board pane's host sweep
//! (`comet_proto::view::board::host_candidates`) to visit, and therefore no
//! board. This host closes exactly that gap and nothing more.
//!
//! Shape: ONE room per org (`orgdev1/{orgId}`, same SessionRoom DO class,
//! org-membership authz at the edge like a workspace room), holding a doc whose
//! only populated container is `devices` — hence the [`WorkspaceDoc`] reuse: the
//! rows, the LWW writer discipline and the `presence/{deviceId}` ephemeral
//! channel are the same ones, so nothing new had to be invented for them.
//!
//! Writer discipline is unchanged and matters more here, since the peers are
//! now other PEOPLE's devices: each device writes only its own row; renames are
//! LWW sets from any device (the settings UI already allowed that within a
//! user's own fleet).
//!
//! Driven entirely by [`crate::workspace_host::WorkspaceHost`] — it shares the
//! workspace host's change channel and snapshot debounce, so a second registry
//! costs no second background task. Since gh#145 it also costs no periodic
//! traffic at all: presence on this room is derived by the edge from the socket
//! set, and this host only reads the answers.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use tokio::sync::watch;

use comet_doc::WorkspaceDoc;
use comet_proto::Device;
use comet_sync::{DocsStore, RoomClient};

use crate::EngineError;
use crate::doc_host::EdgeConfig;
use crate::workspace_host::{lock, spawn_room_join};

/// Snapshot row id in the local `DocsStore` — distinct from the workspace doc's
/// `workspace2` row, so an org registry and a private workspace never overwrite
/// one another.
pub const ORG_DEVICES_DOC_ID: &str = "orgdevices1";

#[derive(Debug, Clone)]
pub struct OrgDevicesConfig {
    pub device_id: String,
    pub org_id: String,
    /// When present, the host joins `/org/{orgId}/devices/ws`. `None` = fully
    /// offline: the registry is then just this device's own row, which is
    /// exactly what a single-device offline engine needs anyway.
    pub edge: Option<EdgeConfig>,
}

struct OrgDevicesInner {
    store: Arc<DocsStore>,
    config: OrgDevicesConfig,
    doc: Arc<WorkspaceDoc>,
    room: Arc<Mutex<Option<RoomClient>>>,
    /// Called on every ephemeral update on the org room — the room's answer
    /// about who it can see, which the workspace host folds into its belief and
    /// its device rows. Wired after construction (the host does not exist yet in
    /// [`OrgDevices::open`]).
    on_presence: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Doc subscription (drop = unsubscribe) — bumps the workspace host's
    /// change watch, so a remote device row lands in `WatchDevices` the same
    /// way a local one does.
    _sub: loro::Subscription,
}

#[derive(Clone)]
pub struct OrgDevices {
    inner: Arc<OrgDevicesInner>,
}

impl OrgDevices {
    /// Load (or init) the registry doc and join the org room when configured.
    /// `changed_tx` is the workspace host's change channel: bumping it is what
    /// republishes the merged device list.
    pub fn open(
        store: Arc<DocsStore>,
        config: OrgDevicesConfig,
        changed_tx: watch::Sender<u64>,
    ) -> Result<Self, EngineError> {
        let doc = match store.load_snapshot(ORG_DEVICES_DOC_ID)? {
            Some(bytes) => {
                let raw = loro::LoroDoc::new();
                raw.import(&bytes).map_err(|e| {
                    EngineError::Other(format!("org devices snapshot import failed: {e}"))
                })?;
                WorkspaceDoc::from_doc(raw)
            }
            None => WorkspaceDoc::new(),
        };
        let doc = Arc::new(doc);
        doc.ensure_schema_version()?;
        let sub = doc.doc().subscribe_root(Arc::new(move |_diff| {
            changed_tx.send_modify(|v| *v = v.wrapping_add(1));
        }));

        let host = Self {
            inner: Arc::new(OrgDevicesInner {
                store,
                config,
                doc,
                room: Arc::new(Mutex::new(None)),
                on_presence: Mutex::new(None),
                _sub: sub,
            }),
        };
        host.join_room();
        Ok(host)
    }

    /// Wire the "this room answered about presence" signal — the workspace host
    /// points it at its own belief and publish, so a teammate's box coming
    /// online updates the device list at once instead of on the next tick.
    pub fn set_presence_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *lock(&self.inner.on_presence) = Some(hook);
    }

    fn join_room(&self) {
        let Some(edge) = &self.inner.config.edge else {
            return;
        };
        let org_id = &self.inner.config.org_id;
        // The device id rides the socket so the edge can DERIVE presence from
        // it rather than be beaten awake to be told (gh#145).
        let url = edge.room_url_as(
            format!("/org/{org_id}/devices/ws"),
            Some(&self.inner.config.device_id),
        );
        // Must match the room id the edge derives from the URL — a mismatch is
        // a room of one.
        let room_id = format!("orgdev1/{org_id}");
        let weak = Arc::downgrade(&self.inner);
        spawn_room_join(
            url,
            room_id,
            self.inner.doc.doc().clone(),
            Arc::downgrade(&self.inner.room),
            Arc::new(move || {
                let hook = weak
                    .upgrade()
                    .and_then(|inner| lock(&inner.on_presence).clone());
                if let Some(hook) = hook {
                    hook();
                }
            }),
        );
    }

    /// Is the org registry room joined RIGHT NOW? (Not "do we hold a client" —
    /// see [`comet_sync::RoomClient::connected`], gh#116.)
    pub fn connected(&self) -> bool {
        lock(&self.inner.room)
            .as_ref()
            .is_some_and(|client| client.connected())
    }

    /// Is the org room's `%EPH` presence sub-room joined RIGHT NOW? This is
    /// the channel a teammate's online dot rides — see
    /// [`crate::workspace_host::WorkspaceHost::presence_connected`] (gh#126).
    pub fn presence_connected(&self) -> bool {
        lock(&self.inner.room)
            .as_ref()
            .is_some_and(|client| client.presence_joined())
    }

    /// The underlying doc — what the room syncs (and what tests bridge).
    pub fn doc_arc(&self) -> Arc<WorkspaceDoc> {
        self.inner.doc.clone()
    }

    /// Every device row the org has published (this device's included).
    /// Best-effort: an unreadable doc logs and reads as empty rather than
    /// failing the device list it feeds.
    pub fn read_devices(&self) -> Vec<Device> {
        match self.inner.doc.read_devices() {
            Ok(devices) => devices,
            Err(err) => {
                tracing::warn!(error = %err, "org devices read failed");
                Vec::new()
            }
        }
    }

    /// Publish THIS device's row (writer discipline: never another's).
    pub fn upsert_device(&self, device: &Device) {
        if let Err(err) = self.inner.doc.upsert_device(device) {
            tracing::warn!(error = %err, "org device row write failed");
        }
    }

    /// The row for `device_id`, if the org has one.
    pub fn device(&self, device_id: &str) -> Option<Device> {
        self.read_devices().into_iter().find(|d| d.id == device_id)
    }

    /// LWW rename from any device (mirrors the workspace doc's rule).
    pub fn rename_device(&self, device_id: &str, name: &str) {
        if let Err(err) = self.inner.doc.rename_device(device_id, name) {
            tracing::warn!(device = %device_id, error = %err, "org device rename failed");
        }
    }

    /// Boot/shutdown `lastSeenAt` stamp — liveness in between is an overlay
    /// over the live presence belief, never a row write.
    pub fn set_device_last_seen(&self, device_id: &str, at: DateTime<Utc>) {
        if let Err(err) = self.inner.doc.set_device_last_seen(device_id, at) {
            tracing::warn!(device = %device_id, error = %err, "org device lastSeenAt stamp failed");
        }
    }

    /// The device ids the org room's latest `%EPH` answer lists — the edge's
    /// own reading of its socket set (gh#145), and what makes the box's online
    /// dot true on a TEAMMATE's laptop (their device is only ever on this room).
    ///
    /// `None` = no live room handle, which is not "nobody is here": it means
    /// this engine has no answer, and the belief must be left alone.
    pub fn presence_ids(&self) -> Option<std::collections::HashSet<String>> {
        lock(&self.inner.room)
            .as_ref()
            .map(crate::presence::room_presence_ids)
    }

    pub fn save_snapshot(&self) {
        match self.inner.doc.export_snapshot() {
            Ok(bytes) => {
                if let Err(err) = self.inner.store.save_snapshot(ORG_DEVICES_DOC_ID, &bytes) {
                    tracing::warn!(error = %err, "org devices snapshot save failed");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "org devices snapshot export failed");
            }
        }
    }
}

/// Merge the org registry over a per-user workspace doc's device rows.
///
/// Both are written by the same device for itself, so in a steady fleet they
/// agree and this is a union. It exists for the two states where they do not:
/// a device still running a build that only knows the workspace doc (kept, or
/// it would vanish from its owner's own device list), and every device of every
/// OTHER user in the org (only ever in the registry). The registry wins ties —
/// it is the doc a device's current build writes.
pub fn merge_devices(workspace: Vec<Device>, org: Vec<Device>) -> Vec<Device> {
    let mut merged: std::collections::HashMap<String, Device> = workspace
        .into_iter()
        .map(|device| (device.id.clone(), device))
        .collect();
    for device in org {
        merged.insert(device.id.clone(), device);
    }
    let mut list: Vec<Device> = merged.into_values().collect();
    // Registration order — the same order the device switcher lists them in,
    // and the order the board's host sweep depends on being stable.
    list.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str, created_ms: i64) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "macos".into(),
            last_seen_at: None,
            created_at: DateTime::from_timestamp_millis(created_ms),
            version: None,
        }
    }

    #[test]
    fn the_registry_row_wins_and_teammates_devices_are_kept() {
        let merged = merge_devices(
            vec![device("mine", "my laptop (stale name)", 1_000)],
            vec![
                device("mine", "my laptop", 1_000),
                device("box", "the box", 500),
            ],
        );
        let names: Vec<&str> = merged.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["the box", "my laptop"], "created_at order");
    }

    #[test]
    fn a_device_only_the_workspace_doc_knows_survives() {
        // An older build of one of the user's own devices writes the workspace
        // doc only; dropping it would delete a device from its owner's list.
        let merged = merge_devices(vec![device("old", "old build", 1)], Vec::new());
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "old");
    }
}
