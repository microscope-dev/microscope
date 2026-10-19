//! The event auditor: reads every event the debugger publishes, from a copy
//! of the stream that never falls behind, and checks the stream's promises
//! against the kernel.

use tokio::sync::mpsc;

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;

use super::hits::Published;
use super::kernel::{ExitStatus, Kernel, Reaped, Tgid, Tid, WaitStatus};
use super::marks::{Mark, Marks};
use super::report::Failure;
use crate::{ConditionOwner, DebuggerEvent};

pub struct Auditor {
    events: mpsc::UnboundedReceiver<DebuggerEvent>,

    /// The hit events counted for the client's checks.
    published: Rc<RefCell<Published>>,

    /// The newest revision of any event.
    revision: u64,

    /// The newest revision a `StateChanged` announced.
    change: u64,

    /// The newest stop published.
    stop: u64,

    /// The processes the session launched or attached to, and those it
    /// said it detached from.
    debugged: BTreeSet<Tgid>,
    detached: BTreeSet<Tgid>,

    /// The threads and processes whose exits were published.
    exited_threads: BTreeSet<Tid>,
    exited_processes: BTreeSet<Tgid>,
}

fn failure(message: String) -> Failure {
    Failure::debugger("events", message)
}

impl Auditor {
    pub const fn new(
        events: mpsc::UnboundedReceiver<DebuggerEvent>,
        published: Rc<RefCell<Published>>,
    ) -> Self {
        Self {
            events,
            published,
            revision: 0,
            change: 0,
            stop: 0,
            debugged: BTreeSet::new(),
            detached: BTreeSet::new(),
            exited_threads: BTreeSet::new(),
            exited_processes: BTreeSet::new(),
        }
    }

    /// Reads the events published since the last action: each state change
    /// announces a new revision, which the events it produced share, so
    /// revisions never decrease; stop identifiers only increase; and every
    /// exit the client hears of, of a thread or of the process, is the one
    /// the kernel reported, or, for a leader, the code it passed to `exit`,
    /// and is heard once. It counts the messages hits logged and the
    /// conditions that failed to evaluate, for the client's checks.
    pub fn check(&mut self, kernel: &Kernel, marks: &mut Marks) -> Result<(), Failure> {
        while let Ok(event) = self.events.try_recv() {
            let revision = event.revision();
            if revision < self.revision {
                return Err(failure(format!(
                    "revision {revision} follows {} in {event:?}",
                    self.revision
                )));
            }

            self.revision = revision;

            match &event {
                DebuggerEvent::StateChanged { revision } => {
                    if *revision <= self.change {
                        return Err(failure(format!(
                            "state change {revision} follows state change {}",
                            self.change
                        )));
                    }

                    self.change = *revision;
                }
                DebuggerEvent::InferiorStopped { stop_id, .. } => {
                    if stop_id.get() <= self.stop {
                        return Err(failure(format!(
                            "stop {stop_id} follows stop {}",
                            self.stop
                        )));
                    }

                    self.stop = stop_id.get();
                    self.published.borrow_mut().stops.insert(stop_id.get());
                }
                DebuggerEvent::InferiorLaunched { process_id, .. }
                | DebuggerEvent::InferiorAttached { process_id, .. } => {
                    self.debugged.insert(tgid_of(process_id.get()));
                }
                DebuggerEvent::InferiorDetached { process_id, .. } => {
                    self.detached.insert(tgid_of(process_id.get()));
                }

                DebuggerEvent::ThreadExited {
                    thread_id, status, ..
                } => {
                    marks.hit(Mark::ThreadExited);
                    let tid = Tid::try_from(thread_id.get()).expect("a simulated tid fits");
                    if !self.exited_threads.insert(tid) {
                        return Err(failure(format!(
                            "thread {tid}'s exit was published again, {status:?}"
                        )));
                    }

                    // A leader that exits while the debugger knows other
                    // threads live is reported at its exit event, with the
                    // code it passed to `exit`: Linux reports its exit only
                    // once every other thread has exited, with the process's
                    // status. Those threads may have begun to exit already.
                    if let Some(&code) = kernel.leader_exits.get(&tid) {
                        if !same_exit(ExitStatus::Code(code), status) {
                            return Err(failure(format!(
                                "leader {tid} passed {code} to exit, but the client heard \
                                 {status:?}"
                            )));
                        }

                        marks.hit(Mark::LeaderExitReported);
                        continue;
                    }

                    let Some(&Reaped { status: reaped, .. }) = kernel.reaped.get(&tid) else {
                        return Err(failure(format!(
                            "thread {tid} reported {status:?} before it was reaped"
                        )));
                    };

                    if !same_status(reaped, status) {
                        return Err(failure(format!(
                            "thread {tid} reported {reaped}, but the client heard {status:?}"
                        )));
                    }
                }

                DebuggerEvent::InferiorExited {
                    process_id, status, ..
                } => {
                    let tgid = tgid_of(process_id.get());
                    if !self.exited_processes.insert(tgid) {
                        return Err(failure(format!(
                            "process {tgid}'s exit was published again, {status:?}"
                        )));
                    }

                    let Some(truth) = kernel.ended.get(&tgid).map(|ended| &ended.status) else {
                        return Err(failure(format!(
                            "process {tgid} reported {status:?} before it ended"
                        )));
                    };

                    if matches!(truth, ExitStatus::Code(_)) {
                        marks.hit(Mark::ProgramExited);
                    }

                    if !same_exit(*truth, status) {
                        return Err(failure(format!(
                            "process {tgid} ended {truth:?}, but the client heard {status:?}"
                        )));
                    }
                }
                DebuggerEvent::LogMessage { .. } | DebuggerEvent::ConditionFailed { .. } => {
                    self.count_hit_event(&event)?;
                }
                _ => {}
            }
        }

        Ok(())
    }

