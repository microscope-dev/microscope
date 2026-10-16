//! A Python fixture under the debugger: launched with its output in a file
//! and its input a pipe, and stopped wherever it parks at a checkpoint.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use microscope::{
    Backtrace, DebugFileOptions, ExecutionContext, InferiorState, LaunchOptions, StackFrameId,
    StopContext, StopId, StopReason, ThreadId,
};
use tokio::task::JoinHandle;

use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir, python};

/// How long a fixture may take to reach a checkpoint.
const CHECKPOINT_DEADLINE: Duration = Duration::from_secs(30);

/// One fixture, run by one interpreter.
pub struct PythonSession {
    pub scenario: Scenario,

    /// The interpreter's name in the matrix, as `3.13`.
    pub interpreter: String,
    fixture: String,
    output: PathBuf,
    input: std::io::PipeWriter,
    running: Option<JoinHandle<microscope::Result<StopReason>>>,
    _scratch: ScratchDir,
}

impl PythonSession {
    /// Launches `fixture` with `arguments` under `interpreter`, running.
    pub async fn launch(interpreter: &str, fixture: &str, arguments: &[&str]) -> Self {
        let scratch = ScratchDir::new("python");
        let output = scratch.path().join("stdout");
        let file = std::fs::File::create(&output).expect("create the fixture's output");
        let errors = file.try_clone().expect("share the fixture's output");
        let (reader, input) = std::io::pipe().expect("the fixture's input");
        let debug_files = DebugFileOptions {
            directories: python::debug_directory(interpreter).into_iter().collect(),
            ..DebugFileOptions::default()
        };
        let mut scenario = Scenario::with_debug_files(
            format!("{interpreter} {fixture}"),
            python::interpreter(interpreter),
            &debug_files,
        );
        let mut command = vec!["-I".into(), python::fixture(fixture).into_os_string()];
        command.extend(arguments.iter().map(Into::into));
        let running = scenario
            .start_running_with(LaunchOptions {
                arguments: command,
                stdin: Some(Stdio::from(reader)),
                stdout: Some(Stdio::from(file)),
                stderr: Some(Stdio::from(errors)),
                ..LaunchOptions::default()
            })
            .await;
        Self {
            scenario,
            interpreter: interpreter.to_owned(),
            fixture: fixture.to_owned(),
            output,
            input,
            running: Some(running),
            _scratch: scratch,
        }
    }

    /// Waits for the fixture to park at the checkpoint `name`, then stops
    /// it, and returns what it reported there.
    pub async fn stop_at(&mut self, name: &str) -> Vec<Vec<String>> {
        let began = Instant::now();
        let lines = loop {
            let text = std::fs::read_to_string(&self.output).unwrap_or_default();
            let ready = text
                .rsplit_once("TRUTH\tcheckpoint\t")
                .is_some_and(|(_, last)| last.contains("\nREADY\n"));
            if ready && let Some(lines) = Report::parse(&text).checkpoint(name) {
                break lines.to_vec();
            }

            assert!(
                began.elapsed() < CHECKPOINT_DEADLINE,
                "{}: no checkpoint {name} in:\n{text}",
                self.context()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        };

        let running = self.running.take().expect("the fixture runs");
        self.scenario
            .operation("pause", self.scenario.handle().pause())
            .await;
        let ended = running.await.expect("the run task").expect("the run");
        assert_eq!(ended, StopReason::Pause, "{}", self.context());
        lines
    }

    /// Lets the fixture go on past the checkpoint it parked at.
    #[allow(
        dead_code,
        reason = "fixtures with several checkpoints come with values"
    )]
    pub async fn go_on(&mut self) {
        self.input.write_all(b"\n").expect("release the fixture");
        self.running = Some(self.scenario.start_resuming().await);
    }

    /// The stop the fixture is held at.
    pub async fn stop(&mut self) -> StopId {
        let snapshot = self.scenario.snapshot().await;
        let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
            panic!("{}: not stopped", self.context());
        };
        stop_id
    }

    /// A thread's backtrace at the stop it is held at.
    pub async fn backtrace(&mut self, thread: ThreadId) -> Backtrace {
        let stop = self.stop().await;
        let context = StopContext {
            stop,
            execution: ExecutionContext::Thread(thread),
            frame: StackFrameId::INNERMOST,
        };
        self.scenario
            .operation(
                "thread backtrace",
                self.scenario.handle().at(context).backtrace(),
            )
            .await
    }

    /// The interpreter and fixture, for messages.
    pub fn context(&self) -> String {
        format!("{} {}", self.interpreter, self.fixture)
    }

    /// Ends the fixture and the debugger.
    pub async fn shutdown(self) {
        drop(self.input);
        self.scenario.shutdown().await;
    }
}
