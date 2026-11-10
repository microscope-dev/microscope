//! What each run-control request and thread event does in each phase of a
//! run: stopped, running, gathering a stop for a hit, a pause or an edit,
//! stepping a thread over a trap, launching, and attaching.
//!
//! Every phase is reached, and every event made, through the controller's
//! messages and wait statuses. A small fake kernel answers what the
//! controller asked of it, read from the native calls it made: a thread
//! asked to stop stops, a thread stepped traps, and a thread let go to its
//! exit exits. So the table holds however the controller keeps its phase.

use std::collections::BTreeSet;

use super::lifecycle::{ATTACHED, LAUNCHED, Reader, attach, idle, launch, reader, send};
use super::*;

/// The second thread, which hits and exits while the first runs.
pub(super) const OTHER: Pid = Pid::from_raw(5001);

/// Where a breakpoint is added while the program runs.
pub(super) const ADDED_SITE: u64 = 0x50;

/// What the fake kernel knows of each thread.
#[derive(Default)]
pub(super) struct Kernel {
    pub(super) running: BTreeSet<Pid>,
    pub(super) stepping: BTreeSet<Pid>,

    /// Threads sent a SIGSTOP not yet delivered.
    pub(super) stop_requested: BTreeSet<Pid>,

    /// Threads interrupted and not yet stopped.
    pub(super) interrupted: BTreeSet<Pid>,

    /// Threads at their exit event, which exit once let go.
    pub(super) exiting: BTreeSet<Pid>,
}

pub(super) struct Run {
    pub(super) harness: WatchHarness,
    pub(super) kernel: Kernel,

    /// The native calls the controller made, other than debug-register
    /// and memory traffic.
    pub(super) calls: Vec<String>,
}

impl Run {
    pub(super) fn new(harness: WatchHarness) -> Self {
        Self {
            harness,
            kernel: Kernel::default(),
            calls: Vec::new(),
        }
    }

    /// Reads the calls the controller made since last asked.
    pub(super) fn take_calls(&mut self) {
        for call in self.harness.trace().take_actions() {
            let mut words = call.split(' ');
            let verb = words.next().unwrap_or_default();
            let pid = words
                .next()
                .and_then(|pid| pid.parse().ok())
                .map(Pid::from_raw);
            match (verb, pid) {
                ("continue" | "continue_to_syscall", Some(pid)) => {
                    self.kernel.running.insert(pid);
                }
                ("step", Some(pid)) => {
                    self.kernel.running.insert(pid);
                    self.kernel.stepping.insert(pid);
                }
                ("request_stop", Some(pid)) => {
                    self.kernel.stop_requested.insert(pid);
                }
                ("interrupt", Some(pid)) => {
                    self.kernel.interrupted.insert(pid);
                }
                ("read" | "write" | "write_word", _) => continue,
                _ => {}
            }
            self.calls.push(call);
        }
    }

    pub(super) fn wait(&mut self, status: WaitEvent) {
        let pid = status.pid();
        self.kernel.running.remove(&pid);
        self.kernel.stepping.remove(&pid);
        self.kernel.interrupted.remove(&pid);
        self.harness.controller.handle_wait(status);
        self.take_calls();
    }

    /// Reports a stop of `pid` the program made itself. A SIGSTOP sent to
    /// it waits behind the stop, as Linux queues it.
    pub(super) fn program_stop(&mut self, status: WaitEvent) {
        let pid = status.pid();
        if self.kernel.stop_requested.remove(&pid) {
            self.harness.trace().queued_stops.borrow_mut().insert(pid);
        }
        self.wait(status);
    }

    /// `pid` executes the trap at [`HIT_SITE`].
    pub(super) fn hit(&mut self, pid: Pid) {
        self.harness.trace().set_pc(pid, HIT_SITE + 1);
        self.signal_info(pid, libc::SI_KERNEL);
        self.program_stop(WaitEvent::Stopped(pid, Signal::SIGTRAP));
    }

    /// `pid` reaches its exit, where a SIGSTOP sent to it dies with it.
    pub(super) fn exit(&mut self, pid: Pid) {
        self.kernel.stop_requested.remove(&pid);
        self.harness.trace().queued_stops.borrow_mut().remove(&pid);
        self.kernel.exiting.insert(pid);
        self.wait(WaitEvent::PtraceEvent(
            pid,
            Signal::SIGTRAP,
            libc::PTRACE_EVENT_EXIT,
        ));
    }

    /// `pid` stops for `signal`, which a process sent it.
    pub(super) fn signal(&mut self, pid: Pid, signal: libc::c_int) {
        self.signal_info(pid, libc::SI_USER);
        let signal = Signal::new(signal).expect("a signal");
        self.program_stop(WaitEvent::Stopped(pid, signal));
    }

