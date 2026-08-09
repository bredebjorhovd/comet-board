//! SpacesSync — owner-side upkeep of space rows (git presence) plus the
//! orphan-chat repair sweep.
//!
//! A space is a synced (device, folder) pair; the folder need NOT be a git
//! repo. This service watches the workspace `spaces` rows owned by THIS device
//! and keeps their `gitDetected`/`checkoutId`/`branch` stamps truthful:
//!
//! - recheck on boot / when a space row is first observed;
//! - a non-recursive `notify` watcher on the space folder — `.git` appearing or
//!   vanishing (git init / de-git) kicks a recheck — and a second one on the
//!   folder's `.git`, whose direct child `HEAD` is rewritten by every checkout,
//!   so switching branch shows up without waiting for the repair tick;
//! - a slow 2-minute repair tick (native watchers coalesce/drop events).
//!
//! Stamps are written ONLY on change, so steady state never grows the oplog.
//! Remote devices read `space.git_detected` straight from the doc — branch
//! pickers and the diff sidebar gate on it with zero RPCs — and `space.branch`
//! the same way: it is what a *collapsed* space row says about where the
//! folder is (gh#229), on a phone that will never run git against that disk.
//!
//! The repair tick also runs the orphan sweep: a chat created concurrently
//! with a `deleteSpace` on another device can sync in after the cascade ran,
//! leaving a dangling `spaceId`. The HOST device deletes its own such chats
//! (writer discipline — we never touch other devices' rows).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use comet_proto::Space;

use crate::repos::Repos;
use crate::workspace_host::WorkspaceHost;

/// Trailing debounce after a filesystem event burst.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(500);
/// Slow repair pass: recheck every owned space + orphan sweep.
const REPAIR_INTERVAL: Duration = Duration::from_secs(120);

struct SpaceEntry {
    path: PathBuf,
    kick_tx: mpsc::UnboundedSender<()>,
    /// Keeps the folder + `.git` watchers alive; dropped on entry close.
    _watchers: Vec<notify::RecommendedWatcher>,
}

struct SpacesSyncInner {
    repos: Repos,
    workspace: WorkspaceHost,
    device_id: String,
    entries: Mutex<HashMap<String, Arc<SpaceEntry>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
pub struct SpacesSync {
    inner: Arc<SpacesSyncInner>,
}

impl SpacesSync {
    /// Build and start the sync loop: follows the workspace spaces watch and
    /// runs the repair tick. Requires a tokio runtime.
    pub fn start(repos: Repos, workspace: WorkspaceHost, device_id: &str) -> Self {
        let sync = Self {
            inner: Arc::new(SpacesSyncInner {
                repos,
                workspace: workspace.clone(),
                device_id: device_id.to_string(),
                entries: Mutex::new(HashMap::new()),
            }),
        };
        tokio::spawn(spaces_task(
            Arc::downgrade(&sync.inner),
            workspace.watch_spaces(),
        ));
        sync
    }

    /// Reconcile + recheck now (tests / opportunistic callers).
    pub async fn reconcile_now(&self) {
        let spaces = self.inner.workspace.watch_spaces().borrow().clone();
        reconcile(&self.inner, &spaces);
        for entry in lock(&self.inner.entries).values() {
            let _ = entry.kick_tx.send(());
        }
    }
}

/// (Re)build the entry set for the spaces THIS device owns.
fn reconcile(inner: &Arc<SpacesSyncInner>, spaces: &[Space]) {
    let owned: HashMap<&str, &Space> = spaces
        .iter()
        .filter(|s| s.device_id == inner.device_id)
        .map(|s| (s.id.as_str(), s))
        .collect();

    let mut entries = lock(&inner.entries);
    entries.retain(|id, _| owned.contains_key(id.as_str()));
    for (id, space) in owned {
        if entries.contains_key(id) {
            continue; // deviceId/path are immutable — nothing to refresh
        }
        let (kick_tx, kick_rx) = mpsc::unbounded_channel();
        // Two non-recursive watchers, both filtered to the two names that mean
        // something here: `.git` among the space folder's direct children is
        // git init / de-git, and `HEAD` among `.git`'s is a checkout. Filtering
        // matters on the second one — `.git/index` is rewritten by every `git
        // status` a build script runs, and an unfiltered watch would spend
        // three git subprocesses per keystroke-driven rebuild. Watch failures
        // are fine (a folder that is not a repo has no `.git` to watch yet):
        // the repair tick still converges.
        let watchers = [PathBuf::from(&space.path), Path::new(&space.path).join(".git")]
            .into_iter()
            .filter_map(|target| {
                let tx = kick_tx.clone();
                let result = notify::recommended_watcher(
                    move |event: Result<notify::Event, notify::Error>| {
                        let Ok(event) = event else { return };
                        if event
                            .paths
                            .iter()
                            .any(|p| p.file_name().is_some_and(|n| n == ".git" || n == "HEAD"))
                        {
                            let _ = tx.send(());
                        }
                    },
                );
                match result {
                    Ok(mut watcher) => {
                        use notify::Watcher as _;
                        match watcher.watch(&target, notify::RecursiveMode::NonRecursive) {
                            Ok(()) => Some(watcher),
                            Err(err) => {
                                tracing::debug!(path = %target.display(), error = %err, "spaces: watch failed");
                                None
                            }
                        }
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "spaces: watcher create failed");
                        None
                    }
                }
            })
            .collect();
        let entry = Arc::new(SpaceEntry {
            path: PathBuf::from(&space.path),
            kick_tx: kick_tx.clone(),
            _watchers: watchers,
        });
        entries.insert(id.to_string(), entry.clone());
        tokio::spawn(entry_task(
            Arc::downgrade(inner),
            id.to_string(),
            Arc::downgrade(&entry),
            kick_rx,
        ));
        let _ = kick_tx.send(()); // initial check (boot / first observed)
    }
}

