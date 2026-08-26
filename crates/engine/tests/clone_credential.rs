//! gh#316: a clone onto a device carries the board's credential.
//!
//! `CloneRepo` used to run a bare `git clone`, so a private repo authenticated
//! with whatever the *target device's* ambient git config happened to provide.
//! On the box that was a `gh` login somebody had set up by hand; on a box
//! provisioned an hour ago it is nothing at all, and the clone died with a
//! `fatal:` that belonged to the machine rather than to the repo.
//!
//! Which URL is spoken and which credential is offered is decided in
//! `comet_board::git_credentials::clone_urls` and unit-tested there. What needs
//! a real `git` is the other half — that the credential the caller passed
//! actually reaches the clone. A listener that answers `401` the way GitHub does
//! makes git go looking for a password; the `Authorization` header it then sends
//! is proof of what the whole path did.
//!
//! No network and no credential: the "comet-board" the askpass shim reaches is a
//! two-line script, as in `comet_board`'s own `askpass_git` test.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use comet_board::config::Paths;
use comet_board::git_credentials::{install_askpass_shim, push_env, verify_askpass};
use comet_engine::Repos;

/// The token the fake helper prints — what a real installation token would be.
const TOKEN: &str = "ghs_only_a_test";

// ---------------------------------------------------------------------------
// A GitHub-shaped 401
// ---------------------------------------------------------------------------

/// A listener that answers every request with `401` and remembers the
/// `Authorization` headers it was sent.
struct Unauthorized {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
    /// Dropped to stop the accept loop.
    _stop: mpsc::Sender<()>,
}

impl Unauthorized {
    fn start() -> Unauthorized {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (stop, stopped) = mpsc::channel::<()>();
        let recorder = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                // The sender lives on the handle; once it is dropped the test is
                // over and the thread goes with the process.
                if stopped.try_recv() != Err(mpsc::TryRecvError::Empty) {
                    return;
                }
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(match stream.try_clone() {
                    Ok(s) => s,
                    Err(_) => return,
                });
                let mut auth = String::new();
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok() {
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some(value) = line
                        .strip_prefix("Authorization: ")
                        .or_else(|| line.strip_prefix("authorization: "))
                    {
                        auth = value.trim().to_string();
                    }
                    line.clear();
                }
                recorder.lock().unwrap().push(auth);
                let _ = stream.write_all(
                    b"HTTP/1.1 401 Unauthorized\r\n\
                      WWW-Authenticate: Basic realm=\"GitHub\"\r\n\
                      Content-Length: 0\r\n\
                      Connection: close\r\n\r\n",
                );
                let _ = stream.flush();
                // Drain whatever git is still sending so it sees the response
                // rather than a reset.
                let _ = stream.read(&mut [0u8; 64]);
            }
        });
        Unauthorized {
            addr,
            seen,
            _stop: stop,
        }
    }

    /// The URL a clone of `o/r` off this listener authenticates over — the
    /// `x-access-token@` form `push_url` produces, with `http` for a test that
    /// has no certificate.
    fn url(&self) -> String {
        format!("http://x-access-token@{}/o/r.git", self.addr)
    }

    /// The credentials git offered a *password* with, decoded as
    /// `("user", "password")`. The empty-password attempt is dropped: a URL
    /// carrying a username makes git try `user:` once before it asks anybody
    /// for anything.
    fn offered(&self) -> Vec<(String, String)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|h| h.strip_prefix("Basic "))
            .filter_map(decode_basic)
            .filter(|(_, password)| !password.is_empty())
            .collect()
    }
}

/// Enough base64 to read one `user:password` back out of a header.
fn decode_basic(b64: &str) -> Option<(String, String)> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut bits = 0u32;
    let mut have = 0;
    let mut out = Vec::new();
    for c in b64.trim().bytes() {
        if c == b'=' {
            break;
        }
        let v = ALPHABET.iter().position(|&a| a == c)? as u32;
        bits = (bits << 6) | v;
        have += 6;
        if have >= 8 {
            have -= 8;
            out.push((bits >> have) as u8);
        }
    }
    let text = String::from_utf8(out).ok()?;
    let (user, pass) = text.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

