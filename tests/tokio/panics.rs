use std::fs::File;
use std::path::PathBuf;
use std::process::Stdio;

use microscope::{
    ExceptionStops, ExitStatus, InferiorState, LanguageExceptionKind, LaunchOptions, StopReason,
    ThreadActivity,
};

use crate::invariants::checked;
use crate::stops::{evaluated, line};
use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir};

const SOURCE: &str = "panics/src/main.rs";
const SIGABRT: u64 = 6;

struct Launched {
    scenario: Scenario,
    reason: StopReason,
    output: PathBuf,
    _scratch: ScratchDir,
}

async fn launched(fixture: &str, case: &str, stops: Option<ExceptionStops>) -> Launched {
    let scratch = ScratchDir::new("panics");
    let output = scratch.path().join("stdout");
    let mut scenario = checked(fixture);

    if let Some(stops) = stops {
        scenario
            .operation(
                "exception stops",
                scenario.handle().set_exception_stops(stops),
            )
            .await;
    }

    let reason = scenario
        .run_with_to_stop(LaunchOptions {
            arguments: vec![case.into()],
            stdout: Some(Stdio::from(File::create(&output).expect("an output file"))),
            stderr: Some(Stdio::null()),
            ..LaunchOptions::default()
        })
        .await;

    Launched {
        scenario,
        reason,
        output,
        _scratch: scratch,
    }
}

impl Launched {
    fn truth(&self) -> Vec<Vec<String>> {
        Report::read(&self.output)
            .lines()
            .map(<[String]>::to_vec)
            .collect()
    }

    fn hooked(&self) -> (String, u64, Option<u64>) {
        let truth = self.truth();

        let panic = truth
            .iter()
            .rev()
            .find(|fields| fields[0] == "panic")
            .unwrap_or_else(|| panic!("no panic reported: {truth:?}"));

        assert!(panic[2].ends_with(SOURCE), "{panic:?}");

        (
            panic[1].clone(),
            panic[3].parse().expect("a line"),
            panic[4].parse().ok(),
        )
    }
}

fn exception(reason: &StopReason, context: &str) -> String {
    let StopReason::LanguageException(exception) = reason else {
        panic!("{context}: {reason:?}");
    };

    assert_eq!(exception.kind, LanguageExceptionKind::Raised, "{context}");
    exception.message.to_string()
}

async fn selected(scenario: &Scenario) -> (String, u64) {
    let location = scenario
        .operation("location", scenario.handle().current_location())
        .await;

    let source = location.image.source.expect("the frame has a line");
    let image = scenario.handle().module_image();

    let path = image
        .source_file(source.file)
        .map(|file| file.path.display().to_string())
        .unwrap_or_default();

    (path, source.line.get())
}

async fn check_panic(launched: &mut Launched, context: &str, message: &str, marker: &str) {
    let (hooked, at, task) = launched.hooked();
    assert_eq!(hooked, message, "{context}");

    let expected = line(SOURCE, marker);
    assert_eq!(at, expected, "{context}: the hook's line");

    assert_eq!(
        exception(&launched.reason, context),
        format!("panicked: {message}")
    );

    let (path, line) = selected(&launched.scenario).await;
    assert!(path.ends_with(SOURCE), "{context}: {path}");
    assert_eq!(line, expected, "{context}");

    let snapshot = launched.scenario.snapshot().await;
    let InferiorState::Stopped { thread_id, .. } = snapshot.inferior else {
        panic!("{context}: not stopped");
    };

    let activity = snapshot
        .threads
        .iter()
        .find(|thread| thread.id == thread_id)
        .and_then(|thread| thread.activity.clone());

    let runs = match activity {
        Some(ThreadActivity::Task { task, .. }) => Some(task.number),
        _ => None,
    };

    assert_eq!(runs, task, "{context}: {activity:?}");
}

async fn stops_where_the_program_panicked(case: &str, message: &str, ending: &str) {
    let marker = format!("// PANIC: {case}");
    for fixture in ["tokio-panics-o0", "tokio-panics-o3"] {
        let context = format!("{fixture} {case}");
        let mut launched = launched(fixture, case, None).await;
        check_panic(&mut launched, &context, message, &marker).await;

        let reason = launched.scenario.resume_to_stop().await;
        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)), "{context}");

        let truth = launched.truth();

        assert!(
            truth
                .iter()
                .any(|fields| fields[0] == ending && fields[1] == "true"),
            "{context}: {truth:?}"
        );

        launched.scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_static_message_stops_where_it_panicked() {
    stops_where_the_program_panicked("str", "a static message", "joined").await;
}

#[tokio::test]
async fn a_formatted_message_stops_where_it_panicked() {
    stops_where_the_program_panicked("format", "formatted 7 times", "joined").await;
}

#[tokio::test]
async fn a_panic_stops_where_it_panicked_under_legacy_mangling() {
    let message = "formatted 7 times";
    let mut launched = launched("tokio-panics-legacy", "format", None).await;
    check_panic(&mut launched, "legacy", message, "// PANIC: format").await;
    launched.scenario.shutdown().await;
}

#[tokio::test]
async fn an_unwrap_stops_at_its_caller() {
    let message = "called `Option::unwrap()` on a `None` value";
    stops_where_the_program_panicked("unwrap", message, "joined").await;
}

#[tokio::test]
async fn a_blocking_closure_panic_stops_where_it_panicked() {
    stops_where_the_program_panicked("blocking", "blocking", "joined").await;
}

