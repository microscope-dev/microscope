//! Proven host/target thread identities shared by runtime readers and
//! live profiler metadata. Ambiguity is never resolved by list order.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{RuntimeStop, TargetThreadId};
use crate::ThreadId;

type Problem = Arc<str>;

/// Bound the host catalog independently of the interpreter's thread list.
const MAX_MAPPED_THREADS: usize = 1 << 14;

/// Proven target-namespace identities, with ambiguity retained rather than
/// assigning an interpreter frame to whichever host thread was seen last.
#[derive(Debug)]
pub struct ThreadIds(BTreeMap<TargetThreadId, Option<ThreadId>>);

/// The unresolved identity itself, formatted only when a caller keeps a
/// diagnostic. Large live catalogs need not allocate a string per failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("the target native thread id is zero")]
    Zero,
    #[error("target native thread {} has an ambiguous debugger-thread mapping", .0.get())]
    Ambiguous(TargetThreadId),
    #[error("target native thread {} has no proven matching debugger thread in its namespace", .0.get())]
    Missing(TargetThreadId),
}

impl ThreadIds {
    pub fn new(stop: &dyn RuntimeStop) -> Result<Self, Problem> {
        Self::read(stop.threads(), |threads| stop.target_thread_ids(threads))
    }

    /// Reads only after bounding and deduplicating the host catalog.
    pub fn read(
        mut threads: Vec<ThreadId>,
        read: impl FnOnce(&[ThreadId]) -> Vec<(ThreadId, TargetThreadId)>,
    ) -> Result<Self, Problem> {
        if threads.len() > MAX_MAPPED_THREADS {
            return Err(format!(
                "mapping target native ids exceeds {MAX_MAPPED_THREADS} host threads"
            )
            .into());
        }
        threads.sort_unstable();
        threads.dedup();
        let mut ids = BTreeMap::new();
        let mut targets = BTreeMap::new();

        for (thread, target) in read(&threads) {
            if threads.binary_search(&thread).is_ok() {
                targets
                    .entry(thread)
                    .and_modify(|known| {
                        if *known != Some(target) {
                            *known = None;
                        }
                    })
                    .or_insert(Some(target));
                ids.entry(target)
                    .and_modify(|known| {
                        if *known != Some(thread) {
                            *known = None;
                        }
                    })
                    .or_insert(Some(thread));
            }
        }

        if !threads.is_empty() && ids.is_empty() {
            return Err(
                "the platform supplied no proven target-namespace thread identity evidence".into(),
            );
        }

        for (target, host) in &mut ids {
            if host.is_some_and(|host| targets.get(&host) != Some(&Some(*target))) {
                *host = None;
            }
        }

        Ok(Self(ids))
    }

    pub fn lookup(&self, native: u64) -> Result<ThreadId, IdentityError> {
        let target = TargetThreadId::new(native).ok_or(IdentityError::Zero)?;
        match self.0.get(&target) {
            Some(Some(thread)) => Ok(*thread),
            Some(None) => Err(IdentityError::Ambiguous(target)),
            None => Err(IdentityError::Missing(target)),
        }
    }

    pub fn resolve(&self, native: u64) -> Result<ThreadId, Problem> {
        self.lookup(native)
            .map_err(|problem| problem.to_string().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    #[test]
    fn an_oversized_catalog_does_not_read_target_identity_fields() {
        let called = std::cell::Cell::new(false);
        let threads = (1..=MAX_MAPPED_THREADS as u64 + 1)
            .map(ThreadId::new)
            .collect();

        let ids = ThreadIds::read(threads, |_| {
            called.set(true);
            Vec::new()
        });
        assert!(ids.is_err());
        assert!(!called.get());
    }

    #[test]
    fn missing_platform_evidence_is_one_global_problem() {
        let ids = ThreadIds::read(vec![ThreadId::new(7)], |_| Vec::new());
        assert!(ids.unwrap_err().contains("platform supplied no proven"));
    }

    /// Every native id resolves to the one listed host that alone claims
    /// it and claims nothing else, is ambiguous where any other listed
    /// host or id is involved, and is missing where no listed host claims
    /// it. Claims by hosts not listed count for nothing, and a list of
    /// hosts none of which claims anything is no evidence at all.
    fn resolution_is_a_proven_bijection(
        hosts: &[u64],
        pairs: &[(u64, u64)],
    ) -> Result<(), TestCaseError> {
        let ids = ThreadIds::read(hosts.iter().copied().map(ThreadId::new).collect(), |_| {
            pairs
                .iter()
                .map(|&(host, target)| (ThreadId::new(host), TargetThreadId::new(target).unwrap()))
                .collect()
        });

        let listed: Vec<_> = pairs
            .iter()
            .filter(|(host, _)| hosts.contains(host))
            .collect();
        if !hosts.is_empty() && listed.is_empty() {
            prop_assert!(ids.is_err(), "{:?} in {:?}", hosts, pairs);
            return Ok(());
        }

        let ids = ids.map_err(|problem| TestCaseError::fail(problem.to_string()))?;
        prop_assert_eq!(ids.lookup(0), Err(IdentityError::Zero));
        for native in 1..=8 {
            let target = TargetThreadId::new(native).unwrap();
            let claimants: BTreeSet<_> = listed
                .iter()
                .filter(|(_, claimed)| *claimed == native)
                .map(|(host, _)| *host)
                .collect();
            let expected = match claimants.first() {
                None => Err(IdentityError::Missing(target)),
                Some(&host) => {
                    let claimed: BTreeSet<_> = listed
                        .iter()
                        .filter(|(claimant, _)| *claimant == host)
                        .map(|(_, claimed)| *claimed)
                        .collect();
                    if claimants.len() == 1 && claimed.len() == 1 {
                        Ok(ThreadId::new(host))
                    } else {
                        Err(IdentityError::Ambiguous(target))
                    }
                }
            };
            prop_assert_eq!(
                ids.lookup(native),
                expected,
                "native {} for hosts {:?} in {:?}",
                native,
                hosts,
                pairs
            );
        }
        Ok(())
    }

    /// Found by the property below.
    #[test]
    fn a_host_claiming_two_target_ids_resolves_neither() {
        resolution_is_a_proven_bijection(&[1, 2, 3, 4, 5, 6, 7, 8], &[(1, 1), (1, 2)]).unwrap();
    }

    proptest! {
        #[test]
        fn identity_resolution_is_a_proven_bijection(
            hosts in proptest::collection::vec(1u64..=12, 0..10),
            pairs in proptest::collection::vec((1u64..=12, 1u64..=8), 0..32),
        ) {
            resolution_is_a_proven_bijection(&hosts, &pairs)?;
        }
    }
}
