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

    proptest! {
        #[test]
        fn identity_resolution_is_a_proven_bijection(pairs in proptest::collection::vec((1u64..=12, 1u64..=8), 0..32)) {
            let hosts = (1..=8).map(ThreadId::new).collect();
            let ids = ThreadIds::read(hosts, |_| pairs.iter().map(|&(host, target)| (ThreadId::new(host), TargetThreadId::new(target).unwrap())).collect());
            for native in 1..=8 {
                let matching: BTreeSet<_> = pairs.iter().filter(|(host, target)| *host <= 8 && *target == native).map(|(host, _)| *host).collect();
                let expected = if matching.len() == 1 {
                    let host = *matching.first().unwrap();
                    let targets: BTreeSet<_> = pairs.iter().filter(|(candidate, _)| *candidate == host).map(|(_, target)| *target).collect();
                    (targets.len() == 1).then_some(ThreadId::new(host))
                } else { None };
                prop_assert_eq!(ids.as_ref().ok().and_then(|ids| ids.resolve(native).ok()), expected, "native {} in {:?}", native, pairs);
            }
        }
    }
}
