#![allow(unsafe_code, reason = "a global allocator is an unsafe trait")]

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Allocated {
    pub blocks: u64,

    pub bytes: u64,
}

impl Allocated {
    #[must_use]
    pub const fn since(&self, earlier: &Self) -> Self {
        Self {
            blocks: self.blocks.saturating_sub(earlier.blocks),
            bytes: self.bytes.saturating_sub(earlier.bytes),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub allocated: Allocated,

    pub live_bytes: i64,

    pub peak_bytes: i64,

    pub sharing_threads: u64,
}

impl Totals {
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

#[derive(Clone, Copy)]
struct ThreadCounts {
    allocated: Allocated,

    live: i64,

    pending: i64,

    slot: SlotUse,
}

#[derive(Clone, Copy)]
enum SlotUse {
    Unassigned,

    Own { index: usize, base: Stored },

    Shared,
}

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
                counts.slot = SlotUse::Shared;
                cell.set(counts);

                slot_at(index).1.store(false, Ordering::Release);
            }
        });
    }
}

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

#[doc(hidden)]
pub const SLOTS: usize = if cfg!(miri) { 16 } else { 256 };

static COUNTS: [Slot; SLOTS] = [const { Slot::new() }; SLOTS];
static CLAIMED: [AtomicBool; SLOTS] = [const { AtomicBool::new(false) }; SLOTS];
static SHARED: Slot = Slot::new();

#[expect(
    clippy::indexing_slicing,
    reason = "every index is a position in CLAIMED, which is as long as COUNTS"
)]
fn slot_at(index: usize) -> (&'static Slot, &'static AtomicBool) {
    debug_assert!(index < SLOTS, "slot index past SLOTS");
    (&COUNTS[index], &CLAIMED[index])
}

static SHARING: AtomicU64 = AtomicU64::new(0);

pub const PEAK_GRANULARITY: i64 = 64 << 10;

static INSTALLED: AtomicBool = AtomicBool::new(false);

static TOTALS_WANTED: AtomicU64 = AtomicU64::new(0);

static REPORTED: AtomicI64 = AtomicI64::new(0);
static PEAK: AtomicI64 = AtomicI64::new(0);

#[must_use]
pub fn thread_allocated() -> Allocated {
    THREAD
        .try_with(|counts| counts.get().allocated)
        .unwrap_or_default()
}

#[must_use]
pub fn installed() -> bool {
    INSTALLED.load(Ordering::Relaxed)
}

pub fn start_totals() {
    if TOTALS_WANTED.fetch_add(1, Ordering::AcqRel) == 0 {
        reset_peak();
    }
}

pub fn stop_totals() {
    let _ = TOTALS_WANTED.try_update(Ordering::AcqRel, Ordering::Relaxed, |wanted| {
        wanted.checked_sub(1)
    });
}

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

#[doc(hidden)]
#[must_use]
pub fn thread_counts() -> Option<(Allocated, i64)> {
    let slot = THREAD.try_with(|counts| counts.get().slot).ok()?;

    match slot {
        SlotUse::Unassigned => Some((Allocated::default(), 0)),
        SlotUse::Shared => None,
        SlotUse::Own { index, base } => {
            let stored = slot_at(index).0.load();
            let allocated = Allocated {
                blocks: stored.blocks.wrapping_sub(base.blocks),
                bytes: stored.bytes.wrapping_sub(base.bytes),
            };

            Some((allocated, stored.live.wrapping_sub(base.live)))
        }
    }
}

pub fn reset_peak() {
    let live = COUNTS.iter().chain([&SHARED]).fold(0_i64, |live, slot| {
        live.wrapping_add(slot.live.load(Ordering::Relaxed))
    });

    REPORTED.store(live, Ordering::Relaxed);
    PEAK.store(live, Ordering::Relaxed);
}

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
                let slot = slot_at(index).0;
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

#[cold]
fn claim(cell: &Cell<ThreadCounts>) {
    let mut counts = cell.get();
    counts.slot = SlotUse::Shared;
    cell.set(counts);

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

    let mut counts = cell.get();
    let found = slot_at(index).0.load();
    let base = Stored {
        blocks: found.blocks.wrapping_sub(counts.allocated.blocks),
        bytes: found.bytes.wrapping_sub(counts.allocated.bytes),
        live: found.live.wrapping_sub(counts.live),
    };

    counts.slot = SlotUse::Own { index, base };
    cell.set(counts);
}

#[inline]
fn grew(bytes: usize, new_block: bool) {
    count(i64::try_from(bytes).unwrap_or(i64::MAX), new_block);
}

#[inline]
fn shrank(bytes: usize) {
    count(-i64::try_from(bytes).unwrap_or(i64::MAX), false);
}

#[inline]
fn installing() {
    if !INSTALLED.load(Ordering::Relaxed) {
        INSTALLED.store(true, Ordering::Relaxed);
    }
}

pub struct Counting<A> {
    inner: A,
}

impl<A> Counting<A> {
    pub const fn new(inner: A) -> Self {
        Self { inner }
    }
}

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

        if !moved.is_null() {
            match new_size.checked_sub(layout.size()) {
                Some(added) => grew(added, added > 0),
                None => shrank(layout.size() - new_size),
            }
        }

        moved
    }
}
