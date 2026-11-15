use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

pub struct Merge<T> {
    rings: Vec<VecDeque<T>>,
}

impl<T> Merge<T> {
    #[must_use]
    pub fn new(rings: usize) -> Self {
        Self {
            rings: (0..rings).map(|_| VecDeque::new()).collect(),
        }
    }

    pub fn push(&mut self, ring: usize, record: T) {
        if let Some(held) = self.rings.get_mut(ring) {
            held.push_back(record);
        }
    }

    #[must_use]
    pub fn holds(&self) -> bool {
        self.rings.iter().any(|ring| !ring.is_empty())
    }

    pub fn release(
        &mut self,
        cut: Option<u64>,
        time: impl Fn(&T) -> Option<u64>,
        out: &mut Vec<T>,
    ) {
        let ready = |ring: &VecDeque<T>| -> Option<(u64, usize)> {
            let (len, at) = ring
                .iter()
                .enumerate()
                .find_map(|(index, record)| Some((index + 1, time(record)?)))?;
            cut.is_none_or(|cut| at <= cut).then_some((at, len))
        };

        let mut heads: BinaryHeap<Reverse<(u64, usize, usize)>> = self
            .rings
            .iter()
            .enumerate()
            .filter_map(|(index, ring)| ready(ring).map(|(at, len)| Reverse((at, index, len))))
            .collect();

        while let Some(Reverse((_, index, len))) = heads.pop() {
            let Some(ring) = self.rings.get_mut(index) else {
                continue;
            };

            out.extend(ring.drain(..len));
            if let Some((at, len)) = ready(ring) {
                heads.push(Reverse((at, index, len)));
            }
        }

        if cut.is_none() {
            for ring in &mut self.rings {
                out.extend(ring.drain(..));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    type Item = (usize, usize, Option<u64>);

    fn rings() -> impl Strategy<Value = Vec<Vec<(usize, u64)>>> {
        prop::collection::vec(prop::collection::vec((0_usize..3, 0_u64..50), 0..12), 1..5)
    }

    proptest! {
        /// Records are pushed in rounds, each released up to a later
        /// cut, then all of them: each leaves once, in its ring's order,
        /// with its group, no later than its cut allows, and earliest
        /// first wherever the rings' times grow.
        #[test]
        fn merged_records_keep_their_rings_order_and_their_groups(
            rings in rings(),
            rounds in prop::collection::vec((0_usize..5, 0_u64..10), 1..6),
            sorted in any::<bool>(),
        ) {
            let mut rings = rings;
            if sorted {
                for ring in &mut rings {
                    ring.sort_by_key(|&(_, at)| at);
                }
            }

            let items: Vec<Vec<Item>> = rings
                .iter()
                .enumerate()
                .map(|(index, groups)| {
                    let mut items = Vec::new();
                    for &(untimed, at) in groups {
                        items.extend((0..untimed).map(|_| (index, 0, None)));
                        items.push((index, 0, Some(at)));
                    }
                    for (place, item) in items.iter_mut().enumerate() {
                        item.1 = place;
                    }
                    items
                })
                .collect();

            let mut merge = Merge::new(items.len());
            let mut pushed = vec![0; items.len()];
            let mut out: Vec<Item> = Vec::new();
            let mut cut = 0;
            for &(take, step) in &rounds {
                for (index, ring) in items.iter().enumerate() {
                    for &item in ring.iter().skip(pushed[index]).take(take * 2) {
                        merge.push(index, item);
                        pushed[index] += 1;
                    }
                }

                cut += step * 10;
                let before = out.len();
                merge.release(Some(cut), |item| item.2, &mut out);
                let released = &out[before..];
                prop_assert!(released.iter().all(|item| item.2.is_none_or(|at| at <= cut)));
                if sorted {
                    let times: Vec<u64> = released.iter().filter_map(|item| item.2).collect();
                    prop_assert!(times.is_sorted(), "{times:?}");
                }
            }

            for (index, ring) in items.iter().enumerate() {
                for &item in ring.iter().skip(pushed[index]) {
                    merge.push(index, item);
                }
            }
            merge.release(None, |item| item.2, &mut out);
            prop_assert!(!merge.holds());

            let total: usize = items.iter().map(Vec::len).sum();
            prop_assert_eq!(out.len(), total);
            for (index, ring) in items.iter().enumerate() {
                let order: Vec<Item> = out.iter().copied().filter(|item| item.0 == index).collect();
                prop_assert_eq!(&order, ring);
            }

            for (position, item) in out.iter().enumerate() {
                if item.2.is_none() && item.1 + 1 < items[item.0].len() {
                    prop_assert_eq!(out.get(position + 1), Some(&items[item.0][item.1 + 1]));
                }
            }
        }
    }

    #[test]
    fn a_held_group_holds_its_ring() {
        let mut merge = Merge::new(2);
        merge.push(0, (0, 0, Some(30)));
        merge.push(0, (0, 1, Some(5)));
        merge.push(1, (1, 0, None));
        merge.push(1, (1, 1, Some(10)));
        merge.push(1, (1, 2, None));

        let mut out = Vec::new();
        merge.release(Some(20), |item: &Item| item.2, &mut out);
        assert_eq!(out, [(1, 0, None), (1, 1, Some(10))]);

        out.clear();
        merge.release(None, |item: &Item| item.2, &mut out);
        assert_eq!(out, [(0, 0, Some(30)), (0, 1, Some(5)), (1, 2, None)]);
    }
}
