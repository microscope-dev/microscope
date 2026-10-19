//! A `Mutex` and `Condvar` for models, scheduled where loom's are not.
//!
//! Loom switches threads only before operations that can block or observe
//! another thread, such as taking a lock; it never does so when its
//! `Mutex` unlocks. A protocol that hands work off through `try_lock`
//! depends on what another thread does between one unlock and the next: a
//! worker that looks for more work, then lets go of the lock, strands
//! whatever arrived in between. Loom never runs a thread in that gap, so a
//! model of the protocol passes with the bug in it.
//!
//! Loom's `Condvar` never wakes a waiter that was not notified, as a real
//! one may, so a wait not rechecked in a loop passes too.
//!
//! A model has no clock, so a timed wait here returns at once, having let
//! the other threads run: the most a real one's time can run out.
//!
//! And loom runs every thread of a model on one OS thread, so while one
//! model thread unwinds a panic, `std::thread::panicking` holds in all of
//! them. A guard another thread drops meanwhile poisons the std mutex that
//! loom's `Mutex` keeps its data in, and loom unwraps it.
//!
//! So this `Mutex` is a flag taken by compare-exchange and released by a
//! store, at which loom schedules. A thread that finds it taken waits to be
//! woken by the next release; spinning instead would give loom more
//! schedules than it can explore. Its data sits in a std mutex that only
//! the flag's holder locks and whose poisoning it ignores. The `Condvar`
//! wakes its waiters the same way.
//!
//! Each waiting thread waits on a `Notify` of its own, which loom wakes
//! spuriously once in some schedules. It is the thread's for as long as
//! the mutex or condvar lives: a new one for each wait would let loom
//! choose a spurious wake every time, and never finish.

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, LockResult, PoisonError, TryLockError, TryLockResult};

use loom::sync::Notify;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::thread::ThreadId;

/// Threads waiting to be woken. Its lists are std mutexes, locked only
/// around steps that are no loom operation, so loom never switches threads
/// while one is held, and none is ever contended.
struct Waiters {
    /// Each thread's `Notify`, made the first time it waits.
    notifies: std::sync::Mutex<Vec<(ThreadId, Arc<Notify>)>>,

    /// Those waiting now.
    waiting: std::sync::Mutex<Vec<Arc<Notify>>>,
}

impl Waiters {
    fn new() -> Self {
        Self {
            notifies: std::sync::Mutex::new(Vec::new()),
            waiting: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Lists this thread as waiting, and returns what it waits on.
    fn enlist(&self) -> Arc<Notify> {
        let thread = loom::thread::current().id();
        let known = self
            .notifies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(waiter, _)| *waiter == thread)
            .map(|(_, notify)| Arc::clone(notify));
        let notify = known.unwrap_or_else(|| {
            let notify = Arc::new(Notify::new());
            self.notifies
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((thread, Arc::clone(&notify)));
            notify
        });

        let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
        if !waiting.iter().any(|listed| Arc::ptr_eq(listed, &notify)) {
            waiting.push(Arc::clone(&notify));
        }

        notify
    }

    /// Wakes the thread listed first.
    fn wake_one(&self) {
        let mut waiting = self.waiting.lock().unwrap_or_else(PoisonError::into_inner);
        let first = (!waiting.is_empty()).then(|| waiting.remove(0));
        drop(waiting);
        if let Some(notify) = first {
            notify.notify();
        }
    }

    fn wake_all(&self) {
        let waiting =
            std::mem::take(&mut *self.waiting.lock().unwrap_or_else(PoisonError::into_inner));
        for notify in waiting {
            notify.notify();
        }
    }
}

pub struct Mutex<T> {
    locked: AtomicBool,
    waiters: Waiters,

    /// Only the holder of `locked` locks it, so it never waits.
    data: std::sync::Mutex<T>,
}

pub struct MutexGuard<'a, T> {
    data: Option<std::sync::MutexGuard<'a, T>>,
    mutex: &'a Mutex<T>,
}

impl<T> Mutex<T> {
    pub fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            waiters: Waiters::new(),
            data: std::sync::Mutex::new(value),
        }
    }

    pub fn lock(&self) -> LockResult<MutexGuard<'_, T>> {
        // A thread is listed in the same step as its failed take, before
        // another can run and release, so no release is missed.
        while !self.take() {
            self.waiters.enlist().wait();
        }

        Ok(self.guard())
    }

    pub fn try_lock(&self) -> TryLockResult<MutexGuard<'_, T>> {
        if self.take() {
            Ok(self.guard())
        } else {
            Err(TryLockError::WouldBlock)
        }
    }

    pub fn into_inner(self) -> LockResult<T> {
        Ok(self
            .data
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner))
    }

    fn take(&self) -> bool {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }

    fn guard(&self) -> MutexGuard<'_, T> {
        MutexGuard {
            data: Some(self.data.lock().unwrap_or_else(PoisonError::into_inner)),
            mutex: self,
        }
    }
}

impl<T> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.data
            .as_deref()
            .expect("a guard holds its lock until dropped")
    }
}

impl<T> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.data
            .as_deref_mut()
            .expect("a guard holds its lock until dropped")
    }
}

impl<T> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        // The data before the flag, so the next holder finds it free.
        if self.data.take().is_some() {
            self.mutex.locked.store(false, Ordering::Release);
            self.mutex.waiters.wake_all();
        }
    }
}

pub struct Condvar {
    waiters: Waiters,
}

impl Condvar {
    pub fn new() -> Self {
        Self {
            waiters: Waiters::new(),
        }
    }

    /// Unlocks `guard`'s mutex until notified, or woken spuriously, and
    /// locks it again. The wait is listed before the unlock, so a notify
    /// that follows a change made under the lock is not missed.
    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> LockResult<MutexGuard<'a, T>> {
        let notify = self.waiters.enlist();
        let mutex = guard.mutex;
        drop(guard);
        notify.wait();
        mutex.lock()
    }

    /// Unlocks `guard`'s mutex for a while and locks it again. A model has
    /// no clock, so its time may run out at once: the thread lets the
    /// others run, notified or not, and goes on.
    pub fn wait_timeout<'a, T>(
        &self,
        guard: MutexGuard<'a, T>,
        _timeout: std::time::Duration,
    ) -> LockResult<(MutexGuard<'a, T>, bool)> {
        let mutex = guard.mutex;
        drop(guard);
        loom::thread::yield_now();
        Ok((mutex.lock().unwrap_or_else(PoisonError::into_inner), true))
    }

    pub fn notify_all(&self) {
        self.waiters.wake_all();
    }

    #[allow(
        dead_code,
        reason = "no protocol uses it; loom/mutants/notify-one-waiter.patch does"
    )]
    pub fn notify_one(&self) {
        self.waiters.wake_one();
    }
}
