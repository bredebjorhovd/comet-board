//! Seed one reviewable attempt, so the review window can be *looked at*
//! (gh#276).
//!
//! The reason this exists is [`seed_stats`](./seed_stats.rs)'s reason. A review
//! is assembled from an attempt's checkout — the diff, the tests either side of
//! it, the manifests, the symbols the claims anchor — so a board with tasks on
//! it still draws an empty review, and an empty review is the one state that
//! cannot show whether the composition is right. This builds the checkout too.
//!
//! Everything here is invented, and it is invented to match
//! `docs/design/canvas/comet-review-window.dc.html`: the same task, the same
//! three claims, the same two unclaimed changes. That is the point — the canvas
//! is the spec, and a fixture that showed different content would make the
//! comparison a matter of imagination.
//!
//! ```sh
//! COMET_DATA_DIR=/tmp/comet-review cargo run -p comet-board --example seed_review
//! ```
//!
//! `scripts/review-demo.sh` runs this and opens the window on it.
//!
//! The attempt's `pane_id` is [`CHAT_ID`], a fixed uuid rather than a fresh
//! one: the chat that authored a review lives in the engine's store and not in
//! `board.db`, so the two halves are joined by a constant the script can create
//! the chat under.

use anyhow::Result;
use comet_board::claims;
use comet_board::db::{Db, NewAttempt, UpsertTask};
use comet_board::evidence::{Check, RunEvidence};
use comet_board::model::{Outcome, Source, UpstreamState};
use std::path::Path;
use std::process::Command;

/// The chat the fixture's attempt was authored in. Fixed so the demo script can
/// create a chat under the same id — see the module docs.
const CHAT_ID: &str = "5b7f1a30-1b2c-4c4e-9a8f-2f0f2f2d1c00";

const TASK_ID: &str = "gh:bredebjorhovd/comet-board#138";

/// What the agent said it did. One line per claim, in the contract's own shape
/// (`what you did :: the paths and symbols it is about`).
const CLAIMS: &str = "\
A live chat's row is drawn by Active and never by the spaces tree :: src/shell/spaces.rs src/view/spaces.rs
Both surfaces read one derivation per frame, so they cannot disagree :: active_placements
An expanded space says so when Active holds every one of its rows :: shelf_note
";

