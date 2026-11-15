use std::collections::{BTreeMap, BTreeSet};

use microscope::{
    Backtrace, CodeRole, CoreDumpOptions, ExecutionContext, FrameKind, InferiorState, StackFrameId,
    StackSegment, StopContext, StopId, ThreadId,
};

use crate::session::{CORES, MATRIX, PythonSession};
use crate::support::Scenario;
use crate::support::truth::Report;

const DESCRIBED: [&str; 3] = ["3.12", "3.13", "3.14"];

#[tokio::test]
async fn the_interpreters_loop_and_calls_are_its_own() {
    for interpreter in DESCRIBED {
        let mut session = PythonSession::launch(interpreter, "stacks.py", &[]).await;
        let truth = session.stop_at("parked").await;
        let threads: Vec<u64> = truth
            .iter()
            .filter(|fields| fields[0] == "thread")
            .map(|fields| fields[1].parse().expect("a native thread id"))
            .collect();
        assert_eq!(threads.len(), 3, "{}: {truth:?}", session.context());
        for thread in threads {
            let trace = session.backtrace(ThreadId::new(thread)).await;
            let context = format!("{} thread {thread}: {trace:#?}", session.context());
            let named = |name: &str| {
                trace.frames.iter().find(|frame| {
                    frame
                        .function
                        .as_ref()
                        .is_some_and(|function| &*function.name == name)
                })
            };

            let evaluating = named("_PyEval_EvalFrameDefault").expect(&context);
            assert_eq!(evaluating.role, CodeRole::Interpreter, "{context}");

            if let Some(vectorcall) = named("_PyFunction_Vectorcall") {
                assert_eq!(vectorcall.role, CodeRole::Interpreter, "{context}");
            }
        }

        session.shutdown().await;
    }
}

pub type Reported = (String, String, String);

pub fn reported_threads(truth: &[Vec<String>]) -> Vec<(u64, bool, Vec<Reported>)> {
    let mut threads: Vec<(u64, bool, Vec<Reported>)> = Vec::new();
    for fields in truth {
        match fields[0].as_str() {
            "thread" => threads.push((
                fields[1].parse().expect("a native thread id"),
                fields[3] == "printer",
                Vec::new(),
            )),
            "frame" => threads
                .last_mut()
                .expect("a frame follows its thread")
                .2
                .push((fields[3].clone(), fields[4].clone(), fields[5].clone())),
            _ => {}
        }
    }

    threads
}

pub fn check_frames(
    trace: &microscope::Backtrace,
    reported: &[Reported],
    beneath: usize,
) -> Result<(), String> {
    if !trace.unfollowed.is_empty() {
        return Err(format!("frames are unfollowed: {:?}", trace.unfollowed));
    }

    let mut shown = Vec::new();
    for frame in python_frames(trace) {
        let place = frame
            .recorded
            .as_ref()
            .and_then(|recorded| Some((recorded, recorded.place.as_ref()?)))
            .ok_or_else(|| format!("frame {} has no recorded place", frame.level))?;
        shown.push((
            place.0.function.to_string(),
            place.1.path.to_string(),
            place.1.line.to_string(),
        ));
    }

    if shown.len() != reported.len() + beneath || !shown.ends_with(reported) {
        return Err(format!(
            "shown {shown:#?}\nwhere CPython reported {reported:#?} beneath {beneath}"
        ));
    }

    for (index, frame) in trace.frames.iter().enumerate() {
        let ends = matches!(frame.kind, FrameKind::Interpreted { .. })
            && trace
                .frames
                .get(index + 1)
                .is_some_and(|next| !matches!(next.kind, FrameKind::Interpreted { .. }));
        if ends {
            let activation = &trace.frames[index + 1];
            if activation.role != CodeRole::Interpreter
                || activation.segment != StackSegment::Thread
            {
                return Err(format!(
                    "frame {} is not the evaluation loop that runs frame {}",
                    activation.level, frame.level
                ));
            }
        }
    }

    Ok(())
}

