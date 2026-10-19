//! The tests of microscope's unsafe code, which `just miri` runs under Miri
//! as well as natively. They are a test target of their own so that Miri
//! checks only this file, not the whole crate, each time it runs them.

#![allow(unsafe_code, reason = "these tests drive unsafe code directly")]

mod allocator {
    use std::alloc::{GlobalAlloc, Layout};
    use std::sync::atomic::{AtomicPtr, Ordering};

    use microscope::timings::alloc::{
        Allocated, Counting, PEAK_GRANULARITY, start_totals, stop_totals, thread_counts, totals,
    };

    /// Where the tests' blocks come from: the system's allocator; under
    /// Miri, Rust's, which Miri serves itself and so checks that every
    /// block is freed with the layout it was allocated with, which the
    /// system's `free` would not.
    #[cfg(not(miri))]
    type Blocks = std::alloc::System;

    #[cfg(miri)]
    type Blocks = Rust;

    /// Rust's global allocator.
    #[cfg(miri)]
    struct Rust;

    // SAFETY: every method forwards to the global allocator with the
    // caller's arguments.
    #[cfg(miri)]
    unsafe impl GlobalAlloc for Rust {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout is valid for `alloc`.
            unsafe { std::alloc::alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout is valid for `alloc_zeroed`.
            unsafe { std::alloc::alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
            // SAFETY: the caller passes a block the global allocator
            // returned, with its layout.
            unsafe { std::alloc::dealloc(block, layout) }
        }

        unsafe fn realloc(&self, block: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            // SAFETY: the caller passes a block the global allocator
            // returned, its layout, and a valid new size.
            unsafe { std::alloc::realloc(block, layout, new_size) }
        }
    }

    /// Allocates from [`Blocks`], but refuses to grow any block.
    struct NoGrowth;

    // SAFETY: allocation forwards to `Blocks`; refusing a reallocation by
    // returning null is allowed and leaves the block valid.
    unsafe impl GlobalAlloc for NoGrowth {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: the caller's layout is valid for `alloc`.
            unsafe { Blocks {}.alloc(layout) }
        }

        unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
            // SAFETY: the caller passes a block `Blocks` returned.
            unsafe { Blocks {}.dealloc(block, layout) }
        }

