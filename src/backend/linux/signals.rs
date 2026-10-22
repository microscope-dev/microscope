//! Linux signals and wait statuses, including the real-time signals that
//! `nix` cannot represent.
//!
//! A tracee can receive any signal from 1 through `SIGRTMAX`. glibc itself
//! uses real-time signals, such as the one behind `pthread_cancel`, so every
//! wait status and every signal delivery is handled by number.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::Signal as NixSignal;
use nix::unistd::Pid;

use crate::protocol::SignalPolicy;

/// One Linux signal number.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Signal(i32);

impl Signal {
    pub const SIGINT: Self = Self(libc::SIGINT);

    pub const SIGTRAP: Self = Self(libc::SIGTRAP);

    pub const SIGKILL: Self = Self(libc::SIGKILL);

    pub const SIGTERM: Self = Self(libc::SIGTERM);

    pub const SIGSTOP: Self = Self(libc::SIGSTOP);

    pub const SIGTSTP: Self = Self(libc::SIGTSTP);

    pub const SIGTTIN: Self = Self(libc::SIGTTIN);

    pub const SIGTTOU: Self = Self(libc::SIGTTOU);

    /// The highest signal number Linux delivers.
    pub const MAX: i32 = 64;

    /// Returns the signal with `number`, if Linux defines one.
    pub const fn new(number: i32) -> Option<Self> {
        if number >= 1 && number <= Self::MAX {
            Some(Self(number))
        } else {
            None
        }
    }

    pub const fn number(self) -> i32 {
        self.0
    }

    /// The signal number as a platform-neutral exception code.
    pub const fn code(self) -> u64 {
        self.0.unsigned_abs() as u64
    }

    /// Returns the signal an exception code names, if it is one.
    pub fn from_code(code: u64) -> Option<Self> {
        i32::try_from(code).ok().and_then(Self::new)
    }

    /// Every signal Linux can deliver, in numeric order.
    pub fn all() -> impl Iterator<Item = Self> {
        (1..=Self::MAX).map(Self)
    }

    /// Finds a signal by name, with or without its `SIG` prefix and in any
    /// case, or by number.
    pub fn named(name: &str) -> Option<Self> {
        if let Ok(number) = name.parse::<i32>() {
            return Self::new(number);
        }

        let upper = name.to_ascii_uppercase();
        let full = if upper.starts_with("SIG") {
            upper
        } else {
            format!("SIG{upper}")
        };

        // Linux defines these names as aliases of others.
        let alias = match full.as_str() {
            "SIGPOLL" => Some(libc::SIGPOLL),
            "SIGIOT" => Some(libc::SIGIOT),
            "SIGCLD" => Some(libc::SIGCHLD),
            _ => None,
        };

        alias
            .and_then(Self::new)
            .or_else(|| Self::all().find(|signal| signal.name() == full))
    }

    /// The signal's conventional name. Real-time signals, which C libraries
    /// number differently, are named by their number as gdb does: `SIG34`.
    pub fn name(self) -> String {
        NixSignal::try_from(self.0).map_or_else(
            |_| format!("SIG{}", self.0),
            |signal| signal.as_str().to_owned(),
        )
    }
}

#[cfg(test)]
impl Signal {
    pub const SIGSEGV: Self = Self(libc::SIGSEGV);

    pub const SIGURG: Self = Self(libc::SIGURG);

    pub const SIGUSR1: Self = Self(libc::SIGUSR1);
}

/// How every signal is handled: as the user chose, or else as the loaded
/// language runtimes use it, or else by gdb's defaults.
#[derive(Debug, Default)]
pub struct SignalPolicies {
    changed: BTreeMap<Signal, SignalPolicy>,

    /// Signals a loaded runtime handles as part of the program's own work.
    handled: BTreeSet<i32>,
}

impl SignalPolicies {
    pub fn get(&self, signal: Signal) -> SignalPolicy {
        self.changed
            .get(&signal)
            .copied()
            .unwrap_or_else(|| self.default_policy(signal))
    }

