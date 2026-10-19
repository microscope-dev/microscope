//! The simulated Linux kernel: processes, threads, signals, ptrace, and
//! wait statuses, following the rules in `plans/simulator.md` (K-*).
//!
//! The kernel contains no randomness. Wherever Linux leaves an order open,
//! such as which thread runs next or which status a wait returns, it
//! exposes the options and the world decides. Requests it does not model
//! fail as model gaps, never with a plausible default.
//!
//! - [`ptrace`]: the tracer's requests and their errno outcomes.
//! - [`signals`]: generation, delivery, and exits.
//! - [`syscalls`]: the system calls the golden runtime makes.

pub mod catching;
pub mod debug_regs;
pub mod perf;
mod processes;
pub mod ptrace;
pub mod shadow;
pub mod signals;
pub mod syscalls;
#[cfg(test)]
mod tests;
pub mod watching;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use nix::libc;

use self::catching::{Catching, UnseenCatch};
use self::watching::Watching;
use super::cpu::{self, Outcome, RAX, Registers};
use super::loader::Image;
use super::memory::AddressSpace;
#[cfg(test)]
use super::world::Sabotage;
use debug_regs::DebugRegisters;
use shadow::{Shadow, Tracking};
use signals::{Pending, SIGTRAP};

/// A thread identifier.
pub type Tid = i32;

/// A thread group's identifier: its process's id, which is its leader's
/// thread id but never stands in for a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tgid(Tid);

impl Tgid {
    /// The process the kernel's ABI numbers `id`.
    #[must_use]
    pub const fn new(id: Tid) -> Self {
        Self(id)
    }

    /// The process's number in the kernel's ABI.
    #[must_use]
    pub const fn id(self) -> Tid {
        self.0
    }

    /// The group `leader` leads.
    #[must_use]
    pub const fn led_by(leader: Tid) -> Self {
        Self(leader)
    }

    /// The group's leader.
    #[must_use]
    pub const fn leader(self) -> Tid {
        self.0
    }
}

impl std::fmt::Display for Tgid {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

/// The identifier the first simulated process gets.
const FIRST_TID: Tid = 1000;

/// The user every simulated process runs as.
const UID: u32 = 1000;

/// `orig_rax` outside any system call.
pub const NO_SYSTEM_CALL: u64 = u64::MAX;

/// `rax` after a call that failed for a signal pending, to be restarted
/// whatever the signal does (K-SYS-7).
const ERESTARTNOINTR_RESULT: u64 = (-513_i64).cast_unsigned();

/// The length of the `syscall` instruction, which a restart rewinds.
const SYSCALL_LENGTH: u64 = 2;

/// What `rax` holds while a thread is inside a system call.
const ENOSYS_RESULT: u64 = (-(libc::ENOSYS as i64)).cast_unsigned();

/// What `waitpid` reports for a thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStatus {
    Exited(Tid, i32),

    /// Killed by a signal, and whether it dumped core.
    Signaled(Tid, i32, bool),

    /// A signal-delivery-stop.
    Stopped(Tid, i32),

    /// A `PTRACE_EVENT_*` stop.
    Event(Tid, i32),

    /// A seized thread's group-stop: `PTRACE_EVENT_STOP` with the signal
    /// that stopped its process (K-STOP-2).
    GroupStop(Tid, i32),

    /// A system-call-stop, as `PTRACE_O_TRACESYSGOOD` reports it:
    /// `SIGTRAP | 0x80` (K-SYS-1).
    Syscall(Tid),
}

impl WaitStatus {
    #[must_use]
    pub const fn tid(self) -> Tid {
        match self {
            Self::Exited(tid, _)
            | Self::Signaled(tid, _, _)
            | Self::Stopped(tid, _)
            | Self::Event(tid, _)
            | Self::GroupStop(tid, _)
            | Self::Syscall(tid) => tid,
        }
    }
}

impl fmt::Display for WaitStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Exited(tid, code) => write!(formatter, "{tid} exited {code}"),
            Self::Signaled(tid, signal, core) => write!(
                formatter,
                "{tid} killed by {}{}",
                signals::name(signal),
                if core { " (core)" } else { "" }
            ),
            Self::Stopped(tid, signal) => {
                write!(formatter, "{tid} stopped {}", signals::name(signal))
            }
            Self::Event(tid, event) => {
                write!(formatter, "{tid} event {}", ptrace::event_name(event))
            }
            Self::GroupStop(tid, signal) => {
                write!(formatter, "{tid} group-stop {}", signals::name(signal))
            }
            Self::Syscall(tid) => write!(formatter, "{tid} system-call-stop"),
        }
    }
}

/// The fields of the `siginfo_t` `PTRACE_GETSIGINFO` reads. Which of
/// `pid` and `address` mean anything depends on the signal and code, as in
/// the real structure, whose union overlays them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SigInfo {
    pub signal: i32,
    pub code: i32,

    /// `si_pid`: the sending process, for codes that name one.
    pub pid: i32,

    /// `si_addr`: the faulting address, for synchronous faults.
    pub address: u64,
}

impl SigInfo {
    /// The siginfo of `tid`'s `PTRACE_EVENT_*` stop: SIGTRAP with the
    /// event in the code's second byte, sent by the thread itself.
    const fn event(tid: Tid, event: i32) -> Self {
        Self {
            signal: SIGTRAP,
            code: SIGTRAP | (event << 8),
            pid: tid,
            // `si_pid` and `si_uid` overlay `si_addr`.
            address: (UID as u64) << 32 | tid.cast_unsigned() as u64,
        }
    }

    /// The siginfo of `tid`'s system-call-stop: SIGTRAP with `0x80` in the
    /// code, sent by the thread itself (K-SYS-1).
    const fn syscall(tid: Tid) -> Self {
        Self {
            code: SIGTRAP | 0x80,
            ..Self::event(tid, 0)
        }
    }

    /// The siginfo of `tid`'s group-stop for `signal`: the stop's event in
    /// the code's second byte, as for an event stop, with that signal.
    const fn group_stop(tid: Tid, signal: i32) -> Self {
        Self {
            signal,
            code: signal | (libc::PTRACE_EVENT_STOP << 8),
            ..Self::event(tid, libc::PTRACE_EVENT_STOP)
        }
    }
}

/// How a thread or process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i32),

    /// The signal that killed it, and whether it dumped core.
    Signal(i32, bool),
}

impl ExitStatus {
    /// The message `PTRACE_GETEVENTMSG` reports at the exit event, which is
    /// the status `waitpid` will report.
    const fn event_message(self) -> u64 {
        match self {
            Self::Code(code) => ((code & 0xff).cast_unsigned() as u64) << 8,
            Self::Signal(signal, core) => {
                signal.cast_unsigned() as u64 | if core { 0x80 } else { 0 }
            }
        }
    }

    const fn wait_status(self, tid: Tid) -> WaitStatus {
        match self {
            Self::Code(code) => WaitStatus::Exited(tid, code & 0xff),
            Self::Signal(signal, core) => WaitStatus::Signaled(tid, signal, core),
        }
    }
}

