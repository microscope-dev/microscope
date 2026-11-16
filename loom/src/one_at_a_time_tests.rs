use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Notify};
use loom::thread;

use crate::one_at_a_time::OneAtATime;

#[test]
fn runs_of_one_key_take_turns_and_see_each_others_work() {
    loom::model(|| {
        let runs = Arc::new(OneAtATime::new());
        let count = Arc::new(AtomicUsize::new(0));

        let other = {
            let (runs, count) = (Arc::clone(&runs), Arc::clone(&count));
            thread::spawn(move || {
                runs.run(1, || {
                    let seen = count.load(Ordering::Relaxed);
                    count.store(seen + 1, Ordering::Relaxed);
                });
            })
        };

        runs.run(1, || {
            let seen = count.load(Ordering::Relaxed);
            count.store(seen + 1, Ordering::Relaxed);
        });
        other.join().expect("the other run");

        assert_eq!(count.load(Ordering::Relaxed), 2);
    });
}

#[test]
fn a_run_of_another_key_does_not_wait() {
    loom::model(|| {
        let runs = Arc::new(OneAtATime::new());
        let finished = Arc::new(Notify::new());

        let other = {
            let (runs, finished) = (Arc::clone(&runs), Arc::clone(&finished));
            thread::spawn(move || runs.run(2, || finished.notify()))
        };

        runs.run(1, || finished.wait());
        other.join().expect("the other run");
    });
}
