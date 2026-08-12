//! Seed a board with a week of synthetic work, so Settings → Board stats can
//! be *looked at* (gh#252).
//!
//! The reason this exists: gh#225–gh#228 each shipped a correct element into
//! the wrong container, and four reviews missed it because nobody could open
//! the page with numbers on it. `scripts/dev-demo.sh` seeds chats, not a board,
//! and an empty board draws the empty state — which is the one state that
//! cannot show whether the layout is right.
//!
//! Everything here is invented. It is dispatch metadata and token counts, not
//! register data, and it never leaves the data dir you point it at.
//!
//! ```sh
//! COMET_DATA_DIR=/tmp/comet-stats cargo run -p comet-board --example seed_stats
//! COMET_EDGE_URL=off COMET_DATA_DIR=/tmp/comet-stats COMET_HARNESS=mock \
//!   ./target/debug/comet
//! ```
//!
//! The shape is chosen to exercise the things the page has to get right rather
//! than to look busy: a quiet day in the middle of the window (the 2px rule), a
//! peak day (the one bar drawn a tone up), all four landing categories
//! including both losses, one attempt still running, two accounts and four
//! models so the breakdown has rows on either axis, one of them unpriced so the
//! footer has a caveat to carry and the row has a word where its money would be
//! (gh#359), and evening-heavy hours in one space so the crossing has a shape
//! rather than a smear.
//!
//! **The unpriced one has to stay unpriced.** It was `gpt-5.6-terra` until the
//! shipped table learned that model's rate (gh#358), which quietly turned the
//! one state this fixture exists to show back into an ordinary priced row —
//! and nothing failed, because a seed has no assertions. `gpt-5.6-luna` is the
//! name the tests use for a model `comet_proto::view::rates::builtin()` has
//! never heard of; if it is ever priced, pick another here.

use anyhow::Result;
use comet_board::db::{Db, NewAttempt, UpsertTask, rfc3339};
use comet_board::model::{Outcome, Source, UpstreamState};
use comet_proto::TokenUsage;

/// One attempt to invent: which day of the window, the hour it ran, how long
/// it took, what it ran on and under, and what it spent.
struct Seed {
    /// Days back from today — 0 is today, 6 the far end of a seven-day window.
    day: i64,
    hour: u32,
    minutes: i64,
    workspace: &'static str,
    runtime: &'static str,
    model: &'static str,
    account: Option<&'static str>,
    outcome: Option<Outcome>,
    /// Millions of tokens, split the way a long agent turn actually splits:
    /// mostly cache reads, a little fresh input, less output.
    megatokens: f64,
    /// An agent released it, rather than the operator.
    by_agent: bool,
}