/// Why a stopped thread stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    /// A signal-delivery-stop: the signal waits for the tracer's verdict.
    Signal(i32),

    /// A `PTRACE_EVENT_*` stop other than the exit event, with the message
    /// `PTRACE_GETEVENTMSG` reports.
    Event(i32, u64),

    /// The `PTRACE_EVENT_EXIT` stop of a thread ending this way.
    Exit(ExitStatus),

    /// A seized thread's group-stop, for the signal that stopped its
    /// process (K-STOP-2).
    Group(i32),

    /// A system-call-stop at a call's entry, or at its exit (K-SYS-1,
    /// K-SYS-2).
    Syscall { exit: bool },
}

/// Where a thread is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Executing instructions whenever the world runs it.
    Running,

    /// In a ptrace-stop, answering the tracer's requests.
    Stopped { kind: StopKind, info: SigInfo },

    /// Untraced, in its process's job-control stop, until SIGCONT ends it
    /// or a tracer seizes it (K-STOP-1 to K-STOP-3).
    JobStopped,

    /// On its way out, which takes it to its exit event the next time it
    /// runs, or straight to a zombie when exits are not traced.
    Exiting(ExitStatus),

    /// Gone, having ended this way, but not yet reaped.
    Zombie(ExitStatus),
}

impl State {
    /// Whether the thread has begun to exit.
    #[must_use]
    pub const fn exiting(self) -> bool {
        matches!(
            self,
            Self::Exiting(_)
                | Self::Zombie(_)
                | Self::Stopped {
                    kind: StopKind::Exit(_),
                    ..
                }
        )
    }
}

/// The `PTRACE_O_*` options the simulation honors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each is one independent PTRACE_O_* flag"
)]
pub struct Options {
    pub trace_clone: bool,
    pub trace_fork: bool,
    pub trace_exit: bool,
    pub exit_kill: bool,
}

/// A byte of a process's code that differs from its program's: its
/// address, the byte, and the program's.
pub type Planted = (u64, u8, u8);

/// Whether, and how, a tracer traces a thread. Only the tracer named
/// reaches the thread with its requests, and only its waits report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracing {
    /// Not at all: the thread runs as if no debugger were there, as one the
    /// tracer detached does (K-FORK-2).
    Untraced,

    /// Attached, as a launched program and the threads it creates are.
    Attached { tracer: i32 },

    /// Seized, or created by a seized thread: interrupts reach it, the
    /// threads and processes it creates first stop for one, and one may
    /// wait to stop it, which any stop takes the place of (K-SEIZE-1,
    /// K-INT-1).
    Seized { tracer: i32, interrupted: bool },
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "each is one independent piece of a thread's kernel state"
)]
pub struct Thread {
    pub tid: Tid,
    pub tgid: Tgid,
    pub tracing: Tracing,
    pub registers: Registers,

    /// `orig_rax`: the system call the thread entered last, until it runs
    /// another instruction or takes an exception, or [`NO_SYSTEM_CALL`].
    pub orig_rax: u64,

    /// The result of a system call the thread stopped inside. `rax` holds
    /// `-ENOSYS` until the call returns it.
    pub returning: Option<u64>,
    pub state: State,
    pub options: Options,

    /// Signals pending for this thread alone.
    pub pending: Pending,

    /// Whether the thread stops after its next instruction.
    pub single_step: bool,

    /// Whether the tracer resumed it with `PTRACE_SYSCALL`, to stop at
    /// each system call's entry and exit (K-SYS-1).
    pub syscall_stops: bool,

    /// Whether the thread stopped at a system call's entry and makes the
    /// call when it next runs (K-SYS-2).
    pub entered: bool,

    /// Whether the call the thread made last failed with
    /// `-ERESTARTNOINTR` for a signal pending, to be made again as the
    /// thread returns to its program (K-SYS-7).
    pub restarting: bool,

    /// How many times the kernel rewound the thread over a `syscall` to
    /// make a call again (K-SYS-7).
    pub restarts: u64,

    /// Whether its tracer has resumed it, as it has not a thread it seized
    /// running, which makes calls before the tracer can catch them.
    pub resumed: bool,

    /// The status a wait would report now, until one reaps it.
    pub report: Option<WaitStatus>,

    /// The address of the trap the thread executed, until it executes
    /// another instruction.
    pub trapped_at: Option<u64>,

    /// Where the thread last executed a trap, and how many instructions it
    /// had completed then.
    pub last_trap: Option<(u64, u64)>,

    /// How many instructions the thread completed, `syscall` among them.
    pub retired: u64,

    /// The calls the thread made and has not returned from.
    pub shadow: Shadow,
    pub debug: DebugRegisters,

    /// The hardware breakpoints of this thread others hold, as perf can.
    pub debug_held: usize,
}

impl Thread {
    #[must_use]
    pub const fn is_stopped(&self) -> bool {
        matches!(self.state, State::Stopped { .. })
    }

    /// Whether a tracer traces the thread.
    #[must_use]
    pub const fn traced(&self) -> bool {
        !matches!(self.tracing, Tracing::Untraced)
    }

    /// The tracer that traces the thread, if one does.
    #[must_use]
    pub const fn tracer(&self) -> Option<i32> {
        match self.tracing {
            Tracing::Untraced => None,
            Tracing::Attached { tracer } | Tracing::Seized { tracer, .. } => Some(tracer),
        }
    }

    /// Whether its tracer seized the thread, or traces it as one a seized
    /// thread created.
    #[must_use]
    pub const fn seized(&self) -> bool {
        matches!(self.tracing, Tracing::Seized { .. })
    }

    /// Whether the thread is held in a ptrace-stop it would leave only for
    /// the tracer: any but its exit event, which leads only to its end.
    #[must_use]
    pub const fn held(&self) -> bool {
        matches!(
            self.state,
            State::Stopped { kind, .. } if !matches!(kind, StopKind::Exit(_))
        )
    }

    /// Enters a ptrace-stop that reports `report`. Any stop takes the place
    /// of an interrupt still waiting (K-INT-1).
    pub(super) const fn enter_stop(&mut self, kind: StopKind, info: SigInfo, report: WaitStatus) {
        self.state = State::Stopped { kind, info };
        self.report = Some(report);
        if let Tracing::Seized { interrupted, .. } = &mut self.tracing {
            *interrupted = false;
        }
    }

    /// The thread `id` of `group` that `creator` made with system call
    /// `call`, which returns zero in it. A creator that traces the creation
    /// passes on its tracing and options: the new thread first stops for an
    /// interrupt if the creator was seized (K-SEIZE-1), otherwise for a
    /// SIGSTOP nobody sent (K-CLONE-1, K-FORK-1). Its debug registers are
    /// disarmed, though DR7 reads as the creator's, and slots others hold
    /// follow it, as perf's do (K-DR-3).
    fn created(
        creator: &Self,
        id: Tid,
        group: Tgid,
        call: u64,
        traced: bool,
        shadow: Shadow,
    ) -> Self {
        let seized = traced && creator.seized();
        let mut registers = creator.registers;
        registers.general[RAX] = 0;
        let mut pending = Pending::default();
        if traced && !seized {
            pending.insert(SigInfo {
                signal: signals::SIGSTOP,
                code: signals::SI_USER,
                pid: 0,
                address: 0,
            });
        }

        Self {
            tid: id,
            tgid: group,
            tracing: match creator.tracer() {
                Some(debugger) if seized => Tracing::Seized {
                    tracer: debugger,
                    interrupted: true,
                },
                Some(debugger) if traced => Tracing::Attached { tracer: debugger },
                _ => Tracing::Untraced,
            },
            registers,
            orig_rax: call,
            returning: None,
            state: State::Running,
            options: if traced {
                creator.options
            } else {
                Options::default()
            },
            pending,
            single_step: false,
            // K-SYS-3: a new thread does not inherit system-call-stops.
            syscall_stops: false,
            entered: false,
            restarting: false,
            restarts: 0,
            resumed: false,
            report: None,
            trapped_at: None,
            last_trap: None,
            retired: 0,
            shadow,
            debug: creator.debug.inherited(),
            debug_held: creator.debug_held,
        }
    }

