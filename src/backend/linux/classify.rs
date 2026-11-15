use std::collections::BTreeSet;

use super::signals::Signal;
use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;

use crate::VirtualAddress;
use crate::backend::linux::debug_registers;
use crate::protocol::{StopReason, WatchpointId};

use super::native::LinuxTraceOps;
use super::trap::Trap;
use super::{
    BREAKPOINT_OPCODE, ClassifiedStop, Controller, ExpectedStop, NativeThreadState, PendingSignal,
    RawStopRecord, SignalMetadata, TRAP_HARDWARE_BREAKPOINT, TRAP_UNKNOWN,
};

impl<P: LinuxTraceOps> Controller<P> {
    pub(super) fn classify_stop(&self, pid: Pid, signal: Signal) -> ClassifiedStop {
        let status = format!("Stopped({pid}, {signal})");
        let siginfo = self.ptrace.signal_metadata(pid);
        let thread = self
            .session
            .inferior()
            .and_then(|inferior| inferior.threads.get(&pid));
        let stepping = thread.is_some_and(|thread| {
            thread.trap.repairing().is_some()
                || matches!(thread.expected, ExpectedStop::UserStep { .. })
        });
        let starting =
            thread.is_some_and(|thread| matches!(thread.state, NativeThreadState::Starting));
        let debugger_requested = signal == Signal::SIGSTOP
            && thread.is_some_and(|thread| thread.debugger_stop_pending)
            && siginfo.as_ref().is_ok_and(|metadata| {
                metadata.code == libc::SI_TKILL
                    && metadata.sender == Some(self.ptrace.tracer_process())
            });
        let expected_trace = signal == Signal::SIGTRAP
            && siginfo.as_ref().is_ok_and(is_single_step_trap)
            && stepping;
        let code = siginfo.as_ref().ok().map(|metadata| metadata.code);
        let watch = if signal == Signal::SIGTRAP {
            self.watch_status(pid, code)
        } else {
            WatchStatus::Absent
        };

        let trap = signal == Signal::SIGTRAP && !expected_trace && code == Some(libc::SI_KERNEL);
        let breakpoint = trap.then(|| self.normalize_breakpoint_pc(pid)).flatten();
        let removed = trap && breakpoint.is_none() && self.rewind_removed_trap(pid);
        let carried = trap
            && breakpoint.is_none()
            && !removed
            && (self.rewind_carried_trap(pid) || self.rewind_copied_trap(pid));
        let program_trap = (trap && breakpoint.is_none() && !removed && !carried)
            .then(|| self.program_trap(pid))
            .flatten();

        match classify_stop_evidence(
            signal,
            status,
            siginfo,
            stepping,
            starting,
            debugger_requested,
            breakpoint,
            removed || carried,
            program_trap,
            watch,
        ) {
            ClassifiedStop::RemovedTrap if carried => ClassifiedStop::CarriedTrap,
            ClassifiedStop::Unclassifiable(_)
                if is_superseded(&self.ptrace.signal_metadata(pid)) =>
            {
                ClassifiedStop::Superseded
            }
            stop => stop,
        }
    }

    pub(super) fn watch_status(&self, pid: Pid, code: Option<i32>) -> WatchStatus {
        let Some(inferior) = self.session.inferior() else {
            return WatchStatus::Absent;
        };

        let consult = code == Some(TRAP_HARDWARE_BREAKPOINT)
            || (code == Some(libc::TRAP_TRACE) && !inferior.watch.plan.is_empty());
        if !consult {
            return WatchStatus::Absent;
        }

        let status = match self
            .ptrace
            .read_debug_register(pid, debug_registers::STATUS_REGISTER)
        {
            Ok(status) => status,
            Err(error) => return WatchStatus::Unknown(format!("reading DR6 failed: {error}")),
        };

        let _ = self.ptrace.write_debug_register(
            pid,
            debug_registers::STATUS_REGISTER,
            debug_registers::STATUS_IDLE,
        );

        if !debug_registers::status_has_hits(status) {
            return WatchStatus::Absent;
        }

        match inferior.watch.plan.owners_for_status(status) {
            Ok(owners) => WatchStatus::Hits(owners),
            Err(unknown) => WatchStatus::Unknown(format!(
                "DR6 {:#x} reports a hit in a slot no watchpoint owns",
                unknown.status
            )),
        }
    }