fn main() -> Result<()> {
    // **`COMET_DATA_DIR` is required, and the directory has to be empty.**
    //
    // Not defensiveness: run without it once and `data_dir()` resolves to
    // `~/.comet-native`, which is somebody's actual board — and this example
    // then inserts eighteen invented tasks into it and overwrites their
    // `routing.toml` with a two-account stub. Both are silent, and the second
    // is not recoverable from anything this repo keeps. A fixture that can
    // reach the real install by *omission* is a fixture with a footgun on it,
    // so the safe path is the only path.
    let Some(dir) = std::env::var_os("COMET_DATA_DIR").filter(|v| !v.is_empty()) else {
        anyhow::bail!(
            "refusing to seed: set COMET_DATA_DIR to a scratch directory first, or this \
             writes into the real board at {}.\n  \
             COMET_DATA_DIR=/tmp/comet-stats cargo run -p comet-board --example seed_stats",
            comet_board::config::data_dir().display()
        );
    };
    let data_dir = std::path::PathBuf::from(dir);
    let paths = comet_board::config::Paths::under(&data_dir)?;
    if paths.db().exists() || paths.routing().exists() {
        anyhow::bail!(
            "refusing to seed: {} already holds a board. Point COMET_DATA_DIR at an empty \
             directory, or delete that one first — this example rewrites routing.toml.",
            data_dir.display()
        );
    }
    let db = Db::open(&paths.db())?;

    // Rates and a plan per slot, so the spend band has both of its figures —
    // the second one is a number a person types in Settings → Agents, and
    // without it the card honestly reads "no plan cost entered".
    std::fs::write(
        paths.routing(),
        "[account.\"seed-one@example.invalid\"]\nmonthly_usd = 200\n\n\
         [account.\"seed-two@example.invalid\"]\nmonthly_usd = 100\n",
    )?;

    let seeds = plan();
    let mut backdates: Vec<(i64, String, Option<String>)> = Vec::new();

    for (index, seed) in seeds.iter().enumerate() {
        let task_id = format!("seed-{index:03}");
        db.upsert_task(&UpsertTask {
            id: task_id.clone(),
            source: Source::Github,
            source_id: format!("{index}"),
            identifier: format!("gh#{}", 300 + index),
            title: format!("Seeded task {index}"),
            body: None,
            url: format!("https://example.invalid/{index}"),
            labels: vec![],
            source_state: None,
            linear_team: None,
            linear_project: None,
            upstream: UpstreamState::Started,
            updated_at: comet_board::db::now(),
        })?;

        let attempt = db.insert_attempt(&NewAttempt {
            stacked_on: None,
            task_id: task_id.clone(),
            pane_id: None,
            workspace: seed.workspace.into(),
            runtime: seed.runtime.into(),
            worktree: None,
            branch: Some(format!("seed/{index}")),
            dispatched_by: seed.by_agent.then(|| "seed-orchestrator".to_string()),
            dispatched_by_pane: None,
            base_sha: None,
            account: seed.account.map(str::to_string),
            repo_path: None,
            dispatched_by_device: None,
            dispatched_by_user: None,
            dispatched_by_verified: false,
            billed_to: seed.account.map(str::to_string),
        })?;

        // `SEED_NO_TOKENS=1` seeds the same week with nothing metered, which is
        // the state the day chart has to *collapse* in rather than reserve a
        // band for (gh#252). Worth one branch: it is the case four reviews
        // could not see, because nobody had a board that produced it.
        if std::env::var("SEED_NO_TOKENS").is_err() {
            db.set_attempt_tokens(attempt, usage(seed.megatokens), Some(seed.model))?;
        }
        if let Some(outcome) = seed.outcome {
            db.close_attempt(attempt, outcome)?;
        }

        // Where it landed. Four categories, and both of the losses are real
        // here — a page that cannot show a rejected pull request beside a
        // merged one is a page nobody can check.
        match index % 6 {
            0 | 1 | 2 => {
                db.set_pr(&task_id, Some("https://example.invalid/pr"), Some(1), false)?;
                db.set_pr_merged(&task_id, true)?;
            }
            3 => db.set_pr(&task_id, Some("https://example.invalid/pr"), Some(2), true)?,
            4 => db.set_pr(&task_id, Some("https://example.invalid/pr"), Some(3), false)?,
            // No pull request raised at all.
            _ => {}
        }

        let start = (chrono::Utc::now() - chrono::Duration::days(seed.day))
            .date_naive()
            .and_hms_opt(seed.hour, 17, 0)
            .expect("valid hour")
            .and_utc();
        backdates.push((
            attempt,
            rfc3339(start),
            seed.outcome
                .map(|_| rfc3339(start + chrono::Duration::minutes(seed.minutes))),
        ));
    }
    drop(db);

    // The timestamps are the whole point of the fixture and `insert_attempt`
    // stamps them `now`, so they are rewritten directly. A seeding example is
    // the one caller allowed to reach past the API for this.
    let conn = rusqlite::Connection::open(paths.db())?;
    for (id, started, ended) in &backdates {
        conn.execute(
            "UPDATE attempts SET started_at = ?2, ended_at = ?3 WHERE id = ?1",
            rusqlite::params![id, started, ended],
        )?;
    }

    println!(
        "seeded {} attempts into {}",
        backdates.len(),
        paths.db().display()
    );
    println!("routing.toml written to {}", paths.routing().display());
    Ok(())
}

