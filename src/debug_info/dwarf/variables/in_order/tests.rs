use std::time::Duration;

use super::*;

/// Adds walks to a list, holding until it is told to stop holding.
struct Log {
    added: Vec<Option<usize>>,

    /// The unit at which the walk apart stops holding.
    fails_at: Option<usize>,
}

impl Add<usize> for Log {
    fn add(&mut self, walk: Option<usize>) -> bool {
        self.added.push(walk);
        walk.is_some() && self.fails_at != Some(self.added.len() - 1)
    }
}

/// Units 0 through 6 in groups {0, 2}, {1}, {3, 4, 6}, {5}.
fn groups() -> (Vec<usize>, Vec<Vec<usize>>) {
    let mut members = vec![Vec::new(); 7];
    for (unit, group) in [0, 1, 0, 3, 3, 5, 3].into_iter().enumerate() {
        members[group].push(unit);
    }

    (vec![0, 1, 3, 5], members)
}

fn walk(_group: usize, units: &[usize]) -> Vec<(usize, Option<usize>)> {
    units.iter().map(|&unit| (unit, Some(unit))).collect()
}

fn run(workers: usize, ahead: usize, fails_at: Option<usize>) -> (usize, Log) {
    let (order, members) = groups();
    let log = Log {
        added: Vec::new(),
        fails_at,
    };

    let in_order = InOrder::new(&order, &members, log, ahead);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| in_order.work(walk));
        }
    });

    in_order.into_parts()
}

#[test]
fn every_unit_is_added_once_in_unit_order() {
    for workers in [1, 2, 4] {
        for ahead in [1, 2, 8] {
            let (added, log) = run(workers, ahead, None);
            assert_eq!(added, 7);
            assert_eq!(log.added, (0..7).map(Some).collect::<Vec<_>>());
        }
    }
}

#[test]
fn units_after_the_walk_stops_holding_are_added_unwalked() {
    for workers in [1, 2, 4] {
        let (added, log) = run(workers, 1, Some(2));
        assert_eq!(added, 7, "every unit is still added");
        assert_eq!(&log.added[..3], [Some(0), Some(1), Some(2)]);

        // Units in groups claimed before the walk stopped holding may
        // have been walked; none claimed after it was.
        assert_eq!(log.added[5], None, "group 5 begins only after unit 2");
    }
}

/// A worker whose walk panics leaves units no one adds. The others must
/// not wait for them forever: the panic ends the walk.
#[test]
fn a_panicking_walk_ends_the_walk() {
    let (ended, end) = std::sync::mpsc::channel();

    std::thread::spawn(move || {
        let (order, members) = groups();
        let log = Log {
            added: Vec::new(),
            fails_at: None,
        };

        let in_order = InOrder::new(&order, &members, log, 1);

        let walked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::thread::scope(|scope| {
                for _ in 0..2 {
                    scope.spawn(|| {
                        in_order.work(|group, units| {
                            assert_ne!(group, 0, "a walk that panics");
                            walk(group, units)
                        });
                    });
                }
            });
        }));

        let _ = ended.send(walked.is_err());
    });

    let panicked = end
        .recv_timeout(Duration::from_secs(10))
        .expect("the walk ends when a worker panics");
    assert!(panicked, "the panic reaches the caller");
}
