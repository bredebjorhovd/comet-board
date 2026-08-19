//! SQLite storage (impl spec §3). WAL, written atomically in transactions.
//!
//! The DB is the only bus between our processes: the daemon writes, the TUI
//! re-reads on a tick. There are no sockets between our own processes.

use crate::model::*;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub struct Db {
    pub conn: Connection,
}

/// Every column [`read_attempt`] expects, in the order it expects them.
const ATTEMPT_COLUMNS: &str = "id, task_id, pane_id, workspace, runtime, worktree, branch, \
     started_at, ended_at, outcome, missing_ticks, agent_status, \
     dispatched_by, dispatched_by_pane, base_sha, saw_working, \
     settled_at, reopened, screen_print, screen_at, nudges, nudged_at, account, \
     blocked_count,
     overrun_warned_at, repo_path, collectable_at, collected_at,
     dispatched_by_device, dispatched_by_user, dispatched_by_verified, billed_to, \
     chat_archivable_at, chat_archived_at, \
     input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens, model, \
     cache_sweepable_at, cache_swept_at, claims, claims_at, claims_error, board_managed, \
     context_used_tokens, context_max_tokens, context_compact_at_tokens, stacked_on, resumes, \
     token_models, token_agents, automation, automation_owner";

/// Build an [`Attempt`] from a row selected with [`ATTEMPT_COLUMNS`].
fn read_attempt(r: &rusqlite::Row<'_>) -> rusqlite::Result<Attempt> {
    Ok(Attempt {
        id: r.get(0)?,
        task_id: r.get(1)?,
        pane_id: r.get(2)?,
        workspace: r.get(3)?,
        runtime: r.get(4)?,
        worktree: r.get(5)?,
        branch: r.get(6)?,
        started_at: r.get(7)?,
        ended_at: r.get(8)?,
        outcome: r
            .get::<_, Option<String>>(9)?
            .and_then(|s| Outcome::parse(&s)),
        missing_ticks: r.get(10)?,
        agent_status: r
            .get::<_, Option<String>>(11)?
            .map(|s| AgentStatus::parse(&s)),
        dispatched_by: r.get(12)?,
        dispatched_by_pane: r.get(13)?,
        base_sha: r.get(14)?,
        saw_working: r.get::<_, i64>(15)? != 0,
        settled_at: r.get(16)?,
        reopened: r.get(17)?,
        screen_print: r.get(18)?,
        screen_at: r.get(19)?,
        nudges: r.get(20)?,
        nudged_at: r.get(21)?,
        account: r.get(22)?,
        blocked_count: r.get(23)?,
        overrun_warned_at: r.get(24)?,
        repo_path: r.get(25)?,
        collectable_at: r.get(26)?,
        collected_at: r.get(27)?,
        dispatched_by_device: r.get(28)?,
        dispatched_by_user: r.get(29)?,
        dispatched_by_verified: r.get::<_, i64>(30)? != 0,
        billed_to: r.get(31)?,
        chat_archivable_at: r.get(32)?,
        chat_archived_at: r.get(33)?,
        // All four together or none: the columns are written in one statement
        // and a partially-NULL row cannot arise. `input_tokens` is the witness
        // — NULL there is "this attempt reported nothing", which the page must
        // never render as a zero (gh#151).
        tokens: match r.get::<_, Option<i64>>(34)? {
            None => None,
            Some(input) => Some(comet_proto::TokenUsage {
                input_tokens: input.max(0) as u64,
                output_tokens: r.get::<_, Option<i64>>(35)?.unwrap_or(0).max(0) as u64,
                cache_read_tokens: r.get::<_, Option<i64>>(36)?.unwrap_or(0).max(0) as u64,
                cache_creation_tokens: r.get::<_, Option<i64>>(37)?.unwrap_or(0).max(0) as u64,
            }),
        },
        model: r.get(38)?,
        cache_sweepable_at: r.get(39)?,
        cache_swept_at: r.get(40)?,
        // Unreadable JSON reads as no claims, never as an error: a column one
        // bad write corrupted must not take `load_tasks` — and with it the
        // whole board pane — down over a review field (§gh#183).
        claims: r
            .get::<_, Option<String>>(41)?
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default(),
        claims_at: r.get(42)?,
        claims_error: r.get(43)?,
        board_managed: r.get::<_, i64>(44)? != 0,
        // Written as a set, like the tokens above, and read on the same terms:
        // `context_max_tokens` is the witness, because a fullness without a
        // window is not a reading — see [`comet_proto::ContextUsage`] (gh#271).
        context: match r.get::<_, Option<i64>>(46)? {
            None => None,
            Some(max) => Some(comet_proto::ContextUsage {
                used_tokens: r.get::<_, Option<i64>>(45)?.unwrap_or(0).max(0) as u64,
                max_tokens: max.max(0) as u64,
                compact_at_tokens: r
                    .get::<_, Option<i64>>(47)?
                    .filter(|at| *at > 0)
                    .map(|at| at as u64),
            }),
        },
        stacked_on: r.get(48)?,
        resumes: r.get(49)?,
        // JSON metadata beside the authoritative four-column total. NULL is
        // "this harness did not attribute it", not an empty breakdown.
        token_models: r
            .get::<_, Option<String>>(50)?
            .and_then(|j| serde_json::from_str(&j).ok()),
        token_agents: r
            .get::<_, Option<String>>(51)?
            .and_then(|j| serde_json::from_str(&j).ok()),
        automation: r.get(52)?,
        automation_owner: r.get(53)?,
    })
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Db::init(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Db> {
        Db::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Db> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // A poll landing while the TUI reads should wait, not fail.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Db { conn };
        db.migrate()?;
        db.ensure_board_id()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS tasks (
              id           TEXT PRIMARY KEY,
              source       TEXT NOT NULL,
              source_id    TEXT NOT NULL,
              identifier   TEXT NOT NULL,
              title        TEXT NOT NULL,
              body         TEXT,
              url          TEXT NOT NULL,
              labels       TEXT NOT NULL DEFAULT '[]',
              state        TEXT NOT NULL,
              source_state TEXT,
              -- Databases created before gh#471 also carry `linear_team` and
              -- `linear_project` columns here. They are dead weight now — both
              -- nullable, so nothing reads or writes them — and dropping them
              -- would rebuild the table for nothing.
              upstream     TEXT NOT NULL DEFAULT 'unstarted',
              local_done   INTEGER NOT NULL DEFAULT 0,
              pr_url       TEXT,
              pr_number    INTEGER,
              pr_open      INTEGER NOT NULL DEFAULT 0,
              pr_merged    INTEGER NOT NULL DEFAULT 0,
              -- GitHub's mergeable_state: clean, behind, dirty, blocked…
              pr_mergeable TEXT,
              -- PR topology (gh#282). The branch the PR merges into, and — only
              -- when GitHub says it is in a stack — which stack, where in it,
              -- how big, and what the stack as a whole targets.
              pr_base_ref        TEXT,
              -- And the branch the work is on (gh#344). Free from the same poll
              -- as the base, and the only place the head is recorded for a pull
              -- request no attempt ever wrote: an attempt's own `branch` column
              -- answers this for everything the board dispatched.
              pr_head_ref        TEXT,
              pr_stack_number    INTEGER,
              pr_stack_size      INTEGER,
              pr_stack_position  INTEGER,
              pr_stack_base_ref  TEXT,
              -- The id of the review that last asked this PR to change, and
              -- which nothing has withdrawn since (gh#289). NULL is "not under
              -- review changes" — never asked, or asked and since approved.
              -- Stored flat beside the topology above because it is read the
              -- same way: the layers on top of this one derive their own state
              -- from it, and a per-row `meta` lookup on every board read is the
              -- shape that would make that expensive.
              pr_changes_requested INTEGER,
              updated_at   TEXT NOT NULL,
              synced_at    TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS attempts (
              id           INTEGER PRIMARY KEY,
              task_id      TEXT NOT NULL REFERENCES tasks(id),
              pane_id      TEXT,
              workspace    TEXT NOT NULL,
              runtime      TEXT NOT NULL,
              worktree     TEXT,
              branch       TEXT,
              started_at   TEXT NOT NULL,
              ended_at     TEXT,
              outcome      TEXT,
              missing_ticks INTEGER NOT NULL DEFAULT 0,
              -- When this attempt first looked finished on commits alone. A
              -- duration, not a count of samples: reconciliation runs on
              -- demand, so two samples can share a second (gh#34).
              settled_at   TEXT,
              -- How often the board closed this attempt and then found its
              -- pane working again (gh#34).
              reopened     INTEGER NOT NULL DEFAULT 0,
              -- Task id of the parent that released this one, when the board
              -- dispatched that parent too. NULL on its own does not mean
              -- "you": an orchestrator pane has no attempt and so no task id.
              dispatched_by TEXT,
              -- The pane the dispatch ran from, when an agent was in it. Both
              -- NULL is what "you" means; this one is set for every agent,
              -- since HERDR_PANE_ID is carried whether or not the board
              -- started the pane.
              dispatched_by_pane TEXT,
              -- Last agent status herdr reported for this attempt's pane. The
              -- TUI reads it to render the dim `idle` marker without having to
              -- shell out to herdr itself.
              agent_status TEXT,
              -- Digest of this pane's screen, and when that screen was *first*
              -- seen. A `working` status whose screen has not changed since is
              -- not a running turn — see gh#32 and `crate::screen::vitals`.
              screen_print TEXT,
              screen_at    TEXT,
              -- Nudges sent into this pane for the stall it is currently in, and
              -- when the last one went (gh#40). Cleared once the pane comes back.
              nudges       INTEGER NOT NULL DEFAULT 0,
              nudged_at    TEXT,
              -- The agent-account slot this attempt actually ran under, i.e.
              -- whose subscription it spent (gh#59). NULL is the device's own
              -- CLI login. Recorded per attempt rather than read back off the
              -- route, because the route's default can change under a run that
              -- is still going.
              account      TEXT,
              -- How many times this attempt has *entered* blocked — the agent
              -- stopped to ask, or its run died (gh#71). The notice upstream is
              -- keyed on this count, so a block that is answered and happens
              -- again is a second notice, and a hundred ticks of the same block
              -- are still one.
              blocked_count INTEGER NOT NULL DEFAULT 0,
              -- When the board told this attempt's chat it was past its
              -- wall-clock cap (gh#70). Stamped once; the grace that follows is
              -- measured from here.
              overrun_warned_at TEXT,
              -- The repo the worktree was cut from (gh#72). Recorded at
              -- dispatch because collection needs it after the checkout is
              -- gone: `git branch -D` runs in the repo, not in the worktree.
              repo_path    TEXT,
              -- When this attempt's checkout first became nobody's — the
              -- attempt closed and its task off the board (gh#72). The
              -- retention window is measured from here, and a task that comes
              -- back to life clears it so the next window starts whole.
              collectable_at TEXT,
              -- When the checkout and its branch were actually reclaimed.
              -- Non-NULL means `worktree` names a path that is no longer there.
              collected_at TEXT,
              -- Which device the dispatch was issued from, and which human
              -- released it (gh#74). The device is the caller's word always;
              -- the human is the edge's answer where the call came over the
              -- relay and the caller's word where it did not, which is what
              -- the column below records.
              dispatched_by_device TEXT,
              dispatched_by_user TEXT,
              -- …and whether the edge verified that human or a frontend merely
              -- said so (gh#161). The relay stamps the caller's verified
              -- identity onto every forwarded frame, so a teammate's dispatch
              -- is checkable; a dispatch issued on the box carries no stamp
              -- (it came from the box) and stays a claim. 0 for every row
              -- written before this column existed, which is what they are.
              dispatched_by_verified INTEGER NOT NULL DEFAULT 0,
              -- Whose subscription this attempt spends, as an email (gh#101):
              -- the `account` slot's login, or the box's own CLI login when the
              -- dispatch named no slot. Resolved once at dispatch — the slot id
              -- above means nothing to a reader who has not saved that login,
              -- and the box's own login can be switched under a live run.
              billed_to TEXT,
              -- When this attempt's chat first became nobody's, and when the
              -- board archived it off its space's shelf (gh#139). The same two
              -- columns `collectable_at`/`collected_at` are for the checkout,
              -- kept apart because the windows are configured apart.
              chat_archivable_at TEXT,
              chat_archived_at TEXT,
              -- What the attempt spent, summed off its chat's run journal
              -- (gh#151). Nullable on purpose, and written as a set: NULL
              -- across all four is "this attempt reported no usage", which is
              -- what every row from before this existed honestly is. A DEFAULT
              -- 0 here would have made the whole history look free.
              input_tokens INTEGER,
              output_tokens INTEGER,
              cache_read_tokens INTEGER,
              cache_creation_tokens INTEGER,
              -- How full the attempt's context window was when its harness
              -- last said (gh#271). The other meter, and a different kind of
              -- number: these are OVERWRITTEN by each reading, never added to,
              -- because fullness is a level. Nullable for the tokens' reason —
              -- a harness that meters no window, and every row from before
              -- this existed, honestly reported nothing.
              context_used_tokens INTEGER,
              context_max_tokens INTEGER,
              -- Where that harness would auto-compact, when it names such a
              -- point (Claude does; codex fails the turn instead). NULL is
              -- "no threshold reported", which is not "no compaction".
              context_compact_at_tokens INTEGER,
              -- The model the harness announced for the run, recorded beside
              -- the tokens. The route's override is not it: most routes name
              -- none, so the column would be NULL exactly where the breakdown
              -- needs it most.
              model TEXT,
              -- Exact per-model result totals and per-agent assistant-step
              -- attribution (gh#426). JSON and nullable: old attempts, and
              -- harnesses that expose no split, honestly have no rows here.
              token_models TEXT,
              token_agents TEXT,
              -- When the build output inside this checkout became nobody's, and
              -- when the board last deleted it (gh#186). Kept apart from
              -- `collectable_at`/`collected_at` because they mean different
              -- things: those say the checkout is gone, these say a cache
              -- inside a checkout that is still there was swept. Writing a
              -- sweep as a collection would have the board reporting checkouts
              -- reclaimed that are still on the disk, still on their branches.
              cache_sweepable_at TEXT,
              cache_swept_at TEXT,
              -- What the agent said it did, file-anchored, and when it said it
              -- (§gh#183). On the attempt beside the outcome because a retry
              -- makes new claims and because the review happens after the chat
              -- is archived — the conversation is gone, the row is not. NULL
              -- and `[]` are different answers: nothing submitted at all, and
              -- an agent that submitted an empty set. `claims_at` is the
              -- witness for the first.
              claims TEXT,
              claims_at TEXT,
              -- Why the block a finished attempt wrote could not be read
              -- (§gh#235). Its own column beside the two above because it is
              -- the third answer they cannot express between them: never
              -- asked, answered, and answered in a way the board threw away.
              claims_error TEXT,
              -- False for an attempt adopted from an ordinary Comet chat.
              -- Its checkout and chat belong to the person, not the Board
              -- retention policy, even though its review facts share this row.
              board_managed INTEGER NOT NULL DEFAULT 1,
              -- The branch diff and the run's own commands, snapshotted off
              -- the live attempt (§gh#183). Kept out of the columns every
              -- board read selects: the board loads every attempt of every
              -- task on every cycle, and these two are read when somebody
              -- opens a review. Snapshotted rather than looked up because gc
              -- reclaims the checkout (gh#72) and the run journal can be
              -- compacted, while the review outlives both.
              changed_files TEXT,
              run_evidence TEXT,
              -- What sandbox the run actually got, beside what was asked for
              -- (§gh#349). Snapshotted here for the two above's reason, and
              -- one more: it is the fact most likely to be needed after the
              -- fact and least likely to still be readable, since a resumed
              -- chat overwrites the journal's answer with its own.
              run_sandbox TEXT,
              -- The attempt this one's branch was cut from, when it was
              -- dispatched onto a sibling instead of onto trunk (gh#285). An
              -- attempt id rather than the base branch name, because the branch
              -- is the part that stops being true — a merged parent has its
              -- branch deleted, and a child that looked for its parent by
              -- branch equality would then find nothing. NULL is an ordinary
              -- dispatch off the route's `base`, and every row from before
              -- stacking existed.
              stacked_on INTEGER REFERENCES attempts(id),
              -- How many times the board has restarted this attempt's run in
              -- its own chat (gh#390). Not a retry and not a second attempt:
              -- the chat, the branch and the checkout are the same ones, and
              -- the count exists to bound the restarting, not to number it.
              resumes INTEGER NOT NULL DEFAULT 0,
              -- The auto-pick rule that released this attempt, and the human
              -- who owns that rule (gh#490). NULL on every attempt a person
              -- (or an orchestrating agent) released — automation provenance
              -- is the exception, and its absence is the ordinary case.
              automation TEXT,
              automation_owner TEXT
            );

            -- Impl spec §7: the duplicate-dispatch guard. A second concurrent
            -- dispatch (double enter, or pane racing the picker) fails this
            -- constraint instead of spawning a second pane.
            CREATE UNIQUE INDEX IF NOT EXISTS attempts_one_live
              ON attempts(task_id) WHERE outcome IS NULL;

            CREATE INDEX IF NOT EXISTS attempts_by_task ON attempts(task_id);

            CREATE TABLE IF NOT EXISTS writeback_queue (
              id         INTEGER PRIMARY KEY,
              task_id    TEXT NOT NULL,
              kind       TEXT NOT NULL,
              payload    TEXT NOT NULL,
              idem_key   TEXT NOT NULL,
              created_at TEXT NOT NULL,
              attempts   INTEGER NOT NULL DEFAULT 0,
              next_try_at TEXT,
              last_error TEXT,
              done       INTEGER NOT NULL DEFAULT 0
            );

            -- Writeback idempotency: the same logical effect can be enqueued
            -- repeatedly (retried sync, restarted daemon) without duplicating
            -- comments upstream.
            CREATE UNIQUE INDEX IF NOT EXISTS writeback_idem
              ON writeback_queue(idem_key);

            CREATE TABLE IF NOT EXISTS meta (
              key TEXT PRIMARY KEY, value TEXT NOT NULL
            );

            -- What each auto-pick rule did and declined to do (gh#490): one
            -- row per (rule, task, decision, reason) streak, not per look —
            -- an unchanged answer bumps `last_at`/`count` on its latest row
            -- instead of appending, so thirty seconds of "at capacity" is one
            -- line and not a hundred. Its own table rather than `meta` rows
            -- because operators *list* this: the run/history view is the
            -- point, and a keyed blob store cannot answer "what happened
            -- lately" without walking every key. Never carries credentials or
            -- webhook material — reasons are the board's own sentences.
            CREATE TABLE IF NOT EXISTS automation_log (
              id         INTEGER PRIMARY KEY,
              rule       TEXT NOT NULL,
              -- NULL for a note about the rule itself rather than one task.
              task_id    TEXT,
              -- Denormalized so history stays legible after a task is reaped.
              identifier TEXT,
              -- dispatched | skipped | deferred | refused.
              decision   TEXT NOT NULL,
              reason     TEXT NOT NULL,
              attempt_id INTEGER,
              first_at   TEXT NOT NULL,
              last_at    TEXT NOT NULL,
              count      INTEGER NOT NULL DEFAULT 1
            );

            CREATE INDEX IF NOT EXISTS automation_log_recent
              ON automation_log(rule, last_at);
            "#,
        )?;

        // `CREATE TABLE IF NOT EXISTS` does nothing to a table that already
        // exists, so every column added after the first release has to be
        // applied to existing databases explicitly. Without this an upgrade
        // leaves `load_tasks` failing on a missing column — which takes the
        // board pane down with it.
        self.add_missing_columns(
            "tasks",
            &[
                ("local_done", "INTEGER NOT NULL DEFAULT 0"),
                ("pr_url", "TEXT"),
                ("pr_number", "INTEGER"),
                ("pr_open", "INTEGER NOT NULL DEFAULT 0"),
                ("pr_merged", "INTEGER NOT NULL DEFAULT 0"),
                ("pr_mergeable", "TEXT"),
                // gh#282. Existing rows keep NULL until the next poll, which is
                // the same thing "we have not looked yet" has always meant here.
                ("pr_base_ref", "TEXT"),
                // gh#344, and NULL until the next poll for the same reason.
                ("pr_head_ref", "TEXT"),
                ("pr_stack_number", "INTEGER"),
                ("pr_stack_size", "INTEGER"),
                ("pr_stack_position", "INTEGER"),
                ("pr_stack_base_ref", "TEXT"),
                // gh#289. NULL on every row that came before, which is what
                // they are: no review of theirs was ever fanned out, and
                // backfilling would mean asking GitHub for the review history
                // of every open pull request on the board to answer a question
                // the next poll answers for free.
                ("pr_changes_requested", "INTEGER"),
                ("upstream", "TEXT NOT NULL DEFAULT 'unstarted'"),
            ],
        )?;
        self.add_missing_columns(
            "attempts",
            &[
                ("missing_ticks", "INTEGER NOT NULL DEFAULT 0"),
                ("agent_status", "TEXT"),
                ("dispatched_by", "TEXT"),
                // Provenance's actual answer for the common case: the pane the
                // dispatch came from. Existing rows keep NULL — they were
                // recorded when only board-dispatched parents could be seen at
                // all, so their "released by you" was never checked (AGE-24).
                ("dispatched_by_pane", "TEXT"),
                // The commit the attempt branched from. Without it, "did the
                // agent produce anything" has to be measured against
                // origin/HEAD, which counts the operator's own unpushed work
                // as the agent's — see AGE-19.
                ("base_sha", "TEXT"),
                // Has this attempt ever been observed working? An agent that
                // was never seen working cannot have finished.
                ("saw_working", "INTEGER NOT NULL DEFAULT 0"),
                // When this attempt first looked finished on commits alone.
                // `idle` flaps mid-turn, so one sample is not enough — and
                // neither were the two consecutive samples gh#18 asked for,
                // because reconciliation runs on demand and two of those can
                // land in the same second (gh#34). Existing rows keep NULL,
                // which reads as "the run starts now".
                ("settled_at", "TEXT"),
                // Times the board closed this attempt and then caught its pane
                // still working (gh#34). Deliberately not a second attempt: no
                // second dispatch happened, and counting one would inflate the
                // retry rate the board reports.
                ("reopened", "INTEGER NOT NULL DEFAULT 0"),
                // The pane's screen last time we looked, and when that screen
                // first appeared. Together they answer the question a manifest
                // cannot: whether a spinner it matched is live (gh#32).
                ("screen_print", "TEXT"),
                ("screen_at", "TEXT"),
                // Times the board has prompted this pane to carry on after its
                // turn died on an Anthropic 5xx, and when it last did (gh#40).
                // Existing rows keep 0, which is "nothing tried yet" — right for
                // a stall that predates the feature as much as for a fresh one.
                ("nudges", "INTEGER NOT NULL DEFAULT 0"),
                ("nudged_at", "TEXT"),
                // Whose subscription this attempt spent (gh#59). Existing rows
                // keep NULL, which is exactly right: they ran before accounts
                // could be chosen, on the device's own CLI login.
                ("account", "TEXT"),
                // Times this attempt entered blocked (gh#71). Existing rows
                // keep 0: a block they are *currently* in was never noticed
                // upstream, so the next tick that sees it is its first notice.
                ("blocked_count", "INTEGER NOT NULL DEFAULT 0"),
                // When this attempt was told it had run past its cap (gh#70).
                // Existing rows keep NULL, which reads as "not warned yet" —
                // right for an attempt already hours over when the cap landed,
                // because it gets its warning and its grace before anything
                // closes it.
                ("overrun_warned_at", "TEXT"),
                // Worktree collection (gh#72). Existing rows keep NULL on all
                // three, which reads as "cut before the board recorded the
                // repo, never marked, never collected" — so an old attempt's
                // checkout is collected on the same terms as a new one, from
                // the first sweep that finds its task finished.
                ("repo_path", "TEXT"),
                ("collectable_at", "TEXT"),
                ("collected_at", "TEXT"),
                // Which device and which human released this attempt (gh#74).
                // Existing rows keep NULL: they were recorded when no frontend
                // sent either, and inventing the box's own device id for them
                // would be a guess dressed as a record.
                ("dispatched_by_device", "TEXT"),
                ("dispatched_by_user", "TEXT"),
                // Whether that human was verified or claimed (gh#161).
                // Existing rows keep 0 — every one of them recorded a claim,
                // because a claim was all there was to record.
                ("dispatched_by_verified", "INTEGER NOT NULL DEFAULT 0"),
                // Whose subscription the attempt spends (gh#101). Existing rows
                // keep NULL, which reads as "nobody resolved it" — resolving it
                // now would answer with today's logins about a run that spent
                // whatever was live months ago.
                ("billed_to", "TEXT"),
                // Chat archiving (gh#139). Existing rows keep NULL on both,
                // which reads as "never marked, never archived" — so the chats
                // already silted up on the shelves are archived on the same
                // terms as new ones, from the first sweep that finds their task
                // finished. That is the backlog this feature is about.
                ("chat_archivable_at", "TEXT"),
                ("chat_archived_at", "TEXT"),
                // Tokens, and the model that spent them (gh#151). Nullable
                // with no default on all five, which is the whole point:
                // existing rows keep NULL and read as "reported nothing", and
                // the stats page counts them out of its coverage rather than
                // adding a zero to a total. Backfilling them is not possible
                // and would not be honest if it were — the runs are over and
                // their journals may be gone.
                ("input_tokens", "INTEGER"),
                ("output_tokens", "INTEGER"),
                ("cache_read_tokens", "INTEGER"),
                ("cache_creation_tokens", "INTEGER"),
                ("model", "TEXT"),
                // Model/agent attribution (gh#426). Existing rows keep NULL:
                // their totals cannot be split after the journals are gone.
                ("token_models", "TEXT"),
                ("token_agents", "TEXT"),
                // Context fullness (gh#271), on the tokens' terms exactly:
                // nullable, no default, existing rows keep NULL and read as
                // "reported nothing". Backfilling is impossible here too — the
                // level is a fact about a moment that has passed.
                ("context_used_tokens", "INTEGER"),
                ("context_max_tokens", "INTEGER"),
                ("context_compact_at_tokens", "INTEGER"),
                // Build-output sweeping (gh#186). Existing rows keep NULL on
                // both, which reads as "never marked, never swept" — so the
                // 109 GiB already sitting in the worktree root is swept on the
                // same terms as tomorrow's, from the first sweep that finds
                // its attempt closed. That backlog is what this is about.
                ("cache_sweepable_at", "TEXT"),
                ("cache_swept_at", "TEXT"),
                // The review contract (§gh#183). Existing rows keep NULL on
                // all four, which is exactly what they are: they ran before
                // anything asked for claims, so they claimed nothing and were
                // never asked to — and their diffs and journals are long gone.
                // Backfilling any of it would be inventing a record.
                ("claims", "TEXT"),
                ("claims_at", "TEXT"),
                ("changed_files", "TEXT"),
                ("run_evidence", "TEXT"),
                // What sandbox the run actually got (§gh#349). NULL on every
                // row that came before, and it stays NULL: those runs were not
                // asked and their journals have moved on. A review of one says
                // the level is unknown, which is true and is also the whole
                // point — backfilling them with the level that was *requested*
                // would restore exactly the false record this column exists to
                // replace.
                ("run_sandbox", "TEXT"),
                // The refusal an attempt's own claims block earned (§gh#235).
                // NULL on every row that came before, which is what they are:
                // nothing read a block off them, so none of them wrote one
                // badly.
                ("claims_error", "TEXT"),
                // What the board read off the branch itself (§gh#236). NULL on
                // every row that came before, which deserializes to
                // `Effects::read = false` — "nobody looked", which is what
                // those rows are, and which the chips say out loud rather than
                // rendering as five clean results.
                ("effects", "TEXT"),
                // Direct Comet chats can author reviewable PRs too. Existing
                // attempts were created by Board dispatch and remain managed.
                ("board_managed", "INTEGER NOT NULL DEFAULT 1"),
                // The stacking edge (gh#285). NULL on every row that came
                // before, which is what they are: nothing could be dispatched
                // onto a sibling then, so every one of them was cut from its
                // route's `base`. Added without the REFERENCES the fresh schema
                // carries — `ALTER TABLE ADD COLUMN` cannot take one — which
                // costs nothing here: the id is written by the same dispatch
                // that read it, and nothing deletes attempt rows.
                ("stacked_on", "INTEGER"),
                // Restarted runs (gh#390). 0 on every row that came before,
                // which is what they are: the board could not restart a run
                // then, it closed the attempt instead.
                ("resumes", "INTEGER NOT NULL DEFAULT 0"),
                // gh#490. NULL on every attempt a person released, which is
                // what they are: automation provenance marks the exception.
                ("automation", "TEXT"),
                ("automation_owner", "TEXT"),
            ],
        )?;
        self.add_missing_columns(
            "writeback_queue",
            &[
                ("idem_key", "TEXT NOT NULL DEFAULT ''"),
                ("next_try_at", "TEXT"),
                ("last_error", "TEXT"),
            ],
        )?;
        // gh#471: Linear is no longer a connected source. Clear the poll
        // bookkeeping an older board left behind, so no reader mistakes a
        // stale watermark or health row for a source that is still polled.
        // Task rows keep their `source = 'linear'` history untouched.
        self.conn.execute_batch(
            "DELETE FROM meta WHERE key IN
               ('linear_watermark','linear_status','linear_last_ok','linear_failures');",
        )?;
        Ok(())
    }

    /// Add any of `columns` the table does not already have.
    fn add_missing_columns(&self, table: &str, columns: &[(&str, &str)]) -> Result<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: std::collections::HashSet<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<_>>()?;
        for (name, ty) in columns {
            if !existing.contains(*name) {
                self.conn
                    .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {name} {ty}"))
                    .with_context(|| format!("adding {table}.{name}"))?;
            }
        }
        Ok(())
    }

    // ---- meta -----------------------------------------------------------

    /// The store's durable identity (gh#461).
    ///
    /// A device id identifies a route to a board, not the board itself: an
    /// alias or a copied device registration can reach the same `board.db`.
    /// The random value is minted once inside SQLite and travels with the
    /// database if it is moved or restored, which is exactly the ownership
    /// needed to collapse transport aliases without collapsing two boards
    /// that happen to poll the same repositories.
    fn ensure_board_id(&self) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO meta(key, value)
             VALUES('board_id', 'board-' || lower(hex(randomblob(16))))",
            [],
        )?;
        Ok(())
    }

    pub fn board_id(&self) -> Result<String> {
        self.meta_get("board_id")?
            .ok_or_else(|| anyhow::anyhow!("board store has no identity"))
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- tasks ----------------------------------------------------------

    /// Insert or refresh a task from a source poll.
    ///
    /// Deliberately does **not** touch `state` (it is derived on every read) or
    /// `local_done` (an operator decision a poll must not clobber).
    pub fn upsert_task(&self, t: &UpsertTask) -> Result<()> {
        let labels = serde_json::to_string(&t.labels)?;
        self.conn.execute(
            "INSERT INTO tasks
               (id, source, source_id, identifier, title, body, url, labels,
                state, source_state, upstream, updated_at, synced_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(id) DO UPDATE SET
               title        = excluded.title,
               body         = excluded.body,
               url          = excluded.url,
               labels       = excluded.labels,
               source_state = excluded.source_state,
               upstream     = excluded.upstream,
               updated_at   = excluded.updated_at,
               synced_at    = excluded.synced_at",
            params![
                t.id,
                t.source.as_str(),
                t.source_id,
                t.identifier,
                t.title,
                t.body,
                t.url,
                labels,
                // Seed value only; every read re-derives.
                BoardState::Ready.as_str(),
                t.source_state,
                t.upstream.as_str(),
                t.updated_at,
                now(),
            ],
        )?;
        Ok(())
    }

    /// Retire a task the source no longer returns.
    ///
    /// Two outcomes, decided by whether anyone ever worked on it (AGE-6):
    ///
    /// - **Nothing was ever dispatched** — the row is noise, an issue created and
    ///   deleted again, and it is forgotten outright.
    /// - **It has attempts** — the row stays, marked `gone` upstream. Deleting it
    ///   threw away the record of which agent ran, on what branch, and how it
    ///   ended; worse, it orphaned the attempt's checkout, leaving `gc` with a
    ///   directory it could not attribute to a repo and so refused to touch. The
    ///   kept row is what `gc` still collects by: `gone` is terminal, so the
    ///   checkout ages out normally instead of leaking.
    ///
    /// Queued writebacks go either way: there is no issue left to comment on, and
    /// a comment aimed at a deleted issue would fail and back off forever.
    /// Delivered ones stay, so their idempotency keys still hold if the task
    /// reappears.
    pub fn reap_task(&self, task_id: &str) -> Result<Reaped> {
        let tx = self.conn.unchecked_transaction()?;
        let attempts: i64 = tx.query_row(
            "SELECT COUNT(*) FROM attempts WHERE task_id = ?1",
            params![task_id],
            |r| r.get(0),
        )?;
        if attempts == 0 {
            tx.execute(
                "DELETE FROM writeback_queue WHERE task_id = ?1",
                params![task_id],
            )?;
            tx.execute("DELETE FROM tasks WHERE id = ?1", params![task_id])?;
            tx.commit()?;
            return Ok(Reaped::Forgotten);
        }
        tx.execute(
            "DELETE FROM writeback_queue WHERE task_id = ?1 AND done = 0",
            params![task_id],
        )?;
        // `state` is derived on every read, but it is also a stored column other
        // readers use — so it is set here rather than left claiming `ready` until
        // the next derivation pass.
        tx.execute(
            "UPDATE tasks SET upstream = ?2, state = ?3 WHERE id = ?1",
            params![
                task_id,
                UpstreamState::Gone.as_str(),
                BoardState::Done.as_str()
            ],
        )?;
        tx.commit()?;
        Ok(Reaped::Kept {
            attempts: attempts as usize,
        })
    }

    /// Task ids from one source that reaping still has something to do to.
    ///
    /// Tasks already marked `gone` are left out: they have been reaped once, and
    /// re-reaping them every sweep would say so in the log every two minutes.
    pub fn reapable_task_ids(&self, source: Source) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM tasks WHERE source = ?1 AND upstream <> ?2")?;
        let rows = stmt.query_map(
            params![source.as_str(), UpstreamState::Gone.as_str()],
            |r| r.get::<_, String>(0),
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_pr(
        &self,
        task_id: &str,
        url: Option<&str>,
        number: Option<i64>,
        open: bool,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET pr_url = ?2, pr_number = ?3, pr_open = ?4 WHERE id = ?1",
            params![task_id, url, number, open as i64],
        )?;
        Ok(())
    }

    pub fn set_pr_mergeable(&self, task_id: &str, state: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET pr_mergeable = ?2 WHERE id = ?1",
            params![task_id, state],
        )?;
        Ok(())
    }

    /// Record where a PR sits relative to the rest of the tree (gh#282), and
    /// which branch the work itself is on (gh#344).
    ///
    /// Written whole every poll, stack and all, so a PR that leaves a stack —
    /// or is retargeted onto `main` when its parent merges — clears the fields
    /// it no longer answers to instead of keeping a stale position forever.
    pub fn set_pr_topology(
        &self,
        task_id: &str,
        base_ref: Option<&str>,
        head_ref: Option<&str>,
        stack: Option<&PrStack>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET pr_base_ref = ?2, pr_stack_number = ?3,
                              pr_stack_size = ?4, pr_stack_position = ?5,
                              pr_stack_base_ref = ?6, pr_head_ref = ?7
             WHERE id = ?1",
            params![
                task_id,
                base_ref.filter(|r| !r.is_empty()),
                stack.map(|s| s.number),
                stack.and_then(|s| s.size),
                stack.and_then(|s| s.position),
                stack.and_then(|s| s.base_ref.as_deref()),
                head_ref.filter(|r| !r.is_empty()),
            ],
        )?;
        Ok(())
    }

    /// How many rows hold a pull request GitHub says is part of a stack (gh#335).
    ///
    /// The one durable answer to "does this box stack?". Nothing in
    /// `routing.toml` says so — the ask is per dispatch (`--stack`), and the flag
    /// is not kept on the attempt — so the board's own history is what is left:
    /// a row whose `pr_stack_number` survived a poll is a stack this board
    /// produced or adopted. [`crate::doctor`] reads it to decide which sentence
    /// its `gh stack` line should say.
    ///
    /// Counts rows and not stacks: two layers of one chain are two rows here.
    /// The caller only asks whether the number is zero, and a count that means
    /// "how many stacks" would have to group by repository as well as number
    /// ([`crate::stacks`]), which is more machinery than the question needs.
    pub fn stacked_task_count(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM tasks WHERE pr_stack_number IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Record that a reviewer asked this pull request to change, or that nothing
    /// is outstanding on it any more (gh#289).
    ///
    /// The projection of [`crate::review::Delivered::changes_requested`] onto
    /// the row, written by [`crate::review::store`] and by nothing else, so the
    /// two cannot come to disagree about what a reviewer last said. The record
    /// is the source; this column is what the rows and the state derivation read
    /// without a `meta` lookup each.
    pub fn set_pr_changes_requested(&self, task_id: &str, review_id: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET pr_changes_requested = ?2 WHERE id = ?1",
            params![task_id, review_id],
        )?;
        Ok(())
    }

    pub fn set_pr_merged(&self, task_id: &str, merged: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET pr_merged = ?2 WHERE id = ?1",
            params![task_id, merged as i64],
        )?;
        Ok(())
    }

    pub fn set_local_done(&self, task_id: &str, done: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET local_done = ?2 WHERE id = ?1",
            params![task_id, done as i64],
        )?;
        Ok(())
    }

    /// Persist the derived state so external readers (and the `state` column in
    /// the spec's schema) stay meaningful. Reads still derive.
    pub fn store_derived_state(&self, task_id: &str, state: BoardState) -> Result<()> {
        self.conn.execute(
            "UPDATE tasks SET state = ?2 WHERE id = ?1",
            params![task_id, state.as_str()],
        )?;
        Ok(())
    }

    pub fn load_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source, source_id, identifier, title, body, url, labels,
                    state, source_state, upstream,
                    local_done, pr_url, pr_number, pr_open, pr_merged,
                    pr_mergeable, updated_at, synced_at,
                    pr_base_ref, pr_stack_number, pr_stack_size,
                    pr_stack_position, pr_stack_base_ref, pr_changes_requested,
                    pr_head_ref
             FROM tasks",
        )?;
        let rows = stmt.query_map([], |r| {
            let labels: String = r.get(7)?;
            Ok(Task {
                id: r.get(0)?,
                source: Source::parse(&r.get::<_, String>(1)?).unwrap_or(Source::Github),
                source_id: r.get(2)?,
                identifier: r.get(3)?,
                title: r.get(4)?,
                body: r.get(5)?,
                url: r.get(6)?,
                labels: serde_json::from_str(&labels).unwrap_or_default(),
                state: BoardState::parse(&r.get::<_, String>(8)?).unwrap_or(BoardState::Ready),
                source_state: r.get(9)?,
                upstream: UpstreamState::parse(&r.get::<_, String>(10)?)
                    .unwrap_or(UpstreamState::Unstarted),
                local_done: r.get::<_, i64>(11)? != 0,
                pr_url: r.get(12)?,
                pr_number: r.get(13)?,
                pr_open: r.get::<_, i64>(14)? != 0,
                pr_merged: r.get::<_, i64>(15)? != 0,
                pr_mergeable: r.get(16)?,
                updated_at: r.get(17)?,
                synced_at: r.get(18)?,
                pr_base_ref: r.get(19)?,
                // The stack number is the witness: without an identity there is
                // no stack to speak of, whatever the other three columns hold.
                pr_stack: match r.get::<_, Option<i64>>(20)? {
                    None => None,
                    Some(number) => Some(PrStack {
                        number,
                        size: r.get(21)?,
                        position: r.get(22)?,
                        base_ref: r.get(23)?,
                    }),
                },
                pr_changes_requested: r.get(24)?,
                pr_head_ref: r.get(25)?,
                attempts: Vec::new(),
            })
        })?;
        let mut tasks: Vec<Task> = rows.collect::<rusqlite::Result<_>>()?;
        for t in &mut tasks {
            t.attempts = self.attempts_for(&t.id)?;
        }
        Ok(tasks)
    }

    pub fn get_task(&self, id: &str) -> Result<Option<Task>> {
        Ok(self.load_tasks()?.into_iter().find(|t| t.id == id))
    }

    // ---- attempts -------------------------------------------------------
    //
    // Two queries read attempt rows and they must agree on the column order, so
    // both share [`ATTEMPT_COLUMNS`] and [`read_attempt`]. They used to carry a
    // copy each, which is a standing invitation for a column added to one and
    // forgotten in the other.

    pub fn attempts_for(&self, task_id: &str) -> Result<Vec<Attempt>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ATTEMPT_COLUMNS} FROM attempts WHERE task_id = ?1 ORDER BY id"
        ))?;
        let rows = stmt.query_map(params![task_id], read_attempt)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Insert a live attempt. Fails cleanly if one is already open for this task
    /// — that is the duplicate-dispatch guard, enforced by a partial unique
    /// index rather than a read-then-write race.
    pub fn insert_attempt(&self, a: &NewAttempt) -> Result<i64> {
        self.insert_attempt_with_ownership(a, true)
    }

    /// Insert review provenance for an ordinary Comet chat.
    ///
    /// Unlike a Board dispatch, this checkout and chat remain owned by the
    /// person who created them. Record that distinction in the same write as
    /// the attempt so a crash can never expose their work to Board retention.
    pub fn insert_adopted_attempt(&self, a: &NewAttempt) -> Result<i64> {
        self.insert_attempt_with_ownership(a, false)
    }

    fn insert_attempt_with_ownership(&self, a: &NewAttempt, board_managed: bool) -> Result<i64> {
        let res = self.conn.execute(
            "INSERT INTO attempts
               (task_id, pane_id, workspace, runtime, worktree, branch,
                dispatched_by, dispatched_by_pane, started_at, base_sha, account,
                repo_path,
                dispatched_by_device, dispatched_by_user, dispatched_by_verified,
                billed_to, board_managed, stacked_on, automation, automation_owner)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
            params![
                a.task_id,
                a.pane_id,
                a.workspace,
                a.runtime,
                a.worktree,
                a.branch,
                a.dispatched_by,
                a.dispatched_by_pane,
                now(),
                a.base_sha,
                a.account,
                a.repo_path,
                a.dispatched_by_device,
                a.dispatched_by_user,
                a.dispatched_by_verified as i64,
                a.billed_to,
                board_managed as i64,
                a.stacked_on,
                a.automation,
                a.automation_owner
            ],
        );
        match res {
            Ok(_) => Ok(self.conn.last_insert_rowid()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                anyhow::bail!("{} already has a live attempt", a.task_id)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_attempt_pane(&self, attempt_id: i64, pane_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET pane_id = ?2 WHERE id = ?1",
            params![attempt_id, pane_id],
        )?;
        Ok(())
    }

    /// Record where the attempt's checkout actually landed. Known only after
    /// the engine cuts it — the row is inserted first as the duplicate-dispatch
    /// guard, so the path arrives as an update.
    pub fn set_attempt_worktree(&self, attempt_id: i64, worktree: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET worktree = ?2 WHERE id = ?1",
            params![attempt_id, worktree],
        )?;
        Ok(())
    }

    /// Start or stop the retention clock on an attempt's checkout (gh#72).
    ///
    /// `true` stamps *now* only if nothing is stamped — the window is measured
    /// from when the checkout first became nobody's, and re-stamping every
    /// sweep would keep pushing collection a cycle further out, forever.
    pub fn set_attempt_collectable(&self, attempt_id: i64, collectable: bool) -> Result<()> {
        if collectable {
            self.conn.execute(
                "UPDATE attempts SET collectable_at = ?2
                  WHERE id = ?1 AND collectable_at IS NULL",
                params![attempt_id, now()],
            )?;
        } else {
            self.conn.execute(
                "UPDATE attempts SET collectable_at = NULL WHERE id = ?1",
                params![attempt_id],
            )?;
        }
        Ok(())
    }

    /// Record that this attempt's checkout and branch are gone (gh#72). The
    /// `worktree` path is kept: it is where the work happened, and a row that
    /// forgot it could not say what it collected.
    pub fn set_attempt_collected(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET collected_at = ?2 WHERE id = ?1",
            params![attempt_id, now()],
        )?;
        Ok(())
    }

    /// Start or stop the sweep clock on the build output inside an attempt's
    /// checkout (gh#186).
    ///
    /// Stamps *now* only if nothing is stamped, for
    /// [`Self::set_attempt_collectable`]'s reason: the window is measured from
    /// when the cache first became nobody's, and re-stamping every sweep would
    /// push the sweep a cycle further out forever.
    pub fn set_attempt_cache_sweepable(&self, attempt_id: i64, sweepable: bool) -> Result<()> {
        if sweepable {
            self.conn.execute(
                "UPDATE attempts SET cache_sweepable_at = ?2
                  WHERE id = ?1 AND cache_sweepable_at IS NULL",
                params![attempt_id, now()],
            )?;
        } else {
            self.conn.execute(
                "UPDATE attempts SET cache_sweepable_at = NULL WHERE id = ?1",
                params![attempt_id],
            )?;
        }
        Ok(())
    }

    /// Record that the build output inside this attempt's checkout is gone
    /// (gh#186).
    ///
    /// Deliberately *not* [`Self::set_attempt_collected`]: `collected_at` means
    /// the worktree and its branch are gone, and a cache sweep is not that. A
    /// board that recorded one as the other would report checkouts reclaimed
    /// that are still on the disk — and would then never reclaim them.
    pub fn set_attempt_cache_swept(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET cache_swept_at = ?2 WHERE id = ?1",
            params![attempt_id, now()],
        )?;
        Ok(())
    }

    /// Forget that the board swept this checkout's build output (gh#186) — the
    /// re-open path, where an agent is about to build in there again.
    ///
    /// Both stamps go. A cache is the one thing here that comes *back*: unlike a
    /// deleted worktree or an archived chat, the next build recreates it, and a
    /// `cache_swept_at` left standing would keep the attempt out of the sweep
    /// for good while a fresh 36 GB accumulated behind it.
    pub fn clear_attempt_cache_swept(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET cache_swept_at = NULL, cache_sweepable_at = NULL
              WHERE id = ?1",
            params![attempt_id],
        )?;
        Ok(())
    }

    /// Start or stop the shelf clock on an attempt's chat (gh#139).
    ///
    /// Stamps *now* only if nothing is stamped, for the same reason
    /// [`Self::set_attempt_collectable`] does: the window is measured from when
    /// the chat first became nobody's, and re-stamping every sweep would push
    /// the archive a cycle further out forever.
    pub fn set_attempt_chat_archivable(&self, attempt_id: i64, archivable: bool) -> Result<()> {
        if archivable {
            self.conn.execute(
                "UPDATE attempts SET chat_archivable_at = ?2
                  WHERE id = ?1 AND chat_archivable_at IS NULL",
                params![attempt_id, now()],
            )?;
        } else {
            self.conn.execute(
                "UPDATE attempts SET chat_archivable_at = NULL WHERE id = ?1",
                params![attempt_id],
            )?;
        }
        Ok(())
    }

    /// Record that this attempt's chat is off the shelf (gh#139). `pane_id` is
    /// kept: an archived chat still exists, and a row that forgot its id could
    /// neither name it nor put it back.
    pub fn set_attempt_chat_archived(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET chat_archived_at = ?2 WHERE id = ?1",
            params![attempt_id, now()],
        )?;
        Ok(())
    }

    /// Forget that the board archived this attempt's chat (gh#139) — the
    /// re-open path, which puts the chat back on the shelf in the same motion.
    /// Both stamps go: the attempt is live again, so it is owed a whole window
    /// when it next finishes.
    pub fn clear_attempt_chat_archived(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET chat_archived_at = NULL, chat_archivable_at = NULL
              WHERE id = ?1",
            params![attempt_id],
        )?;
        Ok(())
    }

    /// Closed attempts whose chat is still on a shelf — `doctor`'s census of
    /// what the board is holding there (gh#139).
    ///
    /// The mirror of [`Self::collectable_attempts`], on the chat instead of the
    /// checkout, and read for the same reason: the sweep itself walks tasks
    /// (it needs each task's route to know the window), so this exists to
    /// answer "how many, and how many are on the clock" in one query.
    pub fn archivable_chat_attempts(&self) -> Result<Vec<Attempt>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ATTEMPT_COLUMNS} FROM attempts
              WHERE outcome IS NOT NULL
                AND board_managed = 1
                AND pane_id IS NOT NULL
                AND chat_archived_at IS NULL
              ORDER BY id"
        ))?;
        let rows = stmt.query_map([], read_attempt)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Closed attempts still holding a checkout the board could reclaim — the
    /// gc sweep's candidate set (gh#72).
    ///
    /// Live attempts are excluded here as well as by [`crate::gc::standing`]:
    /// the sweep must never so much as consider a running agent's checkout, and
    /// the two guards are cheap. Already-collected rows are excluded so a box
    /// that has been up for months does not re-walk every attempt it ever made.
    pub fn collectable_attempts(&self) -> Result<Vec<Attempt>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ATTEMPT_COLUMNS} FROM attempts
              WHERE outcome IS NOT NULL
                AND board_managed = 1
                AND worktree IS NOT NULL
                AND collected_at IS NULL
              ORDER BY id"
        ))?;
        let rows = stmt.query_map([], read_attempt)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn close_attempt(&self, attempt_id: i64, outcome: Outcome) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET outcome = ?2, ended_at = ?3 WHERE id = ?1 AND outcome IS NULL",
            params![attempt_id, outcome.as_str(), now()],
        )?;
        Ok(())
    }

    /// Remove an attempt whose dispatch was refused before it cut anything
    /// (gh#410): no branch, no chat, no agent — nothing in the world refers to
    /// this row, so it is deleted rather than closed `failed`. A `failed` row
    /// here would burn an attempt on a pre-flight refusal, and the count is
    /// what a reader uses to judge whether a task is going badly.
    ///
    /// Guarded to rows that never got a pane and are still open: a row a chat
    /// ever ran under is history, and history closes through
    /// [`Self::close_attempt`].
    pub fn delete_attempt(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM attempts WHERE id = ?1 AND pane_id IS NULL AND outcome IS NULL",
            params![attempt_id],
        )?;
        Ok(())
    }

    /// Rewrite a closed attempt's outcome — the panel's cancel on a `failed`
    /// row (gh#42). The attempt is already over, so `ended_at` is left alone;
    /// only the verdict changes, and flipping `failed`/`orphaned` to `cancelled`
    /// is what re-derives the issue to `ready`. Guarded to closed rows: a live
    /// attempt ends through [`Self::close_attempt`], which owns the timestamp.
    pub fn set_attempt_outcome(&self, attempt_id: i64, outcome: Outcome) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET outcome = ?2 WHERE id = ?1 AND outcome IS NOT NULL",
            params![attempt_id, outcome.as_str()],
        )?;
        Ok(())
    }

    /// Record what an attempt has spent, and what spent it (gh#151).
    ///
    /// The authoritative four buckets and model are written in the same
    /// statement as their two optional attribution views, so `input_tokens IS
    /// NULL` remains a sound witness for "this attempt reported nothing" — the
    /// reader keys the whole [`Attempt::tokens`] option on it. Written on every
    /// reconcile of a live attempt, and the totals only ever grow, so a run
    /// that ends between two ticks keeps the last figure its journal gave.
    ///
    /// `model` is left alone when the run did not name one, rather than
    /// overwriting a model a previous tick did learn.
    pub fn set_attempt_tokens(
        &self,
        attempt_id: i64,
        usage: comet_proto::TokenUsage,
        model: Option<&str>,
        by_model: Option<&[comet_proto::ModelTokenUsage]>,
        by_agent: Option<&[comet_proto::AgentTokenUsage]>,
    ) -> Result<()> {
        let by_model = by_model.map(serde_json::to_string).transpose()?;
        let by_agent = by_agent.map(serde_json::to_string).transpose()?;
        self.conn.execute(
            "UPDATE attempts SET input_tokens = ?2, output_tokens = ?3,
                 cache_read_tokens = ?4, cache_creation_tokens = ?5,
                 model = COALESCE(?6, model), token_models = ?7, token_agents = ?8
               WHERE id = ?1",
            params![
                attempt_id,
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                usage.cache_read_tokens as i64,
                usage.cache_creation_tokens as i64,
                model,
                by_model,
                by_agent,
            ],
        )?;
        Ok(())
    }

    /// Record how full an attempt's context window is (gh#271).
    ///
    /// **Overwrites, never accumulates** — the opposite of
    /// [`Self::set_attempt_tokens`] beside it, and the difference is the whole
    /// point: spend is a total that only grows, fullness is a level that can
    /// (and after a compaction, does) fall. The three columns are written
    /// together so `context_max_tokens IS NULL` is a sound witness for "this
    /// attempt reported no window", which the reader keys the option on.
    pub fn set_attempt_context(
        &self,
        attempt_id: i64,
        context: comet_proto::ContextUsage,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET context_used_tokens = ?2, context_max_tokens = ?3,
                 context_compact_at_tokens = ?4
               WHERE id = ?1",
            params![
                attempt_id,
                context.used_tokens as i64,
                context.max_tokens as i64,
                context.compact_at_tokens.map(|at| at as i64),
            ],
        )?;
        Ok(())
    }

    // ---- the review contract (§gh#183) -----------------------------------

    /// Record what an agent says this attempt did.
    ///
    /// Replaces the set rather than appending to it: claims are an attempt's
    /// answer to one question, an agent that submits twice is correcting
    /// itself, and appending would leave a review reading a superseded claim
    /// beside the one that replaced it. `claims_at` moves with them, so the
    /// stamp always says when the *current* set was written.
    ///
    /// Clears `claims_error` for the same reason (§gh#235): a set that parsed
    /// supersedes the block that did not, and a review showing both would be
    /// showing a draft beside its correction.
    pub fn set_attempt_claims(
        &self,
        attempt_id: i64,
        claims: &[crate::claims::Claim],
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET claims = ?2, claims_at = ?3, claims_error = NULL WHERE id = ?1",
            params![attempt_id, serde_json::to_string(claims)?, now()],
        )?;
        Ok(())
    }

    /// Record that this attempt wrote a claims block the board could not read
    /// (§gh#235).
    ///
    /// Written instead of the claims, never beside them: a row where
    /// `claims_at` is NULL and this is set is an attempt that answered and was
    /// not understood, which is the one claim state that has to be loud.
    pub fn set_attempt_claims_error(&self, attempt_id: i64, error: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET claims_error = ?2 WHERE id = ?1",
            params![attempt_id, error],
        )?;
        Ok(())
    }

    /// The branch diff the board snapshotted off this attempt while it was
    /// still live (§gh#183). Empty for an attempt that never had one taken —
    /// every row from before this existed, and any attempt whose checkout the
    /// board could not read.
    ///
    /// Its own query rather than a column on [`Attempt`]: the board loads every
    /// attempt of every task on every sync cycle, and a diff per row parsed
    /// each time would be work nobody asked for. This is read when somebody
    /// opens a review.
    pub fn attempt_changes(&self, attempt_id: i64) -> Result<Vec<crate::claims::ChangedFile>> {
        Ok(self
            .attempt_json("changed_files", attempt_id)?
            .unwrap_or_default())
    }

    /// Snapshot the branch diff onto the attempt, so the remainder survives the
    /// checkout being reclaimed (gh#72).
    pub fn set_attempt_changes(
        &self,
        attempt_id: i64,
        changed: &[crate::claims::ChangedFile],
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET changed_files = ?2 WHERE id = ?1",
            params![attempt_id, serde_json::to_string(changed)?],
        )?;
        Ok(())
    }

    /// What the run's own journal says it did (§gh#183). `None` is "never
    /// recorded", which is not the same as a run that executed nothing —
    /// [`crate::evidence::RunEvidence::default`] is that one.
    pub fn attempt_evidence(
        &self,
        attempt_id: i64,
    ) -> Result<Option<crate::evidence::RunEvidence>> {
        self.attempt_json("run_evidence", attempt_id)
    }

    pub fn set_attempt_evidence(
        &self,
        attempt_id: i64,
        evidence: &crate::evidence::RunEvidence,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET run_evidence = ?2 WHERE id = ?1",
            params![attempt_id, serde_json::to_string(evidence)?],
        )?;
        Ok(())
    }

    /// What sandbox this attempt's run actually got (§gh#349). `None` is "never
    /// recorded" — every row from before it was, and every attempt whose
    /// journal the board could not read while it was live. Distinct from a
    /// recorded `read-only`, and a review must not render the two the same.
    pub fn attempt_sandbox(&self, attempt_id: i64) -> Result<Option<comet_proto::SandboxReport>> {
        self.attempt_json("run_sandbox", attempt_id)
    }

    pub fn set_attempt_sandbox(
        &self,
        attempt_id: i64,
        sandbox: &comet_proto::SandboxReport,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET run_sandbox = ?2 WHERE id = ?1",
            params![attempt_id, serde_json::to_string(sandbox)?],
        )?;
        Ok(())
    }

    /// What the board derived from this attempt's branch (§gh#236). `None` is
    /// "never recorded" — every row from before this existed, and every attempt
    /// whose checkout the board could not read while it was live.
    ///
    /// Snapshotted for [`Db::attempt_changes`]'s reason: the effects are read
    /// from the checkout, and the checkout is reclaimed (gh#72). A review
    /// opened after that shows what the board saw while it could still see.
    pub fn attempt_effects(&self, attempt_id: i64) -> Result<Option<crate::effects::Effects>> {
        self.attempt_json("effects", attempt_id)
    }

    pub fn set_attempt_effects(
        &self,
        attempt_id: i64,
        effects: &crate::effects::Effects,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET effects = ?2 WHERE id = ?1",
            params![attempt_id, serde_json::to_string(effects)?],
        )?;
        Ok(())
    }

    /// One JSON column off one attempt. Unparseable JSON reads as absent for
    /// [`read_attempt`]'s reason: a review field must never be what stops a
    /// row being read.
    fn attempt_json<T: serde::de::DeserializeOwned>(
        &self,
        column: &str,
        attempt_id: i64,
    ) -> Result<Option<T>> {
        let stored: Option<String> = self
            .conn
            .query_row(
                &format!("SELECT {column} FROM attempts WHERE id = ?1"),
                params![attempt_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        Ok(stored.and_then(|j| serde_json::from_str(&j).ok()))
    }

    /// One attempt by id, whatever task it belongs to — what a review is
    /// addressed to.
    pub fn get_attempt(&self, attempt_id: i64) -> Result<Option<Attempt>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {ATTEMPT_COLUMNS} FROM attempts WHERE id = ?1"),
                params![attempt_id],
                read_attempt,
            )
            .optional()?)
    }

    pub fn set_attempt_status(&self, attempt_id: i64, status: AgentStatus) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET agent_status = ?2 WHERE id = ?1",
            params![attempt_id, status.as_str()],
        )?;
        Ok(())
    }

    /// Correct the attempt's starting commit once its checkout exists.
    ///
    /// Recorded provisionally from the repo's HEAD before dispatch, because the
    /// row is inserted before the worktree is cut — but a retry reuses a branch
    /// that already carries the previous attempt's commits, and measuring from
    /// the repo HEAD counts those as this attempt's output.
    pub fn set_attempt_base_sha(&self, attempt_id: i64, sha: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET base_sha = ?2 WHERE id = ?1",
            params![attempt_id, sha],
        )?;
        Ok(())
    }

    /// Latch that this attempt has been seen working. Never cleared: it records
    /// that the agent got going at all, not what it is doing now.
    pub fn set_saw_working(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET saw_working = 1 WHERE id = ?1",
            params![attempt_id],
        )?;
        Ok(())
    }

    /// Record that this attempt has entered blocked once more, and say which
    /// block that is (1 for the first).
    ///
    /// The counter is what makes the upstream notice "once per block" rather
    /// than once per attempt or once per tick: it goes into the writeback's
    /// idempotency key, so a block that is answered and then happens again
    /// earns a second comment while the ticks in between earn none. Bumped in
    /// the same statement it is read back from, so two reconcile paths racing
    /// the same transition cannot both claim block 1 (gh#71).
    pub fn bump_blocked_count(&self, attempt_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "UPDATE attempts SET blocked_count = blocked_count + 1
              WHERE id = ?1 RETURNING blocked_count",
            params![attempt_id],
            |r| r.get(0),
        )?)
    }

    pub fn set_missing_ticks(&self, attempt_id: i64, ticks: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET missing_ticks = ?2 WHERE id = ?1",
            params![attempt_id, ticks],
        )?;
        Ok(())
    }

    /// Count a run the board restarted in this attempt's own chat, and clear
    /// the absence that led to it (gh#390).
    ///
    /// One statement for both, because they are one fact: the run that was
    /// missing has been asked to come back, so the ticks that measured its
    /// absence are spent — the next verdict has to earn its own two.
    ///
    /// Returns the new count, which is what bounds the restarting
    /// ([`crate::runs::MAX_RESUMES`]) and what the prompt tells the agent.
    pub fn note_resume(&self, attempt_id: i64) -> Result<i64> {
        self.conn.execute(
            "UPDATE attempts SET resumes = resumes + 1, missing_ticks = 0 WHERE id = ?1",
            params![attempt_id],
        )?;
        Ok(self.conn.query_row(
            "SELECT resumes FROM attempts WHERE id = ?1",
            params![attempt_id],
            |r| r.get(0),
        )?)
    }

    /// Every attempt that *started* since `since`, closed or not (gh#390).
    ///
    /// `started_at` rather than `ended_at` because the question this answers is
    /// about the box's recent behaviour, and an attempt that started inside the
    /// window and is still running is part of that behaviour — it is the
    /// evidence that runs can, in fact, still start here.
    pub fn attempts_since(&self, since: &str) -> Result<Vec<Attempt>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ATTEMPT_COLUMNS} FROM attempts
              WHERE started_at >= ?1 AND board_managed = 1 ORDER BY started_at"
        ))?;
        let rows = stmt.query_map(params![since], read_attempt)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Record the screen this pane is showing, and that it is showing it *now*.
    ///
    /// Only called when the screen has changed, which is what makes `screen_at`
    /// mean "first seen": a screen resampled every few seconds has to keep
    /// ageing, or it never reaches `screen::STALL_SECS` and a frozen pane is
    /// watched forever (gh#32).
    pub fn set_screen_sample(&self, attempt_id: i64, fingerprint: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET screen_print = ?2, screen_at = ?3 WHERE id = ?1",
            params![attempt_id, fingerprint, now()],
        )?;
        Ok(())
    }

    /// Forget the screen recorded for this attempt.
    ///
    /// Called when an attempt is closed. The screen it was last seen showing was
    /// recorded while its agent was still working, so it is no baseline for the
    /// question the closed-attempt watch asks — *has this pane moved since we
    /// called it finished* — and comparing against it would answer yes on the
    /// very first look (gh#34).
    pub fn clear_screen_sample(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET screen_print = NULL, screen_at = NULL WHERE id = ?1",
            params![attempt_id],
        )?;
        Ok(())
    }

    /// Count a nudge sent into this attempt's pane, and stamp when (gh#40).
    ///
    /// Counted whatever the delivery reported, because the cap is what bounds
    /// this: a `herdr agent prompt` that keeps failing is a thing to try a few
    /// times and then stop, exactly like one that keeps being ignored, and a
    /// refusal that did not count would leave the board retrying it every cycle
    /// forever. Which it was is in the log line beside it.
    pub fn set_nudged(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET nudges = nudges + 1, nudged_at = ?2 WHERE id = ?1",
            params![attempt_id, now()],
        )?;
        Ok(())
    }

    /// Give an attempt its full allowance of nudges back, because its pane came
    /// back to life.
    ///
    /// Not called on the first sign of movement: a nudge lands as text and gets
    /// echoed, so the screen moves once for a nudge that achieved nothing. See
    /// [`crate::nudge::recovered`] for what is waited for instead.
    pub fn clear_nudges(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET nudges = 0, nudged_at = NULL WHERE id = ?1",
            params![attempt_id],
        )?;
        Ok(())
    }

    /// Stamp that this attempt has been warned about its wall-clock cap, and
    /// start the grace clock (gh#70).
    ///
    /// Guarded to a row with no stamp, so the interval reconcile cannot push
    /// the deadline out by re-stamping every cycle — an ageless warning is a
    /// cancel that never arrives, which is the failure mode this whole check
    /// exists to fix.
    pub fn set_overrun_warned(&self, attempt_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET overrun_warned_at = ?2
              WHERE id = ?1 AND overrun_warned_at IS NULL",
            params![attempt_id, now()],
        )?;
        Ok(())
    }

    /// Start or stop the clock on an attempt that looks finished.
    ///
    /// `Some` only ever on the first sample of a run — restamping it on every
    /// sample is what would make it ageless, and an ageless stamp can never
    /// reach [`crate::settled::SETTLE_SECS`]. Cleared, not latched: an agent that
    /// goes back to work starts over.
    pub fn set_settled_at(&self, attempt_id: i64, at: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE attempts SET settled_at = ?2 WHERE id = ?1",
            params![attempt_id, at],
        )?;
        Ok(())
    }

    /// Put a closed attempt back to work, because its pane never stopped.
    ///
    /// Re-opening the same row rather than inserting a new one is the decision
    /// gh#34 had to make. An attempt is one agent, in one pane, on one branch,
    /// and none of those changed: nobody dispatched anything, so a second row
    /// would claim a retry that never happened and inflate the very number
    /// `stats` reports as "needed more than one go". What must not be lost —
    /// that the board once said this was finished — is a property of *this*
    /// attempt, so it is counted here instead.
    ///
    /// `ended_at` goes with the outcome. Every other reader treats it as proof
    /// the attempt is over (`gc` prunes by it, `stats` measures durations with
    /// it), so a live row carrying one would lie to all of them.
    ///
    /// The overrun warning goes too (gh#70). `started_at` is not reset — the
    /// cap bounds the attempt, and a re-opened one has been live the whole time
    /// — but a row that comes back to work is owed the same warning and the
    /// same grace as any other, not a cancel on the next tick from a stamp it
    /// collected before the board called it finished.
    ///
    /// Returns `false` when the task already has a live attempt — somebody
    /// re-dispatched it, that attempt is the current one, and the partial unique
    /// index would refuse this anyway. Checked in the same statement rather than
    /// read first, so a dispatch racing this loses cleanly.
    pub fn reopen_attempt(&self, attempt_id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE attempts SET outcome = NULL, ended_at = NULL, settled_at = NULL,
                    missing_ticks = 0, screen_print = NULL, screen_at = NULL,
                    nudges = 0, nudged_at = NULL,
                    overrun_warned_at = NULL,
                    reopened = reopened + 1
             WHERE id = ?1 AND outcome IS NOT NULL
               AND NOT EXISTS (
                 SELECT 1 FROM attempts o
                  WHERE o.task_id = attempts.task_id AND o.outcome IS NULL
               )",
            params![attempt_id],
        )?;
        Ok(n > 0)
    }

    /// Live attempts across all tasks, for reconciliation and concurrency caps.
    pub fn live_attempts(&self) -> Result<Vec<Attempt>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ATTEMPT_COLUMNS} FROM attempts
              WHERE outcome IS NULL AND board_managed = 1 ORDER BY id"
        ))?;
        let rows = stmt.query_map([], read_attempt)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Closed attempts whose pane the board might still have to look at.
    ///
    /// Reconciliation only ever visited live attempts, which made every settle
    /// final whether or not it was right (gh#34). These are the rows it has to
    /// look at again, and the filter is what keeps that cheap:
    ///
    /// - `done` and `orphaned` only. Those are the board's own verdicts on
    ///   evidence, and evidence can be wrong. `cancelled` is the operator ending
    ///   an attempt on purpose and `failed` is a dispatch that never produced an
    ///   agent — neither is ours to undo.
    /// - A pane to look at. An attempt with none was never in one.
    pub fn settled_attempts(&self) -> Result<Vec<Attempt>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {ATTEMPT_COLUMNS} FROM attempts
              WHERE outcome IN ('done', 'orphaned')
                AND board_managed = 1
                AND pane_id IS NOT NULL
              ORDER BY id"
        ))?;
        let rows = stmt.query_map([], read_attempt)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The live attempt that owns a pane, if any.
    ///
    /// This names a *board-dispatched* agent by its task. It is not how an
    /// agent is recognised — an orchestrator pane owns no attempt and would
    /// answer `None` here — only how one that has a task on the board gets the
    /// richer label. See `dispatch::dispatcher_from`.
    pub fn live_attempt_for_pane(&self, pane_id: &str) -> Result<Option<Attempt>> {
        Ok(self
            .live_attempts()?
            .into_iter()
            .find(|a| a.pane_id.as_deref() == Some(pane_id)))
    }

    /// Live attempts in a workspace — what `max_concurrent_per_workspace`
    /// counts. `blocked` is included because it still holds a pane.
    pub fn live_count_in_workspace(&self, workspace: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM attempts
              WHERE outcome IS NULL AND board_managed = 1 AND workspace = ?1",
            params![workspace],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    // ---- automation log (gh#490) ----------------------------------------

    /// Live attempts released by one auto-pick rule — what the rule's own
    /// `max_concurrent` counts.
    pub fn live_count_for_automation(&self, rule: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM attempts
              WHERE outcome IS NULL AND automation = ?1",
            params![rule],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Attempts one rule released since `since` — what its `daily_budget`
    /// counts. Off the attempts table rather than the log, because the attempt
    /// row is the record of a dispatch that *happened* and survives a pruned
    /// log.
    pub fn automation_dispatches_since(&self, rule: &str, since: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM attempts
              WHERE automation = ?1 AND started_at >= ?2",
            params![rule, since],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Record what a rule decided about a task — or, with no task, about
    /// itself. An answer identical to the latest one for the same subject
    /// bumps that row's `last_at` and `count` instead of appending, so a
    /// standing condition is one legible streak rather than a page of
    /// repeats. A changed answer appends: history is the point.
    pub fn record_automation_decision(&self, d: &AutomationDecision) -> Result<()> {
        let latest = self
            .conn
            .query_row(
                "SELECT id, decision, reason, attempt_id FROM automation_log
                  WHERE rule = ?1 AND (task_id = ?2 OR (task_id IS NULL AND ?2 IS NULL))
                  ORDER BY id DESC LIMIT 1",
                params![d.rule, d.task_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()?;
        if let Some((id, decision, reason, attempt_id)) = latest
            && decision == d.decision
            && reason == d.reason
            && attempt_id == d.attempt_id
        {
            self.conn.execute(
                "UPDATE automation_log SET last_at = ?2, count = count + 1 WHERE id = ?1",
                params![id, now()],
            )?;
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO automation_log
               (rule, task_id, identifier, decision, reason, attempt_id, first_at, last_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
            params![
                d.rule,
                d.task_id,
                d.identifier,
                d.decision,
                d.reason,
                d.attempt_id,
                now()
            ],
        )?;
        Ok(())
    }

    /// When a rule last *refused* a task — a dispatch that was asked for and
    /// did not happen. What the refusal half of the cooldown is measured from;
    /// the failure half comes off the attempts table.
    pub fn automation_last_refusal(&self, rule: &str, task_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_at FROM automation_log
                  WHERE rule = ?1 AND task_id = ?2 AND decision = 'refused'
                  ORDER BY last_at DESC LIMIT 1",
                params![rule, task_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// When one rule's attempt on a task last *failed* — the other half of
    /// the cooldown.
    pub fn automation_last_failure(&self, rule: &str, task_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT ended_at FROM attempts
                  WHERE automation = ?1 AND task_id = ?2
                    AND outcome = 'failed' AND ended_at IS NOT NULL
                  ORDER BY ended_at DESC LIMIT 1",
                params![rule, task_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// The most recent log rows, newest first — the run/history view. `rule`
    /// scopes to one rule; `None` reads across all of them.
    pub fn automation_log_recent(
        &self,
        rule: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AutomationLogRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rule, task_id, identifier, decision, reason, attempt_id,
                    first_at, last_at, count
               FROM automation_log
              WHERE (?1 IS NULL OR rule = ?1)
              ORDER BY last_at DESC, id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![rule, limit as i64], |r| {
            Ok(AutomationLogRow {
                id: r.get(0)?,
                rule: r.get(1)?,
                task_id: r.get(2)?,
                identifier: r.get(3)?,
                decision: r.get(4)?,
                reason: r.get(5)?,
                attempt_id: r.get(6)?,
                first_at: r.get(7)?,
                last_at: r.get(8)?,
                count: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Drop log rows whose streak last moved before `before`. The log is an
    /// operator's view, not an audit archive; the attempts it led to are the
    /// durable record and keep themselves.
    pub fn prune_automation_log(&self, before: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM automation_log WHERE last_at < ?1",
            params![before],
        )?;
        Ok(())
    }

    /// Dispatches per agent-account slot since `since`, busiest first — what
    /// the automations page shows beside each account it offers (gh#524). Off
    /// the attempts table because an attempt row is the record of a dispatch
    /// that happened; NULL `account` rows (the box's own login) are nobody's
    /// slot and stay out.
    pub fn account_dispatch_totals(&self, since: &str) -> Result<Vec<(String, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT account, COUNT(*) FROM attempts
              WHERE account IS NOT NULL AND started_at >= ?1
              GROUP BY account ORDER BY COUNT(*) DESC, account",
        )?;
        let rows = stmt.query_map(params![since], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- writeback queue ------------------------------------------------

    /// Enqueue a writeback. Returns `false` when this exact effect is already
    /// queued or already delivered — idempotency lives in the index, so a
    /// retried sync cannot double-comment upstream.
    pub fn enqueue_writeback(&self, w: &NewWriteback) -> Result<bool> {
        let res = self.conn.execute(
            "INSERT INTO writeback_queue(task_id, kind, payload, idem_key, created_at)
             VALUES(?1,?2,?3,?4,?5)",
            params![w.task_id, w.kind, w.payload, w.idem_key, now()],
        );
        match res {
            Ok(_) => Ok(true),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Ok(false)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn pending_writebacks(&self, limit: usize) -> Result<Vec<Writeback>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_id, kind, payload, idem_key, attempts, next_try_at
             FROM writeback_queue
             WHERE done = 0 AND (next_try_at IS NULL OR next_try_at <= ?1)
             ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now(), limit as i64], |r| {
            Ok(Writeback {
                id: r.get(0)?,
                task_id: r.get(1)?,
                kind: r.get(2)?,
                payload: r.get(3)?,
                idem_key: r.get(4)?,
                attempts: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mark_writeback_done(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE writeback_queue SET done = 1, last_error = NULL WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// Record a failure and schedule the next try with exponential backoff,
    /// capped at 5 minutes (impl spec §7).
    pub fn defer_writeback(&self, id: i64, attempts: i64, err: &str) -> Result<()> {
        let delay = backoff_secs(attempts + 1);
        let next = chrono::Utc::now() + chrono::Duration::seconds(delay as i64);
        self.conn.execute(
            "UPDATE writeback_queue
               SET attempts = ?2, next_try_at = ?3, last_error = ?4
             WHERE id = ?1",
            params![id, attempts + 1, rfc3339(next), err],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn pending_writeback_count(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM writeback_queue WHERE done = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }
}

/// Exponential backoff capped at 5 minutes.
pub fn backoff_secs(attempt: i64) -> u64 {
    let base = 2u64.saturating_pow(attempt.clamp(0, 16) as u32) * 5;
    base.min(300)
}

/// What reaping did to one task — see [`Db::reap_task`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaped {
    /// Never dispatched, so there was nothing to keep. Row and all.
    Forgotten,
    /// Kept, marked `gone` upstream, with this many attempts still on it.
    Kept { attempts: usize },
}

pub struct UpsertTask {
    pub id: String,
    pub source: Source,
    pub source_id: String,
    pub identifier: String,
    pub title: String,
    pub body: Option<String>,
    pub url: String,
    pub labels: Vec<String>,
    pub source_state: Option<String>,
    pub upstream: UpstreamState,
    pub updated_at: String,
}

pub struct NewAttempt {
    pub task_id: String,
    pub pane_id: Option<String>,
    pub workspace: String,
    pub runtime: String,
    pub worktree: Option<String>,
    pub branch: Option<String>,
    /// Parent task id when the board dispatched the releasing agent too.
    /// `None` here does not mean the operator — see `dispatched_by_pane`.
    pub dispatched_by: Option<String>,
    /// The pane the dispatch ran from, when an agent was in it. Both fields
    /// `None` is the operator.
    pub dispatched_by_pane: Option<String>,
    /// The commit the attempt's branch was cut from, so "what did the agent
    /// produce" can be measured against the attempt's own starting point
    /// rather than against the remote.
    pub base_sha: Option<String>,
    /// The agent-account slot this attempt runs under — whose subscription it
    /// spends (gh#59). `None` is the device's own CLI login.
    pub account: Option<String>,
    /// The repo the attempt's worktree is cut from (gh#72). Known at dispatch
    /// and recorded then, because collection needs it once the checkout is
    /// gone: `git worktree prune` and `git branch -D` run in the repo.
    pub repo_path: Option<String>,
    /// The device the dispatch was issued from, as the caller reported it
    /// (gh#74). `None` for a dispatch that named none — the CLI on the box,
    /// and every attempt recorded before frontends sent it.
    pub dispatched_by_device: Option<String>,
    /// Which human released it (gh#74) — see the column comment in `migrate`.
    pub dispatched_by_user: Option<String>,
    /// Whether the edge verified that human (gh#161), or a frontend said so.
    pub dispatched_by_verified: bool,
    /// Whose subscription this attempt spends, as an email (gh#101). Resolved
    /// by the engine at dispatch, because which logins a device has saved is
    /// engine knowledge; `None` when it could not name one.
    pub billed_to: Option<String>,
    /// The attempt whose branch this one was cut from (gh#285) — set only for
    /// a dispatch that stacked onto a sibling. Written with the insert rather
    /// than as a later update, unlike the worktree and the chat: the parent is
    /// decided *before* anything is created, so a row that exists at all knows
    /// what it was cut from.
    pub stacked_on: Option<i64>,
    /// The auto-pick rule releasing this attempt, and that rule's human owner
    /// (gh#490). Both `None` on every dispatch a person or an orchestrating
    /// agent made — automation provenance marks the exception. Written with
    /// the insert, like `stacked_on`: the rule is decided before anything is
    /// created, and a crash must not leave an autonomous attempt unattributed.
    pub automation: Option<String>,
    pub automation_owner: Option<String>,
}

/// One decision to record in the automation log (gh#490) — see
/// [`Db::record_automation_decision`] for the streak semantics.
pub struct AutomationDecision {
    pub rule: String,
    /// `None` is a note about the rule itself rather than about one task.
    pub task_id: Option<String>,
    pub identifier: Option<String>,
    /// `dispatched` | `skipped` | `deferred` | `refused`.
    pub decision: String,
    pub reason: String,
    pub attempt_id: Option<i64>,
}

/// One row of the automation history, as the run view lists it (gh#490).
/// Serialized camelCase because it rides `ReadBoardAutomations` to viewports.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationLogRow {
    pub id: i64,
    pub rule: String,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub identifier: Option<String>,
    pub decision: String,
    pub reason: String,
    #[serde(default)]
    pub attempt_id: Option<i64>,
    pub first_at: String,
    pub last_at: String,
    pub count: i64,
}

pub struct NewWriteback {
    pub task_id: String,
    pub kind: String,
    pub payload: String,
    pub idem_key: String,
}

#[derive(Debug, Clone)]
pub struct Writeback {
    pub id: i64,
    pub task_id: String,
    pub kind: String,
    pub payload: String,
    pub idem_key: String,
    pub attempts: i64,
}

pub fn now() -> String {
    rfc3339(chrono::Utc::now())
}

/// All timestamps are UTC RFC3339 (impl spec §7).
pub fn rfc3339(t: chrono::DateTime<chrono::Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    /// Deliberately a *legacy* linear-sourced row (gh#471): the store must keep
    /// loading and mutating rows written while Linear was a connected source,
    /// and every generic test below doubles as that coverage.
    fn seed(db: &Db, id: &str) {
        db.upsert_task(&UpsertTask {
            id: id.into(),
            source: Source::Linear,
            source_id: "uuid".into(),
            identifier: "LIN-142".into(),
            title: "Add retry".into(),
            body: None,
            url: "https://linear.app/x/issue/LIN-142".into(),
            labels: vec!["herd".into()],
            source_state: Some("Todo".into()),
            upstream: UpstreamState::Unstarted,
            updated_at: now(),
        })
        .unwrap();
    }

    fn attempt(task: &str) -> NewAttempt {
        NewAttempt {
            automation: None,
            automation_owner: None,
            stacked_on: None,
            task_id: task.into(),
            pane_id: None,
            workspace: "offhand".into(),
            runtime: "claude-code".into(),
            worktree: None,
            branch: Some("board/lin-142".into()),
            dispatched_by: None,
            dispatched_by_pane: None,
            base_sha: None,
            account: None,
            repo_path: None,
            dispatched_by_device: None,
            dispatched_by_user: None,
            dispatched_by_verified: false,
            billed_to: None,
        }
    }

    /// gh#74: who released an attempt is stored on it, so the record survives
    /// the frontend that made the claim going away.
    #[test]
    fn an_attempt_remembers_the_device_and_human_that_released_it() {
        let db = Db::open_in_memory().unwrap();
        seed(&db, "linear:LIN-142");
        db.insert_attempt(&NewAttempt {
            automation: None,
            automation_owner: None,
            stacked_on: None,
            dispatched_by_device: Some("laptop-ana".into()),
            dispatched_by_user: Some("ana@example.com".into()),
            dispatched_by_verified: false,
            billed_to: None,
            ..attempt("linear:LIN-142")
        })
        .unwrap();
        let stored = db.attempts_for("linear:LIN-142").unwrap().remove(0);
        assert_eq!(stored.dispatched_by_device.as_deref(), Some("laptop-ana"));
        assert_eq!(
            stored.dispatched_by_user.as_deref(),
            Some("ana@example.com")
        );
    }

    /// gh#335: the board's own history is the only place that says this box
    /// stacks, so the count has to follow the topology write in both directions
    /// — a retargeted pull request that leaves its stack stops being evidence.
    #[test]
    fn the_stacked_count_follows_what_the_poll_last_saw() {
        let db = db();
        seed(&db, "linear:LIN-142");
        assert_eq!(db.stacked_task_count().unwrap(), 0);

        db.set_pr_topology(
            "linear:LIN-142",
            Some("main"),
            Some("layer-1"),
            Some(&PrStack {
                number: 7,
                size: Some(3),
                position: Some(1),
                base_ref: Some("main".into()),
            }),
        )
        .unwrap();
        assert_eq!(db.stacked_task_count().unwrap(), 1);

        // Its parent merged and GitHub retargeted it onto trunk: no longer a
        // layer, and no longer evidence of anything.
        db.set_pr_topology("linear:LIN-142", Some("main"), Some("layer-1"), None)
            .unwrap();
        assert_eq!(db.stacked_task_count().unwrap(), 0);
    }

    #[test]
    fn an_older_database_gains_the_columns_it_is_missing() {
        // The upgrade path: a database created before a column existed must not
        // take the board down on the next read.
        let dir = std::env::temp_dir().join(format!("hb-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        let _ = std::fs::remove_file(&path);

        // A first-release schema: no dispatched_by, no agent_status, none of
        // the columns added since.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (
                   id TEXT PRIMARY KEY, source TEXT NOT NULL, source_id TEXT NOT NULL,
                   identifier TEXT NOT NULL, title TEXT NOT NULL, body TEXT,
                   url TEXT NOT NULL, labels TEXT NOT NULL DEFAULT '[]',
                   state TEXT NOT NULL, source_state TEXT,
                   updated_at TEXT NOT NULL, synced_at TEXT NOT NULL);
                 CREATE TABLE attempts (
                   id INTEGER PRIMARY KEY, task_id TEXT NOT NULL,
                   pane_id TEXT, workspace TEXT NOT NULL, runtime TEXT NOT NULL,
                   worktree TEXT, branch TEXT, started_at TEXT NOT NULL,
                   ended_at TEXT, outcome TEXT);
                 INSERT INTO tasks VALUES('linear:LIN-1','linear','u','LIN-1','t',NULL,
                   'url','[]','ready',NULL,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z');
                 INSERT INTO attempts VALUES(1,'linear:LIN-1','w1:p1','ws','claude-code',
                   NULL,NULL,'2026-01-01T00:00:00Z',NULL,NULL);",
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let board_id = db.board_id().unwrap();
        let tasks = db.load_tasks().unwrap();
        assert_eq!(tasks.len(), 1, "the existing row survives the upgrade");
        assert_eq!(tasks[0].attempts.len(), 1);
        assert_eq!(tasks[0].attempts[0].dispatched_by, None);
        // An attempt recorded before accounts existed ran on the device's own
        // CLI login, which is exactly what NULL means (gh#59).
        assert_eq!(tasks[0].attempts[0].account, None);
        // Same for who released it: nobody said, and the box's own id would be
        // a guess dressed as a record (gh#74).
        assert_eq!(tasks[0].attempts[0].dispatched_by_device, None);
        assert_eq!(tasks[0].attempts[0].dispatched_by_user, None);
        assert!(!tasks[0].local_done);

        // And it is idempotent — opening again must not try to add them twice.
        let db = Db::open(&path).unwrap();
        assert_eq!(db.load_tasks().unwrap().len(), 1);
        assert_eq!(db.board_id().unwrap(), board_id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// gh#34 replaced a column rather than adding one, and the one it replaced
    /// was `NOT NULL`. Every board in use has it, `insert_attempt` no longer
    /// names it, and an insert that fell foul of that would take dispatch down.
    #[test]
    fn a_database_carrying_the_column_gh34_stopped_using_still_dispatches() {
        let dir = std::env::temp_dir().join(format!("hb-gh34-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("gh18.db");
        let _ = std::fs::remove_file(&path);

        // The gh#18 schema: `settled_ticks INTEGER NOT NULL`, no `settled_at`.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (
                   id TEXT PRIMARY KEY, source TEXT NOT NULL, source_id TEXT NOT NULL,
                   identifier TEXT NOT NULL, title TEXT NOT NULL, body TEXT,
                   url TEXT NOT NULL, labels TEXT NOT NULL DEFAULT '[]',
                   state TEXT NOT NULL, source_state TEXT,
                   updated_at TEXT NOT NULL, synced_at TEXT NOT NULL);
                 CREATE TABLE attempts (
                   id INTEGER PRIMARY KEY, task_id TEXT NOT NULL,
                   pane_id TEXT, workspace TEXT NOT NULL, runtime TEXT NOT NULL,
                   worktree TEXT, branch TEXT, started_at TEXT NOT NULL,
                   ended_at TEXT, outcome TEXT,
                   settled_ticks INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO tasks VALUES('linear:LIN-1','linear','u','LIN-1','t',NULL,
                   'url','[]','ready',NULL,'2026-01-01T00:00:00Z','2026-01-01T00:00:00Z');
                 INSERT INTO attempts VALUES(1,'linear:LIN-1','w1:p1','ws','claude-code',
                   NULL,NULL,'2026-01-01T00:00:00Z','2026-01-01T01:00:00Z','done',1);",
            )
            .unwrap();
        }

        let db = Db::open(&path).unwrap();
        let old = &db.load_tasks().unwrap()[0].attempts[0];
        assert_eq!(old.settled_at, None, "there was never a time to migrate");
        assert_eq!(old.reopened, 0);
        // The insert the abandoned NOT NULL column would have broken.
        let a = db.insert_attempt(&attempt("linear:LIN-1")).unwrap();
        db.set_settled_at(a, Some("2026-07-30T22:03:06Z")).unwrap();
        let attempts = db.attempts_for("linear:LIN-1").unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(
            attempts[1].settled_at.as_deref(),
            Some("2026-07-30T22:03:06Z")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_is_idempotent_and_refreshes_fields() {
        let db = db();
        seed(&db, "linear:LIN-142");
        seed(&db, "linear:LIN-142");
        assert_eq!(db.load_tasks().unwrap().len(), 1);
    }

    #[test]
    fn a_poll_cannot_clobber_local_done() {
        // Otherwise `d mark done` undoes itself on the next sync.
        let db = db();
        seed(&db, "linear:LIN-142");
        db.set_local_done("linear:LIN-142", true).unwrap();
        seed(&db, "linear:LIN-142");
        assert!(db.load_tasks().unwrap()[0].local_done);
    }

    /// Whose subscription an attempt spent, kept on the attempt (gh#59). The
    /// chat row carries it too, but chats get archived and routes get edited —
    /// the attempt is the record of what actually ran.
    #[test]
    fn an_attempt_records_the_account_it_ran_under() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let mut a = attempt("linear:LIN-142");
        a.account = Some("8f2c1d0a7b6e4539".into());
        let id = db.insert_attempt(&a).unwrap();
        let stored = db.attempts_for("linear:LIN-142").unwrap();
        assert_eq!(stored[0].id, id);
        assert_eq!(stored[0].account.as_deref(), Some("8f2c1d0a7b6e4539"));
        // …and it survives the attempt closing, which is when anyone asks.
        db.close_attempt(id, Outcome::Done).unwrap();
        assert_eq!(
            db.attempts_for("linear:LIN-142").unwrap()[0]
                .account
                .as_deref(),
            Some("8f2c1d0a7b6e4539")
        );
    }

    #[test]
    fn second_live_attempt_is_refused() {
        // Impl spec §7 duplicate dispatch: double enter, or pane racing picker.
        let db = db();
        seed(&db, "linear:LIN-142");
        db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        let err = db
            .insert_attempt(&attempt("linear:LIN-142"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("already has a live attempt"), "{err}");
    }

    #[test]
    fn a_new_attempt_is_allowed_once_the_last_one_closed() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.close_attempt(a, Outcome::Failed).unwrap();
        db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        assert_eq!(db.attempts_for("linear:LIN-142").unwrap().len(), 2);
    }

    #[test]
    fn concurrency_counts_only_live_attempts_in_that_workspace() {
        let db = db();
        seed(&db, "linear:LIN-1");
        seed(&db, "linear:LIN-2");
        let a1 = db.insert_attempt(&attempt("linear:LIN-1")).unwrap();
        db.insert_attempt(&attempt("linear:LIN-2")).unwrap();
        assert_eq!(db.live_count_in_workspace("offhand").unwrap(), 2);
        assert_eq!(db.live_count_in_workspace("fintech").unwrap(), 0);
        db.close_attempt(a1, Outcome::Done).unwrap();
        assert_eq!(db.live_count_in_workspace("offhand").unwrap(), 1);
    }

    // ---- re-opening a settle that was wrong (gh#34) ---------------------

    #[test]
    fn reopening_puts_the_same_attempt_back_to_work_rather_than_adding_one() {
        // The decision gh#34 had to make. Nobody dispatched anything, so a
        // second row would claim a retry that never happened.
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.set_attempt_pane(a, "w1:p9").unwrap();
        db.close_attempt(a, Outcome::Done).unwrap();

        assert!(db.reopen_attempt(a).unwrap());
        let attempts = db.attempts_for("linear:LIN-142").unwrap();
        assert_eq!(attempts.len(), 1, "no retry was invented");
        assert_eq!(attempts[0].outcome, None, "and it is live again");
        // `gc` prunes by `ended_at` and `stats` measures durations with it, so a
        // live row must not carry one.
        assert_eq!(attempts[0].ended_at, None);
        // The fact the board once called it finished is what is kept instead.
        assert_eq!(attempts[0].reopened, 1);
        db.close_attempt(a, Outcome::Done).unwrap();
        db.reopen_attempt(a).unwrap();
        assert_eq!(db.attempts_for("linear:LIN-142").unwrap()[0].reopened, 2);
    }

    #[test]
    fn a_task_that_was_re_dispatched_keeps_its_new_attempt() {
        // The unique index would refuse a second live row anyway; refusing in the
        // same statement is what makes a dispatch racing this lose cleanly.
        let db = db();
        seed(&db, "linear:LIN-142");
        let first = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.close_attempt(first, Outcome::Done).unwrap();
        let retry = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();

        assert!(!db.reopen_attempt(first).unwrap());
        let attempts = db.attempts_for("linear:LIN-142").unwrap();
        assert_eq!(attempts[0].outcome, Some(Outcome::Done), "left closed");
        assert_eq!(attempts[0].reopened, 0, "and not counted against");
        assert_eq!(attempts[1].id, retry);
    }

    #[test]
    fn a_live_attempt_is_not_something_to_reopen() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        assert!(!db.reopen_attempt(a).unwrap());
    }

    // ---- a refusal deleting the row it opened (gh#410) ------------------

    #[test]
    fn a_refused_dispatch_deletes_the_row_it_opened() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.delete_attempt(a).unwrap();
        assert!(db.attempts_for("linear:LIN-142").unwrap().is_empty());
    }

    #[test]
    fn delete_only_reaches_rows_nothing_ever_ran_under() {
        let db = db();
        seed(&db, "linear:LIN-142");
        // A row with a pane is history: a chat ran under it.
        let with_pane = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.set_attempt_pane(with_pane, "chat-1").unwrap();
        db.delete_attempt(with_pane).unwrap();
        db.close_attempt(with_pane, Outcome::Done).unwrap();
        // A closed row is history too, pane or not.
        let closed = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.close_attempt(closed, Outcome::Failed).unwrap();
        db.delete_attempt(closed).unwrap();

        let attempts = db.attempts_for("linear:LIN-142").unwrap();
        assert_eq!(attempts.len(), 2, "both survive the delete");
    }

    #[test]
    fn only_the_boards_own_verdicts_are_watched_after_they_close() {
        // `cancelled` is the operator ending an attempt on purpose and `failed`
        // is a dispatch that never produced an agent. Neither is the board's to
        // second-guess; `done` and `orphaned` are.
        let db = db();
        let mut watched = Vec::new();
        for (n, outcome) in [
            Outcome::Done,
            Outcome::Orphaned,
            Outcome::Cancelled,
            Outcome::Failed,
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("linear:LIN-{n}");
            seed(&db, &id);
            let a = db.insert_attempt(&attempt(&id)).unwrap();
            db.set_attempt_pane(a, &format!("w1:p{n}")).unwrap();
            db.close_attempt(a, outcome).unwrap();
            if matches!(outcome, Outcome::Done | Outcome::Orphaned) {
                watched.push(a);
            }
        }
        // ...and one with no pane, which was never in one to look at.
        seed(&db, "linear:LIN-9");
        let paneless = db.insert_attempt(&attempt("linear:LIN-9")).unwrap();
        db.close_attempt(paneless, Outcome::Done).unwrap();

        let ids: Vec<i64> = db
            .settled_attempts()
            .unwrap()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert_eq!(ids, watched);
    }

    #[test]
    fn a_closed_attempts_outcome_can_be_rewritten_but_not_a_live_ones() {
        // The panel's cancel on a `failed` row: `set_attempt_outcome` flips the
        // closed attempt to `cancelled` (gh#42). A live attempt must refuse —
        // that is `close_attempt`'s job, and it owns the timestamp.
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.set_attempt_pane(a, "w1:p9").unwrap();
        db.close_attempt(a, Outcome::Orphaned).unwrap();

        db.set_attempt_outcome(a, Outcome::Cancelled).unwrap();
        let attempts = db.attempts_for("linear:LIN-142").unwrap();
        assert_eq!(attempts[0].outcome, Some(Outcome::Cancelled));
        // The rewrite is a verdict change on an already-finished attempt, not a
        // re-close: `ended_at` is the orphaning's, untouched.
        assert!(attempts[0].ended_at.is_some());

        let b = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.set_attempt_outcome(b, Outcome::Cancelled).unwrap();
        assert_eq!(
            db.attempts_for("linear:LIN-142").unwrap()[1].outcome,
            None,
            "a live attempt is not rewritten"
        );
    }

    #[test]
    fn writeback_enqueue_is_idempotent() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let w = NewWriteback {
            task_id: "linear:LIN-142".into(),
            kind: "comment".into(),
            payload: "{}".into(),
            idem_key: "linear:LIN-142:dispatch:1".into(),
        };
        assert!(db.enqueue_writeback(&w).unwrap());
        // Same logical effect enqueued again — a retried sync must not produce
        // a second comment upstream.
        assert!(!db.enqueue_writeback(&w).unwrap());
        assert_eq!(db.pending_writebacks(10).unwrap().len(), 1);
    }

    #[test]
    fn a_delivered_writeback_is_never_re_enqueued() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let w = NewWriteback {
            task_id: "linear:LIN-142".into(),
            kind: "comment".into(),
            payload: "{}".into(),
            idem_key: "k1".into(),
        };
        db.enqueue_writeback(&w).unwrap();
        let pending = db.pending_writebacks(10).unwrap();
        db.mark_writeback_done(pending[0].id).unwrap();
        assert!(!db.enqueue_writeback(&w).unwrap());
        assert!(db.pending_writebacks(10).unwrap().is_empty());
    }

    #[test]
    fn deferred_writebacks_leave_the_ready_set_then_come_back() {
        let db = db();
        seed(&db, "linear:LIN-142");
        db.enqueue_writeback(&NewWriteback {
            task_id: "linear:LIN-142".into(),
            kind: "comment".into(),
            payload: "{}".into(),
            idem_key: "k1".into(),
        })
        .unwrap();
        let w = db.pending_writebacks(10).unwrap().remove(0);
        db.defer_writeback(w.id, w.attempts, "linear unreachable")
            .unwrap();
        // Backed off into the future, so it is not ready now...
        assert!(db.pending_writebacks(10).unwrap().is_empty());
        // ...but it is still pending and will drain when the source returns.
        assert_eq!(db.pending_writeback_count().unwrap(), 1);
    }

    #[test]
    fn backoff_grows_and_caps_at_five_minutes() {
        assert!(backoff_secs(1) < backoff_secs(3));
        assert_eq!(backoff_secs(20), 300);
    }

    #[test]
    fn reaping_a_task_nobody_worked_on_forgets_it_entirely() {
        // An issue created and deleted again five minutes later was noise; there
        // is no history to protect.
        let db = db();
        seed(&db, "linear:LIN-142");
        db.enqueue_writeback(&NewWriteback {
            task_id: "linear:LIN-142".into(),
            kind: "comment".into(),
            payload: "{}".into(),
            idem_key: "k".into(),
        })
        .unwrap();

        assert_eq!(db.reap_task("linear:LIN-142").unwrap(), Reaped::Forgotten);
        assert!(db.load_tasks().unwrap().is_empty());
        assert_eq!(db.pending_writeback_count().unwrap(), 0);
    }

    #[test]
    fn reaping_a_task_that_was_worked_on_keeps_its_attempts() {
        // AGE-6: the record of which agent ran, on what branch and how it ended
        // is the whole point — and without the attempt row, `gc` can no longer
        // attribute the checkout it left behind.
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.conn
            .execute(
                "UPDATE attempts SET worktree = '/wt/lin-142-1' WHERE id = ?1",
                params![a],
            )
            .unwrap();
        db.close_attempt(a, Outcome::Done).unwrap();

        assert_eq!(
            db.reap_task("linear:LIN-142").unwrap(),
            Reaped::Kept { attempts: 1 }
        );
        let t = db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(t.upstream, UpstreamState::Gone);
        assert_eq!(t.state, BoardState::Done, "stored state is set, not stale");
        assert_eq!(t.attempts.len(), 1);
        assert_eq!(t.attempts[0].worktree.as_deref(), Some("/wt/lin-142-1"));
        assert_eq!(t.attempts[0].branch.as_deref(), Some("board/lin-142"));
    }

    #[test]
    fn reaping_drops_queued_writebacks_but_keeps_delivered_ones() {
        // Nothing upstream to comment on any more, so a queued comment would
        // fail and back off forever. The delivered rows are the idempotency
        // ledger and cost nothing to keep.
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.close_attempt(a, Outcome::Done).unwrap();
        for key in ["delivered", "queued"] {
            db.enqueue_writeback(&NewWriteback {
                task_id: "linear:LIN-142".into(),
                kind: "comment".into(),
                payload: "{}".into(),
                idem_key: key.into(),
            })
            .unwrap();
        }
        let delivered = db.pending_writebacks(10).unwrap()[0].id;
        db.mark_writeback_done(delivered).unwrap();

        db.reap_task("linear:LIN-142").unwrap();
        assert_eq!(db.pending_writeback_count().unwrap(), 0);
        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM writeback_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1, "the delivered row survives");
    }

    #[test]
    fn an_already_reaped_task_is_not_reaped_again() {
        // Otherwise every sweep re-reaps it and says so in the log, forever.
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.close_attempt(a, Outcome::Done).unwrap();
        db.reap_task("linear:LIN-142").unwrap();
        assert!(db.reapable_task_ids(Source::Linear).unwrap().is_empty());
    }

    #[test]
    fn task_ids_are_listed_per_source() {
        let db = db();
        seed(&db, "linear:LIN-142");
        assert_eq!(
            db.reapable_task_ids(Source::Linear).unwrap(),
            vec!["linear:LIN-142"]
        );
        assert!(db.reapable_task_ids(Source::Github).unwrap().is_empty());
    }

    #[test]
    fn meta_round_trips() {
        let db = db();
        let identity = db.board_id().unwrap();
        assert!(identity.starts_with("board-"));
        assert_eq!(db.board_id().unwrap(), identity, "the store id is stable");
        assert_eq!(db.meta_get("cursor").unwrap(), None);
        db.meta_set("cursor", "2026-07-25T00:00:00Z").unwrap();
        db.meta_set("cursor", "2026-07-26T00:00:00Z").unwrap();
        assert_eq!(
            db.meta_get("cursor").unwrap().as_deref(),
            Some("2026-07-26T00:00:00Z")
        );
    }

    #[test]
    fn attempts_load_in_order_with_their_task() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        db.set_attempt_pane(a, "w1:p4").unwrap();
        db.close_attempt(a, Outcome::Cancelled).unwrap();
        db.insert_attempt(&attempt("linear:LIN-142")).unwrap();
        let t = db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(t.attempts.len(), 2);
        assert_eq!(t.attempts[0].outcome, Some(Outcome::Cancelled));
        assert_eq!(t.attempts[0].pane_id.as_deref(), Some("w1:p4"));
        assert!(t.live_attempt().is_some());
    }

    // ---- automation provenance and the automation log (gh#490) ----------

    /// Provenance rides the attempt row from the insert, and the two meters
    /// the rule's limits read — live count and dispatches-since — count it.
    #[test]
    fn automation_provenance_rides_the_attempt() {
        let db = db();
        seed(&db, "linear:LIN-142");
        let a = db
            .insert_attempt(&NewAttempt {
                automation: Some("approved".into()),
                automation_owner: Some("Brede".into()),
                ..attempt("linear:LIN-142")
            })
            .unwrap();
        let t = db.get_task("linear:LIN-142").unwrap().unwrap();
        assert_eq!(t.attempts[0].automation.as_deref(), Some("approved"));
        assert_eq!(t.attempts[0].automation_owner.as_deref(), Some("Brede"));
        assert_eq!(db.live_count_for_automation("approved").unwrap(), 1);
        assert_eq!(db.live_count_for_automation("other").unwrap(), 0);
        assert_eq!(
            db.automation_dispatches_since("approved", "2000-01-01T00:00:00Z")
                .unwrap(),
            1
        );
        db.close_attempt(a, Outcome::Failed).unwrap();
        assert_eq!(db.live_count_for_automation("approved").unwrap(), 0);
        assert!(
            db.automation_last_failure("approved", "linear:LIN-142")
                .unwrap()
                .is_some()
        );
    }

    /// The log collapses an unchanged answer into one streak and appends when
    /// the answer changes — thirty seconds of "at capacity" is one line, and
    /// the moment it becomes something else is a new one.
    #[test]
    fn the_automation_log_collapses_streaks_and_appends_changes() {
        let db = db();
        let decision = |reason: &str, decision: &str| AutomationDecision {
            rule: "approved".into(),
            task_id: Some("gh:o/r#1".into()),
            identifier: Some("gh#1".into()),
            decision: decision.into(),
            reason: reason.into(),
            attempt_id: None,
        };
        db.record_automation_decision(&decision("at capacity", "deferred"))
            .unwrap();
        db.record_automation_decision(&decision("at capacity", "deferred"))
            .unwrap();
        db.record_automation_decision(&decision("at capacity", "deferred"))
            .unwrap();
        let log = db.automation_log_recent(Some("approved"), 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].count, 3);

        db.record_automation_decision(&decision("cooldown active", "deferred"))
            .unwrap();
        let log = db.automation_log_recent(Some("approved"), 10).unwrap();
        assert_eq!(log.len(), 2, "a changed reason appends");

        // Streaks are per subject: a different task's identical answer is its
        // own row, and a rule-level note (no task) is its own subject too.
        db.record_automation_decision(&AutomationDecision {
            task_id: Some("gh:o/r#2".into()),
            ..decision("at capacity", "deferred")
        })
        .unwrap();
        db.record_automation_decision(&AutomationDecision {
            task_id: None,
            identifier: None,
            ..decision("evaluated", "skipped")
        })
        .unwrap();
        assert_eq!(db.automation_log_recent(Some("approved"), 10).unwrap().len(), 4);

        // Refusals are what the cooldown reads.
        db.record_automation_decision(&decision("billing guard refused", "refused"))
            .unwrap();
        assert!(
            db.automation_last_refusal("approved", "gh:o/r#1")
                .unwrap()
                .is_some()
        );
        assert!(
            db.automation_last_refusal("approved", "gh:o/r#2")
                .unwrap()
                .is_none()
        );

        // Retention: everything older than the cutoff goes; the attempts a
        // rule released are the durable record, not this.
        db.prune_automation_log("2999-01-01T00:00:00Z").unwrap();
        assert!(db.automation_log_recent(None, 10).unwrap().is_empty());
    }
}