    /// Changes how `signal` is handled and returns the previous policy.
    pub fn set(&mut self, signal: Signal, policy: SignalPolicy) -> SignalPolicy {
        let previous = self.get(signal);
        if policy == self.default_policy(signal) {
            self.changed.remove(&signal);
        } else {
            self.changed.insert(signal, policy);
        }

        previous
    }

    /// Replaces the signals the loaded runtimes handle themselves, which
    /// pass silently unless the user chose otherwise.
    pub fn set_runtime_handled(&mut self, handled: impl IntoIterator<Item = i32>) {
        self.handled = handled.into_iter().collect();
    }

    fn default_policy(&self, signal: Signal) -> SignalPolicy {
        if self.handled.contains(&signal.0) {
            return QUIET_POLICY;
        }

        default_policy(signal)
    }
}

/// A signal that neither stops nor prints, and is delivered.
const QUIET_POLICY: SignalPolicy = SignalPolicy {
    stop: false,
    print: false,
    pass: true,
};

/// gdb's default handling: signals that programs use for routine work
/// neither stop nor print and are delivered, an interrupt from the
/// terminal stops without being delivered, and everything else stops and
/// is delivered.
fn default_policy(signal: Signal) -> SignalPolicy {
    const QUIET: [i32; 8] = [
        libc::SIGALRM,
        libc::SIGURG,
        libc::SIGCHLD,
        libc::SIGWINCH,
        libc::SIGPROF,
        libc::SIGVTALRM,
        libc::SIGIO,
        libc::SIGPWR,
    ];
    if QUIET.contains(&signal.0) {
        QUIET_POLICY
    } else {
        SignalPolicy {
            stop: true,
            print: true,
            pass: signal != Signal::SIGINT,
        }
    }
}

/// Describes a delivered signal from its siginfo: its name, the code
/// naming its cause, and the faulting address or the process that sent it.
pub fn describe(
    name: &str,
    number: i32,
    code: i32,
    fault_address: Option<u64>,
    sender: Option<i32>,
) -> String {
    let code =
        signal_code_name(number, code).map_or_else(|| format!("si_code {code}"), str::to_owned);
    match (fault_address, sender) {
        (Some(address), _) => format!("{name} ({code}) at {address:#x}"),
        (None, Some(sender)) if sender > 0 => format!("{name} ({code}) sent by process {sender}"),
        _ => format!("{name} ({code})"),
    }
}

fn signal_code_name(signal: i32, code: i32) -> Option<&'static str> {
    let generic = match code {
        0 => Some("SI_USER"),
        0x80 => Some("SI_KERNEL"),
        -1 => Some("SI_QUEUE"),
        -2 => Some("SI_TIMER"),
        -3 => Some("SI_MESGQ"),
        -4 => Some("SI_ASYNCIO"),
        -5 => Some("SI_SIGIO"),
        -6 => Some("SI_TKILL"),
        _ => None,
    };

    if generic.is_some() {
        return generic;
    }

    let names: &[&str] = match signal {
        libc::SIGSEGV => &["SEGV_MAPERR", "SEGV_ACCERR", "SEGV_BNDERR", "SEGV_PKUERR"],
        libc::SIGBUS => &[
            "BUS_ADRALN",
            "BUS_ADRERR",
            "BUS_OBJERR",
            "BUS_MCEERR_AR",
            "BUS_MCEERR_AO",
        ],
        libc::SIGILL => &[
            "ILL_ILLOPC",
            "ILL_ILLOPN",
            "ILL_ILLADR",
            "ILL_ILLTRP",
            "ILL_PRVOPC",
            "ILL_PRVREG",
            "ILL_COPROC",
            "ILL_BADSTK",
        ],
        libc::SIGFPE => &[
            "FPE_INTDIV",
            "FPE_INTOVF",
            "FPE_FLTDIV",
            "FPE_FLTOVF",
            "FPE_FLTUND",
            "FPE_FLTRES",
            "FPE_FLTINV",
            "FPE_FLTSUB",
        ],
        libc::SIGTRAP => &["TRAP_BRKPT", "TRAP_TRACE", "TRAP_BRANCH", "TRAP_HWBKPT"],
        _ => &[],
    };

    usize::try_from(code)
        .ok()
        .and_then(|code| code.checked_sub(1))
        .and_then(|index| names.get(index).copied())
}

