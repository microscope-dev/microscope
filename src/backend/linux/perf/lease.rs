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
//! The locks are POSIX record locks, which belong to a process and which
//! a child does not inherit. A launch's child waits at its gate before it
//! executes, with a copy of every descriptor open when it forked, until
//! its parent has leased its rings: a lock the child could keep would make
//! two launches at once each wait for the other's child.
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
    use std::path::Path;
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};
    use std::time::{Duration, Instant};

    use nix::fcntl::{FcntlArg, fcntl};
    use nix::libc;

    use super::Wanted;

    /// The pool is cut in this many slots, so that a lease of any size
    /// rounds up to whole slots and leases never sum past the pool.
    const SLOTS: usize = 256;

    /// How long opening waits at the fewest pages before it goes on without
    /// a lease, so that a process holding one while it waits for another
    /// cannot wait for good.
    const MOST_WAIT: Duration = Duration::from_secs(20);

    const RETRY: Duration = Duration::from_millis(20);

    /// Slots of ring memory, each a lock on its slot file, held until
    /// dropped: none without a pool.
    #[derive(Debug)]
    pub struct Lease {
        files: Option<Arc<Files>>,
        slots: Vec<usize>,
    }

    /// A pool of `unit` bytes a slot.
    #[derive(Debug)]
    pub struct Pool {
        files: Arc<Files>,
        unit: usize,
    }

    /// The pool's lock files, open while it is. Closing any descriptor of
    /// a file lets go of every lock this process holds on it, so a process
    /// opens each once, and only one pool a directory.
    #[derive(Debug)]
    struct Files {
        /// Locked while a process picks its slots, so that two never each
        /// hold part of what both need.
        taking: File,

        /// Each slot's file, and whether this process holds it. Its locks
        /// keep other processes out, but not its own threads, which keep
        /// apart here.
        slots: Mutex<Vec<(File, bool)>>,
    }

    pub fn take(bytes: usize, wanted: Wanted) -> Option<Lease> {
        static POOL: OnceLock<Option<Pool>> = OnceLock::new();
        let pool = POOL.get_or_init(|| {
            let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/ring-lease");
            let pool = Pool::new(&directory, user_ring_bytes()?);
            #[cfg_attr(
                not(debug_assertions),
                expect(unused_variables, reason = "only recorded")
            )]
            if let Err(error) = &pool {
                record!("perf: no ring lease pool: {error}");
            }
            pool.ok()
        });

        let nothing = || Lease {
            files: None,
            slots: Vec::new(),
        };
        let Some(pool) = pool else {
            return Some(nothing());
        };
        let lease = pool.take(bytes, wanted, MOST_WAIT);
        if lease.is_none() && wanted == Wanted::Wait {
            // The kernel decides.
            record!("perf: no ring lease of {bytes} bytes after {MOST_WAIT:?}");
            return Some(nothing());
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
        pub fn new(directory: &Path, bytes: usize) -> std::io::Result<Self> {
            std::fs::create_dir_all(directory)?;
            let slots = (0..SLOTS)
                .map(|slot| {
                    File::create(directory.join(format!("slot-{slot:03}")))
                        .map(|file| (file, false))
                })
                .collect::<std::io::Result<_>>()?;
            let files = Files {
                taking: File::create(directory.join("taking"))?,
                slots: Mutex::new(slots),
            };

            Ok(Self {
                files: Arc::new(files),
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
                    return Some(Lease {
                        files: Some(Arc::clone(&self.files)),
                        slots,
                    });
                }
                if wanted == Wanted::Try || Instant::now() >= deadline {
                    return None;
                }

                std::thread::sleep(RETRY);
            }
        }

        /// `needed` free slots, taken together under the pool's own lock
        /// so that two takers never each hold part of what both need.
        fn try_take(&self, needed: usize) -> std::io::Result<Option<Vec<usize>>> {
            let files = &*self.files;
            let mut slots = files.slots.lock().unwrap_or_else(PoisonError::into_inner);
            lock(&files.taking, libc::F_WRLCK, true)?;
            let mut taken = Vec::with_capacity(needed);
            for (slot, (file, held)) in slots.iter_mut().enumerate() {
                if taken.len() == needed {
                    break;
                }

                if !*held && lock(file, libc::F_WRLCK, false).is_ok() {
                    *held = true;
                    taken.push(slot);
                }
            }

            let enough = taken.len() == needed;
            if !enough {
                let_go(&mut slots, &taken);
            }

            drop(slots);
            lock(&files.taking, libc::F_UNLCK, false)?;
            Ok(enough.then_some(taken))
        }
    }

    impl Drop for Lease {
        fn drop(&mut self) {
            if let Some(files) = &self.files {
                let_go(
                    &mut files.slots.lock().unwrap_or_else(PoisonError::into_inner),
                    &self.slots,
                );
            }
        }
    }

    /// Unlocks each of `taken` among `slots`.
    fn let_go(slots: &mut [(File, bool)], taken: &[usize]) {
        for &slot in taken {
            if let Some((file, held)) = slots.get_mut(slot) {
                let _ = lock(file, libc::F_UNLCK, false);
                *held = false;
            }
        }
    }

    /// Locks, or with `F_UNLCK` unlocks, the whole of `file` for this
    /// process, waiting for another to let go if `wait`.
    fn lock(file: &File, kind: libc::c_int, wait: bool) -> nix::Result<()> {
        let whole = libc::flock {
            l_type: i16::try_from(kind).unwrap_or(i16::MAX),
            l_whence: i16::try_from(libc::SEEK_SET).unwrap_or(0),
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };
        let arg = if wait {
            FcntlArg::F_SETLKW(&whole)
        } else {
            FcntlArg::F_SETLK(&whole)
        };

        fcntl(file, arg).map(drop)
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
            Pool::new(&directory, SLOTS).expect("a pool")
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

        /// The pool a run of this test executable by
        /// [`another_process_is_refused_a_lease_until_it_is_let_go`] takes
        /// its lease from.
        const OTHER_POOL: &str = "MICROSCOPE_TEST_LEASE_POOL";

        /// Another test process is refused what this one holds, and given
        /// it once this one lets go.
        #[test]
        fn another_process_is_refused_a_lease_until_it_is_let_go() {
            if let Some(directory) = std::env::var_os(OTHER_POOL) {
                let pool = Pool::new(Path::new(&directory), SLOTS).expect("the pool");
                let taken = pool.take(1, Wanted::Try, NONE).is_some();
                println!("taken: {taken}");
                return;
            }

            let pool = pool("processes");
            let directory = std::env::temp_dir().join(format!(
                "microscope-ring-lease-processes-{}",
                std::process::id()
            ));
            let other = || {
                let output = std::process::Command::new(
                    std::env::current_exe().expect("test executable"),
                )
                .args([
                    "--exact",
                    "backend::linux::perf::lease::imp::tests::another_process_is_refused_a_lease_until_it_is_let_go",
                    "--nocapture",
                ])
                .env(OTHER_POOL, &directory)
                .output()
                .expect("run the test executable");
                assert!(output.status.success(), "{output:?}");
                String::from_utf8_lossy(&output.stdout).contains("taken: true")
            };

            let held = pool.take(SLOTS, Wanted::Try, NONE).expect("all of it");
            assert!(!other(), "another process took a slot this one holds");
            drop(held);
            assert!(other(), "another process was refused a free slot");
        }

        /// A lease let go is free again while a child forked as it was held
        /// still waits at its gate. A launch forks with every descriptor
        /// this process has open, and its child execs only once its parent
        /// has taken a lease of its own: if the child kept another's lease,
        /// two launches at once would each wait for the other's child.
        #[test]
        fn a_lease_let_go_is_free_while_a_child_forked_holding_it_waits() {
            use std::io::Read as _;
            use std::os::fd::AsRawFd as _;

            use crate::backend::linux::sys::{BeforeExec, Gate, before_exec};

            let pool = pool("forked");
            let held = pool.take(SLOTS, Wanted::Try, NONE).expect("all of it");

            let pipe = || nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).expect("a pipe");
            let ((pid_read, pid_write), (go_read, go_write)) = (pipe(), pipe());
            let mut command = std::process::Command::new("true");
            before_exec(
                &mut command,
                BeforeExec {
                    gate: Some(Gate {
                        pid: pid_write.as_raw_fd(),
                        go_read: go_read.as_raw_fd(),
                        go_write: go_write.as_raw_fd(),
                    }),
                    ..BeforeExec::default()
                },
            );

            let (again, status) = std::thread::scope(|scope| {
                // Spawning returns once the child executes.
                let child = scope.spawn(move || command.spawn()?.wait());
                let mut pid = [0_u8; 4];
                std::fs::File::from(pid_read)
                    .read_exact(&mut pid)
                    .expect("the child waits at its gate");

                drop(held);
                let again = pool.take(SLOTS, Wanted::Try, NONE);
                nix::unistd::write(&go_write, &[1]).expect("open the gate");
                (again, child.join().expect("the spawner"))
            });
            drop((pid_write, go_read, go_write));

            assert!(status.expect("the child ran").success());
            assert!(
                again.is_some(),
                "the child forked holding the lease kept it"
            );
        }

        /// A lease is a byte count rounded up to whole slots of the pool.
        #[test]
        fn a_lease_rounds_up_to_whole_slots() {
            let directory = std::env::temp_dir().join(format!(
                "microscope-ring-lease-round-{}",
                std::process::id()
            ));
            let pool = Pool::new(&directory, 1000 * SLOTS).expect("a pool");
            let first = pool.take(1001, Wanted::Try, NONE).expect("two slots");
            let rest = pool.take(254_000, Wanted::Try, NONE).expect("254 left");
            assert!(pool.take(1, Wanted::Try, NONE).is_none(), "none left");
            drop((first, rest));
        }
    }
}

pub use imp::{Lease, take};
