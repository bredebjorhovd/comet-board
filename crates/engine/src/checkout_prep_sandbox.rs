//! Host confinement for repository-owned checkout preparation.
//!
//! This is an internal seam of `checkout_prep`: callers ask that module to run
//! one bounded step and do not learn which host adapter enforces it. macOS uses
//! Seatbelt through `sandbox-exec`; Linux uses bubblewrap. Both expose the same
//! view: the checkout and the step runtime are writable, system/toolchain and
//! Git metadata roots are read-only, every other host path is absent/denied,
//! and network remains available for dependency installation.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Build (but do not spawn) the confined command.
pub(crate) fn command(
    script: &str,
    worktree: &Path,
    runtime_root: &Path,
) -> Result<tokio::process::Command, String> {
    #[cfg(debug_assertions)]
    if test_process() && std::env::var_os("COMET_TEST_ASSUME_OUTER_SANDBOX").is_some() {
        let mut command = tokio::process::Command::new("sh");
        command.arg("-c").arg(script);
        return Ok(command);
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

/// Read-only roots a setup command needs in addition to the checkout. They are
/// deliberately structural/tool roots, never `$HOME`: credentials become
/// reachable only through an explicit `[[link]]` copied into the checkout.
fn read_roots(worktree: &Path) -> Vec<PathBuf> {
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
    if let Ok(common) = git_common_dir(worktree) {
        roots.insert(common);
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

fn git_common_dir(worktree: &Path) -> Result<PathBuf, String> {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-C",
            &worktree.to_string_lossy(),
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ])
        .output()
        .map_err(|error| format!("locating Git metadata for preparation sandbox: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "locating Git metadata for preparation sandbox: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

#[cfg(target_os = "macos")]
fn macos_command(
    script: &str,
    worktree: &Path,
    runtime_root: &Path,
) -> Result<tokio::process::Command, String> {
    let mut reads = read_roots(worktree);
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
         (allow file-write* (subpath \"{}\") (subpath \"{}\") \
            (literal \"/dev/null\"))\n\
         (allow network*)\n\
         (allow mach-lookup \
            (global-name \"com.apple.system.opendirectoryd.libinfo\") \
            (global-name \"com.apple.system.opendirectoryd.membership\") \
            (global-name \"com.apple.cfprefsd.agent\") \
            (global-name \"com.apple.mDNSResponder\"))",
        seatbelt_path(worktree),
        seatbelt_path(runtime_root),
    );
    let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
    command
        .arg("-p")
        .arg(profile)
        .arg("/bin/sh")
        .arg("-c")
        .arg(script);
    Ok(command)
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
) -> Result<tokio::process::Command, String> {
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
        "--proc",
        "/proc",
        "--dev",
        "/dev",
    ]);
    for root in read_roots(worktree) {
        command.arg("--ro-bind").arg(&root).arg(&root);
    }
    command
        .arg("--bind")
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
    Ok(command)
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
        let roots = read_roots(Path::new("."));
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
}