        unsafe fn realloc(&self, _: *mut u8, _: Layout, _: usize) -> *mut u8 {
            std::ptr::null_mut()
        }
    }

    /// Refuses every request.
    struct Refusing;

    // SAFETY: refusing an allocation by returning null is allowed, and no
    // block it returned is ever freed, since it returns none.
    unsafe impl GlobalAlloc for Refusing {
        unsafe fn alloc(&self, _: Layout) -> *mut u8 {
            std::ptr::null_mut()
        }

        unsafe fn alloc_zeroed(&self, _: Layout) -> *mut u8 {
            std::ptr::null_mut()
        }

        unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
    }

    #[test]
    fn a_refused_allocation_counts_nothing() {
        let allocator = Counting::new(Refusing);
        let layout = Layout::from_size_align(4096, 8).expect("a layout");
        let before = own();

        // SAFETY: the layout is non-zero, and no block comes back to free.
        let (block, zeroed) = unsafe { (allocator.alloc(layout), allocator.alloc_zeroed(layout)) };
        assert!(block.is_null() && zeroed.is_null());
        assert_eq!(own(), before);
    }

    /// This thread's counts, from its slot. Other threads of a test
    /// process, such as the harness's, may allocate while a test measures,
    /// and their counts can reach it late, so tests read their own threads'
    /// counts exactly and the process's totals only as bounds.
    fn own() -> (Allocated, i64) {
        thread_counts().expect("a slot of the thread's own")
    }

    #[test]
    fn totals_follow_blocks_across_threads_and_failed_growth() {
        let allocator = Counting::new(NoGrowth);

        // A block that moves the peak by more than a thread's step, so the
        // harness's own allocations cannot hide it.
        let size = 2 * usize::try_from(PEAK_GRANULARITY).expect("a small size");
        let bytes = u64::try_from(size).expect("a small size");
        let layout = Layout::from_size_align(size, 8).expect("a layout");
        start_totals();
        let before = totals();

        // Another thread allocates while this one measures the process's
        // totals, as the test harness's own threads may.
        let other_layout = Layout::from_size_align(48, 8).expect("a layout");
        let other = std::thread::scope(|scope| {
            scope
                // SAFETY: the layout is non-zero; the block is freed below.
                .spawn(|| AtomicPtr::new(unsafe { allocator.alloc(other_layout) }))
                .join()
                .expect("the other thread")
        });
        let (allocated, live) = own();

        // SAFETY: the layout is non-zero; the block is freed below.
        let block = unsafe { allocator.alloc(layout) };
        assert!(!block.is_null());
        let (grown, grown_live) = own();
        assert_eq!(grown.since(&allocated), Allocated { blocks: 1, bytes });
        assert_eq!(grown_live - live, 2 * PEAK_GRANULARITY);
        let total = totals().since(&before).allocated;
        assert!(
            total.blocks >= 2 && total.bytes >= bytes + 48,
            "the process's totals miss a block: {total:?}"
        );

        // SAFETY: the block came from `allocator` with `layout`.
        let refused = unsafe { allocator.realloc(block, layout, 2 * size) };
        assert!(refused.is_null());
        assert_eq!(
            own(),
            (grown, grown_live),
            "a failed growth allocates nothing"
        );

        // Another thread frees the block, and the heap shrinks all the
        // same.
        let block = AtomicPtr::new(block);
        let (start, end) = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let start = own();

                    // SAFETY: the block came from `allocator` with `layout`
                    // and is freed once.
                    unsafe { allocator.dealloc(block.load(Ordering::Relaxed), layout) };
                    (start, own())
                })
                .join()
                .expect("the freeing thread")
        });
        let after = totals();
        stop_totals();
        assert_eq!(end.0, start.0, "freeing allocates nothing");
        assert_eq!(end.1 - start.1, -2 * PEAK_GRANULARITY);
        assert!(
            after.since(&before).peak_bytes >= PEAK_GRANULARITY,
            "{after:?}"
        );

        // SAFETY: the other thread's block came from `allocator` with
        // `other_layout` and is freed once.
        unsafe { allocator.dealloc(other.load(Ordering::Relaxed), other_layout) };
    }

    #[test]
    fn blocks_keep_their_contents_and_counts_as_they_grow_and_shrink() {
        let allocator = Counting::new(Blocks {});
        let pattern: [u8; 16] = *b"0123456789abcdef";
        let layout = |size| Layout::from_size_align(size, 8).expect("a layout");
        let start = own();

        // SAFETY: the layout is non-zero; the block is freed below.
        let block = unsafe { allocator.alloc_zeroed(layout(16)) };
        assert!(!block.is_null());
        let zeroed = own();

        // SAFETY: the block holds 16 initialized bytes that nothing else
        // refers to.
        let contents = unsafe { std::slice::from_raw_parts_mut(block, 16) };
        assert_eq!(contents, [0; 16]);
        contents.copy_from_slice(&pattern);

        // SAFETY: the block came from `allocator` with this layout.
        let block = unsafe { allocator.realloc(block, layout(16), 64) };
        assert!(!block.is_null());
        let grown = own();

        // SAFETY: growth kept the block's first 16 bytes, which nothing
        // else refers to.
        let contents = unsafe { std::slice::from_raw_parts(block, 16) };
        assert_eq!(contents, pattern);

        // SAFETY: the block came from `allocator` with this layout.
        let block = unsafe { allocator.realloc(block, layout(64), 8) };
        assert!(!block.is_null());
        let shrunk = own();

        // SAFETY: shrinking kept the block's first 8 bytes.
        let contents = unsafe { std::slice::from_raw_parts(block, 8) };
        assert_eq!(contents, &pattern[..8]);

        // SAFETY: the block came from `allocator` with this layout.
        unsafe { allocator.dealloc(block, layout(8)) };
        let freed = own();

        let counted =
            |(allocated, live): (Allocated, i64)| (allocated.since(&start.0), live - start.1);
        let allocated = |blocks, bytes| Allocated { blocks, bytes };
        assert_eq!(counted(zeroed), (allocated(1, 16), 16));
        assert_eq!(
            counted(grown),
            (allocated(2, 64), 64),
            "a growth counts as a block of what it added"
        );
        assert_eq!(counted(shrunk), (allocated(2, 64), 8));
        assert_eq!(counted(freed), (allocated(2, 64), 0));
    }
}

mod ring {
    use std::ptr::NonNull;
    use std::sync::atomic::{AtomicU64, Ordering};

    use microscope::unsafe_parts::{DATA_HEAD, DATA_TAIL, HEAP_DATA, RingMemory};

    /// What the kernel does to a ring: writes records past the head, then
    /// publishes the head, and reuses only bytes the tail has passed.
    struct Kernel {
        base: NonNull<u8>,
        size: u64,
        head: u64,
    }

    // SAFETY: the kernel's side writes only data the reader's tail has
    // passed, and the metadata words atomically, as the kernel does.
    unsafe impl Send for Kernel {}

    impl Kernel {
        const fn word(&self, offset: usize) -> &AtomicU64 {
            // SAFETY: the word lies in the ring's metadata, aligned, for
            // the ring's life, and both sides access it atomically.
            unsafe { AtomicU64::from_ptr(self.base.as_ptr().add(offset).cast()) }
        }

        fn tail(&self) -> u64 {
            self.word(DATA_TAIL).load(Ordering::Acquire)
        }

        /// Writes `bytes` past the head, wrapping, unpublished.
        fn write(&mut self, bytes: &[u8]) {
            assert!(self.head + bytes.len() as u64 - self.tail() <= self.size);

            for &byte in bytes {
                let at = HEAP_DATA + usize::try_from(self.head % self.size).expect("small");

                // SAFETY: the byte lies in the ring's data, which the
                // reader does not read until the head is published.
                unsafe { self.base.as_ptr().add(at).write(byte) };
                self.head += 1;
            }
        }

