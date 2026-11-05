//! Counts allocations for profiles and tests.
//!
//! A binary that wants its allocations reported installs [`Counting`]
//! around the allocator it uses. Each thread counts its own blocks where
//! every thread can sum them, and while a recording or a test measures
//! totals, the most the heap grew.

#![allow(unsafe_code, reason = "a global allocator is an unsafe trait")]

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

/// What has been allocated: by one thread, or by every thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Allocated {
    /// Blocks allocated, each growth of a block counting as one.
    pub blocks: u64,

    /// Bytes allocated, a block's growth counting only what it added.
    pub bytes: u64,
}

impl Allocated {
    /// What was allocated after `earlier`.
    #[must_use]
    pub const fn since(&self, earlier: &Self) -> Self {
        Self {
            blocks: self.blocks.saturating_sub(earlier.blocks),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }
}

/// Every thread's allocations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub allocated: Allocated,

    /// How much the heap grew, net of blocks freed, since the process
    /// began.
    pub live_bytes: i64,

    /// The most the heap had grown, while a measurement tracked it, to
    /// within [`PEAK_GRANULARITY`] for each thread.
    pub peak_bytes: i64,

    /// Threads that found every slot taken, and so contend on a shared one.
    pub sharing_threads: u64,
}

impl Totals {
    /// What was allocated after `earlier`, with the peak growth over it.
    #[must_use]
    pub const fn since(&self, earlier: &Self) -> Self {
        Self {
            allocated: self.allocated.since(&earlier.allocated),
            live_bytes: self.live_bytes.saturating_sub(earlier.live_bytes),
            peak_bytes: self.peak_bytes.saturating_sub(earlier.live_bytes),
            sharing_threads: self.sharing_threads.saturating_sub(earlier.sharing_threads),
        }
    }
}

/// What one thread has counted, in its own cell. Counts wrap past their
/// range: a panic in the allocator would abort the process, and a wrong
/// statistic is the lesser harm.
#[derive(Clone, Copy)]
struct ThreadCounts {
    allocated: Allocated,

    /// Bytes this thread allocated less those it freed; negative when it
    /// freed others' blocks.
    live: i64,

    /// Growth not yet added to the process's peak counter.
    pending: i64,

    slot: SlotUse,
}

/// Where a thread stores its counts for [`totals`].
#[derive(Clone, Copy)]
enum SlotUse {
    /// Not yet decided: the thread has not counted.
    Unassigned,

    /// `COUNTS[index]`, which earlier owners left holding their counts. The
    /// thread stores `base` plus its own counts, so theirs stay summed.
    Own { index: usize, base: Stored },

    /// [`SHARED`], when every slot was taken, while the thread claims one,
    /// or once it gave its own back as it exited.
    Shared,
}

/// A slot's counts, as one thread stores them or a new owner finds them.
#[derive(Clone, Copy)]
struct Stored {
    blocks: u64,
    bytes: u64,
    live: i64,
}

thread_local! {
    static THREAD: Cell<ThreadCounts> = const {
        Cell::new(ThreadCounts {
            allocated: Allocated { blocks: 0, bytes: 0 },
            live: 0,
            pending: 0,
            slot: SlotUse::Unassigned,
        })
    };

    /// Gives the thread's slot back as it exits. std registers its
    /// destructor with the system allocator rather than this one, and
    /// aborts if registering re-enters itself, so only [`claim`] touches it.
    static RELEASE: Release = const { Release };
}

struct Release;

impl Drop for Release {
    fn drop(&mut self) {
        let _ = THREAD.try_with(|cell| {
            let mut counts = cell.get();
            if let SlotUse::Own { index, .. } = counts.slot {
                // Blocks the thread frees from here on, as its other
                // thread-locals are destroyed, count in the shared slot.
                counts.slot = SlotUse::Shared;
                cell.set(counts);

                // Publishes the slot's counts to its next owner.
                CLAIMED[index].store(false, Ordering::Release);
            }
        });
    }
}

