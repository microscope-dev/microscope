use std::time::Duration;

pub const FIRST_RETRY: Duration = Duration::from_secs(10);
pub const LAST_RETRY: Duration = Duration::from_mins(10);
pub const BLOCKED_RETRY: Duration = Duration::from_hours(1);
const MAX_RETRY_AFTER: Duration = Duration::from_hours(1);
const MIN_REFRESH: Duration = Duration::from_secs(10);
const MIN_REFRESH_LOOPBACK: Duration = Duration::from_secs(1);
const MAX_REFRESH: Duration = Duration::from_hours(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(pub Duration);

impl Instant {
    #[must_use]
    pub const fn after(self, wait: Duration) -> Self {
        Self(self.0.saturating_add(wait))
    }

    #[must_use]
    pub const fn until(self, later: Self) -> Duration {
        later.0.saturating_sub(self.0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Unenrolled { at: Instant, previous: Option<i64> },
    Enrolled { agent_id: i64, refresh_at: Instant },
    Blocked { until: Instant },
    Revoked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Enroll { previous: Option<i64> },
    Refresh { agent_id: i64 },
    Wait { until: Instant },
    WaitForConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Token {
        agent_id: i64,
        refresh_after: Duration,
    },
    Retry {
        retry_after: Option<Duration>,
    },
    EnrollKey,
    Revoked,
    Reenroll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    ForgetKey,
}

#[derive(Clone, Debug)]
pub struct Machine {
    state: State,
    failures: u32,
    reenrolls: u32,
    loopback: bool,
}

impl Machine {
    #[must_use]
    pub const fn new(now: Instant, enrolled: Option<i64>, loopback: bool) -> Self {
        let state = match enrolled {
            Some(agent_id) => State::Enrolled {
                agent_id,
                refresh_at: now,
            },
            None => State::Unenrolled {
                at: now,
                previous: None,
            },
        };
        Self {
            state,
            failures: 0,
            reenrolls: 0,
            loopback,
        }
    }

    #[must_use]
    pub const fn state(&self) -> &State {
        &self.state
    }

    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self.state {
            State::Unenrolled { .. } | State::Enrolled { .. } if self.failures > 0 => "backoff",
            State::Unenrolled { .. } => "unenrolled",
            State::Enrolled { .. } => "enrolled",
            State::Blocked { .. } => "blocked",
            State::Revoked => "revoked",
        }
    }

    #[must_use]
    pub const fn agent_id(&self) -> Option<i64> {
        match self.state {
            State::Enrolled { agent_id, .. } => Some(agent_id),
            _ => None,
        }
    }

    #[must_use]
    pub const fn next(&self, now: Instant) -> Action {
        match self.state {
            State::Unenrolled { at, previous } if now.0.as_nanos() >= at.0.as_nanos() => {
                Action::Enroll { previous }
            }
            State::Unenrolled { at, .. } => Action::Wait { until: at },
            State::Enrolled {
                agent_id,
                refresh_at,
            } if now.0.as_nanos() >= refresh_at.0.as_nanos() => Action::Refresh { agent_id },
            State::Enrolled { refresh_at, .. } => Action::Wait { until: refresh_at },
            State::Blocked { until } if now.0.as_nanos() >= until.0.as_nanos() => {
                Action::Enroll { previous: None }
            }
            State::Blocked { until } => Action::Wait { until },
            State::Revoked => Action::WaitForConfig,
        }
    }

    pub const fn config_changed(&mut self, now: Instant) {
        if matches!(self.state, State::Blocked { .. } | State::Revoked) {
            self.state = State::Unenrolled {
                at: now,
                previous: None,
            };
            self.failures = 0;
            self.reenrolls = 0;
        }
    }

    pub fn on(&mut self, outcome: Outcome, now: Instant, jitter: f64) -> Effect {
        assert!((0.0..=1.0).contains(&jitter));
        let enrolling = matches!(self.state, State::Unenrolled { .. } | State::Blocked { .. });

        match (outcome, self.state.clone()) {
            (_, State::Revoked) => Effect::None,
            (
                Outcome::Token {
                    agent_id,
                    refresh_after,
                },
                _,
            ) => {
                self.failures = 0;
                if !enrolling {
                    self.reenrolls = 0;
                }
                let refresh = self.refresh_wait(refresh_after, jitter);
                self.state = State::Enrolled {
                    agent_id,
                    refresh_at: now.after(refresh),
                };
                Effect::None
            }
            (Outcome::Retry { retry_after }, state) => {
                let wait = self.retry_wait(retry_after, jitter);
                self.state = match state {
                    State::Enrolled { agent_id, .. } => State::Enrolled {
                        agent_id,
                        refresh_at: now.after(wait),
                    },
                    State::Unenrolled { previous, .. } => State::Unenrolled {
                        at: now.after(wait),
                        previous,
                    },
                    State::Blocked { .. } | State::Revoked => State::Unenrolled {
                        at: now.after(wait),
                        previous: None,
                    },
                };
                Effect::None
            }
            (Outcome::EnrollKey, State::Enrolled { .. }) => {
                self.on(Outcome::Retry { retry_after: None }, now, jitter)
            }
            (Outcome::EnrollKey, _) => {
                self.failures = 0;
                self.state = State::Blocked {
                    until: now.after(BLOCKED_RETRY),
                };
                Effect::None
            }
            (Outcome::Revoked, _) => {
                self.state = State::Revoked;
                Effect::ForgetKey
            }
            (Outcome::Reenroll, state) => {
                let previous = match state {
                    State::Enrolled { agent_id, .. } => Some(agent_id),
                    State::Unenrolled { previous, .. } => previous,
                    State::Blocked { .. } | State::Revoked => None,
                };
                self.reenrolls = self.reenrolls.saturating_add(1);
                let wait = if self.reenrolls > 1 {
                    self.retry_wait(None, jitter)
                } else {
                    Duration::ZERO
                };
                self.state = State::Unenrolled {
                    at: now.after(wait),
                    previous,
                };
                Effect::ForgetKey
            }
        }
    }

    fn refresh_wait(&self, refresh_after: Duration, jitter: f64) -> Duration {
        let low = if self.loopback {
            MIN_REFRESH_LOOPBACK
        } else {
            MIN_REFRESH
        };
        refresh_after
            .clamp(low, MAX_REFRESH)
            .mul_f64(jitter.mul_add(-0.1, 1.0))
            .max(low)
    }

    fn retry_wait(&mut self, retry_after: Option<Duration>, jitter: f64) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let doublings = self.failures.saturating_sub(1).min(16);
        let ceiling = FIRST_RETRY.saturating_mul(1 << doublings).min(LAST_RETRY);
        let wait = (ceiling / 2).saturating_add((ceiling / 2).mul_f64(jitter));
        wait.max(retry_after.unwrap_or_default().min(MAX_RETRY_AFTER))
    }
}

#[cfg(test)]
mod tests;
