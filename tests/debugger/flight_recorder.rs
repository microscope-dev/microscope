//! The flight recording a development build keeps of each failing test.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;

use crate::support::flight_recordings::{
    KEPT_RUNS, adapter_finished, recording_path, watch_adapter, watch_scenarios,
};
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

/// Set in the runs of [`a_recorded_run`] that fail.
const FAIL: &str = "MICROSCOPE_TEST_FAIL_RECORDED_RUN";

/// A test whose adapter streamed a recording, failing when [`FAIL`] is set.
/// [`failures_keep_their_recordings_through_later_runs`] runs it.
#[test]
#[ignore = "run by failures_keep_their_recordings_through_later_runs"]
fn a_recorded_run() {
    watch_scenarios();
    let adapter = watch_adapter(&mut Command::new("true")).expect("an adapter recording");
    fs::write(&adapter, "the adapter's recording\n").expect("write the adapter's recording");
    adapter_finished(&adapter);

    assert!(std::env::var_os(FAIL).is_none(), "deliberate failure");
}

/// A failing run's recordings survive later passing runs of the same test,
/// which leave none of their own, while the oldest runs past the bound, and
/// recordings named for no run, lose theirs.
#[test]
fn failures_keep_their_recordings_through_later_runs() {
    const TEST: &str = "flight_recorder::a_recorded_run";
    let directory = microscope::flight_recorder::directory()
        .join("tests")
        .join(env!("CARGO_CRATE_NAME"));
    let recordings = || {
        let mut paths = fs::read_dir(&directory)
            .expect("read the recordings")
            .map(|entry| entry.expect("a recording").path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&format!("{TEST}.")))
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    };

    fs::create_dir_all(&directory).expect("create the recordings' directory");
    for path in recordings() {
        fs::remove_file(path).expect("remove an earlier recording");
    }

    // Runs that were killed before they could discard their recordings,
    // one more than the bound, and a recording no run is named for.
    let older = (0..=KEPT_RUNS)
        .map(|run| directory.join(format!("{TEST}.{run:013}-1.log")))
        .collect::<Vec<_>>();
    let unnamed = directory.join(format!("{TEST}.log"));
    for path in older.iter().chain([&unnamed]) {
        fs::write(path, "an older recording\n").expect("write an older recording");
    }

    let run = |fail: bool| {
        let mut command = Command::new(std::env::current_exe().expect("the test binary"));
        command.args([TEST, "--exact", "--include-ignored", "--nocapture"]);
        if fail {
            command.env(FAIL, "1");
        } else {
            command.env_remove(FAIL);
        }

        let output = command.output().expect("run the recorded test");
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(output.status.success(), !fail, "{stderr}");
        stderr
    };

    run(false);
    assert_eq!(recordings(), older[1..]);

    let failed = run(true);
    let mut kept = failed
        .lines()
        .filter_map(|line| line.strip_prefix("flight recording: "))
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    kept.sort();
    assert_eq!(kept.len(), 2, "{failed}");

    let mut expected = older[2..].to_vec();
    expected.extend(kept.iter().cloned());
    assert_eq!(recordings(), expected);

    run(false);
    run(false);
    assert_eq!(recordings(), expected);

    let adapter = fs::read_to_string(&kept[0]).expect("read the adapter's recording");
    assert_eq!(adapter, "the adapter's recording\n");
    let scenarios = fs::read_to_string(&kept[1]).expect("read the scenarios' recording");
    let header = format!("# microscope flight recording {}\n", kept[1].display());
    assert!(scenarios.starts_with(&header), "{scenarios}");

    for path in recordings() {
        fs::remove_file(path).expect("remove a recording");
    }
}
