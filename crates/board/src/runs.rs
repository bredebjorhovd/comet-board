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
//!
//! ## Why the ceiling counts something it did not start (gh#392)
//!
//! The board is not the only thing on the box that restarts a dead run. The
//! engine's own boot recovery (`Sessions::recover_stale`) walks every journal a
//! crash left open, closes it with a synthetic interrupted `Done` — and then
//! picks the run back up against the remembered harness session, on its own
//! budget of three, counted in a file beside the journal.
//!
//! On the ordinary path the two never meet: the engine acts on a journal that
//! is *stale*, the board on a chat whose journal is *closed* and whose run is
//! gone. They meet at exhaustion. An engine out of revivals still closes the
//! journal and simply stops re-dispatching, which leaves a chat that is alive,
//! a journal that is closed and no run — [`Liveness::Alive`] with nothing
//! running, exactly the state this module decides about. The board would then
//! begin resuming from zero, and one attempt's run would be started six times
//! and reported as three.
//!
//! Three is the number [`gave_up_note`] hands a person, and the sentence it is
//! in — *this box is not keeping runs alive* — is an argument from how many
//! times a thing has actually failed. Three prior failures nobody counted make
//! it false. So [`Restarts`] carries both counts, one budget is spent against
//! their sum, and when the engine's ledger cannot be read the note claims no
//! number at all rather than a number it cannot support.
//!
//! What this does **not** do is stop the engine: boot recovery answers to
//! nobody here, and a revival it performs while the board is between ticks
//! happens whatever the board has spent. The board counts it, decides with it,
//! and says it — which is the whole of what the board can honestly do about a
//! restart it does not perform.

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

/// How often this attempt's run has already been started again — and by which
/// of the two things on the box that can do it (gh#392).
///
/// One budget, two ledgers, because the two restarts are performed by different
/// processes and neither can write the other's record: the board's count is a
/// column on the attempt row, the engine's is a file beside the run journal.
/// The board is the one that has to add them up, because it is the one that
/// decides and the one that reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restarts {
    /// What the board has spent on this attempt — `attempts.resumes`.
    pub board: i64,
    /// What the engine's boot recovery revived this chat, off its own ledger.
    ///
    /// `None` is "nobody could be asked" — no runtime this cycle, a runtime
    /// that could not read the ledger, or a chat this device has never run.
    /// It is not zero: zero is a ledger that says the engine did nothing, and
    /// the difference is exactly what [`gave_up_note`] is allowed to claim.
    pub engine: Option<i64>,
}

impl Restarts {
    /// What the board knows on its own — the answer when the engine's ledger
    /// is out of reach, and the behaviour the board had before gh#392.
    pub fn board_only(board: i64) -> Self {
        Self {
            board,
            engine: None,
        }
    }

    /// What the budget is spent against.
    ///
    /// An unreadable engine ledger counts as nothing, deliberately: the board
    /// must not refuse to restart a run because it could not ask a question,
    /// which is [`Liveness::Unknown`]'s rule applied to the other half of the
    /// same state. It errs towards restarting, and towards saying less.
    pub fn spent(&self) -> i64 {
        self.board + self.engine.unwrap_or(0)
    }

    /// How many times this run has actually been started again, when that is
    /// knowable at all — the number a note may put in front of a person.
    pub fn counted(&self) -> Option<i64> {
        self.engine.map(|engine| self.board + engine)
    }
}

