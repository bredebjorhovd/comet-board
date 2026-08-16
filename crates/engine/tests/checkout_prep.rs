//! gh#422 — a checkout is not ready until the repository's recipe says it is.
//!
//! The unit tests in `checkout_prep.rs` cover the recipe's grammar and the
//! refusals it makes on the *spelling* of a path. These cover the half that
//! needs a real filesystem and a real process: what preparation writes, what it
//! runs, what it kills, and what it refuses to do to a checkout.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use comet_engine::checkout_prep::{CheckoutPrep, PrepState, PrepareRequest, RECIPE_PATH};
use comet_harness::CancellationToken;
use tempfile::TempDir;

/// A data dir, a repo dir, and a checkout with the given recipe in it.
struct Box_ {
    _tmp: TempDir,
    data: PathBuf,
    repository_id: String,
    worktree: PathBuf,
}

fn a_box(recipe: Option<&str>) -> Box_ {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join("data");
    let worktree = tmp
        .path()
        .join("worktrees")
        .join("widget")
        .join("board-gh-1");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&worktree).unwrap();
    git(&worktree, &["init", "-b", "main"]);
    git(&worktree, &["config", "user.email", "prep@test.invalid"]);
    git(&worktree, &["config", "user.name", "Checkout Prep Test"]);
    if let Some(text) = recipe {
        let path = worktree.join(RECIPE_PATH);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    git(&worktree, &["add", "."]);
    git(&worktree, &["commit", "--allow-empty", "-m", "fixture"]);
    let repository_id = format!("test-device:{}", worktree.display());
    Box_ {
        _tmp: tmp,
        data,
        repository_id,
        worktree,
    }
}

impl Box_ {
    fn prep(&self) -> CheckoutPrep {
        CheckoutPrep::new(&self.data)
    }

    fn request(&self) -> PrepareRequest<'_> {
        let _ = self.prep().approve(&self.worktree, &self.repository_id);
        PrepareRequest {
            worktree: &self.worktree,
            repository_id: &self.repository_id,
            force: false,
            cancel: None,
        }
    }

    /// Put a machine-local file where a `[[link]]`'s `from` can reach it.
    fn local(&self, name: &str, contents: &str, mode: u32) -> PathBuf {
        let root = self.prep().locals_root(&self.repository_id);
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
    git(&b.worktree, &["add", RECIPE_PATH]);
    git(&b.worktree, &["commit", "-m", "update recipe"]);
}

fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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
async fn checkout_owned_cancellation_stops_the_active_command_tree() {
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"sleep 60\"\ntimeout = \"1h\"\n",
    ));
    let prep = b.prep();
    prep.approve(&b.worktree, &b.repository_id).unwrap();
    let runner = prep.clone();
    let worktree = b.worktree.clone();
    let repository_id = b.repository_id.clone();
    let task = tokio::spawn(async move {
        runner
            .prepare(PrepareRequest {
                worktree: &worktree,
                repository_id: &repository_id,
                force: true,
                cancel: None,
            })
            .await
    });
    for _ in 0..100 {
        if prep
            .status(&b.worktree)
            .is_some_and(|record| record.state == PrepState::Preparing)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(prep.cancel(&b.worktree));
    let record = task.await.unwrap();
    assert_eq!(record.state, PrepState::Failed);
    assert!(record.detail.as_deref().unwrap().contains("cancelled"));
}