fn main() -> Result<()> {
    // `COMET_DATA_DIR` is required and must be empty, for `seed_stats`'s
    // reason: without it `data_dir()` resolves to somebody's actual board, and
    // this example would insert an invented task into it.
    let Some(dir) = std::env::var_os("COMET_DATA_DIR").filter(|v| !v.is_empty()) else {
        anyhow::bail!(
            "refusing to seed: set COMET_DATA_DIR to a scratch directory first, or this \
             writes into the real board at {}.\n  \
             COMET_DATA_DIR=/tmp/comet-review cargo run -p comet-board --example seed_review",
            comet_board::config::data_dir().display()
        );
    };
    let data_dir = std::path::PathBuf::from(dir);
    let paths = comet_board::config::Paths::under(&data_dir)?;
    if paths.db().exists() {
        anyhow::bail!(
            "refusing to seed: {} already holds a board. Point COMET_DATA_DIR at an empty \
             directory, or delete that one first.",
            data_dir.display()
        );
    }
    let db = Db::open(&paths.db())?;

    db.upsert_task(&UpsertTask {
        id: TASK_ID.into(),
        source: Source::Github,
        source_id: "138".into(),
        identifier: "gh#138".into(),
        title: "Active owns a chat's row while its session is live".into(),
        body: Some(
            "Active should own a chat's row while its session is live, and the tree should \
             show it when idle — one derivation, so the two surfaces can never disagree.\n\n\
             Today both lists derive placement from `state.chats` for themselves, one after \
             the other, and a chat that settles between the two passes is drawn twice."
                .into(),
        ),
        url: "https://github.com/bredebjorhovd/comet-board/issues/138".into(),
        labels: vec!["board".into()],
        source_state: Some("open".into()),
        linear_team: None,
        linear_project: None,
        upstream: UpstreamState::Started,
        updated_at: comet_board::db::now(),
    })?;
    // An open pull request on a settled attempt is what derives `review`.
    db.set_pr(
        TASK_ID,
        Some("https://github.com/bredebjorhovd/comet-board/pull/212"),
        Some(212),
        true,
    )?;

    let checkout = data_dir.join("checkout");
    let base = build_checkout(&checkout)?;

    let attempt = db.insert_attempt(&NewAttempt {
        task_id: TASK_ID.into(),
        pane_id: Some(CHAT_ID.into()),
        workspace: "comet-board".into(),
        runtime: "claude".into(),
        worktree: Some(checkout.to_string_lossy().into_owned()),
        branch: Some("board/gh-138".into()),
        dispatched_by: None,
        dispatched_by_pane: None,
        base_sha: Some(base),
        account: None,
        repo_path: Some(checkout.to_string_lossy().into_owned()),
        dispatched_by_device: None,
        dispatched_by_user: Some("brede".into()),
        dispatched_by_verified: true,
        billed_to: None,
    })?;
    db.set_attempt_claims(attempt, &claims::parse(CLAIMS, None)?)?;
    // What the run journal saw: the checks are the board's, not the agent's
    // account of them.
    db.set_attempt_evidence(
        attempt,
        &RunEvidence {
            commands: 41,
            failed: 2,
            checks: vec![
                Check {
                    command: "cargo test -p comet-ui".into(),
                    runs: 4,
                    failed: 1,
                },
                Check {
                    command: "cargo clippy --workspace --all-targets".into(),
                    runs: 2,
                    failed: 0,
                },
            ],
            truncated: false,
        },
    )?;
    // Settled with a pull request open: `derive_state` reads that as `review`.
    db.close_attempt(attempt, Outcome::Done)?;

    println!(
        "seeded {} (attempt {attempt}, chat {CHAT_ID}) into {}",
        TASK_ID,
        data_dir.display()
    );
    println!("checkout: {}", checkout.display());
    Ok(())
}