    pub(super) fn normalize_breakpoint_pc(&self, pid: Pid) -> Option<VirtualAddress> {
        let mut registers = self.ptrace.registers(pid).ok()?;
        let address = VirtualAddress::new(registers.rip.checked_sub(1)?);
        let installed = self
            .session
            .inferior()
            .and_then(|inferior| inferior.breakpoints.get(&address))
            .is_some_and(|site| site.trap == Trap::Installed);
        if !installed {
            return None;
        }

        registers.rip = address.get();
        self.ptrace.set_registers(pid, registers).ok()?;
        Some(address)
    }

    pub(super) fn rewind_removed_trap(&self, pid: Pid) -> bool {
        let rewind = || {
            let inferior = self.session.inferior()?;
            let mut registers = self.ptrace.registers(pid).ok()?;
            let address = VirtualAddress::new(registers.rip.checked_sub(1)?);
            let original = *inferior.former_sites.get(&address)?;
            if original == BREAKPOINT_OPCODE || inferior.breakpoints.contains_key(&address) {
                return None;
            }

            registers.rip = address.get();
            self.ptrace.set_registers(pid, registers).ok()
        };

        rewind().is_some()
    }

    pub(super) fn program_trap(&self, pid: Pid) -> Option<VirtualAddress> {
        let address = VirtualAddress::new(self.ptrace.registers(pid).ok()?.rip.checked_sub(1)?);
        let ours = self
            .session
            .inferior()
            .and_then(|inferior| inferior.breakpoints.get(&address))
            .is_some_and(|site| {
                site.trap == Trap::Installed && site.original_byte != BREAKPOINT_OPCODE
            });
        if ours {
            return None;
        }

        let byte = super::breakpoints::memory_byte(&self.ptrace, pid, address.get()).ok()??;
        (byte == BREAKPOINT_OPCODE).then_some(address)
    }

    pub(super) fn rewind_carried_trap(&self, pid: Pid) -> bool {
        let rewind = || {
            let inferior = self.session.inferior()?;
            let mut registers = self.ptrace.registers(pid).ok()?;
            let address = registers.rip.checked_sub(1)?;
            let moved = self.moved_code_now(inferior.memory_thread()).ok()?;
            let carried = moved.iter().any(|code| {
                let Some(source) = code.source(VirtualAddress::new(address)) else {
                    return false;
                };

                inferior.breakpoints.get(&source).is_some_and(|site| {
                    site.trap == Trap::Installed && site.original_byte != BREAKPOINT_OPCODE
                })
            });
            if !carried {
                return None;
            }

            registers.rip = address;
            self.ptrace.set_registers(pid, registers).ok()
        };

        rewind().is_some()
    }
}

pub(super) const fn visible_stop_priority(reason: &StopReason) -> u8 {
    match reason {
        StopReason::Attach | StopReason::Entry | StopReason::Pause | StopReason::Jump => 0,
        StopReason::Exception(_) => 1,
        StopReason::LanguageException(_)
        | StopReason::RuntimeEvent { .. }
        | StopReason::ProgramBreakpoint { .. }
        | StopReason::Breakpoint { .. }
        | StopReason::Watchpoint { .. }
        | StopReason::Syscall(_)
        | StopReason::WatchpointInvalidated { .. }
        | StopReason::WatchpointArmFailed { .. }
        | StopReason::Step { .. }
        | StopReason::StepIncomplete { .. }
        | StopReason::TaskEnded { .. }
        | StopReason::FutureDropped { .. }
        | StopReason::Assignment
        | StopReason::ThreadExited { .. } => 2,
        StopReason::Exec { .. } => 3,
        StopReason::Unclassifiable { .. } => 4,
        StopReason::Exited(_) | StopReason::CoreDump { .. } => 5,
    }
}

pub(super) const fn is_stopping_signal(signal: Signal) -> bool {
    matches!(
        signal,
        Signal::SIGSTOP | Signal::SIGTSTP | Signal::SIGTTIN | Signal::SIGTTOU
    )
}

