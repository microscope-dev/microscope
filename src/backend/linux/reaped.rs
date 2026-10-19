//! The exit statuses of children whose owners wait for them, which another
//! thread may reap first. Any thread of a process may wait for any of its
//! children, and a debugger in this process waits for any child, as a
//! tracee may be one it has not met yet: a recording's task snapshot
//! attaches to the program the recording started.
//!
//! Two rules keep both sides whole. Each wait in this process leaves the
//! status of a watched child here, so that an owner who finds its child
//! reaped takes the status from here. And an owner reaps no child that a
//! debugger in this process holds, as a stopped snapshot holds the program
//! while it may trace it: a tracee its tracer did not see exit would leave
//! the tracer waiting for it.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often an owner looks for a status another thread is leaving, or
/// for the holds on its child to end.
const LOOK_EVERY: Duration = Duration::from_millis(1);

/// One watched child.
#[derive(Debug)]
struct Watched {
    pid: u32,

    /// Its wait status, once another thread has reaped it.
    status: Option<i32>,

    /// How many holds keep its owner from reaping it.
    holds: u32,
}

/// The watched children.
pub struct Reaped {
    watched: Mutex<Vec<Watched>>,
}

/// Every watched child of this process.
pub static REAPED: Reaped = Reaped::new();

/// A hold on a watched child, which its owner does not reap until every
/// hold ends.
#[must_use = "the hold ends when it is dropped"]
pub struct Hold<'a> {
    reaped: &'a Reaped,
    pid: u32,
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        if let Some(watched) = self
            .reaped
            .lock()
            .iter_mut()
            .find(|known| known.pid == self.pid)
        {
            debug_assert!(watched.holds > 0, "child {} was released unheld", self.pid);
            watched.holds = watched.holds.saturating_sub(1);
        }
    }
}

impl Reaped {
    pub const fn new() -> Self {
        Self {
            watched: Mutex::new(Vec::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Watched>> {
        self.watched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Watches child `pid`, which must not yet have exited.
    pub fn watch(&self, pid: u32) {
        let mut watched = self.lock();
        debug_assert!(
            watched.iter().all(|known| known.pid != pid),
            "child {pid} is watched twice"
        );
        watched.push(Watched {
            pid,
            status: None,
            holds: 0,
        });
    }

    /// Holds `pid`, if it is watched, until the hold is dropped.
    pub fn hold(&self, pid: u32) -> Hold<'_> {
        if let Some(watched) = self.lock().iter_mut().find(|known| known.pid == pid) {
            watched.holds += 1;
        }

        Hold { reaped: self, pid }
    }

    /// Waits until no hold keeps `pid`'s owner from reaping it.
    pub fn wait_unheld(&self, pid: u32) {
        while self
            .lock()
            .iter()
            .any(|known| known.pid == pid && known.holds > 0)
        {
            std::thread::sleep(LOOK_EVERY);
        }
    }

    /// Leaves the wait status `status` of `pid`, which the caller reaped,
    /// for its owner, if it is watched.
    pub fn leave(&self, pid: u32, status: i32) {
        if let Some(watched) = self.lock().iter_mut().find(|known| known.pid == pid) {
            watched.status = Some(status);
        }
    }

    /// Stops watching `pid`, whose owner reaped it.
    pub fn forget(&self, pid: u32) {
        self.lock().retain(|known| known.pid != pid);
    }

    /// Takes the wait status of `pid`, which another thread reaped, and
    /// stops watching it: `None` if none is left within `patience`, as
    /// when a wait outside this process's took it.
    pub fn take(&self, pid: u32, patience: Duration) -> Option<i32> {
        let deadline = Instant::now() + patience;

        loop {
            let late = Instant::now() >= deadline;
            let mut watched = self.lock();
            let position = watched.iter().position(|known| known.pid == pid);
            let status = position.and_then(|position| watched[position].status);
            if status.is_some() || late {
                if let Some(position) = position {
                    watched.swap_remove(position);
                }

                drop(watched);
                return status;
            }

            drop(watched);
            std::thread::sleep(LOOK_EVERY);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::Reaped;

    /// A watched child's status, left by the thread that reaped it, is its
    /// owner's to take once, while an unwatched child's is no one's.
    #[test]
    fn a_status_left_for_a_watched_child_is_taken_once() {
        let reaped = Reaped::new();
        reaped.watch(7);
        reaped.leave(8, 0);

        let left = std::thread::scope(|scope| {
            let owner = scope.spawn(|| reaped.take(7, Duration::from_secs(20)));
            std::thread::sleep(Duration::from_millis(5));
            reaped.leave(7, 0x0100);
            owner.join().expect("the owner")
        });

        assert_eq!(left, Some(0x0100));
        assert_eq!(reaped.take(7, Duration::ZERO), None);
        assert_eq!(reaped.take(8, Duration::ZERO), None);

        reaped.watch(9);
        reaped.forget(9);
        reaped.leave(9, 0);
        assert_eq!(reaped.take(9, Duration::ZERO), None);
    }

    /// An owner waits to reap its child until every hold on it ends; a
    /// hold on a child no one watches holds nothing.
    #[test]
    fn an_owner_waits_out_every_hold() {
        let reaped = Reaped::new();
        reaped.watch(7);
        let first = reaped.hold(7);
        let second = reaped.hold(7);
        drop(reaped.hold(8));

        let (released, last) = std::thread::scope(|scope| {
            let owner = scope.spawn(|| {
                reaped.wait_unheld(7);
                Instant::now()
            });
            std::thread::sleep(Duration::from_millis(5));
            drop(first);
            std::thread::sleep(Duration::from_millis(5));
            let last = Instant::now();
            drop(second);
            (owner.join().expect("the owner"), last)
        });

        assert!(released >= last, "the owner reaped a held child");
        reaped.wait_unheld(8);
    }
}
