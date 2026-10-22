//! Signals: what they do by default, how they are delivered, and how
//! threads exit.

use nix::errno::Errno;
use nix::libc;

use super::{
    ExitStatus, Happening, Kernel, Parent, SigInfo, State, StopKind, Tgid, Thread, Tid, Tracing,
    WaitStatus,
};

pub const SIGTRAP: i32 = libc::SIGTRAP;

pub const SIGKILL: i32 = libc::SIGKILL;

pub const SIGSTOP: i32 = libc::SIGSTOP;

pub const SIGCONT: i32 = libc::SIGCONT;

/// `si_code` of a signal sent by `kill`.
pub const SI_USER: i32 = libc::SI_USER;

/// `si_code` of a signal sent by `tgkill`.
pub const SI_TKILL: i32 = libc::SI_TKILL;

/// `si_code` of a breakpoint trap.
pub const TRAP_BRKPT: i32 = 1;

/// `si_code` of a single-step trap.
pub const TRAP_TRACE: i32 = 2;

/// `si_code` of a hardware breakpoint's trap.
pub const TRAP_HWBKPT: i32 = 4;

/// Signals the kernel delivers before any other pending signal, because an
/// instruction raised them.
const SYNCHRONOUS: [i32; 6] = [
    libc::SIGSEGV,
    libc::SIGBUS,
    libc::SIGILL,
    SIGTRAP,
    libc::SIGFPE,
    libc::SIGSYS,
];

/// The standard signals, 1 to 31, pending for one thread, each with the
/// siginfo it will be delivered with. A standard signal already pending is
/// not queued again: the first one's siginfo stays.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Pending([Option<SigInfo>; 31]);

impl Pending {
    /// Makes `info.signal` pending, unless it already is.
    pub const fn insert(&mut self, info: SigInfo) {
        let slot = &mut self.0[(info.signal - 1).cast_unsigned() as usize];
        if slot.is_none() {
            *slot = Some(info);
        }
    }

    /// Whether a signal an instruction raised is pending: only a trap or
    /// fault queues a synchronous one (K-TRAP-2).
    #[must_use]
    pub fn raised(&self) -> bool {
        SYNCHRONOUS.iter().any(|&signal| self.contains(signal))
    }

    /// Whether no signal is pending.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(Option::is_none)
    }

    #[must_use]
    pub const fn contains(&self, signal: i32) -> bool {
        self.0[(signal - 1).cast_unsigned() as usize].is_some()
    }

    /// Forgets `signal`, if it is pending.
    pub(super) const fn remove(&mut self, signal: i32) {
        self.0[(signal - 1).cast_unsigned() as usize] = None;
    }

    /// Takes the signal the kernel delivers next: synchronous signals
    /// first, then the lowest-numbered.
    fn take_next(&mut self) -> Option<SigInfo> {
        let signal = SYNCHRONOUS
            .into_iter()
            .find(|&signal| self.contains(signal))
            .or_else(|| (1..32).find(|&signal| self.contains(signal)))?;

        self.0[(signal - 1).cast_unsigned() as usize].take()
    }
}

/// The signals that stop a process by default.
const STOPPING: [i32; 4] = [SIGSTOP, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU];

/// What a signal does when the program has no handler for it.
/// Signals that dump core terminate too: core dumps are off, as with a zero
/// core size limit.
enum DefaultAction {
    Terminate,
    Ignore,
    Stop,
}

const fn default_action(signal: i32) -> DefaultAction {
    match signal {
        libc::SIGCHLD | libc::SIGCONT | libc::SIGURG | libc::SIGWINCH => DefaultAction::Ignore,
        libc::SIGSTOP | libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU => DefaultAction::Stop,
        _ => DefaultAction::Terminate,
    }
}

/// A signal's conventional name.
#[must_use]
pub fn name(signal: i32) -> String {
    nix::sys::signal::Signal::try_from(signal).map_or_else(
        |_| format!("signal {signal}"),
        |signal| signal.as_str().to_owned(),
    )
}

