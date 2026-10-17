//! A Python fixture under the debugger: launched with its output in a file
//! and its input a pipe, and stopped wherever it parks at a checkpoint.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use microscope::{
    Backtrace, BreakpointOptions, BreakpointSpec, DebugFileOptions, Debugger, DebuggerEvent,
    ExecutionContext, InferiorState, LaunchOptions, ProcessId, StackFrameId, StopContext, StopId,
    StopReason, ThreadId,
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir, python};

/// Every interpreter in the matrix, as the flake names it.
pub const MATRIX: [&str; 9] = [
    "3.12",
    "3.13",
    "3.13t",
    "3.14",
    "3.14t",
    "3.15",
    "3.15t",
    "debian-3.13",
    "ubuntu-3.12",
];

/// The interpreters whose fixtures' cores the build saves.
pub const CORES: [&str; 4] = ["3.13", "3.14t", "3.15", "debian-3.13"];

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

    /// A fixture the debugger attached to, which the session ends.
    attached: Option<Ended>,
    _scratch: ScratchDir,
}

/// A process the session started and ends: killed and reaped when it is
/// dropped, whatever else failed.
struct Ended(std::process::Child);

impl Drop for Ended {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Where a fixture writes, and how it is fed: its output in a file, which
/// its errors share, and its input a pipe.
struct Streams {
    scratch: ScratchDir,
    output: PathBuf,
    file: std::fs::File,
    errors: std::fs::File,
    reader: std::io::PipeReader,
    input: std::io::PipeWriter,
}

impl Streams {
    fn new() -> Self {
        let scratch = ScratchDir::new("python");
        let output = scratch.path().join("stdout");
        let file = std::fs::File::create(&output).expect("create the fixture's output");
        let errors = file.try_clone().expect("share the fixture's output");
        let (reader, input) = std::io::pipe().expect("the fixture's input");
        Self {
            scratch,
            output,
            file,
            errors,
            reader,
            input,
        }
    }
}

/// The interpreter's arguments that run `fixture` with `arguments`.
fn command(fixture: &str, arguments: &[&str]) -> Vec<std::ffi::OsString> {
    // Fixtures write no bytecode beside their sources, which would make
    // the fixtures' build think they changed.
    let mut command = vec![
        "-I".into(),
        "-B".into(),
        python::fixture(fixture).into_os_string(),
    ];
    command.extend(arguments.iter().map(Into::into));
    command
}

/// Waits for the fixture writing `output` to park at the checkpoint
/// `name`, and returns what it reported there.
async fn reported(output: &std::path::Path, name: &str, context: &str) -> Vec<Vec<String>> {
    let began = Instant::now();
    loop {
        let text = std::fs::read_to_string(output).unwrap_or_default();
        let ready = text
            .rsplit_once("TRUTH\tcheckpoint\t")
            .is_some_and(|(_, last)| last.contains("\nREADY\n"));
        if ready && let Some(lines) = Report::parse(&text).checkpoint(name) {
            return lines.to_vec();
        }

        assert!(
            began.elapsed() < CHECKPOINT_DEADLINE,
            "{context}: no checkpoint {name} in:\n{text}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

impl PythonSession {
    /// Launches `fixture` with `arguments` under `interpreter`, running.
    pub async fn launch(interpreter: &str, fixture: &str, arguments: &[&str]) -> Self {
        let streams = Streams::new();
        let debug_files = DebugFileOptions {
            directories: python::debug_directory(interpreter).into_iter().collect(),
            ..DebugFileOptions::default()
        };
        let mut scenario = Scenario::with_debug_files(
            format!("{interpreter} {fixture}"),
            python::interpreter(interpreter),
            &debug_files,
        );
        let running = scenario
            .start_running_with(LaunchOptions {
                arguments: command(fixture, arguments),
                stdin: Some(Stdio::from(streams.reader)),
                stdout: Some(Stdio::from(streams.file)),
                stderr: Some(Stdio::from(streams.errors)),
                ..LaunchOptions::default()
            })
            .await;
        Self {
            scenario,
            interpreter: interpreter.to_owned(),
            fixture: fixture.to_owned(),
            output: streams.output,
            input: streams.input,
            running: Some(running),
            attached: None,
            _scratch: streams.scratch,
        }
    }

    /// Launches `fixture` with `arguments` under `interpreter` with
    /// `breakpoints` set, each pending until a module has its code, and
    /// runs it to its first stop; returns its reason, and the debugger's
    /// events from before the launch on.
    pub async fn launch_to_stop(
        interpreter: &str,
        fixture: &str,
        arguments: &[&str],
        breakpoints: Vec<(BreakpointSpec, BreakpointOptions)>,
    ) -> (Self, StopReason, broadcast::Receiver<DebuggerEvent>) {
        let streams = Streams::new();
        let debug_files = DebugFileOptions {
            directories: python::debug_directory(interpreter).into_iter().collect(),
            ..DebugFileOptions::default()
        };
        let mut scenario = Scenario::with_debug_files(
            format!("{interpreter} {fixture}"),
            python::interpreter(interpreter),
            &debug_files,
        );
        for (spec, options) in breakpoints {
            scenario
                .operation(
                    "add breakpoint",
                    scenario.handle().add_breakpoint_with(spec, options),
                )
                .await;
        }

        let events = scenario.handle().subscribe();
        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                arguments: command(fixture, arguments),
                stdin: Some(Stdio::from(streams.reader)),
                stdout: Some(Stdio::from(streams.file)),
                stderr: Some(Stdio::from(streams.errors)),
                ..LaunchOptions::default()
            })
            .await;
        let session = Self {
            scenario,
            interpreter: interpreter.to_owned(),
            fixture: fixture.to_owned(),
            output: streams.output,
            input: streams.input,
            running: None,
            attached: None,
            _scratch: streams.scratch,
        };
        (session, reason, events)
    }

    /// Runs `fixture` under `interpreter` outside the debugger until it
    /// parks at the checkpoint `name`, then attaches to it, which stops
    /// it; and returns what it reported there.
    pub async fn attach_at(
        interpreter: &str,
        fixture: &str,
        name: &str,
    ) -> (Self, Vec<Vec<String>>) {
        let streams = Streams::new();
        let child = Ended(
            std::process::Command::new(python::interpreter(interpreter))
                .args(command(fixture, &[]))
                .stdin(Stdio::from(streams.reader))
                .stdout(Stdio::from(streams.file))
                .stderr(Stdio::from(streams.errors))
                .spawn()
                .expect("start the fixture"),
        );
        let context = format!("{interpreter} {fixture} attached");
        let truth = reported(&streams.output, name, &context).await;
        let debug_files = DebugFileOptions {
            directories: python::debug_directory(interpreter).into_iter().collect(),
            ..DebugFileOptions::default()
        };
        let process = ProcessId::new(u64::from(child.0.id()));
        let debugger = Debugger::attach_with(process, None, &debug_files)
            .await
            .expect("attach to the fixture");
        let session = Self {
            scenario: Scenario::attached(context, debugger),
            interpreter: interpreter.to_owned(),
            fixture: fixture.to_owned(),
            output: streams.output,
            input: streams.input,
            running: None,
            attached: Some(child),
            _scratch: streams.scratch,
        };
        (session, truth)
    }

    /// Waits for the fixture to park at the checkpoint `name`, then stops
    /// it, and returns what it reported there.
    pub async fn stop_at(&mut self, name: &str) -> Vec<Vec<String>> {
        let lines = reported(&self.output, name, &self.context()).await;
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

    /// Everything the fixture has reported so far.
    pub fn report(&self) -> Report {
        Report::read(&self.output)
    }

    /// The interpreter and fixture, for messages.
    pub fn context(&self) -> String {
        format!("{} {}", self.interpreter, self.fixture)
    }

    /// Ends the fixture and the debugger.
    pub async fn shutdown(self) {
        drop(self.input);
        self.scenario.shutdown().await;
        drop(self.attached);
    }
}
