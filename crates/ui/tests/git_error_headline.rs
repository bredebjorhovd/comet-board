//! gh#317: what reaches the screen when git fails, against a REAL git.
//!
//! The unit tests in `popover` pin the rule against the message from gh#316,
//! typed out by hand. This one asks git itself: it makes a clone fail, wraps
//! the stderr exactly as `Repos::git_env` does (`git: {stderr}`), and checks
//! that the one line a footer has room for is git's verdict rather than its
//! opening chatter.
//!
//! The bug was not that the message was wrong — it was whole all the way to the
//! footer. It was that the footer kept the front of it, and the front of a git
//! failure says `Cloning into '…'`, which reads as a clone that is going fine.

use std::process::Command;

use comet_ui::popover::error_headline;

#[test]
fn the_line_that_reaches_the_footer_is_gits_verdict() {
    let dir = std::env::temp_dir().join(format!("comet-gh317-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");

    // A `file://` remote that is not there: git's most ordinary multi-line
    // failure — chatter, two `fatal:` lines, then a paragraph of advice.
    let output = Command::new("git")
        .arg("clone")
        .arg(format!("file://{}", dir.join("no-such-repo").display()))
        .arg(dir.join("target"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("git runs");
    assert!(!output.status.success(), "the clone was supposed to fail");

    // Exactly the string `Repos::git_env` builds and the RPC carries.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = format!("git: {}", stderr.trim());
    assert!(
        message.lines().count() > 2,
        "this test needs a multi-line failure, got: {message}"
    );
    assert!(
        message.starts_with("git: Cloning into"),
        "the front of the message is chatter — that is the premise: {message}"
    );

    let headline = error_headline(&message);
    assert_eq!(headline.lines().count(), 1, "a footer gets one line");
    assert!(
        headline.starts_with("git: fatal:"),
        "the footer should show git's verdict, got: {headline}"
    );
    // Nothing was invented: the line shown is a line git wrote.
    assert!(
        stderr.contains(headline.trim_start_matches("git: ")),
        "{headline} is not one of git's own lines"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
