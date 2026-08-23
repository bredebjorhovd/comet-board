//! Filesystem watcher budget — the engine's account of every `notify` watcher
//! it holds, with a hard bound on the population (gh#552).
//!
//! The incident: the always-on box panicked every few hours with
//! `notify-7.0.0/src/inotify.rs:135 — poll failed: Resource temporarily
//! unavailable`, four times in 48h. Each panic reached this engine's crash
//! shield and exited the process, which dropped every sync room it hosted —
//! 13 active chats read as "threads are not updating" on a phone. At crash
//! time the process held **103 inotify instances** against the kernel's
//! `fs.inotify.max_user_instances = 128`: each [`notify::RecommendedWatcher`]
//! is one instance (one fd + one event-loop thread), recursive or not, so the
//! population was spaces watchers (two per owned space) plus diff-sync
//! checkout entries (one per watched root) — and nothing above the kernel cap
//! would have said so until the kernel refused.
//!
//! Four properties, in the order the issue named them:
//!
//! 1. **Bounded.** Every watcher is created through [`FsWatcher::spawn`],
//!    which counts live instances and refuses to create past
//!    [`limit()`] (`COMET_MAX_FS_WATCHERS`, default [`DEFAULT_MAX_WATCHERS`],
//!    deliberately under the old kernel default of 128). A refusal degrades
//!    that ONE watch to the services' existing 2-minute repair ticks — the
//!    same terms an oversized tree already runs on (gh#526's
//!    `MAX_WATCH_DIRS`) — instead of letting the kernel cap surface as EAGAIN
//!    inside notify.
//! 2. **Released on sweep.** Slots come back on [`Drop`]; callers drop
//!    entries when worktrees are reclaimed (`CheckoutDiffSync::release_under`)
//!    rather than waiting for a reconcile to notice the directory left.
//! 3. **Degrading, not fatal.** The two ways a watcher can die — its
//!    event-loop thread panicking on a poll error, and the `unwrap`s in
//!    notify's own `Drop`/watch paths firing afterwards because that loop is
//!    gone — are classified by [`is_watcher_panic`] and handled without
//!    taking the process down: the crash shield consults it before arming the
//!    exit, and [`FsWatcher`]'s own `Drop` swallows the second-order unwrap
//!    so closing an entry can never unwind into whatever drops it. A dead
//!    watcher stops delivering events; the repair tick keeps the data right.
//! 4. **Observable.** [`census`] reports open vs limit plus lifetime created/
//!    refused/degraded counters; it rides [`comet_proto::EdgeHealth`] so
//!    `comet status` and doctor show an engine nearing its bound instead of
//!    leaving the reading to the kernel's error path.

use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Default ceiling on live watcher instances. Under the kernel default
/// (`fs.inotify.max_user_instances = 128`) so this engine refuses a watch —
/// one degraded checkout — while other processes' instances and headroom for
/// the rest of the box still exist.
pub const DEFAULT_MAX_WATCHERS: usize = 96;

static OPEN: AtomicUsize = AtomicUsize::new(0);
static CREATED_TOTAL: AtomicU64 = AtomicU64::new(0);
static REFUSED_TOTAL: AtomicU64 = AtomicU64::new(0);
static DEGRADED_TOTAL: AtomicU64 = AtomicU64::new(0);

static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

/// The watcher-instance ceiling: `COMET_MAX_FS_WATCHERS` if set (and parses),
/// else [`DEFAULT_MAX_WATCHERS`]. Read once; engines do not re-read env.
pub fn limit() -> usize {
    *LIMIT.get_or_init(|| {
        std::env::var("COMET_MAX_FS_WATCHERS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_MAX_WATCHERS)
    })
}

/// A point-in-time reading of the watcher population.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FsWatchCensus {
    /// Watcher instances held right now (inotify/kqueue fds).
    pub open: usize,
    /// What [`FsWatcher::spawn`] will refuse past.
    pub limit: usize,
    /// Successfully installed watches over the life of the process.
    pub created_total: u64,
    /// Spawns turned away because the process was at its bound.
    pub refused_total: u64,
    /// Watcher deaths survived — poll failures and post-mortem unwraps —
    /// where an older engine would have panicked through the crash shield.
    pub degraded_total: u64,
}

/// Read the counters. Cheap; safe from any thread.
pub fn census() -> FsWatchCensus {
    FsWatchCensus {
        open: OPEN.load(Ordering::Relaxed),
        limit: limit(),
        created_total: CREATED_TOTAL.load(Ordering::Relaxed),
        refused_total: REFUSED_TOTAL.load(Ordering::Relaxed),
        degraded_total: DEGRADED_TOTAL.load(Ordering::Relaxed),
    }
}

/// Count one watcher death survived. Called from the crash shield's hook when
/// it decides a panic belongs to a watcher rather than to the engine.
pub fn note_degraded() {
    DEGRADED_TOTAL.fetch_add(1, Ordering::Relaxed);
}