impl fmt::Debug for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name())
    }
}

impl From<NixSignal> for Signal {
    fn from(signal: NixSignal) -> Self {
        Self(signal as i32)
    }
}

/// A decoded `waitpid` status for one tracee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEvent {
    /// The thread exited with this status code.
    Exited(Pid, i32),

    /// The thread was killed by a signal, possibly dumping core.
    Signaled(Pid, Signal, bool),

    /// The thread stopped for a signal.
    Stopped(Pid, Signal),

    /// The thread stopped for a ptrace event, reported with this signal.
    PtraceEvent(Pid, Signal, i32),

    /// The thread stopped at a system call while tracing system calls.
    PtraceSyscall(Pid),

    /// A stopped thread was resumed by `SIGCONT`.
    Continued(Pid),
}

impl WaitEvent {
    pub const fn pid(&self) -> Pid {
        match *self {
            Self::Exited(pid, _)
            | Self::Signaled(pid, _, _)
            | Self::Stopped(pid, _)
            | Self::PtraceEvent(pid, _, _)
            | Self::PtraceSyscall(pid)
            | Self::Continued(pid) => pid,
        }
    }

    /// Decodes a raw status `waitpid` returned for `pid`.
    pub fn decode(pid: Pid, status: i32) -> Result<Self, Errno> {
        let signal = |number| Signal::new(number).ok_or(Errno::EINVAL);
        if libc::WIFEXITED(status) {
            Ok(Self::Exited(pid, libc::WEXITSTATUS(status)))
        } else if libc::WIFSIGNALED(status) {
            Ok(Self::Signaled(
                pid,
                signal(libc::WTERMSIG(status))?,
                libc::WCOREDUMP(status),
            ))
        } else if libc::WIFSTOPPED(status) {
            let stop = libc::WSTOPSIG(status);

            // PTRACE_O_TRACESYSGOOD marks system-call stops with bit 7.
            if stop == libc::SIGTRAP | 0x80 {
                return Ok(Self::PtraceSyscall(pid));
            }

            let event = (status >> 16) & 0xff;
            if event == 0 {
                Ok(Self::Stopped(pid, signal(stop)?))
            } else {
                Ok(Self::PtraceEvent(pid, signal(stop)?, event))
            }
        } else if libc::WIFCONTINUED(status) {
            Ok(Self::Continued(pid))
        } else {
            Err(Errno::EINVAL)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_statuses_decode_every_signal_and_event() {
        let pid = Pid::from_raw(7);
        assert_eq!(
            WaitEvent::decode(pid, 3 << 8),
            Ok(WaitEvent::Exited(pid, 3))
        );
        assert_eq!(
            WaitEvent::decode(pid, libc::SIGSEGV | 0x80),
            Ok(WaitEvent::Signaled(pid, Signal::SIGSEGV, true))
        );

        // A real-time signal, which nix cannot represent.
        assert_eq!(
            WaitEvent::decode(pid, (35 << 8) | 0x7f),
            Ok(WaitEvent::Stopped(pid, Signal::new(35).expect("real-time")))
        );
        assert_eq!(
            WaitEvent::decode(
                pid,
                (libc::PTRACE_EVENT_CLONE << 16) | (libc::SIGTRAP << 8) | 0x7f
            ),
            Ok(WaitEvent::PtraceEvent(
                pid,
                Signal::SIGTRAP,
                libc::PTRACE_EVENT_CLONE
            ))
        );
        assert_eq!(
            WaitEvent::decode(pid, ((libc::SIGTRAP | 0x80) << 8) | 0x7f),
            Ok(WaitEvent::PtraceSyscall(pid))
        );
        assert_eq!(
            WaitEvent::decode(pid, 0xffff),
            Ok(WaitEvent::Continued(pid))
        );
    }
}
