//! The wall-clock cap on a live attempt (gh#70): how long an agent may run
//! before the board stops it.
//!
//! Nothing else bounds a running attempt. The engine's stall watchdog
//! (`crates/engine/src/sessions.rs`) hard-stops a run that emits *nothing*, and
//! its silence-after-output tier is advisory by design — so an agent that keeps
//! emitting keeps running, and `attempts.started_at` was a column nobody read.
//! The same gap strands the other end: an engine crash past its revival budget
//! settles the chat `Idle`, and with no commits [`crate::settled::decide`]
//! returns `StayLive(NoArtifacts)` — orphaning fires on a *missing* session row
//! and this one exists, so the row renders `working` forever. One clock closes
//! both.
//!
//! Two decisions, kept pure here so the arithmetic is testable without a chat,
//! a config or a database:
//!
//! - [`decide`] — within the cap, past it and unwarned, or warned and out of
//!   grace.
//! - [`grace_secs`] — how long the warning buys, scaled to the cap.
//!
//! The discipline is orphaning's (see [`crate::sync::SyncEngine::reconcile_sessions`]):
//! this rides the interval reconcile, so it ages an attempt in wall time and a
//! burst of watch events cannot run the clock faster than the clock.

/// The longest grace a warning ever buys. Ten minutes is enough for an agent
/// told to wrap up to commit what it has and open a pull request, and short
/// enough that a looping one is not left burning another hour.
pub const MAX_GRACE_SECS: u64 = 600;

/// The shortest cap the config will accept. A cap under a minute would fail
/// every dispatch before its first turn finished.
pub const MIN_CAP_SECS: u64 = 60;

/// What the wall clock says about one live attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Inside the cap. The overwhelming case, and the one that must cost
    /// nothing — a legit long run is untouched.
    Within,
    /// Past the cap and not warned yet. Say so, once, in the chat and the log.
    Warn,
    /// Warned, and the grace has run out. Close the attempt `failed`.
    Cancel,
}

/// The verdict for an attempt that has been live `age_secs`, under a `cap_secs`
/// cap, `warned_secs` after its warning went out (`None` = not warned yet).
///
/// The warning is not skippable, however far past the cap the attempt is: a
/// board that was down for five hours comes back to attempts hours over their
/// cap, and killing those without a word would be exactly the silent stop this
/// exists to avoid. They get their warning and their grace like everyone else.
pub fn decide(age_secs: i64, warned_secs: Option<i64>, cap_secs: u64, grace_secs: u64) -> Verdict {
    if age_secs < cap_secs as i64 {
        // Not over yet. A warning already sent belongs to a cap that has since
        // been raised, or to a clock that moved backwards; either way the
        // attempt is inside its budget now, so nothing is due.
        return Verdict::Within;
    }
    match warned_secs {
        None => Verdict::Warn,
        Some(since) if since >= grace_secs as i64 => Verdict::Cancel,
        Some(_) => Verdict::Within,
    }
}

/// How long the warning buys before the attempt is closed.
///
/// A sixth of the cap, so a short cap gets a proportionate wrap-up window
/// rather than one that dwarfs it, bounded both ways: never more than
/// [`MAX_GRACE_SECS`], and never less than two sync intervals — the check rides
/// the interval clock, so a grace shorter than that could expire before the
/// board looked again and turn the warning into a formality.
pub fn grace_secs(cap_secs: u64, interval_secs: u64) -> u64 {
    (cap_secs / 6)
        .min(MAX_GRACE_SECS)
        .max(interval_secs.saturating_mul(2))
}

/// A duration as an operator reads it: `2h 14m`, `45m`, `30s`. For the log
/// line, the chat warning and the upstream comment — all three name the same
/// numbers, so they have to spell them the same way.
pub fn human_secs(secs: i64) -> String {
    if secs < 0 {
        return "0s".into();
    }
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m"),
        (_, 0) => format!("{h}h"),
        _ => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u64 = 7200; // 2h
    const GRACE: u64 = 600; // 10m

    #[test]
    fn a_run_inside_its_cap_is_left_alone() {
        // The exit criterion's other half: a legit long run is untouched.
        assert_eq!(decide(0, None, CAP, GRACE), Verdict::Within);
        assert_eq!(decide(7199, None, CAP, GRACE), Verdict::Within);
    }

    #[test]
    fn the_cap_warns_before_it_cancels() {
        assert_eq!(decide(7200, None, CAP, GRACE), Verdict::Warn);
        // Warned, still inside the grace: the agent is wrapping up.
        assert_eq!(decide(7500, Some(300), CAP, GRACE), Verdict::Within);
        assert_eq!(decide(7800, Some(600), CAP, GRACE), Verdict::Cancel);
    }

    /// A board that was down for hours comes back to attempts far past their
    /// cap. They are still warned first — killing a run without a word is the
    /// thing this feature exists not to do.
    #[test]
    fn an_attempt_hours_over_is_still_warned_first() {
        assert_eq!(decide(50_000, None, CAP, GRACE), Verdict::Warn);
    }

    /// Raising the cap under a warned attempt un-warns it: it is inside its
    /// budget again, and cancelling on a stamp left by the old cap would be
    /// the board acting on config that no longer exists.
    #[test]
    fn a_raised_cap_calls_off_a_pending_cancel() {
        assert_eq!(decide(7800, Some(9999), 28_800, GRACE), Verdict::Within);
    }

    #[test]
    fn grace_scales_with_the_cap_and_is_bounded_both_ways() {
        // 2h → a sixth is 20m, clamped to the 10m ceiling.
        assert_eq!(grace_secs(7200, 30), 600);
        // 30m → 5m, taken as-is.
        assert_eq!(grace_secs(1800, 30), 300);
        // A tight cap on a slow interval still gets two ticks to be seen in.
        assert_eq!(grace_secs(300, 120), 240);
    }

    #[test]
    fn durations_read_the_way_an_operator_says_them() {
        assert_eq!(human_secs(45), "45s");
        assert_eq!(human_secs(600), "10m");
        assert_eq!(human_secs(3600), "1h");
        assert_eq!(human_secs(8040), "2h 14m");
        assert_eq!(human_secs(-1), "0s");
    }
}
