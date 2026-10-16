//! Futures that `block_on` drives, which no runtime lists as tasks: the
//! thread that drives one shows the future's chain of awaits before the
//! frame that drives it, with each async function's saved variables, as a
//! suspended task's backtrace does.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::process::Stdio;

use microscope::{
    Backtrace, CodeRole, ExecutionContext, FrameKind, InferiorState, LaunchOptions, StackFrameId,
    StackSegment, StopContext, StopReason, ThreadId,
};

use crate::invariants::checked;
use crate::stops::line;
use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir};
use crate::workers::check_saved_local;

const BUILDS: [&str; 4] = [
    "tokio-drivers-o0",
    "tokio-drivers-o3",
    "tokio-drivers-1.52-o0",
    "tokio-drivers-1.52-o3",
];
const SOURCE: &str = "drivers/src/main.rs";

/// How the fixture drives its future: `#[tokio::main]`'s, a
/// current-thread runtime's `Runtime::block_on`, and a multi-thread
/// runtime's `Handle::block_on`.
#[derive(Debug, Clone, Copy)]
enum Mode {
    Main,
    Current,
    Handle,
}

/// The fixture stopped at its checkpoint, and what it reported there: the
/// thread that drives the future, and the values the future recorded.
struct Driven {
    scenario: Scenario,
    driver: ThreadId,
    values: BTreeMap<String, String>,
    _scratch: ScratchDir,
}

impl Driven {
    async fn parked(fixture: &str, mode: Mode) -> Self {
        let scratch = ScratchDir::new("drivers");
        let output: PathBuf = scratch.path().join("stdout");
        let mut scenario = checked(fixture);
        scenario
            .add_breakpoint_spec(microscope::BreakpointSpec::Function("truth_reached".into()))
            .await;
        let arguments = match mode {
            Mode::Main => Vec::new(),
            Mode::Current => vec!["current".into()],
            Mode::Handle => vec!["handle".into()],
        };

        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                arguments,
                stdout: Some(Stdio::from(File::create(&output).expect("an output file"))),
                ..LaunchOptions::default()
            })
            .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture} {mode:?}: {reason:?}"
        );
        let text = std::fs::read_to_string(&output).expect("the fixture's output");
        let mut driver = None;
        let mut values = BTreeMap::new();
        for fields in Report::parse(&text)
            .lines()
            .map(|fields| fields.iter().map(String::as_str).collect::<Vec<_>>())
        {
            match fields[..] {
                ["driver", tid] => driver = Some(ThreadId::new(tid.parse().expect("a tid"))),
                ["value", "0", name, value] => {
                    values.insert(name.to_owned(), value.to_owned());
                }
                _ => {}
            }
        }

        Self {
            scenario,
            driver: driver.expect("the fixture names the driving thread"),
            values,
            _scratch: scratch,
        }
    }
}

/// The async functions the driven future runs, innermost first, each with
/// the line it awaits at.
fn awaits(mode: Mode) -> Vec<(String, u64)> {
    let mut awaits = vec![
        ("waiting".to_owned(), line(SOURCE, "// AWAIT: waiting")),
        ("driven".to_owned(), line(SOURCE, "// AWAIT: driven")),
    ];
    if matches!(mode, Mode::Main) {
        awaits.push((
            "tokio_main::{async block#0}".to_owned(),
            line(SOURCE, "// AWAIT: main"),
        ));
    }

    awaits
}

/// A backtrace's frames, a line each.
fn listing(trace: &Backtrace) -> String {
    let frames = trace
        .frames
        .iter()
        .map(|frame| {
            let name = frame
                .function
                .as_ref()
                .map(|function| function.name.to_string())
                .or_else(|| frame.symbol.as_ref().map(|symbol| symbol.name.to_string()))
                .unwrap_or_default();
            let line = frame.source.as_ref().map_or(0, |source| source.line.get());
            format!(
                "#{} {:?} {:?} {:?} {name}:{line}",
                frame.level, frame.segment, frame.role, frame.kind
            )
        })
        .collect::<Vec<_>>();
    format!(
        "\n{}\nunfollowed: {:?}",
        frames.join("\n"),
        trace.unfollowed
    )
}

