//! Per-key serialization gate used to prevent duplicate concurrent loads of the
//! same binary before the first load populates the cache.

use std::collections::HashSet;
use std::hash::Hash;
use std::sync::PoisonError;

use crate::sync::{Condvar, Mutex};

pub struct OneAtATime<K> {
    running: Mutex<HashSet<K>>,
    finished: Condvar,
}

impl<K> std::fmt::Debug for OneAtATime<K> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("OneAtATime").finish_non_exhaustive()
    }
}

impl<K: Eq + Hash + Clone> OneAtATime<K> {
    pub fn new() -> Self {
        Self {
            running: Mutex::new(HashSet::new()),
            finished: Condvar::new(),
        }
    }

    pub fn run<T>(&self, key: K, work: impl FnOnce() -> T) -> T {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        while running.contains(&key) {
            running = self
                .finished
                .wait(running)
                .unwrap_or_else(PoisonError::into_inner);
        }

        running.insert(key.clone());
        drop(running);

        let _done = Done { runs: self, key };
        work()
    }
}

struct Done<'a, K: Eq + Hash> {
    runs: &'a OneAtATime<K>,
    key: K,
}

impl<K: Eq + Hash> Drop for Done<'_, K> {
    fn drop(&mut self) {
        let removed = self
            .runs
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.key);
        debug_assert!(removed, "a finished run held its key");
        self.runs.finished.notify_all();
    }
}
