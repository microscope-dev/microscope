//! What a Go fixture reports about itself, and a session that collects it.
//!
//! A fixture prints tab-separated `TRUTH` lines at each checkpoint, then
//! calls `main.reached`, where the session stops. The lines come from the
//! runtime's own goroutine dump, so they stay right when the toolchain
//! moves.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Stdio;

use microscope::{LaunchOptions, StopReason, TaskPage, TaskSnapshot, TaskState};

use crate::support::truth::{Report, unescape};
use crate::support::{Scenario, ScratchDir};

/// One goroutine as the runtime's dump shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpedTask {
    /// Its status: the reason it waits, or a word such as `running`.
    pub status: String,

    /// Its frames, innermost first, without the runtime's own: each a
    /// function and its `file:line`.
    pub frames: Vec<(String, String)>,
}

/// What the program reported at one checkpoint.
#[derive(Debug, Default)]
pub struct Checkpoint {
    /// `runtime.NumGoroutine()`.
    pub count: usize,

    /// The goroutine that reached the checkpoint, and its thread.
    pub main: (u64, u64),

    /// Every goroutine the program started, by id.
    pub tasks: BTreeMap<u64, DumpedTask>,
}

impl Checkpoint {
    fn parse(lines: &[Vec<String>]) -> Self {
        let mut checkpoint = Self::default();
        let number = |text: &str| text.parse::<u64>().expect("a number");
        for fields in lines {
            let fields = fields.iter().map(String::as_str).collect::<Vec<_>>();
            match fields.as_slice() {
                ["count", count] => checkpoint.count = count.parse().expect("a count"),
                ["main", id, thread] => checkpoint.main = (number(id), number(thread)),
                ["task", id, status] => {
                    checkpoint.tasks.insert(
                        number(id),
                        DumpedTask {
                            status: (*status).to_owned(),
                            frames: Vec::new(),
                        },
                    );
                }
                ["frame", id, function, position] => checkpoint
                    .tasks
                    .get_mut(&number(id))
                    .expect("a frame follows its task")
                    .frames
                    .push(((*function).to_owned(), (*position).to_owned())),
                _ => {}
            }
        }

        checkpoint
    }

    /// Whether the program's goroutines among `tasks` are exactly those
    /// the runtime dumped, each in the state the dump gives it. The
    /// runtime's own goroutines, which the dump leaves out, are not
    /// compared.
    pub fn check_tasks(&self, tasks: &[TaskSnapshot]) -> Result<(), String> {
        let program = tasks
            .iter()
            .filter(|task| !task.internal)
            .collect::<Vec<_>>();
        let ids = program
            .iter()
            .map(|task| task.id.number)
            .collect::<BTreeSet<_>>();
        if ids.len() != program.len() {
            return Err("a goroutine is listed twice".to_owned());
        }

        let dumped = self.tasks.keys().copied().collect::<BTreeSet<_>>();
        if ids != dumped {
            return Err(format!("lists goroutines {ids:?}, not {dumped:?}"));
        }

        if program.len() != self.count {
            return Err(format!("lists {}, not {}", program.len(), self.count));
        }

        for task in program {
            let dumped = &self.tasks[&task.id.number];

            // The runtime describes a goroutine by what it waits for, or
            // by its status.
            if task.detail.as_deref() != Some(dumped.status.as_str()) {
                return Err(format!("describes {task:?} unlike {dumped:?}"));
            }

            let expected = match dumped.status.as_str() {
                "running" | "syscall" => TaskState::Running,
                "runnable" => TaskState::Runnable,
                _ => TaskState::Blocked,
            };

            if task.state != expected {
                return Err(format!("gives {task:?} a state unlike {dumped:?}"));
            }
        }

        Ok(())
    }
}

impl DumpedTask {
    /// Whether frames shown as functions and `file:line`, innermost first,
    /// are those the dump shows.
    pub fn check_frames(&self, shown: &[(String, String)]) -> Result<(), String> {
        if shown == self.frames.as_slice() {
            Ok(())
        } else {
            Err(format!("shows {shown:?}, not {:?}", self.frames))
        }
    }
}

/// A debugged Go program and what it printed.
pub struct GoSession {
    pub scenario: Scenario,
    pub fixture: String,
    output: PathBuf,
    _scratch: ScratchDir,
}

