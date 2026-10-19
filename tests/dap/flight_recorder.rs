//! The flight recordings a development build keeps of failing DAP tests.

use std::fs;
use std::thread;

use serde_json::json;

use crate::dap::{Configuration, Dap, Profile, fixture};
use crate::support::flight_recordings::recording_path;

/// An adapter's recording is set aside when its session ends, and written
/// back if the test fails afterward, as a comparison of finished sessions
/// does.
#[test]
fn failures_after_a_session_ended_keep_the_adapters_recording() {
    let failing = thread::Builder::new()
        .name("flight_recorder::deliberately_failing".to_owned())
        .spawn(|| {
            let path = recording_path(".adapter");

            let failed = std::panic::catch_unwind(|| {
                let mut dap = Dap::start("finished session");
                let started = dap.launch(
                    Profile::VsCode,
                    &fixture("basic"),
                    json!({}),
                    &Configuration::default(),
                );
                dap.event(started.mark, "terminated", |_| true);
                dap.finish();

                assert!(
                    !path.exists(),
                    "a passing session left its recording at {}",
                    path.display()
                );
                panic!("deliberate failure after the session");
            });
            assert!(failed.is_err(), "the test did not fail");

            path
        })
        .expect("start the failing test");

    let path = failing.join().expect("the failing test's thread");

    let contents = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("no recording at {}: {error}", path.display()));
    let position = |needle: &str| {
        contents
            .find(needle)
            .unwrap_or_else(|| panic!("the recording lacks {needle:?}:\n{contents}"))
    };

    let order = [
        "request launch",
        "spawn ",
        "PTRACE_CONT",
        "exited 0",
        "event InferiorExited",
        "request shutdown",
    ]
    .map(position);
    assert!(order.is_sorted(), "out of order {order:?}:\n{contents}");

    fs::remove_file(&path).expect("remove the deliberate recording");
}