    /// Once the session ended, after the last [`Self::check`]: a process
    /// the session launched or attached to whose leader its tracer reaped
    /// was published as exited, and so was each other thread of it the
    /// tracer reaped that exited by itself. `ThreadExited` promises to tell
    /// of a thread that exits while its process runs on; one that its
    /// process's `exit_group` or fatal signal took may go unpublished, as
    /// one that began as its process was killed does from start to end.
    ///
    /// A process the session detached from is not judged: one killed from
    /// outside as an attached session shuts down can die as it detaches,
    /// and the debugger then publishes the detach, though it reaped the
    /// exit.
    pub fn finish(&self, kernel: &Kernel) -> Result<(), Failure> {
        for (&tid, reaped) in &kernel.reaped {
            if self.detached.contains(&reaped.tgid) {
                continue;
            }

            if tid == reaped.tgid.leader() {
                if self.debugged.contains(&reaped.tgid)
                    && !self.exited_processes.contains(&reaped.tgid)
                {
                    return Err(failure(format!(
                        "process {} ended, reaped {}, but its exit was never published",
                        reaped.tgid, reaped.status
                    )));
                }
            } else if self.debugged.contains(&reaped.tgid)
                && !reaped.group_exit
                && !self.exited_threads.contains(&tid)
            {
                return Err(failure(format!(
                    "thread {tid} of process {} exited, reaped {}, but its exit was never \
                     published",
                    reaped.tgid, reaped.status
                )));
            }
        }

        Ok(())
    }

    /// Counts a message a hit logged or a condition that failed to evaluate.
    fn count_hit_event(&self, event: &DebuggerEvent) -> Result<(), Failure> {
        let mut published = self.published.borrow_mut();

        match event {
            DebuggerEvent::LogMessage { breakpoint, .. } => {
                *published.logged.entry(breakpoint.get()).or_default() += 1;
            }

            DebuggerEvent::ConditionFailed {
                owner: ConditionOwner::Breakpoint(breakpoint),
                ..
            } => {
                *published
                    .condition_failures
                    .entry(breakpoint.get())
                    .or_default() += 1;
            }

            // The client's watch conditions are constants.
            DebuggerEvent::ConditionFailed {
                owner: ConditionOwner::Watchpoint(watchpoint),
                error,
                ..
            } => {
                return Err(failure(format!(
                    "watchpoint {watchpoint}'s constant condition failed to evaluate: {error}"
                )));
            }
            _ => {}
        }

        Ok(())
    }
}

/// The simulated process a process identifier names.
fn tgid_of(process: u64) -> Tgid {
    Tgid::new(Tid::try_from(process).expect("a simulated pid fits"))
}

fn same_exit(kernel: ExitStatus, reported: &crate::ExitStatus) -> bool {
    match (kernel, reported) {
        (ExitStatus::Code(code), crate::ExitStatus::Code(reported)) => {
            i64::from(code & 0xff) == *reported
        }
        (ExitStatus::Signal(signal, _), crate::ExitStatus::Terminated(info)) => {
            u64::try_from(signal).is_ok_and(|signal| signal == info.code)
        }
        _ => false,
    }
}

