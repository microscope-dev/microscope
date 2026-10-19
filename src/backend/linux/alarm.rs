//! An alarm that wakes the controller at the earliest runtime agent's
//! deadline, so that it can block on its messages rather than poll them
//! while an acknowledgement is outstanding.
//!
//! The controller sets the alarm before it blocks, and a thread of the
//! alarm's own rings it once that time comes, by sending the controller a
//! message. A ring only wakes the controller, which then judges every
//! deadline by its own clock: one that was paused or met since is not
//! taken for a timeout. A sooner time replaces a later one, and a later
//! time a sooner one, which then rings for nothing.
//!
//! `loom/` checks that a ring is never lost: an alarm set for a time that
//! has come rings, however the set and the thread's wait interleave.

use std::sync::PoisonError;
use std::time::Duration;

use crate::sync::{Condvar, Mutex};

/// A point in time an alarm is set for.
pub trait Time: Copy + Ord {
    /// How long from `self` until `later`.
    fn until(self, later: Self) -> Duration;
}

impl Time for std::time::Instant {
    fn until(self, later: Self) -> Duration {
        later.saturating_duration_since(self)
    }
}

pub struct Alarm<T> {
    state: Mutex<State<T>>,
    changed: Condvar,
}

struct State<T> {
    /// When it rings next.
    at: Option<T>,
    stopped: bool,
}

impl<T: Time> Alarm<T> {
    #[cfg_attr(
        not(loom),
        expect(
            clippy::missing_const_for_fn,
            reason = "loom's Mutex::new is not const"
        )
    )]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                at: None,
                stopped: false,
            }),
            changed: Condvar::new(),
        }
    }

    /// Sets the alarm to ring at `at`, in place of the time it was set to.
    pub fn set(&self, at: T) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.at = Some(at);
        drop(state);
        self.changed.notify_all();
    }

    /// Stops the alarm's thread, which rings no more.
    pub fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.stopped = true;
        drop(state);
        self.changed.notify_all();
    }

    /// The alarm's thread: calls `ring` each time the time it is set to
    /// comes by `now`, until stopped.
    pub fn run(&self, mut now: impl FnMut() -> T, mut ring: impl FnMut()) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);

        while !state.stopped {
            let Some(at) = state.at else {
                state = self
                    .changed
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
                continue;
            };

            let current = now();
            if at <= current {
                state.at = None;
                drop(state);
                ring();
                state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                continue;
            }

            state = self
                .changed
                .wait_timeout(state, current.until(at))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }

        drop(state);
    }
}

/// An alarm and the thread that rings it.
#[cfg(not(loom))]
pub(super) struct AlarmThread {
    alarm: std::sync::Arc<Alarm<std::time::Instant>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(not(loom))]
impl AlarmThread {
    /// Starts an alarm whose ring calls `ring`.
    pub(super) fn spawn(ring: impl FnMut() + Send + 'static) -> std::io::Result<Self> {
        let alarm = std::sync::Arc::new(Alarm::new());
        let ringing = std::sync::Arc::clone(&alarm);
        let thread = std::thread::Builder::new()
            .name("microscope-alarm".into())
            .spawn(move || ringing.run(std::time::Instant::now, ring))?;

        Ok(Self {
            alarm,
            thread: Some(thread),
        })
    }

    pub(super) fn set(&self, at: std::time::Instant) {
        self.alarm.set(at);
    }
}

#[cfg(not(loom))]
impl Drop for AlarmThread {
    fn drop(&mut self) {
        self.alarm.stop();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::AlarmThread;

    /// An alarm rings once its time comes, a sooner time replacing a later
    /// one, and not again until it is set again.
    #[test]
    fn an_alarm_rings_at_the_time_it_was_last_set_to() {
        let (rang, rings) = mpsc::channel();
        let alarm = AlarmThread::spawn(move || {
            let _ = rang.send(Instant::now());
        })
        .expect("start the alarm");
        let start = Instant::now();
        alarm.set(start + Duration::from_secs(600));
        alarm.set(start + Duration::from_millis(20));
        let when = rings
            .recv_timeout(Duration::from_secs(10))
            .expect("the alarm rings");
        assert!(when >= start + Duration::from_millis(20), "{when:?}");
        assert!(rings.try_recv().is_err());
        alarm.set(Instant::now());
        rings
            .recv_timeout(Duration::from_secs(10))
            .expect("the alarm rings again");
    }
}
