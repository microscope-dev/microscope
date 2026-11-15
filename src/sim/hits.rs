use std::collections::{BTreeMap, BTreeSet};

use crate::{HitCondition, ProcessId, StateSnapshot, StopReason, ThreadState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Known {
    Absent,
    Holds,
    Fails,

    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub hit_condition: Option<HitCondition>,
    pub condition: Known,
    pub logs: bool,

    pub enabled: bool,
}

impl Policy {
    const fn holds(self) -> bool {
        matches!(self.condition, Known::Absent | Known::Holds)
    }

    fn allows(self, hit: u64) -> bool {
        self.enabled
            && self
                .hit_condition
                .is_none_or(|condition| condition.is_met(hit))
    }

    pub(super) fn must_stop(self, hit: u64) -> bool {
        self.allows(hit) && self.holds() && !self.logs
    }

    pub(super) fn may_stop(self, hit: u64) -> bool {
        self.allows(hit)
            && match self.condition {
                Known::Absent | Known::Holds => !self.logs,
                Known::Fails => false,
                Known::Unknown => true,
            }
    }

    pub(super) const fn unconditional(self) -> bool {
        self.enabled && self.hit_condition.is_none() && self.holds() && !self.logs
    }

    fn must_log(self, hit: u64) -> bool {
        self.allows(hit) && self.holds() && self.logs
    }

    fn may_log(self, hit: u64) -> bool {
        self.allows(hit) && self.condition != Known::Fails && self.logs
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Published {
    pub logged: BTreeMap<u64, u64>,
    pub condition_failures: BTreeMap<u64, u64>,

    pub stops: BTreeSet<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    pub process: ProcessId,
    pub counts: BTreeMap<u64, u64>,
    pub published: Published,
}

#[expect(clippy::struct_excessive_bools, reason = "each is a coverage mark")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Found {
    pub declined: bool,

    pub held: bool,

    pub logged: bool,

    pub temporary_stopped: bool,

    pub temporary_shared: bool,
}

fn stopping_hits(snapshot: &StateSnapshot) -> BTreeMap<u64, BTreeSet<u64>> {
    let mut stopping = BTreeMap::<u64, BTreeSet<u64>>::new();
    for thread in snapshot.threads.iter() {
        if let ThreadState::Stopped {
            reason: Some(StopReason::Breakpoint { hits, .. }),
        } = &thread.state
        {
            for hit in hits.iter() {
                stopping
                    .entry(hit.breakpoint.get())
                    .or_default()
                    .insert(hit.hit_count);
            }
        }
    }

    stopping
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub versions: Vec<Policy>,
    pub temporary: bool,
}

pub fn judge(
    baseline: Option<&Baseline>,
    process: ProcessId,
    snapshot: &StateSnapshot,
    policies: &BTreeMap<u64, Kept>,
    published: &Published,
) -> Result<Found, String> {
    let seen_all = published.stops.iter().all(|&stop| {
        snapshot.stop_id.is_some_and(|seen| seen.get() == stop)
            || baseline.is_some_and(|baseline| baseline.published.stops.contains(&stop))
    });

    let baseline = baseline.filter(|baseline| baseline.process == process);
    let stopping = stopping_hits(snapshot);
    let complete = baseline.is_some();
    let mut found = Found::default();
    let count_before = |id| {
        baseline
            .and_then(|baseline| baseline.counts.get(&id))
            .copied()
            .unwrap_or(0)
    };

    let stopped_since = |id| {
        stopping
            .get(&id)
            .map(|hits| {
                hits.range(count_before(id) + 1..)
                    .copied()
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default()
    };

    for (&id, kept) in policies {
        if snapshot
            .breakpoints
            .iter()
            .any(|breakpoint| breakpoint.id.get() == id)
        {
            continue;
        }

        let stopped = stopped_since(id);
        if stopped.is_empty() && !seen_all && kept.temporary {
            continue;
        }

        judge_vanished(id, kept, &stopped, &mut found)?;
    }

    for breakpoint in snapshot.breakpoints.iter() {
        let id = breakpoint.id.get();
        let Some(kept) = policies.get(&id).filter(|kept| !kept.versions.is_empty()) else {
            continue;
        };

        let versions = &kept.versions;
        let before = count_before(id);
        let now = breakpoint.hit_count;

        if now < before {
            return Err(format!(
                "breakpoint {id}'s hit count went down from {before} to {now}"
            ));
        }

        if now > before && versions.iter().all(|policy| !policy.enabled) {
            return Err(format!(
                "breakpoint {id} counted hits {}..={now} while disabled",
                before + 1
            ));
        }

        let stopped = stopped_since(id);
        if kept.temporary && !stopped.is_empty() {
            return Err(format!(
                "temporary breakpoint {id} stopped at hits {stopped:?}, but outlived the stop"
            ));
        }

        for &hit in &stopped {
            if hit > now {
                return Err(format!(
                    "breakpoint {id} stopped at hit {hit}, but counts {now} hits"
                ));
            }

            if !versions.iter().any(|policy| policy.may_stop(hit)) {
                return Err(format!(
                    "breakpoint {id} stopped at hit {hit}, which none of {versions:?} lets stop"
                ));
            }

            found.held |= versions
                .iter()
                .all(|policy| policy.condition == Known::Holds);
        }

        for hit in before + 1..=now {
            if stopped.contains(&hit) {
                continue;
            }

            found.declined = true;
            if versions.iter().all(|policy| policy.must_stop(hit)) {
                return Err(format!(
                    "hit {hit} of breakpoint {id} had to stop under {versions:?}, but did not"
                ));
            }
        }

        if complete {
            let earlier = baseline.map(|baseline| &baseline.published);
            found.logged |= judge_published(id, versions, before, now, published, earlier)?;
        }
    }

    Ok(found)
}

fn judge_vanished(
    id: u64,
    kept: &Kept,
    stopped: &BTreeSet<u64>,
    found: &mut Found,
) -> Result<(), String> {
    if !kept.temporary || stopped.is_empty() {
        return Err(format!(
            "breakpoint {id} is gone, though the client never removed it{}",
            if kept.temporary {
                " and no stop of it deleted it"
            } else {
                ""
            }
        ));
    }

    if let Some(hit) = stopped
        .iter()
        .find(|&&hit| !kept.versions.iter().any(|policy| policy.may_stop(hit)))
    {
        return Err(format!(
            "temporary breakpoint {id} stopped at hit {hit}, which none of {:?} lets stop",
            kept.versions
        ));
    }

    found.temporary_stopped = true;
    found.temporary_shared |= stopped.len() > 1;
    Ok(())
}

fn judge_published(
    id: u64,
    versions: &[Policy],
    before: u64,
    now: u64,
    published: &Published,
    before_published: Option<&Published>,
) -> Result<bool, String> {
    let since = |counts: &BTreeMap<u64, u64>, before: Option<&BTreeMap<u64, u64>>| {
        counts.get(&id).copied().unwrap_or(0)
            - before.and_then(|b| b.get(&id)).copied().unwrap_or(0)
    };

    let logged = since(
        &published.logged,
        before_published.map(|published| &published.logged),
    );

    let must = (before + 1..=now)
        .filter(|&hit| versions.iter().all(|policy| policy.must_log(hit)))
        .count() as u64;
    let may = (before + 1..=now)
        .filter(|&hit| versions.iter().any(|policy| policy.may_log(hit)))
        .count() as u64;

    if !(must..=may).contains(&logged) {
        return Err(format!(
            "breakpoint {id} logged {logged} messages for hits {}..={now}, which must log \
             {must} and may log {may} under {versions:?}",
            before + 1
        ));
    }

    let failed = since(
        &published.condition_failures,
        before_published.map(|published| &published.condition_failures),
    );
    if failed > 0
        && versions
            .iter()
            .all(|policy| policy.condition != Known::Unknown)
    {
        return Err(format!(
            "breakpoint {id}'s condition failed to evaluate {failed} times, though its \
             value was known under {versions:?}"
        ));
    }

    Ok(logged > 0)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{
        Breakpoint, BreakpointHit, BreakpointId, BreakpointSpec, HitComparison, InferiorState,
        ThreadId, ThreadSnapshot, VirtualAddress,
    };

    const PROCESS: ProcessId = ProcessId::new(1000);

    fn stop(count: u64, stopped: &[u64]) -> StateSnapshot {
        let hits = stopped
            .iter()
            .map(|&hit_count| BreakpointHit {
                breakpoint: BreakpointId::new(1),
                hit_count,
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
                    reason: (!hits.is_empty()).then(|| StopReason::Breakpoint {
                        address: Some(VirtualAddress::new(0x1000)),
                        hits,
                    }),
                },
                name: None,
                activity: None,
                labels: Arc::from([]),
            }]),
            presentation: None,
            breakpoints: Arc::from([Breakpoint {
                pending_locations: Arc::from([]),
                id: BreakpointId::new(1),
                spec: BreakpointSpec::Function("f".into()),
                locations: Arc::from([]),
                hit_condition: None,
                condition: None,
                log_message: None,
                hit_count: count,
                enabled: true,
                temporary: false,
            }]),
            watchpoints: Arc::from([]),
            catchpoints: Arc::from([]),
        }
    }

    fn baseline(count: u64, logged: u64) -> Baseline {
        Baseline {
            process: PROCESS,
            counts: BTreeMap::from([(1, count)]),
            published: Published {
                logged: BTreeMap::from([(1, logged)]),
                ..Published::default()
            },
        }
    }

    fn published(logged: u64) -> Published {
        Published {
            logged: BTreeMap::from([(1, logged)]),
            ..Published::default()
        }
    }

    fn policy(hit_condition: Option<HitCondition>, condition: Known, logs: bool) -> Policy {
        Policy {
            hit_condition,
            condition,
            logs,
            enabled: true,
        }
    }

    fn kept(versions: Vec<Policy>) -> BTreeMap<u64, Kept> {
        BTreeMap::from([(
            1,
            Kept {
                versions,
                temporary: false,
            },
        )])
    }

    #[test]
    fn hits_stop_and_log_as_their_policies_say() {
        let at_least_three = HitCondition::new(HitComparison::GreaterOrEqual, 3).ok();
        let judge_with = |versions: Vec<Policy>, before: Baseline, now: StateSnapshot, logs| {
            judge(
                Some(&before),
                PROCESS,
                &now,
                &kept(versions),
                &published(logs),
            )
        };

        let counted = policy(at_least_three, Known::Absent, false);

        assert!(judge_with(vec![counted], baseline(1, 0), stop(3, &[3]), 0).is_ok());

        assert!(judge_with(vec![counted], baseline(1, 0), stop(3, &[]), 0).is_err());

        assert!(judge_with(vec![counted], baseline(1, 0), stop(3, &[2, 3]), 0).is_err());

        assert!(judge_with(vec![counted], baseline(3, 0), stop(3, &[3]), 0).is_ok());

        let never = policy(None, Known::Fails, false);
        assert!(judge_with(vec![never], baseline(0, 0), stop(1, &[1]), 0).is_err());
        let unknown = policy(None, Known::Unknown, false);
        assert!(judge_with(vec![unknown], baseline(0, 0), stop(1, &[]), 0).is_ok());

        let always = policy(None, Known::Holds, false);
        assert!(judge_with(vec![never, always], baseline(0, 0), stop(1, &[1]), 0).is_ok());
        assert!(judge_with(vec![never, always], baseline(0, 0), stop(1, &[]), 0).is_ok());

        let logging = policy(None, Known::Holds, true);
        assert!(judge_with(vec![logging], baseline(0, 4), stop(2, &[]), 6).is_ok());
        assert!(judge_with(vec![logging], baseline(0, 4), stop(2, &[]), 5).is_err());
        assert!(judge_with(vec![logging], baseline(0, 4), stop(2, &[2]), 6).is_err());

        assert!(judge_with(vec![always], baseline(2, 0), stop(1, &[]), 0).is_err());
    }

    #[test]
    fn disabled_breakpoints_count_nothing() {
        let judge_with = |versions: Vec<Policy>, now: StateSnapshot| {
            judge(
                Some(&baseline(1, 0)),
                PROCESS,
                &now,
                &kept(versions),
                &published(0),
            )
        };

        let enabled = policy(None, Known::Absent, false);
        let disabled = Policy {
            enabled: false,
            ..enabled
        };

        assert!(judge_with(vec![disabled], stop(1, &[])).is_ok());
        assert!(judge_with(vec![disabled], stop(2, &[])).is_err());
        assert!(judge_with(vec![disabled], stop(2, &[2])).is_err());
        assert!(judge_with(vec![enabled, disabled], stop(2, &[])).is_ok());
        assert!(judge_with(vec![disabled, enabled], stop(2, &[2])).is_ok());
    }

    #[test]
    fn temporary_breakpoints_are_gone_after_their_stop() {
        let judge_after = |temporary, now: StateSnapshot, published: &Published| {
            judge(
                Some(&baseline(1, 0)),
                PROCESS,
                &now,
                &BTreeMap::from([(
                    1,
                    Kept {
                        versions: vec![policy(None, Known::Absent, false)],
                        temporary,
                    },
                )]),
                published,
            )
        };

        let judge_with = |temporary, now| judge_after(temporary, now, &published(0));
        let gone = |stopped: &[u64]| StateSnapshot {
            breakpoints: Arc::from([]),
            ..stop(0, stopped)
        };

        let found = judge_with(true, gone(&[2])).expect("deleted by its stop");
        assert!(found.temporary_stopped);
        assert!(judge_with(true, stop(2, &[2])).is_err());
        assert!(judge_with(true, gone(&[])).is_err());
        assert!(judge_with(false, gone(&[2])).is_err());

        let unseen = Published {
            stops: BTreeSet::from([2]),
            ..published(0)
        };

        let found = judge_after(true, gone(&[]), &unseen).expect("an unseen stop");
        assert!(!found.temporary_stopped);
        assert!(judge_after(false, gone(&[]), &unseen).is_err());

        assert!(judge_with(true, gone(&[1])).is_err());
    }
}