impl Kernel {
    /// `kill(tgid, signal)`, sent by the tracer: SIGKILL, or SIGCONT.
    pub fn kill(&mut self, group: Tgid, signal: i32) -> Result<(), Errno> {
        if !self.processes.contains_key(&group) {
            return Err(Errno::ESRCH);
        }

        match signal {
            SIGKILL => self.kill_process(group, ExitStatus::Signal(SIGKILL, false)),
            SIGCONT => self.continue_process(group),
            _ => {
                self.gap(format!("kill with {}", name(signal)));
                return Err(Errno::ENOSYS);
            }
        }

        Ok(())
    }

    /// SIGCONT from the tracer to `group` (K-STOP-3). It flushes every
    /// pending stop signal and ends the process's job-control stop, which
    /// its parent hears of once one of its threads next runs. It
    /// interrupts each seized thread, as `PTRACE_INTERRUPT` does. Queued
    /// for the process when its leader is traced, it is otherwise
    /// discarded, as an ignored signal is (Linux's `sig_ignored`).
    fn continue_process(&mut self, group: Tgid) {
        let sender = self.caller();

        let process = self.processes.get_mut(&group).expect("the process lives");
        for signal in STOPPING {
            process.shared.remove(signal);
        }

        let ended = process.stopped.take().is_some();
        process.continued |= ended;

        for thread in self
            .threads
            .values_mut()
            .filter(|thread| thread.tgid == group)
        {
            for signal in STOPPING {
                thread.pending.remove(signal);
            }

            if thread.state == State::JobStopped {
                thread.state = State::Running;
            }

            if let Tracing::Seized { interrupted, .. } = &mut thread.tracing
                && !thread.state.exiting()
            {
                *interrupted = true;
            }
        }

        if ended {
            self.happenings.push(Happening::Continued { tgid: group });
        }

        if self
            .threads
            .get(&group.leader())
            .is_some_and(Thread::traced)
        {
            self.processes
                .get_mut(&group)
                .expect("the process lives")
                .shared
                .insert(SigInfo {
                    signal: SIGCONT,
                    code: SI_USER,
                    pid: sender,
                    address: 0,
                });
        }
    }

    /// Tells a parent its child continued, once a thread of the child's
    /// that SIGCONT woke runs (K-STOP-3).
    pub(super) fn report_continued(&mut self, tid: Tid) {
        let group = self.threads[&tid].tgid;
        let Some(process) = self
            .processes
            .get_mut(&group)
            .filter(|process| process.continued)
        else {
            return;
        };

        process.continued = false;
        self.notify_parent(group, libc::CLD_CONTINUED);
    }

    /// Queues SIGCHLD with `code` for `group`'s parent, if a live process.
    pub(super) fn notify_parent(&mut self, group: Tgid, code: i32) {
        let Some(Parent::Process(parent)) =
            self.processes.get(&group).map(|process| process.parent)
        else {
            return;
        };

        if let Some(parent) = self.processes.get_mut(&parent) {
            parent.shared.insert(SigInfo {
                signal: libc::SIGCHLD,
                code,
                pid: group.id(),
                address: 0,
            });
        }
    }

    /// `tgkill(tgid, tid, SIGSTOP)`, sent by the tracer. K-INT-2: a thread
    /// that has not been reaped, even a zombie, accepts it. K-STOP-3: it
    /// flushes a pending SIGCONT.
    pub fn tgkill(&mut self, group: Tgid, tid: Tid, signal: i32) -> Result<(), Errno> {
        let tracer = self.caller();
        if self
            .threads
            .get(&tid)
            .is_none_or(|thread| thread.tgid != group)
        {
            return Err(Errno::ESRCH);
        }

        if signal != SIGSTOP {
            self.gap(format!("tgkill with {}", name(signal)));
            return Err(Errno::ENOSYS);
        }

        if let Some(process) = self.processes.get_mut(&group) {
            process.shared.remove(SIGCONT);
        }

        for thread in self
            .threads
            .values_mut()
            .filter(|thread| thread.tgid == group)
        {
            thread.pending.remove(SIGCONT);
        }

        // K-SIG-1: delivered at the thread's next chance as a stop from
        // the tracer.
        let thread = self.threads.get_mut(&tid).expect("the thread exists");
        thread.pending.insert(SigInfo {
            signal,
            code: SI_TKILL,
            pid: tracer,
            address: 0,
        });
        Ok(())
    }

