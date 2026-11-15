use tokio::sync::mpsc;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use super::hits::Published;
use super::kernel::{ExitStatus, Kernel, Reaped, Tgid, Tid, WaitStatus};
use super::marks::{Mark, Marks};
use super::report::Failure;
use crate::{ConditionOwner, DebuggerEvent};

pub struct Auditor {
    events: mpsc::UnboundedReceiver<DebuggerEvent>,

    published: Rc<RefCell<Published>>,

    revision: u64,

    change: u64,

    stop: u64,

    executions: BTreeMap<Tgid, u64>,

    launching: Option<Tgid>,

    debugged: BTreeSet<Tgid>,
    detached: BTreeSet<Tgid>,

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
            executions: BTreeMap::new(),
            launching: None,
            debugged: BTreeSet::new(),
            detached: BTreeSet::new(),
            exited_threads: BTreeSet::new(),
            exited_processes: BTreeSet::new(),
        }
    }

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
            self.check_execution(&event)?;

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

    fn check_execution(&mut self, event: &DebuggerEvent) -> Result<(), Failure> {
        match *event {
            DebuggerEvent::InferiorLaunched {
                process_id,
                execution_id,
                ..
            } => {
                let tgid = tgid_of(process_id.get());
                self.executions.insert(tgid, execution_id.get());
                self.launching = Some(tgid);
            }
            DebuggerEvent::InferiorAttached { process_id, .. } => {
                self.executions.remove(&tgid_of(process_id.get()));
            }
            DebuggerEvent::InferiorStopped { .. } => self.launching = None,
            DebuggerEvent::InferiorContinued {
                process_id,
                execution_id,
                ..
            } => {
                let tgid = tgid_of(process_id.get());
                let resumed_launch = self.launching.take() == Some(tgid);
                let last = self.executions.insert(tgid, execution_id.get());
                if let Some(last) = last
                    && (execution_id.get() < last
                        || (execution_id.get() == last && !resumed_launch))
                {
                    return Err(failure(format!(
                        "{tgid} began execution {execution_id} after execution {last}"
                    )));
                }
            }
            _ => {}
        }

        Ok(())
    }

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
    use crate::{ExecutionId, ProcessId, StopId, StopReason, ThreadId};

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

    fn continued(revision: u64, execution: u64) -> DebuggerEvent {
        DebuggerEvent::InferiorContinued {
            revision,
            process_id: PROCESS,
            execution_id: ExecutionId::new(execution),
            resumed: crate::ResumeScope::Process(PROCESS),
        }
    }

    fn stopped(revision: u64, execution: u64) -> DebuggerEvent {
        DebuggerEvent::InferiorStopped {
            revision,
            process_id: PROCESS,
            execution_id: Some(ExecutionId::new(execution)),
            stop_id: StopId::new(revision),
            thread_id: ThreadId::new(1000),
            reason: StopReason::Entry,
        }
    }

    fn audit(events: Vec<DebuggerEvent>) -> Result<(), Failure> {
        audit_exits(false, events)
    }

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

        audit_exits(true, vec![inferior_exited(3)]).expect("a group exit");
        let failure = audit_exits(true, vec![thread_exited(3)])
            .expect_err("a process's exit unpublished after its group exit");
        assert!(failure.message.contains("process 1000"), "{failure}");

        audit(vec![DebuggerEvent::InferiorDetached {
            revision: 3,
            process_id: PROCESS,
        }])
        .expect("detached");
    }

    #[test]
    fn each_execution_is_new() {
        let exits = || [thread_exited(10), inferior_exited(11)];
        let audit_executions =
            |events: Vec<DebuggerEvent>| audit(events.into_iter().chain(exits()).collect());

        audit_executions(vec![continued(3, 1), stopped(4, 1), continued(5, 2)])
            .expect("the launch went on past its exec stop");

        let failure = audit_executions(vec![continued(3, 1), continued(4, 1)])
            .expect_err("the launch's execution began again");
        assert!(
            failure
                .message
                .contains("began execution 1 after execution 1"),
            "{failure}"
        );

        audit_executions(vec![stopped(3, 1), continued(4, 1)])
            .expect_err("a continue from the entry stop reused the launch's execution");

        audit_executions(vec![
            continued(3, 1),
            stopped(4, 1),
            continued(5, 3),
            stopped(6, 3),
            continued(7, 2),
        ])
        .expect_err("an execution older than the last");
    }
}
