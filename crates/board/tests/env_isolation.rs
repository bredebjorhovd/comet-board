//! gh#190: a test's board is the directory it made.
//!
//! `COMET_BOARD_CONFIG_DIR` / `COMET_BOARD_STATE_DIR` are set in the
//! environment of every board-dispatched agent — which is the environment
//! `cargo test` runs in on this box — and `Paths::under` used to let them
//! overrule the directory it was handed. A test that built its paths that way
//! took its tempdir, had it ignored, and ran against the live board: reading
//! the real queue, writing a hand-edited `routing.toml`, and logging into
//! `syncd.log` (gh#162 found two such tests; the log said there were more).
//!
//! This binary runs with both variables set to a directory nothing may touch,
//! and fails if any board file resolves there. `crates/engine/tests/
//! board_env_isolation.rs` is the same assertion with a board loop actually
//! running; `no_other_resolution_reads_the_environment` below is the one that
//! catches the *next* test, by refusing the resolution rather than the symptom.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use comet_board::config::{CONFIG_DIR_ENV, Paths, STATE_DIR_ENV};

const ISOLATED_CASE_ENV: &str = "COMET_BOARD_ENV_ISOLATION_CASE";
const ISOLATED_CASE_COMPLETE: &str = ".isolated-case-complete";

/// Run one assertion in a child test process whose environment is poisoned
/// before it starts. The parent owns the poison directory and never mutates
/// its own process-wide environment.
fn under_poisoned_environment(test_name: &str, check: impl FnOnce(&Path)) {
    if std::env::var_os(ISOLATED_CASE_ENV).as_deref() == Some(OsStr::new(test_name)) {
        let config = std::env::var_os(CONFIG_DIR_ENV).expect("child config dir");
        let state = std::env::var_os(STATE_DIR_ENV).expect("child state dir");
        assert_eq!(config, state, "poisoned directories differ");
        let poisoned = Path::new(&config);
        check(poisoned);
        std::fs::write(poisoned.join(ISOLATED_CASE_COMPLETE), "complete\n")
            .expect("record isolated test completion");
        return;
    }

    let poisoned = tempfile::Builder::new()
        .prefix("comet-board-poison-")
        .tempdir()
        .expect("poison dir");
    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", test_name, "--nocapture"])
        .env(ISOLATED_CASE_ENV, test_name)
        .env(CONFIG_DIR_ENV, poisoned.path())
        .env(STATE_DIR_ENV, poisoned.path())
        .output()
        .expect("run isolated test");

    assert!(
        output.status.success(),
        "isolated test {test_name} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        poisoned.path().join(ISOLATED_CASE_COMPLETE).is_file(),
        "isolated test {test_name} did not run"
    );
}

/// Everything under `dir`, recursively — empty is the assertion.
fn contents(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(contents(&path));
        } else {
            found.push(path);
        }
    }
    found
}

/// The environment-sensitive assertions run outside this test process, so
/// calling them cannot disturb unrelated libtest threads.
#[test]
fn poisoned_environment_checks_leave_the_test_process_unchanged() {
    let config_before = std::env::var_os(CONFIG_DIR_ENV);
    let state_before = std::env::var_os(STATE_DIR_ENV);

    under_a_poisoned_environment_paths_stay_in_the_directory_they_were_given();
    discover_is_where_the_environment_is_still_honoured();

    assert_eq!(std::env::var_os(CONFIG_DIR_ENV), config_before);
    assert_eq!(std::env::var_os(STATE_DIR_ENV), state_before);
}

