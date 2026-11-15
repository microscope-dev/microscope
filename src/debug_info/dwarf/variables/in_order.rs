use std::collections::BTreeMap;
use std::sync::PoisonError;

use crate::sync::{AtomicBool, AtomicUsize, Condvar, Mutex, Ordering};

pub trait Add<W> {
    fn add(&mut self, walk: Option<W>) -> bool;
}

pub struct InOrder<'g, W, A> {
    order: &'g [usize],

    members: &'g [Vec<usize>],
    adding: Mutex<Adding<A>>,

    finished: Mutex<BTreeMap<usize, Option<W>>>,
    abandoned: AtomicBool,
    claimed: AtomicUsize,
    window: Window,
}

struct Adding<A> {
    next: usize,
    adder: A,
}

impl<'g, W, A: Add<W>> InOrder<'g, W, A> {
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

    fn abandon(&self) {
        self.abandoned.store(true, Ordering::Relaxed);

        drop(
            self.window
                .added
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        self.window.progress.notify_all();
    }

    pub fn into_parts(self) -> (usize, A) {
        let adding = self
            .adding
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        (adding.next, adding.adder)
    }

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

struct AbandonOnUnwind<'a, 'g, W, A: Add<W>>(&'a InOrder<'g, W, A>);

impl<W, A: Add<W>> Drop for AbandonOnUnwind<'_, '_, W, A> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.abandon();
        }
    }
}

struct Window {
    added: Mutex<usize>,
    progress: Condvar,
    ahead: usize,
}

impl Window {
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

    fn advance(&self, next: usize) {
        *self.added.lock().unwrap_or_else(PoisonError::into_inner) = next;
        self.progress.notify_all();
    }
}
