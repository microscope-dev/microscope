//! The flight recording a development build keeps of each failing test.

use std::fs;
use std::path::Path;
use std::thread;

use crate::support::flight_recordings::recording_path;
use crate::support::{Scenario, wait_until};

/// A scenario that fails mid-session leaves a recording of the requests,
/// native control calls, wait statuses, and events that led to the failure,
/// ending with the failure itself.
#[test]
fn failing_scenarios_keep_a_flight_recording() {
    let failing = thread::Builder::new()
        .name("flight_recorder::deliberately_failing".to_owned())
        .spawn(|| {
            let path = recording_path("");
            let failed = std::panic::catch_unwind(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build runtime")
                    .block_on(async {
                        let mut scenario = Scenario::launch("basic");
                        scenario.add_breakpoint("main").await;
                        scenario.run_to_stop().await;
                        panic!("deliberate failure");
                    });
            });

            assert!(failed.is_err(), "the scenario did not fail");
            path
        })
        .expect("start the failing scenario");
    let path = failing.join().expect("the failing scenario's thread");

    let contents = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("no recording at {}: {error}", path.display()));

    let position = |needle: &str| {
        contents
            .find(needle)
            .unwrap_or_else(|| panic!("the recording lacks {needle:?}:\n{contents}"))
    };

    let order = [
        "started for",
        "request add breakpoint",
        "request launch",
        "spawn ",
        "stopped by SIGTRAP",
        "install breakpoint",
        "PTRACE_CONT",
        "event InferiorStopped",
        "panic: ",
        "deliberate failure",
    ]
    .map(position);
    assert!(order.is_sorted(), "out of order {order:?}:\n{contents}");
    fs::remove_file(&path).expect("remove the deliberate recording");

    // Dropping the failed scenario's debugger still kills and reaps the
    // inferior.
    let inferior = contents
        .lines()
        .find_map(|line| line.split_once("spawned ")?.1.parse::<i32>().ok())
        .expect("the recording names the inferior");
    wait_until("the failed scenario's inferior is reaped", || {
        !Path::new(&format!("/proc/{inferior}")).exists()
    });
}
