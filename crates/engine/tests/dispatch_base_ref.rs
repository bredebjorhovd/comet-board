//! gh#67: the board's dispatch path cuts its worktree from a *fetched* base
//! ref, not from whatever the space folder was last left on.
//!
//! The failure this pins: an always-on box's space folder sits on a stale main
//! (or on someone's feature branch) for days, and every agent released into it
//! silently starts there. `create_worktree_on` therefore fetches the route's
//! `base` from origin before cutting — and refuses the dispatch when it cannot,
//! because the alternative is a pull request with the wrong diff in it.

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

async fn commit(dir: &Path, message: &str) -> String {
    git(dir, &["commit", "--allow-empty", "-m", message]).await;
    head(dir).await
}

async fn head(dir: &Path) -> String {
    git(dir, &["rev-parse", "HEAD"]).await
}

/// An origin repo on `main` with one commit, and a clone of it. The clone is
/// the "space folder" a route points at.
async fn origin_and_clone(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let origin = root.join("origin");
    std::fs::create_dir_all(&origin).expect("origin dir");
    git(&origin, &["init", "-b", "main"]).await;
    commit(&origin, "root").await;
    let clone = root.join("widget");
    git(
        root,
        &["clone", &origin.to_string_lossy(), &clone.to_string_lossy()],
    )
    .await;
    (origin, clone)
}

fn repos(data_dir: &Path) -> Repos {
    Repos::with_worktrees_root(data_dir, "device-test", data_dir.join("worktrees"))
}

/// The exit condition: a clone whose local main is behind origin — and which is
/// sitting on an unrelated branch, as an always-on box's folder does — still
/// produces a worktree based on the *fetched* `origin/main`.
#[tokio::test]
async fn a_fresh_branch_is_cut_from_fetched_origin_head() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (origin, clone) = origin_and_clone(tmp.path()).await;
    let repos = repos(tmp.path());

    // Origin moves on; the clone knows nothing about it…
    let tip = commit(&origin, "upstream work").await;
    // …and is parked on a feature branch of its own, the way a box that ran
    // something else last week is.
    git(&clone, &["checkout", "-b", "someones-feature"]).await;
    let local = commit(&clone, "unrelated local work").await;

    let worktree = repos
        .create_worktree_on(&clone, "board/gh-67-widget", "origin/HEAD")
        .await
        .expect("dispatch cuts a worktree");

    let cut = head(Path::new(&worktree.path)).await;
    assert_eq!(cut, tip, "the worktree is based on the fetched origin tip");
    assert_ne!(cut, local, "not on the folder's current HEAD");
    assert_eq!(worktree.branch, "board/gh-67-widget");
    // The fetch touched the remote-tracking ref only — the space folder is
    // still on its own branch, untouched.
    assert_eq!(
        git(&clone, &["rev-parse", "--abbrev-ref", "HEAD"]).await,
        "someones-feature"
    );
}

/// A `base` naming a branch explicitly, with and without the `origin/` prefix.
#[tokio::test]
async fn an_explicit_base_branch_is_fetched_by_name() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (origin, clone) = origin_and_clone(tmp.path()).await;
    let repos = repos(tmp.path());

    git(&origin, &["checkout", "-b", "release"]).await;
    let release = commit(&origin, "release work").await;
    git(&origin, &["checkout", "main"]).await;
    commit(&origin, "main work").await;

    for (base, branch) in [
        ("release", "board/gh-1-widget"),
        ("origin/release", "board/gh-2-widget"),
    ] {
        let worktree = repos
            .create_worktree_on(&clone, branch, base)
            .await
            .unwrap_or_else(|e| panic!("base `{base}`: {e}"));
        assert_eq!(
            head(Path::new(&worktree.path)).await,
            release,
            "base `{base}` cuts from origin/release"
        );
    }
}