fn python_frames(trace: &microscope::Backtrace) -> impl Iterator<Item = &microscope::StackFrame> {
    trace
        .frames
        .iter()
        .filter(|frame| matches!(frame.kind, FrameKind::Interpreted { .. }))
}

pub struct CodeLines(BTreeMap<(String, String), BTreeSet<String>>);

impl CodeLines {
    pub fn of(report: &Report) -> Self {
        let mut lines: BTreeMap<_, BTreeSet<String>> = BTreeMap::new();
        for fields in report.lines().filter(|fields| fields[0] == "code") {
            lines
                .entry((fields[1].clone(), fields[2].clone()))
                .or_default()
                .extend(fields[4].split(',').map(ToOwned::to_owned));
        }

        Self(lines)
    }

    pub fn check(&self, trace: &microscope::Backtrace) -> Result<(), String> {
        let mut checked = 0;
        for frame in python_frames(trace) {
            let Some((function, place)) = frame
                .recorded
                .as_ref()
                .and_then(|recorded| Some((&recorded.function, recorded.place.as_ref()?)))
            else {
                continue;
            };

            let covered = self.0.keys().any(|(_, file)| *file == *place.path);
            if !covered {
                continue;
            }

            checked += 1;
            let key = (function.to_string(), place.path.to_string());
            let line = place.line.to_string();
            if !self.0.get(&key).is_some_and(|lines| lines.contains(&line)) {
                return Err(format!(
                    "frame {} runs {function} at {}:{line}, a line its code has not",
                    frame.level, place.path
                ));
            }
        }

        if checked == 0 {
            return Err("the code table covers no frame of the stack".to_owned());
        }

        Ok(())
    }
}

#[tokio::test]
async fn each_threads_python_frames_are_the_ones_cpython_reports() {
    for interpreter in MATRIX {
        let mut session = PythonSession::launch(interpreter, "stacks.py", &[]).await;
        let truth = session.stop_at("parked").await;
        let threads = reported_threads(&truth);
        let lines = CodeLines::of(&session.report());
        let stop = session.stop().await;
        assert_eq!(threads.len(), 3, "{}: {truth:?}", session.context());
        for (thread, printer, reported) in threads {
            let trace = session.backtrace(ThreadId::new(thread)).await;

            let beneath = if printer { 2 } else { 0 };
            let context = format!("{} thread {thread}: {trace:#?}", session.context());
            check_frames(&trace, &reported, beneath)
                .unwrap_or_else(|lie| panic!("{context}\n{lie}"));
            lines
                .check(&trace)
                .unwrap_or_else(|lie| panic!("{context}\n{lie}"));
            check_sources(
                &session,
                stop,
                ThreadId::new(thread),
                &trace,
                reported.len(),
                &context,
            )
            .await;
        }

        session.shutdown().await;
    }
}

#[tokio::test]
async fn a_cores_python_frames_are_the_ones_cpython_reported() {
    for interpreter in CORES {
        let name = format!("python-stacks-{interpreter}.core");
        let core = Scenario::fixture(&name);
        let log = std::fs::read_to_string(core.with_extension("core.log"))
            .expect("the core's log; run `just build-test-programs`");
        let report = Report::parse(&log);
        let truth = report
            .checkpoint("parked")
            .expect("the checkpoint the core was taken at")
            .to_vec();
        let threads = reported_threads(&truth);
        let lines = CodeLines::of(&report);
        assert_eq!(threads.len(), 3, "{name}: {truth:?}");

        let mut scenario = Scenario::open_core(&name, &CoreDumpOptions::new(core));
        let InferiorState::Stopped { stop_id, .. } = scenario.snapshot().await.inferior else {
            panic!("{name}: the core is not stopped");
        };

        for (thread, printer, reported) in threads {
            let context = StopContext {
                stop: stop_id,
                execution: ExecutionContext::Thread(ThreadId::new(thread)),
                frame: StackFrameId::INNERMOST,
            };

            let trace = scenario
                .operation(
                    "thread backtrace",
                    scenario.handle().at(context).backtrace(),
                )
                .await;

            let beneath = usize::from(printer);
            let context = format!("{name} thread {thread}: {trace:#?}");
            check_frames(&trace, &reported, beneath)
                .unwrap_or_else(|lie| panic!("{context}\n{lie}"));
            lines
                .check(&trace)
                .unwrap_or_else(|lie| panic!("{context}\n{lie}"));
        }

        scenario.shutdown().await;
    }
}