    /// Whether a running thread is in its program's code, rather than in
    /// the kernel: inside a system call, returning from one to restart it
    /// (K-SYS-7), or on its way to take a signal its last instruction
    /// raised (K-TRAP-2).
    #[must_use]
    pub fn in_user_code(&self) -> bool {
        self.state == State::Running
            && self.returning.is_none()
            && !self.entered
            && !self.restarting
            && !self.pending.raised()
    }

    /// Whether running the thread can change anything.
    #[must_use]
    pub const fn can_run(&self) -> bool {
        matches!(self.state, State::Running | State::Exiting(_))
    }
}

pub struct Process {
    pub tgid: Tgid,

    /// The name `comm` reports: the executable's file name, cut to fifteen
    /// bytes.
    pub name: Arc<str>,
    pub space: AddressSpace,
    pub image: Arc<Image>,

    /// Who reaps the process once it ends.
    pub parent: Parent,

    /// The thread that forked it, which `/proc` lists it under, until that
    /// thread exits and another of its group takes it on.
    pub creator: Option<Tid>,

    /// The process the tracer launched whose standard output and error
    /// this one shares, its own included.
    pub root: Tid,

    /// Signals pending for the whole process, which whichever of its
    /// threads looks first takes.
    pub shared: Pending,

    /// The status of the group exit: from `exit_group`, a fatal signal, or
    /// the thread that began to exit last (K-EXIT-6).
    pub group_exit: Option<ExitStatus>,

    /// Whether something outside the session killed the process.
    pub killed_externally: bool,

    /// The signal that stopped the process, while it is job-control
    /// stopped (K-STOP-1).
    pub stopped: Option<i32>,

    /// Whether SIGCONT ended the process's stop, which its parent hears of
    /// once one of its threads next runs (K-STOP-3).
    pub continued: bool,

    /// When the process started, as `/proc/<pid>/stat` counts it: no two
    /// processes share one.
    pub start_time: u64,
}

/// Who reaps a process once it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parent {
    /// The tracer, which launched it.
    Tracer,

    /// Whoever started it untraced, for the tracer to attach to later,
    /// which reaps it at once.
    Launcher,

    /// The process that forked it, with `wait4`, after SIGCHLD.
    Process(Tgid),

    /// The reaper orphans pass to, which reaps them at once (K-FORK-3).
    Init,
}

impl fmt::Display for Parent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tracer => formatter.write_str("the tracer"),
            Self::Launcher => formatter.write_str("its launcher"),
            Self::Process(tgid) => write!(formatter, "process {tgid}"),
            Self::Init => formatter.write_str("init"),
        }
    }
}

/// A thread a tracer's `waitpid` reaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reaped {
    pub tgid: Tgid,
    pub status: WaitStatus,

    /// Whether its process was exiting as a whole, by `exit_group` or a
    /// fatal signal.
    pub group_exit: bool,
}

/// How a process ended, and who reaped it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    pub status: ExitStatus,

    /// The process the tracer launched whose output this one shared.
    pub root: Tid,
    pub reaper: Parent,

    /// Whether something outside the session killed it.
    pub killed_externally: bool,
}

/// A process that ended, waiting for its parent to reap it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zombie {
    pub parent: Tgid,
    pub creator: Option<Tid>,
    pub status: ExitStatus,
    pub root: Tid,
    pub killed_externally: bool,
}

/// Something the simulation does not model, which ends the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGap(pub String);

/// Something that happened in the kernel which the world counts or
/// reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Happening {
    /// A thread created another.
    Cloned { parent: Tid, child: Tid },

    /// A thread called `exit_group`.
    GroupExit { tid: Tid },

    /// A group exit or SIGKILL took a thread out of a ptrace-stop.
    PulledFromStop { tid: Tid },

    /// A thread's fork or clone failed for a signal pending, to be made
    /// again (K-SYS-7).
    Restarting { tid: Tid },

    /// An interrupt stopped a thread whose trap or fault is still queued
    /// (K-TRAP-2).
    InterruptAheadOfTrap { tid: Tid },

    /// A group leader exited while other threads ran on.
    LeaderExitedAlone { tid: Tid },

    /// A thread yielded the CPU.
    Yielded { tid: Tid },

    /// A thread forked a process.
    Forked { parent: Tid, child: Tid },

    /// `tracer` detached a thread, leaving the first byte of its code that
    /// differs from the program's, as its address, the byte, and the
    /// program's.
    Released {
        tid: Tid,
        tracer: i32,
        planted: Option<Planted>,
    },

    /// A process reaped a child it forked.
    ReapedChild { parent: Tid, child: Tid },

    /// Init reaped a process whose parent was gone.
    ReapedOrphan { tgid: Tgid },

    /// The tracer tried to seize a thread that had finished exiting.
    SeizeRefused { tid: Tid },

    /// An interrupt reached a thread already stopped, which it stops again
    /// as soon as it resumes.
    InterruptWaits { tid: Tid },

    /// An untraced thread entered its process's job-control stop.
    JobStopped { tid: Tid },

    /// SIGCONT ended a process's job-control stop.
    Continued { tgid: Tgid },

    /// The tracer resumed a thread from a signal-delivery-stop without
    /// its signal.
    Suppressed { tid: Tid, signal: i32 },

    /// A signal reached the program, untraced or passed on by its tracer,
    /// and the thread took its default action.
    Delivered { tid: Tid, signal: i32 },
}

/// A thread that executed the program's own instruction at an address
/// where the user's breakpoint is enabled, without having just trapped
/// there: a hit the debugger could not see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnseenHit {
    pub tid: Tid,
    pub address: u64,
}

pub struct Kernel {
    /// The process identifier of the simulated debugger that launches
    /// programs, as their parent.
    tracer: i32,

    /// The tracer whose request the kernel serves, as `current` names the
    /// calling task in Linux: its requests reach only the threads it
    /// traces, and the signals it sends name it.
    caller: std::cell::Cell<i32>,
    next_tid: Tid,
    pub processes: BTreeMap<Tgid, Process>,
    pub threads: BTreeMap<Tid, Thread>,

    /// Processes reaped, with how they ended.
    pub ended: BTreeMap<Tgid, Ended>,

    /// Processes that ended and wait for their parents to reap them.
    pub zombies: BTreeMap<Tgid, Zombie>,

    /// What each process the tracer launched, and the processes it forked,
    /// wrote to standard output and error, in order, by launched process.
    pub outputs: BTreeMap<Tid, Vec<u8>>,

    /// Each thread a tracer reaped, with the status it reported.
    pub reaped: BTreeMap<Tid, Reaped>,

