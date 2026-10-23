//! The numbers the thread list gives what threads run in, such as V8's
//! isolates: `isolate 1` for the first one seen. A runtime names each by an
//! identity, such as an address, that another may take once it is gone, so
//! a number lasts only while a thread last seen holding its identity lives.

use std::collections::{BTreeMap, BTreeSet};

use crate::RuntimeId;
use crate::runtime_model::HeldIdentity;
use nix::unistd::Pid;

/// What one thread was seen to run in at a stop, of one runtime's things:
/// `None` when the runtime read that it runs in none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Sighting {
    pub thread: Pid,
    pub runtime: RuntimeId,
    pub holding: Option<(&'static str, HeldIdentity)>,
}

/// The numbers of one process's lifetime, which a new process starts over.
#[derive(Debug, Default)]
pub(super) struct HoldingNumbers {
    /// The last number given to each runtime's kind.
    last: BTreeMap<(RuntimeId, &'static str), u64>,
    held: BTreeMap<(RuntimeId, &'static str, HeldIdentity), Held>,
}

#[derive(Debug)]
struct Held {
    number: u64,

    /// The threads last seen holding it.
    holders: BTreeSet<Pid>,
}

impl HoldingNumbers {
    /// The number of what each of `seen`, one stop's sightings, holds, in
    /// their order, given the threads that live at the stop. A thread whose
    /// runtime could not be read is not seen, and holds what it last did.
    ///
    /// An identity no living thread was last seen holding is forgotten
    /// first, so a new one at its address gets a new number. New ones are
    /// then numbered in the order of their threads' ids. `None` only once
    /// a kind's numbers are exhausted.
    pub(super) fn number(&mut self, live: &BTreeSet<Pid>, seen: &[Sighting]) -> Vec<Option<u64>> {
        let now: BTreeMap<(Pid, RuntimeId), Option<(&'static str, HeldIdentity)>> = seen
            .iter()
            .map(|sighting| ((sighting.thread, sighting.runtime), sighting.holding))
            .collect();
        self.held.retain(|&(runtime, kind, identity), held| {
            held.holders.retain(|thread| {
                live.contains(thread)
                    && now
                        .get(&(*thread, runtime))
                        .is_none_or(|holding| *holding == Some((kind, identity)))
            });
            !held.holders.is_empty()
        });

        let mut order: Vec<usize> = (0..seen.len()).collect();
        order.sort_by_key(|&index| (seen[index].thread, seen[index].runtime));
        let mut numbers = vec![None; seen.len()];
        for index in order {
            let sighting = seen[index];
            let Some((kind, identity)) = sighting.holding else {
                continue;
            };
            let key = (sighting.runtime, kind, identity);
            let number = if let Some(held) = self.held.get_mut(&key) {
                held.holders.insert(sighting.thread);
                Some(held.number)
            } else {
                let last = self.last.entry((sighting.runtime, kind)).or_insert(0);
                last.checked_add(1).inspect(|&number| {
                    *last = number;
                    self.held.insert(
                        key,
                        Held {
                            number,
                            holders: BTreeSet::from([sighting.thread]),
                        },
                    );
                })
            };
            if let Some(slot) = numbers.get_mut(index) {
                *slot = number;
            }
        }
        numbers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISOLATE: &str = "isolate";
    const NODE: RuntimeId = RuntimeId::new(1);

    fn holds(thread: i32, identity: u64) -> Sighting {
        Sighting {
            thread: Pid::from_raw(thread),
            runtime: NODE,
            holding: Some((ISOLATE, HeldIdentity::new(identity))),
        }
    }

    fn holds_none(thread: i32) -> Sighting {
        Sighting {
            thread: Pid::from_raw(thread),
            runtime: NODE,
            holding: None,
        }
    }

    fn live(threads: &[i32]) -> BTreeSet<Pid> {
        threads.iter().copied().map(Pid::from_raw).collect()
    }

    /// The first stop numbers in thread-id order, whatever the order of
    /// the sightings, and a thread that holds nothing has no number.
    #[test]
    fn the_first_stop_numbers_in_the_order_of_thread_ids() {
        let mut numbers = HoldingNumbers::default();
        let seen = [
            holds(30, 0xc00),
            holds(10, 0xa00),
            holds_none(15),
            holds(20, 0xb00),
        ];
        assert_eq!(
            numbers.number(&live(&[10, 15, 20, 30]), &seen),
            [Some(3), Some(1), None, Some(2)]
        );
    }

    /// The same identity keeps its number at every later stop, and one
    /// first seen later gets the next number.
    #[test]
    fn a_number_lasts_across_stops() {
        let mut numbers = HoldingNumbers::default();
        let threads = live(&[10, 20, 30]);
        numbers.number(&threads, &[holds(10, 0xa00), holds(20, 0xb00)]);
        assert_eq!(
            numbers.number(&threads, &[holds(20, 0xb00), holds(10, 0xa00)]),
            [Some(2), Some(1)]
        );
        assert_eq!(
            numbers.number(&threads, &[holds(10, 0xa00), holds(30, 0xc00)]),
            [Some(1), Some(3)]
        );
    }

    /// A thread whose runtime could not be read keeps what it held, and
    /// another thread that holds the same identity shares its number.
    #[test]
    fn an_unread_thread_keeps_its_number() {
        let mut numbers = HoldingNumbers::default();
        numbers.number(&live(&[10, 20]), &[holds(10, 0xa00), holds(20, 0xb00)]);
        assert_eq!(
            numbers.number(&live(&[10, 20, 30]), &[holds(10, 0xa00), holds(30, 0xb00)]),
            [Some(1), Some(2)]
        );
    }

    /// Once every thread last seen holding an identity has exited, or
    /// holds another or none, the identity seen again, as an address V8
    /// reuses for a new isolate, gets a new number, and the old one is
    /// never given again.
    #[test]
    fn a_reused_identity_gets_a_new_number() {
        let mut numbers = HoldingNumbers::default();
        numbers.number(
            &live(&[10, 20, 30]),
            &[holds(10, 0xa00), holds(20, 0xb00), holds(30, 0xc00)],
        );

        // Thread 20 exited, and 40, a new worker, has its isolate's address.
        assert_eq!(
            numbers.number(&live(&[10, 30, 40]), &[holds(10, 0xa00), holds(40, 0xb00)]),
            [Some(1), Some(4)]
        );

        // Thread 30 was seen to leave its isolate before 5, with a lower
        // id, took the address, at a single stop.
        assert_eq!(
            numbers.number(
                &live(&[5, 10, 30, 40]),
                &[
                    holds(5, 0xc00),
                    holds(10, 0xa00),
                    holds_none(30),
                    holds(40, 0xb00)
                ]
            ),
            [Some(5), Some(1), None, Some(4)]
        );
    }

    /// Each runtime, and each of its kinds, numbers from 1 on its own.
    #[test]
    fn runtimes_and_kinds_number_apart() {
        let mut numbers = HoldingNumbers::default();
        let other = RuntimeId::new(2);
        let seen = [
            holds(10, 0xa00),
            Sighting {
                thread: Pid::from_raw(10),
                runtime: other,
                holding: Some((ISOLATE, HeldIdentity::new(0xa00))),
            },
            Sighting {
                thread: Pid::from_raw(20),
                runtime: NODE,
                holding: Some(("interpreter", HeldIdentity::new(0xa00))),
            },
            holds(30, 0xb00),
        ];
        assert_eq!(
            numbers.number(&live(&[10, 20, 30]), &seen),
            [Some(1), Some(1), Some(1), Some(2)]
        );
    }
}
