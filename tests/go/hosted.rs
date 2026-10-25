//! A Go library that a C program hosts. The library carries the runtime,
//! which starts as the library loads; the host calls into Go on a thread
//! of its own, which runs a goroutine for the call, and the library's own
//! goroutines are listed beside it.

use std::process::Stdio;

use microscope::{BreakpointOptions, BreakpointSpec, LaunchOptions, StackSegment, StopReason};

use crate::cgo::{names, segments};
use crate::invariants::checked;
use crate::stops::{integer, place};
use crate::support::{self, Scenario};

const SOURCE: &str = "tests/fixtures/go/hosted/main.go";

#[tokio::test]
async fn a_hosted_go_library_shows_the_goroutines_of_its_runtime() {
    let mut scenario = checked("go-host");

    // The library loads once the host runs.
    scenario
        .operation(
            "break in the library",
            scenario.handle().add_breakpoint_with(
                BreakpointSpec::Function("main.Triple".to_owned()),
                BreakpointOptions {
                    pending: true,
                    ..BreakpointOptions::default()
                },
            ),
        )
        .await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
    assert_eq!(
        place(&scenario).await,
        (
            "main.Triple".to_owned(),
            // Unoptimized, Go enters a function on its declaration's line.
            support::source_line(SOURCE, "// HOSTED: entered")
        )
    );

    // The host's thread runs a goroutine for the call, whose stack is
    // the goroutine's, and the host's C below it the thread's.
    let task = integer(&scenario, "$task")
        .await
        .expect("the host's thread runs a goroutine");
    let found = segments(&scenario).await;
    let context = format!("{found:#?}");
    let [(StackSegment::Task, go), (StackSegment::System, c), ..] = found.as_slice() else {
        panic!("{context}");
    };

    assert_eq!(
        *go,
        names(&[
            "main.Triple",
            "_cgoexp_#_Triple",
            "runtime.cgocallbackg1",
            "runtime.cgocallbackg",
            "runtime.cgocallback",
        ]),
        "{context}"
    );
    assert!(
        c.starts_with(&names(&["crosscall2", "Triple", "main"])),
        "{context}"
    );

    // The goroutine the library started as it loaded is listed.
    assert_listed(&scenario, task).await;

    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(microscope::ExitStatus::Code(0))
    );
    scenario.shutdown().await;
}

/// The goroutine the library started as it loaded is in its function,
/// and the one the host's call runs is on its thread.
async fn assert_listed(scenario: &Scenario, task: i128) {
    let tasks = scenario
        .operation("tasks", scenario.handle().tasks(None, 256))
        .await;
    assert!(tasks.gaps.is_empty(), "{:?}", tasks.gaps);
    let waiting = tasks
        .tasks
        .iter()
        .find(|task| {
            task.entry
                .as_ref()
                .and_then(|entry| entry.function.as_deref())
                == Some("main.waiting")
        })
        .unwrap_or_else(|| panic!("no goroutine began in main.waiting: {:#?}", tasks.tasks));

    scenario
        .operation(
            "select the waiting goroutine",
            scenario.handle().select_context(waiting.id),
        )
        .await;

    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;
    let user = trace.user_frame().unwrap_or_else(|| {
        panic!("the goroutine runs the library's code: {waiting:#?}\n{trace:#?}")
    });

    // Initialization waits for entry, but the receive may not have parked yet.
    let lines = ["// HOSTED: began", "// HOSTED: waiting"]
        .map(|marker| support::source_line(SOURCE, marker));
    assert_eq!(
        user.function
            .as_ref()
            .map(|function| function.name.as_ref()),
        Some("main.waiting"),
        "{trace:#?}"
    );
    assert!(
        user.source
            .as_ref()
            .is_some_and(|source| lines.contains(&source.line.get())),
        "{trace:#?}"
    );
    assert!(
        tasks
            .tasks
            .iter()
            .any(|listed| i128::from(listed.id.number) == task && listed.thread.is_some()),
        "{:#?}",
        tasks.tasks
    );
}
