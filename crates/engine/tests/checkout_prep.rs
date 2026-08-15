//! gh#422 — a checkout is not ready until the repository's recipe says it is.
//!
//! The unit tests in `checkout_prep.rs` cover the recipe's grammar and the
//! refusals it makes on the *spelling* of a path. These cover the half that
//! needs a real filesystem and a real process: what preparation writes, what it
//! runs, what it kills, and what it refuses to do to a checkout.

use std::path::{Path, PathBuf};
use std::time::Duration;

use comet_engine::checkout_prep::{CheckoutPrep, PrepState, PrepareRequest, RECIPE_PATH};
use comet_harness::CancellationToken;
use tempfile::TempDir;

/// A data dir, a repo dir, and a checkout with the given recipe in it.
struct Box_ {
    _tmp: TempDir,
    data: PathBuf,
    repo: PathBuf,
    worktree: PathBuf,
}

fn a_box(recipe: Option<&str>) -> Box_ {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join("data");
    let repo = tmp.path().join("src").join("widget");
    let worktree = tmp.path().join("worktrees").join("widget").join("board-gh-1");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    if let Some(text) = recipe {
        let path = worktree.join(RECIPE_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    Box_ {
        _tmp: tmp,
        data,
        repo,
        worktree,
    }
}

impl Box_ {
    fn prep(&self) -> CheckoutPrep {
        CheckoutPrep::new(&self.data)
    }

    fn request(&self) -> PrepareRequest<'_> {
        PrepareRequest {
            worktree: &self.worktree,
            repo_path: &self.repo,
            force: false,
            cancel: None,
        }
    }

    /// Put a machine-local file where a `[[link]]`'s `from` can reach it.
    fn local(&self, name: &str, contents: &str, mode: u32) -> PathBuf {
        let root = self.prep().locals_root(&self.repo);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join(name);
        std::fs::write(&path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        let _ = mode;
        path
    }
}

fn set_recipe(b: &Box_, text: &str) {
    let path = b.worktree.join(RECIPE_PATH);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[tokio::test]
async fn a_repository_with_no_recipe_is_ready_and_leaves_no_state() {
    // The behaviour every repository has today has to survive unchanged, and
    // it has to be free: a box full of recipe-less repos must not grow a state
    // directory per checkout.
    let b = a_box(None);
    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Ready);
    assert!(record.command.is_none());
    assert!(!b.prep().state_dir(&b.worktree).exists());
}

#[tokio::test]
async fn a_setup_that_succeeds_leaves_ready_with_its_output() {
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"echo prepared-the-thing\"\n[run]\nrun = \"cargo run\"\n",
    ));
    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Ready, "{:?}", record.detail);
    assert_eq!(record.exit_code, Some(0));
    assert_eq!(record.command.as_deref(), Some("echo prepared-the-thing"));
    // The canonical run command rides on the record, so a viewport holding the
    // status also holds the offer.
    assert_eq!(record.run_command.as_deref(), Some("cargo run"));

    let log = std::fs::read_to_string(record.log.as_ref().expect("a log path")).unwrap();
    assert!(log.contains("$ echo prepared-the-thing"), "{log}");
    assert!(log.contains("prepared-the-thing"), "{log}");

    // And it survives the call: the record is a property of the checkout.
    let read_back = b.prep().status(&b.worktree).expect("persisted");
    assert_eq!(read_back.state, PrepState::Ready);
}

#[tokio::test]
async fn a_setup_that_fails_is_failed_with_the_exit_code_and_the_output() {
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"echo 'no such toolchain' >&2; exit 3\"\n",
    ));
    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Failed);
    assert_eq!(record.exit_code, Some(3));
    assert!(record.detail.as_deref().unwrap().contains("exited 3"));
    // stderr is in the same file as the command that produced it — a setup log
    // read for "what went wrong" is unusable split across two.
    let log = std::fs::read_to_string(record.log.as_ref().unwrap()).unwrap();
    assert!(log.contains("no such toolchain"), "{log}");
    // The checkout is preserved. A failed preparation is a thing you go and
    // look at, not a thing that deletes the evidence.
    assert!(b.worktree.exists());
}

