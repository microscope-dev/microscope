//! A Go program that calls C, which calls back into Go, built with each C
//! compiler. C runs on the thread's system stack, and the Go it calls back
//! on the goroutine's own; backtraces show both, and steps go between them
//! as they go between functions of one language. cgo's own code between
//! them is a wrapper, which steps pass through.

use std::process::Stdio;

use microscope::{LanguageExceptionKind, LaunchOptions, StackSegment, StepKind, StopReason};

use crate::invariants::checked;
use crate::stops::place;
use crate::support::{self, Scenario};

const BUILDS: [&str; 2] = ["cgo-go-gcc", "cgo-go-clang"];
const SOURCE: &str = "tests/fixtures/go/cgo/main.go";
const CALLBACK: &str = "tests/fixtures/go/cgo/callback.go";

/// The selected thread's backtrace, as runs of function names by stack.
/// cgo names its code with a hash, here `#`.
pub async fn segments(scenario: &Scenario) -> Vec<(StackSegment, Vec<String>)> {
    let trace = scenario
        .operation("backtrace", scenario.handle().backtrace())
        .await;

    let mut segments: Vec<(StackSegment, Vec<String>)> = Vec::new();
    for frame in trace.frames.iter() {
        let name = frame
            .function
            .as_ref()
            .map_or_else(|| "?".to_owned(), |function| unhashed(&function.name));
        match segments.last_mut() {
            Some((segment, names)) if *segment == frame.segment => names.push(name),
            _ => segments.push((frame.segment, vec![name])),
        }
    }

    segments
}

/// A cgo name with its package hash replaced by `#`.
fn unhashed(name: &str) -> String {
    for prefix in ["_cgo_", "_cgoexp_"] {
        if let Some(rest) = name.strip_prefix(prefix)
            && let Some((_, function)) = rest.split_once('_')
        {
            return format!("{prefix}#_{function}");
        }
    }

    name.to_owned()
}

pub fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// A scenario stopped at the first breakpoint on the function `location`,
/// or on a line of the fixture's main file given as `main.go:LINE`.
async fn stopped_at(fixture: &str, location: &str) -> Scenario {
    let mut scenario = checked(fixture);
    match location.strip_prefix("main.go:") {
        Some(line) => {
            let line = line.parse().expect("a line");
            scenario.add_source_breakpoint("main.go", line).await
        }
        None => scenario.add_breakpoint(location).await,
    };

    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            stdout: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;
    assert!(
        matches!(reason, StopReason::Breakpoint { .. }),
        "{fixture}: {reason:?}"
    );
    scenario
}

