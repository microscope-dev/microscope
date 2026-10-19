//! A Go program's core, read against the dump of every goroutine the
//! runtime printed as it crashed, and the crash itself, live.

use std::collections::BTreeMap;
use std::process::Stdio;

use microscope::{
    CoreDumpOptions, ExecutionContext, InferiorState, LanguageExceptionKind, LaunchOptions,
    StackFrameId, StopContext, StopReason, TaskSnapshot, TaskState, UnwindTermination,
};

use crate::support::Scenario;

const BUILDS: [&str; 2] = ["panic-go-o0", "panic-go-o2"];

/// One goroutine of a `GOTRACEBACK=crash` dump.
#[derive(Debug)]
struct Dumped {
    status: String,
    labels: Vec<(String, String)>,

    /// Every frame, the runtime's own among them: a function and its
    /// `file:line`.
    frames: Vec<(String, String)>,
}

/// The goroutines of the dump in a core's log, by id.
fn crash_dump(fixture: &str) -> BTreeMap<u64, Dumped> {
    let path = Scenario::fixture(&format!("{fixture}.core.log"));
    let log = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let mut goroutines = BTreeMap::new();
    let mut lines = log.lines().peekable();
    while let Some(line) = lines.next() {
        // goroutine 11 gp=0x… m=nil [chan receive] {job: resize}:
        let Some(header) = line.strip_prefix("goroutine ") else {
            continue;
        };

        let (id, rest) = header.split_once(' ').expect("a goroutine header");
        let (_, rest) = rest.split_once('[').expect("a status");
        let (status, rest) = rest.split_once(']').expect("a status");
        let status = status.split(',').next().expect("a status").to_owned();
        let labels = rest
            .trim_end_matches(':')
            .trim()
            .trim_start_matches('{')
            .trim_end_matches('}')
            .split(", ")
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once(": ").expect("a label");
                (
                    key.trim_matches('"').to_owned(),
                    value.trim_matches('"').to_owned(),
                )
            })
            .collect();

        let mut frames = Vec::new();
        while let Some(call) = lines.next_if(|line| !line.is_empty()) {
            let position = lines.next().expect("a frame's position").trim();
            if call.starts_with("created by ") {
                continue;
            }

            let function = &call[..call.rfind('(').expect("a call")];
            let position = position.split(' ').next().expect("a position");
            frames.push((function.to_owned(), position.to_owned()));
        }

        goroutines.insert(
            id.parse().expect("a goroutine id"),
            Dumped {
                status,
                labels,
                frames,
            },
        );
    }

    assert!(!goroutines.is_empty(), "{fixture}: no dump in {log}");
    goroutines
}

/// Every task of the core.
async fn tasks(scenario: &Scenario) -> Vec<TaskSnapshot> {
    let mut tasks = Vec::new();
    let mut from = None;

    loop {
        let page = scenario
            .operation("tasks", scenario.handle().tasks(from, 4))
            .await;
        assert!(page.gaps.is_empty(), "{:?}", page.gaps);
        tasks.extend(page.tasks.iter().cloned());
        match page.next {
            Some(next) => from = Some(next),
            None => return tasks,
        }
    }
}

fn open(fixture: &str) -> Scenario {
    Scenario::open_core(
        fixture,
        &CoreDumpOptions::new(Scenario::fixture(&format!("{fixture}.core"))),
    )
}

#[tokio::test]
async fn a_cores_goroutines_are_those_the_runtime_dumped_as_it_crashed() {
    for fixture in BUILDS {
        let dump = crash_dump(fixture);
        let mut scenario = open(fixture);
        let snapshot = scenario.snapshot().await;
        let InferiorState::Stopped {
            stop_id, thread_id, ..
        } = snapshot.inferior
        else {
            panic!("{fixture}: not stopped");
        };

        let image = scenario.handle().module_image();
        let tasks = tasks(&scenario).await;

        // Tasks are listed as `allgs` holds them, and each P takes ids in
        // batches, so their order need not be their ids'.
        let mut listed = tasks.iter().map(|task| task.id.number).collect::<Vec<_>>();
        listed.sort_unstable();
        assert_eq!(
            listed,
            dump.keys().copied().collect::<Vec<_>>(),
            "{fixture}"
        );
        for task in &tasks {
            let dumped = &dump[&task.id.number];
            let context = format!("{fixture}: {task:#?}\n{dumped:#?}");
            assert_eq!(
                task.detail.as_deref(),
                Some(dumped.status.as_str()),
                "{context}"
            );

            let trace = scenario
                .operation(
                    "task backtrace",
                    scenario
                        .handle()
                        .at(StopContext {
                            stop: stop_id,
                            execution: ExecutionContext::Task(task.id),
                            frame: StackFrameId::INNERMOST,
                        })
                        .backtrace(),
                )
                .await;
            assert_eq!(trace.termination, UnwindTermination::Complete, "{context}");

            let frames = trace
                .frames
                .iter()
                .map(|frame| {
                    let function = frame.function.as_ref().expect("a function");
                    let source = frame.source.as_ref().expect("a source line");
                    let file = image.source_file(source.file).expect("a source file");
                    (
                        function.name.to_string(),
                        format!("{}:{}", file.path.display(), source.line),
                    )
                })
                .collect::<Vec<_>>();

            if task.state == TaskState::Running {
                // Main panicked on the thread the core stopped at, which
                // went on to raise the signal that ended it.
                assert_eq!(task.thread, Some(thread_id), "{context}");
                let user = dumped.frames.len() - 1;
                assert!(
                    frames.ends_with(&dumped.frames[1..]),
                    "{context}\n{frames:#?}"
                );
                assert_eq!(user, 4, "{context}");
            } else {
                assert_eq!(frames, dumped.frames, "{context}");
            }
        }

        scenario.shutdown().await;
    }
}

#[tokio::test]
async fn a_goroutines_profiler_labels_are_its_own() {
    for fixture in BUILDS {
        let dump = crash_dump(fixture);
        let scenario = open(fixture);
        let tasks = tasks(&scenario).await;
        let mut labelled = 0;

        for task in &tasks {
            let expected = &dump[&task.id.number].labels;
            let labels = task
                .labels
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<Vec<_>>();
            assert_eq!(&labels, expected, "{fixture}: {task:#?}");
            labelled += usize::from(!labels.is_empty());
        }

        assert_eq!(labelled, 1, "{fixture}");
        scenario.shutdown().await;
    }
}

/// Live, the program the cores come from stops at its panic, then at the
/// SIGABRT the runtime raises to dump core. The session ends there, before
/// the signal makes a core of its own.
#[tokio::test]
async fn a_crashing_panic_stops_then_raises_sigabrt() {
    for fixture in BUILDS {
        let mut scenario = crate::invariants::checked(fixture);
        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                environment: vec![("GOTRACEBACK".into(), Some("crash".into()))],
                stdout: Some(Stdio::null()),
                stderr: Some(Stdio::null()),
                ..LaunchOptions::default()
            })
            .await;
        let StopReason::LanguageException(exception) = &reason else {
            panic!("{fixture}: {reason:?}");
        };

        assert_eq!(
            exception.kind,
            LanguageExceptionKind::Unhandled,
            "{fixture}"
        );
        assert_eq!(
            exception.message.as_ref(),
            "panic: the workers are waiting",
            "{fixture}"
        );

        let reason = scenario.resume_to_stop().await;
        assert!(
            matches!(&reason, StopReason::Exception(exception) if exception.code == 6),
            "{fixture}: {reason:?}"
        );
        scenario.shutdown().await;
    }
}