/// Does this panic belong to a filesystem watcher rather than to the engine?
///
/// Two shapes reach production:
///
/// - the poll failure itself — raised ON notify's dedicated loop thread, whose
///   name starts `"notify-rs "` in every backend, so the thread answers it
///   regardless of crate version or source layout;
/// - the follow-up unwraps — after that thread is gone, notify's `Drop` /
///   `watch` / `unwatch` unwrap sends into the dead loop's channel, on
///   whichever thread dropped or touched the watcher. Those carry no telltale
///   thread, only a location inside the notify crate's sources
///   (`…/notify-<version>/src/<backend>.rs`), matched here against registry
///   and vendored layouts. A digit after `notify-` keeps a user project
///   literally named "notify-things" from silencing its own bugs.
///
/// True means: log loudly, count a degradation, keep the engine up.
pub fn is_watcher_panic(thread_name: Option<&str>, location_file: &str) -> bool {
    if let Some(name) = thread_name
        && name.starts_with("notify-rs ")
    {
        return true;
    }
    match location_file.find("/notify-") {
        Some(start) => {
            location_file[start + "/notify-".len()..].starts_with(|c: char| c.is_ascii_digit())
                && location_file.contains("/src/")
        }
        None => false,
    }
}

/// Why [`FsWatcher::spawn`] did not produce a watching watcher.
#[derive(Debug)]
pub enum WatcherSpawnError {
    /// The process is at [`limit()`]. This one root falls back to repair-tick
    /// coverage; nothing else is affected.
    BudgetExhausted { open: usize, limit: usize },
    /// notify could not build the watcher at all.
    Create(notify::Error),
    /// The watcher exists but installing the watch failed — the shape the
    /// services already treated as fine (a `.git` that is not there yet).
    Watch(notify::Error),
}

impl std::fmt::Display for WatcherSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BudgetExhausted { open, limit } => {
                write!(
                    f,
                    "filesystem-watcher budget exhausted ({open}/{limit} held)"
                )
            }
            Self::Create(err) => write!(f, "watcher create failed: {err}"),
            Self::Watch(err) => write!(f, "watch failed: {err}"),
        }
    }
}

impl std::error::Error for WatcherSpawnError {}

/// A live filesystem watch that counts against the engine's budget.
///
/// Holds the underlying [`notify::RecommendedWatcher`] and gives its slot
/// back on drop. Drop also swallows the panic notify's own `Drop` raises when
/// the watcher's event loop died earlier (see [`is_watcher_panic`]): closing
/// an entry whose watcher already died must release the slot quietly, not
/// unwind through whoever holds the entry.
pub struct FsWatcher {
    inner: Option<notify::RecommendedWatcher>,
}

impl std::fmt::Debug for FsWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsWatcher")
            .field("holding", &self.inner.is_some())
            .finish()
    }
}

impl FsWatcher {
    /// Build a watcher AND install its first watch, counting one slot only
    /// when both succeeded. Everything that can fail fails before the slot is
    /// kept, so a caller that gets an `Ok` is holding exactly one counted,
    /// watching watcher.
    pub fn spawn<F>(
        path: &Path,
        mode: notify::RecursiveMode,
        on_event: F,
    ) -> Result<Self, WatcherSpawnError>
    where
        F: notify::EventHandler + Send + 'static,
    {
        let limit = limit();
        // Reserve first: two threads racing at the boundary may overshoot the
        // check by one, never the count by more than they actually hold.
        if OPEN.fetch_add(1, Ordering::Relaxed) >= limit {
            OPEN.fetch_sub(1, Ordering::Relaxed);
            REFUSED_TOTAL.fetch_add(1, Ordering::Relaxed);
            return Err(WatcherSpawnError::BudgetExhausted {
                open: census().open,
                limit,
            });
        }
        let mut watcher = match notify::recommended_watcher(on_event) {
            Ok(watcher) => watcher,
            Err(err) => {
                OPEN.fetch_sub(1, Ordering::Relaxed);
                return Err(WatcherSpawnError::Create(err));
            }
        };
        use notify::Watcher as _;
        match watcher.watch(path, mode) {
            Ok(()) => {
                CREATED_TOTAL.fetch_add(1, Ordering::Relaxed);
                Ok(Self {
                    inner: Some(watcher),
                })
            }
            Err(err) => {
                OPEN.fetch_sub(1, Ordering::Relaxed);
                Err(WatcherSpawnError::Watch(err))
            }
        }
    }

    /// False once the underlying watcher was taken (or never arrived) — a
    /// probe for tests and diagnostics, not a lifecycle verb.
    #[doc(hidden)]
    pub fn is_holding(&self) -> bool {
        self.inner.is_some()
    }
}

impl Drop for FsWatcher {
    fn drop(&mut self) {
        let Some(watcher) = self.inner.take() else {
            return;
        };
        OPEN.fetch_sub(1, Ordering::Relaxed);
        drop_quietly(watcher);
    }
}