/// A long agent turn's token split: cache reads dominate, output is small and
/// expensive. The four classes are priced apart (gh#225), so a fixture that
/// split them evenly would draw a cost bar that says nothing.
fn usage(megatokens: f64) -> TokenUsage {
    let total = (megatokens * 1_000_000.0) as u64;
    TokenUsage {
        input_tokens: total * 5 / 100,
        cache_read_tokens: total * 83 / 100,
        cache_creation_tokens: total * 9 / 100,
        output_tokens: total * 3 / 100,
    }
}

/// The week. Day 4 is deliberately silent — a window with a quiet day in the
/// middle of it is the only way to see that a quiet day draws a rule and not a
/// gap.
fn plan() -> Vec<Seed> {
    let mut seeds = Vec::new();
    // (days back, hour, minutes, workspace, runtime, model, account, tokens)
    let rows: &[(i64, u32, i64, &str, &str, &str, Option<&str>, f64)] = &[
        (
            6,
            9,
            41,
            "comet-board",
            "claude",
            "claude-opus-5",
            None,
            1.4,
        ),
        (
            6,
            10,
            62,
            "comet-board",
            "claude",
            "claude-opus-5",
            None,
            1.7,
        ),
        (5, 14, 28, "tally", "claude", "claude-sonnet-5", None, 0.6),
        (
            5,
            15,
            96,
            "comet-board",
            "claude",
            "claude-opus-5",
            None,
            3.1,
        ),
        (
            5,
            20,
            51,
            "comet-board",
            "claude",
            "claude-opus-5",
            None,
            2.2,
        ),
        (5, 21, 33, "attn", "codex", "gpt-5.6-luna", None, 0.9),
        (
            3,
            11,
            74,
            "comet-board",
            "claude",
            "claude-opus-5",
            None,
            2.8,
        ),
        (3, 13, 22, "tally", "claude", "claude-sonnet-5", None, 0.5),
        (
            3,
            19,
            118,
            "comet-board",
            "claude",
            "claude-opus-5",
            None,
            4.6,
        ),
        (2, 8, 39, "scratch", "codex", "gpt-5.6-luna", None, 0.8),
        (
            2,
            16,
            87,
            "comet-board",
            "claude",
            "claude-opus-5",
            None,
            3.4,
        ),
        (
            2,
            22,
            64,
            "comet-board",
            "claude",
            "claude-opus-5",
            None,
            2.6,
        ),
        (1, 10, 55, "tally", "claude", "claude-opus-5", None, 2.1),
        (
            1,
            20,
            132,
            "comet-board",
            "claude",
            "claude-opus-5",
            None,
            5.2,
        ),
        (
            1,
            21,
            47,
            "comet-board",
            "claude",
            "claude-haiku-4-5",
            None,
            0.4,
        ),
        (1, 23, 71, "attn", "codex", "gpt-5.6-luna", None, 1.2),
        (
            0,
            9,
            36,
            "comet-board",
            "claude",
            "claude-opus-5",
            None,
            1.6,
        ),
        (
            0,
            14,
            92,
            "comet-board",
            "claude",
            "claude-opus-5",
            None,
            3.3,
        ),
    ];
    for (index, row) in rows.iter().enumerate() {
        seeds.push(Seed {
            day: row.0,
            hour: row.1,
            minutes: row.2,
            workspace: row.3,
            runtime: row.4,
            model: row.5,
            // Two slots, so the per-subscription table has more than one row
            // and therefore earns its place.
            account: row.6.or(if index % 3 == 0 {
                Some("seed-two@example.invalid")
            } else {
                Some("seed-one@example.invalid")
            }),
            // One still running, one cancelled — the page has to show work in
            // flight as a caption and a loss as a colour.
            outcome: match index {
                17 => None,
                9 => Some(Outcome::Cancelled),
                _ => Some(Outcome::Done),
            },
            megatokens: row.7,
            by_agent: index % 4 != 0,
        });
    }
    seeds
}
