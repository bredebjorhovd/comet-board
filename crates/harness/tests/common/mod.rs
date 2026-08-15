//! One timing knob for the harness integration suites.
//!
//! Every fixed deadline in these tests covers work that is *not* the thing
//! under test: spawning a real child, connecting to it, tearing it down. A
//! margin that looks like 7.5x locally is much thinner on a shared 2-core CI
//! runner that is also building a workspace this size, which is how gh#167
//! happened — the same commit timed out on two different tests in
//! `opencode.rs`, both at the suite's fixed 15s ceiling.
//!
//! So the deadlines derive from here instead of being sprinkled literals:
//! `COMET_TEST_TIMEOUT_SCALE` (default 1, raised in `ci.yml`) stretches all of
//! them at once. Setting it *small* is the other half of the point — that is
//! how you force a timeout on demand and check that the failure message says
//! something a reader can act on, which a green run can never prove.

// Each test binary compiles this module and uses a subset of it.
#![allow(dead_code)]

use std::sync::OnceLock;
use std::time::Duration;

const SCALE_VAR: &str = "COMET_TEST_TIMEOUT_SCALE";

/// The deadline multiplier, read once per test binary.
///
/// A malformed value is a hard error rather than a silent fall back to 1: a
/// typo in CI that quietly restores the old deadlines would reproduce gh#167
/// while looking configured.
pub fn timeout_scale() -> f64 {
    static SCALE: OnceLock<f64> = OnceLock::new();
    *SCALE.get_or_init(|| {
        let raw = match std::env::var(SCALE_VAR) {
            Ok(raw) => raw,
            Err(_) => return 1.0,
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return 1.0;
        }
        let scale: f64 = trimmed
            .parse()
            .unwrap_or_else(|_| panic!("{SCALE_VAR}={raw:?} is not a number"));
        assert!(
            scale.is_finite() && scale > 0.0,
            "{SCALE_VAR}={raw:?} must be a positive, finite number"
        );
        scale
    })
}

/// `base` stretched by [`timeout_scale`].
pub fn scaled(base: Duration) -> Duration {
    base.mul_f64(timeout_scale())
}

/// How a scaled deadline was arrived at, for failure messages — it is the
/// difference between "the runner was slower than we allowed for" and "the
/// scale never reached this process".
pub fn deadline_note(base: Duration) -> String {
    format!(
        "{:?} ({base:?} x {SCALE_VAR}={})",
        scaled(base),
        timeout_scale()
    )
}

/// The events a stream managed to produce before its deadline, numbered.
///
/// A timeout report is really about what is *missing* from this list, so it
/// has to show the list.
pub fn events_so_far<T: std::fmt::Debug>(events: &[T]) -> String {
    if events.is_empty() {
        return "    (none — the stream produced nothing at all)\n".to_string();
    }
    events
        .iter()
        .enumerate()
        .map(|(i, ev)| format!("    {i:>3}. {ev:?}\n"))
        .collect()
}

/// A real executable boundary that records argv before chaining to a harness
/// fixture. This keeps adapter integration tests outside private command
/// builders: the assertion sees exactly what the spawned process received.
#[cfg(unix)]
pub fn recording_wrapper(
    target: &std::path::Path,
    dir: &tempfile::TempDir,
) -> (std::path::PathBuf, std::path::PathBuf) {
    fn quote(path: &std::path::Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    let wrapper = dir.path().join("record-argv.sh");
    let record = dir.path().join("argv.txt");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n: > {record}\nfor arg do printf '%s\\n' \"$arg\" >> {record}; done\nexec {target} \"$@\"\n",
            record = quote(&record),
            target = quote(target),
        ),
    )
    .expect("write argv wrapper");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
        .expect("make argv wrapper executable");
    (wrapper, record)
}

pub fn board_mcp_server() -> comet_proto::McpServer {
    comet_proto::McpServer {
        name: "comet-board".into(),
        command: "comet-board".into(),
        args: vec!["mcp".into()],
    }
}