async fn check_sources(
    session: &PythonSession,
    stop: StopId,
    thread: ThreadId,
    trace: &Backtrace,
    reported: usize,
    context: &str,
) {
    let interpreted: Vec<_> = trace
        .frames
        .iter()
        .filter(|frame| matches!(frame.kind, FrameKind::Interpreted { .. }))
        .collect();
    assert!(interpreted.len() >= reported, "{context}");
    for frame in interpreted {
        let place = frame
            .recorded
            .as_ref()
            .and_then(|recorded| recorded.place.as_ref())
            .expect(context);

        let at = StopContext {
            stop,
            execution: ExecutionContext::Thread(thread),
            frame: frame.id,
        };

        let location = session.scenario.handle().at(at).location().await;
        assert!(
            matches!(location, Err(microscope::Error::FrameInterpreted)),
            "{location:?}: {context}"
        );

        let source = session
            .scenario
            .operation(
                "frame source",
                session.scenario.handle().at(at).source_context(1),
            )
            .await;

        let text = std::fs::read_to_string(&*place.path).expect(context);
        let line = usize::try_from(place.line).expect("a line number");
        let expected = text.lines().nth(line - 1).expect(context);
        assert_eq!(**source.recorded, *place.path, "{context}");
        assert_eq!(source.line.get(), u64::from(place.line), "{context}");

        let current = source
            .lines
            .iter()
            .find(|shown| shown.number == source.line)
            .expect(context);
        assert_eq!(&*current.text, expected, "{context}");
        assert!(source.lines.len() > 1, "{context}");
    }
}

#[tokio::test]
async fn a_pid_namespaces_python_frames_match_the_reported_target_threads() {
    for interpreter in ["3.14", "3.14t"] {
        let session = check_namespaced_frames(interpreter, &[]).await;
        session
            .scenario
            .operation("kill namespace init", session.scenario.handle().kill())
            .await;
        session.shutdown().await;
    }
}

#[tokio::test]
async fn a_non_utf8_process_leader_name_preserves_attach_and_inspection() {
    for interpreter in ["3.14"] {
        let session = check_namespaced_frames(interpreter, &["--leader-name"]).await;
        session.detach_and_finish().await;
    }
}

async fn check_namespaced_frames(interpreter: &str, arguments: &[&str]) -> PythonSession {
    let (mut session, truth) = PythonSession::attach_in_pid_namespace_at(
        interpreter,
        "thread_ids.py",
        "parked",
        arguments,
    )
    .await;

    let snapshot = session.scenario.snapshot().await;
    let threads = snapshot.threads;
    assert_eq!(threads.len(), 2, "{}: {threads:?}", session.context());

    let host_process = match snapshot.inferior {
        InferiorState::Stopped { process_id, .. } => process_id.get(),
        other => panic!("not stopped: {other:?}"),
    };

    for (native, printer, reported) in reported_threads(&truth) {
        let host = if printer {
            host_process
        } else {
            threads
                .iter()
                .find(|thread| thread.id.get() != host_process)
                .expect("worker host thread")
                .id
                .get()
        };
        assert_ne!(host, native);

        let trace = session.backtrace(ThreadId::new(host)).await;
        check_frames(&trace, &reported, if printer { 2 } else { 0 }).unwrap_or_else(|lie| {
            panic!(
                "{} native {native}, host {host}: {lie}\n{trace:#?}",
                session.context()
            )
        });
    }

    session
}
