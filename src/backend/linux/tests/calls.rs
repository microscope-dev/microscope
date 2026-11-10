//! Where a thread stands with respect to a system call, through every event
//! that changes it: running while catchpoints watch its calls, inside a
//! call no catchpoint caught, at a caught call's entry or exit, running in
//! a call past its entry, about to step into a call, and in a vfork whose
//! entry was caught.
//!
//! Every phase is reached through the controller's messages and wait
//! statuses, as in the run-phase table, so the table holds however the
//! controller keeps a thread's call. An exit stop's arguments show whether
//! the thread kept what its entry said: the fake kernel's registers name
//! call 0 with no arguments, which is what an exit with its entry unseen
//! reports.

use super::lifecycle::{Reader, reader, send};
use super::run_phase::{OTHER, Run, stopped};
use super::syscalls::info::{AUDIT_ARCH_X86_64, OP_ENTRY, OP_EXIT, SyscallInfo};
use super::*;
use crate::syscalls::{Abi, SyscallId};

/// The thread each phase is about. It is not the leader, so its exit ends
/// only itself.
const THREAD: Pid = OTHER;

/// The leader, the thread's one sibling.
const SIBLING: Pid = Pid::from_raw(5000);

const CLOSE: u32 = 3;
const GETPID: u32 = 39;
const FORK: u32 = 57;
const VFORK: u32 = 58;

/// What `close` is passed, which no register of the fake kernel holds.
const FD: u64 = 7;

/// Where the thread stands in the phases that step into a call, over a
/// `syscall` instruction.
const AT_SYSCALL: u64 = 0x5000;

/// The vfork child, which the controller never traces as a thread.
const VFORK_CHILD: Pid = Pid::from_raw(6000);

impl Run {
    fn catch(&mut self, call: u32, phase: crate::CatchPhase) -> Reader {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        send(
            &mut self.harness,
            Request::AddCatchpoint {
                calls: Arc::from([SyscallId::new(Abi::X86_64, call)]),
                phase,
                condition: None,
                reply,
            },
        );
        reader(receiver)
    }

    fn uncatch(&mut self) -> Reader {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        send(
            &mut self.harness,
            Request::RemoveCatchpoint {
                id: crate::CatchpointId::new(1),
                reply,
            },
        );
        reader(receiver)
    }

    fn step(&mut self, pid: Pid) -> Reader {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        let process_id = self.process();
        let stop_id = self.stop_id();
        send(
            &mut self.harness,
            Request::Step {
                process_id,
                stop_id,
                context: crate::ExecutionContext::Thread(debug_thread_id(pid)),
                frame: StackFrameId::INNERMOST,
                kind: StepKind::Instruction,
                call: None,
                scope: ResumeScope::Process(process_id),
                exception: ExceptionDisposition::Pass,
                reply,
            },
        );
        reader(receiver)
    }

    /// `pid` stops at the entry of `call`, passed `first` first.
    fn entry(&mut self, pid: Pid, call: u32, first: u64) {
        self.syscall_stop(
            pid,
            SyscallInfo {
                op: OP_ENTRY,
                arch: AUDIT_ARCH_X86_64,
                number: u64::from(call),
                args: [first, 0, 0, 0, 0, 0],
                value: 0,
            },
        );
    }

    /// `pid` stops at the exit of the call it is in, which returns
    /// `value`.
    fn exit_call(&mut self, pid: Pid, value: i64) {
        self.syscall_stop(
            pid,
            SyscallInfo {
                op: OP_EXIT,
                arch: AUDIT_ARCH_X86_64,
                number: 0,
                args: [0; 6],
                value,
            },
        );
    }

    fn syscall_stop(&mut self, pid: Pid, info: SyscallInfo) {
        self.harness
            .trace()
            .syscall_stops
            .borrow_mut()
            .insert(pid, info);
        self.program_stop(WaitEvent::PtraceSyscall(pid));
        self.harness.trace().syscall_stops.borrow_mut().remove(&pid);
    }

