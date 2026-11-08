//! A program whose allocator records into the SDK, as a tracking allocator
//! or a traced `malloc` does, re-enters it while the SDK allocates. The
//! program must neither hang nor crash.
#![cfg(feature = "enabled")]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::Duration;

thread_local! {
    /// What the allocator calls on this thread before each allocation.
    static HOOK: Cell<Option<fn()>> = const { Cell::new(None) };
}

struct Recording;

// SAFETY: every allocation is the system allocator's.
unsafe impl GlobalAlloc for Recording {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if let Ok(Some(hook)) = HOOK.try_with(Cell::get) {
            hook();
        }
        // SAFETY: the caller's layout, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, block: *mut u8, layout: Layout) {
        // SAFETY: the block came from `alloc`, which is the system's.
        unsafe { System.dealloc(block, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Recording = Recording;

/// Runs `run` on a new thread whose allocator calls `hook`, and fails if
/// it doesn't finish.
fn reentering(what: &str, hook: fn(), run: fn()) {
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        HOOK.set(Some(hook));
        run();
        HOOK.set(None);
        let _ = done.send(());
    });
    assert!(
        finished.recv_timeout(Duration::from_secs(5)).is_ok(),
        "{what}: the program hung re-entering the SDK"
    );
}

/// One test, since the steps share the process's one arming.
#[test]
fn an_allocator_that_records_never_hangs_or_crashes_the_program() {
    // SAFETY: the only test in this binary sets it before any thread reads
    // the environment.
    unsafe {
        std::env::set_var(
            microscope_probes::abi::RECORDER,
            std::os::unix::process::parent_id().to_string(),
        );
    }

    // Arming reads the environment, which allocates.
    reentering(
        "arming",
        || {
            let _ = microscope_probes::listening();
        },
        || {
            let _ = microscope_probes::listening();
        },
    );
    assert!(microscope_probes::listening(), "the recording armed it");

    // Defining a site copies its texts while it holds the definitions.
    reentering(
        "defining",
        || {
            let _ = microscope_probes::define(1, "inner", "reentry.rs", 1);
        },
        || {
            let site = microscope_probes::define(1, "outer", "reentry.rs", 1);
            assert!(site.is_some_and(|site| site.on()));
        },
    );

    // A thread's first event allocates its ring. An event in there must
    // not take another ring, and another inside that, until none are left.
    reentering(
        "a thread's first event",
        || microscope_probes::plot!("allocations", 1.0),
        || drop(microscope_probes::zone!("outer")),
    );
    assert_eq!(rings(), 1, "the thread took one ring");
}

/// How many of the registry's rings threads took, from the control block
/// as microscope reads it.
fn rings() -> u32 {
    unsafe extern "C" {
        static __start_microscope_probe_control: AtomicU32;
    }
    const RINGS: usize = 20;

    // SAFETY: the linker places the SDK's control block at the section's
    // start, and its rings word is an aligned `u32` the SDK writes
    // atomically.
    unsafe {
        let control = (&raw const __start_microscope_probe_control).cast::<u8>();
        (*control.add(RINGS).cast::<AtomicU32>()).load(Ordering::Acquire)
    }
}
