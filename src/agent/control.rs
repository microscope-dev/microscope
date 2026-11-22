use std::time::Duration;

use super::{
    machine::Instant,
    wire::{
        BurstDirective, FlushDirective, PolicyDirective, PollRequest, PollResponse, WatchDirective,
    },
};

pub const FIRST_POLL_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_POLL_BACKOFF: Duration = Duration::from_secs(60);
const MAX_RETRY_AFTER: Duration = Duration::from_mins(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlAction {
    Poll(PollRequest),
    Wait { until: Instant },
    NeedToken,
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlOutcome {
    Directive(PollResponse),
    Unauthorized,
    Retry { retry_after: Option<Duration> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlEffect {
    Revoked,
    SetWatch(Option<WatchDirective>),
    Flush(FlushDirective),
    ApplyPolicy(PolicyDirective),
    Burst(BurstDirective),
}

#[derive(Clone, Debug)]
pub struct ControlMachine {
    known_version: u64,
    last_flush_id: u64,
    policy_version: u64,
    sanitizer_version: u64,
    projects: Vec<String>,
    services: Vec<String>,
    next_poll: Instant,
    failures: u32,
    need_token: bool,
    revoked: bool,
    active_watch: Option<WatchDirective>,
}

impl ControlMachine {
    #[must_use]
    pub const fn new(now: Instant, projects: Vec<String>, services: Vec<String>) -> Self {
        Self {
            known_version: 0,
            last_flush_id: 0,
            policy_version: 1,
            sanitizer_version: 1,
            projects,
            services,
            next_poll: now,
            failures: 0,
            need_token: false,
            revoked: false,
            active_watch: None,
        }
    }

    #[must_use]
    pub const fn needs_token(&self) -> bool {
        self.need_token
    }

    #[must_use]
    pub const fn is_revoked(&self) -> bool {
        self.revoked
    }

    #[must_use]
    pub const fn last_flush_id(&self) -> u64 {
        self.last_flush_id
    }

    pub const fn token_refreshed(&mut self, now: Instant) {
        if self.need_token {
            self.need_token = false;
            self.failures = 0;
            self.next_poll = now;
        }
    }

    pub const fn mark_flush_done(&mut self, flush_id: u64, now: Instant) {
        if flush_id > self.last_flush_id {
            self.last_flush_id = flush_id;
            if self.failures == 0 && !self.need_token {
                self.next_poll = now;
            }
        }
    }

    #[must_use]
    pub fn next(&self, now: Instant, has_token: bool) -> ControlAction {
        if self.revoked {
            return ControlAction::Revoked;
        }
        if self.need_token || !has_token {
            return ControlAction::NeedToken;
        }
        if now.0.as_nanos() >= self.next_poll.0.as_nanos() {
            ControlAction::Poll(PollRequest {
                known_version: self.known_version,
                last_flush_id: self.last_flush_id,
                policy_version: self.policy_version,
                sanitizer_version: self.sanitizer_version,
                projects: self.projects.clone(),
                services: self.services.clone(),
                hold_ms: None,
            })
        } else {
            ControlAction::Wait {
                until: self.next_poll,
            }
        }
    }

    pub fn on(&mut self, outcome: ControlOutcome, now: Instant, jitter: f64) -> Vec<ControlEffect> {
        assert!((0.0..=1.0).contains(&jitter));
        let mut effects = Vec::new();
        match outcome {
            ControlOutcome::Directive(resp) => {
                self.failures = 0;
                self.need_token = false;
                self.known_version = resp.version;
                if resp.revoked {
                    self.revoked = true;
                    effects.push(ControlEffect::Revoked);
                    return effects;
                }
                if resp.watch != self.active_watch || resp.watch.is_some() {
                    self.active_watch.clone_from(&resp.watch);
                    effects.push(ControlEffect::SetWatch(resp.watch));
                }
                if let Some(flush) = resp.flush
                    && flush.id > self.last_flush_id
                {
                    effects.push(ControlEffect::Flush(flush));
                }
                if let Some(policy) = resp.policy {
                    let newer_policy = policy.version > self.policy_version;
                    let newer_sanitizer = policy.sanitizer_version > self.sanitizer_version;
                    if newer_policy || newer_sanitizer {
                        self.policy_version = self.policy_version.max(policy.version);
                        self.sanitizer_version =
                            self.sanitizer_version.max(policy.sanitizer_version);
                        effects.push(ControlEffect::ApplyPolicy(policy));
                    }
                }
                if let Some(burst) = resp.burst {
                    effects.push(ControlEffect::Burst(burst));
                }
                let wait = Duration::from_millis(resp.poll_after_ms.min(60_000));
                self.next_poll = now.after(wait);
            }
            ControlOutcome::Unauthorized => {
                self.need_token = true;
                self.next_poll = now;
            }
            ControlOutcome::Retry { retry_after } => {
                self.failures = self.failures.saturating_add(1);
                let shift = self.failures.saturating_sub(1).min(10);
                let ceiling = FIRST_POLL_BACKOFF
                    .saturating_mul(1 << shift)
                    .min(MAX_POLL_BACKOFF);
                let wait = (ceiling / 2).saturating_add((ceiling / 2).mul_f64(jitter));
                let wait = wait.max(retry_after.unwrap_or_default().min(MAX_RETRY_AFTER));
                self.next_poll = now.after(wait);
            }
        }
        effects
    }
}
