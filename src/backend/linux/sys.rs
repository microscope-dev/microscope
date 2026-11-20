use std::io;
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

use nix::errno::Errno;
use nix::libc;
use nix::sys::ptrace;
use nix::unistd::Pid;

use super::arch::FLOATING_REGISTERS_SIZE;

const PTRACE_GET_SYSCALL_INFO: u16 = 0x420e;

#[derive(Default)]
pub(super) struct BeforeExec {
    pub dies_with_parent: bool,

    pub fixed_addresses: bool,

    pub gate: Option<Gate>,

    pub filter: Option<Vec<libc::sock_filter>>,

    pub traced: bool,
}

#[derive(Clone, Copy)]
pub(super) struct Gate {
    pub pid: RawFd,
    pub go_read: RawFd,
    pub go_write: RawFd,
}

#[allow(
    unsafe_code,
    reason = "pre_exec is the only way to act in the child between fork and exec"
)]
pub(super) fn before_exec(command: &mut Command, plan: BeforeExec) {
    let parent = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    let pause = gate_pause();

    // SAFETY: after fork, `in_child` makes only async-signal-safe system
    // calls on the descriptors the child inherited and on the plan built
    // before the fork, and builds errors from fixed numbers. It does not
    // allocate.
    unsafe {
        command.pre_exec(move || in_child(&plan, parent, pause));
    }
}

fn in_child(
    plan: &BeforeExec,
    parent: i32,
    pause: Option<rustix::thread::Timespec>,
) -> io::Result<()> {
    if plan.dies_with_parent {
        rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::KILL))?;
        if rustix::process::getppid().map(|pid| pid.as_raw_nonzero().get()) != Some(parent) {
            return Err(io::Error::from_raw_os_error(libc::ECHILD));
        }
    }

    if plan.fixed_addresses {
        super::native::disable_address_randomization();
    }

    if let Some(gate) = plan.gate {
        wait_at(gate, pause)?;
    }

    if let Some(filter) = &plan.filter {
        rustix::thread::set_no_new_privs(true)?;
        install_filter(filter)?;
    }

    if plan.traced {
        nix::sys::ptrace::traceme().map_err(|error| io::Error::from_raw_os_error(error as i32))?;
    }

    Ok(())
}

#[allow(
    unsafe_code,
    reason = "a forked child names the descriptors it inherited by number"
)]
fn wait_at(gate: Gate, pause: Option<rustix::thread::Timespec>) -> io::Result<()> {
    // SAFETY: the parent keeps the gate's descriptors open until the child
    // execs, so each number names its pipe end in the child; `go_write` is
    // the child's own copy, which nothing else in the child closes.
    let (pid, go_read, go_write) = unsafe {
        (
            BorrowedFd::borrow_raw(gate.pid),
            BorrowedFd::borrow_raw(gate.go_read),
            OwnedFd::from_raw_fd(gate.go_write),
        )
    };

    let me = rustix::process::getpid()
        .as_raw_nonzero()
        .get()
        .to_le_bytes();
    if rustix::io::write(pid, &me) != Ok(me.len()) {
        return Err(io::Error::from_raw_os_error(libc::EPIPE));
    }

    if let Some(pause) = pause {
        let _ = rustix::thread::nanosleep(&pause);
    }

    drop(go_write);
    let mut go = [0_u8];
    if rustix::io::read(go_read, &mut go) != Ok(1) {
        return Err(io::Error::from_raw_os_error(libc::ECANCELED));
    }

    Ok(())
}

// Stretches the moment after a child has said its pid and before it waits at
// its gate, where a slow machine can deschedule it, so tests can make that
// happen every time.
#[cfg(debug_assertions)]
const GATE_PAUSE_MS: &str = "MICROSCOPE_TEST_GATE_PAUSE_MS";

#[cfg(debug_assertions)]
fn gate_pause() -> Option<rustix::thread::Timespec> {
    let ms: i64 = std::env::var(GATE_PAUSE_MS).ok()?.parse().ok()?;
    Some(rustix::thread::Timespec {
        tv_sec: ms / 1000,
        tv_nsec: (ms % 1000) * 1_000_000,
    })
}

