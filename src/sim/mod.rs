//! Deterministic simulation of debugging sessions; see `plans/simulator.md`.
//!
//! A run drives the real controller and `DebuggerHandle` against a simulated
//! Linux kernel and x86-64 CPU, so that one seed names one complete,
//! reproducible session.
//!
//! - [`world`]: the step loop that schedules every action of a session, and
//!   `machine`, what its actions and the controller's calls reach.
//! - [`kernel`], [`cpu`], [`memory`], [`loader`]: the simulated machine.
//! - [`client`]: the simulated user, driving `DebuggerHandle`.
//! - [`oracles`], `semantics`, and `audit`: checks of the debugger against
//!   ground truth, and `samples`, of the profiler's samples.
//! - [`choices`], [`swarm`], [`schedule`]: where every random choice comes
//!   from, and how the next action is chosen.
//! - [`faults`]: what the rest of the machine does to a session.
//! - [`corpus`]: the golden programs, with [`facts`] about each binary and
//!   [`markers`] in their sources.
//! - [`report`]: traces, fingerprints, and failures.

mod audit;
mod catches;
pub(crate) mod choices;

mod client;
#[cfg(test)]
mod conformance;
pub(crate) mod corpus;

pub(crate) mod cpu;

pub(crate) mod facts;

pub mod faults;
mod hits;
pub(crate) mod kernel;

pub(crate) mod loader;

mod machine;
pub(crate) mod markers;

pub(crate) mod marks;

pub(crate) mod memory;

mod oracles;
pub(crate) mod report;

mod samples;
pub(crate) mod schedule;

mod semantics;
pub(crate) mod swarm;

#[cfg(test)]
mod tests;
mod tracing;
mod views;
mod watches;
pub(crate) mod world;

use std::fmt::Write as _;

pub use corpus::Corpus;
pub use marks::Reach;
pub use world::{Outcome, Settings, run};

/// A failed run, written to be understood without rerunning it.
#[must_use]
pub fn describe_failure(outcome: &Outcome) -> String {
    let mut text = String::new();
    if let Some(failure) = &outcome.failure {
        let _ = writeln!(
            text,
            "SIM FAILURE  {}  seed={:#018x}",
            failure.kind, outcome.seed
        );
        let _ = writeln!(text, "program  {} {:?}", outcome.program, outcome.arguments);
        let _ = writeln!(text, "swarm    {}", outcome.swarm);
        let _ = writeln!(
            text,
            "{}  #{} {}",
            failure.check, failure.step, failure.message
        );
    }

    if let Some(plan) = outcome.unfired {
        let _ = writeln!(text, "fault    {plan} never fired");
    }

    if outcome.dropped > 0 {
        let _ = writeln!(text, "  ... {} earlier lines", outcome.dropped);
    }

    for line in &outcome.trace {
        let _ = writeln!(text, "  {line}");
    }

    let _ = writeln!(
        text,
        "replay   just sim-seed {:#x} --fingerprint {:#x}",
        outcome.seed, outcome.fingerprint
    );

    text
}
