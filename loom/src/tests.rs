//! Models of the unit-order walk. Each runs a [`Case`]: units in groups,
//! a window, two or three workers, and perhaps a unit at which the walk
//! stops holding or a group whose walk panics. Every schedule must add
//! exactly what [`check`] says, and every walk must begin inside the
//! window.
//!
//! The model's own bookkeeping, how many units are added, uses std's
//! atomics, which loom does not see. Loom runs one thread at a time, so
//! they read as plain memory would.

use std::sync::atomic::{AtomicUsize, Ordering};

use loom::sync::Arc;
use loom::thread;

use crate::in_order::{Add, InOrder};

#[derive(Clone, Copy, Debug)]
struct Case {
    /// Each unit's group, named by its first unit.
    group_of: &'static [usize],
    ahead: usize,
    workers: usize,

    /// The unit whose adding stops the walk holding.
    fails_at: Option<usize>,

    /// The group whose walk panics.
    panics_at: Option<usize>,
}

impl Case {
    const fn new(group_of: &'static [usize], ahead: usize) -> Self {
        Self {
            group_of,
            ahead,
            workers: 2,
            fails_at: None,
            panics_at: None,
        }
    }

    const fn units(self) -> usize {
        self.group_of.len()
    }

    /// The groups in the order claimed, and each group's units.
    fn groups(self) -> (Vec<usize>, Vec<Vec<usize>>) {
        let mut members = vec![Vec::new(); self.units()];
        for (unit, &group) in self.group_of.iter().enumerate() {
            members[group].push(unit);
        }

        let order = (0..self.units())
            .filter(|&group| !members[group].is_empty())
            .collect();
        (order, members)
    }
}

/// Adds walks to a list.
struct Log {
    added: Vec<Option<usize>>,
    fails_at: Option<usize>,

    /// How many units are added, for walks to check their window against.
    count: Arc<AtomicUsize>,
}

impl Add<usize> for Log {
    fn add(&mut self, walk: Option<usize>) -> bool {
        self.added.push(walk);
        self.count.store(self.added.len(), Ordering::Relaxed);
        walk.is_some() && self.fails_at != Some(self.added.len() - 1)
    }
}

/// A walk's own panic, which ends only the worker it panics on, as a
/// worker's thread ends when a walk panics in microscope.
struct WalkPanicked;

struct Run {
    case: Case,
    in_order: InOrder<'static, usize, Log>,
    count: Arc<AtomicUsize>,
}

impl Run {
    fn walk(&self, group: usize, units: &[usize]) -> Vec<(usize, Option<usize>)> {
        let added = self.count.load(Ordering::Relaxed);
        assert!(
            group < added + self.case.ahead,
            "{:?}: group {group} began with {added} units added",
            self.case
        );
        if self.case.panics_at == Some(group) {
            std::panic::panic_any(WalkPanicked);
        }

        units.iter().map(|&unit| (unit, Some(unit))).collect()
    }

    /// One worker's share, ended early by a walk that panics.
    fn work(&self) {
        let worked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.in_order.work(|group, units| self.walk(group, units));
        }));
        if let Err(panic) = worked
            && !panic.is::<WalkPanicked>()
        {
            std::panic::resume_unwind(panic);
        }
    }
}

/// How many preemptions a schedule may have: `LOOM_MAX_PREEMPTIONS`, which
/// `just loom` sets, or 3.
fn bound() -> usize {
    std::env::var("LOOM_MAX_PREEMPTIONS")
        .ok()
        .and_then(|bound| bound.parse().ok())
        .unwrap_or(3)
}

/// Runs `case` under every schedule with at most `bound` preemptions.
fn model(case: Case, bound: usize) {
    quiet_walk_panics();
    let (order, members) = case.groups();
    let (order, members): (&'static [usize], &'static [Vec<usize>]) =
        (Vec::leak(order), Vec::leak(members));
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(bound);
    builder.check(move || {
        let count = Arc::new(AtomicUsize::new(0));
        let log = Log {
            added: Vec::new(),
            fails_at: case.fails_at,
            count: Arc::clone(&count),
        };

        let run = Arc::new(Run {
            case,
            in_order: InOrder::new(order, members, log, case.ahead),
            count,
        });
        let others = (1..case.workers)
            .map(|_| {
                let run = Arc::clone(&run);
                thread::spawn(move || run.work())
            })
            .collect::<Vec<_>>();

        run.work();
        for other in others {
            other.join().expect("a worker finishes");
        }

        let Ok(run) = Arc::try_unwrap(run) else {
            panic!("every worker is done with it");
        };

        let (added, log) = run.in_order.into_parts();
        check(case, added, &log.added);
    });
}

