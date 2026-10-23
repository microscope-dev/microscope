//! A Node fixture under the debugger: launched with its output in a file,
//! its input a pipe, and no environment, and stopped wherever it parks at a
//! checkpoint.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use microscope::{InferiorState, LaunchOptions, StateSnapshot, StopReason};
use tokio::task::JoinHandle;

use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir, node};

/// Every Node build in the matrix, as the flake names it.
pub const MATRIX: [&str; 6] = ["26", "24", "22", "nixpkgs-26", "nixpkgs-24", "nixpkgs-22"];

/// The builds the flake provides that the suite does not run here: Debian's
/// shared libnode runs only inside its own mount namespace, in a test group
/// of its own.
pub const SEPARATE: [&str; 1] = ["debian-24"];

/// The builds whose fixtures' cores the build saves, one per V8 line.
pub const CORES: [&str; 3] = ["26", "24", "22"];

/// How long a fixture may take to reach a checkpoint.
const CHECKPOINT_DEADLINE: Duration = Duration::from_secs(30);

/// The major Node version of the build `entry`, as 24 for `nixpkgs-24`.
pub fn major(entry: &str) -> u32 {
    entry
        .rsplit('-')
        .next()
        .and_then(|version| version.parse().ok())
        .unwrap_or_else(|| panic!("no Node version in the build name {entry}"))
}

/// One fixture, run by one Node build.
pub struct NodeSession {
    pub scenario: Scenario,

    /// The build's name in the matrix, as `26`.
    pub entry: String,
    pub fixture: String,
    output: PathBuf,
    input: Option<std::io::PipeWriter>,
    running: Option<JoinHandle<microscope::Result<StopReason>>>,
    _scratch: ScratchDir,
}

/// Waits for the fixture writing `output` to park at the checkpoint
/// `name`, and returns what it reported there.
async fn reported(
    output: &Path,
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
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

impl NodeSession {
    /// Launches `fixture` with `fixture_arguments` under the Node build
    /// `entry`, given `node_arguments`, running, with an empty environment.
    pub async fn launch(
        entry: &str,
        fixture: &str,
        node_arguments: &[&str],
        fixture_arguments: &[&str],
    ) -> Self {
        let scratch = ScratchDir::new("node");
        let output = scratch.path().join("stdout");
        let file = std::fs::File::create(&output).expect("create the fixture's output");
        let errors = file.try_clone().expect("share the fixture's output");
        let (reader, input) = std::io::pipe().expect("the fixture's input");

        let mut arguments: Vec<std::ffi::OsString> =
            node_arguments.iter().map(Into::into).collect();
        arguments.push(node::fixture(fixture).into_os_string());
        arguments.extend(fixture_arguments.iter().map(Into::into));

        // No NODE_OPTIONS, or any other variable, reaches the fixture.
        let environment = std::env::vars_os().map(|(name, _)| (name, None)).collect();

        let mut scenario = Scenario::new(format!("node {entry} {fixture}"), node::node(entry));
        let running = scenario
            .start_running_with(LaunchOptions {
                arguments,
                environment,
                stdin: Some(Stdio::from(reader)),
                stdout: Some(Stdio::from(file)),
                stderr: Some(Stdio::from(errors)),
                ..LaunchOptions::default()
            })
            .await;

        Self {
            scenario,
            entry: entry.to_owned(),
            fixture: fixture.to_owned(),
            output,
            input: Some(input),
            running: Some(running),
            _scratch: scratch,
        }
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

    /// The stop the fixture is held at, with every thread it lists.
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

    /// Everything the fixture has reported so far.
    pub fn report(&self) -> Report {
        Report::read(&self.output)
    }

    /// The build and fixture, for messages.
    pub fn context(&self) -> String {
        format!("node {} {}", self.entry, self.fixture)
    }

    /// Ends the fixture and the debugger.
    pub async fn shutdown(mut self) {
        drop(self.input.take());
        self.scenario.shutdown().await;
    }
}

/// The matrix is every build the dev shell provides, so that one the flake
/// adds is tested, or this says it is not.
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
