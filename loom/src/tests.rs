use std::sync::atomic::{AtomicUsize, Ordering};

use loom::sync::Arc;
use loom::thread;

use crate::in_order::{Add, InOrder};

#[derive(Clone, Copy, Debug)]
struct Case {
    group_of: &'static [usize],
    ahead: usize,
    workers: usize,

    fails_at: Option<usize>,

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

struct Log {
    added: Vec<Option<usize>>,
    fails_at: Option<usize>,

    count: Arc<AtomicUsize>,
}

impl Add<usize> for Log {
    fn add(&mut self, walk: Option<usize>) -> bool {
        self.added.push(walk);
        self.count.store(self.added.len(), Ordering::Relaxed);
        walk.is_some() && self.fails_at != Some(self.added.len() - 1)
    }
}

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

fn bound() -> usize {
    std::env::var("LOOM_MAX_PREEMPTIONS")
        .ok()
        .and_then(|bound| bound.parse().ok())
        .unwrap_or(3)
}

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

#[test]
fn single_unit_groups_are_added_in_order() {
    model(Case::new(&[0, 1, 2], 1), bound());
}

#[test]
fn a_group_split_by_another_waits_for_it() {
    model(Case::new(&[0, 1, 0], 1), bound());
}

#[test]
fn a_wider_window_still_adds_in_order() {
    model(Case::new(&[0, 1, 2], 2), bound());
}

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
