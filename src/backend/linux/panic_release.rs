//! Releasing an attached process when a panic ends the controller, as
//! `dev/assertions.md` requires. The controller's state may contradict
//! itself, so the release trusts only what the kernel says: which threads
//! answer as stopped, which bytes still hold a trap. A launched process
//! needs none of this: `PTRACE_O_EXITKILL` kills it with its tracer.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use nix::libc;
use nix::unistd::Pid;
use tokio::sync::mpsc::error::TryRecvError;

use super::debug_registers::CONTROL_REGISTER;
use super::native::LinuxTraceOps;
use super::{
    BREAKPOINT_OPCODE, Controller, ControllerMessage, Inferior, InferiorOrigin, Signal, WaitEvent,
};

use crate::VirtualAddress;

/// How long the release waits for the threads it interrupted to stop.
const STOP_WAIT: Duration = Duration::from_secs(1);

/// What the release learned of a thread from its wait statuses.
#[derive(Default)]
struct Seen {
    /// Threads now in a ptrace-stop.
    stopped: BTreeSet<Pid>,

    /// Threads that exited.
    gone: BTreeSet<Pid>,

    /// The signal each thread stopped to take, which it keeps once released.
    signals: BTreeMap<Pid, Signal>,

    /// Threads that stopped for a trap: an `int3`, a step, or a watchpoint.
    trapped: BTreeSet<Pid>,
}

impl Seen {
    fn note(&mut self, status: &WaitEvent) {
        let pid = status.pid();
        match *status {
            WaitEvent::Exited(..) | WaitEvent::Signaled(..) => {
                self.stopped.remove(&pid);
                self.gone.insert(pid);
            }
            WaitEvent::Stopped(_, Signal::SIGTRAP) => {
                self.stopped.insert(pid);
                self.trapped.insert(pid);
            }
            WaitEvent::Stopped(_, signal) => {
                self.stopped.insert(pid);
                self.signals.insert(pid, signal);
            }
            WaitEvent::PtraceEvent(..) | WaitEvent::PtraceSyscall(_) => {
                self.stopped.insert(pid);
            }
            WaitEvent::Continued(_) => {}
        }
    }
}

impl<P: LinuxTraceOps> Controller<P> {
    /// Releases an attached process after a panic, as a detach would: every
    /// thread stopped, each byte that still holds the debugger's trap given
    /// back its instruction, a thread that executed such a trap rewound
    /// over it, debug registers cleared, and every thread detached with the
    /// signal it was to take. A thread that does not stop in time is left
    /// for the kernel to release when this thread exits.
    pub(super) fn release_after_panic(&mut self) {
        // A fork child is released, never held for another session.
        self.shutting_down = true;
        let Some(mut inferior) = self.inferior.take() else {
            return;
        };

        if inferior.origin != InferiorOrigin::Attached {
            return;
        }

        record!("releasing {} after a panic", inferior.tgid);

        // A stop no event announced yet is a new thread's or a fork child's.
        // Only a child is scrubbed, and killed if it cannot be: SIGKILL sent
        // to a thread would end the whole process.
        let tgid = inferior.tgid;
        let (unannounced_children, unannounced_threads): (Vec<Pid>, Vec<Pid>) =
            inferior.unowned_stops.keys().copied().partition(|&pid| {
                self.ptrace
                    .thread_group_id(pid)
                    .is_ok_and(|group| group != tgid)
            });
        let threads: Vec<Pid> = (inferior.threads.keys().copied())
            .chain(unannounced_threads)
            .collect();
        let children: Vec<Pid> = (inferior.fork_children.keys().copied())
            .chain(unannounced_children)
            .collect();
        let seen = self.stop_for_release(&mut inferior, &threads, &children);
        let live = |pid: &Pid| seen.stopped.contains(pid) && !seen.gone.contains(pid);
        let stopped: Vec<Pid> = threads.iter().copied().filter(live).collect();
        let sites = inferior.inherited_sites();
        if let Some(&memory) = stopped.first() {
            self.restore_every_trap(memory, &sites);
        }

        let traps: BTreeSet<VirtualAddress> = sites
            .iter()
            .filter(|&&(_, original)| original != BREAKPOINT_OPCODE)
            .map(|&(address, _)| address)
            .collect();
        for &pid in &stopped {
            if seen.trapped.contains(&pid) {
                self.rewind_over_trap(pid, &traps);
            }

            let _ = self.ptrace.write_debug_register(pid, CONTROL_REGISTER, 0);
            let thread = inferior.threads.get(&pid);
            let signal = seen
                .signals
                .get(&pid)
                .copied()
                .filter(|&signal| {
                    signal != Signal::SIGSTOP
                        || !thread.is_some_and(|thread| thread.debugger_stop_pending)
                })
                .or_else(|| {
                    thread
                        .and_then(|thread| thread.pending_signal.or(thread.held_signal))
                        .map(|pending| pending.signal)
                })
                .filter(|&signal| self.signals.get(signal).pass);

            let _ = self.ptrace.detach(pid, signal);
        }

        // A child keeps the traps its parent held when it forked.
        for child in children.into_iter().filter(live) {
            let child_sites = inferior.fork_children.get(&child).unwrap_or(&sites);
            let vfork = inferior.vfork_children.contains(&child);
            self.scrub_and_settle(tgid, child, child_sites, vfork);
        }

        record!(
            "released {} after a panic: {} of {} threads stopped",
            inferior.tgid,
            stopped.len(),
            threads.len()
        );
    }