    /// Ends every thread of a process with `exit`, as `exit_group` or a
    /// fatal signal does (K-EXIT-1, K-EXIT-3). Any thread in a stop is taken
    /// out of it, losing a status not yet reported (K-WAIT-1, K-EXIT-2):
    /// one at the exit event of its own `exit` finishes exiting, and any
    /// other runs to its exit. A system call a thread stopped inside, or
    /// was resumed from inside and has not yet run to the end of, returns
    /// on the way out. A group already exiting keeps its status, and its
    /// threads at their exit events stay there (K-EXIT-4).
    pub(super) fn kill_process(&mut self, group: Tgid, exit: ExitStatus) {
        let Some(process) = self.processes.get_mut(&group) else {
            return;
        };

        if process.group_exit.is_some() {
            return;
        }

        process.group_exit = Some(exit);
        let mut pulled = Vec::new();
        let mut finished = Vec::new();

        for thread in self
            .threads
            .values_mut()
            .filter(|thread| thread.tgid == group)
        {
            match thread.state {
                State::Running | State::JobStopped => {
                    thread.state = State::Exiting(exit);

                    // K-EXIT-3: the call the thread was resumed inside of
                    // returns before the signal is taken.
                    if let Some(result) = thread.returning.take() {
                        thread.registers.general[super::RAX] = result;
                    }
                }
                State::Stopped {
                    kind:
                        StopKind::Signal(_)
                        | StopKind::Event(..)
                        | StopKind::Group(_)
                        | StopKind::Syscall { .. },
                    ..
                } => {
                    thread.state = State::Exiting(exit);
                    thread.report = None;

                    // K-SYS-5: a call stopped at its entry never runs.
                    thread.entered = false;

                    // A system call the thread stopped inside returns on
                    // the way out.
                    if let Some(result) = thread.returning.take() {
                        thread.registers.general[super::RAX] = result;
                    }

                    pulled.push(thread.tid);
                }

                State::Stopped {
                    kind: StopKind::Exit(own),
                    ..
                } => finished.push((thread.tid, own)),
                State::Exiting(_) | State::Zombie(_) => {}
            }
        }

        for &(tid, own) in &finished {
            self.become_zombie(tid, own);
        }

        self.happenings.extend(
            pulled
                .into_iter()
                .chain(finished.into_iter().map(|(tid, _)| tid))
                .map(|tid| Happening::PulledFromStop { tid }),
        );
    }

    /// Delivers a running thread's pending signals, its own before its
    /// process's, until one stops it or none is left. A traced thread
    /// stops for the tracer; an untraced one takes the signal's default
    /// action, and goes on to the next one it ignores, as Linux's
    /// `get_signal` does. Returns whether it stopped.
    pub(super) fn deliver_pending(&mut self, tid: Tid) -> bool {
        loop {
            let thread = self.threads.get_mut(&tid).expect("running thread exists");
            let group = thread.tgid;
            let Some(info) = thread.pending.take_next().or_else(|| {
                self.processes
                    .get_mut(&group)
                    .and_then(|process| process.shared.take_next())
            }) else {
                return false;
            };

            // Synchronous signals are only ever those an instruction raised
            // (K-TRAP-2).
            if ![SIGSTOP, SIGCONT, libc::SIGCHLD].contains(&info.signal)
                && !SYNCHRONOUS.contains(&info.signal)
            {
                self.gap(format!("delivering pending {}", name(info.signal)));
                return true;
            }

            self.signal_stop(tid, info);
            if !matches!(self.threads[&tid].state, State::Running) {
                return true;
            }
        }
    }

