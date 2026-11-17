#![allow(unsafe_code, reason = "a global allocator is an unsafe trait")]
#![cfg_attr(miri, allow(dead_code, reason = "Miri keeps its own allocator"))]

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
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

#[allow(dead_code, reason = "only the cap's own test reads it")]
pub fn reserved() -> usize {
    LIVE.load(Ordering::Relaxed)
}

struct CappedAllocator {
    inner: Counting<MiMalloc>,
}

static LIVE: AtomicUsize = AtomicUsize::new(0);

const BATCH: usize = 64 << 10;

#[derive(Clone, Copy)]
enum Credit {
    Unclaimed,
    Shared,
    Held(usize),
}

thread_local! {
    static CREDIT: Cell<Credit> = const { Cell::new(Credit::Unclaimed) };

    static RETURN: ReturnCredit = const { ReturnCredit };
}

struct ReturnCredit;

impl Drop for ReturnCredit {
    fn drop(&mut self) {
        let _ = CREDIT.try_with(|cell| {
            if let Credit::Held(held) = cell.replace(Credit::Shared) {
                LIVE.fetch_sub(held, Ordering::Relaxed);
            }
        });
    }
}

#[cold]
fn claim(cell: &Cell<Credit>) -> Credit {
    cell.set(Credit::Shared);
    if RETURN.try_with(|_| ()).is_ok() {
        cell.set(Credit::Held(0));
    }

    cell.get()
}

fn reserve(bytes: usize) -> bool {
    let reserved = CREDIT
        .try_with(|cell| {
            let credit = match cell.get() {
                Credit::Unclaimed => claim(cell),
                credit => credit,
            };

            match credit {
                Credit::Held(held) if bytes < BATCH => match held.checked_sub(bytes) {
                    Some(rest) => {
                        cell.set(Credit::Held(rest));
                        true
                    }
                    None if reserve_shared(BATCH) => {
                        cell.set(Credit::Held(held + BATCH - bytes));
                        true
                    }
                    None => reserve_shared(bytes),
                },
                _ => reserve_shared(bytes),
            }
        })
        .unwrap_or_else(|_| reserve_shared(bytes));

    if !reserved {
        let _ = std::io::stderr().write_all(
            b"an allocation exceeds the test memory cap (tests/support/memory_cap.rs)\n",
        );
    }

    reserved
}

fn reserve_shared(bytes: usize) -> bool {
    LIVE.try_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
        live.checked_add(bytes).filter(|&total| total <= CAP_BYTES)
    })
    .is_ok()
}

fn release(bytes: usize) {
    let kept = CREDIT.try_with(|cell| match cell.get() {
        Credit::Held(held) if bytes < BATCH => {
            let held = held + bytes;
            if held > 2 * BATCH {
                LIVE.fetch_sub(held - BATCH, Ordering::Relaxed);
                cell.set(Credit::Held(BATCH));
            } else {
                cell.set(Credit::Held(held));
            }

            true
        }
        _ => false,
    });

    if kept != Ok(true) {
        LIVE.fetch_sub(bytes, Ordering::Relaxed);
    }
}

// SAFETY: every method forwards to the counting `MiMalloc` with the caller's arguments, so
// `MiMalloc` upholds the allocator contract; the cap only refuses some
// requests by returning null, which the contract allows, and counts only
// blocks `MiMalloc` actually returned or released.
unsafe impl GlobalAlloc for CappedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !reserve(layout.size()) {
            return std::ptr::null_mut();
        }

        // SAFETY: the caller's layout satisfies `alloc`'s requirements.
        let block = unsafe { self.inner.alloc(layout) };
        if block.is_null() {
            release(layout.size());
        }

        block
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !reserve(layout.size()) {
            return std::ptr::null_mut();
        }

        // SAFETY: the caller's layout satisfies `alloc_zeroed`'s requirements.
        let block = unsafe { self.inner.alloc_zeroed(layout) };
        if block.is_null() {
            release(layout.size());
        }

        block
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: the caller passes a block this allocator returned, with its
        // layout, and every such block came from `MiMalloc`.
        unsafe { self.inner.dealloc(block, layout) };
        release(layout.size());
    }

    unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let growth = new_size.saturating_sub(layout.size());
        if !reserve(growth) {
            return std::ptr::null_mut();
        }

        // SAFETY: the caller passes a block `MiMalloc` returned, its layout,
        // and a valid new size.
        let moved = unsafe { self.inner.realloc(block, layout, new_size) };
        if moved.is_null() {
            release(growth);
        } else {
            release(layout.size().saturating_sub(new_size));
        }

        moved
    }
}

#[cfg(not(miri))]
#[global_allocator]
static ALLOCATOR: CappedAllocator = CappedAllocator {
    inner: Counting::new(MiMalloc),
};
