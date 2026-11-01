//! How long `zone join` waits between tries while it is not admitted yet. Core answers
//! `not-admitted` or 404 for as long as the owner (or the zone's admitter) takes, so those two
//! kinds back off exponentially with jitter up to five minutes; any other retryable failure
//! (network, 5xx, 429) keeps the short retry. The same schedule as the SDK's own agent, so a
//! waiting CLI loads Core no more than a waiting agent does.

use std::time::Duration;

/// First wait after a failure, and the fixed wait for failures that are not a "wait for me" answer.
pub const RETRY: Duration = Duration::from_secs(10);
/// Longest wait while not admitted.
pub const CAP: Duration = Duration::from_secs(300);

/// Consecutive failures of the waiting kind, and the wait they earn.
#[derive(Debug, Default, Clone, Copy)]
pub struct Backoff {
    waiting_failures: u32,
}

impl Backoff {
    /// Record a failure of `kind` ([`crate::join::kind`]'s output) and return the wait before
    /// the next try. `jitter` is any random u64; the wait lands in `[3/4 d, d]` where
    /// `d = min(10 s * 2^(n-1), 300 s)`, so clients that failed together drift apart and the cap
    /// still holds.
    pub fn failed(&mut self, kind: &str, jitter: u64) -> Duration {
        if !is_waiting(kind) {
            self.waiting_failures = 0;
            return RETRY;
        }
        self.waiting_failures = self.waiting_failures.saturating_add(1);
        let exp = self.waiting_failures.saturating_sub(1).min(16);
        let full = RETRY.saturating_mul(1u32 << exp).min(CAP);
        let full_ms = full.as_millis() as u64;
        let spread = full_ms / 4;
        Duration::from_millis(full_ms - spread + jitter % (spread + 1))
    }

    /// A try went through: the next failure starts from the short retry again.
    pub fn succeeded(&mut self) {
        self.waiting_failures = 0;
    }
}

/// Core's "not yet" answers: the device is not in the roster, or the zone is not visible to it.
pub fn is_waiting(kind: &str) -> bool {
    kind == "not-admitted" || kind == "core-404"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_doubles_from_ten_seconds_to_the_cap_within_a_quarter_of_jitter() {
        let mut b = Backoff::default();
        let mut want = 10_000u64;
        for _ in 0..8 {
            let mut copy = b;
            let low = copy.failed("core-404", 0).as_millis() as u64;
            let high = b.failed("core-404", u64::MAX).as_millis() as u64;
            assert!(
                low >= want * 3 / 4 && high <= want,
                "{low}..{high} vs {want}"
            );
            want = (want * 2).min(300_000);
        }
        // jitter == spread (a quarter of the cap) lands on the cap itself.
        assert_eq!(b.failed("not-admitted", 75_000), CAP);
    }

    #[test]
    fn other_failures_retry_short_and_reset_the_count() {
        let mut b = Backoff::default();
        b.failed("core-404", 0);
        b.failed("core-404", 0);
        assert_eq!(b.failed("core-0", 7), RETRY);
        assert!(b.failed("core-404", u64::MAX) <= RETRY);
        b.succeeded();
        assert!(b.failed("not-admitted", u64::MAX) <= RETRY);
    }
}