    pub(super) fn is_stopped(&self) -> bool {
        self.harness
            .controller
            .session
            .inferior()
            .is_some_and(|inferior| inferior.run.stop().is_some())
    }

    pub(super) fn signal_info(&self, pid: Pid, code: i32) {
        let mut registers = self.harness.trace().registers_of(pid);
        registers[debug_registers::STATUS_REGISTER] = debug_registers::STATUS_IDLE;
        self.harness
            .trace()
            .registers
            .borrow_mut()
            .insert(pid, registers);
        let sender =
            (code == libc::SI_TKILL).then(|| i32::try_from(std::process::id()).expect("pid fits"));
        self.harness.trace().siginfo.borrow_mut().insert(
            pid,
            SignalMetadata {
                code,
                sender,
                fault_address: None,
            },
        );
    }

    /// Answers what the controller asked of the kernel until it asks for
    /// nothing more, delivering one status at a time. Steps finish only
    /// when `steps` says so.
    pub(super) fn settle(&mut self, steps: bool) {
        self.take_calls();
        for _ in 0..64 {
            let queued = self.harness.trace().unreaped.borrow_mut().pop_first();
            let running = |set: &BTreeSet<Pid>, kernel: &Kernel| {
                set.iter().copied().find(|pid| kernel.running.contains(pid))
            };

            if let Some((pid, status)) = queued {
                // Only the debugger sends a SIGSTOP here, and Linux names
                // the tracer as its sender.
                if status == WaitEvent::Stopped(pid, Signal::SIGSTOP) {
                    self.signal_info(pid, libc::SI_TKILL);
                }
                self.wait(status);
            } else if let Some(pid) = running(&self.kernel.exiting, &self.kernel) {
                self.kernel.exiting.remove(&pid);
                self.wait(WaitEvent::Exited(pid, 0));
            } else if let Some(pid) = self.kernel.stepping.first().copied().filter(|_| steps) {
                self.signal_info(pid, libc::TRAP_TRACE);
                self.program_stop(WaitEvent::Stopped(pid, Signal::SIGTRAP));
            } else if let Some(pid) = running(&self.kernel.stop_requested, &self.kernel) {
                self.kernel.stop_requested.remove(&pid);
                self.signal_info(pid, libc::SI_TKILL);
                self.wait(WaitEvent::Stopped(pid, Signal::SIGSTOP));
            } else if let Some(pid) = running(&self.kernel.interrupted, &self.kernel) {
                self.wait(WaitEvent::PtraceEvent(
                    pid,
                    Signal::SIGTRAP,
                    libc::PTRACE_EVENT_STOP,
                ));
            } else {
                return;
            }
        }
        panic!("the controller never settles: {:#?}", self.calls);
    }

    pub(super) fn snapshot(&self) -> StateSnapshot {
        self.harness.controller.snapshot()
    }

    pub(super) fn stop_id(&self) -> StopId {
        match self.snapshot().inferior {
            InferiorState::Stopped { stop_id, .. } => stop_id,
            _ => StopId::new(1),
        }
    }

    pub(super) fn process(&self) -> ProcessId {
        match self.snapshot().inferior {
            InferiorState::Stopped { process_id, .. }
            | InferiorState::Running { process_id, .. } => process_id,
            InferiorState::NotRunning => process_id(Pid::from_raw(5000)),
        }
    }

    pub(super) fn pause(&mut self) -> Reader {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        let process_id = self.process();
        send(&mut self.harness, Request::Pause { process_id, reply });
        reader(receiver)
    }

    pub(super) fn resume(&mut self) -> Reader {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        let process_id = self.process();
        let stop_id = self.stop_id();
        send(
            &mut self.harness,
            Request::Continue {
                process_id,
                stop_id,
                scope: ResumeScope::Process(process_id),
                exception: ExceptionDisposition::Pass,
                reply,
            },
        );
        reader(receiver)
    }

    pub(super) fn add(&mut self, address: u64, hit_condition: Option<&str>) -> Reader {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        send(
            &mut self.harness,
            Request::AddBreakpoint {
                spec: BreakpointSpec::Address(VirtualAddress::new(address)),
                options: Box::new(crate::BreakpointOptions {
                    hit_condition: hit_condition.map(|text| text.parse().expect("a condition")),
                    ..crate::BreakpointOptions::default()
                }),
                reply,
            },
        );
        reader(receiver)
    }

    pub(super) fn remove(&mut self) -> Reader {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        send(
            &mut self.harness,
            Request::RemoveBreakpoint {
                id: BreakpointId::new(1),
                reply,
            },
        );
        reader(receiver)
    }