    /// The code each group leader passed to `exit`: its own status, which
    /// the process's replaces when it is reaped (K-EXIT-6).
    pub leader_exits: BTreeMap<Tid, i32>,

    /// The first model gap the run hit.
    pub gap: Option<ModelGap>,

    /// What happened since the world last looked.
    pub happenings: Vec<Happening>,

    /// The process the debugger controls, whose memory the user's
    /// breakpoints and watches are in.
    pub debugged: Option<Tgid>,

    /// Addresses where the user's breakpoints are certainly enabled, as
    /// the client last knew them. The breakpoint-accounting oracle watches
    /// executions there.
    pub user_breakpoints: BTreeSet<u64>,

    /// The addresses among them only temporary breakpoints own, for
    /// [`Sabotage::SkipTemporaryTraps`].
    #[cfg(test)]
    pub temporary_breakpoints: BTreeSet<u64>,

    /// Executions at those addresses no trap reported.
    pub unseen_hits: Vec<UnseenHit>,

    /// Where the thread the client steps has been, while it steps.
    pub tracking: Option<Tracking>,

    /// What the kernel follows of the client's watchpoints.
    pub watching: Watching,

    /// What the kernel follows of the client's catchpoints.
    pub catching: Catching,

    /// The calls served since the world last took them.
    pub served: Vec<syscalls::Served>,

    /// How the debug registers answer the tracer.
    pub debug_behavior: DebugBehavior,

    /// The execute breakpoint a probe armed to sample a process.
    pub perf: Option<perf::Breakpoint>,

    /// The identifier the next activation takes.
    next_activation: u64,

    /// The start time the next process takes.
    next_start_time: u64,

    /// A deliberate defect, so tests can check that the oracles notice.
    #[cfg(test)]
    pub sabotage: Option<Sabotage>,

    /// The thread whose single step went on past one instruction, under
    /// [`Sabotage::LateSingleSteps`].
    #[cfg(test)]
    late_step: Option<Tid>,

    /// The slots each thread's next instruction reports hit again, under
    /// [`Sabotage::RepeatWatchTraps`].
    #[cfg(test)]
    repeat_watch: BTreeMap<Tid, u64>,

    /// How many small stack words ptrace has read, under
    /// [`Sabotage::FlickeringStackWords`].
    #[cfg(test)]
    flickers: std::cell::Cell<u64>,

    /// The debug registers the tracer believes it wrote, under
    /// [`Sabotage::PhantomArming`].
    #[cfg(test)]
    pub(super) phantom_debug: BTreeMap<Tid, debug_regs::DebugRegisters>,
}

/// How a thread's debug registers answer the tracer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugBehavior {
    /// As Linux's do (K-DR-*).
    Faithful,

    /// Writes succeed and change nothing, and reads give zero, as under
    /// gVisor.
    Discarding,

    /// Others hold this many hardware breakpoints of every thread the
    /// program creates, as a perf session following new threads can, so
    /// fewer of their slots take an address (K-DR-4).
    Contended(usize),
}

/// What running a thread for a while did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ran {
    /// How many instructions it executed.
    pub executed: u64,

    /// Whether it gave up the CPU with `sched_yield`.
    pub yielded: bool,
}

impl Kernel {
    #[must_use]
    pub const fn new(tracer: i32) -> Self {
        Self {
            tracer,
            caller: std::cell::Cell::new(tracer),
            next_tid: FIRST_TID,
            processes: BTreeMap::new(),
            threads: BTreeMap::new(),
            ended: BTreeMap::new(),
            zombies: BTreeMap::new(),
            outputs: BTreeMap::new(),
            reaped: BTreeMap::new(),
            leader_exits: BTreeMap::new(),
            gap: None,
            happenings: Vec::new(),
            debugged: None,
            user_breakpoints: BTreeSet::new(),
            #[cfg(test)]
            temporary_breakpoints: BTreeSet::new(),
            unseen_hits: Vec::new(),
            tracking: None,
            watching: Watching::new(),
            catching: Catching {
                rules: Vec::new(),
                unseen: Vec::new(),
                unreported: BTreeMap::new(),
                skipped: Vec::new(),
            },
            served: Vec::new(),
            debug_behavior: DebugBehavior::Faithful,
            perf: None,
            next_activation: 1,
            next_start_time: 1,

            #[cfg(test)]
            sabotage: None,

            #[cfg(test)]
            phantom_debug: BTreeMap::new(),

            #[cfg(test)]
            late_step: None,

            #[cfg(test)]
            repeat_watch: BTreeMap::new(),

            #[cfg(test)]
            flickers: std::cell::Cell::new(0),
        }
    }

    /// Serves `tracer`'s requests from now on.
    pub fn serve(&self, tracer: i32) {
        self.caller.set(tracer);
    }

    /// The tracer whose requests the kernel serves.
    #[must_use]
    pub const fn caller(&self) -> i32 {
        self.caller.get()
    }

    /// Records that the run reached something the simulation does not
    /// model. The first gap is the one reported.
    pub fn gap(&mut self, description: impl Into<String>) {
        self.gap.get_or_insert_with(|| ModelGap(description.into()));
    }

    const fn allocate_start_time(&mut self) -> u64 {
        let start_time = self.next_start_time;
        self.next_start_time += 1;
        start_time
    }

    const fn allocate_tid(&mut self) -> Tid {
        let tid = self.next_tid;
        self.next_tid += 1;
        tid
    }

    /// The shadow of a thread that has made no call yet.
    const fn new_shadow(&mut self) -> Shadow {
        let base = self.next_activation;
        self.next_activation += 1;
        Shadow {
            base,
            calls: Vec::new(),
            jumps: Vec::new(),
            lost: false,
        }
    }

    /// Starts `image` as a traced child that has just executed it (K-EXEC-1):
    /// it reports a stop for `SIGTRAP` from itself, at its entry point.
    pub fn spawn(
        &mut self,
        image: Arc<Image>,
        name: &str,
        arguments: &[String],
        random: [u8; 16],
    ) -> Tid {
        self.start(image, name, arguments, random, true)
    }

    /// Starts `image` untraced, running from its entry point, for the
    /// tracer to attach to.
    pub fn spawn_untraced(
        &mut self,
        image: Arc<Image>,
        name: &str,
        arguments: &[String],
        random: [u8; 16],
    ) -> Tid {
        self.start(image, name, arguments, random, false)
    }

