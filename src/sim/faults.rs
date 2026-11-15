use std::fmt;

use super::choices::{Choices, Stream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    KillAtStep(u64),

    KillNearClone(u64),

    KillInsideCall(u64),

    KillNearFork(u64),
}

impl Plan {
    pub fn choose(choices: &mut Choices, creates_threads: bool, forks: bool) -> Self {
        let weights = [1, 1, u64::from(creates_threads), 5 * u64::from(forks)];

        match choices.weighted(Stream::Fault, &weights) {
            0 => Self::KillAtStep(choices.below(Stream::Fault, 300) + 1),
            1 => Self::KillInsideCall(choices.below(Stream::Fault, 100) + 1),
            2 => Self::KillNearClone(choices.below(Stream::Fault, 2) + 1),
            _ => Self::KillNearFork(choices.below(Stream::Fault, 2) + 1),
        }
    }

    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::KillAtStep(_) => "sigkill at a step",
            Self::KillNearClone(_) => "sigkill near a clone",
            Self::KillInsideCall(_) => "sigkill inside a call",
            Self::KillNearFork(_) => "sigkill near a fork",
        }
    }
}

impl fmt::Display for Plan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KillAtStep(step) => write!(formatter, "sigkill@step({step})"),
            Self::KillNearClone(clone) => write!(formatter, "sigkill@clone({clone})"),
            Self::KillInsideCall(call) => write!(formatter, "sigkill@call({call})"),
            Self::KillNearFork(fork) => write!(formatter, "sigkill@fork({fork})"),
        }
    }
}

#[derive(Debug, Default)]
pub struct Faults {
    pub plan: Option<Plan>,

    pub fired: Option<u64>,

    clones: u64,

    forks: u64,

    calls: u64,
}

impl Faults {
    #[must_use]
    pub const fn new(plan: Option<Plan>) -> Self {
        Self {
            plan,
            fired: None,
            clones: 0,
            forks: 0,
            calls: 0,
        }
    }

    #[must_use]
    pub const fn at_step(&self, step: u64) -> bool {
        matches!(self.plan, Some(Plan::KillAtStep(due)) if self.fired.is_none() && step >= due)
    }

    pub const fn cloned(&mut self) -> bool {
        self.clones += 1;
        matches!(self.plan, Some(Plan::KillNearClone(due)) if self.fired.is_none() && self.clones >= due)
    }

    pub const fn forked(&mut self) -> bool {
        self.forks += 1;
        matches!(self.plan, Some(Plan::KillNearFork(due)) if self.fired.is_none() && self.forks >= due)
    }

    pub const fn call(&mut self) -> bool {
        self.calls += 1;
        matches!(self.plan, Some(Plan::KillInsideCall(due)) if self.fired.is_none() && self.calls >= due)
    }
}