#[tokio::test]
async fn a_dropped_preparation_is_killed_and_reconciled_from_preparing_to_failed() {
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"echo $$ > setup.pid; sleep 60\"\ntimeout = \"1h\"\n",
    ));
    let prep = b.prep();
    prep.approve(&b.worktree, &b.repository_id).unwrap();
    let runner = prep.clone();
    let worktree = b.worktree.clone();
    let repository_id = b.repository_id.clone();
    let task = tokio::spawn(async move {
        runner
            .prepare(PrepareRequest {
                worktree: &worktree,
                repository_id: &repository_id,
                force: true,
                cancel: None,
            })
            .await
    });
    for _ in 0..100 {
        if b.worktree.join("setup.pid").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let pid: i32 = std::fs::read_to_string(b.worktree.join("setup.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    task.abort();
    assert!(task.await.is_err());
    let record = prep
        .status(&b.worktree)
        .expect("orphaned record reconciled");
    assert_eq!(record.state, PrepState::Failed);
    assert!(record.detail.as_deref().unwrap().contains("interrupted"));

    #[cfg(unix)]
    for _ in 0..50 {
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    #[cfg(unix)]
    panic!("pid {pid} outlived its dropped preparation future");
}

#[tokio::test]
async fn cancellation_revokes_parked_work_before_a_late_setup_can_release_it() {
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"sleep 60\"\ntimeout = \"1h\"\n",
    ));
    let prep = b.prep();
    prep.approve(&b.worktree, &b.repository_id).unwrap();
    prep.park(&b.worktree, "run this agent").unwrap();

    let runner = prep.clone();
    let worktree = b.worktree.clone();
    let repository_id = b.repository_id.clone();
    let task = tokio::spawn(async move {
        runner
            .prepare(PrepareRequest {
                worktree: &worktree,
                repository_id: &repository_id,
                force: true,
                cancel: None,
            })
            .await
    });
    for _ in 0..100 {
        if prep
            .status(&b.worktree)
            .is_some_and(|record| record.state == PrepState::Preparing)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // This is the ordering the dispatch cancellation path promises: first
    // make release impossible, then stop preparation.
    prep.revoke_parked(&b.worktree);
    assert!(prep.cancel(&b.worktree));
    let record = task.await.unwrap();
    assert_eq!(record.state, PrepState::Failed);
    assert!(prep.parked(&b.worktree).is_none());
    assert!(prep.claim_parked(&b.worktree).is_none());
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
    assert_eq!(
        outcome.mode.as_deref(),
        Some("0600"),
        "a 0600 source must not land 0644"
    );
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
    let root = b.prep().locals_root(&b.repository_id);
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&secret, root.join("dev.env")).unwrap();

    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Failed);
    assert!(
        record.detail.as_deref().unwrap().contains("symlink"),
        "{:?}",
        record.detail
    );
    assert!(!b.worktree.join(".env.local").exists());
}

#[tokio::test]
async fn repository_code_cannot_execute_before_the_host_approves_its_exact_digest() {
    let b = a_box(None);
    let outside = b._tmp.path().join("host-secret");
    std::fs::write(&outside, "do-not-read\n").unwrap();
    set_recipe(
        &b,
        &format!(
            "version = 1\n[setup]\nrun = \"cp '{}' stolen\"\n",
            outside.display()
        ),
    );
    let record = b
        .prep()
        .prepare(PrepareRequest {
            worktree: &b.worktree,
            repository_id: &b.repository_id,
            force: false,
            cancel: None,
        })
        .await;
    assert_eq!(record.state, PrepState::Failed);
    assert!(record.requires_approval);
    assert!(!b.worktree.join("stolen").exists());

    b.prep()
        .approve(&b.worktree, &b.repository_id)
        .expect("host approval");
    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Ready, "{:?}", record.detail);
    assert_eq!(
        std::fs::read_to_string(b.worktree.join("stolen")).unwrap(),
        "do-not-read\n"
    );

    // A repository edit is a new trust decision, not an inherited approval.
    set_recipe(&b, "version = 1\n[setup]\nrun = \"touch changed\"\n");
    let changed = b
        .prep()
        .prepare(PrepareRequest {
            worktree: &b.worktree,
            repository_id: &b.repository_id,
            force: false,
            cancel: None,
        })
        .await;
    assert!(changed.requires_approval);
    assert!(!b.worktree.join("changed").exists());
}