/// Per-space task: trailing-debounce kicks, then recheck git presence.
async fn entry_task(
    inner: Weak<SpacesSyncInner>,
    space_id: String,
    entry: Weak<SpaceEntry>,
    mut kick_rx: mpsc::UnboundedReceiver<()>,
) {
    while kick_rx.recv().await.is_some() {
        loop {
            match tokio::time::timeout(WATCH_DEBOUNCE, kick_rx.recv()).await {
                Ok(Some(())) => continue,
                Ok(None) => return, // entry closed mid-burst
                Err(_) => break,
            }
        }
        let (Some(inner), Some(entry)) = (inner.upgrade(), entry.upgrade()) else {
            return;
        };
        check_space(&inner, &space_id, &entry.path).await;
    }
}

/// Probe git presence + the checked-out branch and stamp the row — write only
/// on change.
async fn check_space(inner: &Arc<SpacesSyncInner>, space_id: &str, path: &Path) {
    let detected = inner.repos.is_repo(path).await;
    let (checkout_id, branch) = if detected {
        let checkout_id = match inner.repos.checkout_identity(path).await {
            Ok(identity) => Some(identity.id),
            Err(err) => {
                tracing::debug!(space = %space_id, error = %err, "spaces: checkout identity failed");
                None
            }
        };
        let branch = match inner.repos.current_branch(path).await {
            Ok(branch) => Some(branch),
            Err(err) => {
                tracing::debug!(space = %space_id, error = %err, "spaces: branch read failed");
                None
            }
        };
        (checkout_id, branch)
    } else {
        (None, None)
    };
    let current = match inner.workspace.read_spaces() {
        Ok(spaces) => spaces.into_iter().find(|s| s.id == space_id),
        Err(err) => {
            tracing::warn!(space = %space_id, error = %err, "spaces: row read failed");
            return;
        }
    };
    let Some(current) = current else {
        return; // deleted while checking
    };
    if current.git_detected == detected
        && current.checkout_id == checkout_id
        && current.branch == branch
    {
        return; // unchanged — no oplog growth
    }
    match inner.workspace.set_space_git(
        space_id,
        detected,
        checkout_id.as_deref(),
        branch.as_deref(),
    ) {
        Ok(_) => {
            tracing::info!(space = %space_id, git = detected, "space git presence updated");
        }
        Err(err) => {
            tracing::warn!(space = %space_id, error = %err, "spaces: git stamp failed");
        }
    }
    // A folder that just became (or stopped being) a repo has the wrong
    // watchers: the `.git` watch is installed at reconcile time and there was
    // no `.git` to watch. Dropping the entry makes the next reconcile — which
    // the stamp above just triggered, via the spaces watch — build it again
    // with both watchers.
    if current.git_detected != detected {
        lock(&inner.entries).remove(space_id);
    }
}

/// Host-side repair: delete OUR chats whose `spaceId` dangles (create-vs-delete
/// race). Chats hosted by other devices are left alone.
fn sweep_orphans(inner: &Arc<SpacesSyncInner>) {
    let spaces = inner.workspace.watch_spaces().borrow().clone();
    let live: std::collections::HashSet<&str> = spaces.iter().map(|s| s.id.as_str()).collect();
    let chats = inner.workspace.watch_chats().borrow().clone();
    for chat in chats {
        if chat.device_id != inner.device_id {
            continue;
        }
        let Some(space_id) = chat.space_id.as_deref() else {
            continue;
        };
        if live.contains(space_id) {
            continue;
        }
        tracing::info!(chat = %chat.id, space = %space_id, "deleting orphaned chat (space gone)");
        if let Err(err) = inner.workspace.delete_chat(&chat.id) {
            tracing::warn!(chat = %chat.id, error = %err, "spaces: orphan delete failed");
        }
    }
}

/// Spaces-watch follower + repair tick. Weak handles so dropping the service
/// tears the loop down.
async fn spaces_task(inner: Weak<SpacesSyncInner>, mut spaces_rx: watch::Receiver<Vec<Space>>) {
    let mut repair = tokio::time::interval(REPAIR_INTERVAL);
    repair.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    repair.tick().await; // consume the immediate first tick
    {
        let Some(inner) = inner.upgrade() else { return };
        let spaces = spaces_rx.borrow().clone();
        reconcile(&inner, &spaces);
    }
    loop {
        tokio::select! {
            changed = spaces_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let Some(inner) = inner.upgrade() else { break };
                let spaces = spaces_rx.borrow_and_update().clone();
                reconcile(&inner, &spaces);
            }
            _ = repair.tick() => {
                let Some(inner) = inner.upgrade() else { break };
                let spaces = spaces_rx.borrow().clone();
                reconcile(&inner, &spaces);
                for entry in lock(&inner.entries).values() {
                    let _ = entry.kick_tx.send(());
                }
                sweep_orphans(&inner);
            }
        }
    }
}