    /// `pid` stops at the ptrace event `event`, whose message names
    /// [`VFORK_CHILD`].
    fn call_event(&mut self, pid: Pid, event: libc::c_int) {
        self.harness
            .trace()
            .clone
            .replace(Some((VFORK_CHILD, VFORK_CHILD)));
        self.program_stop(WaitEvent::PtraceEvent(pid, Signal::SIGTRAP, event));
        self.harness.trace().clone.replace(None);
    }

    /// Every catchpoint's hits so far.
    fn catches(&self) -> String {
        self.harness
            .controller
            .catchpoints
            .values()
            .map(|catchpoint| format!("c{}:{}", catchpoint.id, catchpoint.hit_count))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Two threads stopped at a pause, catching `call` at `phase`.
fn stopped_catching(call: u32, phase: crate::CatchPhase) -> Run {
    let mut run = stopped(false);
    let mut caught = run.catch(call, phase);
    run.settle(true);
    assert!(caught().starts_with("ok"), "the setup's catchpoint");
    run
}

fn running_catching(call: u32, phase: crate::CatchPhase) -> Run {
    let mut run = stopped_catching(call, phase);
    let mut resumed = run.resume();
    run.settle(true);
    assert_eq!(resumed(), "ok", "the setup's continue");
    run
}

fn continued(mut run: Run) -> Run {
    let mut resumed = run.resume();
    run.settle(true);
    assert_eq!(resumed(), "ok", "the setup's continue");
    run
}

/// Builds the call phase `name`. Each catches `close`, at its entry and
/// exit unless the phase says otherwise.
fn phase(name: &str) -> Run {
    use crate::CatchPhase::{Both, Exit};

    let mut run = match name {
        "running" => running_catching(CLOSE, Both),
        "inside an uncaught call" => {
            let mut run = running_catching(CLOSE, Both);
            run.entry(THREAD, GETPID, FD);
            run.settle(true);
            run
        }
        "at a caught entry" => {
            let mut run = running_catching(CLOSE, Both);
            run.entry(THREAD, CLOSE, FD);
            run.settle(true);
            run
        }
        "inside a call past its caught entry" => continued(phase("at a caught entry")),
        "at a caught exit" => {
            let mut run = phase("inside a call past its caught entry");
            run.exit_call(THREAD, 0);
            run.settle(true);
            run
        }
        "inside a call past its uncaught entry" => {
            let mut run = running_catching(CLOSE, Exit);
            run.entry(THREAD, CLOSE, FD);
            run.settle(true);
            run
        }
        "before a syscall instruction" => {
            let run = stopped_catching(CLOSE, Both);
            run.harness.trace().set_pc(THREAD, AT_SYSCALL);
            run.harness
                .trace()
                .memory
                .borrow_mut()
                .insert(AT_SYSCALL, 0x050f);
            run
        }
        "stepping into a call" => {
            let mut run = phase("before a syscall instruction");
            let mut stepped = run.step(THREAD);
            run.settle(false);
            assert_eq!(stepped(), "ok", "the setup's step");
            run
        }
        "stepping inside an uncaught call" => {
            let mut run = phase("stepping into a call");
            run.entry(THREAD, GETPID, FD);
            run.settle(false);
            run
        }
        "in a vfork past its caught entry" => {
            let mut run = running_catching(VFORK, Both);
            run.entry(THREAD, VFORK, FD);
            run.settle(true);
            let mut run = continued(run);
            run.call_event(THREAD, libc::PTRACE_EVENT_VFORK);
            run.settle(false);
            run
        }
        "at a fork inside an uncaught call" => {
            let mut run = running_catching(CLOSE, Both);
            run.hit(SIBLING);
            run.call_event(THREAD, libc::PTRACE_EVENT_FORK);
            run.settle(true);
            run
        }
        "at a fork inside a call past its caught entry" => {
            let mut run = running_catching(FORK, Both);
            run.entry(THREAD, FORK, FD);
            run.settle(true);
            let mut run = continued(run);
            run.hit(SIBLING);
            run.call_event(THREAD, libc::PTRACE_EVENT_FORK);
            run.settle(true);
            run
        }
        _ => unreachable!("no phase {name}"),
    };

    run.calls.clear();
    run.harness.published();
    run
}

const PHASES: [&str; 12] = [
    "running",
    "inside an uncaught call",
    "at a caught entry",
    "inside a call past its caught entry",
    "at a caught exit",
    "inside a call past its uncaught entry",
    "before a syscall instruction",
    "stepping into a call",
    "stepping inside an uncaught call",
    "in a vfork past its caught entry",
    "at a fork inside an uncaught call",
    "at a fork inside a call past its caught entry",
];

const EVENTS: [&str; 13] = [
    "continue",
    "continue, then exit",
    "step",
    "uncatch",
    "catch getpid",
    "entry of close",
    "entry of getpid",
    "exit",
    "signal",
    "signal, then exit",
    "sibling hits, then continue, then exit",
    "vfork done, then exit",
    "thread exits",
];

/// One event in one phase: what each request was answered, where the run
/// settles once the kernel has answered everything asked of it, and how
/// many calls each catchpoint caught. An edit at a stop is followed by a
/// continue, to show how each thread runs on.
fn outcome(name: &str, event: &str) -> Option<String> {
    let mut run = phase(name);
    let stopped = run.is_stopped();
    let thread_runs = run.kernel.running.contains(&THREAD);
    let vforked = name.starts_with("in a vfork");
    let none = || -> Reader { Box::new(|| "-".to_owned()) };
    let mut asked: Reader = match event {
        "continue" => run.resume(),
        "continue, then exit" if stopped => {
            let mut resumed = run.resume();
            run.settle(true);
            let resumed = resumed();
            if run.kernel.running.contains(&THREAD) {
                run.exit_call(THREAD, 5);
            }
            Box::new(move || resumed.clone())
        }
        "step" if stopped => run.step(THREAD),
        "uncatch" => run.uncatch(),
        "catch getpid" => run.catch(GETPID, crate::CatchPhase::Both),
        "entry of close" if thread_runs && !vforked => {
            run.entry(THREAD, CLOSE, FD);
            none()
        }
        "entry of getpid" if thread_runs && !vforked => {
            run.entry(THREAD, GETPID, FD);
            none()
        }
        "exit" if thread_runs && !vforked => {
            run.exit_call(THREAD, 5);
            none()
        }
        // A thread waiting for its vfork child takes no signal until the
        // child execs or exits.
        "signal" if thread_runs && !vforked => {
            run.signal(THREAD, libc::SIGALRM);
            none()
        }
        "signal, then exit" if thread_runs && !vforked => {
            run.signal(THREAD, libc::SIGALRM);
            run.settle(true);
            run.exit_call(THREAD, 5);
            none()
        }
        "sibling hits, then continue, then exit"
            if thread_runs && !vforked && run.kernel.running.contains(&SIBLING) =>
        {
            run.hit(SIBLING);
            run.settle(true);
            let mut resumed = run.resume();
            run.settle(true);
            let resumed = resumed();
            if run.kernel.running.contains(&THREAD) {
                run.exit_call(THREAD, 5);
            }
            Box::new(move || format!("continue {resumed}"))
        }
        "vfork done, then exit" if vforked && thread_runs => {
            run.call_event(THREAD, libc::PTRACE_EVENT_VFORK_DONE);
            run.settle(false);
            run.exit_call(THREAD, 5);
            none()
        }
        "thread exits" if thread_runs => {
            run.exit(THREAD);
            none()
        }
        _ => return None,
    };
    run.settle(true);
    let mut replies = vec![asked()];

    let edited = matches!(event, "uncatch" | "catch getpid");
    if edited && stopped && run.is_stopped() {
        let mut resumed = run.resume();
        run.settle(true);
        replies.push(format!("continue {}", resumed()));
    }
    if vforked
        && run.kernel.running.contains(&THREAD)
        && !matches!(event, "vfork done, then exit" | "thread exits")
    {
        run.call_event(THREAD, libc::PTRACE_EVENT_VFORK_DONE);
        run.settle(true);
        replies.push(format!("vfork done {}", asked()));
    }

    Some(format!(
        "{name} / {event}: {} -> {} ({})",
        replies.join(", "),
        run.describe(),
        run.catches()
    ))
}

/// The table's rows, as [`outcome`] writes them.
const EXPECTED: &[&str] = &[
    "running / continue: NotStopped -> running e2 [] {} (c1:0)",
    "running / uncatch: ok -> running e2 [] {request_stop 5000, request_stop 5001, continue 5000 None, continue 5001 None} ()",
    "running / catch getpid: ok -> running e2 [] {request_stop 5000, request_stop 5001, continue_to_syscall 5000, continue_to_syscall 5001} (c1:0 c2:0)",
    "running / entry of close: - -> stopped call 3(7) at entry by 5001 [stopped e2 call 3(7) at entry by 5001] {request_stop 5000} (c1:1)",
    "running / entry of getpid: - -> running e2 [] {continue_to_syscall 5001} (c1:0)",
    "running / exit: - -> running e2 [] {continue_to_syscall 5001} (c1:0)",
    "running / signal: - -> running e2 [] {continue_to_syscall 5001} (c1:0)",
    "running / signal, then exit: - -> running e2 [] {continue_to_syscall 5001, continue_to_syscall 5001} (c1:0)",
    "running / sibling hits, then continue, then exit: continue ok -> running e3 [stopped e2 hit b1#1 by 5000, continued e3] {set_registers 5000 rip=0x40, request_stop 5001, remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001, continue_to_syscall 5001} (c1:0)",
    "running / thread exits: - -> running e2 [5001 exited] {continue 5001 None} (c1:0)",
    "inside an uncaught call / continue: NotStopped -> running e2 [] {} (c1:0)",
    "inside an uncaught call / uncatch: ok -> running e2 [] {request_stop 5000, request_stop 5001, continue 5000 None, continue 5001 None} ()",
    "inside an uncaught call / catch getpid: ok -> running e2 [] {request_stop 5000, request_stop 5001, continue_to_syscall 5000, continue_to_syscall 5001} (c1:0 c2:0)",
    "inside an uncaught call / entry of close: - -> stopped call 3(7) at entry by 5001 [stopped e2 call 3(7) at entry by 5001] {request_stop 5000} (c1:1)",
    "inside an uncaught call / entry of getpid: - -> running e2 [] {continue_to_syscall 5001} (c1:0)",
    "inside an uncaught call / exit: - -> running e2 [] {continue_to_syscall 5001} (c1:0)",
    "inside an uncaught call / signal: - -> running e2 [] {continue_to_syscall 5001} (c1:0)",
    "inside an uncaught call / signal, then exit: - -> running e2 [] {continue_to_syscall 5001, continue_to_syscall 5001} (c1:0)",
    "inside an uncaught call / sibling hits, then continue, then exit: continue ok -> running e3 [stopped e2 hit b1#1 by 5000, continued e3] {set_registers 5000 rip=0x40, request_stop 5001, remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001, continue_to_syscall 5001} (c1:0)",
    "inside an uncaught call / thread exits: - -> running e2 [5001 exited] {continue 5001 None} (c1:0)",
    "at a caught entry / continue: ok -> running e3 [continued e3] {continue_to_syscall 5000, continue_to_syscall 5001} (c1:1)",
    "at a caught entry / continue, then exit: ok -> stopped call 3(7) = 5 by 5001 [continued e3, stopped e3 call 3(7) = 5 by 5001] {continue_to_syscall 5000, continue_to_syscall 5001, request_stop 5000} (c1:2)",
    "at a caught entry / step: ok -> running e3 [continued e3] {continue_to_syscall 5001, continue_to_syscall 5000} (c1:1)",
    "at a caught entry / uncatch: ok, continue ok -> running e3 [continued e3] {continue 5000 None, continue 5001 None} ()",
    "at a caught entry / catch getpid: ok, continue ok -> running e3 [continued e3] {continue_to_syscall 5000, continue_to_syscall 5001} (c1:1 c2:0)",
    "inside a call past its caught entry / continue: NotStopped -> running e3 [] {} (c1:1)",
    "inside a call past its caught entry / uncatch: ok -> running e3 [] {request_stop 5000, request_stop 5001, continue 5000 None, continue 5001 None} ()",
    "inside a call past its caught entry / catch getpid: ok -> running e3 [] {request_stop 5000, request_stop 5001, continue_to_syscall 5000, continue_to_syscall 5001} (c1:1 c2:0)",
    "inside a call past its caught entry / entry of close: - -> stopped call 3(7) at entry by 5001 [stopped e3 call 3(7) at entry by 5001] {request_stop 5000} (c1:2)",
    "inside a call past its caught entry / entry of getpid: - -> running e3 [] {continue_to_syscall 5001} (c1:1)",
    "inside a call past its caught entry / exit: - -> stopped call 3(7) = 5 by 5001 [stopped e3 call 3(7) = 5 by 5001] {request_stop 5000} (c1:2)",
    "inside a call past its caught entry / signal: - -> running e3 [] {continue_to_syscall 5001} (c1:1)",
    "inside a call past its caught entry / signal, then exit: - -> stopped call 3(7) = 5 by 5001 [stopped e3 call 3(7) = 5 by 5001] {continue_to_syscall 5001, request_stop 5000} (c1:2)",
    "inside a call past its caught entry / sibling hits, then continue, then exit: continue ok -> stopped call 3(7) = 5 by 5001 [stopped e3 hit b1#1 by 5000, continued e4, stopped e4 call 3(7) = 5 by 5001] {set_registers 5000 rip=0x40, request_stop 5001, remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001, request_stop 5000} (c1:2)",
    "inside a call past its caught entry / thread exits: - -> running e3 [5001 exited] {continue 5001 None} (c1:1)",
    "at a caught exit / continue: ok -> running e4 [continued e4] {continue_to_syscall 5000, continue_to_syscall 5001} (c1:2)",
    "at a caught exit / continue, then exit: ok -> running e4 [continued e4] {continue_to_syscall 5000, continue_to_syscall 5001, continue_to_syscall 5001} (c1:2)",
    "at a caught exit / step: ok -> stopped Step { kind: Instruction } by 5001 [continued e4, stopped e4 Step { kind: Instruction } by 5001] {step 5001, continue_to_syscall 5000, request_stop 5000} (c1:2)",
    "at a caught exit / uncatch: ok, continue ok -> running e4 [continued e4] {continue 5000 None, continue 5001 None} ()",
    "at a caught exit / catch getpid: ok, continue ok -> running e4 [continued e4] {continue_to_syscall 5000, continue_to_syscall 5001} (c1:2 c2:0)",
    "inside a call past its uncaught entry / continue: NotStopped -> running e2 [] {} (c1:0)",
    "inside a call past its uncaught entry / uncatch: ok -> running e2 [] {request_stop 5000, request_stop 5001, continue 5000 None, continue 5001 None} ()",
    "inside a call past its uncaught entry / catch getpid: ok -> running e2 [] {request_stop 5000, request_stop 5001, continue_to_syscall 5000, continue_to_syscall 5001} (c1:0 c2:0)",
    "inside a call past its uncaught entry / entry of close: - -> running e2 [] {continue_to_syscall 5001} (c1:0)",
    "inside a call past its uncaught entry / entry of getpid: - -> running e2 [] {continue_to_syscall 5001} (c1:0)",
    "inside a call past its uncaught entry / exit: - -> stopped call 3(7) = 5 by 5001 [stopped e2 call 3(7) = 5 by 5001] {request_stop 5000} (c1:1)",
    "inside a call past its uncaught entry / signal: - -> running e2 [] {continue_to_syscall 5001} (c1:0)",
    "inside a call past its uncaught entry / signal, then exit: - -> stopped call 3(7) = 5 by 5001 [stopped e2 call 3(7) = 5 by 5001] {continue_to_syscall 5001, request_stop 5000} (c1:1)",
    "inside a call past its uncaught entry / sibling hits, then continue, then exit: continue ok -> stopped call 3(7) = 5 by 5001 [stopped e2 hit b1#1 by 5000, continued e3, stopped e3 call 3(7) = 5 by 5001] {set_registers 5000 rip=0x40, request_stop 5001, remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001, request_stop 5000} (c1:1)",
    "inside a call past its uncaught entry / thread exits: - -> running e2 [5001 exited] {continue 5001 None} (c1:0)",
    "before a syscall instruction / continue: ok -> running e2 [continued e2] {continue_to_syscall 5000, continue_to_syscall 5001} (c1:0)",
    "before a syscall instruction / continue, then exit: ok -> running e2 [continued e2] {continue_to_syscall 5000, continue_to_syscall 5001, continue_to_syscall 5001} (c1:0)",
    "before a syscall instruction / step: ok -> running e2 [continued e2] {continue_to_syscall 5001, continue_to_syscall 5000} (c1:0)",
    "before a syscall instruction / uncatch: ok, continue ok -> running e2 [continued e2] {continue 5000 None, continue 5001 None} ()",
    "before a syscall instruction / catch getpid: ok, continue ok -> running e2 [continued e2] {continue_to_syscall 5000, continue_to_syscall 5001} (c1:0 c2:0)",
    "stepping into a call / continue: NotStopped -> running e2 [] {} (c1:0)",
    "stepping into a call / uncatch: ok -> stopped Step { kind: Instruction } by 5001 [stopped e2 Step { kind: Instruction } by 5001] {request_stop 5000, request_stop 5001, step 5001, continue 5000 None, request_stop 5000} ()",
    "stepping into a call / catch getpid: ok -> running e2 [] {request_stop 5000, request_stop 5001, continue_to_syscall 5001, continue_to_syscall 5000} (c1:0 c2:0)",
    "stepping into a call / entry of close: - -> stopped call 3(7) at entry by 5001 [stopped e2 call 3(7) at entry by 5001] {request_stop 5000} (c1:1)",
    "stepping into a call / entry of getpid: - -> running e2 [] {continue_to_syscall 5001} (c1:0)",
    "stepping into a call / exit: - -> stopped Step { kind: Instruction } by 5001 [stopped e2 Step { kind: Instruction } by 5001] {request_stop 5000} (c1:0)",
    "stepping into a call / signal: - -> running e2 [] {install_site 0x5000, continue_to_syscall 5001} (c1:0)",
    "stepping into a call / signal, then exit: - -> running e2 [] {install_site 0x5000, continue_to_syscall 5001, continue_to_syscall 5001} (c1:0)",
    "stepping into a call / sibling hits, then continue, then exit: continue ok -> running e3 [stopped e2 hit b1#1 by 5000, continued e3] {set_registers 5000 rip=0x40, request_stop 5001, remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001, continue_to_syscall 5001} (c1:0)",
    "stepping into a call / thread exits: - -> stopped exit of 5001 by 5000 [5001 exited, stopped e2 exit of 5001 by 5000] {continue 5001 None, request_stop 5000} (c1:0)",
    "stepping inside an uncaught call / continue: NotStopped -> running e2 [] {} (c1:0)",
    "stepping inside an uncaught call / uncatch: ok -> stopped Step { kind: Instruction } by 5001 [stopped e2 Step { kind: Instruction } by 5001] {request_stop 5000, request_stop 5001, step 5001, continue 5000 None, request_stop 5000} ()",
    "stepping inside an uncaught call / catch getpid: ok -> running e2 [] {request_stop 5000, request_stop 5001, continue_to_syscall 5001, continue_to_syscall 5000} (c1:0 c2:0)",
    "stepping inside an uncaught call / entry of close: - -> stopped call 3(7) at entry by 5001 [stopped e2 call 3(7) at entry by 5001] {request_stop 5000} (c1:1)",
    "stepping inside an uncaught call / entry of getpid: - -> running e2 [] {continue_to_syscall 5001} (c1:0)",
    "stepping inside an uncaught call / exit: - -> stopped Step { kind: Instruction } by 5001 [stopped e2 Step { kind: Instruction } by 5001] {request_stop 5000} (c1:0)",
    "stepping inside an uncaught call / signal: - -> running e2 [] {install_site 0x5000, continue_to_syscall 5001} (c1:0)",
    "stepping inside an uncaught call / signal, then exit: - -> running e2 [] {install_site 0x5000, continue_to_syscall 5001, continue_to_syscall 5001} (c1:0)",
    "stepping inside an uncaught call / sibling hits, then continue, then exit: continue ok -> running e3 [stopped e2 hit b1#1 by 5000, continued e3] {set_registers 5000 rip=0x40, request_stop 5001, remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001, continue_to_syscall 5001} (c1:0)",
    "stepping inside an uncaught call / thread exits: - -> stopped exit of 5001 by 5000 [5001 exited, stopped e2 exit of 5001 by 5000] {continue 5001 None, request_stop 5000} (c1:0)",
    "in a vfork past its caught entry / continue: NotStopped, vfork done NotStopped -> running e3 [] {reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001} (c1:1)",
    "in a vfork past its caught entry / uncatch: pending, vfork done ok -> running e3 [] {reinstall_site 0x40, continue 5000 None, continue 5001 None} ()",
    "in a vfork past its caught entry / catch getpid: pending, vfork done ok -> running e3 [] {reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001} (c1:1 c2:0)",
    "in a vfork past its caught entry / vfork done, then exit: - -> stopped call 58(7) = 5 by 5001 [stopped e3 call 58(7) = 5 by 5001] {reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001, request_stop 5000} (c1:2)",
    "in a vfork past its caught entry / thread exits: - -> running e3 [5001 exited] {continue 5001 None, reinstall_site 0x40, continue_to_syscall 5000} (c1:1)",
    "at a fork inside an uncaught call / continue: ok -> running e3 [continued e3] {remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001} (c1:0)",
    "at a fork inside an uncaught call / continue, then exit: ok -> running e3 [continued e3] {remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001, continue_to_syscall 5001} (c1:0)",
    "at a fork inside an uncaught call / step: ok -> running e3 [continued e3] {remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5001, continue_to_syscall 5000} (c1:0)",
    "at a fork inside an uncaught call / uncatch: ok, continue ok -> running e3 [continued e3] {remove_site 0x40, step 5000, reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5001 None} ()",
    "at a fork inside an uncaught call / catch getpid: ok, continue ok -> running e3 [continued e3] {remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001} (c1:0 c2:0)",
    "at a fork inside a call past its caught entry / continue: ok -> running e4 [continued e4] {remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001} (c1:1)",
    "at a fork inside a call past its caught entry / continue, then exit: ok -> stopped call 57(7) = 5 by 5001 [continued e4, stopped e4 call 57(7) = 5 by 5001] {remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001, request_stop 5000} (c1:2)",
    "at a fork inside a call past its caught entry / step: ok -> running e4 [continued e4] {remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5001, continue_to_syscall 5000} (c1:1)",
    "at a fork inside a call past its caught entry / uncatch: ok, continue ok -> running e4 [continued e4] {remove_site 0x40, step 5000, reinstall_site 0x40, continue 5000 None, continue 5001 None, continue 5001 None} ()",
    "at a fork inside a call past its caught entry / catch getpid: ok, continue ok -> running e4 [continued e4] {remove_site 0x40, step 5000, reinstall_site 0x40, continue_to_syscall 5000, continue_to_syscall 5001} (c1:1 c2:0)",
];

/// Each run-control request and call event, in each phase of a thread's
/// system call, is answered and settles as it always has: which stop is
/// published and what call it names, whether each thread resumes to its
/// next system-call stop, and how many calls each catchpoint caught.
#[test]
fn each_call_event_settles_the_same_in_each_phase() {
    let actual: Vec<String> = PHASES
        .iter()
        .flat_map(|name| EVENTS.iter().filter_map(|event| outcome(name, event)))
        .collect();
    let rows = actual.join("\n");
    assert_eq!(
        EXPECTED,
        actual.as_slice(),
        "the call table changed; its rows are now:\n{rows}"
    );
}
