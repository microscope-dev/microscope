use std::time::Duration;

use super::{machine::Instant, wire::WatchDirective};

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
    watch_interval: Option<Duration>,
    watch_until: Option<Instant>,
    watch_generation: u64,
    pending_flush: Option<u64>,
    next_post: Instant,
    failures: u32,
    need_token: bool,
}

impl PosterMachine {
    #[must_use]
    pub const fn new(now: Instant, post_interval: Duration) -> Self {
        Self {
            post_interval,
            watch_interval: None,
            watch_until: None,
            watch_generation: 0,
            pending_flush: None,
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

    #[must_use]
    pub const fn pending_flush(&self) -> Option<u64> {
        self.pending_flush
    }

    #[must_use]
    pub const fn effective_interval(&self, now: Instant) -> Duration {
        if let (Some(interval), Some(until)) = (self.watch_interval, self.watch_until)
            && now.0.as_nanos() < until.0.as_nanos()
            && interval.as_nanos() < self.post_interval.as_nanos()
        {
            return interval;
        }
        self.post_interval
    }

    pub fn apply_watch(&mut self, directive: Option<&WatchDirective>, now: Instant) -> bool {
        if let Some(watch) = directive {
            let interval = Duration::from_millis(watch.interval_ms.clamp(1_000, 60_000));
            let lease = Duration::from_millis(watch.lease_ms.clamp(5_000, 300_000));
            let was_active = self
                .watch_until
                .is_some_and(|until| now.0.as_nanos() < until.0.as_nanos());
            let tightened = !was_active
                || watch.generation != self.watch_generation
                || self.watch_interval.is_none_or(|cur| interval < cur);
            self.watch_interval = Some(interval);
            self.watch_until = Some(now.after(lease));
            self.watch_generation = watch.generation;
            if tightened && self.failures == 0 && !self.need_token {
                self.next_post = now;
            }
            tightened
        } else {
            self.watch_interval = None;
            self.watch_until = None;
            false
        }
    }

    pub fn request_flush(&mut self, flush_id: u64, now: Instant) {
        if self.pending_flush.is_none_or(|cur| flush_id > cur) {
            self.pending_flush = Some(flush_id);
            if self.failures == 0 && !self.need_token {
                self.next_post = now;
            }
        }
    }

    pub fn clear_flush(&mut self, flush_id: u64) {
        if self.pending_flush == Some(flush_id) {
            self.pending_flush = None;
        }
    }

    pub const fn token_refreshed(&mut self, now: Instant) {
        if self.need_token {
            self.need_token = false;
            self.failures = 0;
            self.next_post = now;
        }
    }

    pub const fn chunk_queued(&mut self, now: Instant, was_empty: bool) {
        if self.failures == 0 && !self.need_token {
            if self.pending_flush.is_some() {
                self.next_post = now;
            } else if was_empty {
                let interval = self.effective_interval(now);
                let deadline = now.after(interval);
                if self.next_post.0.as_nanos() > deadline.0.as_nanos()
                    || self.next_post.0.as_nanos() <= now.0.as_nanos()
                {
                    self.next_post = deadline;
                }
            }
        }
    }

    #[must_use]
    pub const fn next(&self, now: Instant, has_chunks: bool, has_token: bool) -> PosterAction {
        if self.need_token {
            return PosterAction::NeedToken;
        }
        let interval = self.effective_interval(now);
        if !has_chunks {
            return PosterAction::Wait {
                until: now.after(interval),
            };
        }
        if !has_token {
            return PosterAction::NeedToken;
        }
        if self.pending_flush.is_some() || now.0.as_nanos() >= self.next_post.0.as_nanos() {
            PosterAction::Post
        } else {
            let capped = now.after(interval);
            let until = if capped.0.as_nanos() < self.next_post.0.as_nanos() {
                capped
            } else {
                self.next_post
            };
            PosterAction::Wait { until }
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
        let interval = self.effective_interval(now);
        match outcome {
            PosterOutcome::Accepted { .. } => {
                self.failures = 0;
                self.need_token = false;
                if !still_has_chunks {
                    self.pending_flush = None;
                }
                self.next_post = if still_has_chunks {
                    now
                } else {
                    now.after(interval)
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
