//! Ending a process when a panic ends its controller, as
//! `dev/assertions.md` requires: a launched one is killed, as a shutdown
//! would, and an attached one released. The controller's state may
//! contradict itself, so the release trusts only what the kernel says:
//! which threads answer as stopped, which bytes still hold a trap.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use nix::libc;
use nix::unistd::Pid;
use tokio::sync::mpsc::error::TryRecvError;

use super::debug_registers::CONTROL_REGISTER;
use super::memory::holding_word;
use super::native::LinuxTraceOps;
use super::session::{Orphans, Session, Shutdown};
use super::{
    BREAKPOINT_OPCODE, Controller, ControllerMessage, Ending, Inferior, InferiorOrigin, Signal,
    Tracked, WaitEvent, Waiter,
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

    /// Threads due a SIGSTOP the debugger sent, which the release takes
    /// rather than hands on to the released process.
    debugger_stops: BTreeSet<Pid>,

    /// Threads resumed with the signal they were to take, which they took.
    delivered: BTreeSet<Pid>,
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
    /// Kills a launched process after a panic, unless the session releases
    /// it on shutdown, and otherwise releases it as a detach would: every
    /// thread stopped, each byte that still holds the debugger's trap given
    /// back its instruction, a thread that executed such a trap rewound
    /// over it, debug registers cleared, and every thread detached with the
    /// signal it was to take. A thread that does not stop in time is left
    /// for the kernel to release when this thread exits. Once the process
    /// has ended, the fork children still due their first stop are
    /// released instead.
    pub(super) fn release_after_panic(&mut self) {
        // A fork child is released, never held for another session.
        self.shutdown.get_or_insert_with(Shutdown::default);

        // The replies the session owed are dropped: the panic ends the
        // controller, which fails them at their clients.
        let mut inferior = match std::mem::replace(&mut self.session, Session::idle()) {
            Session::Traced(traced) => traced.inferior,
            Session::Draining { orphans, .. } => {
                self.release_orphans_after_panic(orphans);
                return;
            }
            Session::Idle { .. } => return,
        };

        if inferior.origin == InferiorOrigin::PostMortem {
            return;
        }

        // A stop no event announced yet is a new thread's or a fork child's.
        // Only a child is scrubbed, and killed if it cannot be: SIGKILL sent
        // to a thread would end the whole process.
        let tgid = inferior.tgid;
        let (unannounced_children, unannounced_threads): (Vec<Pid>, Vec<Pid>) =
            (inferior.tracked.iter())
                .filter(|(_, record)| matches!(record, Tracked::EarlyStop(_)))
                .map(|(&pid, _)| pid)
                .partition(|&pid| {
                    self.ptrace
                        .thread_group_id(pid)
                        .is_ok_and(|group| group != tgid)
                });
        let threads: Vec<Pid> = (inferior.threads.keys().copied())
            .chain(unannounced_threads)
            .collect();
        let children: Vec<Pid> = (inferior.tracked.iter())
            .filter(|(_, record)| matches!(record, Tracked::ForkChild { .. }))
            .map(|(&pid, _)| pid)
            .chain(unannounced_children)
            .collect();
        let sites = inferior.inherited_sites();

        // As a shutdown would, a launched process is killed unless the
        // session releases it, and its fork children are released. The
        // kernel's PTRACE_O_EXITKILL comes only when the tracer thread ends,
        // which a group's outlives this controller by, leaving the process
        // stopped with no one to serve it.
        if inferior.origin.owned() && self.ending == Ending::Kill {
            record!("killing {} after a panic", tgid);
            let _ = self.ptrace.kill(tgid, Signal::SIGKILL);
            let seen = self.stop_for_release(
                inferior.waiter.take(),
                &[],
                &children,
                None,
                BTreeSet::new(),
                |_| false,
            );
            self.settle_children_after_panic(&inferior, children, &seen, &sites);
            return;
        }

        record!("releasing {} after a panic", tgid);
        let seen = self.release_threads_after_panic(&mut inferior, &threads, &children, &sites);
        self.settle_children_after_panic(&inferior, children, &seen, &sites);
    }

    /// Stops, scrubs, and detaches the threads of `inferior`, and stops its
    /// `children`. Returns what the statuses said of each.
    fn release_threads_after_panic(
        &self,
        inferior: &mut Inferior,
        threads: &[Pid],
        children: &[Pid],
        sites: &[(VirtualAddress, u8)],
    ) -> Seen {
        let tgid = inferior.tgid;

        // A launched process's threads stop for a SIGSTOP the debugger
        // sends, which another stop may have overtaken, leaving it queued.
        let seized = inferior.origin.seized();
        let debugger_stops: BTreeSet<Pid> = (inferior.threads.iter())
            .filter(|(_, thread)| !seized && thread.debugger_stop_pending)
            .map(|(&pid, _)| pid)
            .collect();
        let mut seen = self.stop_for_release(
            inferior.waiter.take(),
            threads,
            children,
            (!seized).then_some(tgid),
            debugger_stops,
            |pid| {
                inferior
                    .threads
                    .get(&pid)
                    .is_some_and(|thread| inferior.exited_leader(pid, thread))
            },
        );

        // The signal each thread is released with, as a detach would pass
        // it on, short of a SIGSTOP of the debugger's.
        let signal_for = |pid: Pid, seen: &Seen| {
            let thread = inferior.threads.get(&pid);
            seen.signals
                .get(&pid)
                .copied()
                .filter(|&signal| signal != Signal::SIGSTOP || !seen.debugger_stops.contains(&pid))
                .or_else(|| {
                    thread
                        .filter(|_| !seen.delivered.contains(&pid))
                        .and_then(|thread| thread.pending_signal.or(thread.held_signal))
                        .map(|pending| pending.signal)
                })
                .filter(|&signal| self.signals.get(signal).pass)
        };

        self.drain_queued_traps_for_release(threads, &mut seen);

        let live = |pid: &Pid, seen: &Seen| seen.stopped.contains(pid) && !seen.gone.contains(pid);
        let stopped: Vec<Pid> = threads
            .iter()
            .copied()
            .filter(|pid| live(pid, &seen))
            .collect();
        if let Some(&memory) = stopped.first() {
            self.restore_every_trap(memory, sites);
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
        }

        // Once each thread is rewound, a resume to take a SIGSTOP may
        // deliver a signal, whose handler returns where the thread stopped.
        self.drain_debugger_stops_for_release(&stopped, &mut seen, signal_for);

        let released: Vec<Pid> = stopped.into_iter().filter(|pid| live(pid, &seen)).collect();
        for &pid in &released {
            let _ = self.ptrace.detach(pid, signal_for(pid, &seen));
        }

        record!(
            "released {} after a panic: {} of {} threads stopped",
            tgid,
            released.len(),
            threads.len()
        );
        seen
    }

    /// Scrubs and releases the fork children of `inferior` that stopped.
    /// A child keeps the traps its parent held when it forked.
    fn settle_children_after_panic(
        &mut self,
        inferior: &Inferior,
        children: Vec<Pid>,
        seen: &Seen,
        sites: &[(VirtualAddress, u8)],
    ) {
        let live = |pid: &Pid| seen.stopped.contains(pid) && !seen.gone.contains(pid);
        for child in children.into_iter().filter(live) {
            let (child_sites, vfork) = match inferior.tracked.get(&child) {
                Some(Tracked::ForkChild { sites, vfork }) => (sites.as_slice(), *vfork),
                _ => (sites, false),
            };
            self.scrub_and_settle(inferior.tgid, child, child_sites, vfork);
        }
    }

    /// Releases the fork children a process left on their way to their
    /// first stop, scrubbed of the traps they inherited, as that stop and a
    /// shutdown would have, whether the process was attached or launched.
    /// Children already killed for traps they kept need nothing more.
    fn release_orphans_after_panic(&mut self, mut orphans: Orphans) {
        record!(
            "releasing the fork children of {} after a panic",
            orphans.parent
        );
        let children: Vec<Pid> = orphans.children.keys().copied().collect();
        let seen = self.stop_for_release(
            orphans.waiter.take(),
            &[],
            &children,
            None,
            BTreeSet::new(),
            |_| false,
        );
        for child in children {
            if seen.stopped.contains(&child)
                && !seen.gone.contains(&child)
                && let Some(sites) = orphans.children.get(&child)
            {
                self.scrub_and_settle(orphans.parent, child, sites, false);
            }
        }
    }

    /// Stops every thread and child it can, and learns from their statuses
    /// which stopped, which exited, and what each stopped to take. The
    /// threads of `signaled`, a process that was not seized, stop only for a
    /// SIGSTOP, as do those already in `debugger_stops`.
    fn stop_for_release(
        &self,
        mut waiter: Option<Waiter>,
        threads: &[Pid],
        children: &[Pid],
        signaled: Option<Pid>,
        debugger_stops: BTreeSet<Pid>,
        zombie: impl Fn(Pid) -> bool,
    ) -> Seen {
        let mut seen = Seen {
            debugger_stops,
            ..Seen::default()
        };

        // Statuses already taken from the waiter and not yet handled come
        // first: their threads are stopped, and some carry a signal.
        for message in self.pending.borrow_mut().drain(..) {
            if let ControllerMessage::Wait(status) = message {
                seen.note(&status);
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

        // A waiter that panicked takes no more statuses, so the threads'
        // stops are this thread's to take, after those it took before.
        let waiter_failed = waiter.as_ref().is_some_and(Waiter::failed);
        if waiter_failed && let Some(waiter) = waiter.take() {
            let _ = waiter.stop_and_join(|| take_queued(&mut seen));
        }

        // Only a thread in a ptrace-stop answers a register read.
        let mut awaited = BTreeSet::new();
        for &pid in threads.iter().chain(children) {
            // A leader that exited before its process stays a zombie, which
            // no request reaches, until its last sibling ends.
            if zombie(pid) {
                continue;
            }

            if self.ptrace.registers(pid).is_ok() {
                seen.stopped.insert(pid);
            } else if !seen.gone.contains(&pid) {
                // A fork child stops on its own on the way to its first stop.
                match signaled {
                    Some(process) if threads.contains(&pid) => {
                        let _ = self.ptrace.request_stop(process, pid);
                        seen.debugger_stops.insert(pid);
                    }
                    _ => {
                        let _ = self.ptrace.interrupt(pid);
                    }
                }
                awaited.insert(pid);
            }
        }

        let deadline = Instant::now() + STOP_WAIT;
        while awaited
            .iter()
            .any(|pid| !seen.stopped.contains(pid) && !seen.gone.contains(pid))
            && Instant::now() < deadline
        {
            if waiter_failed {
                for &pid in &awaited {
                    while let Some(status) = self.ptrace.poll_status(pid) {
                        seen.note(&status);
                    }
                }
            }

            match self.messages.borrow_mut().try_recv() {
                Ok(ControllerMessage::Wait(status)) => seen.note(&status),
                // A request dropped unanswered is cancelled for its client.
                Ok(
                    ControllerMessage::Request(_)
                    | ControllerMessage::AgentTimeout
                    | ControllerMessage::Inspector(_),
                ) => {}
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(1)),
                Err(TryRecvError::Disconnected) => break,
            }
        }

        if let Some(waiter) = waiter.take() {
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

    /// Takes the traps threads raised just before an interrupt stopped
    /// them, as `drain_queued_traps` does for a detach. `PTRACE_INTERRUPT`
    /// stops a thread before it dequeues signals, so its trap is still
    /// queued, and the detach would deliver it to the released process.
    /// Resumed without a signal, the thread dequeues it into a stop for the
    /// trap before it runs an instruction, and is then rewound over it. A
    /// signal it stopped to take is not lost: the detach passes it on from
    /// the trap's stop, as it would have from its own.
    fn drain_queued_traps_for_release(&self, threads: &[Pid], seen: &mut Seen) {
        let queued: Vec<Pid> = threads
            .iter()
            .copied()
            .filter(|pid| {
                seen.stopped.contains(pid)
                    && !seen.gone.contains(pid)
                    && !seen.trapped.contains(pid)
            })
            .filter(|&pid| {
                self.ptrace
                    .queued_signal(pid, Signal::SIGTRAP)
                    .unwrap_or(false)
            })
            .collect();

        let resumed: Vec<Pid> = queued
            .into_iter()
            .filter(|&pid| self.ptrace.continue_execution(pid, None).is_ok())
            .collect();
        self.await_stops(&resumed, seen);
    }

    /// Takes the SIGSTOPs the debugger sent that another stop overtook, as
    /// `drain_requested_stops` does for a detach: the detach would deliver
    /// one and stop the released process for good. Resumed with the signal
    /// it was to take, the thread takes that signal, then dequeues the
    /// SIGSTOP into a stop before it runs an instruction.
    fn drain_debugger_stops_for_release(
        &self,
        threads: &[Pid],
        seen: &mut Seen,
        signal_for: impl Fn(Pid, &Seen) -> Option<Signal>,
    ) {
        let queued: Vec<Pid> = threads
            .iter()
            .copied()
            .filter(|pid| seen.debugger_stops.contains(pid) && !seen.gone.contains(pid))
            .filter(|&pid| {
                self.ptrace
                    .queued_signal(pid, Signal::SIGSTOP)
                    .unwrap_or(false)
            })
            .collect();

        let mut resumed = Vec::new();
        for pid in queued {
            let signal = signal_for(pid, seen);
            if self.ptrace.continue_execution(pid, signal).is_ok() {
                if signal.is_some() {
                    seen.signals.remove(&pid);
                    seen.delivered.insert(pid);
                }
                resumed.push(pid);
            }
        }

        self.await_stops(&resumed, seen);
    }

    /// Waits for the threads in `resumed` to stop again, or exit. A waiter
    /// still running for other members may take a stop and route it to this
    /// controller's queue.
    fn await_stops(&self, resumed: &[Pid], seen: &mut Seen) {
        for pid in resumed {
            seen.stopped.remove(pid);
        }

        let deadline = Instant::now() + STOP_WAIT;
        loop {
            while let Ok(message) = self.messages.borrow_mut().try_recv() {
                if let ControllerMessage::Wait(status) = message {
                    seen.note(&status);
                }
            }
            for &pid in resumed {
                while let Some(status) = self.ptrace.poll_status(pid) {
                    seen.note(&status);
                }
            }

            let waiting = resumed
                .iter()
                .any(|pid| !seen.stopped.contains(pid) && !seen.gone.contains(pid));
            if !waiting || Instant::now() >= deadline {
                return;
            }

            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Gives each byte of `sites` that still holds a trap its original back,
    /// going on past any that fails, unlike a fork child's scrub, which
    /// kills the child instead.
    fn restore_every_trap(&self, pid: Pid, sites: &[(VirtualAddress, u8)]) {
        for &(address, original) in sites {
            let (word_address, offset) = holding_word(address.get());
            let Ok(word) = self.ptrace.read_memory_word(pid, word_address) else {
                continue;
            };

            let mut bytes = word.to_ne_bytes();
            let byte = offset.of(&mut bytes);
            if *byte == BREAKPOINT_OPCODE {
                *byte = original;
                let _ = self
                    .ptrace
                    .write_word(pid, word_address, u64::from_ne_bytes(bytes));
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
