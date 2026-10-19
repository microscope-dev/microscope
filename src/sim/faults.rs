//! Faults: what the rest of the machine does to a debugging session.
//!
//! Each fault is something Linux can do, never an invented failure. A run
//! plans at most one, aimed at a moment where bugs are likely rather than
//! left to uniform chance, and reports a plan that never fired: a sweep
//! whose faults silently never happen tests nothing.

use std::fmt;

use super::choices::{Choices, Stream};

/// Where an external SIGKILL lands (K-EXIT-3): any process can be killed at
/// any moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// At the first action from this one on while a program runs.
    KillAtStep(u64),

    /// Right after the program creates its Nth thread.
    KillNearClone(u64),

    /// Between two ptrace requests: before the Nth call the controller makes
    /// into the kernel takes effect.
    KillInsideCall(u64),

    /// Right after the program forks its Nth child.
    KillNearFork(u64),
}

impl Plan {
    /// Chooses a fault for a run, aimed early enough that most sessions
    /// reach it. Only a program that creates threads plans a fault near
    /// one, and only one that forks near a fork, which, rarer than a clone,
    /// it aims at more often than at any other moment.
    pub fn choose(choices: &mut Choices, creates_threads: bool, forks: bool) -> Self {
        let weights = [1, 1, u64::from(creates_threads), 5 * u64::from(forks)];

        match choices.weighted(Stream::Fault, &weights) {
            0 => Self::KillAtStep(choices.below(Stream::Fault, 300) + 1),
            1 => Self::KillInsideCall(choices.below(Stream::Fault, 100) + 1),
            2 => Self::KillNearClone(choices.below(Stream::Fault, 2) + 1),
            _ => Self::KillNearFork(choices.below(Stream::Fault, 2) + 1),
        }
    }

    /// The fault's kind, for grouping.
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

/// A run's planned fault and its progress towards firing.
#[derive(Debug, Default)]
pub struct Faults {
    pub plan: Option<Plan>,

    /// The action at which the fault fired.
    pub fired: Option<u64>,

    /// How many threads programs created so far.
    clones: u64,

    /// How many children programs forked so far.
    forks: u64,

    /// How many calls the controller made into the kernel so far.
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

    /// Whether the plan fires at `step`, with a program running.
    #[must_use]
    pub const fn at_step(&self, step: u64) -> bool {
        matches!(self.plan, Some(Plan::KillAtStep(due)) if self.fired.is_none() && step >= due)
    }

    /// Counts a new thread. Returns whether the plan fires now.
    pub const fn cloned(&mut self) -> bool {
        self.clones += 1;
        matches!(self.plan, Some(Plan::KillNearClone(due)) if self.fired.is_none() && self.clones >= due)
    }

    /// Counts a fork. Returns whether the plan fires now.
    pub const fn forked(&mut self) -> bool {
        self.forks += 1;
        matches!(self.plan, Some(Plan::KillNearFork(due)) if self.fired.is_none() && self.forks >= due)
    }

    /// Counts a call into the kernel. Returns whether the plan fires now.
    pub const fn call(&mut self) -> bool {
        self.calls += 1;
        matches!(self.plan, Some(Plan::KillInsideCall(due)) if self.fired.is_none() && self.calls >= due)
    }
}
