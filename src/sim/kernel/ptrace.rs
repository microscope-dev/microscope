//! Simulated `ptrace` operations (register, memory, debug-register, seize,
//! interrupt, resume, and detach) enforcing Linux's tracer-ownership and
//! ptrace-stop preconditions.

use nix::errno::Errno;
use nix::libc;

use super::{
    CallPhase, DebugBehavior, Happening, Kernel, Options, SigInfo, State, StopKind, Tgid, Thread,
    Tid, Tracing, WaitStatus,
};

use crate::sim::cpu::{R8, R9, R10, RAX, RDI, RDX, RSI, Registers};

#[cfg(test)]
use crate::sim::world::Sabotage;

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

    pub fn set_registers(&mut self, tid: Tid, registers: Registers) -> Result<(), Errno> {
        #[cfg(test)]
        let registers = {
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

        let thread = self.stopped(tid)?;
        if thread.call == CallPhase::Restarting
            && (registers.rip != thread.registers.rip
                || registers.general[RAX] != thread.registers.general[RAX])
        {
            self.gap("writing rax or rip of a call about to restart");
            return Err(Errno::ENOSYS);
        }

        self.stopped_mut(tid)?.registers = registers;
        Ok(())
    }

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

    #[cfg(test)]
    pub fn hold_debug_slots(&mut self, tid: Tid, count: usize) {
        if let Some(thread) = self.threads.get_mut(&tid) {
            thread.debug_held += count;
        }
    }

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

    pub fn poke(&mut self, tid: Tid, address: u64, value: u64) -> Result<(), Errno> {
        let group = self.stopped(tid)?.tgid;
        #[cfg(test)]
        if self.sabotage == Some(Sabotage::LosePokes) {
            return Ok(());
        }

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

    pub fn signal_info(&self, tid: Tid) -> Result<SigInfo, Errno> {
        match self.stopped(tid)?.state {
            State::Stopped { info, .. } => Ok(info),
            _ => unreachable!("a stopped thread has stop information"),
        }
    }

    pub fn get_registers_and_call(&self, tid: Tid) -> Result<(Registers, u64), Errno> {
        let thread = self.stopped(tid)?;
        let registers = thread.registers;
        #[cfg(test)]
        let registers = self.reported(registers);

        Ok((registers, thread.orig_rax))
    }

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

    #[must_use]
    pub fn finished_exiting(&self, tid: Tid) -> bool {
        self.threads
            .get(&tid)
            .is_none_or(|thread| matches!(thread.state, State::Zombie(_)))
    }

    #[must_use]
    pub fn thread_group(&self, tid: Tid) -> Option<Tgid> {
        self.threads.get(&tid).map(|thread| thread.tgid)
    }

    pub fn set_options(&mut self, tid: Tid, options: Options) -> Result<(), Errno> {
        self.stopped_mut(tid)?.options = options;
        Ok(())
    }

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
                if signal.is_some_and(|signal| signal != libc::SIGKILL) {
                    self.gap("a signal other than SIGKILL passed at a system-call-stop");
                    return Err(Errno::ENOSYS);
                }

                thread.state = State::Running;
                thread.report = None;
                let (registers, group) = (thread.registers, thread.tgid);

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

    pub fn resume_to_call(&mut self, tid: Tid, signal: Option<i32>) -> Result<(), Errno> {
        self.resume(tid, signal, false)?;
        self.threads
            .get_mut(&tid)
            .expect("the resumed thread")
            .syscall_stops = true;
        Ok(())
    }

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

        #[cfg(test)]
        if self.sabotage == Some(Sabotage::LoseStopRequests) {
            return Ok(());
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

    pub fn detach(&mut self, tid: Tid, signal: Option<i32>) -> Result<(), Errno> {
        let tracer = self.caller();
        let thread = self.stopped_mut(tid)?;
        thread.tracing = Tracing::Untraced;
        thread.single_step = false;

        thread.syscall_stops = false;
        thread.options = Options::default();
        thread.report = None;
        let state = thread.state;
        let group = thread.tgid;

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

    #[must_use]
    pub fn signal_queued(&self, tid: Tid, signal: i32) -> bool {
        self.threads
            .get(&tid)
            .is_some_and(|thread| thread.pending.contains(signal))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallInfo {
    pub op: u8,

    pub number: u64,

    pub args: [u64; 6],

    pub value: i64,
}
