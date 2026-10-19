//! Statistics per call, folded the same way from either engine: in the
//! kernel by the BPF engine's count mode, from events otherwise.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::model::{Errno, Outcome, Pid, SyscallEvent, SyscallId};
use super::specs::{self, Arg, Count, Direction, MemKind};

/// Buckets in a latency histogram. Bucket 0 holds 0 ns; bucket `i` holds
/// `[2^(i-1), 2^i)` ns; the last holds everything longer, from about nine
/// minutes.
pub const BUCKETS: usize = 40;

/// The bucket a duration falls in. The BPF programs compute the same.
#[must_use]
pub fn bucket(nanoseconds: u64) -> usize {
    (64 - nanoseconds.leading_zeros() as usize).min(BUCKETS - 1)
}

/// The smallest duration a bucket holds, and the smallest the next holds.
#[must_use]
pub const fn bucket_range(bucket: usize) -> (u64, u64) {
    if bucket == 0 {
        (0, 1)
    } else if bucket == BUCKETS - 1 {
        (1 << (bucket - 1), u64::MAX)
    } else {
        (1 << (bucket - 1), 1 << bucket)
    }
}

/// What one call cost, over one process or a whole trace.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SyscallStats {
    pub calls: u64,
    pub errors: u64,
    pub errors_by_errno: BTreeMap<u16, u64>,

    /// Calls with a duration, which `total_ns` and the histogram cover.
    pub timed: u64,
    pub total_ns: u64,

    /// The shortest timed call, or 0 before any.
    pub min_ns: u64,
    pub max_ns: u64,
    pub log2_ns: Vec<u64>,

    /// Bytes read or written, for calls that move them.
    pub bytes: u64,
}

impl SyscallStats {
    /// Adds one call.
    pub fn add(&mut self, outcome: Outcome, duration: Option<u64>, bytes: u64) {
        self.calls += 1;
        if let Outcome::Failed(Errno(errno)) = outcome {
            self.errors += 1;
            *self.errors_by_errno.entry(errno).or_default() += 1;
        }

        if let Some(duration) = duration {
            self.min_ns = if self.timed == 0 {
                duration
            } else {
                self.min_ns.min(duration)
            };

            self.timed += 1;
            self.total_ns = self.total_ns.saturating_add(duration);
            self.max_ns = self.max_ns.max(duration);
            if self.log2_ns.len() < BUCKETS {
                self.log2_ns.resize(BUCKETS, 0);
            }

            self.log2_ns[bucket(duration)] += 1;
        }

        self.bytes = self.bytes.saturating_add(bytes);
    }

    /// Adds another's counts, as merging processes or CPUs does.
    pub fn merge(&mut self, other: &Self) {
        self.calls += other.calls;
        self.errors += other.errors;
        for (&errno, &count) in &other.errors_by_errno {
            *self.errors_by_errno.entry(errno).or_default() += count;
        }

        if other.timed > 0 {
            self.min_ns = if self.timed == 0 {
                other.min_ns
            } else {
                self.min_ns.min(other.min_ns)
            };
        }

        self.timed += other.timed;
        self.total_ns = self.total_ns.saturating_add(other.total_ns);
        self.max_ns = self.max_ns.max(other.max_ns);
        if self.log2_ns.len() < other.log2_ns.len() {
            self.log2_ns.resize(other.log2_ns.len(), 0);
        }

        for (into, &count) in self.log2_ns.iter_mut().zip(&other.log2_ns) {
            *into += count;
        }

        self.bytes = self.bytes.saturating_add(other.bytes);
    }

    /// Mean nanoseconds per timed call.
    #[must_use]
    pub const fn mean_ns(&self) -> Option<u64> {
        self.total_ns.checked_div(self.timed)
    }

