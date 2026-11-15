#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Swarm,

    Schedule,

    Client,

    Program,

    Preempt,

    Fault,

    Control,

    Sample,
}

impl Stream {
    pub(crate) const ALL: [Self; 8] = [
        Self::Swarm,
        Self::Schedule,
        Self::Client,
        Self::Program,
        Self::Preempt,
        Self::Fault,
        Self::Control,
        Self::Sample,
    ];

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Swarm => "swarm",
            Self::Schedule => "schedule",
            Self::Client => "client",
            Self::Program => "program",
            Self::Preempt => "preempt",
            Self::Fault => "fault",
            Self::Control => "control",
            Self::Sample => "sample",
        }
    }

    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tape {
    pub streams: [Vec<u64>; Stream::ALL.len()],
}

impl Tape {
    #[must_use]
    pub fn len(&self) -> usize {
        self.streams.iter().map(Vec::len).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct Replay {
    given: Tape,
    used: [usize; Stream::ALL.len()],
    drawn: Tape,
}

pub struct Choices {
    streams: [Xoshiro256StarStar; Stream::ALL.len()],

    replay: Option<Box<Replay>>,
}

impl Choices {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            streams: Stream::ALL.map(|stream| {
                let mut expand = SplitMix64(seed ^ fnv1a(stream.name().as_bytes()));
                Xoshiro256StarStar([expand.next(), expand.next(), expand.next(), expand.next()])
            }),
            replay: None,
        }
    }

    #[must_use]
    pub fn replaying(seed: u64, tape: Tape) -> Self {
        Self {
            replay: Some(Box::new(Replay {
                given: tape,
                used: [0; Stream::ALL.len()],
                drawn: Tape::default(),
            })),
            ..Self::new(seed)
        }
    }

    #[must_use]
    pub fn drawn(&self) -> Option<&Tape> {
        self.replay.as_ref().map(|replay| &replay.drawn)
    }

    pub fn below(&mut self, stream: Stream, bound: u64) -> u64 {
        assert!(bound > 0, "an empty range has nothing to choose");

        let Some(replay) = &mut self.replay else {
            return self.streams[stream.index()].below(bound);
        };

        let index = stream.index();
        let value = match replay.given.streams[index].get(replay.used[index]) {
            Some(&value) => {
                replay.used[index] += 1;
                value.min(bound - 1)
            }
            None => self.streams[index].below(bound),
        };

        replay.drawn.streams[index].push(value);
        value
    }

    pub fn chance(&mut self, stream: Stream, per_mille: u64) -> bool {
        self.below(stream, 1000) < per_mille
    }

    pub fn pick<'a, T>(&mut self, stream: Stream, items: &'a [T]) -> &'a T {
        let index = self.below(stream, items.len() as u64);
        &items[usize::try_from(index).expect("an index below a slice length fits usize")]
    }

    pub fn weighted(&mut self, stream: Stream, weights: &[u64]) -> usize {
        let mut draw = self.below(stream, weights.iter().sum());

        for (index, &weight) in weights.iter().enumerate() {
            if draw < weight {
                return index;
            }

            draw -= weight;
        }

        unreachable!("a draw below the total weight falls in some weight")
    }

    pub fn fill(&mut self, stream: Stream, bytes: &mut [u8]) {
        for chunk in bytes.chunks_mut(8) {
            let word = self.streams[stream.index()].next().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
}

struct SplitMix64(u64);

impl SplitMix64 {
    const fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = self.0;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^ (mixed >> 31)
    }
}

struct Xoshiro256StarStar([u64; 4]);

impl Xoshiro256StarStar {
    fn below(&mut self, bound: u64) -> u64 {
        let threshold = bound.wrapping_neg() % bound;

        loop {
            let product = u128::from(self.next()) * u128::from(bound);
            #[expect(
                clippy::cast_possible_truncation,
                reason = "the low half of the product is wanted"
            )]
            let low = product as u64;
            if low >= threshold {
                return (product >> 64) as u64;
            }
        }
    }

    const fn next(&mut self) -> u64 {
        let state = &mut self.0;
        let result = state[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let shifted = state[1] << 17;

        state[2] ^= state[0];
        state[3] ^= state[1];
        state[1] ^= state[2];
        state[0] ^= state[3];
        state[2] ^= shifted;
        state[3] = state[3].rotate_left(45);

        result
    }
}

#[must_use]
pub const fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_continue(0xcbf2_9ce4_8422_2325, bytes)
}

#[must_use]
pub const fn fnv1a_continue(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut index = 0;
    while index < bytes.len() {
        hash = (hash ^ bytes[index] as u64).wrapping_mul(0x0100_0000_01b3);
        index += 1;
    }

    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generators_match_their_reference_values() {
        let mut expand = SplitMix64(1_234_567);
        assert_eq!(
            [expand.next(), expand.next(), expand.next()],
            [
                6_457_827_717_110_365_317,
                3_203_168_211_198_807_973,
                9_817_491_932_198_370_423
            ]
        );

        let mut generator = Xoshiro256StarStar([1, 2, 3, 4]);
        assert_eq!(
            [
                generator.next(),
                generator.next(),
                generator.next(),
                generator.next()
            ],
            [11_520, 0, 1_509_978_240, 1_215_971_899_390_074_240]
        );

        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);

        let mut choices = Choices::new(42);
        let schedule = &mut choices.streams[Stream::Schedule.index()];
        assert_eq!(
            [schedule.next(), schedule.next()],
            [1_886_335_871_282_662_602, 10_910_123_076_694_925_545]
        );

        let swarm = &mut choices.streams[Stream::Swarm.index()];
        assert_eq!(
            [swarm.next(), swarm.next()],
            [10_982_569_894_039_428_951, 15_233_243_796_857_723_996]
        );
    }
}
