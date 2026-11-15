#![expect(
    clippy::indexing_slicing,
    reason = "the simulator is a test harness and reports a bug by panicking"
)]

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
pub mod shrink;
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

    if outcome.tape.is_none() {
        let _ = writeln!(
            text,
            "replay   just sim-seed {:#x} --fingerprint {:#x}",
            outcome.seed, outcome.fingerprint
        );
    }

    text
}
