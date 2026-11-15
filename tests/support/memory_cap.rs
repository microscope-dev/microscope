#![allow(unsafe_code, reason = "a global allocator is an unsafe trait")]
#![cfg_attr(miri, allow(dead_code, reason = "Miri keeps its own allocator"))]

use std::alloc::{GlobalAlloc, Layout};
use std::io::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};

use mimalloc::MiMalloc;

use microscope::timings::alloc::Counting;

#[allow(
    unused_imports,
    reason = "only some test processes measure their allocations"
)]
pub use microscope::timings::alloc::{Allocated, start_totals, stop_totals, totals};

const CAP_BYTES: usize = 1 << 30;

#[allow(
    dead_code,
    reason = "only some test processes measure their allocations"
)]
pub fn allocated() -> Allocated {
    microscope::timings::alloc::thread_allocated()
}

struct CappedAllocator {
    live: AtomicUsize,
    inner: Counting<MiMalloc>,
}

impl CappedAllocator {
    fn reserve(&self, bytes: usize) -> bool {
        let reserved = self
            .live
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
                live.checked_add(bytes).filter(|&total| total <= CAP_BYTES)
            })
            .is_ok();

        if !reserved {
            let _ = std::io::stderr().write_all(
                b"an allocation exceeds the test memory cap (tests/support/memory_cap.rs)\n",
            );
        }

        reserved
    }

    fn release(&self, bytes: usize) {
        self.live.fetch_sub(bytes, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to the counting `MiMalloc` with the caller's arguments, so
// `MiMalloc` upholds the allocator contract; the cap only refuses some
// requests by returning null, which the contract allows, and counts only
// blocks `MiMalloc` actually returned or released.
unsafe impl GlobalAlloc for CappedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.reserve(layout.size()) {
            return std::ptr::null_mut();
        }

        // SAFETY: the caller's layout satisfies `alloc`'s requirements.
        let block = unsafe { self.inner.alloc(layout) };
        if block.is_null() {
            self.release(layout.size());
        }

        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !self.reserve(layout.size()) {
            return std::ptr::null_mut();
        }

        // SAFETY: the caller's layout satisfies `alloc_zeroed`'s requirements.
        let block = unsafe { self.inner.alloc_zeroed(layout) };
        if block.is_null() {
            self.release(layout.size());
        }

        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: the caller passes a block this allocator returned, with its
        // layout, and every such block came from `MiMalloc`.
        unsafe { self.inner.dealloc(block, layout) };
        self.release(layout.size());
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let growth = new_size.saturating_sub(layout.size());
        if !self.reserve(growth) {
            return std::ptr::null_mut();
        }

        // SAFETY: the caller passes a block `MiMalloc` returned, its layout,
        // and a valid new size.
        let moved = unsafe { self.inner.realloc(block, layout, new_size) };
        if moved.is_null() {
            self.release(growth);
        } else {
            self.release(layout.size().saturating_sub(new_size));
        }

        moved
    }
}

#[cfg(not(miri))]
#[global_allocator]
static ALLOCATOR: CappedAllocator = CappedAllocator {
    live: AtomicUsize::new(0),
    inner: Counting::new(MiMalloc),
};
