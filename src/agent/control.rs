use std::time::Duration;

use super::{
    machine::Instant,
    wire::{
        AgentLimits, BurstDirective, FlushDirective, PolicyDirective, PollRequest, PollResponse,
        WatchDirective,
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
    Directive(Box<PollResponse>),
    Unauthorized,
    Retry { retry_after: Option<Duration> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlEffect {
    Revoked,
    SetWatch(Option<WatchDirective>),
    Flush(FlushDirective),
    ApplyPolicy(PolicyDirective),
    Bursts(Vec<BurstDirective>),
}

#[derive(Clone, Debug)]
pub struct ControlMachine {
    known_version: u64,
    last_flush_id: u64,
    dispatched_flush_id: u64,
    empty_flush_id: u64,
    policy_version: u64,
    sanitizer_version: u64,
    dispatched_policy_version: u64,
    dispatched_sanitizer_version: u64,
    projects: Vec<String>,
    services: Vec<String>,
    limits: Option<AgentLimits>,
    next_poll: Instant,
    failures: u32,
    need_token: bool,
    revoked: bool,
    active_watch: Option<WatchDirective>,
    active_bursts: Vec<BurstDirective>,
}

impl ControlMachine {
    #[must_use]
    pub const fn new(now: Instant, projects: Vec<String>, services: Vec<String>) -> Self {
        Self {
            known_version: 0,
            last_flush_id: 0,
            dispatched_flush_id: 0,
            empty_flush_id: 0,
            policy_version: 0,
            sanitizer_version: 0,
            dispatched_policy_version: 0,
            dispatched_sanitizer_version: 0,
            projects,
            services,
            limits: None,
            next_poll: now,
            failures: 0,
            need_token: false,
            revoked: false,
            active_watch: None,
            active_bursts: Vec::new(),
        }
    }

    pub fn set_limits(&mut self, limits: Option<AgentLimits>) {
        self.limits = limits;
    }

    pub fn set_targets(&mut self, projects: Vec<String>, services: Vec<String>) {
        self.projects = projects;
        self.services = services;
    }

    pub fn reset(&mut self, now: Instant) {
        self.known_version = 0;
        self.last_flush_id = 0;
        self.dispatched_flush_id = 0;
        self.empty_flush_id = 0;
        self.policy_version = 0;
        self.sanitizer_version = 0;
        self.dispatched_policy_version = 0;
        self.dispatched_sanitizer_version = 0;
        self.next_poll = now;
        self.failures = 0;
        self.active_bursts.clear();
        self.need_token = false;
        self.revoked = false;
        self.active_watch = None;
    }

    pub const fn set_applied_policy(&mut self, policy_version: u64, sanitizer_version: u64) {
        if policy_version > self.policy_version {
            self.policy_version = policy_version;
        }
        if sanitizer_version > self.sanitizer_version {
            self.sanitizer_version = sanitizer_version;
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

    #[must_use]
    pub const fn empty_flush_id(&self) -> u64 {
        self.empty_flush_id
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

    pub const fn mark_empty_flush(&mut self, flush_id: u64, now: Instant) {
        if flush_id > self.empty_flush_id {
            self.empty_flush_id = flush_id;
            if flush_id > self.last_flush_id {
                self.last_flush_id = flush_id;
            }
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
                empty_flush_id: self.empty_flush_id,
                policy_version: self.policy_version,
                sanitizer_version: self.sanitizer_version,
                projects: self.projects.clone(),
                services: self.services.clone(),
                limits: self.limits.clone(),
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
                    && flush.id > self.dispatched_flush_id
                {
                    self.dispatched_flush_id = flush.id;
                    effects.push(ControlEffect::Flush(flush));
                }
                if let Some(policy) = resp.policy {
                    let changed_policy = policy.version > self.dispatched_policy_version;
                    let changed_sanitizer =
                        policy.sanitizer_version > self.dispatched_sanitizer_version;
                    if changed_policy || changed_sanitizer {
                        if changed_policy {
                            self.dispatched_policy_version = policy.version;
                        }
                        if changed_sanitizer {
                            self.dispatched_sanitizer_version = policy.sanitizer_version;
                        }
                        effects.push(ControlEffect::ApplyPolicy(policy));
                    }
                }
                if resp.bursts != self.active_bursts {
                    self.active_bursts.clone_from(&resp.bursts);
                    effects.push(ControlEffect::Bursts(resp.bursts));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_response(version: u64) -> PollResponse {
        PollResponse {
            version,
            revoked: false,
            watch: None,
            flush: None,
            policy: None,
            bursts: Vec::new(),
            poll_after_ms: 1000,
        }
    }

    #[test]
    fn control_machine_dedups_flush_detects_burst_edits_and_handles_revocation() {
        let t0 = Instant(Duration::ZERO);
        let mut m = ControlMachine::new(t0, vec!["p".into()], vec!["s".into()]);

        let mut resp1 = empty_response(1);
        resp1.flush = Some(FlushDirective {
            id: 10,
            deadline_ms: 5_000,
        });
        resp1.bursts = vec![BurstDirective {
            id: 1,
            duration_ms: 60_000,
            preset: "dev".into(),
            time_unit: Some("1us".into()),
            project: Some("p".into()),
            ends_at_ns: Some(1_000_000),
        }];
        let effects1 = m.on(ControlOutcome::Directive(Box::new(resp1)), t0, 0.5);
        assert!(effects1.contains(&ControlEffect::Flush(FlushDirective {
            id: 10,
            deadline_ms: 5_000,
        })));
        assert_eq!(
            effects1
                .iter()
                .filter(|e| matches!(e, ControlEffect::Bursts(_)))
                .count(),
            1
        );

        let mut resp2 = empty_response(2);
        resp2.flush = Some(FlushDirective {
            id: 10,
            deadline_ms: 5_000,
        });
        resp2.bursts = vec![BurstDirective {
            id: 1,
            duration_ms: 120_000,
            preset: "full".into(),
            time_unit: Some("1ns".into()),
            project: Some("p".into()),
            ends_at_ns: Some(2_000_000),
        }];
        let effects2 = m.on(ControlOutcome::Directive(Box::new(resp2)), t0, 0.5);
        assert!(
            !effects2
                .iter()
                .any(|e| matches!(e, ControlEffect::Flush(_))),
            "in-flight flush must not be re-dispatched"
        );
        assert!(
            effects2
                .iter()
                .any(|e| matches!(e, ControlEffect::Bursts(_))),
            "edited burst with same id must emit Bursts effect"
        );

        let effects_unauth = m.on(ControlOutcome::Unauthorized, t0, 0.5);
        assert!(effects_unauth.is_empty());
        assert_eq!(m.next(t0, true), ControlAction::NeedToken);
        m.token_refreshed(t0);
        assert!(matches!(m.next(t0, true), ControlAction::Poll(_)));

        let mut rev = empty_response(3);
        rev.revoked = true;
        let effects_rev = m.on(ControlOutcome::Directive(Box::new(rev)), t0, 0.5);
        assert_eq!(effects_rev, vec![ControlEffect::Revoked]);
        assert_eq!(m.next(t0, true), ControlAction::Revoked);
    }
}