fn same_status(kernel: WaitStatus, reported: &crate::ExitStatus) -> bool {
    match kernel {
        WaitStatus::Exited(_, code) => same_exit(ExitStatus::Code(code), reported),
        WaitStatus::Signaled(_, signal, core) => {
            same_exit(ExitStatus::Signal(signal, core), reported)
        }

        WaitStatus::Stopped(..)
        | WaitStatus::Event(..)
        | WaitStatus::GroupStop(..)
        | WaitStatus::Syscall(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::kernel::{Ended, Parent};
    use crate::{ExecutionId, ProcessId, ThreadId};

    const LEADER: Tid = 1000;
    const WORKER: Tid = 1001;
    const PROCESS: ProcessId = ProcessId::new(1000);

    fn thread_exited(revision: u64) -> DebuggerEvent {
        DebuggerEvent::ThreadExited {
            revision,
            process_id: PROCESS,
            thread_id: ThreadId::new(1001),
            status: crate::ExitStatus::Code(0),
        }
    }

    fn inferior_exited(revision: u64) -> DebuggerEvent {
        DebuggerEvent::InferiorExited {
            revision,
            process_id: PROCESS,
            execution_id: None,
            status: crate::ExitStatus::Code(0),
        }
    }

    /// Audits a session that launched process 1000, whose thread 1001
    /// started, and both threads exited 0 and were reaped, publishing
    /// `events` after the start.
    fn audit(events: Vec<DebuggerEvent>) -> Result<(), Failure> {
        audit_exits(false, events)
    }

    /// Like [`audit`], with the threads' exits taken by `exit_group` when
    /// `group_exit` says so.
    fn audit_exits(group_exit: bool, events: Vec<DebuggerEvent>) -> Result<(), Failure> {
        let mut kernel = Kernel::new(1);
        let tgid = Tgid::led_by(LEADER);

        for tid in [LEADER, WORKER] {
            kernel.reaped.insert(
                tid,
                Reaped {
                    tgid,
                    status: WaitStatus::Exited(tid, 0),
                    group_exit,
                },
            );
        }

        kernel.ended.insert(
            tgid,
            Ended {
                status: ExitStatus::Code(0),
                root: LEADER,
                reaper: Parent::Tracer,
                killed_externally: false,
            },
        );

        let (sender, receiver) = mpsc::unbounded_channel();
        let start = [
            DebuggerEvent::InferiorLaunched {
                revision: 1,
                process_id: PROCESS,
                execution_id: ExecutionId::new(1),
            },
            DebuggerEvent::ThreadStarted {
                revision: 2,
                process_id: PROCESS,
                thread_id: ThreadId::new(1001),
            },
        ];

        for event in start.into_iter().chain(events) {
            sender.send(event).expect("the auditor reads");
        }

        let mut auditor = Auditor::new(receiver, Rc::default());
        auditor.check(&kernel, &mut Marks::default())?;
        auditor.finish(&kernel)
    }

    /// Each reaped thread that exited by itself, and its process, is heard
    /// to exit exactly once.
    #[test]
    fn every_exit_is_published_once() {
        audit(vec![thread_exited(3), inferior_exited(4)]).expect("each exit once");

        let failure = audit(vec![inferior_exited(4)]).expect_err("a thread's exit unpublished");
        assert!(
            failure.message.contains("thread 1001") && failure.message.contains("never published"),
            "{failure}"
        );

        let failure = audit(vec![thread_exited(3)]).expect_err("a process's exit unpublished");
        assert!(
            failure.message.contains("process 1000") && failure.message.contains("never published"),
            "{failure}"
        );

        let failure = audit(vec![thread_exited(3), thread_exited(4), inferior_exited(5)])
            .expect_err("a thread's exit published twice");
        assert!(failure.message.contains("published again"), "{failure}");

        let failure = audit(vec![
            thread_exited(3),
            inferior_exited(4),
            inferior_exited(5),
        ])
        .expect_err("a process's exit published twice");
        assert!(failure.message.contains("published again"), "{failure}");

        // A thread its process's exit took may go unpublished.
        audit_exits(true, vec![inferior_exited(3)]).expect("a group exit");
        let failure = audit_exits(true, vec![thread_exited(3)])
            .expect_err("a process's exit unpublished after its group exit");
        assert!(failure.message.contains("process 1000"), "{failure}");

        // A process detached from, as it died, is not judged.
        audit(vec![DebuggerEvent::InferiorDetached {
            revision: 3,
            process_id: PROCESS,
        }])
        .expect("detached");
    }
}