#[tokio::test]
async fn a_malformed_recipe_fails_rather_than_reads_as_absent() {
    let b = a_box(Some("version = 1\n[setup]\nrunn = \"scripts/setup.sh\"\n"));
    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Failed);
    assert!(record.detail.as_deref().unwrap().contains("runn"));
}

#[tokio::test]
async fn a_setup_past_its_budget_is_killed_with_everything_it_started() {
    // The bound is on the process *group*: killing the `sh` and leaving the
    // sleep behind is the failure this exists to prevent, so the test asserts
    // on the grandchild rather than on the shell.
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"sh -c 'sleep 60' & echo $! > child.pid; wait\"\ntimeout = \"1s\"\n",
    ));
    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Failed);
    assert!(
        record.detail.as_deref().unwrap().contains("1s budget"),
        "{:?}",
        record.detail
    );

    #[cfg(unix)]
    {
        let pid: i32 = std::fs::read_to_string(b.worktree.join("child.pid"))
            .expect("the script recorded its grandchild")
            .trim()
            .parse()
            .unwrap();
        // The kill is asynchronous to us; give the group a moment to go.
        for _ in 0..50 {
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("pid {pid} outlived the setup that started it");
    }
}

#[tokio::test]
async fn a_cancelled_setup_stops_and_says_so() {
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"sleep 60\"\ntimeout = \"1h\"\n",
    ));
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        token.cancel();
    });
    let record = b
        .prep()
        .prepare(PrepareRequest {
            cancel: Some(cancel),
            ..b.request()
        })
        .await;
    assert_eq!(record.state, PrepState::Failed);
    assert!(record.detail.as_deref().unwrap().contains("cancelled"));
}

#[tokio::test]
async fn a_ready_checkout_is_not_prepared_twice_until_the_recipe_changes() {
    // What makes a retry cheap, and what makes an edited recipe take effect
    // without anybody remembering to force it.
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"echo one >> ran.txt\"\n",
    ));
    let prep = b.prep();
    prep.prepare(b.request()).await;
    prep.prepare(b.request()).await;
    let ran = std::fs::read_to_string(b.worktree.join("ran.txt")).unwrap();
    assert_eq!(ran.lines().count(), 1, "the second visit re-ran setup");

    // Forced: the escape hatch after a half-finished setup.
    prep.prepare(PrepareRequest {
        force: true,
        ..b.request()
    })
    .await;
    let ran = std::fs::read_to_string(b.worktree.join("ran.txt")).unwrap();
    assert_eq!(ran.lines().count(), 2);

    // Edited: the digest moved, so it prepares again on its own.
    set_recipe(&b, "version = 1\n[setup]\nrun = \"echo two >> ran.txt\"\n");
    prep.prepare(b.request()).await;
    let ran = std::fs::read_to_string(b.worktree.join("ran.txt")).unwrap();
    assert_eq!(ran.lines().count(), 3);
    assert!(ran.contains("two"));
}

#[tokio::test]
async fn a_failed_checkout_is_prepared_again_on_the_next_visit() {
    // Only `ready` short-circuits. A retry of a failed preparation must
    // actually retry — that is the whole recovery path.
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"echo attempt >> tries.txt; exit 1\"\n",
    ));
    let prep = b.prep();
    assert_eq!(prep.prepare(b.request()).await.state, PrepState::Failed);
    assert_eq!(prep.prepare(b.request()).await.state, PrepState::Failed);
    let tries = std::fs::read_to_string(b.worktree.join("tries.txt")).unwrap();
    assert_eq!(tries.lines().count(), 2);
}

#[tokio::test]
async fn a_link_projects_from_the_boxs_own_directory_with_its_mode() {
    let b = a_box(Some(
        "version = 1\n[[link]]\nfrom = \"dev.env\"\nto = \"config/.env.local\"\n",
    ));
    b.local("dev.env", "TOKEN=hunter2\n", 0o600);
    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Ready, "{:?}", record.detail);

    let dest = b.worktree.join("config/.env.local");
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "TOKEN=hunter2\n");
    // The credential reach is on the record, before the agent starts.
    let outcome = &record.links[0];
    assert_eq!(outcome.result, "copied");
    assert_eq!(outcome.to, "config/.env.local");
    #[cfg(unix)]
    assert_eq!(outcome.mode.as_deref(), Some("0600"), "a 0600 source must not land 0644");
}

