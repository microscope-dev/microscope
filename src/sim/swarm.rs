//! A run's shape, chosen from its seed before the session starts.
//!
//! Swarm testing (Groce et al.) varies which features are active and how
//! intensely, rather than enabling everything always, because some bugs
//! only appear when other activity stays quiet.

use std::fmt;

use super::choices::{Choices, Stream};
use super::corpus::Corpus;
use super::faults::Plan;
use super::kernel::DebugBehavior;
use super::schedule::Policy;

/// How likely each kind of action is, relative to the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Weights {
    pub run: u64,
    pub collect: u64,
    pub deliver: u64,
    pub poll: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swarm {
    pub program: usize,
    pub variant: usize,

    /// Which of the program's manifest runs, and so which arguments.
    pub run: usize,

    /// How the world chooses its actions.
    pub policy: Policy,

    /// How often, in a thousand, other actors act before a call the
    /// controller makes into the kernel.
    pub preempt: u64,

    /// The fault the run plans, if any.
    pub fault: Option<Plan>,

    /// The most instructions one `Run` action executes.
    pub burst: u64,

    /// The controller queue's capacity.
    pub queue_capacity: usize,

    /// The event channel's capacity; small ones make clients lag.
    pub event_capacity: usize,

    /// Whether launches stop at the program's first instruction.
    pub stop_at_entry: bool,

    /// How many requests the client makes before it shuts down.
    pub requests: u64,

    /// How many times the client may launch the program.
    pub launches: u64,

    /// Breakpoints the client adds before its first launch.
    pub early_breakpoints: u64,

    /// How the debug registers answer the tracer.
    pub debug: DebugBehavior,

    /// Whether the client favors watching memory.
    pub watching: bool,

    /// Whether the program starts untraced, for the client to attach to
    /// before it launches anything, and how many instructions the program
    /// runs before the client acts.
    pub attach: Option<u64>,

    /// Whether the client stops at system calls.
    pub catching: bool,
}

impl Swarm {
    /// Chooses a run's shape from the `Swarm` stream.
    pub fn choose(choices: &mut Choices, corpus: &Corpus) -> Self {
        let mut index = |count: usize| {
            usize::try_from(choices.below(Stream::Swarm, count as u64)).expect("an index fits")
        };

        let program = index(corpus.programs.len());
        let variant = index(corpus.programs[program].variants.len());
        let run = index(corpus.programs[program].runs.len());
        let mut pick = |options: &[u64]| *choices.pick(Stream::Swarm, options);
        let weight = [1, 4, 16];
        let weights = Weights {
            run: pick(&weight),
            collect: pick(&weight),
            deliver: pick(&weight),
            poll: pick(&weight),
        };

        let policy = if pick(&[0, 1]) == 0 {
            Policy::Walk(weights)
        } else {
            Policy::Pct {
                depth: pick(&[1, 2, 3, 4]),
                horizon: pick(&[32, 128, 512]),
            }
        };

        let preempt = pick(&[0, 20, 200, 600]);
        let creates_threads = corpus.programs[program].variants[variant]
            .functions
            .iter()
            .any(|function| function == "rt_clone");
        let forks = corpus.programs[program].variants[variant]
            .functions
            .iter()
            .any(|function| function == "rt_fork");

        // A program that forks plans a fault in three seeds of four: what
        // becomes of its children when it dies near a fork is much of what
        // it exists to show.
        let faulty = pick(if forks { &[0, 1, 1, 1] } else { &[0, 1] }) == 1;
        let fault = faulty.then(|| Plan::choose(choices, creates_threads, forks));

        let mut pick = |options: &[u64]| *choices.pick(Stream::Swarm, options);
        Self {
            program,
            variant,
            run,
            policy,
            preempt,
            fault,
            burst: pick(&[1, 8, 64, 512]),
            queue_capacity: usize::try_from(pick(&[1, 2, 8, 32])).expect("small"),
            event_capacity: usize::try_from(pick(&[2, 16, 1024])).expect("small"),
            stop_at_entry: pick(&[0, 1]) == 1,
            requests: pick(&[4, 16, 48, 128]),
            launches: pick(&[1, 2]),
            early_breakpoints: pick(&[0, 1, 3]),
            debug: match pick(&[0, 0, 0, 0, 1, 2, 2, 2]) {
                0 => DebugBehavior::Faithful,
                1 => DebugBehavior::Discarding,
                _ => DebugBehavior::Contended(usize::try_from(pick(&[2, 3, 4, 4])).expect("small")),
            },
            watching: pick(&[0, 1]) == 1,
            // Early enough that most attaches find the program still running.
            attach: (pick(&[0, 1]) == 1).then(|| choices.below(Stream::Swarm, 500)),
            catching: choices.below(Stream::Swarm, 3) == 0,
        }
    }
}

impl fmt::Display for Swarm {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} preempt={} fault={} burst={} queue={} events={} entry={} requests={} \
             launches={} early-breakpoints={} debug={:?} watching={} attach={} catching={}",
            self.policy,
            self.preempt,
            self.fault
                .map_or_else(|| "none".to_owned(), |fault| fault.to_string()),
            self.burst,
            self.queue_capacity,
            self.event_capacity,
            self.stop_at_entry,
            self.requests,
            self.launches,
            self.early_breakpoints,
            self.debug,
            self.watching,
            self.attach
                .map_or_else(|| "none".to_owned(), |after| format!("after({after})")),
            self.catching,
        )
    }
}
