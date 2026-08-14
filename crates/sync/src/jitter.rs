//! Herd-breaking jitter for reconnect schedules — no `rand` dependency.
//!
//! Every retry loop in this workspace is one of N identical loops (one room
//! per chat, one link per peer) that share their failures: an edge deploy, a
//! DO that dies mid-session, a laptop resuming from sleep. Undelayed, those N
//! loops redial in the same millisecond forever, and the client is a
//! synchronised battering ram against the thing that just failed (gh#396).
//! Spreading each wait over a window is what turns N simultaneous dials into
//! N dials smeared across that window.
//!
//! The entropy is the nanosecond field of the wall clock, taken modulo the
//! span in milliseconds. Two callers a microsecond apart differ by ~1000 in
//! that field, and the modulo scatters that microsecond of separation across
//! the whole window — which is exactly the property a herd-breaker needs and
//! the only one it needs. This is NOT a random number generator: it is not
//! uniform to any standard worth quoting, it repeats on a 1s cycle, and
//! nothing that wants unpredictability (an id, a token, a nonce) may use it.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A pseudo-random duration in `[0, span)`.
///
/// Millisecond granularity: a span under 1ms has no room to spread and
/// returns zero, as does `Duration::ZERO`.
pub fn spread(span: Duration) -> Duration {
    let span_ms = u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
    if span_ms == 0 {
        return Duration::ZERO;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::from(since.subsec_nanos()));
    Duration::from_millis(nanos % span_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draw_stays_inside_its_span() {
        let span = Duration::from_millis(1500);
        for _ in 0..1000 {
            assert!(spread(span) < span, "jitter must stay under its span");
        }
    }

    #[test]
    fn spans_too_small_to_spread_are_zero() {
        assert_eq!(spread(Duration::ZERO), Duration::ZERO);
        assert_eq!(spread(Duration::from_micros(999)), Duration::ZERO);
    }

    /// The whole point: two loops drawing microseconds apart must not get the
    /// same delay. Sleeps between draws so the assertion holds on a coarse
    /// wall clock too.
    #[test]
    fn consecutive_draws_differ() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..20 {
            seen.insert(spread(Duration::from_millis(1000)));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            seen.len() >= 3,
            "jitter that repeats is not jitter; saw {} distinct delays",
            seen.len()
        );
    }
}