    /// An estimate of the `q` quantile (0 to 1) of the timed calls'
    /// durations, interpolated linearly within its log2 bucket and never
    /// above the largest seen. The bucket bounds it within a factor of two.
    #[must_use]
    pub fn quantile_ns(&self, q: f64) -> Option<u64> {
        if self.timed == 0 {
            return None;
        }

        #[expect(
            clippy::cast_precision_loss,
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "estimates"
        )]
        let rank = ((q.clamp(0.0, 1.0) * self.timed as f64).ceil() as u64).max(1);
        let mut seen = 0;

        for (index, &count) in self.log2_ns.iter().enumerate() {
            if count == 0 {
                continue;
            }

            if seen + count >= rank {
                let (low, high) = bucket_range(index);
                let high = high.min(self.max_ns.saturating_add(1)).max(low + 1);

                #[expect(
                    clippy::cast_precision_loss,
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "estimates"
                )]
                let within =
                    (((rank - seen) as f64 - 0.5) / count as f64 * (high - low) as f64) as u64;
                return Some((low + within).min(self.max_ns));
            }

            seen += count;
        }

        Some(self.max_ns)
    }
}

/// The bytes a call moved, for calls whose result counts them: reads,
/// writes, sends, and receives.
#[must_use]
pub fn bytes_moved(event: &SyscallEvent) -> u64 {
    match event.outcome {
        Outcome::Returned(value) if moves(event.call) => value.max(0).unsigned_abs(),
        _ => 0,
    }
}

/// Whether a call's result counts the bytes it read or wrote through a
/// descriptor, as `read` and `sendmsg`'s do.
#[must_use]
pub fn moves(call: SyscallId) -> bool {
    let Some(spec) = specs::spec(call) else {
        return false;
    };

    spec.args.iter().any(|arg| {
        matches!(
            arg,
            Arg::Mem {
                kind: MemKind::Bytes | MemKind::Struct(specs::Struct::Iovec),
                count: Count::Return | Count::Arg(_),
                direction: Direction::In | Direction::Out,
            }
        )
    }) && spec.args.first().is_some_and(|arg| matches!(arg, Arg::Fd))
}

/// Statistics folded per process and call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Folded {
    pub by_call: BTreeMap<(Pid, SyscallId), SyscallStats>,
}

impl Folded {
    /// Adds every call `other` counted.
    pub fn merge(&mut self, other: &Self) {
        for (key, stats) in &other.by_call {
            self.by_call.entry(*key).or_default().merge(stats);
        }
    }

    pub fn add(&mut self, event: &SyscallEvent) {
        self.by_call
            .entry((event.process, event.call))
            .or_default()
            .add(event.outcome, event.duration(), bytes_moved(event));
    }

    /// Totals per call, over every process.
    #[must_use]
    pub fn per_call(&self) -> BTreeMap<SyscallId, SyscallStats> {
        let mut calls: BTreeMap<SyscallId, SyscallStats> = BTreeMap::new();

        for (&(_, call), stats) in &self.by_call {
            calls.entry(call).or_default().merge(stats);
        }

        calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_split_at_powers_of_two() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(1), 1);
        assert_eq!(bucket(2), 2);
        assert_eq!(bucket(3), 2);
        assert_eq!(bucket(4), 3);
        assert_eq!(bucket(1023), 10);
        assert_eq!(bucket(1024), 11);
        assert_eq!(bucket(u64::MAX), BUCKETS - 1);
        for index in 1..BUCKETS - 1 {
            let (low, high) = bucket_range(index);
            assert_eq!(bucket(low), index);
            assert_eq!(bucket(high - 1), index);
            assert_eq!(bucket(high), index + 1);
        }
    }

    #[test]
    fn quantiles_stay_in_their_bucket_and_below_the_largest() {
        let mut stats = SyscallStats::default();

        for duration in [100, 110, 120, 130, 5_000] {
            stats.add(Outcome::Returned(0), Some(duration), 0);
        }

        stats.add(Outcome::Failed(Errno(2)), None, 0);
        assert_eq!((stats.calls, stats.timed, stats.errors), (6, 5, 1));
        let median = stats.quantile_ns(0.5).expect("timed");
        assert!((64..128).contains(&median), "{median}");
        let p99 = stats.quantile_ns(0.99).expect("timed");
        assert!((4096..=5_000).contains(&p99), "{p99}");
        assert_eq!(stats.mean_ns(), Some(1092));
        let mut merged = stats.clone();
        merged.merge(&stats);
        assert_eq!(
            (merged.calls, merged.errors_by_errno[&2], merged.max_ns),
            (12, 2, 5_000)
        );
    }
}
