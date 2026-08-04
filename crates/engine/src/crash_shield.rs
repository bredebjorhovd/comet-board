//! Crash shield — feature-inventory §3.1 "crash shield (drain+exit)".
//!
//! Rust's default when a `tokio::spawn`ed task panics is to unwind THAT task
//! and carry on. On an always-on box that is the worst possible outcome: the
//! process stays up holding its data-dir lock, its IPC port and every open doc,
//! while whatever the dead task owned — a run's stream pump, a room socket, the
//! per-chat command drain — is silently gone. The chat's row reads "Working"
//! forever, no error is ever written, and nobody is home to notice.
//!
//! So a panic anywhere is treated as fatal. The hook logs it (location +
//! payload) and hands off to a watcher task that runs the normal drain —
//! settle live runs so their streaming entries are stamped `aborted` with a
//! visible note, kill PTYs, flush snapshots — then exits non-zero so
//! launchd/systemd restarts a clean process. Boot recovery picks the runs back
//! up from the journal from there.
//!
//! Everything here is bounded, because the drain may well touch whatever just
//! panicked (a poisoned lock, a wedged executor): a shield that hangs is no
//! better than no shield. The drain gets [`DRAIN_DEADLINE`]; independently, a
//! plain OS thread — no runtime, nothing to wedge it — force-exits at
//! [`HARD_DEADLINE`] whatever the async side is doing.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::future::BoxFuture;

/// How long the graceful drain gets before we stop waiting for it.
const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Backstop for a drain that never even reaches its own deadline (the runtime
/// itself is wedged). Enforced from an OS thread, so it cannot be starved.
const HARD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// Exit code on a shielded panic — Rust's own code for a panicking process, so
/// service managers and `$?` readers see nothing unusual.
const EXIT_CODE: i32 = 101;

/// The teardown the shield runs before exiting. Detached by construction: it
/// must not borrow the engine, because the panic may have happened inside it.
pub type Drain = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// Install the process-wide panic shield. Chains onto the existing hook (so the
/// default backtrace print survives), then drains and exits.
///
/// Idempotent per process: a second call is ignored, and a panic RAISED BY THE
/// DRAIN re-enters the hook and is dropped rather than recursing.
///
/// Must be called from inside a tokio runtime — the watcher lives there.
pub fn install(drain: Drain) {
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".into());
        let payload = panic_message(info);
        tracing::error!(location = %location, payload = %payload, "PANIC — crash shield engaged");
        // `try_send` on a 1-slot channel: the first panic wins and every
        // later one (including any raised by the drain) is a no-op.
        let _ = tx.try_send(format!("{payload} ({location})"));
    }));

    tokio::spawn(async move {
        let Some(reason) = rx.recv().await else {
            return; // sender gone — the hook outlives the process, so unreachable
        };
        tracing::error!(reason = %reason, "crash shield: draining before exit");
        // The hard backstop is armed BEFORE the drain starts: if the runtime
        // is too broken to honour the timeout below, this thread still exits.
        std::thread::spawn(|| {
            std::thread::sleep(HARD_DEADLINE);
            eprintln!("comet: crash shield drain hung; exiting");
            std::process::exit(EXIT_CODE);
        });
        match tokio::time::timeout(DRAIN_DEADLINE, drain()).await {
            Ok(()) => tracing::error!("crash shield: drained; exiting"),
            Err(_) => tracing::error!("crash shield: drain timed out; exiting anyway"),
        }
        std::process::exit(EXIT_CODE);
    });
}

fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    payload_message(info.payload())
}

/// The human-readable half of a panic payload. `&str` (a literal `panic!`) and
/// `String` (a formatted one, and everything `unwrap`/`expect` raise) are the
/// only shapes worth reading; anything else is opaque by construction.
fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".into()
}

// NOTE: `install` itself is deliberately untested here — it replaces the
// PROCESS-wide panic hook and arms an exit path, so exercising it inside a
// shared test binary would take the whole run down with the first
// `#[should_panic]` test. Its behaviour is verified by the shape of what it
// hands to `payload_message` below plus the engine's shutdown-path tests.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_message_reads_the_shapes_panics_actually_carry() {
        assert_eq!(payload_message(&"boom"), "boom");
        assert_eq!(payload_message(&String::from("boom 1")), "boom 1");
        assert_eq!(payload_message(&7u32), "<non-string panic payload>");
    }

    #[test]
    fn payload_message_reads_a_real_unwind_payload() {
        // `unwrap` on a None raises a String payload, not a `&str` — the shape
        // most real panics take, and the one the log line has to read through.
        // (The lint below wants the deliberate panic written as `panic!`; that
        // would change the payload to `&str` and test the wrong shape.)
        #[allow(clippy::unnecessary_literal_unwrap)]
        fn always_panics() -> u8 {
            Option::<u8>::None.unwrap()
        }
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {})); // keep the test output clean
        let payload = std::panic::catch_unwind(always_panics).unwrap_err();
        std::panic::set_hook(previous);
        assert!(payload_message(payload.as_ref()).contains("None"));
    }
}
