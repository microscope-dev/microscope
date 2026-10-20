//! How long the agent waits between attempts to connect: exponential, from
//! one second to a minute, with full jitter, so agents that lost the same
//! service do not all return at once.

use std::time::Duration;

const FIRST: Duration = Duration::from_secs(1);
const CAP: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub struct Backoff {
    attempts: u32,
}

impl Backoff {
    /// The wait before the next attempt: random, up to a ceiling that
    /// doubles with each attempt.
    pub fn next(&mut self) -> Duration {
        let ceiling = self.ceiling();
        self.attempts = self.attempts.saturating_add(1);
        let mut random = [0; 8];
        // Without randomness, waiting the whole ceiling is still safe.
        if getrandom::fill(&mut random).is_err() {
            return ceiling;
        }
        let millis = u64::try_from(ceiling.as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(u64::from_le_bytes(random) % (millis + 1))
    }

    /// The longest the next wait may be.
    pub fn ceiling(&self) -> Duration {
        FIRST
            .checked_mul(1 << self.attempts.min(16))
            .map_or(CAP, |ceiling| ceiling.min(CAP))
    }

    /// Starts over, as after a connection that lasted.
    pub const fn reset(&mut self) {
        self.attempts = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_grow_to_a_minute() {
        let mut backoff = Backoff::default();
        let mut ceilings = Vec::new();
        for _ in 0..10 {
            ceilings.push(backoff.ceiling().as_secs());
            let wait = backoff.next();
            assert!(wait <= Duration::from_secs(*ceilings.last().unwrap()));
        }
        assert_eq!(ceilings, [1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
    }

    #[test]
    fn waits_vary() {
        let mut backoff = Backoff::default();
        for _ in 0..6 {
            backoff.next();
        }
        let waits: std::collections::BTreeSet<_> = (0..8)
            .map(|_| {
                let wait = backoff.next();
                backoff.attempts = 6;
                wait
            })
            .collect();
        assert!(waits.len() > 1, "{waits:?}");
    }

    #[test]
    fn reset_starts_over() {
        let mut backoff = Backoff::default();
        for _ in 0..5 {
            backoff.next();
        }
        backoff.reset();
        assert_eq!(backoff.ceiling(), FIRST);
    }
}