/// What every schedule of `case` adds.
fn check(case: Case, added: usize, log: &[Option<usize>]) {
    assert_eq!(added, log.len(), "{case:?}");
    for (unit, walk) in log.iter().enumerate() {
        assert!(
            walk.is_none_or(|walked| walked == unit),
            "{case:?}: unit {unit} was added another's walk: {log:?}"
        );
    }

    match (case.fails_at, case.panics_at) {
        (None, None) => assert_eq!(
            log,
            (0..case.units()).map(Some).collect::<Vec<_>>(),
            "{case:?}: every unit, walked, in unit order"
        ),
        (Some(failed), None) => {
            assert_eq!(added, case.units(), "{case:?}: every unit is still added");
            assert!(
                log[..=failed].iter().all(Option::is_some),
                "{case:?}: {log:?}"
            );

            // A group that can begin only once the failed unit is added
            // finds the walk abandoned.
            for (unit, &group) in case.group_of.iter().enumerate() {
                if group >= failed + case.ahead {
                    assert_eq!(log[unit], None, "{case:?}: {log:?}");
                }
            }
        }
        (None, Some(panicked)) => {
            assert_eq!(
                added, panicked,
                "{case:?}: adding stops at the group that panicked"
            );
        }
        (Some(_), Some(_)) => unreachable!("a case fails or panics, not both"),
    }
}

/// Keeps walks' own panics, which are expected, out of the output.
fn quiet_walk_panics() {
    static QUIET: std::sync::Once = std::sync::Once::new();
    QUIET.call_once(|| {
        let report = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !info.payload().is::<WalkPanicked>() {
                report(info);
            }
        }));
    });
}

/// Every way to put `units` units in groups, each named by its first unit.
fn groupings(units: usize) -> Vec<&'static [usize]> {
    let mut all = vec![Vec::new()];
    for unit in 0..units {
        all = all
            .into_iter()
            .flat_map(|grouping: Vec<usize>| {
                let firsts = (0..unit)
                    .filter(|&first| grouping[first] == first)
                    .collect::<Vec<_>>();
                firsts.into_iter().chain([unit]).map(move |group| {
                    let mut next = grouping.clone();
                    next.push(group);
                    next
                })
            })
            .collect();
    }

    all.into_iter()
        .map(|grouping| &*Vec::leak(grouping))
        .collect()
}

/// Units walked one to a group, so each worker's adding hands off to the
/// other's. A unit stranded in the finished map fails the count; a worker
/// left waiting for the window fails as a deadlock.
#[test]
fn single_unit_groups_are_added_in_order() {
    model(Case::new(&[0, 1, 2], 1), bound());
}

/// A group whose units are apart: unit 2 is walked with unit 0, before
/// unit 1, and waits for it.
#[test]
fn a_group_split_by_another_waits_for_it() {
    model(Case::new(&[0, 1, 0], 1), bound());
}

/// A wider window lets both workers walk ahead of the units added.
#[test]
fn a_wider_window_still_adds_in_order() {
    model(Case::new(&[0, 1, 2], 2), bound());
}

/// Once the walk stops holding, the units left are added unwalked.
#[test]
fn units_after_the_walk_stops_holding_are_added_unwalked() {
    model(
        Case {
            fails_at: Some(0),
            ..Case::new(&[0, 1, 2], 1)
        },
        bound(),
    );
}

/// A worker whose walk of unit 0 panics abandons the walk as it unwinds,
/// and unit 0 is never added. The worker waiting to walk unit 2, which
/// waits for unit 1 to be added, must be woken all the same.
#[test]
fn a_panicking_walk_wakes_the_waiting_worker() {
    model(
        Case {
            panics_at: Some(0),
            ..Case::new(&[0, 1, 2], 1)
        },
        bound(),
    );
}

// The sweeps run each case with one preemption fewer, so that they cover
// every shape for about what the cases above cost.

#[test]
fn every_grouping_of_four_units_in_a_window_of_one() {
    for group_of in groupings(4) {
        model(Case::new(group_of, 1), bound().saturating_sub(1));
    }
}

#[test]
fn every_grouping_of_four_units_in_a_window_of_two() {
    for group_of in groupings(4) {
        model(Case::new(group_of, 2), bound().saturating_sub(1));
    }
}

#[test]
fn every_grouping_of_three_units_stopping_at_every_unit() {
    for group_of in groupings(3) {
        for ahead in [1, 2] {
            for unit in 0..3 {
                model(
                    Case {
                        fails_at: Some(unit),
                        ..Case::new(group_of, ahead)
                    },
                    bound().saturating_sub(1),
                );
            }
        }
    }
}

#[test]
fn every_grouping_of_three_units_panicking_in_every_group() {
    for group_of in groupings(3) {
        for ahead in [1, 2] {
            for group in Case::new(group_of, ahead).groups().0 {
                model(
                    Case {
                        panics_at: Some(group),
                        ..Case::new(group_of, ahead)
                    },
                    bound().saturating_sub(1),
                );
            }
        }
    }
}

// Three workers: two can wait for the window at once, so one notified
// alone would strand the other. Waking one with `notify_one` instead
// deadlocks this model at two preemptions, not at one, and not with three
// units. Three preemptions take two minutes, so it never runs with more
// than two.

#[test]
fn three_workers_add_in_order() {
    model(
        Case {
            workers: 3,
            ..Case::new(&[0, 1, 2, 3], 1)
        },
        bound().saturating_sub(1).min(2),
    );
}

/// With a panic as well, a third worker costs about what two more
/// preemptions do, so this runs with two fewer.
#[test]
fn three_workers_finish_when_a_walk_panics() {
    model(
        Case {
            workers: 3,
            panics_at: Some(0),
            ..Case::new(&[0, 1, 2, 3], 1)
        },
        bound().saturating_sub(2),
    );
}
