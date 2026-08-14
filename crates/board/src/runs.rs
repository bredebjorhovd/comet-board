//! What a run that is no longer there means (gh#390) — and what a box whose
//! runs keep dying looks like from the outside.
//!
//! Two pure decisions, kept together because they are the two halves of one
//! failure. The first says what the board should do about *this* attempt whose
//! session row has gone; the second says whether the box has stopped being able
//! to keep a run alive at all.
//!
//! ## Why the first one exists
//!
//! Reconciliation used to read an absent session row, on an attempt that had
//! been seen working, as "the chat is gone" and close the attempt `orphaned`.
//! That reading rested on a premise that is only true *inside one engine
//! process*: a session row persists as `Idle` after its run ends, so absence
//! after activity means the chat itself was archived or deleted.
//!
//! Across a restart the premise is false. The engine's session mirror is an
//! in-memory map rebuilt from live runs, so a box that restarts with six
//! attempts running comes back with six chats that are perfectly intact and six
//! session rows that do not exist. Two ticks later the board declared all six
//! chats gone, burned an attempt each, and told the dispatcher six times that
//! its work had vanished — while every one of those chats still sat on the
//! shelf holding the whole task.
//!
//! So the absence is split in two, and [`Runtime::chat_alive`] is what splits
//! it: a chat that is gone is an orphan exactly as before, and a chat that is
//! *there* with no run is an interrupted run, which is a thing to resume rather
//! than a thing to bury. Resuming costs no attempt, deletes no chat, and reuses
//! the branch and the worktree the run already had.
//!
//! [`Runtime::chat_alive`]: crate::runtime::Runtime::chat_alive
//!
//! ## Why it is bounded
//!
//! A box where every fresh run dies within a minute of starting would otherwise
//! be resumed forever. [`MAX_RESUMES`] is the ceiling: past it the attempt
//! closes `failed` — not `orphaned`, because nothing vanished, and the row
//! should stay red until somebody looks at the box — and [`health`] is what the
//! looking finds.

/// Is this attempt's chat still there?
///
/// Three answers and not two, for the reason every other absence on the board
/// gets three: "we asked and it is gone" and "we could not ask" are different
/// facts, and collapsing them is how a board ends an attempt because a runtime
/// call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// The chat is on its shelf, unarchived. Whatever died, it was the run.
    Alive,
    /// The chat has been archived or deleted out from under the attempt.
    Gone,
    /// Nobody could be asked — no runtime on this cycle, or the call failed.
    Unknown,
}

/// What to do about a live attempt whose session row is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Not yet: too few ticks to be sure, or nothing that could be asked.
    Wait,
    /// The chat is gone with the run. Close the attempt `orphaned`.
    Orphan,
    /// The chat is intact and its run is not. Prompt it to carry on — same
    /// attempt, same chat, same branch.
    Resume,
    /// Resumed as often as it is going to be. This box cannot keep a run
    /// alive, and saying so once is worth more than a fourth prompt.
    GiveUp,
}

/// How many consecutive ticks of absence before anything is decided.
///
/// Unchanged from what orphaning always used, and for its reason: a snapshot
/// taken while the engine is mid-handoff shows one absent row, and one absent
/// row has never proved anything.
pub const MISSING_TICKS: i64 = 2;

/// How many times the board will restart one attempt's run in place.
///
/// Three, because the failure this is bounded against is the box rather than
/// the run: an engine restart takes one resume, a second engine restart in the
/// same attempt is ordinary on an evening somebody is updating, and a third
/// failure inside one attempt is no longer bad luck. Past it the attempt is
/// closed and `doctor` has something to report.
pub const MAX_RESUMES: i64 = 3;

/// The verdict, given what the chat says and what this attempt has already had.
pub fn decide(chat: Liveness, missing_ticks: i64, resumes: i64) -> Verdict {
    if missing_ticks < MISSING_TICKS {
        return Verdict::Wait;
    }
    match chat {
        // Absence of evidence, again: a runtime that could not answer is a
        // reason to look next tick, never a reason to end an attempt.
        Liveness::Unknown => Verdict::Wait,
        Liveness::Gone => Verdict::Orphan,
        Liveness::Alive if resumes < MAX_RESUMES => Verdict::Resume,
        Liveness::Alive => Verdict::GiveUp,
    }
}

/// The prompt that restarts an interrupted run in its own chat.
///
/// Addressed to the agent that was working, so it says the one thing the agent
/// cannot see for itself — that its turn was killed from outside rather than
/// finished — and then gets out of the way. It deliberately does not re-state
/// the task: the chat holds the brief, the transcript and whatever the run had
/// already done, which is the whole reason resuming beats re-dispatching.
pub fn resume_prompt(resume: i64) -> String {
    format!(
        "comet-board: this run stopped without finishing — the engine was restarted under it, \
         or its harness died. Nothing else has changed: this chat, its branch and its checkout \
         are the ones you were working in, and no attempt has been spent. Pick up where you \
         left off — check `git status` and `git log` in your checkout before you redo anything \
         — and finish the task. (Restart {resume} of {MAX_RESUMES}; after that the board closes \
         the attempt and a human looks at the box.)"
    )
}