#[tokio::test]
async fn approval_follows_the_committed_script_not_only_the_recipe_file() {
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"sh scripts/setup.sh\"\n",
    ));
    std::fs::create_dir_all(b.worktree.join("scripts")).unwrap();
    std::fs::write(b.worktree.join("scripts/setup.sh"), "touch original\n").unwrap();
    git(&b.worktree, &["add", "scripts/setup.sh"]);
    git(&b.worktree, &["commit", "-m", "add setup script"]);

    let prep = b.prep();
    let approved_tree = prep.approve(&b.worktree, &b.repository_id).unwrap();

    // The TOML stays byte-for-byte identical. Only what its command opens
    // changes, which was the hole in a recipe-only trust key.
    std::fs::write(b.worktree.join("scripts/setup.sh"), "touch changed\n").unwrap();
    git(&b.worktree, &["add", "scripts/setup.sh"]);
    git(&b.worktree, &["commit", "-m", "change setup script"]);
    let record = prep
        .prepare(PrepareRequest {
            worktree: &b.worktree,
            repository_id: &b.repository_id,
            force: false,
            cancel: None,
        })
        .await;

    assert_eq!(record.state, PrepState::Failed);
    assert!(record.requires_approval);
    assert_ne!(
        record.execution_digest.as_deref(),
        Some(approved_tree.as_str())
    );
    assert!(!b.worktree.join("changed").exists());
}

#[tokio::test]
async fn tracked_edits_cannot_borrow_their_heads_approval() {
    let b = a_box(Some(
        "version = 1\n[setup]\nrun = \"sh scripts/setup.sh\"\n",
    ));
    std::fs::create_dir_all(b.worktree.join("scripts")).unwrap();
    std::fs::write(b.worktree.join("scripts/setup.sh"), "touch original\n").unwrap();
    git(&b.worktree, &["add", "scripts/setup.sh"]);
    git(&b.worktree, &["commit", "-m", "add setup script"]);
    let prep = b.prep();
    prep.approve(&b.worktree, &b.repository_id).unwrap();

    std::fs::write(b.worktree.join("scripts/setup.sh"), "touch dirty\n").unwrap();
    let record = prep
        .prepare(PrepareRequest {
            worktree: &b.worktree,
            repository_id: &b.repository_id,
            force: true,
            cancel: None,
        })
        .await;

    assert_eq!(record.state, PrepState::Failed);
    assert!(
        record
            .detail
            .as_deref()
            .unwrap()
            .contains("tracked files differ")
    );
    assert!(!b.worktree.join("dirty").exists());
}