    fn start(
        &mut self,
        image: Arc<Image>,
        name: &str,
        arguments: &[String],
        random: [u8; 16],
        traced: bool,
    ) -> Tid {
        let tid = self.allocate_tid();
        let shadow = self.new_shadow();
        let (space, registers) = image.load(arguments, random);
        let comm = name.rsplit('/').next().unwrap_or(name);
        let comm = &comm[..comm.len().min(15)];
        let start_time = self.allocate_start_time();

        self.processes.insert(
            Tgid::led_by(tid),
            Process {
                tgid: Tgid::led_by(tid),
                name: Arc::from(comm),
                space,
                image,
                parent: if traced {
                    Parent::Tracer
                } else {
                    Parent::Launcher
                },
                creator: None,
                root: tid,
                shared: Pending::default(),
                group_exit: None,
                killed_externally: false,
                stopped: None,
                continued: false,
                start_time,
            },
        );

        self.outputs.insert(tid, Vec::new());
        let info = SigInfo {
            signal: SIGTRAP,
            code: signals::SI_USER,
            pid: tid,
            address: 0,
        };

        self.threads.insert(
            tid,
            Thread {
                tid,
                tgid: Tgid::led_by(tid),
                tracing: if traced {
                    Tracing::Attached {
                        tracer: self.tracer,
                    }
                } else {
                    Tracing::Untraced
                },
                registers,
                // The stop is at the end of `execve`.
                orig_rax: syscalls::SYS_EXECVE,
                syscall_stops: false,
                entered: false,
                restarting: false,
                restarts: 0,
                resumed: false,
                returning: None,
                state: if traced {
                    State::Stopped {
                        kind: StopKind::Signal(SIGTRAP),
                        info,
                    }
                } else {
                    State::Running
                },
                options: Options::default(),
                pending: Pending::default(),
                single_step: false,
                report: traced.then_some(WaitStatus::Stopped(tid, SIGTRAP)),
                trapped_at: None,
                last_trap: None,
                retired: 0,
                shadow,
                debug: DebugRegisters::default(),
                debug_held: 0,
            },
        );
        tid
    }

