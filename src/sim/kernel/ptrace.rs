//! The tracer's ptrace requests. Each answers as Linux does, in terms of
//! the K-* rules; above all K-WAIT-1, that a request to a thread not in a
//! ptrace-stop fails with `ESRCH`.

use nix::errno::Errno;
use nix::libc;

use super::{
    DebugBehavior, Happening, Kernel, Options, SigInfo, State, StopKind, Tgid, Thread, Tid,
    Tracing, WaitStatus,
};

use crate::sim::cpu::{R8, R9, R10, RAX, RDI, RDX, RSI, Registers};

#[cfg(test)]
use crate::sim::world::Sabotage;

/// A ptrace event's name, as `PTRACE_EVENT_*` without the prefix.
#[must_use]
pub fn event_name(event: i32) -> String {
    match event {
        libc::PTRACE_EVENT_FORK => "FORK".into(),
        libc::PTRACE_EVENT_VFORK => "VFORK".into(),
        libc::PTRACE_EVENT_CLONE => "CLONE".into(),
        libc::PTRACE_EVENT_EXEC => "EXEC".into(),
        libc::PTRACE_EVENT_EXIT => "EXIT".into(),
        libc::PTRACE_EVENT_STOP => "STOP".into(),
        other => format!("{other}"),
    }
}

impl Kernel {
    /// The thread a request addresses, which the caller must trace, in a
    /// ptrace-stop.
    fn stopped(&self, tid: Tid) -> Result<&Thread, Errno> {
        let caller = self.caller();
        self.threads
            .get(&tid)
            .filter(|thread| thread.tracer() == Some(caller) && thread.is_stopped())
            .ok_or(Errno::ESRCH)
    }

    fn stopped_mut(&mut self, tid: Tid) -> Result<&mut Thread, Errno> {
        let caller = self.caller();
        self.threads
            .get_mut(&tid)
            .filter(|thread| thread.tracer() == Some(caller) && thread.is_stopped())
            .ok_or(Errno::ESRCH)
    }

    /// `PTRACE_SETREGS`.
    pub fn set_registers(&mut self, tid: Tid, registers: Registers) -> Result<(), Errno> {
        #[cfg(test)]
        let registers = {
            // A value the sabotage reported writes back what the register
            // held, so the program runs on unchanged.
            let held = self.stopped(tid)?.registers;
            let reported = self.reported(held);
            let mut registers = registers;
            for (index, value) in registers.general.iter_mut().enumerate() {
                if *value == reported.general[index] {
                    *value = held.general[index];
                }
            }

            registers
        };

        // K-SYS-7: Linux decides whether to restart a call from `rax` and
        // `rip` as the thread returns, and no probe pins a tracer's change
        // to either before then.
        let thread = self.stopped(tid)?;
        if thread.restarting
            && (registers.rip != thread.registers.rip
                || registers.general[RAX] != thread.registers.general[RAX])
        {
            self.gap("writing rax or rip of a call about to restart");
            return Err(Errno::ENOSYS);
        }

        self.stopped_mut(tid)?.registers = registers;
        Ok(())
    }

    /// Registers as the kernel reports them: under
    /// [`Sabotage::SkewSmallRegisters`],
    /// general registers other than the stack and frame pointers that hold
    /// small numbers other than zero read one greater.
    #[cfg(test)]
    fn reported(&self, mut registers: Registers) -> Registers {
        if self.sabotage == Some(Sabotage::SkewSmallRegisters) {
            for (index, value) in registers.general.iter_mut().enumerate() {
                if index != crate::sim::cpu::RSP
                    && index != crate::sim::cpu::RBP
                    && (1..0x1000).contains(value)
                {
                    *value += 1;
                }
            }
        }

        registers
    }

    /// `PTRACE_PEEKUSER` of a debug register (K-DR-3, K-DR-4).
    pub fn peek_debug(&self, tid: Tid, index: usize) -> Result<u64, Errno> {
        let thread = self.stopped(tid)?;
        #[cfg(test)]
        if let Some(phantom) = self.phantom_debug.get(&tid) {
            return Ok(phantom.peek(index));
        }

        match self.debug_behavior {
            DebugBehavior::Discarding => Ok(0),
            _ => Ok(thread.debug.peek(index)),
        }
    }