#[tokio::test]
async fn a_link_never_clobbers_what_the_checkout_already_has() {
    let b = a_box(Some(
        "version = 1\n[[link]]\nfrom = \"dev.env\"\nto = \".env.local\"\n",
    ));
    b.local("dev.env", "FROM_THE_BOX\n", 0o600);
    std::fs::write(b.worktree.join(".env.local"), "MINE\n").unwrap();
    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Ready);
    assert_eq!(
        std::fs::read_to_string(b.worktree.join(".env.local")).unwrap(),
        "MINE\n"
    );
    assert_eq!(record.links[0].result, "kept");
}

#[tokio::test]
async fn a_link_whose_source_the_operator_has_not_filled_in_is_a_note_not_a_refusal() {
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"true\"\n[[link]]\nfrom = \"dev.env\"\nto = \".env.local\"\n",
    ));
    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Ready, "{:?}", record.detail);
    assert_eq!(record.links[0].result, "missing");
    assert!(!b.worktree.join(".env.local").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlink_in_the_locals_directory_is_refused_rather_than_followed() {
    // The rooted `from` is only a boundary if nothing inside the root can point
    // back out of it. One `ln -s ~/.ssh/id_ed25519` would otherwise undo it.
    let b = a_box(Some(
        "version = 1\n[[link]]\nfrom = \"dev.env\"\nto = \".env.local\"\n",
    ));
    let secret = b._tmp.path().join("id_ed25519");
    std::fs::write(&secret, "PRIVATE KEY\n").unwrap();
    let root = b.prep().locals_root(&b.repo);
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&secret, root.join("dev.env")).unwrap();

    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Failed);
    assert!(
        record.detail.as_deref().unwrap().contains("not a regular file"),
        "{:?}",
        record.detail
    );
    assert!(!b.worktree.join(".env.local").exists());
}

#[tokio::test]
async fn setup_runs_from_the_worktree_root_with_the_recipes_environment() {
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"pwd > where.txt; echo $WIDGET_MODE $COMET_WORKTREE > env.txt\"\n[env]\nWIDGET_MODE = \"dev\"\n",
    ));
    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Ready, "{:?}", record.detail);
    let where_ = std::fs::read_to_string(b.worktree.join("where.txt")).unwrap();
    assert_eq!(
        Path::new(where_.trim()),
        std::fs::canonicalize(&b.worktree).unwrap()
    );
    let env = std::fs::read_to_string(b.worktree.join("env.txt")).unwrap();
    assert!(env.starts_with("dev "), "{env}");
    assert!(env.contains("board-gh-1"), "{env}");
}

#[tokio::test]
async fn a_parked_brief_is_released_exactly_once() {
    // The recovery contract: a retry that succeeds releases the work the
    // failure held back, and two racing retries cannot both release it.
    let b = a_box(None);
    let prep = b.prep();
    prep.park(&b.worktree, "{\"brief\":\"go\"}").unwrap();
    assert_eq!(
        prep.take_parked(&b.worktree).as_deref(),
        Some("{\"brief\":\"go\"}")
    );
    assert!(prep.take_parked(&b.worktree).is_none());
}

#[tokio::test]
async fn archive_runs_the_repositorys_own_cleanup() {
    let b = a_box(Some(
        "version = 1\n[archive]\nrun = \"rm -rf build-output\"\n",
    ));
    std::fs::create_dir_all(b.worktree.join("build-output")).unwrap();
    let said = b.prep().archive(&b.worktree, None).await.expect("ran");
    assert!(said.contains("succeeded"), "{said}");
    assert!(!b.worktree.join("build-output").exists());

    // A repo with no `[archive]` says nothing, and the caller sweeps as before.
    let plain = a_box(Some("version = 1\n"));
    assert!(plain.prep().archive(&plain.worktree, None).await.is_none());
}

#[tokio::test]
async fn forgetting_a_checkout_removes_its_record() {
    let b = a_box(Some("version = 1\n[setup]\nrun = \"true\"\n"));
    let prep = b.prep();
    prep.prepare(b.request()).await;
    assert!(prep.status(&b.worktree).is_some());
    prep.forget(&b.worktree);
    assert!(prep.status(&b.worktree).is_none());
}