/// The exit criterion, at the level the whole suite builds its paths.
#[test]
fn under_a_poisoned_environment_paths_stay_in_the_directory_they_were_given() {
    under_poisoned_environment(
        "under_a_poisoned_environment_paths_stay_in_the_directory_they_were_given",
        |poisoned| {
            let dir = tempfile::tempdir().expect("tempdir");
            let paths = Paths::under(dir.path()).expect("board dirs");

            for resolved in [
                paths.config_dir.clone(),
                paths.state_dir.clone(),
                paths.db(),
                paths.routing(),
                paths.env_file(),
                paths.pidfile(),
                paths.logfile(),
            ] {
                assert!(
                    resolved.starts_with(dir.path()),
                    "{} resolved outside the directory the caller named",
                    resolved.display()
                );
                assert!(
                    !resolved.starts_with(poisoned),
                    "{} resolved into the environment's directory",
                    resolved.display()
                );
            }

            // Writing through the accessors is what a test actually does, and
            // creating the dirs is what `under` does on its own — neither may
            // reach the poisoned pair.
            std::fs::write(paths.routing(), "# fixture\n").expect("write routing.toml");
            std::fs::write(paths.logfile(), "fixture\n").expect("write syncd.log");
            let stray = contents(poisoned);
            assert!(
                stray.is_empty(),
                "the environment's board was written to: {stray:?}"
            );
        },
    );
}

/// The variables still mean what they were introduced for: a `comet-board`
/// spawned by git, with no arguments, attaching to the board that dispatched
/// the agent (gh#68). Pinned so the fix above cannot be "read them nowhere",
/// which would leave the askpass helper on the wrong board.
#[test]
fn discover_is_where_the_environment_is_still_honoured() {
    under_poisoned_environment(
        "discover_is_where_the_environment_is_still_honoured",
        |poisoned| {
            let paths = Paths::discover().expect("discover");
            assert_eq!(paths.config_dir.as_path(), poisoned);
            assert_eq!(paths.state_dir.as_path(), poisoned);
        },
    );
}

/// The guard against the next test written the old way: outside the two files
/// that own the pair, nothing in the workspace may name it, and nothing but the
/// CLI's own entry point may call `Paths::discover`.
///
/// A source scan rather than a runtime assertion because the failure it catches
/// is a call that was never made — a new suite resolving its paths through the
/// environment fails here, at the line that did it, instead of quietly logging
/// into somebody's board.
#[test]
fn no_other_resolution_reads_the_environment() {
    // `crates/board/tests/` → repo root.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf();

    // Where the pair is allowed to appear at all: the resolution that owns it,
    // the environment a dispatched agent is handed, and the tests that pin both.
    let names_the_pair = [
        "crates/board/src/config.rs",
        "crates/board/src/git_credentials.rs",
        "crates/board/tests/env_isolation.rs",
        "crates/engine/src/push_credentials.rs",
        "crates/engine/tests/board_env_isolation.rs",
        "crates/engine/tests/dispatched_push.rs",
    ];
    // Where `Paths::discover` may be *called*: a `comet-board` process that was
    // handed no data dir. Everything else knows its own. Doc links to it
    // (`[`Paths::discover`]`) are not calls, which is why the trailing paren is
    // part of the needle.
    let calls_discover = [
        "crates/board/src/config.rs",
        "crates/board/tests/env_isolation.rs",
        "apps/board-cli/src/main.rs",
    ];

    let mut offenders = Vec::new();
    for file in rust_sources(&root) {
        let rel = file
            .strip_prefix(&root)
            .expect("under the root")
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            let n = n + 1;
            if !names_the_pair.contains(&rel.as_str())
                && (line.contains(CONFIG_DIR_ENV) || line.contains(STATE_DIR_ENV))
            {
                offenders.push(format!("{rel}:{n}: names COMET_BOARD_*_DIR"));
            }
            if !calls_discover.contains(&rel.as_str()) && line.contains("Paths::discover(") {
                offenders.push(format!("{rel}:{n}: calls Paths::discover"));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "the board's directories come from the data dir the caller names, never \
         from the environment (gh#190) — use `Paths::under(dir)`:\n  {}",
        offenders.join("\n  ")
    );
}

/// Every `.rs` file in the workspace, skipping build output.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name == "target" || name == ".git" || name == "node_modules" {
                continue;
            }
            found.extend(rust_sources(&path));
        } else if name.ends_with(".rs") {
            found.push(path);
        }
    }
    found
}
