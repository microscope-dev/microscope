//! microscope's counting allocator serving a whole process, as it serves
//! microscope's own, which `just miri` runs under Miri as well as natively.
//! Here every allocation passes through it: the harness's, each thread's as
//! it starts, and each thread's as its thread-locals are destroyed, after
//! which the allocator must neither fail nor recurse.
//!
//! It is a test target of its own because the allocator it installs is the
//! C library's, which Miri serves through `malloc` and so does not check a
//! freed block's layout as it checks Rust's; `tests/miri.rs` keeps that
//! check for the rest.

use std::alloc::System;

use microscope::timings::alloc::{
    Allocated, Counting, SLOTS, installed, start_totals, stop_totals, thread_allocated,
    thread_counts, totals,
};

#[global_allocator]
static ALLOCATOR: Counting<System> = Counting::new(System);

/// What a thread allocates while its thread-locals are destroyed.
const ON_EXIT: usize = 64;

/// What each thread allocates while it runs.
const WHILE_RUNNING: usize = 1000;

/// Allocates as its thread exits, after the thread's other thread-locals
/// may have been destroyed.
struct AllocatesOnExit;

impl Drop for AllocatesOnExit {
    fn drop(&mut self) {
        drop(std::hint::black_box(vec![0_u8; ON_EXIT]));
    }
}

thread_local! {
    static EXITING: AllocatesOnExit = const { AllocatesOnExit };
}

/// Threads start, allocate, and exit at once, each counting its own blocks
/// exactly; the process's totals count every one, including those freed as
/// the threads exited.
#[test]
fn concurrent_threads_are_counted_from_start_to_exit() {
    const THREADS: usize = 3;
    start_totals();
    let before = totals();
    let threads: Vec<_> = (0..THREADS)
        .map(|_| {
            std::thread::spawn(|| {
                EXITING.with(|_| ());
                let start = thread_allocated();
                let block = std::hint::black_box(vec![0_u8; WHILE_RUNNING]);
                let counted = thread_allocated().since(&start);
                drop(block);
                counted
            })
        })
        .collect();

    for thread in threads {
        let counted = thread.join().expect("a counting thread");
        assert_eq!(
            counted,
            Allocated {
                blocks: 1,
                bytes: WHILE_RUNNING as u64
            }
        );
    }

    let counted = totals().since(&before).allocated;
    stop_totals();
    assert!(installed());
    assert!(
        counted.blocks >= 2 * THREADS as u64
            && counted.bytes >= (THREADS * (WHILE_RUNNING + ON_EXIT)) as u64,
        "the process's totals miss a thread's blocks: {counted:?}"
    );
}

/// Threads give their slots back as they exit: after more threads than
/// there are slots have come and gone, a new thread still counts in a slot
/// of its own, which holds only its own counts.
#[test]
fn exited_threads_give_their_slots_to_new_ones() {
    for _ in 0..=SLOTS {
        std::thread::spawn(|| drop(std::hint::black_box(vec![0_u8; 1])))
            .join()
            .expect("a counting thread");
    }

    let (counts, allocated) = std::thread::spawn(|| {
        let block = std::hint::black_box(vec![0_u8; WHILE_RUNNING]);
        let counts = (thread_counts(), thread_allocated());
        drop(block);
        counts
    })
    .join()
    .expect("the new thread");

    let (slot, _) = counts.expect("a slot of the new thread's own");
    assert_eq!(slot, allocated, "the slot holds another thread's counts");
}