/// The attempt's checkout: a base commit, then the work on top of it.
///
/// The review reads this with git — the changed set, the tests either side, the
/// manifests, the symbols — so the fixture is a real repository rather than a
/// recorded diff. Returns the base sha the branch is measured against.
fn build_checkout(dir: &Path) -> Result<String> {
    std::fs::create_dir_all(dir.join("src/shell"))?;
    std::fs::create_dir_all(dir.join("src/view"))?;
    let git = |args: &[&str]| -> Result<String> {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output()?;
        anyhow::ensure!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };

    // -- the base ---------------------------------------------------------
    write(
        dir,
        "Cargo.toml",
        "[package]\nname = \"comet-ui\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\nserde = \"1\"\n",
    )?;
    write(
        dir,
        "src/shell/spaces.rs",
        "//! The spaces tree.\n\n\
         pub fn visible_chats(state: &State) -> Vec<Chat> {\n    \
         state.chats.iter().filter(|c| c.visible).cloned().collect()\n}\n\n\
         #[cfg(test)]\nmod tests {\n    use super::*;\n\n    \
         #[test]\n    fn the_tree_lists_visible_chats() {}\n}\n",
    )?;
    write(
        dir,
        "src/shelf.rs",
        "//! What a space says about itself.\n\n\
         pub fn shelf_note(space: &Space) -> Option<String> {\n    None\n}\n",
    )?;
    write(
        dir,
        "src/view/spaces.rs",
        "//! Active.\n\n\
         pub fn active_rows(state: &State) -> Vec<Chat> {\n    \
         visible_chats(state).into_iter().filter(|c| c.live).collect()\n}\n\n\
         #[cfg(test)]\nmod tests {\n    use super::*;\n\n    \
         #[test]\n    fn active_lists_live_chats() {}\n}\n",
    )?;
    write(
        dir,
        "src/shell.rs",
        "//! The window shell.\n\n\
         pub fn right_target(shell: &Shell) -> f32 {\n    \
         if shell.board_open { 520.0 } else { 0.0 }\n}\n",
    )?;
    git(&["init", "-b", "main"])?;
    git(&["config", "user.email", "fixture@comet"])?;
    git(&["config", "user.name", "fixture"])?;
    git(&["add", "-A"])?;
    git(&["commit", "-m", "base"])?;
    let base = git(&["rev-parse", "HEAD"])?;

    // -- the work ---------------------------------------------------------
    // A dependency nobody claimed, which is what makes the effects row's one
    // working chip and one of the two unclaimed rows.
    write(
        dir,
        "Cargo.toml",
        "[package]\nname = \"comet-ui\"\nversion = \"0.1.0\"\n\n\
         [dependencies]\nserde = \"1\"\nitertools = \"0.13\"\n",
    )?;
    write(
        dir,
        "src/shell/spaces.rs",
        "//! The spaces tree.\n\n\
         pub fn visible_chats(state: &State) -> Vec<Chat> {\n    \
         state.chats.iter().filter(|c| c.visible).cloned().collect()\n}\n\n\
         pub fn tree_rows(state: &State, placements: &Placements) -> Vec<Chat> {\n    \
         visible_chats(state)\n        .into_iter()\n        \
         .filter(|c| !placements.holds(&c.id))\n        .collect()\n}\n\n\
         #[cfg(test)]\nmod tests {\n    use super::*;\n\n    \
         #[test]\n    fn the_tree_lists_visible_chats() {}\n\n    \
         #[test]\n    fn a_live_chat_is_not_in_the_tree() {}\n}\n",
    )?;
    // The claim nothing checks: the shelf note moved and no test in this
    // branch reaches it, which is what drops its glyph to a dot.
    write(
        dir,
        "src/shelf.rs",
        "//! What a space says about itself.\n\n\
         pub fn shelf_note(space: &Space, placements: &Placements) -> Option<String> {\n    \
         placements.all_active_in(space).then(|| \"all live\".to_string())\n}\n",
    )?;
    write(
        dir,
        "src/view/spaces.rs",
        "//! Active.\n\n\
         pub fn active_rows(state: &State, placements: &Placements) -> Vec<Chat> {\n    \
         placements.rows(&state.chats)\n}\n\n\
         pub fn rows_for(state: &State, active: &Active) -> Vec<Chat> {\n    \
         active_rows(state, &active_placements(active, &state.chats))\n}\n\n\
         #[cfg(test)]\nmod tests {\n    use super::*;\n\n    \
         #[test]\n    fn active_lists_live_chats() {}\n\n    \
         #[test]\n    fn active_and_the_tree_never_hold_the_same_row() {}\n}\n",
    )?;
    // The derivation the second claim anchors. One call site now; there were
    // two before, in the two readers that each derived placement themselves.
    write(
        dir,
        "src/placement.rs",
        "//! One derivation per frame.\n\n\
         pub struct Placements(Vec<String>);\n\n\
         pub fn active_placements(active: &Active, chats: &[Chat]) -> Placements {\n    \
         Placements(chats.iter().filter(|c| active.holds(&c.id)).map(|c| c.id.clone()).collect())\n}\n",
    )?;
    write(
        dir,
        "src/shell.rs",
        "//! The window shell.\n\n\
         pub fn right_target(shell: &Shell) -> f32 {\n    \
         if shell.board_open {\n        520.0\n    } else if shell.review_open {\n        \
         380.0\n    } else {\n        0.0\n    }\n}\n",
    )?;
    // On the attempt's own branch, not on `main`: the session column names the
    // branch it is checked out on, and a fixture that said `main` there would
    // be photographing the wrong fact.
    git(&["checkout", "-q", "-b", "board/gh-138"])?;
    git(&["add", "-A"])?;
    git(&["commit", "-m", "Active owns a chat's row while its session is live"])?;
    Ok(base)
}

fn write(dir: &Path, path: &str, body: &str) -> Result<()> {
    Ok(std::fs::write(dir.join(path), body)?)
}