/// Drop `value`, swallowing a panic it raises on the way down.
///
/// If this watcher's event-loop thread died earlier, notify's own `Drop`
/// unwraps a send into that dead loop's channel and panics. The panic has
/// already been classified and counted by the crash shield's hook (or merely
/// printed, headed); here it must not propagate into whatever task is closing
/// the entry — an unwind out of a reconcile or a board reclaim costs more
/// than the dead watch it is cleaning up.
fn drop_quietly<T>(value: T) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(value)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    // Serializes the tests that touch global counters or install panic hooks;
    // unit tests run on parallel threads and these read absolute values.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn locked() -> MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn quiet_panic_hook() -> impl Drop {
        type Hook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send>;
        let previous: Hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        struct Restore(std::sync::Mutex<Option<Hook>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                if let Some(hook) = self
                    .0
                    .get_mut()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                {
                    std::panic::set_hook(hook);
                }
            }
        }
        Restore(std::sync::Mutex::new(Some(previous)))
    }

    #[test]
    fn classification_reads_the_two_real_shapes() {
        // Shape 1: the poll failure, raised on notify's own loop thread — the
        // exact line in the incident report.
        assert!(is_watcher_panic(
            Some("notify-rs inotify loop"),
            "/anywhere/main.rs"
        ));
        assert!(is_watcher_panic(
            Some("notify-rs kqueue loop"),
            "/anywhere/main.rs"
        ));
        // Shape 2: the follow-up unwraps, on a caller thread, located in the
        // notify crate's sources — registry and vendored layouts both.
        assert!(is_watcher_panic(
            Some("tokio-runtime-worker"),
            "/Users/x/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/notify-7.0.0/src/inotify.rs"
        ));
        assert!(is_watcher_panic(
            None,
            "/srv/comet/vendor/notify-7.0.0/src/kqueue.rs"
        ));
        // Engine code panics stay fatal.
        assert!(!is_watcher_panic(
            Some("tokio-runtime-worker"),
            "/home/dev/comet/crates/engine/src/spaces.rs"
        ));
        // …including from a directory that merely contains "notify" in a name.
        assert!(!is_watcher_panic(
            Some("main"),
            "/home/me/notify-things/src/main.rs"
        ));
        assert!(!is_watcher_panic(Some("main"), ""));
    }

    #[test]
    fn spawn_counts_and_drop_gives_back() {
        let _serial = locked();
        let tmp = tempfile::tempdir().unwrap();
        let before = census();
        let watcher = FsWatcher::spawn(
            tmp.path(),
            notify::RecursiveMode::NonRecursive,
            |event: Result<notify::Event, notify::Error>| {
                let _ = event;
            },
        )
        .expect("spawn under budget");
        let during = census();
        assert_eq!(during.open, before.open + 1);
        assert_eq!(during.created_total, before.created_total + 1);
        assert_eq!(during.refused_total, before.refused_total);
        assert!(watcher.is_holding());
        drop(watcher);
        let after = census();
        assert_eq!(after.open, before.open);
    }

    #[test]
    fn spawn_refuses_at_the_bound_and_reports_it() {
        let _serial = locked();
        let tmp = tempfile::tempdir().unwrap();
        // Park the counter at the bound directly rather than filling it with
        // ~96 real watchers (each an fd and a thread): the arithmetic under
        // test is the reserve, not the kernel.
        let limit = limit();
        let original = OPEN.load(Ordering::Relaxed);
        OPEN.store(limit, Ordering::Relaxed);
        let refused_before = census().refused_total;
        let err = FsWatcher::spawn(
            tmp.path(),
            notify::RecursiveMode::NonRecursive,
            |event: Result<notify::Event, notify::Error>| {
                let _ = event;
            },
        )
        .expect_err("at the bound");
        OPEN.store(original, Ordering::Relaxed);
        let (open, cap) = match err {
            WatcherSpawnError::BudgetExhausted { open, limit } => (open, limit),
            other => panic!("expected budget refusal, got {other}"),
        };
        assert_eq!(cap, limit);
        assert!(open <= cap, "the refusal reports what was held");
        assert_eq!(census().refused_total, refused_before + 1);
        assert_eq!(census().open, original, "a refused spawn keeps no slot");
    }

    /// The gh#552 second shape: a watcher whose event-loop thread already
    /// died panics inside notify's own `Drop` (an unwrap into the dead
    /// channel). Closing such a watcher must release its slot quietly.
    #[test]
    fn dropping_a_watcher_whose_loop_died_is_quiet() {
        struct LoopAlreadyDead;
        impl Drop for LoopAlreadyDead {
            fn drop(&mut self) {
                panic!("called `Result::unwrap()` on an `Err` value: channel closed");
            }
        }
        let _serial = locked();
        let _quiet = quiet_panic_hook();
        drop_quietly(LoopAlreadyDead); // must not unwind
        // And the same swallow wraps the real thing end to end:
        let tmp = tempfile::tempdir().unwrap();
        let before = census().open;
        let watcher = FsWatcher::spawn(
            tmp.path(),
            notify::RecursiveMode::NonRecursive,
            |event: Result<notify::Event, notify::Error>| {
                let _ = event;
            },
        )
        .expect("spawn under budget");
        drop(watcher);
        assert_eq!(census().open, before);
    }
}