    /// `PTRACE_POKEUSER` of a debug register (K-DR-4).
    pub fn poke_debug(&mut self, tid: Tid, index: usize, value: u64) -> Result<(), Errno> {
        let contended = match self.debug_behavior {
            DebugBehavior::Discarding => {
                self.stopped(tid)?;
                return Ok(());
            }
            DebugBehavior::Faithful => 0,
            DebugBehavior::Contended(others) => others,
        };

        #[cfg(test)]
        let phantom = self.sabotage == Some(Sabotage::PhantomArming);

        let thread = self.stopped_mut(tid)?;
        let contended = if thread.tid == thread.tgid.leader() {
            0
        } else {
            contended
        };

        let others = contended + thread.debug_held;
        #[cfg(test)]
        if phantom && thread.tid != thread.tgid.leader() {
            let registers = thread.debug.clone();
            return self
                .phantom_debug
                .entry(tid)
                .or_insert(registers)
                .poke(index, value, others);
        }

        thread.debug.poke(index, value, others)
    }

    /// Lets something other than the tracer hold `count` more of a
    /// thread's hardware breakpoints, as perf can.
    #[cfg(test)]
    pub fn hold_debug_slots(&mut self, tid: Tid, count: usize) {
        if let Some(thread) = self.threads.get_mut(&tid) {
            thread.debug_held += count;
        }
    }

    /// `PTRACE_PEEKDATA`, which ignores protections and fails with `EIO`
    /// where nothing is mapped (K-MEM-1).
    pub fn peek(&self, tid: Tid, address: u64) -> Result<u64, Errno> {
        let thread = self.stopped(tid)?;
        let word = self.processes[&thread.tgid]
            .space
            .peek(address)
            .ok_or(Errno::EIO)?;
        #[cfg(test)]
        let word = self.sabotage_read(thread.tgid, address, word);

        Ok(word)
    }

    /// `PTRACE_POKEDATA` (K-MEM-1).
    pub fn poke(&mut self, tid: Tid, address: u64, value: u64) -> Result<(), Errno> {
        let group = self.stopped(tid)?.tgid;
        #[cfg(test)]
        if self.sabotage == Some(Sabotage::LosePokes) {
            return Ok(());
        }

        // A byte a sabotaged read showed falsely, written back as shown, is
        // written as the byte it hid: the lie is one only reads can see.
        #[cfg(test)]
        let value = self.processes[&group]
            .space
            .peek(address)
            .map_or(value, |word| {
                let shown = self.sabotage_read(group, address, word);
                let hidden = (shown ^ word) & !(shown ^ value);
                value & !hidden | word & hidden
            });

        let space = &mut self
            .processes
            .get_mut(&group)
            .expect("a thread's process exists")
            .space;

        if space.poke(address, value) {
            Ok(())
        } else {
            Err(Errno::EIO)
        }
    }

    /// `PTRACE_GETSIGINFO`.
    pub fn signal_info(&self, tid: Tid) -> Result<SigInfo, Errno> {
        match self.stopped(tid)?.state {
            State::Stopped { info, .. } => Ok(info),
            _ => unreachable!("a stopped thread has stop information"),
        }
    }

    /// `PTRACE_GETREGS`, with `orig_rax`.
    pub fn get_registers_and_call(&self, tid: Tid) -> Result<(Registers, u64), Errno> {
        let thread = self.stopped(tid)?;
        let registers = thread.registers;
        #[cfg(test)]
        let registers = self.reported(registers);

        Ok((registers, thread.orig_rax))
    }