        fn publish(&self) {
            self.word(DATA_HEAD).store(self.head, Ordering::Release);
        }
    }

    fn ring(size: usize) -> (RingMemory, Kernel) {
        let (memory, base) = RingMemory::heap(size);
        let kernel = Kernel {
            base,
            size: size as u64,
            head: 0,
        };

        (memory, kernel)
    }

    fn bytes(from: u8, count: u8) -> Vec<u8> {
        (from..from + count).collect()
    }

    #[test]
    fn an_empty_ring_drains_nothing_and_unpublished_records_wait() {
        let (mut memory, mut kernel) = ring(64);
        let mut out = vec![7];
        assert_eq!(memory.drain(&mut out), 0);
        kernel.write(&bytes(0, 16));
        assert_eq!(memory.drain(&mut out), 0, "the head is not yet published");
        assert_eq!(out, [7]);
        assert_eq!(kernel.tail(), 0);
    }

    #[test]
    fn records_that_wrap_follow_what_was_drained_before() {
        let (mut memory, mut kernel) = ring(64);
        let mut out = Vec::new();
        kernel.write(&bytes(0, 48));
        kernel.publish();
        assert_eq!(memory.drain(&mut out), 48);
        assert_eq!(kernel.tail(), 48);

        // 16 bytes to the end, then 24 from the start.
        kernel.write(&bytes(48, 40));
        kernel.publish();
        assert_eq!(memory.drain(&mut out), 40);
        assert_eq!(out, bytes(0, 88));
        assert_eq!(kernel.tail(), 88);
    }

    #[test]
    fn a_full_ring_drains_whole() {
        let (mut memory, mut kernel) = ring(64);
        kernel.write(&bytes(0, 8));
        kernel.publish();
        let mut out = Vec::new();
        memory.drain(&mut out);
        kernel.write(&bytes(8, 64));
        kernel.publish();
        assert_eq!(memory.drain(&mut out), 64);
        assert_eq!(out, bytes(0, 72));
    }

    #[test]
    fn a_span_that_ends_at_the_rings_end_drains_in_one_part() {
        let (mut memory, mut kernel) = ring(64);
        let mut out = Vec::new();
        kernel.write(&bytes(0, 48));
        kernel.publish();
        memory.drain(&mut out);
        kernel.write(&bytes(48, 16));
        kernel.publish();
        assert_eq!(memory.drain(&mut out), 16);
        kernel.write(&bytes(64, 64));
        kernel.publish();
        assert_eq!(memory.drain(&mut out), 64);
        assert_eq!(out, bytes(0, 128));
        assert_eq!(memory.untrusted(), 0);
    }

    /// A head further ahead than the ring holds cannot be trusted: the
    /// ring reads nothing, skips its tail to the head so that the kernel
    /// can go on writing, and counts the span it skipped.
    #[test]
    fn a_head_past_a_whole_ring_is_skipped_and_counted() {
        let (mut memory, mut kernel) = ring(64);
        kernel.word(DATA_HEAD).store(65, Ordering::Release);
        let mut out = Vec::new();
        assert_eq!(memory.drain(&mut out), 0);
        assert!(out.is_empty());
        assert_eq!(kernel.tail(), 65);
        assert_eq!(memory.untrusted(), 1);

        kernel.head = 65;
        kernel.write(&bytes(0, 8));
        kernel.publish();
        assert_eq!(memory.drain(&mut out), 8);
        assert_eq!(out, bytes(0, 8));
        assert_eq!(memory.untrusted(), 1, "a trusted head counts nothing");
    }

    /// A tail past the head is not one the reader wrote: the same.
    #[test]
    fn a_tail_past_the_head_is_skipped_and_counted() {
        let (mut memory, kernel) = ring(64);
        kernel.word(DATA_HEAD).store(5, Ordering::Release);
        kernel.word(DATA_TAIL).store(10, Ordering::Release);
        let mut out = Vec::new();
        assert_eq!(memory.drain(&mut out), 0);
        assert!(out.is_empty());
        assert_eq!(kernel.tail(), 5);
        assert_eq!(memory.untrusted(), 1);
    }

    /// The kernel writes as the ring drains: every record arrives once,
    /// whole, in order. Run with many seeds, Miri tries the orderings the
    /// memory model allows.
    #[test]
    fn concurrent_records_arrive_once_whole_and_in_order() {
        const RECORDS: u8 = 24;
        let (mut memory, mut kernel) = ring(32);
        let writer = std::thread::spawn(move || {
            for record in 0..RECORDS {
                while kernel.head + 8 - kernel.tail() > kernel.size {
                    std::thread::yield_now();
                }

                kernel.write(&[record; 8]);
                kernel.publish();
            }
        });

        let mut out = Vec::new();
        while out.len() < usize::from(RECORDS) * 8 {
            if memory.drain(&mut out) == 0 {
                std::thread::yield_now();
            }
        }

        writer.join().expect("the kernel's side");
        let expected: Vec<u8> = (0..RECORDS).flat_map(|record| [record; 8]).collect();
        assert_eq!(out, expected);
    }
}