    /// The run's state, and what it published, since last asked.
    pub(super) fn describe(&mut self) -> String {
        let state = match self.snapshot().inferior {
            InferiorState::NotRunning => "not running".to_owned(),
            InferiorState::Running { execution_id, .. } => {
                format!("running {}", execution(execution_id))
            }
            InferiorState::Stopped {
                thread_id, reason, ..
            } => format!("stopped {} by {thread_id}", reason_text(&reason)),
        };
        let published = self
            .harness
            .published()
            .into_iter()
            .filter_map(|event| match event {
                DebuggerEvent::InferiorStopped {
                    execution_id,
                    thread_id,
                    reason,
                    ..
                } => Some(format!(
                    "stopped {} {} by {thread_id}",
                    execution(execution_id),
                    reason_text(&reason)
                )),
                DebuggerEvent::InferiorContinued { execution_id, .. } => {
                    Some(format!("continued {}", execution(Some(execution_id))))
                }
                DebuggerEvent::InferiorLaunched { execution_id, .. } => {
                    Some(format!("launched {}", execution(Some(execution_id))))
                }
                DebuggerEvent::InferiorAttached { .. } => Some("attached".to_owned()),
                DebuggerEvent::ThreadExited { thread_id, .. } => {
                    Some(format!("{thread_id} exited"))
                }
                DebuggerEvent::BreakpointsChanged { .. } => Some("breakpoints".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let calls = std::mem::take(&mut self.calls);
        format!(
            "{state} [{}] {{{}}}",
            published.join(", "),
            calls.join(", ")
        )
    }
}

fn execution(id: Option<ExecutionId>) -> String {
    id.map_or_else(|| "-".to_owned(), |id| format!("e{id}"))
}

fn reason_text(reason: &StopReason) -> String {
    match reason {
        StopReason::Breakpoint { hits, .. } => {
            let hits = hits
                .iter()
                .map(|hit| format!("b{}#{}", hit.breakpoint, hit.hit_count))
                .collect::<Vec<_>>();
            format!("hit {}", hits.join(" "))
        }
        StopReason::ThreadExited { thread_id, .. } => format!("exit of {thread_id}"),
        StopReason::Syscall(stop) => format!(
            "call {}({}) {}",
            stop.call.number,
            stop.args[0],
            stop.returned
                .map_or_else(|| "at entry".to_owned(), |value| format!("= {value}"))
        ),
        other => format!("{other:?}"),
    }
}

/// Two threads stopped at a pause, with a breakpoint at [`HIT_SITE`] that
/// stops every hit, or none when `declining`.
pub(super) fn stopped(declining: bool) -> Run {
    let mut run = Run::new(watch_harness(2));
    let mut added = run.add(HIT_SITE, declining.then_some("==1000"));
    assert!(added().starts_with("ok"), "the setup's breakpoint");
    run.settle(true);
    run
}

pub(super) fn running(declining: bool) -> Run {
    let mut run = stopped(declining);
    let mut resumed = run.resume();
    run.settle(true);
    assert_eq!(resumed(), "ok", "the setup's continue");
    run
}

/// Builds the phase `name`, with the replies it waits to give and what
/// the kernel reports once the event is made, as a first stop.
fn phase(name: &str) -> (Run, Vec<(&'static str, Reader)>, Option<WaitEvent>) {
    let mut waiting = Vec::new();
    let mut first_stop = None;
    let run = match name {
        "stopped" => stopped(false),
        "running" => running(false),
        "gathering a hit" => {
            let mut run = running(false);
            run.hit(Pid::from_raw(5000));
            run
        }
        "gathering a pause" => {
            let mut run = running(false);
            waiting.push(("pause", run.pause()));
            run.take_calls();
            run
        }
        "gathering a pause a hit outranks" => {
            let mut run = running(false);
            waiting.push(("pause", run.pause()));
            run.take_calls();
            run.hit(OTHER);
            run
        }
        "gathering an edit" => {
            let mut run = running(false);
            waiting.push(("edit", run.add(ADDED_SITE, None)));
            run.take_calls();
            run
        }
        "stepping over a trap" => {
            // A hit the breakpoint declines is stepped over while the
            // other thread is held.
            let mut run = running(true);
            run.hit(Pid::from_raw(5000));
            run.settle(false);
            run
        }
        "launching" => {
            let mut run = Run::new(idle());
            waiting.push(("launch", launch(&mut run.harness).1));
            run.take_calls();
            first_stop = Some(WaitEvent::Stopped(LAUNCHED, Signal::SIGTRAP));
            run
        }
        "attaching" => {
            let mut run = Run::new(idle());
            waiting.push(("attach", attach(&mut run.harness).1));
            run.take_calls();
            run.kernel.interrupted.clear();
            first_stop = Some(WaitEvent::PtraceEvent(
                ATTACHED,
                Signal::SIGTRAP,
                libc::PTRACE_EVENT_STOP,
            ));
            run
        }
        _ => unreachable!("no phase {name}"),
    };

    let mut run = run;
    run.calls.clear();
    run.harness.published();
    (run, waiting, first_stop)
}

const PHASES: [&str; 9] = [
    "stopped",
    "running",
    "gathering a hit",
    "gathering a pause",
    "gathering a pause a hit outranks",
    "gathering an edit",
    "stepping over a trap",
    "launching",
    "attaching",
];

const EVENTS: [&str; 6] = [
    "pause",
    "continue",
    "add",
    "remove",
    "other hits",
    "other exits",
];

/// One event in one phase: what the request was answered, and where the
/// run settles once the kernel has answered everything asked of it.
fn outcome(name: &str, event: &str) -> Option<String> {
    let (mut run, mut waiting, first_stop) = phase(name);
    // Only a running thread hits a trap or reaches its exit.
    let other_runs = run.kernel.running.contains(&OTHER);
    let mut asked: Reader = match event {
        "pause" => run.pause(),
        "continue" => run.resume(),
        "add" => run.add(ADDED_SITE, None),
        "remove" => run.remove(),
        "other hits" if other_runs => {
            run.hit(OTHER);
            Box::new(|| "-".to_owned())
        }
        "other exits" if other_runs => {
            run.exit(OTHER);
            Box::new(|| "-".to_owned())
        }
        _ => return None,
    };

    if let Some(status) = first_stop {
        run.take_calls();
        run.wait(status);
    }
    run.settle(true);

    let replies = std::iter::once(asked())
        .chain(
            waiting
                .iter_mut()
                .map(|(name, read)| format!("{name} {}", read())),
        )
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("{name} / {event}: {replies} -> {}", run.describe()))
}

/// Each run-control request and thread event, in each phase of a run, is
/// answered and ends as it always has: which stop is published, from which
/// thread and for which execution, and which threads the controller asks
/// the kernel to run, step, or stop on the way.
#[test]
fn each_run_control_event_settles_the_same_in_each_phase() {
    let actual: Vec<String> = PHASES
        .iter()
        .flat_map(|name| EVENTS.iter().filter_map(move |event| outcome(name, event)))
        .collect();

    let expected = [
        "stopped / pause: AlreadyStopped -> stopped Pause by 5000 [] {}",
        "stopped / continue: ok -> running e2 [continued e2] {continue 5000 None, continue 5001 None}",
        "stopped / add: ok -> stopped Pause by 5000 [breakpoints] {install_site 0x50}",
        "stopped / remove: ok -> stopped Pause by 5000 [breakpoints] {remove_site 0x40}",
        "running / pause: ok -> stopped Pause by 5000 [stopped e2 Pause by 5000] {request_stop 5000, request_stop 5001}",
        "running / continue: NotStopped -> running e2 [] {}",
        "running / add: ok -> running e2 [breakpoints] {request_stop 5000, request_stop 5001, install_site 0x50, continue 5000 None, continue 5001 None}",
        "running / remove: ok -> running e2 [breakpoints] {request_stop 5000, request_stop 5001, remove_site 0x40, continue 5000 None, continue 5001 None}",
        "running / other hits: - -> stopped hit b1#1 by 5001 [stopped e2 hit b1#1 by 5001] {set_registers 5001 rip=0x40, request_stop 5000}",
        "running / other exits: - -> running e2 [5001 exited] {continue 5001 None}",
        "gathering a hit / pause: ok -> stopped hit b1#1 by 5000 [stopped e2 hit b1#1 by 5000] {}",
        "gathering a hit / continue: NotStopped -> stopped hit b1#1 by 5000 [stopped e2 hit b1#1 by 5000] {}",
        "gathering a hit / add: ok -> stopped hit b1#1 by 5000 [breakpoints, stopped e2 hit b1#1 by 5000] {install_site 0x50}",
        "gathering a hit / remove: ok -> running e2 [breakpoints] {remove_site 0x40, continue 5000 None, continue 5001 None}",
        "gathering a hit / other hits: - -> stopped hit b1#1 by 5000 [stopped e2 hit b1#1 by 5000] {set_registers 5001 rip=0x40}",
        "gathering a hit / other exits: - -> stopped hit b1#1 by 5000 [5001 exited, stopped e2 hit b1#1 by 5000] {continue 5001 None}",
        "gathering a pause / pause: ok, pause ok -> stopped Pause by 5000 [stopped e2 Pause by 5000] {}",
        "gathering a pause / continue: NotStopped, pause ok -> stopped Pause by 5000 [stopped e2 Pause by 5000] {}",
        "gathering a pause / add: ok, pause ok -> stopped Pause by 5000 [breakpoints, stopped e2 Pause by 5000] {install_site 0x50}",
        "gathering a pause / remove: ok, pause ok -> stopped Pause by 5000 [breakpoints, stopped e2 Pause by 5000] {remove_site 0x40}",
        "gathering a pause / other hits: -, pause ok -> stopped hit b1#1 by 5001 [stopped e2 hit b1#1 by 5001] {set_registers 5001 rip=0x40}",
        "gathering a pause / other exits: -, pause ok -> stopped Pause by 5000 [5001 exited, stopped e2 Pause by 5000] {continue 5001 None}",
        "gathering a pause a hit outranks / pause: ok, pause ok -> stopped hit b1#1 by 5001 [stopped e2 hit b1#1 by 5001] {}",
        "gathering a pause a hit outranks / continue: NotStopped, pause ok -> stopped hit b1#1 by 5001 [stopped e2 hit b1#1 by 5001] {}",
        "gathering a pause a hit outranks / add: ok, pause ok -> stopped hit b1#1 by 5001 [breakpoints, stopped e2 hit b1#1 by 5001] {install_site 0x50}",
        "gathering a pause a hit outranks / remove: ok, pause ok -> stopped Pause by 5001 [breakpoints, stopped e2 Pause by 5001] {remove_site 0x40}",
        "gathering an edit / pause: ok, edit ok -> stopped Pause by 5000 [breakpoints, stopped e2 Pause by 5000] {install_site 0x50}",
        "gathering an edit / continue: NotStopped, edit ok -> running e2 [breakpoints] {install_site 0x50, continue 5000 None, continue 5001 None}",
        "gathering an edit / add: ok, edit ok -> running e2 [breakpoints] {install_site 0x50, continue 5000 None, continue 5001 None}",
        "gathering an edit / remove: ok, edit ok -> running e2 [breakpoints, breakpoints] {install_site 0x50, remove_site 0x40, continue 5000 None, continue 5001 None}",
        "gathering an edit / other hits: -, edit ok -> stopped hit b1#1 by 5001 [breakpoints, stopped e2 hit b1#1 by 5001] {set_registers 5001 rip=0x40, install_site 0x50}",
        "gathering an edit / other exits: -, edit ok -> running e2 [5001 exited, breakpoints] {continue 5001 None, install_site 0x50, continue 5000 None}",
        "stepping over a trap / pause: ok -> stopped Pause by 5000 [stopped e2 Pause by 5000] {request_stop 5000, reinstall_site 0x40}",
        "stepping over a trap / continue: NotStopped -> running e2 [] {reinstall_site 0x40, continue 5000 None, continue 5001 None}",
        "stepping over a trap / add: ok -> running e2 [breakpoints] {request_stop 5000, reinstall_site 0x40, install_site 0x50, continue 5000 None, continue 5001 None, continue 5000 None}",
        "stepping over a trap / remove: ok -> running e2 [breakpoints] {request_stop 5000, reinstall_site 0x40, remove_site 0x40, continue 5000 None, continue 5001 None, continue 5000 None}",
        "launching / pause: ok, launch ok -> stopped Pause by 7000 [stopped e1 Pause by 7000] {set_options 7000}",
        "launching / continue: NotStopped, launch ok -> running e1 [continued e1] {set_options 7000, continue 7000 None}",
        "launching / add: ok, launch ok -> running e1 [breakpoints, continued e1] {set_options 7000, install_site 0x50, continue 7000 None}",
        "launching / remove: BreakpointNotFound(1), launch ok -> running e1 [continued e1] {set_options 7000, continue 7000 None}",
        "attaching / pause: AlreadyStopped, attach ok -> stopped Attach by 4000 [attached, stopped - Attach by 4000] {}",
        "attaching / continue: NotStopped, attach ok -> stopped Attach by 4000 [attached, stopped - Attach by 4000] {}",
        "attaching / add: ok, attach ok -> stopped Attach by 4000 [breakpoints, attached, stopped - Attach by 4000] {install_site 0x50}",
        "attaching / remove: BreakpointNotFound(1), attach ok -> stopped Attach by 4000 [attached, stopped - Attach by 4000] {}",
    ];
    assert_eq!(actual, expected, "\n{}", actual.join("\n"));
}
