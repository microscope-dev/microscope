use std::time::Duration;

use super::machine::Instant;

pub const FIRST_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
const MAX_RETRY_AFTER: Duration = Duration::from_mins(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosterAction {
    Post,
    Wait { until: Instant },
    NeedToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosterOutcome {
    Accepted { last_chunk: u64 },
    Overlap { last_chunk: u64 },
    Unauthorized,
    Retry { retry_after: Option<Duration> },
}

#[derive(Clone, Debug)]
pub struct PosterMachine {
    post_interval: Duration,
    next_post: Instant,
    failures: u32,
    need_token: bool,
}

impl PosterMachine {
    #[must_use]
    pub const fn new(now: Instant, post_interval: Duration) -> Self {
        Self {
            post_interval,
            next_post: now,
            failures: 0,
            need_token: false,
        }
    }

    #[must_use]
    pub const fn failures(&self) -> u32 {
        self.failures
    }

    #[must_use]
    pub const fn needs_token(&self) -> bool {
        self.need_token
    }

    pub const fn token_refreshed(&mut self, now: Instant) {
        if self.need_token {
            self.need_token = false;
            self.failures = 0;
            self.next_post = now;
        }
    }

    pub const fn chunk_queued(&mut self, now: Instant, was_empty: bool) {
        if was_empty && self.failures == 0 && !self.need_token {
            self.next_post = now.after(self.post_interval);
        }
    }

    #[must_use]
    pub const fn next(&self, now: Instant, has_chunks: bool, has_token: bool) -> PosterAction {
        if self.need_token {
            return PosterAction::NeedToken;
        }
        if !has_chunks {
            return PosterAction::Wait {
                until: now.after(self.post_interval),
            };
        }
        if !has_token {
            return PosterAction::NeedToken;
        }
        if now.0.as_nanos() >= self.next_post.0.as_nanos() {
            PosterAction::Post
        } else {
            PosterAction::Wait {
                until: self.next_post,
            }
        }
    }

    pub fn on(
        &mut self,
        outcome: PosterOutcome,
        now: Instant,
        still_has_chunks: bool,
        jitter: f64,
    ) {
        assert!((0.0..=1.0).contains(&jitter));
        match outcome {
            PosterOutcome::Accepted { .. } => {
                self.failures = 0;
                self.need_token = false;
                self.next_post = if still_has_chunks {
                    now
                } else {
                    now.after(self.post_interval)
                };
            }
            PosterOutcome::Overlap { .. } => {
                self.failures = 0;
                self.need_token = false;
                self.next_post = now;
            }
            PosterOutcome::Unauthorized => {
                self.need_token = true;
                self.next_post = now;
            }
            PosterOutcome::Retry { retry_after } => {
                self.failures = self.failures.saturating_add(1);
                let shift = self.failures.saturating_sub(1).min(10);
                let ceiling = FIRST_BACKOFF.saturating_mul(1 << shift).min(MAX_BACKOFF);
                let wait = (ceiling / 2).saturating_add((ceiling / 2).mul_f64(jitter));
                let wait = wait.max(retry_after.unwrap_or_default().min(MAX_RETRY_AFTER));
                self.next_post = now.after(wait);
            }
        }
    }
}

#[cfg(test)]
mod sim;
