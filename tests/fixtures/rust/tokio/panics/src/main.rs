//! Rust panics of every kind a debugger stops at, one per run, chosen by
//! the first argument. The program's own panic hook prints each panic's
//! message and location as a `TRUTH` line, then runs the default hook, so
//! a test can compare the debugger's report with the program's. Each
//! panicking line carries a `PANIC:` marker.

use std::hint::black_box;
use std::panic::{self, AssertUnwindSafe};

/// Reports every panic as the program sees it, then as Rust would.
fn install_hook() {
    let default = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let message = info.payload_as_str().unwrap_or("-");
        let (file, line) = info
            .location()
            .map_or(("-", 0), |location| (location.file(), location.line()));
        // The task that panicked, as tokio numbers it, if a task did.
        let task = tokio::task::try_id().map_or_else(|| "-".to_owned(), |id| id.to_string());
        truth::line(&[&"panic", &message, &file, &line, &task]);
        default(info);
    }));
}

/// Panics as it is dropped.
struct Bomb;

impl Drop for Bomb {
    fn drop(&mut self) {
        panic!("dropped while unwinding"); // PANIC: drop
    }
}

fn formatted(count: u32) {
    let count = black_box(count);
    panic!("formatted {count} times"); // PANIC: format
}

fn unwrapped(value: Option<u8>) -> u8 {
    black_box(value).unwrap() // PANIC: unwrap
}

async fn in_task(case: &str) {
    match case {
        "str" => panic!("a static message"), // PANIC: str
        "format" => formatted(7),
        "unwrap" => _ = unwrapped(None),
        "any" => panic::panic_any(42_i32), // PANIC: any
        "resume" => {
            let caught = panic::catch_unwind(|| panic!("first")); // PANIC: first
            truth::line(&[&"caught"]);
            panic::resume_unwind(caught.expect_err("it panicked")); // PANIC: resume
        }
        other => unreachable!("no case {other}"),
    }
}

fn main() {
    install_hook();
    let case = std::env::args().nth(1).expect("a case");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .build()
        .expect("a runtime");
    match case.as_str() {
        "main" => panic!("in main"), // PANIC: main
        "caught" => {
            let caught = panic::catch_unwind(|| panic!("caught here")); // PANIC: caught
            truth::line(&[&"recovered", &caught.is_err()]);
        }
        "drop" => {
            let _bomb = Bomb;
            panic!("unwinding"); // PANIC: unwinding
        }
        "silent" => {
            // A hook that never reads the payload leaves its message
            // unformatted.
            panic::set_hook(Box::new(|_| {}));
            let caught = panic::catch_unwind(AssertUnwindSafe(|| formatted(3)));
            truth::line(&[&"recovered", &caught.is_err()]);
        }
        "blocking" => {
            let joined = runtime.block_on(runtime.spawn_blocking(|| {
                panic!("blocking"); // PANIC: blocking
            }));
            truth::line(&[&"joined", &joined.is_err()]);
        }
        "local" => {
            let local = tokio::task::LocalSet::new();
            let joined = local.block_on(&runtime, async {
                tokio::task::spawn_local(async {
                    panic!("local"); // PANIC: local
                })
                .await
            });
            truth::line(&[&"joined", &joined.is_err()]);
        }
        "current" => {
            let current = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("a runtime");
            let joined = current.block_on(current.spawn(async {
                panic!("current"); // PANIC: current
            }));
            truth::line(&[&"joined", &joined.is_err()]);
        }
        task => {
            let task = task.to_owned();
            let joined = runtime.block_on(runtime.spawn(async move { in_task(&task).await }));
            truth::line(&[&"joined", &joined.is_err()]);
        }
    }
    truth::line(&[&"end"]);
}
