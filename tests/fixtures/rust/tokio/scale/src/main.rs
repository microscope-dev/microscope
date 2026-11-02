//! Thousands of tasks on a multi-thread runtime, each parked at an
//! await that never finishes. Once every task waits there and the workers
//! are idle, the program reports its truth and stops at the checkpoint
//! `parked`.

/// The tasks the program spawns.
const TASKS: usize = 2_049;

async fn parked() {
    let me = truth::start();
    let _at = truth::at(me, "pending");
    std::future::pending::<()>().await; // AWAIT: pending
}

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("a runtime");
    for _ in 0..TASKS {
        runtime.spawn(parked());
    }
    while !truth::all_parked(TASKS) || !truth::workers_parked(runtime.handle()) {
        std::thread::yield_now();
    }
    truth::checkpoint("parked", Some(runtime.handle()));
    runtime.shutdown_background();
}