    /// Stops every thread and child it can, and learns from their statuses
    /// which stopped, which exited, and what each stopped to take.
    fn stop_for_release(&self, inferior: &mut Inferior, threads: &[Pid], children: &[Pid]) -> Seen {
        let mut seen = Seen::default();

        // Statuses already taken from the waiter and not yet handled come
        // first: their threads are stopped, and some carry a signal.
        for message in self.pending.borrow_mut().drain(..) {
            if let ControllerMessage::Wait(status) = message {
                seen.note(&status);
            }
        }

        // Only a thread in a ptrace-stop answers a register read.
        let mut awaited = BTreeSet::new();
        for &pid in threads.iter().chain(children) {
            // A leader that exited before its process stays a zombie, which
            // no request reaches, until its last sibling ends.
            if inferior
                .threads
                .get(&pid)
                .is_some_and(|thread| inferior.exited_leader(pid, thread))
            {
                continue;
            }

            if self.ptrace.registers(pid).is_ok() {
                seen.stopped.insert(pid);
            } else if !seen.gone.contains(&pid) {
                let _ = self.ptrace.interrupt(pid);
                awaited.insert(pid);
            }
        }

        let deadline = Instant::now() + STOP_WAIT;
        while awaited
            .iter()
            .any(|pid| !seen.stopped.contains(pid) && !seen.gone.contains(pid))
            && Instant::now() < deadline
        {
            match self.messages.borrow_mut().try_recv() {
                Ok(ControllerMessage::Wait(status)) => seen.note(&status),
                // A request dropped unanswered is cancelled for its client.
                Ok(ControllerMessage::Request(_) | ControllerMessage::AgentTimeout) => {}
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(1)),
                Err(TryRecvError::Disconnected) => break,
            }
        }

        // A waiter blocked on a full queue ends once its status is taken.
        let take_queued = |seen: &mut Seen| {
            while let Ok(message) = self.messages.borrow_mut().try_recv() {
                if let ControllerMessage::Wait(status) = message {
                    seen.note(&status);
                }
            }
        };

        if let Some(waiter) = inferior.waiter.take() {
            let _ = waiter.stop_and_join(|| take_queued(&mut seen));
        }

        take_queued(&mut seen);

        // With the waiter gone, the statuses the kernel still holds are the
        // controller's to take: a stop at a trap or for a signal, unreported.
        for &pid in threads.iter().chain(children) {
            while let Some(status) = self.ptrace.poll_status(pid) {
                seen.note(&status);
            }
        }

        seen
    }

    /// Gives each byte of `sites` that still holds a trap its original back,
    /// going on past any that fails, unlike a fork child's scrub, which
    /// kills the child instead.
    fn restore_every_trap(&self, pid: Pid, sites: &[(VirtualAddress, u8)]) {
        for &(address, original) in sites {
            let Ok(word) = self.ptrace.read_memory_word(pid, address.get()) else {
                continue;
            };

            let mut bytes = word.to_ne_bytes();
            if bytes[0] == BREAKPOINT_OPCODE {
                bytes[0] = original;
                let _ = self
                    .ptrace
                    .write_word(pid, address.get(), u64::from_ne_bytes(bytes));
            }
        }
    }

    /// Rewinds a thread that executed one of `traps` before it was
    /// removed, so that it runs the instruction now restored there. Only an
    /// `int3` reports `SI_KERNEL`.
    fn rewind_over_trap(&self, pid: Pid, traps: &BTreeSet<VirtualAddress>) {
        let Ok(metadata) = self.ptrace.signal_metadata(pid) else {
            return;
        };

        if metadata.code != libc::SI_KERNEL {
            return;
        }

        let Ok(mut registers) = self.ptrace.registers(pid) else {
            return;
        };

        let Some(address) = registers.rip.checked_sub(1) else {
            return;
        };

        if traps.contains(&VirtualAddress::new(address)) {
            registers.rip = address;
            let _ = self.ptrace.set_registers(pid, registers);
        }
    }
}