    /// `PTRACE_GETEVENTMSG`: the message of the latest event stop, or zero.
    /// At a system-call-stop it says which (K-SYS-1).
    pub fn event_message(&self, tid: Tid) -> Result<u64, Errno> {
        match self.stopped(tid)?.state {
            State::Stopped {
                kind: StopKind::Exit(exit),
                ..
            } => Ok(exit.event_message()),
            State::Stopped {
                kind: StopKind::Event(_, message),
                ..
            } => Ok(message),
            State::Stopped {
                kind: StopKind::Syscall { exit },
                ..
            } => Ok(if exit { 2 } else { 1 }),
            _ => Ok(0),
        }
    }

    /// Whether `tid` finished exiting: it is a zombie, or reaped.
    #[must_use]
    pub fn finished_exiting(&self, tid: Tid) -> bool {
        self.threads
            .get(&tid)
            .is_none_or(|thread| matches!(thread.state, State::Zombie(_)))
    }

    /// The thread group `/proc/<tid>/status` names, until the thread is
    /// reaped.
    #[must_use]
    pub fn thread_group(&self, tid: Tid) -> Option<Tgid> {
        self.threads.get(&tid).map(|thread| thread.tgid)
    }

    /// `PTRACE_SETOPTIONS`.
    pub fn set_options(&mut self, tid: Tid, options: Options) -> Result<(), Errno> {
        self.stopped_mut(tid)?.options = options;
        Ok(())
    }

    /// `PTRACE_CONT` with `signal`, delivered only from a
    /// signal-delivery-stop. A thread at an event stop returns from the
    /// system call it stopped in when it next runs; one at its exit event
    /// goes on to exit. Resuming a thread from a group-stop while its
    /// process is still stopped is not modeled.
    pub fn resume(
        &mut self,
        tid: Tid,
        signal: Option<i32>,
        single_step: bool,
    ) -> Result<(), Errno> {
        let thread = self.stopped_mut(tid)?;
        thread.single_step = single_step;
        thread.syscall_stops = false;
        thread.resumed = true;

        match thread.state {
            State::Stopped {
                kind: StopKind::Syscall { .. },
                ..
            } => {
                // K-SYS-5: a signal passed here is sent to the process.
                // SIGKILL is the only one modeled.
                if signal.is_some_and(|signal| signal != libc::SIGKILL) {
                    self.gap("a signal other than SIGKILL passed at a system-call-stop");
                    return Err(Errno::ENOSYS);
                }

                thread.state = State::Running;
                thread.report = None;
                let (registers, group) = (thread.registers, thread.tgid);

                // Resuming a call a catchpoint stops, though no stop
                // reported it, is the debugger's mistake; killing it is not.
                if let Some(caught) = self.catching.unreported.remove(&tid)
                    && signal.is_none()
                    && self.catching.stops(caught.call, &registers, caught.exit)
                {
                    self.catching.skipped.push(caught);
                }

                if signal.is_some() {
                    self.kill_process(group, super::ExitStatus::Signal(libc::SIGKILL, false));
                }
            }

            State::Stopped {
                kind: StopKind::Signal(_),
                ..
            } => self.resume_with(tid, signal),
            State::Stopped {
                kind: StopKind::Event(..),
                ..
            } => {
                thread.state = State::Running;
                thread.report = None;
            }

            State::Stopped {
                kind: StopKind::Group(_),
                ..
            } => {
                let group = thread.tgid;
                if self.processes[&group].stopped.is_some() {
                    self.gap("resuming a thread of a stopped process");
                    return Err(Errno::ENOSYS);
                }

                let thread = self.threads.get_mut(&tid).expect("the thread");
                thread.state = State::Running;
                thread.report = None;
            }

            State::Stopped {
                kind: StopKind::Exit(exit),
                ..
            } => self.become_zombie(tid, exit),
            _ => unreachable!("a resumed thread was stopped"),
        }

        #[cfg(test)]
        if self.sabotage == Some(Sabotage::ResumeSiblings) && !single_step {
            self.resume_siblings(tid);
        }

        Ok(())
    }