    /// Threads that can make progress if run.
    pub fn runnable(&self) -> impl Iterator<Item = Tid> + '_ {
        self.threads
            .values()
            .filter(|thread| thread.can_run())
            .map(|thread| thread.tid)
    }

    /// Threads with a status `tracer`'s wait would report now. A group
    /// leader's exit waits until every other thread of its group is reaped
    /// (K-WAIT-2).
    pub fn reportable(&self, tracer: i32) -> impl Iterator<Item = Tid> + '_ {
        self.threads
            .values()
            .filter(move |thread| {
                thread.tracer() == Some(tracer) && thread.report.is_some() && !self.delayed(thread)
            })
            .map(|thread| thread.tid)
    }

    /// Whether `thread` is a zombie leader whose group still has others.
    fn delayed(&self, thread: &Thread) -> bool {
        thread.tid == thread.tgid.leader()
            && matches!(thread.state, State::Zombie(_))
            && self
                .threads
                .values()
                .any(|other| other.tgid == thread.tgid && other.tid != thread.tid)
    }

    /// Reaps `tid`'s status, as the waiter's `waitpid` does. A zombie is
    /// released, and its process ends with its last thread.
    pub fn collect(&mut self, tid: Tid) -> Option<WaitStatus> {
        let thread = self.threads.get(&tid)?;
        if thread.report.is_none() || self.delayed(thread) {
            return None;
        }

        let thread = self.threads.get_mut(&tid).expect("the thread exists");
        let mut status = thread.report.take().expect("a report");

        if let State::Zombie(own) = thread.state {
            let group = thread.tgid;
            let leader = tid == group.leader();
            self.threads.remove(&tid);
            let process = self.processes.get(&group).expect("a zombie's process");

            // K-EXIT-6: a group exit decides every status reaped after it.
            let exit = process.group_exit.unwrap_or(own);
            status = exit.wait_status(tid);

            self.reaped.insert(
                tid,
                Reaped {
                    tgid: group,
                    status,
                    group_exit: process.group_exit.is_some(),
                },
            );

            if leader {
                self.end_process(group, exit);
            } else {
                self.end_with_untraced_leader(group);
            }
        }

        Some(status)
    }

    /// Runs `tid` for up to `budget` instructions, stopping early when it
    /// stops, exits, yields, or hits a gap.
    pub fn run(&mut self, tid: Tid, budget: u64) -> Ran {
        let mut ran = Ran {
            executed: 0,
            yielded: false,
        };

        while ran.executed < budget && self.gap.is_none() {
            let Some(thread) = self.threads.get_mut(&tid) else {
                break;
            };

            match thread.state {
                State::Exiting(exit) => {
                    self.reach_exit(tid, exit);
                    break;
                }
                State::Running => {}
                State::Stopped { .. } | State::JobStopped | State::Zombie(_) => break,
            }

            if let Some(result) = thread.returning.take() {
                // The system call the thread stopped inside returns.
                thread.registers.general[RAX] = result;
                if thread.single_step {
                    // K-TRAP-1: a step across `syscall` reports TRAP_BRKPT.
                    self.trap(tid, signals::TRAP_BRKPT);
                    self.note_return(tid);
                    break;
                }

                if thread.syscall_stops {
                    // K-SYS-3: the call stops at its exit as it returns.
                    self.syscall_stop(tid, true);
                    break;
                }

                self.note_return(tid);
                continue;
            }

            if std::mem::take(&mut thread.entered) {
                // K-SYS-2: the call the thread stopped at the entry of runs
                // before anything else can happen to the thread.
                let single_step = thread.single_step;
                match self.serve_entered(tid, single_step) {
                    Executed::Continue => continue,
                    Executed::Stopped => break,
                    Executed::Yielded => {
                        ran.yielded = true;
                        self.happenings.push(Happening::Yielded { tid });
                        break;
                    }
                }
            }

            self.report_continued(tid);
            if self.stop_for_interrupt(tid) || self.deliver_pending(tid) {
                break;
            }

            self.restart_call(tid);
            ran.executed += 1;
            match self.execute(tid) {
                Executed::Continue => {}
                Executed::Stopped => break,
                Executed::Yielded => {
                    ran.yielded = true;
                    self.happenings.push(Happening::Yielded { tid });
                    break;
                }
            }
        }

        ran
    }

    /// Executes one instruction of a running thread.
    fn execute(&mut self, tid: Tid) -> Executed {
        self.sample_breakpoint(tid);
        let thread = &self.threads[&tid];

        // What a thread does is the debugger's to report only while it
        // traces the thread, in the process it debugs.
        let debugged = thread.traced() && self.debugged == Some(thread.tgid);
        if debugged {
            self.watching.resume(tid);
        }

        let thread = self.threads.get_mut(&tid).expect("running thread exists");
        let process = self
            .processes
            .get_mut(&thread.tgid)
            .expect("a thread's process exists");

        let address = thread.registers.rip;
        let trap = process.space.peek_bytes(address, 1).as_deref() == Some(&[0xcc]);
        let user_breakpoint = debugged && self.user_breakpoints.contains(&address);
        #[cfg(test)]
        let skipped = trap
            && debugged
            && skips_trap(
                self.sabotage,
                &self.temporary_breakpoints,
                address,
                user_breakpoint,
            );
        #[cfg(not(test))]
        let skipped = false;

        if user_breakpoint && thread.trapped_at != Some(address) && (!trap || skipped) {
            self.unseen_hits.push(UnseenHit { tid, address });
        }

        thread.trapped_at = None;
        let single_step = thread.single_step;
        let before = self.watching.bytes(debugged, &process.space);
        #[cfg(test)]
        let cpu::Executed {
            outcome,
            flow,
            accesses,
        } = if skipped {
            step_under_trap(
                &mut thread.registers,
                &mut process.space,
                &process.image,
                single_step,
            )
        } else {
            cpu::execute(&mut thread.registers, &mut process.space, single_step)
        };

        #[cfg(not(test))]
        let cpu::Executed {
            outcome,
            flow,
            accesses,
        } = cpu::execute(&mut thread.registers, &mut process.space, single_step);

        // K-DR-1: an access an enabled slot covers raises a debug
        // exception once the instruction completes.
        let watch_hits = if outcome == Outcome::Completed {
            thread.debug.hits(&accesses)
        } else {
            0
        };

        #[cfg(test)]
        let watch_hits = sabotaged_watch_hits(
            self.sabotage,
            &mut self.repeat_watch,
            (tid, thread.tgid.leader()),
            &outcome,
            watch_hits,
        );

        if thread.debug.breaks_on_execution() {
            self.gap("debug registers breaking on execution");
            return Executed::Stopped;
        }

        let after = if accesses.iter().any(|access| access.write) {
            self.watching.bytes(debugged, &process.space)
        } else {
            before.clone()
        };

        self.watching
            .follow(tid, &thread.debug, &accesses, &before, &after);

        if matches!(outcome, Outcome::Completed | Outcome::Syscall) {
            if let Some(tracking) = &mut self.tracking {
                tracking.note(tid, &thread.shadow, address);
            }

            thread.retired += 1;
            thread
                .shadow
                .follow(flow, &thread.registers, &mut self.next_activation);
        }

        if outcome != Outcome::Syscall {
            // Instructions and exceptions enter the kernel, if at all, outside
            // any system call.
            thread.orig_rax = NO_SYSTEM_CALL;
        }

        match outcome {
            Outcome::Completed => self.complete(tid, single_step, watch_hits),
            Outcome::Syscall => {
                thread.orig_rax = thread.registers.general[RAX];
                self.enter_syscall(tid, single_step, debugged)
            }
            Outcome::Breakpoint | Outcome::Fault(_) | Outcome::Unsupported(_) => {
                self.take_exception(tid, address, outcome)
            }
        }
    }

    /// A thread executed `syscall`: it stops at the call's entry if its
    /// tracer resumed it to (K-SYS-1), and otherwise makes the call, which
    /// a catchpoint of the debugged process's may have had to stop.
    fn enter_syscall(&mut self, tid: Tid, single_step: bool, debugged: bool) -> Executed {
        let thread = self.threads.get_mut(&tid).expect("the calling thread");
        let caught = debugged
            && thread.resumed
            && self
                .catching
                .stops(thread.orig_rax, &thread.registers, false);
        #[cfg(test)]
        let stops =
            thread.syscall_stops && !(caught && self.sabotage == Some(Sabotage::SkipCallStops));
        #[cfg(not(test))]
        let stops = thread.syscall_stops;
        if stops && thread.traced() {
            thread.entered = true;
            self.syscall_stop(tid, false);
            return Executed::Stopped;
        }

        if caught {
            self.catching.unseen.push(UnseenCatch {
                tid,
                call: thread.orig_rax,
                exit: false,
            });
        }

        let executed = self.serve_syscall(tid, single_step);
        self.note_return(tid);
        executed
    }

    /// Serves the call a thread stopped at the entry of, which stops at its
    /// exit if the tracer resumed it with `PTRACE_SYSCALL` (K-SYS-2).
    fn serve_entered(&mut self, tid: Tid, single_step: bool) -> Executed {
        let executed = self.serve_syscall(tid, single_step);

        if self
            .threads
            .get(&tid)
            .is_some_and(|thread| thread.syscall_stops)
        {
            if executed == Executed::Stopped {
                return executed;
            }

            self.syscall_stop(tid, true);

            // A yield gave up the CPU before the exit stopped the thread.
            return if executed == Executed::Yielded {
                executed
            } else {
                Executed::Stopped
            };
        }

        self.note_return(tid);
        executed
    }

    /// Notes a call that returned with no system-call-stop at its exit, if
    /// a catchpoint stops it there: the debugger could not see it. A call
    /// still inside an event stop, or one that ended its thread, has not
    /// returned.
    fn note_return(&mut self, tid: Tid) {
        let Some(thread) = self.threads.get(&tid) else {
            return;
        };

        let inside = matches!(
            thread.state,
            State::Stopped {
                kind: StopKind::Event(..) | StopKind::Syscall { .. },
                ..
            }
        );
        let returned = thread.returning.is_none() && !inside && !thread.state.exiting();
        let debugged = thread.traced() && thread.resumed && self.debugged == Some(thread.tgid);

        if returned
            && debugged
            && self
                .catching
                .stops(thread.orig_rax, &thread.registers, true)
        {
            self.catching.unseen.push(UnseenCatch {
                tid,
                call: thread.orig_rax,
                exit: true,
            });
        }
    }

    /// Stops a thread at the entry or exit of the system call `orig_rax`
    /// names. At its entry, `rax` holds `-ENOSYS` (K-SYS-1).
    fn syscall_stop(&mut self, tid: Tid, exit: bool) {
        let thread = self.threads.get_mut(&tid).expect("stopping thread exists");
        if !exit {
            thread.registers.general[RAX] = ENOSYS_RESULT;
        }

        let debugged = thread.traced() && thread.resumed && self.debugged == Some(thread.tgid);
        if debugged
            && self
                .catching
                .stops(thread.orig_rax, &thread.registers, exit)
        {
            self.catching.unreported.insert(
                tid,
                UnseenCatch {
                    tid,
                    call: thread.orig_rax,
                    exit,
                },
            );
        }

        thread.enter_stop(
            StopKind::Syscall { exit },
            SigInfo::syscall(tid),
            WaitStatus::Syscall(tid),
        );
    }

    /// Serves the system call a thread just entered.
    fn serve_syscall(&mut self, tid: Tid, single_step: bool) -> Executed {
        let yielded = syscalls::serve(self, tid);

        // An untraced thread that exited is reaped at once.
        if !self
            .threads
            .get(&tid)
            .is_some_and(|thread| matches!(thread.state, State::Running))
        {
            return Executed::Stopped;
        }

        if single_step {
            // A yield gives up the CPU even under a single step.
            if yielded {
                self.happenings.push(Happening::Yielded { tid });
            }

            // K-TRAP-1: a step across `syscall` reports TRAP_BRKPT.
            self.trap(tid, signals::TRAP_BRKPT);
            return Executed::Stopped;
        }

        if yielded {
            Executed::Yielded
        } else {
            Executed::Continue
        }
    }

    /// Raises the signal for the exception the instruction at `address`
    /// raised (K-TRAP-2).
    fn take_exception(&mut self, tid: Tid, address: u64, outcome: Outcome) -> Executed {
        match outcome {
            Outcome::Breakpoint => {
                // K-TRAP-1: int3 reports SI_KERNEL with rip past the trap.
                let thread = self
                    .threads
                    .get_mut(&tid)
                    .expect("the trapping thread exists");
                thread.trapped_at = Some(address);
                thread.last_trap = Some((address, thread.retired));

                let info = SigInfo {
                    signal: SIGTRAP,
                    code: cpu::SI_KERNEL,
                    ..SigInfo::default()
                };

                self.raise(tid, info);
            }
            Outcome::Fault(fault) => {
                let info = SigInfo {
                    signal: fault.signal,
                    code: fault.code,
                    pid: 0,
                    address: fault.address.unwrap_or(0),
                };

                self.raise(tid, info);
            }
            Outcome::Unsupported(description) => {
                self.gap(description);
            }
            Outcome::Completed | Outcome::Syscall => {
                unreachable!("{outcome:?} raises no exception")
            }
        }

        Executed::Stopped
    }

    /// Ends an instruction that completed: a single step or a watched
    /// access raises a debug exception, and otherwise the thread runs on.
    fn complete(&mut self, tid: Tid, single_step: bool, watch_hits: u64) -> Executed {
        if single_step {
            #[cfg(test)]
            if watch_hits == 0 && self.step_late(tid) {
                return Executed::Continue;
            }

            // K-TRAP-1: a single step reports TRAP_TRACE. K-DR-2: with DR6
            // holding the step and any slot that hit.
            self.threads
                .get_mut(&tid)
                .expect("the stepping thread exists")
                .debug
                .debug_exception(true, watch_hits);

            self.trap(tid, signals::TRAP_TRACE);
            return Executed::Stopped;
        }

        if watch_hits != 0 {
            // K-DR-1: TRAP_HWBKPT with `rip` after the instruction.
            self.threads
                .get_mut(&tid)
                .expect("the accessing thread exists")
                .debug
                .debug_exception(false, watch_hits);

            self.trap(tid, signals::TRAP_HWBKPT);
            return Executed::Stopped;
        }

        Executed::Continue
    }

    /// Whether a single step of the thread the client steps goes on to a
    /// second instruction, under [`Sabotage::LateSingleSteps`].
    #[cfg(test)]
    fn step_late(&mut self, tid: Tid) -> bool {
        let stepped = self
            .tracking
            .as_ref()
            .is_some_and(|tracking| tracking.tid == tid);
        if self.sabotage != Some(Sabotage::LateSingleSteps) || !stepped {
            return false;
        }

        self.late_step.take() != Some(tid) && {
            self.late_step = Some(tid);
            true
        }
    }

    /// The word at `address`, with the first byte of the call instruction
    /// the thread the client steps has come to hidden as a no-op, under
    /// [`Sabotage::HideSteppedCalls`].
    #[cfg(test)]
    fn hide_stepped_call(&self, tgid: Tgid, address: u64, word: u64) -> u64 {
        // Once the thread has moved: a call it stands at as the step begins
        // may be the one the step is to go into.
        let Some(thread) = self
            .tracking
            .as_ref()
            .filter(|tracking| !tracking.positions.is_empty())
            .and_then(|tracking| self.threads.get(&tracking.tid))
            .filter(|thread| thread.tgid == tgid)
        else {
            return word;
        };

        let rip = thread.registers.rip;
        let Some(offset) = rip.checked_sub(address).filter(|offset| *offset < 8) else {
            return word;
        };

        let calls = self.processes.get(&tgid).is_some_and(|process| {
            crate::sim::cpu::decode(rip, &process.space)
                .is_ok_and(|instruction| instruction.mnemonic() == iced_x86::Mnemonic::Call)
        });

        if !calls {
            return word;
        }

        let shift = offset * 8;

        (word & !(0xff << shift)) | (0x90 << shift)
    }

    /// A word ptrace reads, as a sabotaged kernel reports it.
    #[cfg(test)]
    pub(super) fn sabotage_read(&self, tgid: Tgid, address: u64, word: u64) -> u64 {
        let return_slot = |exact: bool| {
            self.threads_of(tgid).any(|thread| {
                thread
                    .shadow
                    .calls
                    .iter()
                    .any(|call| call.slot == address && (!exact || call.return_address == word))
            })
        };

        // Below where the main thread's first call pushed its return
        // address: what the program itself stored, not its entry's frame
        // or what the kernel laid out at exec.
        let computed = || {
            self.threads
                .get(&tgid.leader())
                .and_then(|leader| leader.shadow.calls.first())
                .is_some_and(|call| address < call.slot)
        };
        let small_on_stack = || {
            (1..0x1000).contains(&word)
                && !return_slot(false)
                && computed()
                && self.processes[&tgid].space.maps().lines().any(|line| {
                    line.ends_with("[stack]") && {
                        let range = line.split_whitespace().next().unwrap_or_default();
                        let (start, end) = range.split_once('-').unwrap_or_default();
                        (u64::from_str_radix(start, 16).unwrap_or(0)
                            ..u64::from_str_radix(end, 16).unwrap_or(0))
                            .contains(&address)
                    }
                })
        };

        match self.sabotage {
            Some(Sabotage::SkipLinkedNodes) if address.checked_add(8) == Some(word) => word + 16,
            Some(Sabotage::HideSteppedCalls) => self.hide_stepped_call(tgid, address, word),
            Some(Sabotage::SkewReturnAddresses) if return_slot(true) => word + 1,
            Some(Sabotage::SkewSmallStackWords) if small_on_stack() => word + 1,
            Some(Sabotage::FlickeringStackWords) if small_on_stack() => {
                let reads = self.flickers.get();
                self.flickers.set(reads + 1);
                word + reads % 2
            }
            _ => word,
        }
    }

    /// Raises `SIGTRAP` with `code` at a thread's current `rip`.
    fn trap(&mut self, tid: Tid, code: i32) {
        let rip = self.threads[&tid].registers.rip;
        self.raise(
            tid,
            SigInfo {
                signal: SIGTRAP,
                code,
                pid: 0,
                address: rip,
            },
        );
    }

    /// Queues the signal a trap or fault raised for the thread that raised
    /// it, which takes it as it takes any pending signal, before it runs
    /// another instruction: after an interrupt waiting for it, and ahead
    /// of any signal that is not synchronous (K-TRAP-2). The thread runs no
    /// further in this burst.
    fn raise(&mut self, tid: Tid, info: SigInfo) {
        self.threads
            .get_mut(&tid)
            .expect("the raising thread exists")
            .pending
            .insert(info);
    }

    /// Enters a signal-delivery-stop for `info`, or, for an untraced
    /// thread, takes the signal's default action.
    fn signal_stop(&mut self, tid: Tid, info: SigInfo) {
        let thread = self.threads.get_mut(&tid).expect("stopping thread exists");
        if !thread.traced() {
            self.act_by_default(tid, info.signal);
            return;
        }

        thread.enter_stop(
            StopKind::Signal(info.signal),
            info,
            WaitStatus::Stopped(tid, info.signal),
        );
    }

    /// Stops a thread inside the system call it is making for a
    /// `PTRACE_EVENT_*` stop. The call returns `result` once the thread
    /// resumes.
    fn event_stop(&mut self, tid: Tid, event: i32, message: u64, result: u64) {
        let thread = self.threads.get_mut(&tid).expect("stopping thread exists");
        thread.registers.general[RAX] = ENOSYS_RESULT;
        thread.returning = Some(result);
        thread.enter_stop(
            StopKind::Event(event, message),
            SigInfo::event(tid, event),
            WaitStatus::Event(tid, event),
        );
    }

    /// Whether a signal or an interrupt is pending for a thread, which
    /// makes a fork or clone it enters fail with `-ERESTARTNOINTR`
    /// (K-SYS-7), as Linux's `signal_pending` does `copy_process`.
    fn signal_pending(&self, tid: Tid) -> bool {
        let thread = &self.threads[&tid];
        let interrupted = matches!(
            thread.tracing,
            Tracing::Seized {
                interrupted: true,
                ..
            }
        );

        interrupted
            || !thread.pending.is_empty()
            || self
                .processes
                .get(&thread.tgid)
                .is_some_and(|process| !process.shared.is_empty())
    }

    /// Fails the fork or clone a thread is making for the signal pending
    /// (K-SYS-7): it creates nothing and returns `-ERESTARTNOINTR`, to be
    /// made again once the thread has taken what is pending.
    fn fail_to_restart(&mut self, tid: Tid) {
        let thread = self.threads.get_mut(&tid).expect("the calling thread");
        thread.registers.general[RAX] = ERESTARTNOINTR_RESULT;
        thread.restarting = true;
        self.happenings.push(Happening::Restarting { tid });
    }

    /// Makes a call that failed to restart again as the thread returns to
    /// its program, having taken every stop and signal on the way: the
    /// kernel rewinds `rip` over the `syscall` and puts the call back in
    /// `rax`, unless the tracer changed the result meanwhile (K-SYS-7).
    fn restart_call(&mut self, tid: Tid) {
        let thread = self.threads.get_mut(&tid).expect("running thread exists");
        if std::mem::take(&mut thread.restarting)
            && thread.orig_rax != NO_SYSTEM_CALL
            && thread.registers.general[RAX] == ERESTARTNOINTR_RESULT
        {
            thread.registers.general[RAX] = thread.orig_rax;
            thread.registers.rip -= SYSCALL_LENGTH;
            thread.restarts += 1;
        }
    }

    /// Stops a running thread for the interrupt waiting for it, with
    /// `PTRACE_EVENT_STOP`, before it takes a signal, one a trap raised
    /// included, or runs (K-INT-1, K-TRAP-2).
    /// Returns whether it stopped.
    fn stop_for_interrupt(&mut self, tid: Tid) -> bool {
        let thread = self.threads.get_mut(&tid).expect("running thread exists");
        if !matches!(
            thread.tracing,
            Tracing::Seized {
                interrupted: true,
                ..
            }
        ) {
            return false;
        }

        let ahead = thread.pending.raised();
        thread.enter_stop(
            StopKind::Event(libc::PTRACE_EVENT_STOP, 0),
            SigInfo::event(tid, libc::PTRACE_EVENT_STOP),
            WaitStatus::Event(tid, libc::PTRACE_EVENT_STOP),
        );

        if ahead {
            self.happenings
                .push(Happening::InterruptAheadOfTrap { tid });
        }

        true
    }

    /// The first byte of `group`'s code that differs from its program's:
    /// its address, the byte, and the program's. A page still shared with
    /// the image was never written.
    #[must_use]
    pub fn planted(&self, group: Tgid) -> Option<Planted> {
        let process = self.processes.get(&group)?;
        process.image.code().find_map(|(page_address, original)| {
            if process
                .space
                .page(page_address)
                .is_some_and(|page| Arc::ptr_eq(page, original))
            {
                return None;
            }

            let current = process.space.peek_bytes(page_address, original.len())?;

            current
                .iter()
                .zip(original.iter())
                .enumerate()
                .find(|(_, (now, was))| now != was)
                .map(|(offset, (&now, &was))| (page_address + offset as u64, now, was))
        })
    }

    /// The process `tid` belongs to.
    #[must_use]
    pub fn process_of(&self, tid: Tid) -> Option<&Process> {
        self.processes.get(&self.threads.get(&tid)?.tgid)
    }

    /// `/proc/<tid>/maps`, or `None` once the thread is reaped. A zombie
    /// has no address space left, and its maps read empty.
    #[must_use]
    pub fn maps(&self, tid: Tid) -> Option<String> {
        let thread = self.threads.get(&tid)?;
        if matches!(thread.state, State::Zombie(_)) {
            return Some(String::new());
        }

        let maps = self.processes.get(&thread.tgid)?.space.maps();
        #[cfg(test)]
        let maps = if self.sabotage == Some(Sabotage::ShiftMappings) {
            shift_file_mappings(&maps)
        } else {
            maps
        };

        Some(maps)
    }

    /// The threads of `group` not yet reaped, as `/proc/<tgid>/task` lists
    /// them.
    pub fn threads_of(&self, group: Tgid) -> impl Iterator<Item = &Thread> + '_ {
        self.threads
            .values()
            .filter(move |thread| thread.tgid == group)
    }
}

