use std::collections::BTreeMap;

use super::bpf::SampledStack;
use crate::syscalls::Tid;

#[derive(Debug, PartialEq, Eq)]
pub struct Taken {
    pub stack: SampledStack,
    pub python: Option<Vec<u8>>,
}

#[derive(Default)]
pub struct Pairs {
    python: BTreeMap<Tid, Vec<u8>>,

    next: BTreeMap<Tid, Taken>,

    waiting: BTreeMap<Tid, Taken>,
}

impl Pairs {
    pub fn python(&mut self, thread: Tid, record: Vec<u8>) {
        self.python.insert(thread, record);
    }

    pub fn stack(&mut self, stack: SampledStack) {
        let thread = stack.thread;
        let python = self.python.remove(&thread);
        let taken = Taken {
            python: python.filter(|_| stack.interpreted),
            stack,
        };

        if taken.stack.entered.is_some() {
            self.waiting.insert(thread, taken);
        } else {
            self.next.insert(thread, taken);
        }
    }

    pub fn call(&mut self, thread: Tid, entered: Option<u64>) -> Option<Taken> {
        let waiting = self.waiting(thread, entered);
        let next = self.next.remove(&thread);
        self.python.remove(&thread);
        waiting.or(next)
    }

    pub fn signal(&mut self, thread: Tid) -> Option<Taken> {
        self.python.remove(&thread);
        self.next.remove(&thread)
    }

    #[must_use]
    pub fn has_waiting(&self, thread: Tid, entered: Option<u64>) -> bool {
        entered.is_some()
            && self
                .waiting
                .get(&thread)
                .is_some_and(|waiting| waiting.stack.entered == entered)
    }

