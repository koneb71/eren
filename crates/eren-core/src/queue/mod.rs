use chrono::{DateTime, Duration, Utc};
use rand::Rng;

/// Backoff for rate-limited runs: 5m → 15m → 45m (capped), with jitter so a
/// burst of queued runs doesn't stampede the moment the window resets.
pub fn rate_limit_backoff(attempt: u32, reset_at: Option<DateTime<Utc>>) -> DateTime<Utc> {
    let jitter = Duration::seconds(rand::rng().random_range(5..90));
    // A reset time already behind us says nothing about when the limit
    // lifts — the engine's clock, a stale header, a window that turned while
    // the stream was still ending. Honoured, it re-dispatched the run within
    // seconds into the same limit, and every hold counted towards a ladder
    // that was never climbed.
    match reset_at.filter(|t| *t > Utc::now()) {
        Some(t) => t + jitter,
        None => {
            let minutes = match attempt {
                0 => 5,
                1 => 15,
                _ => 45,
            };
            Utc::now() + Duration::minutes(minutes) + jitter
        }
    }
}

/// Which rung of the ladder a run that has now been held `holds` times sits on.
///
/// The first hold is attempt 0, and `hold_rate_limited` increments the counter
/// before it asks — so this is the off-by-one that kept the only production
/// call site written as `rate_limit_backoff(0, ..)` and left 15m and 45m
/// reachable from nothing but this file's own tests.
pub fn attempt_index(holds: i32) -> u32 {
    holds.saturating_sub(1).max(0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_escalates_and_caps() {
        let now = Utc::now();
        let b0 = rate_limit_backoff(0, None) - now;
        let b1 = rate_limit_backoff(1, None) - now;
        let b9 = rate_limit_backoff(9, None) - now;
        assert!(b0 >= Duration::minutes(5) && b0 < Duration::minutes(7));
        assert!(b1 >= Duration::minutes(15) && b1 < Duration::minutes(17));
        assert!(b9 >= Duration::minutes(45) && b9 < Duration::minutes(47));
    }

    #[test]
    fn backoff_honors_reset_time() {
        let reset = Utc::now() + Duration::hours(2);
        let t = rate_limit_backoff(0, Some(reset));
        assert!(t > reset && t < reset + Duration::minutes(2));
    }

    #[test]
    fn a_reset_time_in_the_past_falls_back_to_the_ladder() {
        let now = Utc::now();
        let stale = now - Duration::minutes(10);
        let first = rate_limit_backoff(0, Some(stale)) - now;
        assert!(first >= Duration::minutes(5) && first < Duration::minutes(7));
        let third = rate_limit_backoff(2, Some(stale)) - now;
        assert!(third >= Duration::minutes(45) && third < Duration::minutes(47));
    }

    #[test]
    fn the_ladder_actually_escalates_across_three_holds() {
        // The assertion that would have failed for the life of the feature:
        // production always asked for attempt 0, so a limit with no reset time
        // — every OpenCode run — retried every five minutes forever.
        let now = Utc::now();
        let mins =
            |holds: i32| (rate_limit_backoff(attempt_index(holds), None) - now).num_minutes();
        assert!(
            (5..=7).contains(&mins(1)),
            "first hold ~5m, got {}",
            mins(1)
        );
        assert!(
            (15..=17).contains(&mins(2)),
            "second hold ~15m, got {}",
            mins(2)
        );
        assert!(
            (45..=47).contains(&mins(3)),
            "third hold ~45m, got {}",
            mins(3)
        );
        assert!((45..=47).contains(&mins(9)), "and it caps");
    }

    #[test]
    fn a_counter_that_somehow_went_backwards_still_asks_for_the_first_rung() {
        assert_eq!(attempt_index(0), 0);
        assert_eq!(attempt_index(-3), 0);
    }
}
