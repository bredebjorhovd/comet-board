//! Host confinement for repository-owned checkout preparation.
//!
//! This is an internal seam of `checkout_prep`: callers ask that module to run
//! one bounded step and do not learn which host adapter enforces it. macOS uses
//! Seatbelt through `sandbox-exec`; Linux uses bubblewrap. Both expose the same
//! view: an approved, Git-materialized source tree is read-only; only the
//! engine-owned runtime directories behind declared output symlinks are
//! writable. Git metadata and the mutable checkout are absent.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub(crate) struct SandboxedCommand {
    pub(crate) command: tokio::process::Command,
    /// The adapter creates a process group whose id is the spawned child's pid.
    pub(crate) establishes_process_group: bool,
}

/// Build (but do not spawn) the confined command.
pub(crate) fn command(
    script: &str,
    worktree: &Path,
    runtime_root: &Path,
) -> Result<SandboxedCommand, String> {
    #[cfg(debug_assertions)]
    if test_process() && std::env::var_os("COMET_TEST_ASSUME_OUTER_SANDBOX").is_some() {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(script);
        return Ok(SandboxedCommand {
            command,
            establishes_process_group: false,
        });
    }

    #[cfg(target_os = "macos")]
    {
        macos_command(script, worktree, runtime_root)
    }
    #[cfg(target_os = "linux")]
    {
        linux_command(script, worktree, runtime_root)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (script, worktree, runtime_root);
        Err(
            "repository setup/archive is disabled on this host because Comet has no filesystem sandbox adapter for it"
                .to_string(),
        )
    }
}

#[cfg(debug_assertions)]
fn test_process() -> bool {
    std::env::current_exe().ok().is_some_and(|path| {
        path.components()
            .any(|part| part.as_os_str() == std::ffi::OsStr::new("deps"))
    })
}

/// Read-only roots a setup command needs in addition to the approved source.
/// They are deliberately structural/tool roots, never `$HOME`: credentials
/// become reachable only through an explicit `[[link]]` projection.
fn read_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for root in [
        "/bin",
        "/sbin",
        "/usr",
        "/lib",
        "/lib64",
        "/System",
        "/Library/Developer",
        "/Applications/Xcode.app",
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/opt/homebrew/Cellar",
        "/opt/homebrew/opt",
        "/opt/homebrew/Library",
        "/etc/ssl",
        "/etc/pki",
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/nsswitch.conf",
        "/etc/passwd",
        "/etc/group",
        "/etc/gitconfig",
    ] {
        let path = PathBuf::from(root);
        if path.exists() {
            roots.insert(path);
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(path) = std::env::var_os("PATH") {
        for entry in std::env::split_paths(&path) {
            if entry.is_dir() && allowed_path_root(&entry, home.as_deref()) {
                roots.insert(entry);
            }
        }
    }
    // rustup is executable/toolchain material. Cargo's package cache is not
    // shared: run_step points CARGO_HOME at the confined runtime instead, so a
    // setup cannot read credentials.toml or poison another checkout's cache.
    let rustup = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rustup")));
    if let Some(rustup) = rustup.filter(|path| path.is_dir()) {
        roots.insert(rustup);
    }
    roots.into_iter().collect()
}

/// PATH is an execution search path, not blanket permission to read arbitrary
/// per-user directories. Rustup's shims are the one narrow home-relative
/// exception: their toolchains are separately mounted read-only below, while
/// Cargo credentials remain outside `.cargo/bin`.
fn allowed_path_root(path: &Path, home: Option<&Path>) -> bool {
    if !path.is_absolute() {
        return false;
    }
    match home {
        Some(home) if path.starts_with(home) => path == home.join(".cargo/bin"),
        _ => true,
    }
}

#[cfg(target_os = "macos")]
fn macos_command(
    script: &str,
    worktree: &Path,
    runtime_root: &Path,
) -> Result<SandboxedCommand, String> {
    let mut reads = read_roots();
    reads.push(worktree.to_path_buf());
    reads.push(runtime_root.to_path_buf());
    let read_rules = reads
        .iter()
        .map(|path| format!("(subpath \"{}\")", seatbelt_path(path)))
        .collect::<Vec<_>>()
        .join(" ");
    let profile = format!(
        "(version 1)\n\
         (deny default)\n\
         (allow process*)\n\
         (allow signal (target self))\n\
         (allow sysctl-read)\n\
         (allow file-read-metadata)\n\
         (allow file-read* {read_rules} (literal \"/dev/null\") \
            (literal \"/dev/random\") (literal \"/dev/urandom\"))\n\
         (allow file-write* (subpath \"{}\") \
            (literal \"/dev/null\"))\n\
         (allow network*)\n\
         (allow mach-lookup \
            (global-name \"com.apple.system.opendirectoryd.libinfo\") \
            (global-name \"com.apple.system.opendirectoryd.membership\") \
            (global-name \"com.apple.cfprefsd.agent\") \
            (global-name \"com.apple.mDNSResponder\"))",
        seatbelt_path(runtime_root),
    );
    let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
    command
        .arg("-p")
        .arg(profile)
        .arg("/bin/sh")
        .arg("-c")
        .arg(script);
    Ok(SandboxedCommand {
        command,
        establishes_process_group: false,
    })
}

#[cfg(target_os = "macos")]
fn seatbelt_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(target_os = "linux")]
fn linux_command(
    script: &str,
    worktree: &Path,
    runtime_root: &Path,
) -> Result<SandboxedCommand, String> {
    let bwrap = find_program("bwrap").ok_or_else(|| {
        "repository setup/archive requires bubblewrap (`bwrap`) on Linux; install it before retrying"
            .to_string()
    })?;
    let mut command = tokio::process::Command::new(bwrap);
    command.args([
        "--die-with-parent",
        "--new-session",
        "--unshare-all",
        "--share-net",
        "--tmpfs",
        "/",
    ]);
    let reads = read_roots();
    add_parent_dirs(
        &mut command,
        reads
            .iter()
            .map(PathBuf::as_path)
            .chain([worktree, runtime_root]),
    );
    command.args([
        "--dir", "/proc", "--dir", "/dev", "--proc", "/proc", "--dev", "/dev",
    ]);
    for root in reads {
        command.arg("--ro-bind").arg(&root).arg(&root);
    }
    command
        .arg("--ro-bind")
        .arg(worktree)
        .arg(worktree)
        .arg("--bind")
        .arg(runtime_root)
        .arg(runtime_root)
        .arg("--chdir")
        .arg(worktree)
        .arg("/bin/sh")
        .arg("-c")
        .arg(script);
    Ok(SandboxedCommand {
        command,
        // --new-session calls setsid(2). Making bwrap a process-group leader
        // before it starts would make that call fail with EPERM.
        establishes_process_group: true,
    })
}