    /// Resumes each sibling of `tid` stopped with a signal or an event,
    /// under [`Sabotage::ResumeSiblings`].
    #[cfg(test)]
    fn resume_siblings(&mut self, tid: Tid) {
        let group = self.threads[&tid].tgid;
        let siblings = self
            .threads
            .iter()
            .filter(|&(&other, thread)| {
                other != tid
                    && thread.tgid == group
                    && matches!(
                        thread.state,
                        State::Stopped {
                            kind: StopKind::Signal(_) | StopKind::Event(..),
                            ..
                        }
                    )
            })
            .map(|(&other, _)| other)
            .collect::<Vec<_>>();
        self.sabotage = None;
        for sibling in siblings {
            let _ = self.resume(sibling, None, false);
        }

        self.sabotage = Some(Sabotage::ResumeSiblings);
    }

    /// `PTRACE_SYSCALL`: resumes a stopped thread as `PTRACE_CONT` does,
    /// to stop next at a system call's entry or exit (K-SYS-1, K-SYS-2).
    /// Resuming with a signal from a system-call-stop is not modeled.
    pub fn resume_to_call(&mut self, tid: Tid, signal: Option<i32>) -> Result<(), Errno> {
        self.resume(tid, signal, false)?;
        self.threads
            .get_mut(&tid)
            .expect("the resumed thread")
            .syscall_stops = true;
        Ok(())
    }

    /// `PTRACE_GET_SYSCALL_INFO` (K-SYS-1, K-SYS-2, K-SYS-4): the call a
    /// thread stopped at the entry or exit of, or only the architecture at
    /// any other stop.
    pub fn call_info(&self, tid: Tid) -> Result<CallInfo, Errno> {
        let thread = self.stopped(tid)?;
        let State::Stopped { kind, .. } = thread.state else {
            unreachable!("a stopped thread has stop information");
        };

        let general = &thread.registers.general;
        let args = [
            general[RDI],
            general[RSI],
            general[RDX],
            general[R10],
            general[R8],
            general[R9],
        ];

        #[cfg(test)]
        let args = if self.sabotage == Some(Sabotage::MisreportCallArguments) {
            let mut args = args;
            args[1] = args[1].wrapping_add(1);
            args
        } else {
            args
        };

        #[cfg(test)]
        let args = if self.sabotage == Some(Sabotage::MisreportFirstCallArgument) {
            let mut args = args;
            args[0] = args[0].wrapping_add(1);
            args
        } else {
            args
        };

        Ok(match kind {
            StopKind::Syscall { exit: false } => CallInfo {
                op: 1,
                number: thread.orig_rax,
                args,
                value: 0,
            },
            StopKind::Syscall { exit: true } => CallInfo {
                op: 2,
                number: 0,
                args: [0; 6],
                value: general[RAX].cast_signed(),
            },
            _ => CallInfo::default(),
        })
    }

    /// `PTRACE_SEIZE` with `options`: the caller traces the thread, which
    /// runs on (K-SEIZE-1), or, in a job-control stop, reports a group-stop
    /// (K-STOP-2). A thread already traced, or one that has finished
    /// exiting, cannot be seized (K-EXIT-5).
    pub fn seize(&mut self, tid: Tid, options: Options) -> Result<(), Errno> {
        let tracer = self.caller();
        let thread = self.threads.get_mut(&tid).ok_or(Errno::ESRCH)?;
        if thread.traced() {
            return Err(Errno::EPERM);
        }

        if matches!(thread.state, State::Zombie(_)) {
            self.happenings.push(Happening::SeizeRefused { tid });
            return Err(Errno::EPERM);
        }

        thread.tracing = Tracing::Seized {
            tracer,
            interrupted: false,
        };

        thread.options = options;

        if thread.state == State::JobStopped {
            let signal = self.processes[&thread.tgid]
                .stopped
                .expect("a stopped thread's process is stopped");
            thread.enter_stop(
                StopKind::Group(signal),
                SigInfo::group_stop(tid, signal),
                WaitStatus::GroupStop(tid, signal),
            );
        }

        Ok(())
    }