/// One thread's counts where every thread can read them, alone on its
/// cache line so that no other thread's writes contend with its own.
#[repr(align(64))]
struct Slot {
    blocks: AtomicU64,
    bytes: AtomicU64,
    live: AtomicI64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            blocks: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            live: AtomicI64::new(0),
        }
    }

    fn load(&self) -> Stored {
        Stored {
            blocks: self.blocks.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            live: self.live.load(Ordering::Relaxed),
        }
    }
}

/// How many threads at once count in slots of their own. Fewer under Miri,
/// where running past them takes a test seconds per hundred threads.
#[doc(hidden)]
pub const SLOTS: usize = if cfg!(miri) { 16 } else { 256 };

/// A slot for each thread that counts, while it runs, which it stores its
/// own totals in. Threads give their slots back as they exit; while every
/// one is taken, a new thread adds to the shared one instead.
static COUNTS: [Slot; SLOTS] = [const { Slot::new() }; SLOTS];
static CLAIMED: [AtomicBool; SLOTS] = [const { AtomicBool::new(false) }; SLOTS];
static SHARED: Slot = Slot::new();

/// How many threads have found every slot taken.
static SHARING: AtomicU64 = AtomicU64::new(0);

/// How far a thread's heap may grow or shrink before it adds the change to
/// the process's peak counter. The peak is exact to within this much for
/// each thread, and threads rarely contend on the counter.
pub const PEAK_GRANULARITY: i64 = 64 << 10;

/// Whether some binary counts its allocations with [`Counting`].
static INSTALLED: AtomicBool = AtomicBool::new(false);

/// How many measurements want the peak; tracking it costs threads a shared
/// counter, so it is tracked only while wanted.
static TOTALS_WANTED: AtomicU64 = AtomicU64::new(0);

/// The heap's growth as threads have reported it, and its most.
static REPORTED: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);

/// What this thread has allocated since it began, or zero when no
/// [`Counting`] allocator is installed.
#[must_use]
pub fn thread_allocated() -> Allocated {
    THREAD
        .try_with(|counts| counts.get().allocated)
        .unwrap_or_default()
}

/// Whether allocations are counted at all.
#[must_use]
pub fn installed() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

/// Starts tracking the heap's peak, until a matching [`stop_totals`].
/// Measurements may nest.
pub fn start_totals() {
    if TOTALS_WANTED.fetch_add(1, Ordering::AcqRel) == 0 {
        reset_peak();
    }
}

/// Stops one measurement's tracking of the peak.
pub fn stop_totals() {
    let _ = TOTALS_WANTED.try_update(Ordering::AcqRel, Ordering::Relaxed, |wanted| {
        wanted.checked_sub(1)
    });
}

/// Every thread's allocations so far, and the heap's peak while tracked.
#[must_use]
pub fn totals() -> Totals {
    let mut totals = Totals::default();

    for slot in COUNTS.iter().chain([&SHARED]) {
        let stored = slot.load();
        totals.allocated.blocks = totals.allocated.blocks.wrapping_add(stored.blocks);
        totals.allocated.bytes = totals.allocated.bytes.wrapping_add(stored.bytes);
        totals.live_bytes = totals.live_bytes.wrapping_add(stored.live);
    }

    totals.peak_bytes = PEAK.load(Ordering::Relaxed).max(totals.live_bytes);
    totals.sharing_threads = SHARING.load(Ordering::Relaxed);
    totals
}

/// What this thread has allocated, and its net growth of the heap, as its
/// slot holds them for [`totals`]: zero before its first count, and `None`
/// while it counts in the shared slot.
#[doc(hidden)]
#[must_use]
pub fn thread_counts() -> Option<(Allocated, i64)> {
    let slot = THREAD.try_with(|counts| counts.get().slot).ok()?;

    match slot {
        SlotUse::Unassigned => Some((Allocated::default(), 0)),
        SlotUse::Shared => None,
        SlotUse::Own { index, base } => {
            let stored = COUNTS[index].load();
            let allocated = Allocated {
                blocks: stored.blocks.wrapping_sub(base.blocks),
                bytes: stored.bytes.wrapping_sub(base.bytes),
            };

            Some((allocated, stored.live.wrapping_sub(base.live)))
        }
    }
}