#[cfg(not(debug_assertions))]
const fn gate_pause() -> Option<rustix::thread::Timespec> {
    None
}

#[allow(unsafe_code, reason = "neither nix nor rustix wraps seccomp")]
fn install_filter(filter: &[libc::sock_filter]) -> io::Result<()> {
    let program = libc::sock_fprog {
        len: u16::try_from(filter.len()).unwrap_or(u16::MAX),
        filter: filter.as_ptr().cast_mut(),
    };

    // SAFETY: the kernel reads the program and its instructions, which
    // outlive the call, and copies them before it returns.
    let installed = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            0,
            std::ptr::from_ref(&program),
        )
    };

    if installed == -1 {
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

#[allow(unsafe_code, reason = "a raw ptrace request")]
pub(super) fn ptrace_with_signal(
    request: ptrace::Request,
    pid: Pid,
    signal: Option<i32>,
) -> Result<(), Errno> {
    let data = match signal {
        None => 0,
        Some(signal) => usize::try_from(signal).map_err(|_| Errno::EINVAL)?,
    };

    // SAFETY: continuing, single-stepping, and detaching ignore `addr` and
    // read no memory; `data` carries only the signal number to deliver.
    let result = unsafe {
        libc::ptrace(
            request as _,
            pid.as_raw(),
            std::ptr::null_mut::<libc::c_void>(),
            std::ptr::without_provenance_mut::<libc::c_void>(data),
        )
    };

    Errno::result(result).map(drop)
}

#[allow(unsafe_code, reason = "a raw ptrace request")]
pub(super) fn siginfo(pid: Pid) -> Result<[u8; 128], Errno> {
    let mut buffer = [0_u8; 128];

    // SAFETY: PTRACE_GETSIGINFO writes one siginfo_t, 128 bytes on every
    // Linux architecture, into the buffer, which outlives the call.
    let result = unsafe {
        libc::ptrace(
            libc::PTRACE_GETSIGINFO,
            pid.as_raw(),
            std::ptr::null_mut::<libc::c_void>(),
            buffer.as_mut_ptr().cast::<libc::c_void>(),
        )
    };

    Errno::result(result).map(|_| buffer)
}

#[allow(unsafe_code, reason = "a raw ptrace request")]
pub(super) fn syscall_info(pid: Pid) -> Result<[u8; 88], Errno> {
    let mut buffer = [0_u8; 88];

    // SAFETY: the kernel writes at most `buffer.len()` bytes, the size the
    // request passes, into the buffer, which outlives the call.
    let result = unsafe {
        libc::ptrace(
            PTRACE_GET_SYSCALL_INFO.into(),
            pid.as_raw(),
            std::ptr::without_provenance_mut::<libc::c_void>(buffer.len()),
            buffer.as_mut_ptr().cast::<libc::c_void>(),
        )
    };

    Errno::result(result).map(|_| buffer)
}

#[allow(unsafe_code, reason = "a raw ptrace request")]
pub(super) fn floating_registers(pid: Pid) -> Result<[u8; FLOATING_REGISTERS_SIZE], Errno> {
    let mut buffer = [0_u8; FLOATING_REGISTERS_SIZE];

    // SAFETY: PTRACE_GETFPREGS writes one 512-byte FXSAVE image on x86-64
    // into the buffer, which is that size and outlives the call.
    let result = unsafe {
        libc::ptrace(
            ptrace::Request::PTRACE_GETFPREGS as _,
            pid.as_raw(),
            std::ptr::null_mut::<libc::c_void>(),
            buffer.as_mut_ptr().cast::<libc::c_void>(),
        )
    };

    Errno::result(result).map(|_| buffer)
}

#[allow(unsafe_code, reason = "neither nix nor rustix wraps tgkill")]
pub(super) fn tgkill(process: Pid, thread: Pid, signal: i32) -> Result<(), Errno> {
    // SAFETY: tgkill takes three integers, passed as the `long`s the
    // variadic call reads, and reads no memory.
    let result = unsafe {
        libc::syscall(
            libc::SYS_tgkill,
            libc::c_long::from(process.as_raw()),
            libc::c_long::from(thread.as_raw()),
            libc::c_long::from(signal),
        )
    };
    Errno::result(result).map(drop)
}
