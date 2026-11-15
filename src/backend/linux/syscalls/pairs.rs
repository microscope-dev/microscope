//! Each stack the kernel sent, paired with the call or signal it belongs to.
//!
//! A stack taken at a call's exit, or as a signal is delivered, goes in
//! its ring just before the record it belongs to, so it pairs with its
//! thread's next call or signal. A waiting call's stack is taken as the
//! call begins and carries that time, since the call can return on
//! another CPU. It pairs only with the call that began then: a signal
//! delivered while the call waits has a stack of its own. A thread's
//! interpreted frames go just before its stack, which says whether they
//! did, so that frames whose own stack was lost are never another's.

use std::collections::BTreeMap;

use super::bpf::SampledStack;
use crate::syscalls::Tid;

/// A stack and the walker's record of the interpreted frames sent with it.
#[derive(Debug, PartialEq, Eq)]
pub struct Taken {
    pub stack: SampledStack,
    pub python: Option<Vec<u8>>,
}

/// The stacks not yet paired, by thread.
#[derive(Default)]
pub struct Pairs {
    /// Interpreted frames whose stack has not come yet.
    python: BTreeMap<Tid, Vec<u8>>,

    /// The stack of each thread's next call or signal.
    next: BTreeMap<Tid, Taken>,

    /// The stack of each thread's waiting call, taken as it began.
    waiting: BTreeMap<Tid, Taken>,
}

impl Pairs {
    /// The walker's record of a thread's interpreted frames.
    pub fn python(&mut self, thread: Tid, record: Vec<u8>) {
        self.python.insert(thread, record);
    }

    /// A thread's stack, with the interpreted frames before it.
    pub fn stack(&mut self, stack: SampledStack) {
        let thread = stack.thread;
        let python = self.python.remove(&thread);
        let taken = Taken {
            python: python.filter(|_| stack.interpreted),
            stack,
        };

        // A thread waits in one call at a time; a stack left from one
        // whose record was lost is replaced.
        if taken.stack.entered.is_some() {
            self.waiting.insert(thread, taken);
        } else {
            self.next.insert(thread, taken);
        }
    }

    /// The stack of a thread's call that began at `entered`. Whatever
    /// else the thread left unpaired is another record's, lost, and goes.
    pub fn call(&mut self, thread: Tid, entered: Option<u64>) -> Option<Taken> {
        let waiting = self.waiting(thread, entered);
        let next = self.next.remove(&thread);
        self.python.remove(&thread);
        waiting.or(next)
    }

    /// The stack of a thread's signal; a waiting call's stays.
    pub fn signal(&mut self, thread: Tid) -> Option<Taken> {
        self.python.remove(&thread);
        self.next.remove(&thread)
    }

    /// The stack a thread's call that began at `entered` took as it
    /// began, if it did: for a call still waiting as the trace ends.
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

    /// What a thread does: a call, whose stack is taken as it begins if it
    /// waits and at its exit if not, with the signals delivered while it
    /// waits; or a signal.
    #[derive(Debug, Clone)]
    enum Act {
        Call { waits: bool, signals: Vec<Fate> },
        Signal,
    }

    /// What became of the interpreted frames the walker sends before a
    /// stack.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Frames {
        /// The process runs no interpreter the walker knows.
        None,
        Sent,
        Lost,
    }

    /// How an act's records fare in the ring: its frames, and whether its
    /// stack, or the record the stack went with, was lost.
    #[derive(Debug, Clone, Copy)]
    struct Fate {
        frames: Frames,
        stack_lost: bool,
        record_lost: bool,
    }

    impl Fate {
        /// Whether the frames are in the ring before the stack.
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

    /// A record as the reader hands it on, after the rings are merged.
    #[derive(Debug)]
    enum Seen {
        Python(Tid, u64),
        Stack(Box<SampledStack>),
        /// A call: its thread, its id, when it began, and whether the
        /// record says its stack was lost.
        Call(Tid, u64, u64, bool),
        Signal(Tid, u64, bool),
    }

    /// The stack whose kernel walk is just `id`.
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

    /// The records a thread's acts send, in order. Each call or signal
    /// whose record is sent is in `expected` with the id of the stack it
    /// must be paired with, its own or none when its stack was lost, and
    /// whether its frames must be too.
    fn records(
        thread: Tid,
        acts: &[(Act, Fate)],
        ids: &mut u64,
        expected: &mut BTreeMap<u64, Option<(u64, bool)>>,
    ) -> Vec<Seen> {
        // A stack and its frames, as its fate allows.
        let stack = |out: &mut Vec<Seen>, id: u64, entered: Option<u64>, fate: Fate| {
            if fate.interpreted() {
                out.push(Seen::Python(thread, id));
            }
            if !fate.stack_lost {
                let sent = stack(thread, id, entered, fate.interpreted());
                out.push(Seen::Stack(Box::new(sent)));
            }
        };
        // A record of a call or signal, whose stack was sent before it.
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

            // The threads' records as `order` picks them, each thread's in
            // its own order, then the rest.
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

    /// A call still waiting as the trace ends takes the stack it took as
    /// it began, and only that: not the stack of a lost record, nor one of
    /// an earlier call of its thread.
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

    /// Frames whose stack did not fit are not the next stack's, when that
    /// stack's own frames did not fit either.
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
