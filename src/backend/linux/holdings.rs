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

        let mut order: Vec<(usize, &Sighting)> = seen.iter().enumerate().collect();
        order.sort_by_key(|(_, sighting)| (sighting.thread, sighting.runtime));
        let mut numbers = vec![None; seen.len()];
        for (index, &sighting) in order {
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

    /// A stop: the threads that live at it, and its sightings in the
    /// order the stop made them and in another.
    #[derive(Debug, Clone)]
    struct Stop {
        live: BTreeSet<Pid>,
        seen: Vec<Sighting>,
        shuffled: Vec<Sighting>,
    }

    const KINDS: [&str; 2] = [ISOLATE, "interpreter"];

    fn stop() -> impl proptest::strategy::Strategy<Value = Stop> {
        use proptest::prelude::*;

        let live = prop::collection::btree_set(1..=5_i32, 0..=5);
        let sightings = prop::collection::vec(
            (
                any::<prop::sample::Index>(),
                1..=2_u32,
                prop::option::of((0..KINDS.len(), 0xa..=0xc_u64)),
            ),
            0..8,
        );
        (live, sightings)
            .prop_map(|(live, sightings)| {
                let threads = live.iter().copied().map(Pid::from_raw).collect::<Vec<_>>();
                let mut seen = Vec::<Sighting>::new();
                for (thread, runtime, holding) in sightings {
                    if threads.is_empty() {
                        break;
                    }
                    let sighting = Sighting {
                        thread: *thread.get(&threads),
                        runtime: RuntimeId::new(runtime),
                        holding: holding
                            .map(|(kind, identity)| (KINDS[kind], HeldIdentity::new(identity))),
                    };
                    if !seen.iter().any(|other| {
                        (other.thread, other.runtime) == (sighting.thread, sighting.runtime)
                    }) {
                        seen.push(sighting);
                    }
                }
                (threads.into_iter().collect::<BTreeSet<_>>(), seen)
            })
            .prop_flat_map(|(live, seen)| {
                let shuffled = Just(seen.clone()).prop_shuffle();
                (Just(live), Just(seen), shuffled)
            })
            .prop_map(|(live, seen, shuffled)| Stop {
                live,
                seen,
                shuffled,
            })
    }

    type Key = (RuntimeId, &'static str, HeldIdentity);

    /// The numbering rules, stop by stop: an identity keeps its number while
    /// a living thread whose last sighting held it is not seen now holding
    /// anything else, and an identity without one takes the next of its
    /// kind, in the order of the threads that hold it.
    #[derive(Default)]
    struct Reference {
        /// What each thread was last seen holding, of each runtime.
        last: BTreeMap<(Pid, RuntimeId), Option<(&'static str, HeldIdentity)>>,
        numbers: BTreeMap<Key, u64>,
        given: BTreeMap<(RuntimeId, &'static str), u64>,
    }

    impl Reference {
        fn number(&mut self, stop: &Stop) -> Vec<Option<u64>> {
            let now = stop
                .seen
                .iter()
                .map(|sighting| ((sighting.thread, sighting.runtime), sighting.holding))
                .collect::<BTreeMap<_, _>>();
            let last = &self.last;
            self.numbers.retain(|&(runtime, kind, identity), _| {
                last.iter().any(|(&(thread, held_in), &holding)| {
                    held_in == runtime
                        && holding == Some((kind, identity))
                        && stop.live.contains(&thread)
                        && now
                            .get(&(thread, runtime))
                            .is_none_or(|now| *now == holding)
                })
            });
            self.last
                .retain(|(thread, _), _| stop.live.contains(thread));
            self.last.extend(now);

            let mut order = stop.seen.clone();
            order.sort_by_key(|sighting| (sighting.thread, sighting.runtime));
            for sighting in order {
                let Some((kind, identity)) = sighting.holding else {
                    continue;
                };
                self.numbers
                    .entry((sighting.runtime, kind, identity))
                    .or_insert_with(|| {
                        let given = self.given.entry((sighting.runtime, kind)).or_default();
                        *given += 1;
                        *given
                    });
            }

            stop.seen
                .iter()
                .map(|sighting| {
                    let (kind, identity) = sighting.holding?;
                    self.numbers
                        .get(&(sighting.runtime, kind, identity))
                        .copied()
                })
                .collect()
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(512))]

        /// Every stop numbers as the rules say, whatever the order of its
        /// sightings. Within a stop one identity has one number and two
        /// have two, and no number is given to a second identity.
        #[test]
        fn holdings_are_numbered_as_the_rules_say(
            stops in proptest::collection::vec(stop(), 1..8)
        ) {
            let mut numbers = HoldingNumbers::default();
            let mut shuffled = HoldingNumbers::default();
            let mut reference = Reference::default();
            let mut ever = BTreeMap::<(RuntimeId, &str, u64), HeldIdentity>::new();

            for stop in &stops {
                let given = numbers.number(&stop.live, &stop.seen);
                proptest::prop_assert_eq!(&given, &reference.number(stop));

                let reordered = shuffled.number(&stop.live, &stop.shuffled);
                for (sighting, number) in stop.shuffled.iter().zip(&reordered) {
                    let index = stop.seen.iter().position(|seen| seen == sighting).unwrap();
                    proptest::prop_assert_eq!(*number, given[index], "{:?}", sighting);
                }

                let mut by_key = BTreeMap::<Key, u64>::new();
                for (sighting, number) in stop.seen.iter().zip(&given) {
                    let (Some((kind, identity)), Some(number)) = (sighting.holding, *number) else {
                        continue;
                    };
                    let key = (sighting.runtime, kind, identity);
                    proptest::prop_assert_eq!(*by_key.entry(key).or_insert(number), number);
                    let shared = by_key.iter().any(|(&(runtime, other_kind, other), &held)| {
                        held == number
                            && runtime == sighting.runtime
                            && other_kind == kind
                            && other != identity
                    });
                    proptest::prop_assert!(!shared, "{number} is given to two identities");

                    let first = *ever.entry((key.0, key.1, number)).or_insert(identity);
                    proptest::prop_assert_eq!(first, identity, "{} is given again", number);
                }
            }
        }
    }
}