/// Resets the peak to the heap's current growth, so that a measurement
/// sees only its own peak.
pub fn reset_peak() {
    let live = COUNTS.iter().chain([&SHARED]).fold(0_i64, |live, slot| {
        live.wrapping_add(slot.live.load(Ordering::Relaxed))
    });

    REPORTED.store(live, Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
}

/// Counts `bytes` allocated, in a new block when `new_block`, or `bytes`
/// freed when negative.
#[inline]
fn count(bytes: i64, new_block: bool) {
    let _ = THREAD.try_with(|cell| {
        if matches!(cell.get().slot, SlotUse::Unassigned) {
            claim(cell);
        }

        let mut counts = cell.get();
        if bytes > 0 {
            counts.allocated.blocks = counts.allocated.blocks.wrapping_add(u64::from(new_block));
            counts.allocated.bytes = counts.allocated.bytes.wrapping_add(bytes.unsigned_abs());
        }

        counts.live = counts.live.wrapping_add(bytes);

        match counts.slot {
            SlotUse::Own { index, base } => {
                // Only this thread writes its slot.
                let slot = &COUNTS[index];
                slot.blocks.store(
                    base.blocks.wrapping_add(counts.allocated.blocks),
                    Ordering::Relaxed,
                );
                slot.bytes.store(
                    base.bytes.wrapping_add(counts.allocated.bytes),
                    Ordering::Relaxed,
                );
                slot.live
                    .store(base.live.wrapping_add(counts.live), Ordering::Relaxed);
            }
            SlotUse::Unassigned | SlotUse::Shared => {
                debug_assert!(!matches!(counts.slot, SlotUse::Unassigned));
                if bytes > 0 {
                    SHARED
                        .blocks
                        .fetch_add(u64::from(new_block), Ordering::Relaxed);
                    SHARED
                        .bytes
                        .fetch_add(bytes.unsigned_abs(), Ordering::Relaxed);
                }

                SHARED.live.fetch_add(bytes, Ordering::Relaxed);
            }
        }

        if TOTALS_WANTED.load(Ordering::Relaxed) != 0 {
            counts.pending = counts.pending.wrapping_add(bytes);
            if counts.pending.unsigned_abs() >= PEAK_GRANULARITY.unsigned_abs() {
                let reported = REPORTED
                    .fetch_add(counts.pending, Ordering::Relaxed)
                    .saturating_add(counts.pending);
                PEAK.fetch_max(reported, Ordering::Relaxed);
                counts.pending = 0;
            }
        }

        cell.set(counts);
    });
}

/// Gives the thread a slot of its own, or the shared one when every slot is
/// taken. Any allocation made meanwhile counts in the shared slot and
/// cannot claim again.
#[cold]
fn claim(cell: &Cell<ThreadCounts>) {
    let mut counts = cell.get();
    counts.slot = SlotUse::Shared;
    cell.set(counts);

    // A thread already past its thread-locals' destruction could not give a
    // slot back.
    if RELEASE.try_with(|_| ()).is_err() {
        return;
    }

    let claimed = CLAIMED.iter().position(|claimed| {
        !claimed.load(Ordering::Relaxed)
            && claimed
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
    });

    let Some(index) = claimed else {
        SHARING.fetch_add(1, Ordering::Relaxed);
        return;
    };

    // What the thread counted so far is already in the shared slot, so the
    // base leaves it out of this one.
    let mut counts = cell.get();
    let found = COUNTS[index].load();
    let base = Stored {
        blocks: found.blocks.wrapping_sub(counts.allocated.blocks),
        bytes: found.bytes.wrapping_sub(counts.allocated.bytes),
        live: found.live.wrapping_sub(counts.live),
    };

    counts.slot = SlotUse::Own { index, base };
    cell.set(counts);
}

/// Counts a new block of `bytes`.
#[inline]
fn grew(bytes: usize, new_block: bool) {
    count(i64::try_from(bytes).unwrap_or(i64::MAX), new_block);
}

/// Counts `bytes` freed.
#[inline]
fn shrank(bytes: usize) {
    count(-i64::try_from(bytes).unwrap_or(i64::MAX), false);
}

/// Notes that a [`Counting`] allocator serves the process. Only the first
/// allocation stores, since a store to the shared flag on every one would
/// contend between threads.
#[inline]
fn installing() {
    if !INSTALLED.load(Ordering::Relaxed) {
        INSTALLED.store(true, Ordering::Relaxed);
    }
}

/// Wraps an allocator to count what each thread, and while wanted the
/// whole process, allocates. Counting allocates nothing itself.
pub struct Counting<A> {
    inner: A,
}

impl<A> Counting<A> {
    pub const fn new(inner: A) -> Self {
        Self { inner }
    }
}

/// The allocator microscope's programs count.
///
/// mimalloc serves the many small blocks reading debug information takes far
/// faster than the C library's; `system-alloc` selects the C library's
/// instead, for heap profilers that cannot see mimalloc.
#[cfg(not(feature = "system-alloc"))]
pub type Selected = mimalloc::MiMalloc;

#[cfg(feature = "system-alloc")]
pub type Selected = std::alloc::System;

/// Has the allocator return freed memory to the system within 10 ms.
///
/// mimalloc otherwise waits a second, and `MIMALLOC_PURGE_DELAY` still
/// overrides it. Loading a large program frees gigabytes it built along the way, which
/// otherwise stay resident through the rest of the load: some 2 GB of a
/// 9 GB peak for the largest measured.
///
/// # Safety
///
/// No other thread may run meanwhile: mimalloc stores the option without
/// synchronizing with threads that read it. A program calls this first,
/// before it starts any thread.
#[cfg_attr(
    feature = "system-alloc",
    expect(clippy::missing_const_for_fn, reason = "mimalloc's is not const")
)]
#[allow(unsafe_code, reason = "its caller must start no thread meanwhile")]
pub unsafe fn return_freed_memory_promptly() {
    #[cfg(not(feature = "system-alloc"))]
    {
        // mimalloc's `mi_option_purge_delay`, in its second and third
        // versions alike.
        const PURGE_DELAY: std::ffi::c_int = 15;

        #[allow(
            unsafe_code,
            reason = "mimalloc's options are set through its C interface"
        )]
        // SAFETY: the declaration matches mimalloc.h's `void
        // mi_option_set_default(mi_option_t option, long value)`, whose
        // enum is an int, and libmimalloc-sys links the library. It only
        // stores the default, which is not thread-safe: the caller
        // guarantees no other thread runs.
        unsafe {
            unsafe extern "C" {
                fn mi_option_set_default(option: std::ffi::c_int, value: std::ffi::c_long);
            }

            mi_option_set_default(PURGE_DELAY, 10);
        }
    }
}

