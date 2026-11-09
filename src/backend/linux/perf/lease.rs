//! Ring memory as a lease, for test builds only.
//!
//! The kernel lets a user lock `perf_event_mlock_kb` for each CPU in rings,
//! and each process `RLIMIT_MEMLOCK` past that. Tests that record at once
//! share it: past it, rings shrink, and below the fewest pages a recording
//! needs, opening fails. Built with the `ring-lease` feature, as `just`
//! builds tests, every recording first leases what its rings lock from a
//! pool that size, held in lock files that every test process shares. A
//! lease that is not free makes the rings smaller, as the kernel's refusal
//! does; at the fewest pages, opening waits for one.
//!
//! Without the feature, every lease is granted and holds nothing.

/// How to take a lease that is not free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wanted {
    /// Give up at once, for rings that can still be smaller.
    Try,

    /// Wait for one, for the fewest pages the rings can have, and go on
    /// without one if it takes too long.
    Wait,
}

#[cfg(not(feature = "ring-lease"))]
mod imp {
    /// A lease of nothing.
    #[derive(Debug)]
    pub struct Lease;

    #[expect(
        clippy::unnecessary_wraps,
        reason = "a test build's lease may be refused"
    )]
    pub const fn take(_bytes: usize, _wanted: super::Wanted) -> Option<Lease> {
        Some(Lease)
    }
}