/// The frames of the future a frame drives, and the frame after them.
fn spliced(trace: &Backtrace) -> Option<(&[microscope::StackFrame], &microscope::StackFrame)> {
    let start = trace
        .frames
        .iter()
        .position(|frame| frame.segment == StackSegment::Future)?;
    let length = trace.frames[start..]
        .iter()
        .take_while(|frame| frame.segment == StackSegment::Future)
        .count();
    Some((
        &trace.frames[start..start + length],
        trace.frames.get(start + length)?,
    ))
}

/// The thread that drives a future shows the future's chain of awaits,
/// from the future it waits on out to the async function `block_on` was
/// given, just before tokio's frame that drives it; each async frame keeps
/// the local its function recorded. Where the debug information loses the
/// variable that holds the future, the backtrace says so at that frame
/// instead.
async fn driven_futures_show_their_awaits(mode: Mode) {
    for fixture in BUILDS {
        let context = format!("{fixture} {mode:?}");
        let mut stopped = Driven::parked(fixture, mode).await;
        let InferiorState::Stopped { stop_id, .. } = stopped.scenario.snapshot().await.inferior
        else {
            panic!("{context}: not stopped");
        };

        let view = |frame| StopContext {
            stop: stop_id,
            execution: ExecutionContext::Thread(stopped.driver),
            frame,
        };

        let handle = stopped.scenario.handle();
        let trace = stopped
            .scenario
            .operation(
                "driver backtrace",
                handle.at(view(StackFrameId::INNERMOST)).backtrace(),
            )
            .await;
        let Some((future, driver)) = spliced(&trace) else {
            // Only an optimized build may lose the future, and then the
            // backtrace says where and why.
            assert!(fixture.ends_with("o3"), "{context}: {}", listing(&trace));
            let [unfollowed] = &trace.unfollowed[..] else {
                panic!("{context}: {}", listing(&trace));
            };

            let driver = &trace.frames[unfollowed.driver.get() as usize];
            assert_eq!(
                driver.role,
                CodeRole::RuntimeInternal,
                "{context}: {}",
                listing(&trace)
            );
            stopped.scenario.shutdown().await;
            continue;
        };

        assert!(
            trace.unfollowed.is_empty(),
            "{context}: {}",
            listing(&trace)
        );
        assert!(
            matches!(future[0].kind, FrameKind::Awaited { .. }),
            "{context}: {}",
            listing(&trace)
        );
        let shown = future[1..]
            .iter()
            .map(|frame| {
                assert!(
                    matches!(frame.kind, FrameKind::Async { .. }),
                    "{context}: {frame:#?}"
                );
                (
                    frame
                        .function
                        .as_ref()
                        .map(|function| function.name.to_string())
                        .unwrap_or_default(),
                    frame.source.as_ref().map_or(0, |source| source.line.get()),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(shown, awaits(mode), "{context}: {}", listing(&trace));
        assert_eq!(
            driver.role,
            CodeRole::RuntimeInternal,
            "{context}: {driver:#?}"
        );
        assert!(
            driver
                .function
                .as_ref()
                .and_then(|function| function.linkage_name.as_deref())
                .is_some_and(|name| name.contains("8block_on")),
            "{context}: {driver:#?}"
        );

        // Only one future is driven, once.
        assert_eq!(
            trace
                .frames
                .iter()
                .filter(|frame| frame.segment == StackSegment::Future)
                .count(),
            future.len(),
            "{context}: {}",
            listing(&trace)
        );
        for frame in &future[1..] {
            check_saved_local(
                &stopped.scenario,
                view(frame.id),
                frame,
                &stopped.values,
                &context,
            )
            .await;
        }

        stopped.scenario.shutdown().await;
    }
}

#[tokio::test]
async fn tokio_main_shows_its_future() {
    driven_futures_show_their_awaits(Mode::Main).await;
}

#[tokio::test]
async fn a_current_thread_runtime_shows_the_future_it_blocks_on() {
    driven_futures_show_their_awaits(Mode::Current).await;
}

#[tokio::test]
async fn handle_block_on_shows_its_future() {
    driven_futures_show_their_awaits(Mode::Handle).await;
}