#[tokio::test]
async fn a_backtrace_from_c_reaches_the_go_that_called_it() {
    for fixture in BUILDS {
        let scenario = stopped_at(fixture, "leaf").await;
        assert_eq!(
            segments(&scenario).await,
            [
                (
                    StackSegment::System,
                    names(&["leaf", "_cgo_#_Cfunc_leaf", "runtime.asmcgocall"])
                ),
                (
                    StackSegment::Task,
                    names(&[
                        "runtime.cgocall",
                        "main._Cfunc_leaf",
                        "main.main",
                        "runtime.main",
                        "runtime.goexit",
                    ])
                ),
            ],
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_backtrace_from_go_that_c_called_shows_the_c_between() {
    for fixture in BUILDS {
        let scenario = stopped_at(fixture, "main.callback").await;
        let found = segments(&scenario).await;
        let context = format!("{fixture}: {found:#?}");
        let [
            (StackSegment::Task, callback),
            (StackSegment::System, c),
            (StackSegment::Task, go),
        ] = found.as_slice()
        else {
            panic!("{context}");
        };

        // Optimized, the export inlines the callback.
        assert!(
            callback.starts_with(&names(&["main.callback"])),
            "{context}"
        );
        assert!(
            callback.ends_with(&names(&[
                "_cgoexp_#_callback",
                "runtime.cgocallbackg1",
                "runtime.cgocallbackg",
                "runtime.cgocallback",
            ])),
            "{context}"
        );
        assert_eq!(
            *c,
            names(&[
                "crosscall2",
                "callback",
                "calls",
                "_cgo_#_Cfunc_calls",
                "runtime.asmcgocall",
            ]),
            "{context}"
        );
        assert_eq!(
            *go,
            names(&[
                "runtime.cgocall",
                "main._Cfunc_calls",
                "main.main",
                "runtime.main",
                "runtime.goexit",
            ]),
            "{context}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn steps_go_from_go_into_c_and_back() {
    let go = |marker: &str| support::source_line(SOURCE, &format!("// GO: {marker}"));
    let c = |marker: &str| support::source_line(SOURCE, &format!("// CGO: {marker}"));

    for fixture in BUILDS {
        let mut scenario = stopped_at(fixture, &format!("main.go:{}", go("leaf"))).await;
        scenario.step_to_stop(StepKind::IntoSource).await;
        assert_eq!(
            place(&scenario).await,
            ("leaf".to_owned(), c("leaf")),
            "{fixture}"
        );

        // Out of C, through cgo's code and the runtime's, to the Go after
        // the call.
        let mut function = String::new();
        for _ in 0..8 {
            scenario.step_to_stop(StepKind::OverSource).await;
            function = place(&scenario).await.0;
            if function != "leaf" {
                break;
            }
        }

        assert_eq!(
            place(&scenario).await,
            ("main.main".to_owned(), go("calls")),
            "{fixture}: stepped out of leaf into {function}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn steps_c_into_the_go_it_calls() {
    let line = support::source_line(SOURCE, "// CGO: calls");
    for fixture in BUILDS {
        let mut scenario = stopped_at(fixture, &format!("main.go:{line}")).await;
        scenario.step_to_stop(StepKind::IntoSource).await;

        // Unoptimized, Go enters a function on its declaration's line.
        let marker = if fixture.ends_with("gcc") {
            "// GO: entered"
        } else {
            "// GO: callback"
        };

        let callback = support::source_line(CALLBACK, marker);
        assert_eq!(
            place(&scenario).await,
            ("main.callback".to_owned(), callback),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn finishing_go_that_c_called_returns_to_the_c() {
    for fixture in BUILDS {
        let mut scenario = stopped_at(fixture, "main.callback").await;
        let reason = scenario.step_to_stop(StepKind::Out).await;
        assert_eq!(
            reason,
            StopReason::Step {
                kind: StepKind::Out
            },
            "{fixture}"
        );

        // Optimized, the instruction after the call may begin the next line.
        let line = support::source_line(SOURCE, "// CGO: calls");
        let (function, at) = place(&scenario).await;
        assert_eq!(function, "calls", "{fixture}");
        assert!([line, line + 1].contains(&at), "{fixture}: line {at}");
        scenario.shutdown().await;
    }
}

/// Only Go code can panic, so a fault in C is fatal, at the C that
/// faulted, and the program then ends as it does alone.
#[tokio::test]
async fn a_fault_in_c_is_fatal_where_it_faulted() {
    for fixture in BUILDS {
        let alone = std::process::Command::new(Scenario::fixture(fixture))
            .arg("fault")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run the fixture alone");
        assert_eq!(alone.code(), Some(2), "{fixture}");

        let mut scenario = checked(fixture);
        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                arguments: vec!["fault".into()],
                stdout: Some(Stdio::null()),
                stderr: Some(Stdio::null()),
                ..LaunchOptions::default()
            })
            .await;
        let StopReason::LanguageException(exception) = &reason else {
            panic!("{fixture}: {reason:?}");
        };

        assert_eq!(exception.kind, LanguageExceptionKind::Fatal, "{fixture}");

        let line = support::source_line(SOURCE, "// CGO: fault");
        assert_eq!(
            place(&scenario).await,
            ("fault".to_owned(), line),
            "{fixture}"
        );
        assert_eq!(
            scenario.resume_to_stop().await,
            StopReason::Exited(microscope::ExitStatus::Code(2)),
            "{fixture}"
        );
        scenario.shutdown().await;
    }
}
