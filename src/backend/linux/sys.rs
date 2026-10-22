//! The system calls microscope makes with no safe wrapper: what a launched
//! child does between fork and exec, the ptrace requests nix cannot make,
//! and `tgkill`. The rest of the backend reaches them through the safe
//! functions here.

use std::io;
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;

use nix::errno::Errno;
use nix::libc;
use nix::sys::ptrace;
use nix::unistd::Pid;

use super::core_dump::FXSAVE_SIZE;

// The kernel's numbers for the requests nix's `Request` lacks with musl.
// glibc declares requests unsigned and musl signed, so they convert to
// whichever `libc::ptrace` takes.
const PTRACE_GET_SYSCALL_INFO: u16 = 0x420e;

/// What a launched child does between fork and exec, in this order.
#[derive(Default)]
pub(super) struct BeforeExec {
    /// Die with the thread that launched it, and fail if its process has
    /// already exited.
    pub dies_with_parent: bool,

    /// Run without address space randomization.
    pub fixed_addresses: bool,

    /// Wait at a gate before going on.
    pub gate: Option<Gate>,

    /// A seccomp filter to install. It is built before the fork, because
    /// the child may not allocate.
    pub filter: Option<Vec<libc::sock_filter>>,

    /// Ask its parent to trace it.
    pub traced: bool,
}

/// The pipes a child waits at: it writes its process ID to `pid`, closes
/// its copy of `go_write`, which only the parent may hold open, and waits
/// for one byte from `go_read`. A parent that closes `go_write` instead
/// cancels the launch.
///
/// The parent keeps the descriptors open until the child has exec'd or
/// failed, and the child inherits them.
#[derive(Clone, Copy)]
pub(super) struct Gate {
    pub pid: RawFd,
    pub go_read: RawFd,
    pub go_write: RawFd,
}

/// Makes `command`'s child do what `plan` says between fork and exec.
#[allow(
    unsafe_code,
    reason = "pre_exec is the only way to act in the child between fork and exec"
)]
pub(super) fn before_exec(command: &mut Command, plan: BeforeExec) {
    let parent = i32::try_from(std::process::id()).unwrap_or(i32::MAX);

    // SAFETY: after fork, `in_child` makes only async-signal-safe system
    // calls on the descriptors the child inherited and on the plan built
    // before the fork, and builds errors from fixed numbers. It does not
    // allocate.
    unsafe {
        command.pre_exec(move || in_child(&plan, parent));
    }
}

fn in_child(plan: &BeforeExec, parent: i32) -> io::Result<()> {
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
        wait_at(gate)?;
    }

    if let Some(filter) = &plan.filter {
        // An unprivileged process may install a filter only once it can
        // gain no privileges by executing.
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
fn wait_at(gate: Gate) -> io::Result<()> {
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

    drop(go_write);
    let mut go = [0_u8];
    if rustix::io::read(go_read, &mut go) != Ok(1) {
        return Err(io::Error::from_raw_os_error(libc::ECANCELED));
    }

    Ok(())
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

/// Resumes or detaches a stopped tracee with `request`, delivering
/// `signal` to it. nix's wrappers cannot deliver real-time signals.
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

/// A stopped tracee's siginfo, as the 128 bytes the kernel wrote, so no
/// field is read through libc's union.
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

/// What `PTRACE_GET_SYSCALL_INFO` reports of a tracee stopped at a system
/// call, as bytes, with any the kernel did not write left zero.
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

/// A stopped tracee's x87 and SSE registers, as the FXSAVE image the kernel
/// wrote, so the layout is the kernel's and not a C library's.
#[allow(unsafe_code, reason = "a raw ptrace request")]
pub(super) fn floating_registers(pid: Pid) -> Result<[u8; FXSAVE_SIZE], Errno> {
    let mut buffer = [0_u8; FXSAVE_SIZE];

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

/// Sends `signal` to one thread of `process`.
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
