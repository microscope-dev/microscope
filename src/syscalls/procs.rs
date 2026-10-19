//! The traced processes and threads: who made whom, what each runs, and
//! how each ended, from the trace's spawn, exec, and exit events.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::model::{Pid, ProcessEnd, Tid, TraceEvent};

/// One traced process.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Process {
    pub pid: Pid,
    pub parent: Option<Pid>,

    /// Images it ran, in order, with when each exec happened.
    pub images: Vec<(u64, Arc<str>)>,
    pub threads: Vec<Tid>,

    /// When the trace first saw it.
    pub started: u64,
    pub ended: Option<(u64, ProcessEnd)>,
}

/// Every process a trace saw.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessTree {
    processes: BTreeMap<Pid, Process>,

    /// Each thread's process.
    threads: BTreeMap<Tid, Pid>,
}

impl ProcessTree {
    /// Records a process the trace began with, as a launched program or an
    /// attached process.
    pub fn root(&mut self, pid: Pid, time: u64, image: Option<Arc<str>>) {
        let process = self.processes.entry(pid).or_insert_with(|| Process {
            pid,
            started: time,
            threads: vec![pid.leader()],
            ..Process::default()
        });

        if let Some(image) = image {
            process.images.push((time, image));
        }

        self.threads.insert(pid.leader(), pid);
    }

    pub fn apply(&mut self, event: &TraceEvent) {
        match *event {
            TraceEvent::Spawn {
                parent,
                child,
                thread,
                time,
                ..
            } => {
                if thread {
                    self.threads.insert(child, parent);
                    let process = self.processes.entry(parent).or_insert_with(|| Process {
                        pid: parent,
                        started: time,
                        ..Process::default()
                    });

                    if !process.threads.contains(&child) {
                        process.threads.push(child);
                    }
                } else {
                    let pid = Pid::led_by(child);
                    let images = self
                        .processes
                        .get(&parent)
                        .and_then(|process| process.images.last().cloned())
                        .into_iter()
                        .collect();

                    self.processes.insert(
                        pid,
                        Process {
                            pid,
                            parent: Some(parent),
                            images,
                            threads: vec![child],
                            started: time,
                            ended: None,
                        },
                    );
                    self.threads.insert(child, pid);
                }
            }

            TraceEvent::Exec {
                process,
                time,
                ref path,
                ..
            } => {
                let entry = self.processes.entry(process).or_insert_with(|| Process {
                    pid: process,
                    started: time,
                    threads: vec![process.leader()],
                    ..Process::default()
                });
                entry.images.push((time, path.clone()));

                // Every other thread is gone after an exec.
                entry.threads.retain(|&thread| thread == process.leader());
            }
            TraceEvent::Exit { process, time, end } => {
                if let Some(entry) = self.processes.get_mut(&process) {
                    entry.ended = Some((time, end));
                }
            }

            TraceEvent::Call(_)
            | TraceEvent::Signal { .. }
            | TraceEvent::ThreadExit { .. }
            | TraceEvent::Lost { .. }
            | TraceEvent::Descriptor { .. } => {}
        }
    }

    /// The process a thread belongs to, when the trace knows.
    #[must_use]
    pub fn process_of(&self, thread: Tid) -> Option<Pid> {
        self.threads.get(&thread).copied()
    }

    #[must_use]
    pub fn get(&self, pid: Pid) -> Option<&Process> {
        self.processes.get(&pid)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Process> {
        self.processes.values()
    }

    /// Processes that have not ended.
    pub fn live(&self) -> impl Iterator<Item = Pid> + '_ {
        self.processes
            .values()
            .filter(|process| process.ended.is_none())
            .map(|process| process.pid)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.processes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forks_execs_and_exits_build_the_tree() {
        let mut tree = ProcessTree::default();
        tree.root(Pid::new(10), 0, Some("/bin/sh".into()));
        tree.apply(&TraceEvent::Spawn {
            parent_thread: Tid::new(10),
            parent: Pid::new(10),
            child: Tid::new(11),
            thread: false,
            time: 1,
        });
        tree.apply(&TraceEvent::Spawn {
            parent_thread: Tid::new(10),
            parent: Pid::new(10),
            child: Tid::new(12),
            thread: true,
            time: 2,
        });
        tree.apply(&TraceEvent::Exec {
            thread: Tid::new(11),
            process: Pid::new(11),
            time: 3,
            path: "/bin/ls".into(),
        });
        tree.apply(&TraceEvent::Exit {
            process: Pid::new(11),
            time: 4,
            end: ProcessEnd::Exited(0),
        });
        let child = tree.get(Pid::new(11)).expect("child");
        assert_eq!(child.parent, Some(Pid::new(10)));
        assert_eq!(
            child
                .images
                .iter()
                .map(|(_, path)| &**path)
                .collect::<Vec<_>>(),
            ["/bin/sh", "/bin/ls"]
        );
        assert_eq!(child.ended, Some((4, ProcessEnd::Exited(0))));
        assert_eq!(tree.process_of(Tid::new(12)), Some(Pid::new(10)));
        assert_eq!(tree.live().collect::<Vec<_>>(), [Pid::new(10)]);
    }
}
