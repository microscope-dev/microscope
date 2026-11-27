//! Kernel-side ground truth for active syscall catchpoints (`Catching`).
//!
//! Tracks catchpoint rules so the kernel can flag syscalls that bypassed
//! `PTRACE_SYSCALL` or were resumed without reporting a stop.

use std::collections::BTreeMap;

use super::Tid;
use crate::sim::cpu::{RAX, RDI, Registers};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserCatch {
    pub calls: Vec<u64>,
    pub entry: bool,
    pub exit: bool,

    pub arg0: Option<u64>,

    pub ret: Option<i64>,
}

impl UserCatch {
    #[must_use]
    pub fn stops(&self, number: u64, registers: &Registers, exit: bool) -> bool {
        (self.calls.is_empty() || self.calls.contains(&number))
            && if exit { self.exit } else { self.entry }
            && self
                .arg0
                .is_none_or(|value| registers.general[RDI] == value)
            && self
                .ret
                .is_none_or(|value| !exit || registers.general[RAX].cast_signed() == value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnseenCatch {
    pub tid: Tid,
    pub call: u64,
    pub exit: bool,
}

#[derive(Debug, Default)]
pub struct Catching {
    pub rules: Vec<UserCatch>,
    pub unseen: Vec<UnseenCatch>,

    pub unreported: BTreeMap<Tid, UnseenCatch>,

    pub skipped: Vec<UnseenCatch>,
}

impl Catching {
    #[must_use]
    pub fn stops(&self, number: u64, registers: &Registers, exit: bool) -> bool {
        self.rules
            .iter()
            .any(|rule| rule.stops(number, registers, exit))
    }
}
