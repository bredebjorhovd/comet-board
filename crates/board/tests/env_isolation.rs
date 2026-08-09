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

use std::path::{Path, PathBuf};

use comet_board::config::{CONFIG_DIR_ENV, Paths, STATE_DIR_ENV};

/// The poisoned pair, set for the whole binary before any test runs.
///
/// Test binaries are multi-threaded and `set_var` is process-wide, so this
/// happens once, from `poison`, guarded by a `Once` — never per test.
fn poison() -> PathBuf {
    static ONCE: std::sync::Once = std::sync::Once::new();
    let dir = std::env::temp_dir().join(format!("comet-board-poison-{}", std::process::id()));
    ONCE.call_once(|| {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("poison dir");
        // SAFETY: single-threaded here (a `Once` before any test spawns a
        // thread), and every test in this binary wants these set.
        unsafe {
            std::env::set_var(CONFIG_DIR_ENV, &dir);
            std::env::set_var(STATE_DIR_ENV, &dir);
        }
    });
    dir
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

/// The exit criterion, at the level the whole suite builds its paths.
#[test]
fn under_a_poisoned_environment_paths_stay_in_the_directory_they_were_given() {
    let poisoned = poison();
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
            !resolved.starts_with(&poisoned),
            "{} resolved into the environment's directory",
            resolved.display()
        );
    }

    // Writing through the accessors is what a test actually does, and creating
    // the dirs is what `under` does on its own — neither may reach the pair.
    std::fs::write(paths.routing(), "# fixture\n").expect("write routing.toml");
    std::fs::write(paths.logfile(), "fixture\n").expect("write syncd.log");
    let stray = contents(&poisoned);
    assert!(
        stray.is_empty(),
        "the environment's board was written to: {stray:?}"
    );
}

/// The variables still mean what they were introduced for: a `comet-board`
/// spawned by git, with no arguments, attaching to the board that dispatched
/// the agent (gh#68). Pinned so the fix above cannot be "read them nowhere",
/// which would leave the askpass helper on the wrong board.
#[test]
fn discover_is_where_the_environment_is_still_honoured() {
    let poisoned = poison();
    let paths = Paths::discover().expect("discover");
    assert_eq!(paths.config_dir, poisoned);
    assert_eq!(paths.state_dir, poisoned);
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
