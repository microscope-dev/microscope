use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use microscope::{
    BreakpointOptions, BreakpointSpec, DebuggerEvent, InferiorState, LaunchOptions, StateSnapshot,
    StopReason,
};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir, node};

pub const MATRIX: [&str; 6] = ["26", "24", "22", "nixpkgs-26", "nixpkgs-24", "nixpkgs-22"];

pub const SEPARATE: [&str; 2] = ["debian-24", "26-without-postmortem"];

pub const LINES: [&str; 3] = ["26", "24", "22"];

pub const CORES: [&str; 3] = ["26", "24", "22"];

const CHECKPOINT_DEADLINE: Duration = Duration::from_secs(30);

pub fn major(entry: &str) -> u32 {
    entry
        .rsplit('-')
        .next()
        .and_then(|version| version.parse().ok())
        .unwrap_or_else(|| panic!("no Node version in the build name {entry}"))
}

pub struct NodeSession {
    pub scenario: Scenario,

    pub entry: String,
    pub fixture: String,
    output: PathBuf,
    input: Option<std::io::PipeWriter>,
    running: Option<JoinHandle<microscope::Result<StopReason>>>,
    _scratch: ScratchDir,
}

async fn reported(
    output: &Path,
    name: &str,
    context: &str,
    mut ended: impl FnMut() -> bool,
) -> Vec<Vec<String>> {
    let began = Instant::now();
    loop {
        let gone = ended();
        let text = std::fs::read_to_string(output).unwrap_or_default();
        let ready = text
            .rsplit_once("TRUTH\tcheckpoint\t")
            .is_some_and(|(_, last)| last.contains("\nREADY\n"));
        if ready
            && let Some(lines) = Report::parse(&text).checkpoint(name)
            && printer_reads_input(&text)
        {
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
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn printer_reads_input(text: &str) -> bool {
    let Some(pid) = text
        .lines()
        .find_map(|line| line.strip_prefix("TRUTH\tprocess\t"))
        .and_then(|rest| rest.split('\t').next())
    else {
        return false;
    };
    let call = std::fs::read_to_string(format!("/proc/{pid}/syscall")).unwrap_or_default();
    let mut fields = call.split_whitespace();
    matches!(fields.next(), Some("0" | "17" | "19" | "295")) && fields.next() == Some("0x0")
}

impl NodeSession {
    pub async fn launch(
        entry: &str,
        fixture: &str,
        node_arguments: &[&str],
        fixture_arguments: &[&str],
    ) -> Self {
        let (mut session, options) =
            Self::prepare(entry, fixture, node_arguments, fixture_arguments);
        session.running = Some(session.scenario.start_running_with(options).await);
        session
    }

    pub async fn launch_to_stop(
        entry: &str,
        fixture: &str,
        breakpoints: Vec<(BreakpointSpec, BreakpointOptions)>,
    ) -> (Self, StopReason, broadcast::Receiver<DebuggerEvent>) {
        Self::unlaunched(entry, fixture)
            .run_to_stop(breakpoints)
            .await
    }

    pub fn unlaunched(entry: &str, fixture: &str) -> Pending {
        Self::unlaunched_with(entry, fixture, &[])
    }

    pub fn unlaunched_with(entry: &str, fixture: &str, fixture_arguments: &[&str]) -> Pending {
        let (session, options) = Self::prepare(entry, fixture, &[], fixture_arguments);
        Pending { session, options }
    }

    pub fn unlaunched_with_node(entry: &str, script: &str, node_arguments: &[&str]) -> Pending {
        let (session, options) = Self::prepare(entry, script, node_arguments, &[]);
        Pending { session, options }
    }

    fn prepare(
        entry: &str,
        fixture: &str,
        node_arguments: &[&str],
        fixture_arguments: &[&str],
    ) -> (Self, LaunchOptions) {
        let scratch = ScratchDir::new("node");
        let output = scratch.path().join("stdout");
        let file = std::fs::File::create(&output).expect("create the fixture's output");
        let errors = file.try_clone().expect("share the fixture's output");
        let (reader, input) = std::io::pipe().expect("the fixture's input");

        let mut arguments: Vec<std::ffi::OsString> =
            node_arguments.iter().map(Into::into).collect();
        arguments.push(node::fixture(fixture).into_os_string());
        arguments.extend(fixture_arguments.iter().map(Into::into));

        let environment = std::env::vars_os().map(|(name, _)| (name, None)).collect();
        let options = LaunchOptions {
            arguments,
            environment,
            stdin: Some(Stdio::from(reader)),
            stdout: Some(Stdio::from(file)),
            stderr: Some(Stdio::from(errors)),
            ..LaunchOptions::default()
        };
        let session = Self {
            scenario: Scenario::for_fixture(format!("node {entry} {fixture}"), node::node(entry)),
            entry: entry.to_owned(),
            fixture: fixture.to_owned(),
            output,
            input: Some(input),
            running: None,
            _scratch: scratch,
        };
        (session, options)
    }

    pub fn output(&self) -> String {
        std::fs::read_to_string(&self.output).unwrap_or_default()
    }

    pub async fn stop_at(&mut self, name: &str) -> Vec<Vec<String>> {
        let running = self.running.as_ref();
        let lines = reported(&self.output, name, &self.context(), || {
            running.is_some_and(JoinHandle::is_finished)
        })
        .await;

        self.pause().await;
        lines
    }

    pub async fn wrote(&self, line: &str) {
        let started = Instant::now();
        let running = self.running.as_ref().expect("the fixture runs");
        while !self.output().lines().any(|written| written == line) {
            assert!(
                !running.is_finished(),
                "{}: stopped before {line:?}\n{}",
                self.context(),
                self.output()
            );
            assert!(
                started.elapsed() < CHECKPOINT_DEADLINE,
                "{}: never wrote {line:?}",
                self.context()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn reading_input(&self) {
        let started = Instant::now();
        loop {
            let snapshot = self
                .scenario
                .operation("snapshot", self.scenario.handle().snapshot())
                .await;
            if let microscope::InferiorState::Running { process_id, .. } = snapshot.inferior {
                let path = format!("/proc/{}/syscall", process_id.get());
                let call = std::fs::read_to_string(path).unwrap_or_default();
                if call.split(' ').take(2).eq(["0", "0x0"]) {
                    return;
                }
            }
            assert!(
                started.elapsed() < CHECKPOINT_DEADLINE,
                "{}: never blocked reading its input",
                self.context()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

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

    pub async fn resume(&mut self) {
        assert!(
            self.running.is_none(),
            "{}: already running",
            self.context()
        );
        self.running = Some(self.scenario.start_resuming().await);
    }

    pub async fn proceed(&mut self) {
        self.feed();
        self.resume().await;
    }

    pub fn feed(&mut self) {
        use std::io::Write;

        self.input
            .as_mut()
            .expect("the fixture's input")
            .write_all(b"\n")
            .expect("write the fixture's input");
    }

    pub async fn stopped(&mut self) -> StateSnapshot {
        let snapshot = self.scenario.snapshot().await;
        assert!(
            matches!(snapshot.inferior, InferiorState::Stopped { .. }),
            "{}: not stopped: {:?}",
            self.context(),
            snapshot.inferior
        );
        snapshot
    }

    pub fn report(&self) -> Report {
        Report::read(&self.output)
    }

    pub fn context(&self) -> String {
        format!("node {} {}", self.entry, self.fixture)
    }

    pub async fn shutdown(mut self) {
        drop(self.input.take());
        self.scenario.shutdown().await;
    }
}

pub struct Pending {
    session: NodeSession,
    options: LaunchOptions,
}

impl std::ops::Deref for Pending {
    type Target = NodeSession;

    fn deref(&self) -> &NodeSession {
        &self.session
    }
}

impl Pending {
    pub async fn run_to_stop(
        self,
        breakpoints: Vec<(BreakpointSpec, BreakpointOptions)>,
    ) -> (NodeSession, StopReason, broadcast::Receiver<DebuggerEvent>) {
        let Self {
            mut session,
            options,
        } = self;
        for (spec, options) in breakpoints {
            session
                .scenario
                .operation(
                    "add breakpoint",
                    session.scenario.handle().add_breakpoint_with(spec, options),
                )
                .await;
        }
        let events = session.scenario.handle().subscribe();
        let reason = session.scenario.run_with_to_stop(options).await;
        (session, reason, events)
    }
}

#[test]
fn the_matrix_is_every_build_the_dev_shell_provides() {
    let root = std::env::var_os("MICROSCOPE_NODE")
        .expect("MICROSCOPE_NODE names the Node builds; run tests in the dev shell");

    let mut provided: Vec<String> = std::fs::read_dir(root)
        .expect("the builds' directory")
        .map(|entry| {
            entry
                .expect("a build")
                .file_name()
                .into_string()
                .expect("a UTF-8 name")
        })
        .collect();
    provided.sort();

    let mut known: Vec<String> = MATRIX
        .iter()
        .chain(&SEPARATE)
        .map(|&e| e.to_owned())
        .collect();
    known.sort();

    assert_eq!(known, provided);
    assert!(CORES.iter().all(|entry| MATRIX.contains(entry)));
}

#[tokio::test]
async fn repeated_sessions_read_the_node_program_once() {
    let recording = microscope::timings::Recording::start(microscope::timings::Options::default())
        .expect("record loads");
    for _ in 0..2 {
        let pending = NodeSession::unlaunched("nixpkgs-26", "exceptions.js");
        pending.session.shutdown().await;
    }
    let report = recording.finish().expect("load timings");
    let loads = report.phases.iter().find(|phase| phase.name == "load");
    assert_eq!(
        loads.map(|phase| phase.spans),
        Some(1),
        "{:?}",
        report.phases
    );
}