#[tokio::test]
async fn repository_edits_cannot_broaden_machine_local_file_reach_without_approval() {
    let b = a_box(Some(
        "version = 1\n[[link]]\nfrom = \"signing.env\"\nto = \".env.local\"\n",
    ));
    b.local("signing.env", "TOKEN=host-secret\n", 0o600);

    let record = b
        .prep()
        .prepare(PrepareRequest {
            worktree: &b.worktree,
            repository_id: &b.repository_id,
            force: false,
            cancel: None,
        })
        .await;
    assert_eq!(record.state, PrepState::Failed);
    assert!(record.requires_approval);
    assert!(record.links.is_empty());
    assert!(!b.worktree.join(".env.local").exists());

    b.prep().approve(&b.worktree, &b.repository_id).unwrap();
    let approved = b.prep().prepare(b.request()).await;
    assert_eq!(approved.state, PrepState::Ready);
    assert_eq!(approved.links[0].result, "copied");
    assert_eq!(
        std::fs::read_to_string(b.worktree.join(".env.local")).unwrap(),
        "TOKEN=host-secret\n"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn projection_refuses_symlinked_source_and_destination_ancestors() {
    let source = a_box(Some(
        "version = 1\n[[link]]\nfrom = \"nested/dev.env\"\nto = \".env.local\"\n",
    ));
    let outside = source._tmp.path().join("outside-source");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("dev.env"), "SECRET\n").unwrap();
    let locals = source.prep().locals_root(&source.repository_id);
    std::fs::create_dir_all(&locals).unwrap();
    std::os::unix::fs::symlink(&outside, locals.join("nested")).unwrap();
    let record = source.prep().prepare(source.request()).await;
    assert_eq!(record.state, PrepState::Failed);
    assert!(record.detail.as_deref().unwrap().contains("symlink"));

    let destination = a_box(Some(
        "version = 1\n[[link]]\nfrom = \"dev.env\"\nto = \"config/.env.local\"\n",
    ));
    destination.local("dev.env", "SECRET\n", 0o600);
    let outside = destination._tmp.path().join("outside-destination");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, destination.worktree.join("config")).unwrap();
    let record = destination.prep().prepare(destination.request()).await;
    assert_eq!(record.state, PrepState::Failed);
    assert!(record.detail.as_deref().unwrap().contains("symlink"));
    assert!(!outside.join(".env.local").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn projection_refuses_a_destination_symlink_leaf_without_touching_its_target() {
    let b = a_box(Some(
        "version = 1\n[[link]]\nfrom = \"dev.env\"\nto = \".env.local\"\n",
    ));
    b.local("dev.env", "FROM_THE_BOX\n", 0o600);
    let outside = b._tmp.path().join("outside-env");
    std::fs::write(&outside, "KEEP_ME\n").unwrap();
    std::os::unix::fs::symlink(&outside, b.worktree.join(".env.local")).unwrap();

    let record = b.prep().prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Failed);
    assert!(record.detail.as_deref().unwrap().contains("symlink"));
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "KEEP_ME\n");
}