#[derive(Debug)]
pub(super) enum WatchStatus {
    Absent,

    Hits(BTreeSet<WatchpointId>),

    Unknown(String),
}

#[expect(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    reason = "the pure classifier receives every piece of native stop evidence explicitly"
)]
pub(super) fn classify_stop_evidence(
    signal: Signal,
    status: String,
    siginfo: std::result::Result<SignalMetadata, Errno>,
    stepping: bool,
    starting: bool,
    debugger_requested: bool,
    breakpoint: Option<VirtualAddress>,
    removed: bool,
    program_trap: Option<VirtualAddress>,
    watch: WatchStatus,
) -> ClassifiedStop {
    if is_superseded(&siginfo) {
        return ClassifiedStop::Superseded;
    }

    if signal == Signal::SIGSTOP && starting {
        return ClassifiedStop::ThreadStart;
    }

    if debugger_requested {
        return ClassifiedStop::DebuggerRequested;
    }

    if signal == Signal::SIGTRAP
        && siginfo
            .as_ref()
            .is_ok_and(|metadata| metadata.code == TRAP_HARDWARE_BREAKPOINT)
    {
        return match watch {
            WatchStatus::Hits(owners) if !owners.is_empty() => ClassifiedStop::Watch(owners),
            WatchStatus::Unknown(description) => ClassifiedStop::Unclassifiable(RawStopRecord {
                status: format!("{status}; {description}"),
                siginfo,
            }),
            WatchStatus::Hits(_) | WatchStatus::Absent => {
                ClassifiedStop::Unclassifiable(RawStopRecord {
                    status: format!("{status}; hardware breakpoint trap without a reported slot"),
                    siginfo,
                })
            }
        };
    }

    if signal == Signal::SIGTRAP && siginfo.as_ref().is_ok_and(is_single_step_trap) && stepping {
        return match watch {
            WatchStatus::Unknown(description) => ClassifiedStop::Unclassifiable(RawStopRecord {
                status: format!("{status}; {description}"),
                siginfo,
            }),
            WatchStatus::Hits(watch) => ClassifiedStop::Trace { watch },
            WatchStatus::Absent => ClassifiedStop::Trace {
                watch: BTreeSet::new(),
            },
        };
    }

    if let Some(address) = breakpoint {
        return ClassifiedStop::Breakpoint(address);
    }

    if removed {
        return ClassifiedStop::RemovedTrap;
    }

    if let Some(address) = program_trap {
        return ClassifiedStop::ProgramTrap(address);
    }

    match siginfo {
        Ok(metadata) if signal != Signal::SIGTRAP || metadata.code <= 0 => {
            ClassifiedStop::SignalDelivery(PendingSignal {
                signal,
                code: metadata.code,
                sender: metadata.sender,
                fault_address: metadata.fault_address,
            })
        }
        Err(Errno::EINVAL) if is_stopping_signal(signal) => ClassifiedStop::GroupStop(signal),
        outcome => ClassifiedStop::Unclassifiable(RawStopRecord {
            status,
            siginfo: outcome,
        }),
    }
}

pub(super) fn is_superseded(siginfo: &std::result::Result<SignalMetadata, Errno>) -> bool {
    const EXIT_EVENT: i32 = libc::SIGTRAP | (libc::PTRACE_EVENT_EXIT << 8);
    match siginfo {
        Ok(metadata) => metadata.code == EXIT_EVENT,
        Err(errno) => *errno == Errno::ESRCH,
    }
}

pub(super) const fn is_single_step_trap(metadata: &SignalMetadata) -> bool {
    matches!(
        metadata.code,
        libc::TRAP_TRACE | libc::TRAP_BRKPT | TRAP_UNKNOWN
    )
}

pub(super) fn format_raw_stop(raw: &RawStopRecord) -> String {
    match raw.siginfo {
        Ok(metadata) => format!(
            "{}; siginfo code={} sender={:?}",
            raw.status, metadata.code, metadata.sender
        ),
        Err(error) => format!("{}; PTRACE_GETSIGINFO failed: {error}", raw.status),
    }
}
