//! Kernel-side ground truth for watched memory ranges (`Watching`).
//!
//! Records every matching CPU load/store and checks whether hardware debug
//! registers actually fired, catching missed watchpoints or unnotified resumes.

use std::collections::{BTreeMap, BTreeSet};

use super::Tid;
use super::debug_regs::DebugRegisters;
use crate::sim::cpu::Accesses;
use crate::sim::memory::AddressSpace;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserWatch {
    pub id: u64,

    pub spans: Vec<(u64, u64)>,

    pub loads: bool,

    pub every: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnseenWatch {
    pub tid: Tid,
    pub watch: u64,
    pub address: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LostWatch {
    pub tid: Tid,
    pub watch: u64,
}

pub type Watched = Vec<Option<Vec<Vec<u8>>>>;

#[derive(Debug, Default)]
pub struct Watching {
    pub watches: Vec<UserWatch>,

    pub unseen: Vec<UnseenWatch>,

    pub lost: Vec<LostWatch>,

    pub log: BTreeMap<Tid, BTreeSet<u64>>,

    pub unchanged: BTreeSet<u64>,

    pub counts: BTreeMap<u64, u64>,

    pub stored: BTreeMap<u64, Vec<Vec<u8>>>,

    pub ran: BTreeSet<Tid>,
}

impl Watching {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            watches: Vec::new(),
            unseen: Vec::new(),
            lost: Vec::new(),
            log: BTreeMap::new(),
            unchanged: BTreeSet::new(),
            counts: BTreeMap::new(),
            stored: BTreeMap::new(),
            ran: BTreeSet::new(),
        }
    }

    pub fn resume(&mut self, tid: Tid) {
        let Some(accessed) = self.log.get(&tid) else {
            return;
        };

        for watch in &self.watches {
            if watch.every && accessed.contains(&watch.id) {
                self.lost.push(LostWatch {
                    tid,
                    watch: watch.id,
                });
            }
        }
    }

    #[must_use]
    pub fn bytes(&self, debugged: bool, space: &AddressSpace) -> Watched {
        if !debugged {
            return Vec::new();
        }

        self.watches
            .iter()
            .map(|watch| {
                watch
                    .spans
                    .iter()
                    .map(|&(start, end)| {
                        space.peek_bytes(start, usize::try_from(end - start).ok()?)
                    })
                    .collect()
            })
            .collect()
    }

    pub fn follow(
        &mut self,
        tid: Tid,
        debug: &DebugRegisters,
        accesses: &Accesses,
        before: &Watched,
        after: &Watched,
    ) {
        self.ran.insert(tid);
        for ((watch, before), after) in self.watches.iter().zip(before).zip(after) {
            let mut counted = false;

            for access in accesses.iter() {
                let overlaps = watch.spans.iter().any(|&(start, end)| {
                    access.address < end && start < access.address + access.size
                });
                if !overlaps || !(access.write || watch.loads) {
                    continue;
                }

                self.log.entry(tid).or_default().insert(watch.id);
                if !std::mem::replace(&mut counted, true) {
                    *self.counts.entry(watch.id).or_default() += 1;
                }

                if access.write && before == after {
                    self.unchanged.insert(watch.id);
                }

                if access.write
                    && let Some(spans) = after
                {
                    self.stored
                        .entry(watch.id)
                        .or_default()
                        .push(spans.concat());
                }

                let mut alone = Accesses::default();
                alone.push(access);
                if debug.hits(&alone) == 0 {
                    self.unseen.push(UnseenWatch {
                        tid,
                        watch: watch.id,
                        address: access.address,
                    });
                }
            }
        }
    }

    pub fn restart(&mut self) {
        self.log.clear();
        self.unchanged.clear();
        self.counts.clear();
        self.stored.clear();
        self.ran.clear();
    }
}
