//! gh#72: reclaiming an attempt's checkout — and, crucially, the branch under
//! it.
//!
//! `delete_worktree` used to delete a branch only when it was named `comet/…`,
//! which is what the frontend's own worktrees are called. The board's are named
//! by `routing.toml`'s `branch_template` (`board/…` by default), so every
//! dispatched branch survived every deletion, forever. The rule is now "the
//! branch its creator vouches for, while the checkout is still on it" — with
//! the user's own checkout still off limits.

use std::path::Path;

use comet_engine::Repos;

async fn git(cwd: &Path, args: &[&str]) -> String {
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
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repo with one commit on `main`, and a `Repos` rooted beside it.
async fn repo(root: &Path) -> (std::path::PathBuf, Repos) {
    let dir = root.join("widget");
    std::fs::create_dir_all(&dir).expect("repo dir");
    git(&dir, &["init", "-b", "main"]).await;
    git(&dir, &["commit", "--allow-empty", "-m", "root"]).await;
    let repos = Repos::with_worktrees_root(root, "device-test", root.join("worktrees"));
    (dir, repos)
}

/// The exit criterion: after collection the checkout is gone, the branch is
/// gone, and the registration with them.
#[tokio::test]
async fn a_dispatched_checkout_and_its_board_branch_are_both_reclaimed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (dir, repos) = repo(tmp.path()).await;
    let worktree = repos
        .create_worktree_on(&dir, "board/gh-72-widget", "HEAD")
        .await
        .expect("dispatch cuts a worktree");
    // The agent worked in it.
    git(
        Path::new(&worktree.path),
        &["commit", "--allow-empty", "-m", "agent work"],
    )
    .await;

    repos
        .delete_worktree(&dir, Path::new(&worktree.path), Some(&worktree.branch))
        .await
        .expect("reclaim");

    assert!(!Path::new(&worktree.path).exists(), "checkout removed");
    let branches = repos.branches(&dir).await.expect("branches");
    assert!(
        !branches.contains(&"board/gh-72-widget".to_string()),
        "the board's branch goes with its checkout: {branches:?}"
    );
    assert!(
        !git(&dir, &["worktree", "list", "--porcelain"])
            .await
            .contains(&worktree.path),
        "and the registration is pruned"
    );
}

/// The guard the `comet/` test was standing in for: an operator who checked
/// their own branch out inside the worktree keeps it. The directory is still
/// reclaimed — it is the board's — but the branch in it is not the board's to
/// delete.
#[tokio::test]
async fn a_branch_the_operator_checked_out_is_left_alone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (dir, repos) = repo(tmp.path()).await;
    let worktree = repos
        .create_worktree_on(&dir, "board/gh-72-widget", "HEAD")
        .await
        .expect("dispatch cuts a worktree");
    git(
        Path::new(&worktree.path),
        &["checkout", "-b", "brede/my-own-thing"],
    )
    .await;

    repos
        .delete_worktree(&dir, Path::new(&worktree.path), Some(&worktree.branch))
        .await
        .expect("reclaim");

    let branches = repos.branches(&dir).await.expect("branches");
    assert!(
        branches.contains(&"brede/my-own-thing".to_string()),
        "the operator's branch survives: {branches:?}"
    );
    // The board's own branch is left too — nothing was checked out on it, so
    // this deletion had no claim to make about it either way.
    assert!(branches.contains(&"board/gh-72-widget".to_string()));
}

/// A checkout somebody deleted by hand still leaks its branch. There is nothing
/// left to ask what it was on, so the caller's word is what there is — and git
/// still refuses to delete a branch checked out somewhere else.
#[tokio::test]
async fn a_hand_deleted_checkout_still_gives_up_its_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (dir, repos) = repo(tmp.path()).await;
    let worktree = repos
        .create_worktree_on(&dir, "board/gh-72-widget", "HEAD")
        .await
        .expect("dispatch cuts a worktree");
    std::fs::remove_dir_all(&worktree.path).expect("hand-deleted");

    repos
        .delete_worktree(&dir, Path::new(&worktree.path), Some(&worktree.branch))
        .await
        .expect("reclaim");

    let branches = repos.branches(&dir).await.expect("branches");
    assert!(
        !branches.contains(&"board/gh-72-widget".to_string()),
        "the branch goes even with no checkout to read: {branches:?}"
    );
}

/// No vouched-for branch, no branch deletion — the frontend's `DeleteWorktree`
/// passes `None`, and comet's own `comet/…` naming is what it relies on.
#[tokio::test]
async fn without_a_vouched_branch_only_comets_own_naming_counts() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (dir, repos) = repo(tmp.path()).await;
    let board = repos
        .create_worktree_on(&dir, "board/gh-72-widget", "HEAD")
        .await
        .expect("board worktree");
    let comet = repos
        .create_worktree(&dir, "main")
        .await
        .expect("comet cut");

    repos
        .delete_worktree(&dir, Path::new(&board.path), None)
        .await
        .expect("reclaim board");
    repos
        .delete_worktree(&dir, Path::new(&comet.path), None)
        .await
        .expect("reclaim comet");

    let branches = repos.branches(&dir).await.expect("branches");
    assert!(
        branches.contains(&"board/gh-72-widget".to_string()),
        "nobody vouched for it: {branches:?}"
    );
    assert!(
        !branches.contains(&comet.branch),
        "comet's own branch still goes: {branches:?}"
    );
}