#[cfg(unix)]
#[tokio::test]
async fn swapping_a_destination_ancestor_cannot_redirect_projection_outside_the_checkout() {
    let mut recipe = "version = 1\n".to_string();
    for index in 0..256 {
        recipe.push_str(&format!(
            "[[link]]\nfrom = \"dev.env\"\nto = \"config/projected-{index}\"\n"
        ));
    }
    let b = a_box(Some(&recipe));
    b.local("dev.env", &"safe\n".repeat(256), 0o600);
    let live = b.worktree.join("config");
    let held = b.worktree.join("config-held-during-swap");
    let outside = b._tmp.path().join("outside-destination-race");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::create_dir_all(&outside).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let swaps = Arc::new(AtomicUsize::new(0));
    let swapper = {
        let live = live.clone();
        let held = held.clone();
        let outside = outside.clone();
        let stop = Arc::clone(&stop);
        let swaps = Arc::clone(&swaps);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if std::fs::rename(&live, &held).is_err() {
                    std::thread::yield_now();
                    continue;
                }
                if std::os::unix::fs::symlink(&outside, &live).is_ok() {
                    swaps.fetch_add(1, Ordering::Release);
                    std::thread::yield_now();
                    let _ = std::fs::remove_file(&live);
                } else if live.is_dir() {
                    let _ = std::fs::remove_dir_all(&live);
                }
                if live.is_dir() {
                    let _ = std::fs::remove_dir_all(&live);
                }
                let _ = std::fs::rename(&held, &live);
            }
            if std::fs::symlink_metadata(&live).is_ok_and(|meta| meta.file_type().is_symlink()) {
                let _ = std::fs::remove_file(&live);
            }
            if held.exists() {
                if live.is_dir() {
                    let _ = std::fs::remove_dir_all(&live);
                }
                let _ = std::fs::rename(&held, &live);
            }
        })
    };
    while swaps.load(Ordering::Acquire) < 10 {
        std::thread::yield_now();
    }

    // Either the descriptor walk wins a component and writes through that
    // already-open directory, or it observes the symlink and refuses. It may
    // never re-resolve the replacement symlink for the eventual create.
    let _record = b.prep().prepare(b.request()).await;
    stop.store(true, Ordering::Release);
    swapper.join().unwrap();
    assert_eq!(
        std::fs::read_dir(&outside).unwrap().count(),
        0,
        "an ancestor swap redirected a projected credential outside the checkout"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn swapping_a_source_ancestor_cannot_import_a_file_outside_the_allowlist() {
    let mut recipe = "version = 1\n".to_string();
    for index in 0..256 {
        recipe.push_str(&format!(
            "[[link]]\nfrom = \"nested/source-{index}\"\nto = \"projected/source-{index}\"\n"
        ));
    }
    let b = a_box(Some(&recipe));
    let locals = b.prep().locals_root(&b.repository_id);
    let live = locals.join("nested");
    let held = locals.join("nested-held-during-swap");
    let outside = b._tmp.path().join("outside-source-race");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    for index in 0..256 {
        std::fs::write(live.join(format!("source-{index}")), "ALLOWLISTED\n").unwrap();
        std::fs::write(outside.join(format!("source-{index}")), "OUTSIDE\n").unwrap();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let swaps = Arc::new(AtomicUsize::new(0));
    let swapper = {
        let live = live.clone();
        let held = held.clone();
        let outside = outside.clone();
        let stop = Arc::clone(&stop);
        let swaps = Arc::clone(&swaps);
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if std::fs::rename(&live, &held).is_err() {
                    std::thread::yield_now();
                    continue;
                }
                std::os::unix::fs::symlink(&outside, &live).unwrap();
                swaps.fetch_add(1, Ordering::Release);
                std::thread::yield_now();
                std::fs::remove_file(&live).unwrap();
                std::fs::rename(&held, &live).unwrap();
            }
        })
    };
    while swaps.load(Ordering::Acquire) < 10 {
        std::thread::yield_now();
    }

    let _record = b.prep().prepare(b.request()).await;
    stop.store(true, Ordering::Release);
    swapper.join().unwrap();
    if let Ok(files) = std::fs::read_dir(b.worktree.join("projected")) {
        for file in files.flatten() {
            assert_eq!(
                std::fs::read_to_string(file.path()).unwrap(),
                "ALLOWLISTED\n",
                "an ancestor swap imported a file outside the allowlisted root"
            );
        }
    }
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
async fn setup_does_not_inherit_engine_credentials_or_ambient_values() {
    const SECRET: &str = "CHECKOUT_PREP_AMBIENT_SECRET_TEST";
    // SAFETY: this unique variable is read only by this test; no other test
    // gives it meaning.
    unsafe { std::env::set_var(SECRET, "must-not-reach-repository-code") };
    let b = a_box(Some(&format!(
        "version = 1\n[setup]\nrun = \"test -z \\\"${{{SECRET}+set}}\\\"\"\n"
    )));
    let record = b.prep().prepare(b.request()).await;
    unsafe { std::env::remove_var(SECRET) };
    assert_eq!(record.state, PrepState::Ready, "{:?}", record.detail);
}

#[tokio::test]
async fn a_parked_brief_is_released_exactly_once() {
    // The recovery contract: a retry that succeeds releases the work the
    // failure held back, and two racing retries cannot both release it.
    let b = a_box(None);
    let prep = b.prep();
    prep.park(&b.worktree, "{\"brief\":\"go\"}").unwrap();
    let claim = prep.claim_parked(&b.worktree).expect("claimed");
    assert_eq!(claim.payload(), "{\"brief\":\"go\"}");
    assert!(
        prep.claim_parked(&b.worktree).is_none(),
        "a racing claimant cannot own the same release"
    );
    drop(claim);
    let claim = prep.claim_parked(&b.worktree).expect("rolled back");
    assert!(claim.deliver(|_| Ok::<_, ()>(())).unwrap());
    assert!(prep.claim_parked(&b.worktree).is_none());
}