/// The verdict, given what the chat says and what this attempt has already had.
pub fn decide(chat: Liveness, missing_ticks: i64, restarts: Restarts) -> Verdict {
    if missing_ticks < MISSING_TICKS {
        return Verdict::Wait;
    }
    match chat {
        // Absence of evidence, again: a runtime that could not answer is a
        // reason to look next tick, never a reason to end an attempt.
        Liveness::Unknown => Verdict::Wait,
        Liveness::Gone => Verdict::Orphan,
        Liveness::Alive if restarts.spent() < MAX_RESUMES => Verdict::Resume,
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
///
/// `resume` is [`Restarts::spent`] and not the board's own column: what the
/// sentence is about is the budget and what happens at the end of it, and the
/// budget is the joined one (gh#392).
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
///
/// The one sentence a person reads when the board finally stops, so the number
/// in it is evidence and has to be defensible (gh#392). It is the joined count
/// or it is nothing: an engine revival the board never saw is still a time this
/// run was started and died, and a count that omits three of them is an
/// argument from a premise that is false. When the engine's ledger could not be
/// read the sentence says what happened without saying how often — the reader
/// loses a number the board never had, rather than being handed a wrong one.
pub fn gave_up_note(restarts: Restarts) -> String {
    let Some(total) = restarts.counted() else {
        return "its run kept dying and was restarted until the board would restart it no more, \
                never finishing — the box is not keeping runs alive"
            .to_string();
    };
    let mut note = format!(
        "its run died {total} times and was restarted every time without finishing — the box \
         is not keeping runs alive"
    );
    // Named rather than folded in, because it is the part that is otherwise
    // unfindable: the engine's revivals leave nothing on the board at all, so a
    // reader comparing this number against `attempts.resumes` would find them
    // missing and trust the smaller one.
    if let Some(engine) = restarts.engine.filter(|e| *e > 0) {
        note.push_str(&format!(
            " ({engine} of those restarts were the engine's own boot recovery, before the board \
             saw the run was gone)"
        ));
    }
    note
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

    /// The board's own count, with an engine that says it did nothing.
    fn board(n: i64) -> Restarts {
        Restarts {
            board: n,
            engine: Some(0),
        }
    }

    #[test]
    fn one_absent_tick_decides_nothing_whatever_the_chat_says() {
        for chat in [Liveness::Alive, Liveness::Gone, Liveness::Unknown] {
            assert_eq!(decide(chat, 1, board(0)), Verdict::Wait);
        }
    }

    /// The bug: six chats that were all still there, all closed as gone.
    #[test]
    fn a_chat_that_is_still_there_is_a_dead_run_not_an_orphan() {
        assert_eq!(decide(Liveness::Alive, 2, board(0)), Verdict::Resume);
    }

    #[test]
    fn a_chat_that_is_really_gone_still_orphans() {
        assert_eq!(decide(Liveness::Gone, 2, board(0)), Verdict::Orphan);
    }

    /// A runtime that could not answer must not end an attempt — the same rule
    /// that already governed a chat which had never worked.
    #[test]
    fn an_unanswerable_chat_is_never_orphaned() {
        assert_eq!(decide(Liveness::Unknown, 9, board(0)), Verdict::Wait);
    }

    #[test]
    fn resuming_is_bounded_and_then_the_attempt_closes() {
        assert_eq!(
            decide(Liveness::Alive, 2, board(MAX_RESUMES - 1)),
            Verdict::Resume
        );
        assert_eq!(
            decide(Liveness::Alive, 2, board(MAX_RESUMES)),
            Verdict::GiveUp
        );
    }

    // ---- the two budgets that could not see each other (§gh#392) ----------

    /// The bug: an engine that spent its whole revival budget hands the board a
    /// live chat with no run, which is the state the board resumes from zero.
    /// Six starts, and a note that would have said three.
    #[test]
    fn an_engine_that_spent_its_budget_leaves_the_board_none() {
        let restarts = Restarts {
            board: 0,
            engine: Some(MAX_RESUMES),
        };
        assert_eq!(decide(Liveness::Alive, 2, restarts), Verdict::GiveUp);
    }

    /// One budget, wherever the restart came from: one engine revival leaves
    /// the board two, not three.
    #[test]
    fn one_budget_is_shared_between_the_engine_and_the_board() {
        let after_engine = |board| Restarts {
            board,
            engine: Some(1),
        };
        assert_eq!(decide(Liveness::Alive, 2, after_engine(1)), Verdict::Resume);
        assert_eq!(decide(Liveness::Alive, 2, after_engine(2)), Verdict::GiveUp);
    }

    /// An engine ledger nobody could read must not cost the attempt a restart —
    /// the same rule as a chat nobody could ask about, and the same reason.
    #[test]
    fn an_unreadable_engine_ledger_spends_nothing() {
        assert_eq!(
            decide(Liveness::Alive, 2, Restarts::board_only(MAX_RESUMES - 1)),
            Verdict::Resume
        );
    }

    #[test]
    fn the_note_counts_the_restarts_the_board_did_not_perform() {
        let note = gave_up_note(Restarts {
            board: 2,
            engine: Some(3),
        });
        assert!(note.contains("died 5 times"), "{note}");
        assert!(
            note.contains("3 of those restarts were the engine's own"),
            "{note}"
        );
    }

    /// A count the board cannot see is a count it must not state.
    #[test]
    fn the_note_claims_no_number_it_cannot_support() {
        let note = gave_up_note(Restarts::board_only(3));
        assert!(!note.contains('3'), "{note}");
        assert!(
            note.contains("restarted until the board would restart it no more"),
            "{note}"
        );
    }

    /// Nothing extra is said about an engine that did nothing.
    #[test]
    fn the_note_is_unchanged_when_only_the_board_restarted_it() {
        let note = gave_up_note(board(3));
        assert_eq!(
            note,
            "its run died 3 times and was restarted every time without finishing — the box is \
             not keeping runs alive"
        );
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