/// A retry lands on the previous attempt's commits. It must not fetch, rebase,
/// or re-cut — an agent that pushed two commits and got interrupted comes back
/// to those two commits, however far origin has moved since.
///
/// It also comes back to a *hand-deleted* checkout directory: that leaves a
/// stale worktree registration behind, which used to fail the next
/// `worktree add` on the same path. `git worktree prune` first (gh#67).
#[tokio::test]
async fn a_retry_reuses_the_branch_without_moving_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (origin, clone) = origin_and_clone(tmp.path()).await;
    let repos = repos(tmp.path());

    let first = repos
        .create_worktree_on(&clone, "board/gh-67-widget", "origin/HEAD")
        .await
        .expect("first dispatch");
    let attempt = commit(Path::new(&first.path), "the agent's work").await;

    // The operator deletes the checkout by hand; origin moves on.
    std::fs::remove_dir_all(&first.path).expect("remove the checkout");
    commit(&origin, "more upstream work").await;

    let retry = repos
        .create_worktree_on(&clone, "board/gh-67-widget", "origin/HEAD")
        .await
        .expect("a retry re-opens the branch through a stale registration");
    assert_eq!(retry.path, first.path);
    assert_eq!(
        head(Path::new(&retry.path)).await,
        attempt,
        "the retry keeps the attempt's commits rather than rebasing them"
    );

    // And a retry whose checkout is still there is returned as-is.
    let again = repos
        .create_worktree_on(&clone, "board/gh-67-widget", "origin/HEAD")
        .await
        .expect("a retry onto a live checkout");
    assert_eq!(again.path, first.path);
    assert_eq!(head(Path::new(&again.path)).await, attempt);
}

/// An origin that cannot be reached refuses the dispatch, naming the repo — it
/// does not quietly fall back to the stale local checkout, which is the whole
/// bug. Both spellings of the base refuse: the remote's default branch and a
/// branch named outright.
#[tokio::test]
async fn an_unreachable_origin_refuses_rather_than_going_stale() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (_origin, clone) = origin_and_clone(tmp.path()).await;
    let repos = repos(tmp.path());
    let gone = tmp.path().join("no-such-remote.git");
    git(
        &clone,
        &["remote", "set-url", "origin", &gone.to_string_lossy()],
    )
    .await;

    for base in ["origin/HEAD", "main"] {
        let err = repos
            .create_worktree_on(&clone, &format!("board/{}", base.replace('/', "-")), base)
            .await
            .expect_err("an unreachable origin refuses the dispatch")
            .to_string();
        assert!(err.contains("origin"), "base `{base}`: {err}");
        assert!(
            err.contains(&clone.display().to_string()),
            "base `{base}` names the repo: {err}"
        );
    }

    // Nothing was cut: a refusal leaves no half-made checkout behind.
    assert!(
        !tmp.path().join("worktrees/widget/board-main").exists(),
        "a refused dispatch cuts nothing"
    );
}

/// `base = "HEAD"` is the opt-out — the pre-gh#67 behaviour, for a repo with no
/// remote at all. It must not touch the network, and a repo with no origin must
/// still dispatch under it.
#[tokio::test]
async fn base_head_branches_from_the_local_checkout() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("local-only");
    std::fs::create_dir_all(&repo).expect("repo dir");
    git(&repo, &["init", "-b", "main"]).await;
    commit(&repo, "root").await;
    let local = commit(&repo, "local work").await;
    let repos = repos(tmp.path());

    let worktree = repos
        .create_worktree_on(&repo, "board/gh-67-local", "HEAD")
        .await
        .expect("a repo with no remote dispatches under base = HEAD");
    assert_eq!(head(Path::new(&worktree.path)).await, local);

    // ...and the default refuses it, legibly, rather than pretending.
    let err = repos
        .create_worktree_on(&repo, "board/gh-68-local", "origin/HEAD")
        .await
        .expect_err("no origin, no default base")
        .to_string();
    assert!(err.contains("base"), "{err}");
}