#[cfg(target_os = "linux")]
fn add_parent_dirs<'a>(
    command: &mut tokio::process::Command,
    paths: impl Iterator<Item = &'a Path>,
) {
    let mut parents = BTreeSet::new();
    for path in paths {
        if path.is_dir() {
            parents.insert(path.to_path_buf());
        }
        let mut cursor = path.parent();
        while let Some(parent) = cursor {
            if parent != Path::new("/") {
                parents.insert(parent.to_path_buf());
            }
            cursor = parent.parent();
        }
    }
    let mut parents = parents.into_iter().collect::<Vec<_>>();
    parents.sort_by_key(|path| path.components().count());
    for parent in parents {
        command.arg("--dir").arg(parent);
    }
}

#[cfg(target_os = "linux")]
fn find_program(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_roots_never_grants_the_users_home() {
        let roots = read_roots();
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            assert!(!roots.iter().any(|root| root == &home));
        }
    }

    #[test]
    fn path_does_not_turn_a_users_tool_directory_into_home_read_access() {
        let home = Path::new("/Users/operator");
        assert!(allowed_path_root(
            Path::new("/Users/operator/.cargo/bin"),
            Some(home)
        ));
        assert!(!allowed_path_root(
            Path::new("/Users/operator/bin"),
            Some(home)
        ));
        assert!(!allowed_path_root(
            Path::new("/Users/operator/.local/bin"),
            Some(home)
        ));
        assert!(allowed_path_root(Path::new("/usr/local/bin"), Some(home)));
        assert!(!allowed_path_root(Path::new("relative/bin"), Some(home)));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bubblewrap_starts_a_real_confined_session() {
        let source = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let mut sandboxed = linux_command("test ! -e .git", source.path(), runtime.path()).unwrap();
        let argv = format!("{:?}", sandboxed.command.as_std());
        let output = sandboxed.command.output().await.unwrap();
        assert!(
            output.status.success(),
            "bubblewrap smoke failed\nargv: {argv}\nstderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