impl GoSession {
    /// Launches a fixture that stops at each of its checkpoints.
    pub async fn launch(fixture: &str) -> Self {
        let scratch = ScratchDir::new("go");
        let output = scratch.path().join("stdout");
        let file = std::fs::File::create(&output).expect("create the fixture's output");
        let errors = file.try_clone().expect("share the fixture's output");
        let mut scenario = crate::invariants::checked(fixture);
        scenario.add_breakpoint("main.reached").await;
        let reason = scenario
            .run_with_to_stop(LaunchOptions {
                stdout: Some(Stdio::from(file)),
                stderr: Some(Stdio::from(errors)),
                ..LaunchOptions::default()
            })
            .await;
        assert!(
            matches!(reason, StopReason::Breakpoint { .. }),
            "{fixture}: {reason:?}"
        );
        Self {
            scenario,
            fixture: fixture.to_owned(),
            output,
            _scratch: scratch,
        }
    }

    /// What the program reported at its latest checkpoint, which must be
    /// `name`.
    pub fn checkpoint(&self, name: &str) -> Checkpoint {
        let report = Report::read(&self.output);
        let (latest, lines) = report
            .latest()
            .unwrap_or_else(|| panic!("{}: no checkpoint in {report:?}", self.fixture));
        assert_eq!(latest, name, "{}", self.fixture);
        Checkpoint::parse(lines)
    }

    /// Every task of the stopped program, read a page at a time.
    pub async fn tasks(&self, page: usize) -> (Vec<TaskSnapshot>, Vec<String>) {
        let mut tasks = Vec::new();
        let mut gaps = Vec::new();
        let mut from = None;
        loop {
            let TaskPage {
                tasks: found,
                next,
                gaps: missing,
                ..
            } = self
                .scenario
                .operation("tasks", self.scenario.handle().tasks(from, page))
                .await;
            assert!(found.len() <= page, "{}", self.fixture);
            tasks.extend(found.iter().cloned());
            gaps.extend(missing.iter().map(ToString::to_string));
            match next {
                Some(next) => from = Some(next),
                None => return (tasks, gaps),
            }
        }
    }
}

/// The comparison fails on the lies it exists to catch: a goroutine left
/// out, one listed twice, one in the wrong state, and a dropped frame.
#[tokio::test]
async fn the_truth_fails_on_the_lies_it_looks_for() {
    let session = GoSession::launch("workers-go-o0").await;
    let truth = session.checkpoint("parked");
    let (tasks, gaps) = session.tasks(64).await;
    assert!(gaps.is_empty(), "{gaps:?}");
    truth.check_tasks(&tasks).expect("the listing holds");
    let program = tasks
        .iter()
        .position(|task| !task.internal && task.thread.is_none())
        .expect("a parked goroutine of the program's");

    let mut missing = tasks.clone();
    missing.remove(program);
    assert!(truth.check_tasks(&missing).is_err());
    let mut twice = tasks.clone();
    twice.push(tasks[program].clone());
    assert!(truth.check_tasks(&twice).is_err());
    let mut running = tasks.clone();
    running[program].state = TaskState::Running;
    assert!(truth.check_tasks(&running).is_err());

    let dumped = &truth.tasks[&tasks[program].id.number];
    dumped
        .check_frames(&dumped.frames)
        .expect("the dump's frames");
    assert!(dumped.check_frames(&dumped.frames[1..]).is_err());
    session.scenario.shutdown().await;
}

const PRINTED: &str = "starting\n\
    TRUTH\tversion\t3.14\n\
    TRUTH\tcheckpoint\tfirst\n\
    TRUTH\tvalue\tx\t1\n\
    unrelated output\n\
    TRUTH\tcheckpoint\tsecond\n\
    TRUTH\tvalue\tx\t'a\\tb\\\\c\\nd'\n\
    TRUTH\tcheckpoint\tfirst\n\
    TRUTH\tvalue\tx\t3\n";

fn fields(line: &[&str]) -> Vec<String> {
    line.iter().map(ToString::to_string).collect()
}

/// The shared reader of `TRUTH` lines, which every suite's comparisons
/// rest on.
#[test]
fn a_checkpoint_holds_the_lines_up_to_the_next() {
    let report = Report::parse(PRINTED);
    assert_eq!(report.lines().count(), 7);
    assert_eq!(
        report.checkpoint("second").expect("the second checkpoint"),
        [fields(&["value", "x", "'a\tb\\c\nd'"])]
    );
    assert_eq!(
        report
            .checkpoint("first")
            .expect("the last first checkpoint"),
        [fields(&["value", "x", "3"])],
        "a name reached twice is its last checkpoint"
    );
    assert_eq!(report.checkpoint("third"), None);
    let (name, lines) = report.latest().expect("a checkpoint");
    assert_eq!((name, lines.len()), ("first", 1));
}

#[test]
fn escapes_are_undone_and_other_backslashes_kept() {
    assert_eq!(unescape(r"a\tb\nc\\d"), "a\tb\nc\\d");
    assert_eq!(unescape(r"\x\"), r"\x\");
    assert_eq!(unescape(""), "");
}
