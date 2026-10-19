//! What the kernel follows of the watchpoints the client was told exist.
//!
//! It notes which threads accessed them, whether an armed slot covered
//! each access, and whether a thread ran on from an access the debugger
//! must report.

use std::collections::{BTreeMap, BTreeSet};

use super::Tid;
use super::debug_regs::DebugRegisters;
use crate::sim::cpu::Accesses;
use crate::sim::memory::AddressSpace;

/// A watch the client was told exists, whose accesses the kernel follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserWatch {
    pub id: u64,

    /// The hardware spans covering it, as start and end.
    pub spans: Vec<(u64, u64)>,

    /// Whether loads count too.
    pub loads: bool,

    /// Whether the debugger reports every access, rather than only those
    /// that change the bytes or that its conditions let stop.
    pub every: bool,
}

/// An access to a watched range by a thread no slot of which covered it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnseenWatch {
    pub tid: Tid,
    pub watch: u64,
    pub address: u64,
}

/// A thread that ran on from an access the debugger reports every one of,
/// with no stop published since: a hit the debugger lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LostWatch {
    pub tid: Tid,
    pub watch: u64,
}

/// The watched bytes of each watch, span by span, where mapped.
pub type Watched = Vec<Option<Vec<Vec<u8>>>>;

#[derive(Debug, Default)]
pub struct Watching {
    /// The watches the client was told exist, as it last knew them.
    pub watches: Vec<UserWatch>,

    /// Accesses to them no armed slot covered.
    pub unseen: Vec<UnseenWatch>,

    /// Hits lost to threads running on.
    pub lost: Vec<LostWatch>,

    /// Per thread, the watches it accessed since the world last looked.
    pub log: BTreeMap<Tid, BTreeSet<u64>>,

    /// The watches a store left as they were since the world last looked.
    pub unchanged: BTreeSet<u64>,

    /// Per watch, how many instructions accessed it since the world last
    /// looked, each of which raised one debug exception.
    pub counts: BTreeMap<u64, u64>,

    /// Per watch, the bytes each store since the world last looked left,
    /// where all were mapped.
    pub stored: BTreeMap<u64, Vec<Vec<u8>>>,

    /// The threads that executed an instruction since the world last
    /// looked.
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

    /// Notes `tid` running again. An access traps once its instruction
    /// completes, holding the thread there until the debugger, having
    /// reported it or its conditions having declined it, resumes it.
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

    /// The bytes every watch covers in `space`: none unless it is the
    /// memory of the process the debugger controls, which the watches are
    /// in. Its fork children have memory of their own.
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

    /// Follows one instruction's `accesses` by `tid`, whose slots are
    /// `debug`, given the watched bytes before and after it.
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

    /// Starts following again from a stop the world judged.
    pub fn restart(&mut self) {
        self.log.clear();
        self.unchanged.clear();
        self.counts.clear();
        self.stored.clear();
        self.ran.clear();
    }
}
