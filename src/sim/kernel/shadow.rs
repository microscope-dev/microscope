//! Shadow state: what threads really did, kept where the debugger cannot
//! see or change it, for the semantic oracles.
//!
//! Each thread's shadow call stack holds the calls it made and has not
//! returned from, as the CPU executed them, and the jumps each of their
//! activations took with its stack as the call left it, as a tail call
//! does; backtraces are judged by it.
//! While the client steps a thread, the kernel also records each
//! instruction that thread completed, so the stepping oracle can tell where
//! a step should have stopped. A trap the thread hit is no instruction it
//! passed.

use super::Tid;
use crate::sim::cpu::{Flow, RSP, Registers};

/// A call a thread made and has not returned from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Call {
    /// Where the call returns to.
    pub return_address: u64,

    /// Where the call pushed the return address.
    pub slot: u64,

    /// The activation the call began.
    pub activation: u64,

    /// Where the call went.
    pub target: u64,

    /// The general registers the callee was entered with.
    pub entry: [u64; 16],
}

/// A jump an activation took with its stack as its call left it, holding
/// only the return address: a tail call does, though a function without
/// a frame of its own may jump within itself so too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Jump {
    /// The jump instruction.
    pub from: u64,

    /// The instruction after it.
    pub next: u64,

    /// Where it went.
    pub to: u64,
}

/// A thread's calls, as the CPU executed them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shadow {
    /// The activation the thread began in, which no call made.
    pub base: u64,

    /// Calls not yet returned from, outermost first.
    pub calls: Vec<Call>,

    /// For each of `calls`, the jumps its activation took with its stack
    /// as the call left it, each once.
    pub jumps: Vec<Vec<Jump>>,

    /// Whether a return went somewhere no call said, after which the
    /// shadow no longer describes the stack.
    pub lost: bool,
}

impl Shadow {
    /// The activation the thread runs in now.
    #[must_use]
    pub fn activation(&self) -> u64 {
        self.calls.last().map_or(self.base, |call| call.activation)
    }

    /// How many calls deep the thread is.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.calls.len()
    }

    /// The jumps the activation that `calls[index]` began took with its
    /// stack as the call left it.
    #[must_use]
    pub fn jumps(&self, index: usize) -> &[Jump] {
        self.jumps.get(index).map_or(&[], Vec::as_slice)
    }

    /// Follows an instruction that completed, leaving `registers` as it
    /// left them. A new activation takes `next_activation`, which is then
    /// advanced.
    pub(super) fn follow(&mut self, flow: Flow, registers: &Registers, next_activation: &mut u64) {
        let rip = registers.rip;

        match flow {
            Flow::Call {
                return_address,
                slot,
            } => {
                self.calls.push(Call {
                    return_address,
                    slot,
                    activation: *next_activation,
                    target: rip,
                    entry: registers.general,
                });
                self.jumps.resize(self.calls.len(), Vec::new());
                *next_activation += 1;
            }
            Flow::Return => {
                match self.calls.pop() {
                    Some(call) if call.return_address == rip => {}
                    _ => self.lost = true,
                }

                self.jumps.truncate(self.calls.len());
            }
            Flow::Jump { from, next } => {
                let Some(call) = self.calls.last() else {
                    return;
                };

                if registers.general[RSP] != call.slot {
                    return;
                }

                self.jumps.resize(self.calls.len(), Vec::new());
                let jump = Jump {
                    from,
                    next,
                    to: rip,
                };

                let jumps = self.jumps.last_mut().expect("a call's jumps");
                if !jumps.contains(&jump) {
                    jumps.push(jump);
                }
            }
            Flow::Other => {}
        }
    }
}

/// Where a thread was when it executed an instruction, which completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub rip: u64,

    /// How many calls deep it was.
    pub depth: usize,
    pub activation: u64,
}

/// The positions one thread passes through while the client steps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracking {
    pub tid: Tid,

    /// Positions deeper than this many calls are not recorded: no step
    /// must stop in a callee it entered, so none decides an oracle.
    pub depth: usize,

    /// Positions in the order the thread passed them, without repeats of
    /// the one before.
    pub positions: Vec<Position>,
}

impl Tracking {
    /// Records that `tid` completed the instruction at `rip`, where its
    /// shadow was as given.
    pub(super) fn note(&mut self, tid: Tid, shadow: &Shadow, rip: u64) {
        if tid != self.tid || shadow.depth() > self.depth {
            return;
        }

        let position = Position {
            rip,
            depth: shadow.depth(),
            activation: shadow.activation(),
        };

        if self.positions.last() != Some(&position) {
            self.positions.push(position);
        }
    }
}
