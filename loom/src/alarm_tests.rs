use std::time::Duration;

use loom::sync::atomic::{AtomicBool, Ordering};
use loom::sync::{Arc, Notify};
use loom::thread;

use crate::alarm::{Alarm, Time};

impl Time for u32 {
    fn until(self, later: Self) -> Duration {
        Duration::from_millis(u64::from(later.saturating_sub(self)))
    }
}

const NOW: u32 = 10;

struct Running {
    alarm: Arc<Alarm<u32>>,
    rang: Arc<AtomicBool>,
    heard: Arc<Notify>,
    thread: thread::JoinHandle<()>,
}

impl Running {
    fn start() -> Self {
        let alarm = Arc::new(Alarm::new());
        let rang = Arc::new(AtomicBool::new(false));
        let heard = Arc::new(Notify::new());

        let thread = {
            let (alarm, rang, heard) = (Arc::clone(&alarm), Arc::clone(&rang), Arc::clone(&heard));
            thread::spawn(move || {
                alarm.run(
                    || NOW,
                    || {
                        rang.store(true, Ordering::SeqCst);
                        heard.notify();
                    },
                );
            })
        };

        Self {
            alarm,
            rang,
            heard,
            thread,
        }
    }

    fn hear(&self) {
        while !self.rang.load(Ordering::SeqCst) {
            self.heard.wait();
        }
    }

    fn stop(self) {
        self.alarm.stop();
        self.thread.join().expect("the alarm's thread ends");
    }
}

#[test]
fn an_alarm_whose_time_has_come_rings() {
    loom::model(|| {
        let running = Running::start();
        running.alarm.set(NOW - 5);
        running.hear();
        running.stop();
    });
}

#[test]
fn a_sooner_time_replaces_a_later_one() {
    loom::model(|| {
        let running = Running::start();
        running.alarm.set(NOW + 10);
        running.alarm.set(NOW);
        running.hear();
        running.stop();
    });
}

#[test]
fn a_stopped_alarm_ends_its_thread() {
    loom::model(|| {
        let running = Running::start();
        running.alarm.set(NOW + 10);
        running.stop();
    });
}