/// The debug exceptions an instruction of `tid`, in the process `leader`
/// leads, raises under `sabotage` when its slots raise `hits`.
#[cfg(test)]
fn sabotaged_watch_hits(
    sabotage: Option<Sabotage>,
    repeat_watch: &mut BTreeMap<Tid, u64>,
    (tid, leader): (Tid, Tid),
    outcome: &Outcome,
    hits: u64,
) -> u64 {
    match sabotage {
        Some(Sabotage::MissWatchTraps) if tid != leader => 0,
        Some(Sabotage::RepeatWatchTraps) if *outcome == Outcome::Completed => {
            let repeated = repeat_watch.remove(&tid).unwrap_or(0);
            if hits != 0 {
                repeat_watch.insert(tid, hits);
            }

            hits | repeated
        }
        _ => hits,
    }
}

/// Whether `sabotage` makes the CPU skip the trap at `address`, which is a
/// user breakpoint's or not, with `temporaries` those only temporary
/// breakpoints own.
#[cfg(test)]
fn skips_trap(
    sabotage: Option<Sabotage>,
    temporaries: &BTreeSet<u64>,
    address: u64,
    user_breakpoint: bool,
) -> bool {
    match sabotage {
        Some(Sabotage::SkipTraps) => user_breakpoint,
        Some(Sabotage::SkipTemporaryTraps) => temporaries.contains(&address),
        _ => false,
    }
}

