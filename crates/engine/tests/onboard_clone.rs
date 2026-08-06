//! gh#97: the clone half of `comet-board onboard`.
//!
//! The decisions — what a slug is, where the checkout goes, what to do about a
//! directory that already exists — are unit-tested in
//! `comet_board::onboard`. What needs a real git and a real disk is what
//! `Repos::clone_to` promises beyond `clone_repo`: the caller names the path,
//! the credential rides in the environment, the checkout keeps the remote a
//! human would have typed, and a clone that fails leaves nothing behind.
//!
//! That last one is the load-bearing case. A half-written directory would read
//! as an existing checkout on the next attempt and be *reused* — onboarding
//! would then report success over a broken clone, which is precisely the
//! half-onboarded state the verb exists to remove.

use std::path::Path;

use comet_engine::Repos;

async fn git(cwd: &Path, args: &[&str]) {
    let output = tokio::process::Command::new("git")
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

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("comet-gh97-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

#[tokio::test]
async fn a_clone_lands_where_it_was_told_and_keeps_the_canonical_remote() {
    let dir = scratch("clone");
    let origin = dir.join("origin");
    origin_repo(&origin).await;
    let repos = Repos::with_worktrees_root(&dir.join("data"), "device-test", dir.join("worktrees"));

    // The URL that authenticates is not the URL the checkout should keep: an
    // installation-token clone goes through `x-access-token@…`, and a checkout
    // outlives the clone that made it.
    let target = dir.join("dev").join("widget");
    let canonical = format!("file://{}", origin.display());
    let repo = repos
        .clone_to(
            &format!("file://{}", origin.display()),
            &canonical,
            &target,
            &[("GIT_TERMINAL_PROMPT".into(), "0".into())],
        )
        .await
        .expect("clone lands");

    assert_eq!(repo.path, target.to_string_lossy());
    assert!(target.join("README.md").exists(), "the work tree is there");
    // The parent was created for it: `--dir ~/dev/widget` on a box with no
    // `~/dev` should not be an error about a missing directory.
    assert!(dir.join("dev").is_dir());
    assert_eq!(
        repos.origin_url(&target).await.as_deref(),
        Some(canonical.as_str()),
        "the checkout keeps the remote a human would have typed"
    );
    // Registered, so the desktop's repo list shows it like any other.
    assert!(
        repos.list().await.iter().any(|r| r.path == repo.path),
        "an onboarded clone joins the device's known repos"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_clone_that_fails_leaves_nothing_to_be_mistaken_for_a_checkout() {
    let dir = scratch("failed");
    let repos = Repos::with_worktrees_root(&dir.join("data"), "device-test", dir.join("worktrees"));
    let target = dir.join("dev").join("nothing");

    let url = format!("file://{}", dir.join("no-such-repo").display());
    let err = repos
        .clone_to(
            &url,
            &url,
            &target,
            &[("GIT_TERMINAL_PROMPT".into(), "0".into())],
        )
        .await
        .expect_err("cloning a repo that is not there fails");
    assert!(format!("{err}").contains("git"), "{err}");
    // The whole point: a leftover directory would read as an existing checkout
    // on the next attempt and be reused, reporting success over a broken clone.
    assert!(
        !target.exists(),
        "a failed clone must leave nothing behind at {}",
        target.display()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_occupied_target_is_refused_before_git_is_asked() {
    let dir = scratch("occupied");
    let origin = dir.join("origin");
    origin_repo(&origin).await;
    let repos = Repos::with_worktrees_root(&dir.join("data"), "device-test", dir.join("worktrees"));

    let target = dir.join("dev").join("taken");
    std::fs::create_dir_all(&target).expect("target dir");
    std::fs::write(target.join("notes.txt"), "somebody's files\n").expect("write notes");

    let url = format!("file://{}", origin.display());
    let err = repos
        .clone_to(
            &url,
            &url,
            &target,
            &[("GIT_TERMINAL_PROMPT".into(), "0".into())],
        )
        .await
        .expect_err("cloning over an occupied directory is refused");
    assert!(format!("{err}").contains("Already exists"), "{err}");
    assert!(
        target.join("notes.txt").exists(),
        "the refusal must not have touched what was there"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `origin_url` is what decides between "reuse this checkout" and "refuse
/// this directory", so it has to answer `None` for a folder that is not a
/// checkout rather than erroring into the reuse path.
#[tokio::test]
async fn origin_url_answers_none_for_something_that_is_not_a_checkout() {
    let dir = scratch("origin");
    let repos = Repos::with_worktrees_root(&dir.join("data"), "device-test", dir.join("worktrees"));

    let plain = dir.join("plain");
    std::fs::create_dir_all(&plain).expect("plain dir");
    assert_eq!(repos.origin_url(&plain).await, None);
    assert_eq!(repos.origin_url(&dir.join("absent")).await, None);

    // A repo with no `origin` at all is not a checkout of anything we can name.
    let no_remote = dir.join("no-remote");
    origin_repo(&no_remote).await;
    assert_eq!(repos.origin_url(&no_remote).await, None);

    let _ = std::fs::remove_dir_all(&dir);
}