#[tokio::test]
async fn a_local_task_panic_stops_where_it_panicked() {
    stops_where_the_program_panicked("local", "local", "joined").await;
}

#[tokio::test]
async fn a_current_thread_task_panic_stops_where_it_panicked() {
    stops_where_the_program_panicked("current", "current", "joined").await;
}

#[tokio::test]
async fn a_panic_the_program_catches_stops_before_it_is_caught() {
    stops_where_the_program_panicked("caught", "caught here", "recovered").await;
}

#[tokio::test]
async fn a_panic_in_main_stops_and_then_exits_as_rust_does() {
    for fixture in ["tokio-panics-o0", "tokio-panics-o3"] {
        let mut launched = launched(fixture, "main", None).await;
        check_panic(&mut launched, fixture, "in main", "// PANIC: main").await;

        let reason = launched.scenario.resume_to_stop().await;
        assert_eq!(
            reason,
            StopReason::Exited(ExitStatus::Code(101)),
            "{fixture}"
        );

        launched.scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_panic_with_a_value_names_its_type() {
    for fixture in ["tokio-panics-o0", "tokio-panics-o3"] {
        let launched = launched(fixture, "any", None).await;
        let StopReason::LanguageException(raised) = &launched.reason else {
            panic!("{fixture}: {:?}", launched.reason);
        };

        assert_eq!(raised.message.as_ref(), "panicked with a value of type i32");

        let value = raised
            .value
            .as_deref()
            .expect("an expression for the value");

        let held = evaluated(&launched.scenario, value).await;

        assert_eq!(
            microscope::value_summary(held.type_info.as_ref(), &held.state),
            "Some(42)",
            "{value}"
        );

        let (path, line) = selected(&launched.scenario).await;
        assert!(path.ends_with(SOURCE), "{fixture}: {path}");
        assert_eq!(line, crate::stops::line(SOURCE, "// PANIC: any"));

        launched.scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_resumed_panic_stops_again_as_a_resumption() {
    for fixture in ["tokio-panics-o0", "tokio-panics-o3"] {
        let mut launched = launched(fixture, "resume", None).await;
        check_panic(&mut launched, fixture, "first", "// PANIC: first").await;

        let reason = launched.scenario.resume_to_stop().await;
        assert_eq!(exception(&reason, fixture), "panic resumed: first");

        let (_, line) = selected(&launched.scenario).await;
        assert_eq!(line, crate::stops::line(SOURCE, "// PANIC: resume"));

        let reason = launched.scenario.resume_to_stop().await;
        assert_eq!(reason, StopReason::Exited(ExitStatus::Code(0)), "{fixture}");

        launched.scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_panic_while_unwinding_stops_then_aborts() {
    for fixture in ["tokio-panics-o0", "tokio-panics-o3"] {
        let mut launched = launched(fixture, "drop", None).await;
        check_panic(&mut launched, fixture, "unwinding", "// PANIC: unwinding").await;

        launched.reason = launched.scenario.resume_to_stop().await;
        check_panic(
            &mut launched,
            fixture,
            "dropped while unwinding",
            "// PANIC: drop",
        )
        .await;

        let reason = launched.scenario.resume_to_stop().await;
        assert!(
            matches!(&reason, StopReason::Exception(info) if info.code == SIGABRT),
            "{fixture}: {reason:?}"
        );

        launched.scenario.shutdown().await;
    }
}

#[tokio::test]
async fn an_aborting_panic_stops_before_it_aborts() {
    let mut launched = launched("tokio-panics-abort", "format", None).await;
    check_panic(
        &mut launched,
        "abort",
        "formatted 7 times",
        "// PANIC: format",
    )
    .await;

    let reason = launched.scenario.resume_to_stop().await;
    assert!(
        matches!(&reason, StopReason::Exception(info) if info.code == SIGABRT),
        "{reason:?}"
    );

    launched.scenario.shutdown().await;
}

#[tokio::test]
async fn panics_chosen_not_to_stop_run_as_natively() {
    let fixture = "tokio-panics-o0";
    let native = std::process::Command::new(Scenario::fixture(fixture))
        .arg("format")
        .stderr(Stdio::null())
        .output()
        .expect("the fixture runs");

    let stops = ExceptionStops::default()
        .with("rust-panic", false)
        .expect("Rust declares its panics");

    let launched = launched(fixture, "format", Some(stops)).await;
    assert_eq!(launched.reason, StopReason::Exited(ExitStatus::Code(0)));
    assert_eq!(
        std::fs::read(&launched.output).expect("the output"),
        native.stdout
    );

    launched.scenario.shutdown().await;
}

#[tokio::test]
async fn the_panic_machinery_is_marked() {
    let launched = launched("tokio-panics-o0", "format", None).await;
    let trace = crate::stops::backtrace(&launched.scenario).await;

    let roles = trace
        .frames
        .iter()
        .map(|frame| {
            (
                frame
                    .function
                    .as_ref()
                    .map(|function| function.name.to_string()),
                frame.role,
            )
        })
        .collect::<Vec<_>>();

    for name in [
        "rust_panic",
        "panic_with_hook",
        "panic_handler",
        "panic_fmt",
    ] {
        assert!(
            roles
                .iter()
                .any(|(function, role)| function.as_deref() == Some(name)
                    && *role == microscope::CodeRole::Panic),
            "{name}: {roles:?}"
        );
    }

    launched.scenario.shutdown().await;
}
