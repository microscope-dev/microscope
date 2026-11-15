use std::alloc::System;

use microscope::timings::alloc::{
    Allocated, Counting, SLOTS, installed, start_totals, stop_totals, thread_allocated,
    thread_counts, totals,
};

#[global_allocator]
static ALLOCATOR: Counting<System> = Counting::new(System);

const ON_EXIT: usize = 64;

const WHILE_RUNNING: usize = 1000;

struct AllocatesOnExit;

impl Drop for AllocatesOnExit {
    fn drop(&mut self) {
        drop(std::hint::black_box(vec![0_u8; ON_EXIT]));
    }
}

thread_local! {
    static EXITING: AllocatesOnExit = const { AllocatesOnExit };
}

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