    /// Resumes a thread from a signal-delivery-stop with `signal` delivered
    /// to it, or suppressed when `None`.
    pub(super) fn resume_with(&mut self, tid: Tid, signal: Option<i32>) {
        let thread = self.threads.get_mut(&tid).expect("resumed thread exists");
        if let State::Stopped {
            kind: StopKind::Signal(stopped),
            ..
        } = thread.state
            && signal.is_none()
        {
            self.happenings.push(Happening::Suppressed {
                tid,
                signal: stopped,
            });
        }

        thread.state = State::Running;
        thread.report = None;
        if let Some(signal) = signal {
            self.act_by_default(tid, signal);
        }
    }

    /// Takes a thread through `signal`'s default action, as if no debugger
    /// were there: SIGCHLD and SIGCONT are ignored, a trap or fault ends its
    /// process (K-FORK-2), and SIGSTOP stops an untraced process with one
    /// thread (K-STOP-1).
    pub(super) fn act_by_default(&mut self, tid: Tid, signal: i32) {
        self.happenings.push(Happening::Delivered { tid, signal });

        match default_action(signal) {
            DefaultAction::Ignore => {}
            DefaultAction::Terminate => {
                let group = self.threads[&tid].tgid;
                self.kill_process(group, ExitStatus::Signal(signal, false));
            }
            DefaultAction::Stop => self.stop_process(tid, signal),
        }
    }

    /// Stops `tid`'s process for `signal`, untraced, and tells its parent
    /// (K-STOP-1). Group-stops of a traced thread, or of a process with
    /// several threads, are not modeled.
    fn stop_process(&mut self, tid: Tid, signal: i32) {
        let thread = &self.threads[&tid];
        let group = thread.tgid;
        if thread.traced() {
            self.gap(format!("group-stop of a traced thread by {}", name(signal)));
            return;
        }

        if self.threads_of(group).count() > 1 {
            self.gap(format!(
                "group-stop of a process with several threads by {}",
                name(signal)
            ));
            return;
        }

        self.threads.get_mut(&tid).expect("the thread").state = State::JobStopped;
        let process = self.processes.get_mut(&group).expect("the process lives");
        process.stopped = Some(signal);
        process.continued = false;
        self.happenings.push(Happening::JobStopped { tid });
        self.notify_parent(group, libc::CLD_STOPPED);
    }

    /// Takes an exiting thread to its exit: the exit event stop when exits
    /// are traced (K-EXIT-1, K-EXIT-3), otherwise straight to a zombie.
    pub(super) fn reach_exit(&mut self, tid: Tid, exit: ExitStatus) {
        let thread = self.threads.get_mut(&tid).expect("exiting thread exists");
        if thread.traced() && thread.options.trace_exit {
            thread.enter_stop(
                StopKind::Exit(exit),
                SigInfo::event(tid, libc::PTRACE_EVENT_EXIT),
                WaitStatus::Event(tid, libc::PTRACE_EVENT_EXIT),
            );
        } else {
            self.become_zombie(tid, exit);
        }
    }

    /// Ends a thread for good; its status waits to be reaped. A leader
    /// leaving others behind stays a zombie until they are gone (K-EXIT-5).
    pub(super) fn become_zombie(&mut self, tid: Tid, exit: ExitStatus) {
        let thread = self.threads.get_mut(&tid).expect("exiting thread exists");
        let group = thread.tgid;
        let traced = thread.traced();
        thread.state = State::Zombie(exit);

        // The tracer reaps a traced thread; nobody waits for an untraced one
        // but its process's parent.
        thread.report = traced.then(|| exit.wait_status(tid));
        thread.pending = Pending::default();
        self.forget_children(tid);

        let Some(process) = self.processes.get(&group) else {
            return;
        };

        let alone = process.group_exit.is_none()
            && self.threads.values().any(|thread| {
                thread.tgid == group
                    && thread.tid != tid
                    && !matches!(thread.state, State::Zombie(_))
            });
        if tid == group.leader() && alone {
            self.happenings.push(Happening::LeaderExitedAlone { tid });
        }

        if !traced {
            self.reap_untraced(tid);
        }
    }
}
