//! Walking groups of units on several workers at once, and adding each
//! unit's walk in unit order.
//!
//! Workers claim groups in the order of their first units. A worker waits
//! to begin a group too far past the units added, so what waits in memory
//! stays bounded; the units the adding waits for are in groups claimed
//! before, so they are walked meanwhile. One worker at a time adds what is
//! ready, in unit order. One that finds another adding leaves its units to
//! that one, which looks again once it is done.
//!
//! The protocol uses only the types `crate::sync` names, so that `loom/`
//! can check it under every interleaving of a few workers.

use std::collections::BTreeMap;
use std::sync::PoisonError;

use crate::sync::{AtomicBool, AtomicUsize, Condvar, Mutex, Ordering};

/// What adds units' walks in unit order.
pub trait Add<W> {
    /// Adds the next unit's walk, or `None` for a unit not walked, and
    /// returns whether the walk apart still holds. Once it does not, the
    /// units left are not walked.
    fn add(&mut self, walk: Option<W>) -> bool;
}

/// Groups of units walked at once and added in unit order.
pub struct InOrder<'g, W, A> {
    /// The groups, named by their first units, in the order claimed.
    order: &'g [usize],

    /// Each group's units, by the group.
    members: &'g [Vec<usize>],
    adding: Mutex<Adding<A>>,

    /// Units walked but not yet added, by unit.
    finished: Mutex<BTreeMap<usize, Option<W>>>,
    abandoned: AtomicBool,
    claimed: AtomicUsize,
    window: Window,
}

struct Adding<A> {
    /// The next unit to add.
    next: usize,
    adder: A,
}

impl<'g, W, A: Add<W>> InOrder<'g, W, A> {
    /// Walks the groups in `order`, whose units `members` lists by group,
    /// adding to `adder`. A group's walk begins at most `ahead` units past
    /// the units added.
    pub fn new(order: &'g [usize], members: &'g [Vec<usize>], adder: A, ahead: usize) -> Self {
        debug_assert!(ahead > 0, "a walk window of no units waits forever");
        debug_assert!(
            order.iter().all(|&group| group < members.len()),
            "a group in the order is past the {} groups",
            members.len(),
        );
        Self {
            order,
            members,
            adding: Mutex::new(Adding { next: 0, adder }),
            finished: Mutex::new(BTreeMap::new()),
            abandoned: AtomicBool::new(false),
            claimed: AtomicUsize::new(0),
            window: Window {
                added: Mutex::new(0),
                progress: Condvar::new(),
                ahead,
            },
        }
    }

    /// One worker's share: walks each group it claims with `walk`, given
    /// the group and its units, until none is left, adding what is ready as
    /// it goes. Every worker runs it.
    pub fn work(&self, walk: impl Fn(usize, &[usize]) -> Vec<(usize, Option<W>)>) {
        let _unwinding = AbandonOnUnwind(self);
        while let Some(&group) = self.order.get(self.claimed.fetch_add(1, Ordering::Relaxed)) {
            self.window.enter(group, &self.abandoned);

            #[expect(
                clippy::indexing_slicing,
                reason = "`order` names groups `members` lists, which `Self::new` asserts"
            )]
            let units = &self.members[group];
            let walks = if self.abandoned.load(Ordering::Relaxed) {
                units.iter().map(|&unit| (unit, None)).collect()
            } else {
                walk(group, units)
            };

            self.finished
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(walks);
            self.add_ready();
        }
    }

    /// Stops walking the units left, and wakes every worker waiting to.
    fn abandon(&self) {
        self.abandoned.store(true, Ordering::Relaxed);

        // Under the lock, so that no worker sees the walk held and then
        // waits past the wake.
        drop(
            self.window
                .added
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        self.window.progress.notify_all();
    }

    /// The number of units added, and what they were added to.
    pub fn into_parts(self) -> (usize, A) {
        let adding = self
            .adding
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        (adding.next, adding.adder)
    }

    /// Adds the finished units that are next, in order, unless another
    /// worker is adding them: that one looks again once it is done.
    fn add_ready(&self) {
        while let Ok(mut adding) = self.adding.try_lock() {
            loop {
                let next = adding.next;
                let Some(walk) = self
                    .finished
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&next)
                else {
                    break;
                };

                adding.next += 1;
                if !adding.adder.add(walk) {
                    self.abandoned.store(true, Ordering::Relaxed);
                }
            }

            let next = adding.next;
            self.window.advance(next);
            drop(adding);
            if !self
                .finished
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key(&next)
            {
                break;
            }
        }
    }
}

/// Abandons the walk when a worker unwinds from a panic, which leaves units
/// that no one adds: the workers waiting for them then finish, and the
/// panic reaches whoever started them rather than every worker waiting.
struct AbandonOnUnwind<'a, 'g, W, A: Add<W>>(&'a InOrder<'g, W, A>);

impl<W, A: Add<W>> Drop for AbandonOnUnwind<'_, '_, W, A> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.abandon();
        }
    }
}

/// How far past the units added a group's walk may begin: what is walked
/// waits in memory until the units before it are added.
struct Window {
    added: Mutex<usize>,
    progress: Condvar,
    ahead: usize,
}

impl Window {
    /// Waits until `group`, its first unit, is within the window, or the
    /// walk is abandoned.
    fn enter(&self, group: usize, abandoned: &AtomicBool) {
        let mut added = self.added.lock().unwrap_or_else(PoisonError::into_inner);
        while group >= added.saturating_add(self.ahead) && !abandoned.load(Ordering::Relaxed) {
            added = self
                .progress
                .wait(added)
                .unwrap_or_else(PoisonError::into_inner);
        }

        drop(added);
    }

    /// Records that the units before `next` are added.
    fn advance(&self, next: usize) {
        *self.added.lock().unwrap_or_else(PoisonError::into_inner) = next;
        self.progress.notify_all();
    }
}

// loom/ builds this file under cfg(loom), where these tests have no place.
#[cfg(all(test, not(loom)))]
mod tests;