    /// `PTRACE_INTERRUPT` (K-INT-1, K-INT-2): a running seized thread stops
    /// before it runs on, and a stopped one as soon as it resumes. One
    /// exiting, at its exit event or a zombie, stays as it is.
    pub fn interrupt(&mut self, tid: Tid) -> Result<(), Errno> {
        let caller = self.caller();
        let thread = self
            .threads
            .get_mut(&tid)
            .filter(|thread| thread.tracer() == Some(caller))
            .ok_or(Errno::ESRCH)?;
        if !thread.seized() {
            return Err(Errno::EIO);
        }

        if !thread.state.exiting() {
            thread.tracing = Tracing::Seized {
                tracer: caller,
                interrupted: true,
            };

            if thread.is_stopped() {
                self.happenings.push(Happening::InterruptWaits { tid });
            }
        }

        Ok(())
    }

    /// `PTRACE_DETACH` with `signal`, delivered only from a
    /// signal-delivery-stop. The thread runs on untraced from where it
    /// stopped, its debug registers as they were (K-FORK-2, K-DR-3), or,
    /// its process stopped, stops again (K-STOP-2).
    pub fn detach(&mut self, tid: Tid, signal: Option<i32>) -> Result<(), Errno> {
        let tracer = self.caller();
        let thread = self.stopped_mut(tid)?;
        thread.tracing = Tracing::Untraced;
        thread.single_step = false;

        // K-SYS-6: an untraced thread stops at no call.
        thread.syscall_stops = false;
        thread.options = Options::default();
        thread.report = None;
        let state = thread.state;
        let group = thread.tgid;

        // Only a thread that runs on can meet a trap left in its code.
        let exiting = matches!(
            state,
            State::Stopped {
                kind: StopKind::Exit(_),
                ..
            }
        );
        let planted = if exiting { None } else { self.planted(group) };
        self.happenings.push(Happening::Released {
            tid,
            tracer,
            planted,
        });

        match state {
            State::Stopped {
                kind: StopKind::Signal(_),
                ..
            } => {
                self.threads.get_mut(&tid).expect("the thread").state = State::Running;
                #[cfg(test)]
                let signal = if self.sabotage == Some(Sabotage::PassTrapsOnDetach)
                    && matches!(
                        state,
                        State::Stopped {
                            kind: StopKind::Signal(libc::SIGTRAP),
                            ..
                        }
                    ) {
                    Some(libc::SIGTRAP)
                } else {
                    signal
                };

                if let Some(signal) = signal {
                    self.act_by_default(tid, signal);
                }
            }

            State::Stopped {
                kind: StopKind::Event(..) | StopKind::Group(_),
                ..
            } => self.threads.get_mut(&tid).expect("the thread").state = State::Running,
            // K-SYS-6: the call goes on, made or returning, untraced.
            State::Stopped {
                kind: StopKind::Syscall { .. },
                ..
            } => {
                if signal.is_some() {
                    self.gap("a signal passed detaching at a system-call-stop");
                    return Err(Errno::ENOSYS);
                }

                let thread = self.threads.get_mut(&tid).expect("the thread");
                thread.state = State::Running;
                thread.syscall_stops = false;
            }

            State::Stopped {
                kind: StopKind::Exit(exit),
                ..
            } => self.become_zombie(tid, exit),
            _ => unreachable!("a detached thread was stopped"),
        }

        let stopped = self
            .processes
            .get(&group)
            .is_some_and(|process| process.stopped.is_some());

        if let Some(thread) = self.threads.get_mut(&tid)
            && thread.state == State::Running
            && stopped
        {
            thread.state = State::JobStopped;
        }

        Ok(())
    }

    /// Whether `signal` is pending in a stopped thread's own set.
    #[must_use]
    pub fn signal_queued(&self, tid: Tid, signal: i32) -> bool {
        self.threads
            .get(&tid)
            .is_some_and(|thread| thread.pending.contains(signal))
    }
}

/// What `PTRACE_GET_SYSCALL_INFO` reports, for x86-64 alone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallInfo {
    /// 1 at an entry, 2 at an exit, 0 at any other stop.
    pub op: u8,

    /// The call, at an entry.
    pub number: u64,

    /// Its arguments, at an entry.
    pub args: [u64; 6],

    /// What it returned, at an exit.
    pub value: i64,
}
