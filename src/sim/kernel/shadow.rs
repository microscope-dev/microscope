//! Ground-truth shadow call stack (`Shadow`) and step trajectory (`Tracking`).
//!
//! Maintained directly from CPU `Call`/`Return`/`Jump` events so backtrace,
//! tail-call, sample-unwinding, and stepping oracles can compare the debugger's
//! output against what the thread actually executed.

use super::Tid;
use crate::sim::cpu::{Flow, RSP, Registers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Call {
    pub return_address: u64,

    pub slot: u64,

    pub activation: u64,

    pub target: u64,

    pub entry: [u64; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Jump {
    pub from: u64,

    pub next: u64,

    pub to: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shadow {
    pub base: u64,

    pub calls: Vec<Call>,

    pub jumps: Vec<Vec<Jump>>,

    pub lost: bool,
}

impl Shadow {
    #[must_use]
    pub fn activation(&self) -> u64 {
        self.calls.last().map_or(self.base, |call| call.activation)
    }

    #[must_use]
    pub const fn depth(&self) -> usize {
        self.calls.len()
    }

    #[must_use]
    pub fn jumps(&self, index: usize) -> &[Jump] {
        self.jumps.get(index).map_or(&[], Vec::as_slice)
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub rip: u64,

    pub depth: usize,
    pub activation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tracking {
    pub tid: Tid,

    pub depth: usize,

    pub positions: Vec<Position>,
}

impl Tracking {
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