    pub fn waiting(&mut self, thread: Tid, entered: Option<u64>) -> Option<Taken> {
        let waiting = self.waiting.remove(&thread)?;
        (entered.is_some() && waiting.stack.entered == entered).then_some(waiting)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use proptest::prelude::*;

    use super::*;
    use crate::syscalls::Pid;
    use crate::unwind::DwarfRegister;

    #[derive(Debug, Clone)]
    enum Act {
        Call { waits: bool, signals: Vec<Fate> },
        Signal,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Frames {
        None,
        Sent,
        Lost,
    }

    #[derive(Debug, Clone, Copy)]
    struct Fate {
        frames: Frames,
        stack_lost: bool,
        record_lost: bool,
    }

    impl Fate {
        fn interpreted(self) -> bool {
            self.frames == Frames::Sent
        }
    }

    fn fate() -> impl Strategy<Value = Fate> {
        let frames = prop_oneof![
            2 => Just(Frames::None),
            2 => Just(Frames::Sent),
            1 => Just(Frames::Lost),
        ];
        let lost = || prop::bool::weighted(0.2);
        (frames, lost(), lost()).prop_map(|(frames, stack_lost, record_lost)| Fate {
            frames,
            stack_lost,
            record_lost,
        })
    }

    fn act() -> impl Strategy<Value = (Act, Fate)> {
        let act = prop_oneof![
            (any::<bool>(), prop::collection::vec(fate(), 0..3))
                .prop_map(|(waits, signals)| Act::Call { waits, signals }),
            Just(Act::Signal),
        ];
        (act, fate())
    }

    #[derive(Debug)]
    enum Seen {
        Python(Tid, u64),
        Stack(Box<SampledStack>),
        Call(Tid, u64, u64, bool),
        Signal(Tid, u64, bool),
    }

    fn stack(thread: Tid, id: u64, entered: Option<u64>, interpreted: bool) -> SampledStack {
        SampledStack {
            thread,
            process: Pid::new(1),
            entered,
            interpreted,
            registers: std::array::from_fn(|number| {
                (DwarfRegister::new(u16::try_from(number).expect("17")), 0)
            }),
            chain: vec![id],
            copy: Vec::new(),
        }
    }

    fn records(
        thread: Tid,
        acts: &[(Act, Fate)],
        ids: &mut u64,
        expected: &mut BTreeMap<u64, Option<(u64, bool)>>,
    ) -> Vec<Seen> {
        let stack = |out: &mut Vec<Seen>, id: u64, entered: Option<u64>, fate: Fate| {
            if fate.interpreted() {
                out.push(Seen::Python(thread, id));
            }
            if !fate.stack_lost {
                let sent = stack(thread, id, entered, fate.interpreted());
                out.push(Seen::Stack(Box::new(sent)));
            }
        };
        let record = |out: &mut Vec<Seen>,
                      expected: &mut BTreeMap<u64, Option<(u64, bool)>>,
                      seen: Seen,
                      id: u64,
                      fate: Fate| {
            if !fate.record_lost {
                out.push(seen);
                expected.insert(id, (!fate.stack_lost).then_some((id, fate.interpreted())));
            }
        };

        let mut out = Vec::new();
        for (act, fate) in acts {
            *ids += 1;
            let id = *ids;
            match act {
                Act::Signal => {
                    stack(&mut out, id, None, *fate);
                    let seen = Seen::Signal(thread, id, fate.stack_lost);
                    record(&mut out, expected, seen, id, *fate);
                }
                Act::Call { waits, signals } => {
                    if *waits {
                        stack(&mut out, id, Some(id), *fate);
                        for during in signals {
                            *ids += 1;
                            stack(&mut out, *ids, None, *during);
                            let seen = Seen::Signal(thread, *ids, during.stack_lost);
                            record(&mut out, expected, seen, *ids, *during);
                        }
                    } else {
                        stack(&mut out, id, None, *fate);
                    }
                    let seen = Seen::Call(thread, id, id, fate.stack_lost);
                    record(&mut out, expected, seen, id, *fate);
                }
            }
        }

        out
    }

    proptest! {
        /// However the threads' records interleave, each call and signal
        /// is paired with its own stack and frames, or with none when its
        /// stack was lost: never with another's, whether signals came while
        /// a call waited or the record a stack went with was lost.
        #[test]
        fn each_call_and_signal_is_paired_with_its_own_stack(
            threads in prop::collection::vec(prop::collection::vec(act(), 0..8), 1..4),
            order in prop::collection::vec(0_usize..4, 0..100),
        ) {
            let mut ids = 0;
            let mut expected = BTreeMap::new();
            let mut sent: Vec<VecDeque<Seen>> = (10..)
                .zip(&threads)
                .map(|(thread, acts)| {
                    records(Tid::new(thread), acts, &mut ids, &mut expected).into()
                })
                .collect();

            let mut seen = Vec::new();
            for pick in order {
                let count = sent.len();
                seen.extend(sent[pick % count].pop_front());
            }
            seen.extend(sent.into_iter().flatten());

            let mut pairs = Pairs::default();
            let mut paired = BTreeMap::new();
            for record in seen {
                match record {
                    Seen::Python(thread, id) => pairs.python(thread, id.to_le_bytes().to_vec()),
                    Seen::Stack(stack) => pairs.stack(*stack),
                    Seen::Call(thread, id, entered, lost) => {
                        let taken = pairs.call(thread, Some(entered));
                        paired.insert(id, taken.filter(|_| !lost));
                    }
                    Seen::Signal(thread, id, lost) => {
                        let taken = pairs.signal(thread);
                        paired.insert(id, taken.filter(|_| !lost));
                    }
                }
            }

            for (id, wanted) in &expected {
                let taken = paired.get(id).and_then(Option::as_ref).map(|taken| {
                    let python = taken.python.as_ref().map(|bytes| bytes[..] == id.to_le_bytes());
                    (taken.stack.chain[0], python)
                });
                let wanted = wanted.map(|(stack, interpreted)| (stack, interpreted.then_some(true)));
                prop_assert_eq!(taken, wanted, "record {}", id);
            }
        }
    }

    #[test]
    fn a_call_still_waiting_takes_only_its_own_stack() {
        let thread = Tid::new(7);
        let mut pairs = Pairs::default();
        pairs.stack(stack(thread, 1, Some(100), false));
        assert_eq!(pairs.waiting(thread, Some(200)), None);

        pairs.stack(stack(thread, 2, None, false));
        pairs.stack(stack(thread, 3, Some(300), false));
        let taken = pairs
            .waiting(thread, Some(300))
            .map(|taken| taken.stack.chain);
        assert_eq!(taken, Some(vec![3]));
        assert_eq!(pairs.waiting(thread, Some(300)), None);
    }

    #[test]
    fn frames_whose_stack_was_lost_are_no_others() {
        let thread = Tid::new(7);
        let mut pairs = Pairs::default();
        pairs.python(thread, vec![1]);
        pairs.stack(stack(thread, 2, None, false));
        let taken = pairs.signal(thread).expect("its stack");
        assert_eq!((taken.stack.chain, taken.python), (vec![2], None));
    }
}
