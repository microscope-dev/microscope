//! The only source of randomness in a simulated run.
//!
//! A run's seed is expanded with `SplitMix64` into one `xoshiro256**` generator
//! per [`Stream`], salted by the stream's name. Each concern draws from its
//! own stream, so changing how one concern draws leaves the others' choices
//! as they were. Both generators are written here, and pinned by test
//! vectors, so a seed means the same run with every toolchain and
//! dependency version.

/// A concern that draws random choices of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// The run's shape, chosen before it starts.
    Swarm,

    /// Which action the world takes next.
    Schedule,

    /// What the client asks the debugger to do.
    Client,

    /// What the program sees of the outside world, such as `AT_RANDOM`.
    Program,

    /// What happens between two of the controller's calls into the kernel.
    Preempt,

    /// When planned faults fire.
    Fault,

    /// Which breakpoints and watchpoints the client enables, disables, and
    /// makes temporary, and where it advances to. Drawn apart from
    /// [`Stream::Client`], so seeds that never use them choose as before.
    Control,

    /// Whether and how the profiler samples running threads. Drawn apart
    /// from the rest, so seeds that never sample choose as before.
    Sample,
}

impl Stream {
    const ALL: [Self; 8] = [
        Self::Swarm,
        Self::Schedule,
        Self::Client,
        Self::Program,
        Self::Preempt,
        Self::Fault,
        Self::Control,
        Self::Sample,
    ];

    const fn name(self) -> &'static str {
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

    const fn index(self) -> usize {
        self as usize
    }
}

/// Every random choice a run makes, one generator per stream.
pub struct Choices {
    streams: [Xoshiro256StarStar; Stream::ALL.len()],
}

impl Choices {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            streams: Stream::ALL.map(|stream| {
                let mut expand = SplitMix64(seed ^ fnv1a(stream.name().as_bytes()));
                Xoshiro256StarStar([expand.next(), expand.next(), expand.next(), expand.next()])
            }),
        }
    }

    /// A uniformly distributed integer in `0..bound`.
    ///
    /// # Panics
    ///
    /// If `bound` is zero.
    pub fn below(&mut self, stream: Stream, bound: u64) -> u64 {
        assert!(bound > 0, "an empty range has nothing to choose");

        // Lemire's method: the high half of a 128-bit product, rejecting
        // the few low halves that would bias it.
        let threshold = bound.wrapping_neg() % bound;

        loop {
            let product = u128::from(self.streams[stream.index()].next()) * u128::from(bound);
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

    /// Whether an event with a chance of `per_mille` in a thousand happens.
    pub fn chance(&mut self, stream: Stream, per_mille: u64) -> bool {
        self.below(stream, 1000) < per_mille
    }

    /// One of `items`, each equally likely.
    ///
    /// # Panics
    ///
    /// If `items` is empty.
    pub fn pick<'a, T>(&mut self, stream: Stream, items: &'a [T]) -> &'a T {
        let index = self.below(stream, items.len() as u64);
        &items[usize::try_from(index).expect("an index below a slice length fits usize")]
    }

    /// An index into `weights`, each chosen in proportion to its weight.
    ///
    /// # Panics
    ///
    /// If every weight is zero.
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

    /// Fills `bytes` from `stream`.
    pub fn fill(&mut self, stream: Stream, bytes: &mut [u8]) {
        for chunk in bytes.chunks_mut(8) {
            let word = self.streams[stream.index()].next().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
}

/// `SplitMix64` (Steele, Lea, and Flood), used to expand a seed.
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

/// `xoshiro256**` (Blackman and Vigna).
struct Xoshiro256StarStar([u64; 4]);

impl Xoshiro256StarStar {
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

/// The 64-bit FNV-1a hash.
#[must_use]
pub const fn fnv1a(bytes: &[u8]) -> u64 {
    fnv1a_continue(0xcbf2_9ce4_8422_2325, bytes)
}

/// Continues an FNV-1a hash over more bytes.
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

    /// The generators and the hash match their published test vectors, and
    /// streams expand as the vectors computed independently from them say,
    /// so a seed names the same run on every toolchain.
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