#[cfg(feature = "ring-lease")]
mod imp {
    use std::fs::File;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};

    use super::Wanted;

    /// The pool is cut in this many slots, so that a lease of any size
    /// rounds up to whole slots and leases never sum past the pool.
    const SLOTS: usize = 256;

    /// How long opening waits at the fewest pages before it goes on without
    /// a lease, so that a process holding one while it waits for another
    /// cannot wait for good.
    const MOST_WAIT: Duration = Duration::from_secs(20);

    const RETRY: Duration = Duration::from_millis(20);

    /// Slots of ring memory, each a locked slot file, held until dropped.
    #[derive(Debug)]
    pub struct Lease {
        _slots: Vec<File>,
    }

    /// The lock files of a pool of `unit` bytes a slot.
    #[derive(Debug)]
    pub struct Pool {
        directory: PathBuf,
        unit: usize,
    }

    pub fn take(bytes: usize, wanted: Wanted) -> Option<Lease> {
        static POOL: OnceLock<Option<Pool>> = OnceLock::new();
        let pool = POOL.get_or_init(|| {
            let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/ring-lease");
            let pool = Pool::new(directory, user_ring_bytes()?);
            if let Err(error) = &pool {
                record!("perf: no ring lease pool: {error}");
            }
            pool.ok()
        });

        let Some(pool) = pool else {
            return Some(Lease { _slots: Vec::new() });
        };
        let lease = pool.take(bytes, wanted, MOST_WAIT);
        if lease.is_none() && wanted == Wanted::Wait {
            // The kernel decides.
            record!("perf: no ring lease of {bytes} bytes after {MOST_WAIT:?}");
            return Some(Lease { _slots: Vec::new() });
        }
        lease
    }

    /// What this user may lock in rings: `perf_event_mlock_kb` for each
    /// online CPU, then this process's `RLIMIT_MEMLOCK`. Every test process
    /// has the same.
    fn user_ring_bytes() -> Option<usize> {
        let kib: usize = std::fs::read_to_string("/proc/sys/kernel/perf_event_mlock_kb")
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let cpus = super::super::online_cpus().ok()?.len();
        let (limit, _) =
            nix::sys::resource::getrlimit(nix::sys::resource::Resource::RLIMIT_MEMLOCK).ok()?;
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        Some(
            kib.saturating_mul(1024)
                .saturating_mul(cpus)
                .saturating_add(limit),
        )
    }

    impl Pool {
        /// The pool of `bytes` whose lock files are in `directory`.
        pub fn new(directory: PathBuf, bytes: usize) -> std::io::Result<Self> {
            std::fs::create_dir_all(&directory)?;
            Ok(Self {
                directory,
                unit: bytes.div_ceil(SLOTS).max(1),
            })
        }

        /// A lease of `bytes`, all of it or none: while another holds what
        /// it needs, none if `wanted` is [`Wanted::Try`], and otherwise once
        /// enough is let go, or none after `most`.
        pub fn take(&self, bytes: usize, wanted: Wanted, most: Duration) -> Option<Lease> {
            let needed = bytes.div_ceil(self.unit);
            if needed > SLOTS && wanted == Wanted::Try {
                return None;
            }

            let deadline = Instant::now() + most;
            loop {
                if let Ok(Some(slots)) = self.try_take(needed) {
                    return Some(Lease { _slots: slots });
                }
                if wanted == Wanted::Try || Instant::now() >= deadline {
                    return None;
                }

                std::thread::sleep(RETRY);
            }
        }

        /// `needed` free slots, taken together under the pool's own lock
        /// so that two takers never each hold part of what both need.
        fn try_take(&self, needed: usize) -> std::io::Result<Option<Vec<File>>> {
            let taking = File::create(self.directory.join("taking"))?;
            taking.lock()?;
            let mut slots = Vec::with_capacity(needed);
            for slot in 0..SLOTS {
                if slots.len() == needed {
                    break;
                }

                let file = File::create(self.directory.join(format!("slot-{slot:03}")))?;
                if file.try_lock().is_ok() {
                    slots.push(file);
                }
            }

            Ok((slots.len() == needed).then_some(slots))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A scratch pool of 256 bytes, a byte a slot.
        fn pool(name: &str) -> Pool {
            let directory = std::env::temp_dir().join(format!(
                "microscope-ring-lease-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&directory);
            Pool::new(directory, SLOTS).expect("a pool")
        }

        const NONE: Duration = Duration::ZERO;

        /// Leases are granted while what they hold fits the pool, and one
        /// that does not fit is refused until another lets go.
        #[test]
        fn leases_never_hold_more_than_the_pool() {
            let pool = pool("fits");
            let most = pool.take(200, Wanted::Try, NONE).expect("200 of 256");
            let rest = pool.take(56, Wanted::Try, NONE).expect("the other 56");
            assert!(
                pool.take(1, Wanted::Try, NONE).is_none(),
                "the pool is full"
            );

            drop(rest);
            assert!(pool.take(57, Wanted::Try, NONE).is_none(), "56 are free");
            let again = pool.take(56, Wanted::Try, NONE).expect("56 again");
            drop((most, again));
            assert!(pool.take(256, Wanted::Try, NONE).is_some(), "all of it");
            assert!(pool.take(257, Wanted::Try, NONE).is_none(), "more than all");
        }

        /// A lease that waits is granted once enough is let go, by another
        /// thread here as by another test process, and gives up at its
        /// deadline.
        #[test]
        fn a_lease_waits_for_another_to_let_go_or_gives_up() {
            let pool = pool("waits");
            let held = pool.take(200, Wanted::Try, NONE).expect("200 of 256");
            assert!(
                pool.take(100, Wanted::Wait, Duration::from_millis(60))
                    .is_none(),
                "nothing was let go"
            );

            let (let_go, waiting) = std::sync::mpsc::channel();
            std::thread::scope(|scope| {
                scope.spawn(move || {
                    waiting.recv().expect("the taker waits");
                    drop(held);
                });
                let_go.send(()).expect("let go");
                let taken = pool.take(100, Wanted::Wait, Duration::from_secs(60));
                assert!(taken.is_some(), "the lease let go of is taken");
            });
        }

        /// A lease is a byte count rounded up to whole slots of the pool.
        #[test]
        fn a_lease_rounds_up_to_whole_slots() {
            let directory = std::env::temp_dir().join(format!(
                "microscope-ring-lease-round-{}",
                std::process::id()
            ));
            let pool = Pool::new(directory, 1000 * SLOTS).expect("a pool");
            let first = pool.take(1001, Wanted::Try, NONE).expect("two slots");
            let rest = pool.take(254_000, Wanted::Try, NONE).expect("254 left");
            assert!(pool.take(1, Wanted::Try, NONE).is_none(), "none left");
            drop((first, rest));
        }
    }
}

pub use imp::{Lease, take};
