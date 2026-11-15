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

pub type Tid = i32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tgid(Tid);

impl Tgid {
    #[must_use]
    pub const fn new(id: Tid) -> Self {
        Self(id)
    }

    #[must_use]
    pub const fn id(self) -> Tid {
        self.0
    }

    #[must_use]
    pub const fn led_by(leader: Tid) -> Self {
        Self(leader)
    }

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

const FIRST_TID: Tid = 1000;

const UID: u32 = 1000;

pub const NO_SYSTEM_CALL: u64 = u64::MAX;

const ERESTARTNOINTR_RESULT: u64 = (-513_i64).cast_unsigned();

const SYSCALL_LENGTH: u64 = 2;

const ENOSYS_RESULT: u64 = (-(libc::ENOSYS as i64)).cast_unsigned();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitStatus {
    Exited(Tid, i32),

    Signaled(Tid, i32, bool),

    Stopped(Tid, i32),

    Event(Tid, i32),

    GroupStop(Tid, i32),

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SigInfo {
    pub signal: i32,
    pub code: i32,

    pub pid: i32,

    pub address: u64,
}

impl SigInfo {
    const fn event(tid: Tid, event: i32) -> Self {
        Self {
            signal: SIGTRAP,
            code: SIGTRAP | (event << 8),
            pid: tid,

            address: (UID as u64) << 32 | tid.cast_unsigned() as u64,
        }
    }

    const fn syscall(tid: Tid) -> Self {
        Self {
            code: SIGTRAP | 0x80,
            ..Self::event(tid, 0)
        }
    }

    const fn group_stop(tid: Tid, signal: i32) -> Self {
        Self {
            signal,
            code: signal | (libc::PTRACE_EVENT_STOP << 8),
            ..Self::event(tid, libc::PTRACE_EVENT_STOP)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    Code(i32),

    Signal(i32, bool),
}

impl ExitStatus {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopKind {
    Signal(i32),

    Event(i32, u64),

    Exit(ExitStatus),

    Group(i32),

    Syscall { exit: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,

    Stopped { kind: StopKind, info: SigInfo },

    JobStopped,

    Exiting(ExitStatus),

    Zombie(ExitStatus),
}

impl State {
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

pub type Planted = (u64, u8, u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tracing {
    Untraced,

    Attached { tracer: i32 },

    Seized { tracer: i32, interrupted: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallPhase {
    Outside,

    AtEntry,

    Returning(u64),

    Restarting,
}

pub struct Thread {
    pub tid: Tid,
    pub tgid: Tgid,
    pub tracing: Tracing,
    pub registers: Registers,

    pub orig_rax: u64,

    pub call: CallPhase,
    pub state: State,
    pub options: Options,

    pub pending: Pending,

    pub single_step: bool,

    pub syscall_stops: bool,

    pub restarts: u64,

    pub resumed: bool,

    pub report: Option<WaitStatus>,

    pub trapped_at: Option<u64>,

    pub last_trap: Option<(u64, u64)>,

    pub retired: u64,

    pub shadow: Shadow,
    pub debug: DebugRegisters,

    pub debug_held: usize,
}

impl Thread {
    #[must_use]
    pub const fn is_stopped(&self) -> bool {
        matches!(self.state, State::Stopped { .. })
    }

    #[must_use]
    pub const fn traced(&self) -> bool {
        !matches!(self.tracing, Tracing::Untraced)
    }

    #[must_use]
    pub const fn tracer(&self) -> Option<i32> {
        match self.tracing {
            Tracing::Untraced => None,
            Tracing::Attached { tracer } | Tracing::Seized { tracer, .. } => Some(tracer),
        }
    }

    #[must_use]
    pub const fn seized(&self) -> bool {
        matches!(self.tracing, Tracing::Seized { .. })
    }

    #[must_use]
    pub const fn held(&self) -> bool {
        matches!(
            self.state,
            State::Stopped { kind, .. } if !matches!(kind, StopKind::Exit(_))
        )
    }

    pub(super) const fn enter_stop(&mut self, kind: StopKind, info: SigInfo, report: WaitStatus) {
        self.state = State::Stopped { kind, info };
        self.report = Some(report);
        if let Tracing::Seized { interrupted, .. } = &mut self.tracing {
            *interrupted = false;
        }
    }

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
            call: CallPhase::Outside,
            state: State::Running,
            options: if traced {
                creator.options
            } else {
                Options::default()
            },
            pending,
            single_step: false,

            syscall_stops: false,
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

    #[must_use]
    pub fn in_user_code(&self) -> bool {
        self.state == State::Running && self.call == CallPhase::Outside && !self.pending.raised()
    }

    #[must_use]
    pub const fn can_run(&self) -> bool {
        matches!(self.state, State::Running | State::Exiting(_))
    }
}

pub struct Process {
    pub tgid: Tgid,

    pub name: Arc<str>,
    pub space: AddressSpace,
    pub image: Arc<Image>,

    pub parent: Parent,

    pub creator: Option<Tid>,

    pub root: Tid,

    pub shared: Pending,

    pub group_exit: Option<ExitStatus>,

    pub killed_externally: bool,

    pub stopped: Option<i32>,

    pub continued: bool,

    pub start_time: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parent {
    Tracer,

    Launcher,

    Process(Tgid),

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reaped {
    pub tgid: Tgid,
    pub status: WaitStatus,

    pub group_exit: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    pub status: ExitStatus,

    pub root: Tid,
    pub reaper: Parent,

    pub killed_externally: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zombie {
    pub parent: Tgid,
    pub creator: Option<Tid>,
    pub status: ExitStatus,
    pub root: Tid,
    pub killed_externally: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelGap(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Happening {
    /// A thread created another.
    Cloned { parent: Tid, child: Tid },

    /// A thread called `exit_group`.
    GroupExit { tid: Tid },

    /// A group exit or SIGKILL took a thread out of a ptrace-stop.
    PulledFromStop { tid: Tid },

    /// A thread's fork or clone failed for a signal pending, to be made
    Restarting { tid: Tid },

    /// (K-TRAP-2).
    InterruptAheadOfTrap { tid: Tid },

    /// A group leader exited while other threads ran on.
    LeaderExitedAlone { tid: Tid },

    /// A thread yielded the CPU.
    Yielded { tid: Tid },

    /// A thread forked a process.
    Forked { parent: Tid, child: Tid },

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

    /// as soon as it resumes.
    InterruptWaits { tid: Tid },

    /// An untraced thread entered its process's job-control stop.
    JobStopped { tid: Tid },

    /// SIGCONT ended a process's job-control stop.
    Continued { tgid: Tgid },

    /// its signal.
    Suppressed { tid: Tid, signal: i32 },

    /// and the thread took its default action.
    Delivered { tid: Tid, signal: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnseenHit {
    pub tid: Tid,
    pub address: u64,
}

pub struct Kernel {
    tracer: i32,

    caller: std::cell::Cell<i32>,
    next_tid: Tid,
    pub processes: BTreeMap<Tgid, Process>,
    pub threads: BTreeMap<Tid, Thread>,

    pub ended: BTreeMap<Tgid, Ended>,

    pub zombies: BTreeMap<Tgid, Zombie>,

    pub outputs: BTreeMap<Tid, Vec<u8>>,

    pub reaped: BTreeMap<Tid, Reaped>,

    pub leader_exits: BTreeMap<Tid, i32>,

    pub gap: Option<ModelGap>,

    pub happenings: Vec<Happening>,

    pub debugged: Option<Tgid>,

    pub user_breakpoints: BTreeSet<u64>,

    #[cfg(test)]
    pub temporary_breakpoints: BTreeSet<u64>,

    pub unseen_hits: Vec<UnseenHit>,

    pub tracking: Option<Tracking>,

    pub watching: Watching,

    pub catching: Catching,

    pub served: Vec<syscalls::Served>,

    pub debug_behavior: DebugBehavior,

    pub perf: Option<perf::Breakpoint>,

    next_activation: u64,

    next_start_time: u64,

    #[cfg(test)]
    pub sabotage: Option<Sabotage>,

    #[cfg(test)]
    late_step: Option<Tid>,

    #[cfg(test)]
    repeat_watch: BTreeMap<Tid, u64>,

    #[cfg(test)]
    flickers: std::cell::Cell<u64>,

    #[cfg(test)]
    pub(super) phantom_debug: BTreeMap<Tid, debug_regs::DebugRegisters>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugBehavior {
    Faithful,

    Discarding,

    Contended(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ran {
    pub executed: u64,

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

    pub fn serve(&self, tracer: i32) {
        self.caller.set(tracer);
    }

    #[must_use]
    pub const fn caller(&self) -> i32 {
        self.caller.get()
    }

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

    pub fn spawn(
        &mut self,
        image: Arc<Image>,
        name: &str,
        arguments: &[String],
        random: [u8; 16],
    ) -> Tid {
        self.start(image, name, arguments, random, true)
    }

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

                orig_rax: syscalls::SYS_EXECVE,
                syscall_stops: false,
                call: CallPhase::Outside,
                restarts: 0,
                resumed: false,
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

    pub fn runnable(&self) -> impl Iterator<Item = Tid> + '_ {
        self.threads
            .values()
            .filter(|thread| thread.can_run())
            .map(|thread| thread.tid)
    }

    pub fn reportable(&self, tracer: i32) -> impl Iterator<Item = Tid> + '_ {
        self.threads
            .values()
            .filter(move |thread| {
                thread.tracer() == Some(tracer) && thread.report.is_some() && !self.delayed(thread)
            })
            .map(|thread| thread.tid)
    }

    fn delayed(&self, thread: &Thread) -> bool {
        thread.tid == thread.tgid.leader()
            && matches!(thread.state, State::Zombie(_))
            && self
                .threads
                .values()
                .any(|other| other.tgid == thread.tgid && other.tid != thread.tid)
    }

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

            if let CallPhase::Returning(result) = thread.call {
                thread.call = CallPhase::Outside;
                thread.registers.general[RAX] = result;
                if thread.single_step {
                    self.trap(tid, signals::TRAP_BRKPT);
                    self.note_return(tid);
                    break;
                }

                if thread.syscall_stops {
                    self.syscall_stop(tid, true);
                    break;
                }

                self.note_return(tid);
                continue;
            }

            if thread.call == CallPhase::AtEntry {
                thread.call = CallPhase::Outside;
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

    fn execute(&mut self, tid: Tid) -> Executed {
        self.sample_breakpoint(tid);
        let thread = &self.threads[&tid];

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
            thread.call = CallPhase::AtEntry;
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

            return if executed == Executed::Yielded {
                executed
            } else {
                Executed::Stopped
            };
        }

        self.note_return(tid);
        executed
    }

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
        let returned =
            !matches!(thread.call, CallPhase::Returning(_)) && !inside && !thread.state.exiting();
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

    fn serve_syscall(&mut self, tid: Tid, single_step: bool) -> Executed {
        let yielded = syscalls::serve(self, tid);

        if !self
            .threads
            .get(&tid)
            .is_some_and(|thread| matches!(thread.state, State::Running))
        {
            return Executed::Stopped;
        }

        if single_step {
            if yielded {
                self.happenings.push(Happening::Yielded { tid });
            }

            self.trap(tid, signals::TRAP_BRKPT);
            return Executed::Stopped;
        }

        if yielded {
            Executed::Yielded
        } else {
            Executed::Continue
        }
    }

    fn take_exception(&mut self, tid: Tid, address: u64, outcome: Outcome) -> Executed {
        match outcome {
            Outcome::Breakpoint => {
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

    fn complete(&mut self, tid: Tid, single_step: bool, watch_hits: u64) -> Executed {
        if single_step {
            #[cfg(test)]
            if watch_hits == 0 && self.step_late(tid) {
                return Executed::Continue;
            }

            self.threads
                .get_mut(&tid)
                .expect("the stepping thread exists")
                .debug
                .debug_exception(true, watch_hits);

            self.trap(tid, signals::TRAP_TRACE);
            return Executed::Stopped;
        }

        if watch_hits != 0 {
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

    #[cfg(test)]
    fn hide_stepped_call(&self, tgid: Tgid, address: u64, word: u64) -> u64 {
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

    fn raise(&mut self, tid: Tid, info: SigInfo) {
        self.threads
            .get_mut(&tid)
            .expect("the raising thread exists")
            .pending
            .insert(info);
    }

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

    fn event_stop(&mut self, tid: Tid, event: i32, message: u64, result: u64) {
        let thread = self.threads.get_mut(&tid).expect("stopping thread exists");
        thread.registers.general[RAX] = ENOSYS_RESULT;
        thread.call = CallPhase::Returning(result);
        #[cfg(test)]
        if self.sabotage == Some(Sabotage::ReturnBeforeEvents) {
            thread.registers.general[RAX] = result;
            thread.call = CallPhase::Outside;
        }

        thread.enter_stop(
            StopKind::Event(event, message),
            SigInfo::event(tid, event),
            WaitStatus::Event(tid, event),
        );
    }

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

    fn fail_to_restart(&mut self, tid: Tid) {
        let thread = self.threads.get_mut(&tid).expect("the calling thread");
        thread.registers.general[RAX] = ERESTARTNOINTR_RESULT;
        thread.call = CallPhase::Restarting;
        self.happenings.push(Happening::Restarting { tid });
    }

    fn restart_call(&mut self, tid: Tid) {
        let thread = self.threads.get_mut(&tid).expect("running thread exists");
        if thread.call != CallPhase::Restarting {
            return;
        }

        thread.call = CallPhase::Outside;
        if thread.orig_rax != NO_SYSTEM_CALL
            && thread.registers.general[RAX] == ERESTARTNOINTR_RESULT
        {
            thread.registers.general[RAX] = thread.orig_rax;
            thread.registers.rip -= SYSCALL_LENGTH;
            thread.restarts += 1;
        }
    }

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

    #[must_use]
    pub fn process_of(&self, tid: Tid) -> Option<&Process> {
        self.processes.get(&self.threads.get(&tid)?.tgid)
    }

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

    pub fn threads_of(&self, group: Tgid) -> impl Iterator<Item = &Thread> + '_ {
        self.threads
            .values()
            .filter(move |thread| thread.tgid == group)
    }
}

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

#[derive(PartialEq, Eq)]
enum Executed {
    Continue,
    Stopped,
    Yielded,
}
