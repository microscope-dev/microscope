use std::ops::{Deref, DerefMut};
use std::sync::{Arc, LockResult, PoisonError, TryLockError, TryLockResult};

use loom::sync::Notify;
use loom::sync::atomic::{AtomicBool, Ordering};
use loom::thread::ThreadId;

struct Waiters {
    notifies: std::sync::Mutex<Vec<(ThreadId, Arc<Notify>)>>,

    waiting: std::sync::Mutex<Vec<Arc<Notify>>>,
}

impl Waiters {
    fn new() -> Self {
        Self {
            notifies: std::sync::Mutex::new(Vec::new()),
            waiting: std::sync::Mutex::new(Vec::new()),
        }
    }

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

    pub fn wait<'a, T>(&self, guard: MutexGuard<'a, T>) -> LockResult<MutexGuard<'a, T>> {
        let notify = self.waiters.enlist();
        let mutex = guard.mutex;
        drop(guard);
        notify.wait();
        mutex.lock()
    }

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
