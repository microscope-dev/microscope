//! A Python fixture under the debugger: launched with its output in a file
//! and its input a pipe, and stopped wherever it parks at a checkpoint.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use microscope::{
    Backtrace, BreakpointOptions, BreakpointSpec, DebugFileOptions, Debugger, DebuggerEvent,
    Evaluation, ExecutionContext, Expression, InferiorState, LaunchOptions, ProcessId, ScalarValue,
    StackFrameId, StopContext, StopId, StopReason, ThreadId, VariableState, VariableValue,
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir, python};

/// Every interpreter in the matrix, as the flake names it.
pub const MATRIX: [&str; 10] = [
    "3.12",
    "3.13",
    "3.13t",
    "3.14",
    "3.14t",
    "3.15",
    "3.15t",
    "debian-3.13",
    "ubuntu-3.12",
    "standalone-3.14",
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
    pub fixture: String,
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

/// The interpreter's arguments that run `fixture` with `arguments`,
/// isolated from the user's site and the script's directory, and from
/// `PYTHON*` variables unless it is `configured` by them.
fn command(
    interpreter: &str,
    fixture: &str,
    arguments: &[&str],
    configured: bool,
) -> Vec<std::ffi::OsString> {
    // 3.12 and 3.13 take the agent through the launch bootstrap, which
    // PYTHONPATH leads them to and `-I` would ignore.
    let bootstrapped = ["3.12", "3.13"]
        .iter()
        .any(|version| interpreter.trim_end_matches('t').ends_with(version));
    // Fixtures write no bytecode beside their sources, which would make
    // the fixtures' build think they changed.
    let isolation: &[&str] = if configured || bootstrapped {
        &["-P", "-s"]
    } else {
        &["-I"]
    };
    let mut command: Vec<std::ffi::OsString> = isolation.iter().map(Into::into).collect();
    command.push("-B".into());
    command.push(python::fixture(fixture).into_os_string());
    command.extend(arguments.iter().map(Into::into));
    command
}

/// Waits for the fixture writing `output` to park at the checkpoint
/// `name`, and returns what it reported there.
async fn reported(
    output: &std::path::Path,
    name: &str,
    context: &str,
    mut ended: impl FnMut() -> bool,
) -> Vec<Vec<String>> {
    let began = Instant::now();
    loop {
        // Asked before the output is read, so that a fixture that ended
        // after printing the checkpoint is not taken for one that never did.
        let gone = ended();
        let text = std::fs::read_to_string(output).unwrap_or_default();
        let ready = text
            .rsplit_once("TRUTH\tcheckpoint\t")
            .is_some_and(|(_, last)| last.contains("\nREADY\n"));
        if ready && let Some(lines) = Report::parse(&text).checkpoint(name) {
            return lines.to_vec();
        }

        assert!(
            !gone,
            "{context}: the fixture ended before checkpoint {name}:\n{text}"
        );
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
        Self::launch_with(interpreter, fixture, arguments, &[]).await
    }

    /// Launches `fixture` with `arguments` under `interpreter`, running,
    /// with each variable of `environment` set.
    pub async fn launch_with(
        interpreter: &str,
        fixture: &str,
        arguments: &[&str],
        environment: &[(&str, &str)],
    ) -> Self {
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
                arguments: command(interpreter, fixture, arguments, !environment.is_empty()),
                environment: environment
                    .iter()
                    .map(|(name, value)| ((*name).into(), Some((*value).into())))
                    .collect(),
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
                arguments: command(interpreter, fixture, arguments, false),
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
        Self::attach_at_in(interpreter, fixture, name, false, &[]).await
    }

    /// A real nested PID namespace, retaining the outer proc mount so the
    /// fixture can report its debugger-visible PID separately from `getpid()`.
    pub async fn attach_in_pid_namespace_at(
        interpreter: &str,
        fixture: &str,
        name: &str,
        arguments: &[&str],
    ) -> (Self, Vec<Vec<String>>) {
        Self::attach_at_in(interpreter, fixture, name, true, arguments).await
    }

    async fn attach_at_in(
        interpreter: &str,
        fixture: &str,
        name: &str,
        namespace: bool,
        arguments: &[&str],
    ) -> (Self, Vec<Vec<String>>) {
        let streams = Streams::new();
        let mut command = if namespace {
            let mut command = std::process::Command::new("unshare");
            command.args([
                "--user",
                "--map-root-user",
                "--pid",
                "--fork",
                "--kill-child=KILL",
            ]);
            command.arg(python::interpreter(interpreter));
            command
        } else {
            std::process::Command::new(python::interpreter(interpreter))
        };
        let child = Ended(
            command
                .args(self::command(interpreter, fixture, arguments, false))
                .stdin(Stdio::from(streams.reader))
                .stdout(Stdio::from(streams.file))
                .stderr(Stdio::from(streams.errors))
                .spawn()
                .expect("start the fixture"),
        );
        let context = format!("{interpreter} {fixture} attached");
        let mut child = child;
        let truth = reported(&streams.output, name, &context, || {
            child.0.try_wait().ok().flatten().is_some()
        })
        .await;
        let debug_files = DebugFileOptions {
            directories: python::debug_directory(interpreter).into_iter().collect(),
            ..DebugFileOptions::default()
        };
        let process = if namespace {
            let host = Report::read(&streams.output)
                .lines()
                .find(|fields| fields[0] == "host_pid")
                .expect("the namespace fixture reports its host PID")[1]
                .parse()
                .expect("a host PID");
            assert_ne!(
                host,
                u64::from(child.0.id()),
                "the unshare parent is not the interpreter"
            );
            ProcessId::new(host)
        } else {
            ProcessId::new(u64::from(child.0.id()))
        };
        let debugger = tokio::time::timeout(
            CHECKPOINT_DEADLINE,
            Debugger::attach_with(process, None, &debug_files),
        )
        .await
        .expect("attach deadline")
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
        let running = self.running.as_ref();
        let lines = reported(&self.output, name, &self.context(), || {
            running.is_some_and(JoinHandle::is_finished)
        })
        .await;
        self.pause().await;
        lines
    }

    /// Stops the running fixture wherever it is.
    pub async fn pause(&mut self) {
        let running = self.running.take().expect("the fixture runs");
        self.scenario
            .operation("pause", self.scenario.handle().pause())
            .await;
        let ended = tokio::time::timeout(CHECKPOINT_DEADLINE, running)
            .await
            .unwrap_or_else(|_| panic!("{}: the pause never stopped it", self.context()))
            .expect("the run task")
            .expect("the run");
        assert_eq!(ended, StopReason::Pause, "{}", self.context());
    }

    /// Waits for the fixture's output to contain `text`.
    pub async fn wait_for_output(&self, text: &str) {
        let began = Instant::now();
        loop {
            let ended = self.running.as_ref().is_some_and(JoinHandle::is_finished);
            if self.output().contains(text) {
                return;
            }
            assert!(
                !ended,
                "{}: the fixture ended with no {text:?} in:\n{}",
                self.context(),
                self.output()
            );
            assert!(
                began.elapsed() < CHECKPOINT_DEADLINE,
                "{}: no {text:?} in:\n{}",
                self.context(),
                self.output()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Starts the stopped fixture without releasing any application checkpoint.
    pub async fn resume(&mut self) {
        self.running = Some(self.scenario.start_resuming().await);
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

    /// Waits for the current execution to reach an observable public stop.
    pub async fn finish_run(&mut self) -> StopReason {
        let running = self.running.take().expect("running fixture");
        tokio::time::timeout(CHECKPOINT_DEADLINE, running)
            .await
            .expect("execution deadline")
            .expect("run task")
            .expect("run result")
    }

    /// The stop the fixture is held at.
    pub async fn stop(&mut self) -> StopId {
        let snapshot = self.scenario.snapshot().await;
        let InferiorState::Stopped { stop_id, .. } = snapshot.inferior else {
            panic!("{}: not stopped", self.context());
        };
        stop_id
    }

    /// Selects the frame of the stop's thread that runs the Python
    /// function `function`.
    pub async fn select_python_frame(&self, function: &str) {
        let trace = self
            .scenario
            .operation("backtrace", self.scenario.handle().backtrace())
            .await;
        let frame = trace
            .frames
            .iter()
            .find(|frame| {
                frame
                    .recorded
                    .as_ref()
                    .is_some_and(|recorded| &*recorded.function == function)
            })
            .unwrap_or_else(|| panic!("{}: no {function} frame: {trace:?}", self.context()))
            .id;
        self.scenario
            .operation("select", self.scenario.handle().select_frame(frame))
            .await;
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

    /// The fixture's complete output so far, including stderr.
    pub fn output(&self) -> String {
        std::fs::read_to_string(&self.output).expect("read fixture output")
    }

    /// The interpreter and fixture, for messages.
    pub fn context(&self) -> String {
        format!("{} {}", self.interpreter, self.fixture)
    }

    /// How an expression's value presents in the selected frame, in brief.
    pub async fn summary(&self, text: &str) -> String {
        let context = self.context();
        let expression = Expression::parse(text).expect("an expression");
        let evaluation = self
            .scenario
            .operation(text, self.scenario.handle().evaluate(&expression))
            .await;
        match evaluation {
            Evaluation::Value { value, .. } => match value.state {
                VariableState::Available {
                    presentation: Some(presentation),
                    ..
                } => presentation.summary.to_string(),
                state => panic!("{context}: `{text}` presents nothing: {state:?}"),
            },
            other => panic!("{context}: `{text}` has no value: {other:?}"),
        }
    }

    /// The integer an expression has in the selected frame.
    pub async fn integer(&self, text: &str) -> i128 {
        let context = self.context();
        let expression = Expression::parse(text).expect("an expression");
        let evaluation = self
            .scenario
            .operation(text, self.scenario.handle().evaluate(&expression))
            .await;
        let Evaluation::Value { value, .. } = evaluation else {
            panic!("{context}: `{text}` has no value");
        };
        match value.state {
            VariableState::Available {
                value: VariableValue::Scalar(scalar),
                ..
            } if let Some(value) = as_integer(&scalar) => value,
            state => panic!("{context}: `{text}` is not an integer: {state:?}"),
        }
    }

    /// Detaches, releases the checkpoint, and waits for a fixture whose
    /// contract is to exit after that checkpoint. The owned child is reaped.
    pub async fn detach_and_finish(mut self) {
        self.input
            .write_all(b"\n")
            .expect("release fixture after detach");
        self.scenario.shutdown().await;
        let child = self.attached.as_mut().expect("an attached fixture owner");
        let deadline = Instant::now() + CHECKPOINT_DEADLINE;
        let status = loop {
            if let Some(status) = child.0.try_wait().expect("poll detached fixture") {
                break status;
            }
            assert!(Instant::now() < deadline, "detached fixture did not finish");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(status.success(), "detached fixture failed: {status}");
    }

    /// Detaches at a breakpoint, waits for an application checkpoint and
    /// attaches a new debugger while retaining the external process owner.
    pub async fn reattach_at(mut self, name: &str) -> Self {
        let InferiorState::Stopped {
            process_id: process,
            ..
        } = self.scenario.snapshot().await.inferior
        else {
            panic!("reattach starts at a public stop")
        };
        let context = self.context();
        let Self {
            scenario,
            interpreter,
            fixture,
            output,
            input,
            attached,
            _scratch: scratch,
            running,
        } = self;
        assert!(running.is_none());
        scenario.shutdown().await;
        // The detached fixture is no child of this test's to ask.
        reported(&output, name, &context, || false).await;
        let debug_files = DebugFileOptions {
            directories: python::debug_directory(&interpreter).into_iter().collect(),
            ..DebugFileOptions::default()
        };
        let debugger = tokio::time::timeout(
            CHECKPOINT_DEADLINE,
            Debugger::attach_with(process, None, &debug_files),
        )
        .await
        .expect("reattach deadline")
        .expect("reattach fixture");
        Self {
            scenario: Scenario::attached(context, debugger),
            interpreter,
            fixture,
            output,
            input,
            attached,
            _scratch: scratch,
            running: None,
        }
    }

    /// Ends the fixture and the debugger.
    pub async fn shutdown(self) {
        drop(self.input);
        self.scenario.shutdown().await;
        drop(self.attached);
    }
}

/// The integer a scalar holds, if it holds one.
pub fn as_integer(scalar: &ScalarValue) -> Option<i128> {
    match *scalar {
        ScalarValue::Signed(value) => Some(value),
        ScalarValue::Unsigned(value) => i128::try_from(value).ok(),
        _ => None,
    }
}

/// The matrix is every interpreter the dev shell provides, so that one the
/// flake adds is tested, or this says it is not.
#[test]
fn the_matrix_is_every_interpreter_the_dev_shell_provides() {
    let root = std::env::var_os("MICROSCOPE_CPYTHON")
        .expect("MICROSCOPE_CPYTHON names the interpreters; run tests in the dev shell");
    let mut provided: Vec<String> = std::fs::read_dir(root)
        .expect("the interpreters' directory")
        .map(|entry| {
            entry
                .expect("an interpreter")
                .file_name()
                .into_string()
                .expect("a UTF-8 name")
        })
        .collect();
    provided.sort();
    let mut matrix = MATRIX.map(str::to_owned).to_vec();
    matrix.sort();
    assert_eq!(matrix, provided);
}