#[test]
fn cancellation_after_claim_prevents_the_release_callback() {
    let b = a_box(None);
    let prep = b.prep();
    prep.park(&b.worktree, "run this agent").unwrap();
    let claim = prep.claim_parked(&b.worktree).expect("claimed");

    prep.revoke_parked(&b.worktree);
    let called = AtomicBool::new(false);
    let delivered = claim
        .deliver(|_| {
            called.store(true, Ordering::SeqCst);
            Ok::<_, ()>(())
        })
        .unwrap();

    assert!(!delivered);
    assert!(!called.load(Ordering::SeqCst));
    assert!(prep.parked(&b.worktree).is_none());
    assert!(prep.claim_parked(&b.worktree).is_none());
}

#[tokio::test]
async fn a_fresh_engine_can_recover_a_ready_abandoned_claim() {
    let b = a_box(Some("version = 1\n[setup]\nrun = \"true\"\n"));
    let prep = b.prep();
    prep.park(&b.worktree, "run this agent").unwrap();
    let record = prep.prepare(b.request()).await;
    assert_eq!(record.state, PrepState::Ready);

    let claim = prep
        .claim_parked(&b.worktree)
        .expect("claimed before crash");
    std::mem::forget(claim); // Model the process dying while it owned the claim.

    let restarted = CheckoutPrep::new(&b.data);
    let handoffs = restarted.ready_handoffs();
    assert_eq!(handoffs.len(), 1);
    assert_eq!(handoffs[0].0, std::fs::canonicalize(&b.worktree).unwrap());
    let recovered = restarted.claim_parked(&b.worktree).expect("reclaimed");
    assert_eq!(recovered.payload(), "run this agent");
    assert!(recovered.deliver(|_| Ok::<_, ()>(())).unwrap());
}

#[test]
fn parking_is_create_only_and_never_replaces_recovery_state() {
    let b = a_box(None);
    let prep = b.prep();
    prep.park(&b.worktree, "first").unwrap();
    assert!(prep.park(&b.worktree, "second").is_err());
    assert_eq!(prep.parked(&b.worktree).as_deref(), Some("first"));
}

#[tokio::test]
async fn archive_runs_the_repositorys_own_cleanup() {
    let b = a_box(Some(
        "version = 1\n[archive]\nrun = \"rm -rf build-output\"\n",
    ));
    std::fs::create_dir_all(b.worktree.join("build-output")).unwrap();
    b.prep()
        .approve(&b.worktree, &b.repository_id)
        .expect("approved");
    let said = b
        .prep()
        .archive(&b.worktree, &b.repository_id, None)
        .await
        .expect("ran");
    assert!(said.contains("succeeded"), "{said}");
    assert!(!b.worktree.join("build-output").exists());

    // A repo with no `[archive]` says nothing, and the caller sweeps as before.
    let plain = a_box(Some("version = 1\n"));
    assert!(
        plain
            .prep()
            .archive(&plain.worktree, &plain.repository_id, None)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn an_archive_only_recipe_is_approved_before_the_agent_can_edit_it() {
    let b = a_box(Some(
        "version = 1\n[archive]\nrun = \"rm -rf build-output\"\n",
    ));
    let unapproved = b
        .prep()
        .prepare(PrepareRequest {
            worktree: &b.worktree,
            repository_id: &b.repository_id,
            force: false,
            cancel: None,
        })
        .await;
    assert_eq!(unapproved.state, PrepState::Failed);
    assert!(unapproved.requires_approval);

    b.prep().approve(&b.worktree, &b.repository_id).unwrap();
    let approved = b.prep().prepare(b.request()).await;
    assert_eq!(approved.state, PrepState::Ready);
    assert!(approved.command.is_none());
}

#[tokio::test]
async fn an_unapproved_archive_command_is_never_executed() {
    let b = a_box(Some(
        "version = 1\n[archive]\nrun = \"touch archive-ran\"\n",
    ));
    let said = b
        .prep()
        .archive(&b.worktree, &b.repository_id, None)
        .await
        .expect("archive refusal is visible");
    assert!(said.contains("not approved"), "{said}");
    assert!(!b.worktree.join("archive-ran").exists());
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