/// The allocator a program installs, counted: `#[global_allocator] static
/// ALLOCATOR: Counting<Selected> = alloc::selected();`.
#[must_use]
pub const fn selected() -> Counting<Selected> {
    Counting::new(Selected {})
}

// SAFETY: every method forwards to `inner` with the caller's arguments and
// returns its result unchanged, so `inner` upholds the allocator contract.
// Counting only reads sizes and touches atomics and constant-initialized
// thread-locals, none of which unwinds. The one with a destructor registers
// it through the system allocator, not this one.
unsafe impl<A: GlobalAlloc> GlobalAlloc for Counting<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's layout satisfies `alloc`'s requirements.
        let block = unsafe { self.inner.alloc(layout) };
        if !block.is_null() {
            installing();
            grew(layout.size(), true);
        }

        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's layout satisfies `alloc_zeroed`'s requirements.
        let block = unsafe { self.inner.alloc_zeroed(layout) };
        if !block.is_null() {
            installing();
            grew(layout.size(), true);
        }

        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: the caller passes a block this allocator, and so `inner`,
        // returned, with the layout it was allocated with.
        unsafe { self.inner.dealloc(block, layout) };
        shrank(layout.size());
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller passes a block `inner` returned, its layout, and
        // a valid new size.
        let moved = unsafe { self.inner.realloc(block, layout, new_size) };

        // A failed reallocation leaves the old block as it was.
        if !moved.is_null() {
            match new_size.checked_sub(layout.size()) {
                Some(added) => grew(added, added > 0),
                None => shrank(layout.size() - new_size),
            }
        }

        moved
    }
}