/// What an attempt closed for having its run die too often says upstream.
pub fn gave_up_note(resumes: i64) -> String {
    format!(
        "its run died {resumes} times and was restarted every time without finishing — the box \
         is not keeping runs alive"
    )
}

// ---- is this box keeping runs alive at all? (the `doctor` half) ------------

/// One closed attempt, reduced to the three facts the verdict needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// How long it ran, in seconds.
    pub lived_secs: i64,
    /// Did it produce something the board counts as work finishing?
    pub finished: bool,
}

/// A run that ended this fast, having produced nothing, did not fail at the
/// task — it failed at starting.
///
/// Five minutes is well under the shortest real dispatch on the box (a one-file
/// change with a test is minutes of tool calls before the first commit) and
/// well over the seconds a run takes to die when the harness cannot spawn.
pub const YOUNG_SECS: i64 = 5 * 60;

/// How far back the question is asked. A day, so a box broken overnight is
/// still broken at breakfast, and a box that was broken last week is not.
pub const WINDOW_SECS: i64 = 24 * 60 * 60;

/// How many young deaths in a row it takes before the box is the suspect
/// rather than the work.
///
/// Three, and no successes among them: two is a bad afternoon on one repo, and
/// a single success in the window means runs demonstrably still start here.
pub const DYING_RUNS: usize = 3;

/// What the recent attempts say about the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Nothing ran in the window. Says nothing about the box either way.
    Quiet,
    /// Runs are ending like runs: some finished, or the ones that did not at
    /// least lived long enough to have been trying.
    Healthy { ran: usize, young: usize },
    /// Every recent run died within minutes of starting and none finished.
    /// This is the box, not the work.
    Dying { ran: usize, young: usize },
}

/// Weigh the window (§gh#390's third bug: nothing on the board said the box had
/// stopped being able to run anything).
///
/// The rule is deliberately blunt, because the state it names is blunt: a run
/// that dies in under [`YOUNG_SECS`] having finished nothing produced no
/// evidence at all, and enough of those with nothing finishing between them is
/// a box that cannot start work. One finished attempt in the window clears it —
/// whatever is wrong then, it is not that runs cannot start.
pub fn health(samples: &[Sample]) -> Health {
    if samples.is_empty() {
        return Health::Quiet;
    }
    let ran = samples.len();
    let young = samples
        .iter()
        .filter(|s| !s.finished && s.lived_secs < YOUNG_SECS)
        .count();
    let finished = samples.iter().any(|s| s.finished);
    if !finished && young >= DYING_RUNS && young == ran {
        return Health::Dying { ran, young };
    }
    Health::Healthy { ran, young }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_absent_tick_decides_nothing_whatever_the_chat_says() {
        for chat in [Liveness::Alive, Liveness::Gone, Liveness::Unknown] {
            assert_eq!(decide(chat, 1, 0), Verdict::Wait);
        }
    }

    /// The bug: six chats that were all still there, all closed as gone.
    #[test]
    fn a_chat_that_is_still_there_is_a_dead_run_not_an_orphan() {
        assert_eq!(decide(Liveness::Alive, 2, 0), Verdict::Resume);
    }

    #[test]
    fn a_chat_that_is_really_gone_still_orphans() {
        assert_eq!(decide(Liveness::Gone, 2, 0), Verdict::Orphan);
    }

    /// A runtime that could not answer must not end an attempt — the same rule
    /// that already governed a chat which had never worked.
    #[test]
    fn an_unanswerable_chat_is_never_orphaned() {
        assert_eq!(decide(Liveness::Unknown, 9, 0), Verdict::Wait);
    }

    #[test]
    fn resuming_is_bounded_and_then_the_attempt_closes() {
        assert_eq!(decide(Liveness::Alive, 2, MAX_RESUMES - 1), Verdict::Resume);
        assert_eq!(decide(Liveness::Alive, 2, MAX_RESUMES), Verdict::GiveUp);
    }

    fn died(secs: i64) -> Sample {
        Sample {
            lived_secs: secs,
            finished: false,
        }
    }

    fn finished(secs: i64) -> Sample {
        Sample {
            lived_secs: secs,
            finished: true,
        }
    }

    #[test]
    fn a_quiet_box_is_not_a_broken_one() {
        assert_eq!(health(&[]), Health::Quiet);
    }

    #[test]
    fn every_run_dying_in_minutes_is_the_box() {
        let box_ = health(&[died(40), died(65), died(12), died(90)]);
        assert_eq!(box_, Health::Dying { ran: 4, young: 4 });
    }

    #[test]
    fn one_run_that_finished_clears_the_window() {
        assert!(matches!(
            health(&[died(40), died(65), died(12), finished(30)]),
            Health::Healthy { .. }
        ));
    }

    /// A long run that failed is a run that was trying — it says nothing about
    /// whether runs can start.
    #[test]
    fn a_long_failure_is_not_a_young_death() {
        assert_eq!(
            health(&[died(40), died(65), died(4 * 3600)]),
            Health::Healthy { ran: 3, young: 2 }
        );
    }

    #[test]
    fn two_young_deaths_are_a_bad_afternoon() {
        assert!(matches!(
            health(&[died(40), died(65)]),
            Health::Healthy { ran: 2, young: 2 }
        ));
    }
}