// ---------------------------------------------------------------------------
// Scaffolding
// ---------------------------------------------------------------------------

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("comet-gh316-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A stand-in `comet-board` that answers `git-askpass` the way the real one does.
fn fake_board(dir: &Path) -> PathBuf {
    let exe = dir.join("comet-board");
    std::fs::write(
        &exe,
        format!(
            "#!/bin/sh\n\
             [ \"$1\" = git-askpass ] || {{ echo \"unknown subcommand: $1\" >&2; exit 2; }}\n\
             case \"$2\" in *sername*) echo x-access-token ;; *) echo {TOKEN} ;; esac\n"
        ),
    )
    .expect("write");
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    exe
}

async fn git(cwd: &Path, args: &[&str]) {
    // Resolved past the board's own shim directories: run from inside a
    // dispatched chat — where the suite gets run (§gh#561) — PATH's first
    // `git` is the gh#488 guard, which re-stamps the live credential at its
    // exec boundary and decides this test from the shell it was raised in.
    let Some(git) = comet_board::git_credentials::resolve_git(None) else {
        panic!("no real git on this box")
    };
    let output = tokio::process::Command::new(git)
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test")
        .output()
        .await
        .expect("git spawns");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repo with one commit, to serve as the origin a clone is made from.
async fn origin_repo(dir: &Path) {
    std::fs::create_dir_all(dir).expect("repo dir");
    git(dir, &["init", "-b", "main"]).await;
    std::fs::write(dir.join("README.md"), "hello\n").expect("write README");
    git(dir, &["add", "."]).await;
    git(dir, &["commit", "-m", "initial"]).await;
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// The exit criterion. Nothing on this device can authenticate — the
/// credential helper is disabled by the environment the board passes, and the
/// repo is private as far as the server is concerned — and the clone still
/// offers the board's credential.
#[tokio::test]
async fn a_clone_onto_a_device_offers_the_boards_credential() {
    let dir = scratch("credential");
    let shim = install_askpass_shim(&dir.join("bin"), &fake_board(&dir)).expect("shim");
    // What the engine does before handing a run its credential, and here for the
    // second reason it does it (gh#385): the shim and the "comet-board" under it
    // were both written by this test binary a moment ago, and git's own exec of
    // them cannot be retried by anyone. A successful exec first proves the
    // inodes have no writers left. See `verify_askpass`.
    verify_askpass(
        &shim,
        &Paths {
            config_dir: dir.join("config"),
            state_dir: dir.join("state"),
        },
    )
    .expect("the path answers before git is asked to use it");
    let repos = Repos::with_worktrees_root(&dir.join("data"), "device-test", dir.join("worktrees"));

    let server = Unauthorized::start();
    let url = server.url();
    let err = repos
        .clone_repo(&url, &url, &push_env(&shim, "o/r"))
        .await
        .expect_err("a server that answers 401 to everything cannot be cloned");

    assert_eq!(
        server.offered(),
        vec![("x-access-token".to_string(), TOKEN.to_string())],
        "the clone never sent the board's credential; git said: {err}"
    );
    // And the device's own git identity was never what decided it: a box with no
    // `gh` login and no keys is the case this exists for.
    assert!(
        !format!("{err}").contains("cannot exec"),
        "git could not exec the helper: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other half of the same call: a clone that needs no credential still
/// lands where `CloneRepo` has always put it — `{data_dir}/repos/<name>` — with
/// the remote a human would have typed.
#[tokio::test]
async fn a_clone_lands_in_the_devices_repo_folder_under_the_repos_name() {
    let dir = scratch("lands");
    let origin = dir.join("origin");
    origin_repo(&origin).await;
    let repos = Repos::with_worktrees_root(&dir.join("data"), "device-test", dir.join("worktrees"));

    let url = format!("file://{}/", origin.display());
    let canonical = format!("file://{}", origin.display());
    let repo = repos
        .clone_repo(
            &url,
            &canonical,
            &[("GIT_TERMINAL_PROMPT".into(), "0".into())],
        )
        .await
        .expect("clone lands");

    assert_eq!(
        repo.path,
        dir.join("data")
            .join("repos")
            .join("origin")
            .to_string_lossy()
    );
    assert!(Path::new(&repo.path).join("README.md").exists());
    assert!(
        repos.list().await.iter().any(|r| r.path == repo.path),
        "a clone joins the device's known repos"
    );

    // Cloning it a second time is refused rather than half-done over the top.
    let err = repos
        .clone_repo(&url, &canonical, &[])
        .await
        .expect_err("the second clone is refused");
    assert!(format!("{err}").contains("Already exists"), "{err}");

    let _ = std::fs::remove_dir_all(&dir);
}
