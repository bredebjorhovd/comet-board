//! The credential path, exercised by the thing that actually uses it (gh#233).
//!
//! gh#58 shipped `GIT_ASKPASS='<path>' git-askpass` on the documented belief
//! that git runs an askpass command with a space in it through
//! `sh -c '<cmd> "$@"'`. It does not — that is `credential.helper`'s rule, not
//! this one. git execs `GIT_ASKPASS` directly, as one filename with the prompt
//! as its single argument, so the whole string was taken as a path, no such
//! file existed, and every dispatched push that reached the helper failed with
//! `cannot exec`. The unit tests passed throughout, because what they asserted
//! was the shape of a string.
//!
//! So this asserts against `git` instead. A listener that answers `401` the way
//! GitHub does makes git go looking for a password; what arrives in the
//! `Authorization` header is proof of what the whole path did — that git could
//! exec what it was given, that the helper's subcommand survived, and that the
//! answer came back on the pipe git was holding.
//!
//! No network and no credential: the "comet-board" the shim reaches is a
//! two-line script, because the part under test is everything around the mint.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use comet_board::config::Paths;
use comet_board::git_credentials::{install_askpass_shim, push_env, verify_askpass};

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
                // The sender lives on the handle; once it is dropped the test
                // is over and the thread goes with the process.
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
                // GitHub's own shape: a challenge git will answer by asking
                // its askpass helper for a password.
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

    fn url(&self) -> String {
        format!("http://x-access-token@{}/o/r.git", self.addr)
    }

    /// The credentials git offered a *password* with, decoded as
    /// `("user", "password")`.
    ///
    /// The empty-password attempt is dropped: a URL carrying a username makes
    /// git try `user:` once before it asks anybody for anything, so that
    /// header proves only that git read the URL. What the helper produced is
    /// the attempt with something in it.
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
    let dir = std::env::temp_dir().join(format!("comet-askpass-git-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// A stand-in `comet-board` that answers `git-askpass` the way the real one
/// does. Deliberately at a path with a space in it: the box that first hit
/// gh#233 runs its binaries out of an app directory, and "a space in the path"
/// is the case the old quoting existed to serve.
fn fake_board(dir: &Path) -> PathBuf {
    let exe = dir.join("comet board");
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

/// `git ls-remote` against the listener, with the environment a dispatched
/// push gets. Answers git's own stderr.
fn ls_remote(url: &str, env: &[(String, String)], cwd: &Path) -> String {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("ls-remote").arg(url).current_dir(cwd);
    for (key, value) in env {
        cmd.env(key, value);
    }
    // Whatever the box's own git is configured with must not decide this test.
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd.env("GIT_CONFIG_SYSTEM", "/dev/null");
    let out = cmd.output().expect("run git");
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// The exit criterion, at the layer that failed: a push that needs a password
/// gets the board's, through `comet-board git-askpass`, with no wrapper
/// anywhere.
#[test]
fn a_push_authenticates_through_the_boards_askpass_helper() {
    if !have_git() {
        eprintln!("no git on this box — skipping");
        return;
    }
    let dir = scratch("works");
    let board = fake_board(&dir);
    let shim = install_askpass_shim(&dir.join("bin"), &board).expect("shim");
    let paths = Paths {
        config_dir: dir.join("config"),
        state_dir: dir.join("state"),
    };
    verify_askpass(&shim, &paths).expect("the path answers before git is asked to use it");

    let server = Unauthorized::start();
    let stderr = ls_remote(&server.url(), &push_env(&shim, "o/r"), &dir);

    // git got the credential and sent it. That single assertion covers the
    // whole of gh#233: it can only hold if git could exec what GIT_ASKPASS
    // named, if the `git-askpass` subcommand reached the binary, and if the
    // answer came back on stdout.
    assert_eq!(
        server.offered(),
        vec![("x-access-token".to_string(), TOKEN.to_string())],
        "git never sent the board's credential; git said: {stderr}"
    );
    // And the failure mode that started this must not be anywhere in it.
    assert!(
        !stderr.contains("cannot exec"),
        "git could not exec the helper: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The regression itself, kept as a test rather than as a sentence in a commit
/// message: the form gh#58 shipped cannot work, on this git or any other.
///
/// It is the same helper, reachable and correct, named the way `GIT_ASKPASS`
/// was named before this change. git takes the whole string as a filename.
#[test]
fn the_old_quoted_subcommand_form_never_reaches_the_helper() {
    if !have_git() {
        eprintln!("no git on this box — skipping");
        return;
    }
    let dir = scratch("oldform");
    let board = fake_board(&dir);

    let server = Unauthorized::start();
    let old_form = format!("'{}' git-askpass", board.display());
    let env = vec![
        ("GIT_ASKPASS".to_string(), old_form.clone()),
        ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
    ];
    let stderr = ls_remote(&server.url(), &env, &dir);

    assert!(
        server.offered().is_empty(),
        "the old form authenticated after all: {stderr}"
    );
    assert!(
        stderr.contains("cannot exec"),
        "expected git to fail exec'ing {old_form}, got: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The other exit criterion. A deliberately broken helper does not quietly
/// become somebody else's credential: the check refuses it before a run is
/// given it, with the reason, and git — handed it anyway — sends nothing.
#[test]
fn a_broken_helper_authenticates_with_nothing_at_all() {
    if !have_git() {
        eprintln!("no git on this box — skipping");
        return;
    }
    let dir = scratch("broken");
    // Installed, executable, and pointed at a binary that is not there — the
    // shape of a box where the payload shipped the engine alone.
    let shim = install_askpass_shim(&dir.join("bin"), &dir.join("comet-board")).expect("shim");
    let err = verify_askpass(
        &shim,
        &Paths {
            config_dir: dir.join("config"),
            state_dir: dir.join("state"),
        },
    )
    .expect_err("a helper reaching nothing verified")
    .to_string();
    assert!(err.contains("askpass helper"), "{err}");

    let server = Unauthorized::start();
    let stderr = ls_remote(&server.url(), &push_env(&shim, "o/r"), &dir);
    assert!(
        server.offered().is_empty(),
        "something answered for a helper that cannot run: {stderr}"
    );
    // And it fails rather than blocking on a terminal the dispatch has not got.
    assert!(
        stderr.contains("terminal prompts disabled") || stderr.contains("could not read Password"),
        "{stderr}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