/// `maps` with each file's mappings a page higher than they are, for
/// [`Sabotage::ShiftMappings`].
#[cfg(test)]
fn shift_file_mappings(maps: &str) -> String {
    maps.lines()
        .map(|line| {
            let range = line.split_whitespace().next().unwrap_or_default();
            let bounds = range
                .split_once('-')
                .and_then(|(start, end)| {
                    Some((
                        u64::from_str_radix(start, 16).ok()?,
                        u64::from_str_radix(end, 16).ok()?,
                    ))
                })
                .filter(|_| line.contains('/'));
            match bounds {
                Some((start, end)) => format!(
                    "{:08x}-{:08x}{}\n",
                    start + crate::sim::memory::PAGE_SIZE,
                    end + crate::sim::memory::PAGE_SIZE,
                    &line[range.len()..]
                ),
                None => format!("{line}\n"),
            }
        })
        .collect()
}

/// Executes the program's own instruction at `rip` though a trap covers
/// it, as a broken CPU would, for [`Sabotage::SkipTraps`].
#[cfg(test)]
fn step_under_trap(
    registers: &mut Registers,
    space: &mut AddressSpace,
    image: &Image,
    single_step: bool,
) -> cpu::Executed {
    let address = registers.rip;
    let original = image
        .original_byte(address)
        .expect("a trap in the program's code");
    assert!(space.poke_bytes(address, &[original]));
    let executed = cpu::execute(registers, space, single_step);
    assert!(space.poke_bytes(address, &[0xcc]));
    executed
}

/// What executing one instruction did to the thread's run.
#[derive(PartialEq, Eq)]
enum Executed {
    Continue,
    Stopped,
    Yielded,
}
