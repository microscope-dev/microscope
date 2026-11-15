use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Stdio;

use microscope::{LaunchOptions, StopReason, TaskPage, TaskSnapshot, TaskState};

use crate::support::truth::Report;
use crate::support::{Scenario, ScratchDir};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpedTask {
    pub status: String,

    pub frames: Vec<(String, String)>,
}

#[derive(Debug, Default)]
pub struct Checkpoint {
    pub count: usize,

    pub main: (u64, u64),

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
    pub fn check_frames(&self, shown: &[(String, String)]) -> Result<(), String> {
        if shown == self.frames.as_slice() {
            Ok(())
        } else {
            Err(format!("shows {shown:?}, not {:?}", self.frames))
        }
    }
}

pub struct GoSession {
    pub scenario: Scenario,
    pub fixture: String,
    output: PathBuf,
    _scratch: ScratchDir,
}

impl GoSession {
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

    pub fn checkpoint(&self, name: &str) -> Checkpoint {
        let report = Report::read(&self.output);
        let (latest, lines) = report
            .latest()
            .unwrap_or_else(|| panic!("{}: no checkpoint in {report:?}", self.fixture));
        assert_eq!(latest, name, "{}", self.fixture);
        Checkpoint::parse(lines)
    }

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
