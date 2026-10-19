//! An HTTP server and its client in one process. A handler runs on the
//! goroutine the server started for its connection, and a handler's panic,
//! which the server recovers from, is no stop of its own.

use std::process::Stdio;

use microscope::{ExitStatus, LaunchOptions, StopReason};

use crate::invariants::checked;
use crate::stops::{integer, place};
use crate::support::{self, ScratchDir};

const SOURCE: &str = "tests/fixtures/go/server/main.go";

#[tokio::test]
async fn a_handler_stops_on_its_connection_and_a_recovered_panic_does_not() {
    let scratch = ScratchDir::new("server");
    let output = scratch.path().join("stdout");
    let mut scenario = checked("server-go-o0");
    let line = support::source_line(SOURCE, "// SERVER: greet");
    let breakpoint = scenario.add_source_breakpoint("main.go", line).await;
    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::from(
                std::fs::File::create(&output).expect("create standard output"),
            )),
            stderr: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{reason:?}"
    );
    assert_eq!(place(&scenario).await, ("main.greet".to_owned(), line));
    assert_eq!(integer(&scenario, "len(name)").await, Some(6));

    // The handler runs on its connection's goroutine, which the server
    // started, through net/http to where the goroutine began.
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;

    let names = trace
        .frames
        .iter()
        .filter_map(|frame| frame.function.as_ref())
        .map(|function| function.name.as_ref())
        .collect::<Vec<_>>();
    assert!(names.contains(&"net/http.(*conn).serve"), "{names:?}");
    assert_eq!(names.last(), Some(&"runtime.goexit"), "{names:?}");

    let task = integer(&scenario, "$task").await.expect("a goroutine");
    let tasks = scenario
        .operation("tasks", scenario.handle().tasks(None, 256))
        .await;
    let handling = tasks
        .tasks
        .iter()
        .find(|listed| i128::from(listed.id.number) == task)
        .expect("the handler's goroutine is listed");
    assert_eq!(
        handling
            .creation
            .as_ref()
            .and_then(|creation| creation.function.as_deref()),
        Some("net/http.(*Server).Serve"),
        "{handling:#?}"
    );

    // The server recovers from the other handler's panic: the program goes
    // on to its end, its client having seen the connection close.
    scenario.remove_breakpoint(breakpoint.id).await;
    assert_eq!(
        scenario.resume_to_stop().await,
        StopReason::Exited(ExitStatus::Code(0))
    );

    let printed = std::fs::read_to_string(&output).expect("read standard output");
    assert!(
        printed.contains("/greet?name=gopher: 200 hello, gopher"),
        "{printed}"
    );
    assert!(printed.contains("/broken: no response"), "{printed}");
    scenario.shutdown().await;
}
