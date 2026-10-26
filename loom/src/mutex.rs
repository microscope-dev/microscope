//! A `Mutex` and `Condvar` for models, scheduled where loom's are not.
//!
//! Loom schedules before blocking or observing another thread, but not on
//! mutex unlock. A `try_lock` handoff can strand work arriving between a
//! worker's last check and its unlock; loom misses that gap and the bug.
//!
//! Loom's `Condvar` has no spurious wakes, so it also misses waits that
//! fail to recheck their condition in a loop.
//!
//! Models have no clock. Timed waits here yield to other threads, then
//! return immediately, modeling the earliest possible timeout.
//!
//! All model threads share one OS thread, so `std::thread::panicking`
//! holds in all of them while one unwinds. Another thread dropping a guard
//! then poisons loom's backing std mutex, whose lock result loom unwraps.
//!
//! This mutex instead uses a compare-exchange flag and a release store,
//! both loom scheduling points. Contenders wait for the next release;
//! spinning would produce too many schedules to explore. Only the flag's
//! holder locks the backing std mutex, ignoring poison. The condvar uses
//! the same wake mechanism.
//!
//! Each mutex or condvar keeps one `Notify` per waiting thread for as long
//! as it lives. Loom may wake it spuriously once; allocating one per wait
//! would allow endless spurious wakes.

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
