//! Watchpoint hit accounting comparing reported `WatchpointHit`
//! stops against kernel-logged accesses, byte baselines, and `WatchAccess`
//! semantics (including `WatchAccess::Change` suppressing unchanged stores).

use std::collections::{BTreeMap, BTreeSet};

use super::hits::{Known, Policy};
use super::kernel::{Kernel, Tgid, Tid};
use super::marks::Mark;
use crate::{StateSnapshot, StopReason, ThreadState, WatchAccess, WatchpointHit};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub id: u64,
    pub address: u64,
    pub size: u64,
    pub access: WatchAccess,

    pub spans: Vec<(u64, u64)>,

    pub policies: Vec<Policy>,
}

impl Intent {
    pub fn unconditional(&self) -> bool {
        self.policies.iter().all(|policy| policy.unconditional())
    }

    pub fn reports_every_access(&self) -> bool {
        self.access != WatchAccess::Change && self.unconditional()
    }
}

pub fn bytes(kernel: &Kernel, tgid: Tgid, intent: &Intent) -> Option<Vec<u8>> {
    kernel
        .processes
        .get(&tgid)?
        .space
        .peek_bytes(intent.address, usize::try_from(intent.size).ok()?)
}

pub struct Stop<'a> {
    pub kernel: &'a Kernel,
    pub tgid: Tgid,

    pub reasons: &'a BTreeMap<Tid, StopReason>,
    pub intents: &'a [Intent],

    pub baselines: &'a BTreeMap<u64, Vec<u8>>,

    pub last_counts: &'a BTreeMap<u64, u64>,

    pub counts: &'a BTreeMap<u64, u64>,
}

pub fn judge(stop: &Stop<'_>) -> Result<Vec<Mark>, String> {
    let kernel = stop.kernel;
    let mut marks = Vec::new();
    let mut reach = |mark| {
        if !marks.contains(&mark) {
            marks.push(mark);
        }
    };

    let mut stopped = BTreeMap::<u64, BTreeSet<u64>>::new();
    let accessors = |watch: u64| {
        kernel
            .watching
            .log
            .iter()
            .filter(|(_, watches)| watches.contains(&watch))
            .map(|(&tid, _)| tid)
            .collect::<BTreeSet<_>>()
    };

    for &tid in &kernel.watching.ran {
        if kernel
            .threads
            .get(&tid)
            .is_none_or(|thread| thread.tgid != stop.tgid)
        {
            continue;
        }

        let accessed = kernel.watching.log.get(&tid).cloned().unwrap_or_default();
        let reported = match stop.reasons.get(&tid) {
            Some(StopReason::Watchpoint { hits }) => hits.to_vec(),
            _ => Vec::new(),
        };

        for hit in &reported {
            let id = hit.watchpoint.get();
            let Some(intent) = stop.intents.iter().find(|intent| intent.id == id) else {
                continue;
            };

            if !accessed.contains(&id) {
                return Err(format!(
                    "thread {tid} reported a hit on watchpoint {id}, but made no access to it \
                     since the last stop"
                ));
            }

            if judge_hit(stop, tid, hit, intent)? {
                reach(Mark::WatchConditionHeld);
            }

            stopped.entry(id).or_default().insert(hit.hit_count);
            reach(Mark::WatchHit);
            if tid != stop.tgid.leader() {
                reach(Mark::WatchHitOnAnotherThread);
            }
        }

        for &id in &accessed {
            let Some(intent) = stop.intents.iter().find(|intent| intent.id == id) else {
                continue;
            };

            let hit = reported.iter().any(|hit| hit.watchpoint.get() == id);
            let changed = bytes(kernel, stop.tgid, intent) != stop.baselines.get(&id).cloned();
            let required = intent.unconditional()
                && match intent.access {
                    WatchAccess::Write | WatchAccess::ReadWrite | WatchAccess::Read => true,
                    WatchAccess::Change => changed && accessors(id) == BTreeSet::from([tid]),
                };
            if required && !hit {
                return Err(format!(
                    "thread {tid} accessed watchpoint {id} ({}), but the stop reports no hit \
                     for it: {:?}",
                    intent.access,
                    stop.reasons.get(&tid)
                ));
            }

            if kernel.watching.unchanged.contains(&id) {
                reach(Mark::UnchangedStore);
            }
        }
    }

    if judge_counts(stop, &stopped)? {
        reach(Mark::WatchHitDeclined);
    }

    Ok(marks)
}

fn judge_hit(
    stop: &Stop<'_>,
    tid: Tid,
    hit: &WatchpointHit,
    intent: &Intent,
) -> Result<bool, String> {
    let id = intent.id;
    let now = bytes(stop.kernel, stop.tgid, intent);
    let before = stop.baselines.get(&id).cloned();

    let held =
        |previous: Option<&[u8]>| {
            previous == before.as_deref()
                || !intent.unconditional()
                    && stop.kernel.watching.stored.get(&id).is_some_and(|stored| {
                        stored.iter().any(|bytes| Some(&bytes[..]) == previous)
                    })
        };
    if hit.current.as_deref() != now.as_deref() || !held(hit.previous.as_deref()) {
        return Err(format!(
            "thread {tid}'s hit on watchpoint {id} shows {:02x?} becoming {:02x?}; the bytes \
             were {before:02x?} at the last stop and are {now:02x?}",
            hit.previous, hit.current
        ));
    }

    if intent.access == WatchAccess::Change && hit.previous == hit.current {
        return Err(format!(
            "thread {tid} reported a change of watchpoint {id}, whose bytes are as they were, \
             {now:02x?}"
        ));
    }

    let (counted_before, counted) = (count_of(stop.last_counts, id), count_of(stop.counts, id));
    if !(counted_before + 1..=counted).contains(&hit.hit_count) {
        return Err(format!(
            "thread {tid} reported hit {} of watchpoint {id}, which counted hits {}..={counted} \
             since the last stop",
            hit.hit_count,
            counted_before + 1
        ));
    }

    if !intent
        .policies
        .iter()
        .any(|policy| policy.may_stop(hit.hit_count))
    {
        return Err(format!(
            "thread {tid} stopped at hit {} of watchpoint {id}, which none of {:?} lets stop",
            hit.hit_count, intent.policies
        ));
    }

    Ok(!intent.unconditional()
        && intent
            .policies
            .iter()
            .all(|policy| policy.condition != Known::Unknown && policy.must_stop(hit.hit_count)))
}

fn judge_counts(stop: &Stop<'_>, stopped: &BTreeMap<u64, BTreeSet<u64>>) -> Result<bool, String> {
    let mut declined = false;

    for intent in stop.intents {
        let id = intent.id;
        let (counted_before, counted) = (count_of(stop.last_counts, id), count_of(stop.counts, id));
        let accesses = count_of(&stop.kernel.watching.counts, id);
        let hits = counted.checked_sub(counted_before).ok_or_else(|| {
            format!("watchpoint {id}'s hit count went down from {counted_before} to {counted}")
        })?;
        let exact = intent.access != WatchAccess::Change;
        if hits > accesses || (exact && hits != accesses) {
            return Err(format!(
                "watchpoint {id} ({}) counted {hits} hits since the last stop, but threads made \
                 {accesses} accesses to it",
                intent.access
            ));
        }

        declined |= hits > stopped.get(&id).map_or(0, |hits| hits.len() as u64);
    }

    Ok(declined)
}

fn count_of(counts: &BTreeMap<u64, u64>, id: u64) -> u64 {
    counts.get(&id).copied().unwrap_or(0)
}

pub fn judge_disabled(
    disabled: &BTreeMap<u64, u64>,
    snapshot: &StateSnapshot,
) -> Result<(), String> {
    for watchpoint in snapshot.watchpoints.iter() {
        let Some(&counted) = disabled.get(&watchpoint.id.get()) else {
            continue;
        };

        if watchpoint.enabled || watchpoint.hit_count != counted {
            return Err(format!(
                "watchpoint {} was disabled with {counted} hits, but is {watchpoint:?}",
                watchpoint.id
            ));
        }
    }

    for thread in snapshot.threads.iter() {
        let ThreadState::Stopped {
            reason: Some(StopReason::Watchpoint { hits }),
        } = &thread.state
        else {
            continue;
        };

        if let Some(hit) = hits.iter().find(|hit| {
            disabled
                .get(&hit.watchpoint.get())
                .is_some_and(|&counted| hit.hit_count > counted)
        }) {
            return Err(format!(
                "disabled watchpoint {} reported {hit:?}",
                hit.watchpoint
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        AddressRange, InferiorState, ThreadId, ThreadSnapshot, VirtualAddress, WatchScope,
        Watchpoint, WatchpointId,
    };

    fn stop(enabled: bool, count: u64, reported: &[u64]) -> StateSnapshot {
        let hits = reported
            .iter()
            .map(|&hit_count| WatchpointHit {
                watchpoint: WatchpointId::new(1),
                thread: ThreadId::new(1000),
                hit_count,
                previous: None,
                current: None,
                previous_value: None,
                current_value: None,
            })
            .collect::<Arc<[_]>>();

        StateSnapshot {
            revision: 1,
            inferior: InferiorState::NotStarted,
            stop_id: None,
            selected: None,
            selected_frame: None,
            threads: Arc::from([ThreadSnapshot {
                id: ThreadId::new(1000),
                state: ThreadState::Stopped {
                    reason: Some(StopReason::Watchpoint { hits }),
                },
                name: None,
                activity: None,
                labels: Arc::from([]),
            }]),
            presentation: None,
            breakpoints: Arc::from([]),
            watchpoints: Arc::from([Watchpoint {
                id: WatchpointId::new(1),
                access: WatchAccess::Write,
                expression: None,
                address: VirtualAddress::new(0x1000),
                byte_size: 8,
                type_info: None,
                scope: WatchScope::Location,
                coverage: Arc::from([AddressRange {
                    start: VirtualAddress::new(0x1000),
                    end: VirtualAddress::new(0x1008),
                }]),
                hit_condition: None,
                condition: None,
                hit_count: count,
                enabled,
            }]),
            catchpoints: Arc::from([]),
        }
    }

    #[test]
    fn disabled_watchpoints_count_and_report_nothing() {
        let disabled = BTreeMap::from([(1, 3)]);
        assert_eq!(judge_disabled(&disabled, &stop(false, 3, &[3])), Ok(()));
        assert!(judge_disabled(&disabled, &stop(false, 4, &[])).is_err());
        assert!(judge_disabled(&disabled, &stop(false, 3, &[4])).is_err());
        assert!(judge_disabled(&disabled, &stop(true, 3, &[])).is_err());
        assert_eq!(
            judge_disabled(&BTreeMap::new(), &stop(true, 4, &[4])),
            Ok(())
        );
    }
}
