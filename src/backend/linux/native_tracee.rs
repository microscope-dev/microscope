//! A real traced process, which the simulator's conformance tests drive
//! beside the simulated kernel.

use std::path::Path;

use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;

use super::LinuxError;
use super::native::{InspectionOps as _, LinuxTraceOps as _};
use super::signals::{Signal, WaitEvent};
use super::sim_edge::{SETTABLE_FLAGS, apply_user_registers, user_registers};
use crate::Error;
use crate::protocol::LaunchOptions;
use crate::sim::cpu::Registers;
use crate::sim::kernel::ptrace::CallInfo;
use crate::sim::kernel::{Tid, WaitStatus};

/// A real traced process, for tests that compare the simulation with
/// Linux. Requests name the thread they address. Every thread is killed and
/// reaped when the tracee is dropped.
pub struct NativeTracee {
    ptrace: super::native::LinuxPtrace,
    pid: Pid,

    /// Perf breakpoints holding some of a thread's hardware breakpoints.
    held: Vec<std::os::fd::OwnedFd>,

    /// The events sampling the process at a breakpoint, as the profiler
    /// opens them.
    sampling: Option<super::perf::Events>,

    /// Children the process forked, killed with it.
    forked: Vec<Pid>,

    /// Whether the process was left to its parent rather than killed.
    abandoned: bool,
}

#[expect(
    clippy::unused_self,
    reason = "requests about a thread go through the tracee it belongs to"
)]
impl NativeTracee {
    /// Launches `executable` traced, as the controller does, and returns
    /// once it reported its first stop, which is returned too. Its
    /// environment is empty, as a simulated program's is.
    #[must_use]
    pub fn spawn(executable: &Path, arguments: &[String]) -> (Self, WaitStatus) {
        let ptrace = super::native::LinuxPtrace::new();
        let options = LaunchOptions {
            arguments: arguments.iter().map(Into::into).collect(),
            environment: std::env::vars_os().map(|(name, _)| (name, None)).collect(),
            stdout: Some(std::process::Stdio::null()),
            ..LaunchOptions::default()
        };

        let pid = ptrace
            .spawn(executable, options)
            .expect("spawn a traced program");
        let tracee = Self {
            ptrace,
            pid,
            held: Vec::new(),
            sampling: None,
            forked: Vec::new(),
            abandoned: false,
        };

        let first = tracee.wait(tracee.pid());
        (tracee, first)
    }

    /// Launches `executable` traced as [`Self::spawn`] does, except that it
    /// outlives the thread that traces it, as a program attached to does,
    /// rather than dying with it.
    #[must_use]
    pub fn spawn_outliving_tracer(executable: &Path, arguments: &[String]) -> (Self, WaitStatus) {
        let mut command = std::process::Command::new(executable);
        command
            .args(arguments)
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null());
        super::sys::before_exec(
            &mut command,
            super::sys::BeforeExec {
                fixed_addresses: true,
                traced: true,
                ..Default::default()
            },
        );

        #[expect(
            clippy::zombie_processes,
            reason = "the tracee waits for its process with waitpid, and its drop reaps it"
        )]
        let child = command.spawn().expect("spawn a traced program");
        let tracee = Self {
            ptrace: super::native::LinuxPtrace::new(),
            pid: Pid::from_raw(i32::try_from(child.id()).expect("a process id fits i32")),
            held: Vec::new(),
            sampling: None,
            forked: Vec::new(),
            abandoned: false,
        };

        let first = tracee.wait(tracee.pid());
        (tracee, first)
    }

    /// The process, which is its first thread.
    #[must_use]
    pub const fn pid(&self) -> Tid {
        self.pid.as_raw()
    }

    /// Waits for `thread`'s next status, failing after ten seconds.
    #[must_use]
    pub fn wait(&self, thread: Tid) -> WaitStatus {
        let started = std::time::Instant::now();
        let event = loop {
            match super::native::wait_for(Pid::from_raw(thread), libc::__WALL | libc::WNOHANG) {
                Ok(Some(event)) => break event,
                Ok(None) | Err(Errno::EINTR) => {
                    let waited = started.elapsed();
                    assert!(
                        waited < std::time::Duration::from_secs(10),
                        "{thread} reported nothing for ten seconds"
                    );

                    // A single step reports within microseconds.
                    if waited < std::time::Duration::from_millis(1) {
                        std::thread::yield_now();
                    } else {
                        std::thread::sleep(std::time::Duration::from_micros(100));
                    }
                }
                Err(errno) => panic!("waiting for {thread} failed: {errno}"),
            }
        };

        match event {
            WaitEvent::Exited(pid, code) => WaitStatus::Exited(pid.as_raw(), code),
            WaitEvent::Signaled(pid, signal, core) => {
                WaitStatus::Signaled(pid.as_raw(), signal.number(), core)
            }
            WaitEvent::Stopped(pid, signal) => WaitStatus::Stopped(pid.as_raw(), signal.number()),
            WaitEvent::PtraceEvent(pid, signal, libc::PTRACE_EVENT_STOP)
                if signal != Signal::SIGTRAP =>
            {
                WaitStatus::GroupStop(pid.as_raw(), signal.number())
            }
            WaitEvent::PtraceEvent(pid, _, event) => WaitStatus::Event(pid.as_raw(), event),
            WaitEvent::PtraceSyscall(pid) => WaitStatus::Syscall(pid.as_raw()),
            other @ WaitEvent::Continued(_) => panic!("unexpected status {other:?}"),
        }
    }

    /// Whether `thread` has a status to report now, which is not reaped.
    #[must_use]
    pub fn has_report(&self, thread: Tid) -> bool {
        use rustix::process::{WaitId, WaitIdOptions};
        let thread = rustix::process::Pid::from_raw(thread).expect("a thread id");
        let options = WaitIdOptions::EXITED
            | WaitIdOptions::STOPPED
            | WaitIdOptions::NOHANG
            | WaitIdOptions::NOWAIT
            | WaitIdOptions::from_bits_retain(libc::__WALL.cast_unsigned());

        rustix::process::waitid(WaitId::Pid(thread), options)
            .unwrap_or_else(|error| panic!("waitid for {thread} failed: {error}"))
            .is_some()
    }

    pub fn registers(&self, thread: Tid) -> std::result::Result<Registers, Errno> {
        let user = self
            .ptrace
            .registers(Pid::from_raw(thread))
            .map_err(errno_of)?;
        let mut registers = Registers::default();
        apply_user_registers(&mut registers, &user);
        registers.rflags = user.eflags;

        Ok(registers)
    }

    /// `orig_rax`: the system call a thread stopped inside, or -1.
    pub fn system_call(&self, thread: Tid) -> std::result::Result<u64, Errno> {
        self.ptrace
            .registers(Pid::from_raw(thread))
            .map(|user| user.orig_rax)
            .map_err(errno_of)
    }

    pub fn set_registers(
        &self,
        thread: Tid,
        registers: &Registers,
    ) -> std::result::Result<(), Errno> {
        let mut user = self
            .ptrace
            .registers(Pid::from_raw(thread))
            .map_err(errno_of)?;
        let flags = user.eflags;
        user = libc::user_regs_struct {
            orig_rax: user.orig_rax,
            ..user_registers(registers, crate::sim::kernel::NO_SYSTEM_CALL)
        };

        user.eflags = (flags & !SETTABLE_FLAGS) | (registers.rflags & SETTABLE_FLAGS);

        self.ptrace
            .set_registers(Pid::from_raw(thread), user)
            .map_err(errno_of)
    }

    /// `PTRACE_GETSIGINFO`, as the controller sees it.
    pub fn signal_view(
        &self,
        thread: Tid,
    ) -> std::result::Result<(i32, i32, Option<i32>, Option<u64>), Errno> {
        let bytes = super::sys::siginfo(Pid::from_raw(thread))?;
        let (number, metadata) = super::native::decode_siginfo(&bytes);

        Ok((
            number,
            metadata.code,
            metadata.sender,
            metadata.fault_address,
        ))
    }

    pub fn event_message(&self, thread: Tid) -> std::result::Result<u64, Errno> {
        self.ptrace
            .event_message(Pid::from_raw(thread))
            .map(i64::cast_unsigned)
            .map_err(errno_of)
    }

    /// Leaves the process to its parent, as a tracer that exits does:
    /// nothing is killed or reaped. Returns the process.
    #[must_use]
    pub fn abandon(mut self) -> Tid {
        self.abandoned = true;
        self.pid()
    }

    /// Kills `child`, which the process forked, with it.
    pub fn adopt(&mut self, child: Tid) {
        self.forked.push(Pid::from_raw(child));
    }

    /// `PTRACE_SEIZE` with the options the controller seizes with, which
    /// leaves the thread running.
    pub fn seize(&self, thread: Tid) -> std::result::Result<(), Errno> {
        nix::sys::ptrace::seize(Pid::from_raw(thread), super::native::trace_options(false))
    }

    /// A register read and a seize of `thread` from a thread that traces
    /// nothing, as another tracer makes them.
    pub fn as_another_tracer(
        &self,
        thread: Tid,
    ) -> (
        std::result::Result<(), Errno>,
        std::result::Result<(), Errno>,
    ) {
        let pid = Pid::from_raw(thread);
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let read = nix::sys::ptrace::getregs(pid).map(drop);
                    let seize = nix::sys::ptrace::seize(pid, super::native::trace_options(false));
                    (read, seize)
                })
                .join()
                .expect("the other tracer's thread finishes")
        })
    }

    /// `PTRACE_INTERRUPT`.
    pub fn interrupt(&self, thread: Tid) -> std::result::Result<(), Errno> {
        nix::sys::ptrace::interrupt(Pid::from_raw(thread))
    }

    /// `PTRACE_DETACH`, delivering `signal`.
    pub fn detach(&self, thread: Tid, signal: Option<i32>) -> std::result::Result<(), Errno> {
        let signal =
            signal.map(|number| nix::sys::signal::Signal::try_from(number).expect("a real signal"));
        nix::sys::ptrace::detach(Pid::from_raw(thread), signal)
    }

    /// The children `thread` forked and its process has not reaped, as
    /// `/proc/<thread>/task/<thread>/children` lists them.
    #[must_use]
    pub fn children(&self, thread: Tid) -> Vec<Tid> {
        std::fs::read_to_string(format!("/proc/{thread}/task/{thread}/children"))
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|child| child.parse().ok())
            .collect()
    }

    pub fn set_options(&self, thread: Tid, exit_kill: bool) -> std::result::Result<(), Errno> {
        self.ptrace
            .set_options(Pid::from_raw(thread), exit_kill)
            .map_err(errno_of)
    }

    /// `PTRACE_SYSCALL`, with `signal`.
    pub fn resume_to_call(
        &self,
        thread: Tid,
        signal: Option<i32>,
    ) -> std::result::Result<(), Errno> {
        let signal = signal.map(|number| Signal::new(number).expect("a real signal"));
        self.ptrace
            .continue_to_syscall(Pid::from_raw(thread), signal)
            .map_err(errno_of)
    }

    /// `PTRACE_GET_SYSCALL_INFO`, with only what each kind of stop reports.
    pub fn call_info(&self, thread: Tid) -> std::result::Result<CallInfo, Errno> {
        let info = self.ptrace.syscall_info(Pid::from_raw(thread))?;
        let entry = info.op == super::syscalls::info::OP_ENTRY;

        Ok(CallInfo {
            op: info.op,
            number: if entry { info.number } else { 0 },
            args: if entry { info.args } else { [0; 6] },
            value: if info.op == super::syscalls::info::OP_EXIT {
                info.value
            } else {
                0
            },
        })
    }

    pub fn resume(
        &self,
        thread: Tid,
        signal: Option<i32>,
        single_step: bool,
    ) -> std::result::Result<(), Errno> {
        let signal = signal.map(|number| Signal::new(number).expect("a real signal"));

        if single_step {
            self.ptrace.step(Pid::from_raw(thread), signal)
        } else {
            self.ptrace
                .continue_execution(Pid::from_raw(thread), signal)
        }
        .map_err(errno_of)
    }

    /// `kill(2)` of the whole process.
    pub fn kill(&self, signal: i32) -> std::result::Result<(), Errno> {
        self.signal_process(self.pid(), signal)
    }

    /// `kill(process, signal)`.
    pub fn signal_process(&self, process: Tid, signal: i32) -> std::result::Result<(), Errno> {
        nix::sys::signal::kill(
            Pid::from_raw(process),
            nix::sys::signal::Signal::try_from(signal)?,
        )
    }

    /// The tracer's `tgkill(SIGSTOP)` of one thread of the process.
    pub fn request_stop(&self, thread: Tid) -> std::result::Result<(), Errno> {
        self.request_stop_of(self.pid(), thread)
    }

    /// The tracer's `tgkill(SIGSTOP)` of one thread of `process`.
    pub fn request_stop_of(&self, process: Tid, thread: Tid) -> std::result::Result<(), Errno> {
        self.ptrace
            .request_stop(Pid::from_raw(process), Pid::from_raw(thread))
            .map_err(errno_of)
    }

    /// Whether `thread` is in a job-control stop, untraced: `/proc` shows
    /// `T`, where a ptrace-stop shows `t`.
    #[must_use]
    pub fn job_stopped(&self, thread: Tid) -> bool {
        super::proc_status::read_path(format!("/proc/{thread}/stat"))
            .is_ok_and(|stat| super::proc_status::stat_state(&stat) == Some(b'T'))
    }

    pub fn peek(&self, thread: Tid, address: u64) -> std::result::Result<u64, Errno> {
        nix::sys::ptrace::read(
            Pid::from_raw(thread),
            address as nix::sys::ptrace::AddressType,
        )
        .map(i64::cast_unsigned)
    }

    pub fn poke(&self, thread: Tid, address: u64, value: u64) -> std::result::Result<(), Errno> {
        self.ptrace
            .write_word(Pid::from_raw(thread), address, value)
            .map_err(errno_of)
    }

    /// `PTRACE_PEEKUSER` of a debug register.
    pub fn read_debug_register(
        &self,
        thread: Tid,
        index: usize,
    ) -> std::result::Result<u64, Errno> {
        self.ptrace
            .read_debug_register(Pid::from_raw(thread), index)
    }

    /// `PTRACE_POKEUSER` of a debug register.
    pub fn write_debug_register(
        &self,
        thread: Tid,
        index: usize,
        value: u64,
    ) -> std::result::Result<(), Errno> {
        self.ptrace
            .write_debug_register(Pid::from_raw(thread), index, value)
    }

    /// Holds `count` of `thread`'s hardware breakpoints with disabled perf
    /// breakpoints watching `address`, until the tracee is dropped.
    /// Samples every thread of the process each time it reaches `address`,
    /// copying `stack_bytes` of stack, through the profiler's events.
    pub fn sample_at(&mut self, address: u64, stack_bytes: u32) {
        let options = super::perf::EventOptions {
            source: super::perf::Source::Breakpoint { address },
            stack_bytes,
            ring_pages: 64,
            switches: false,
        };

        let pid = self.pid.as_raw().cast_unsigned();
        let events = super::perf::Events::attach(pid, &options, 4096).expect("open perf events");
        self.sampling = Some(events);
    }

    /// The records the events wrote since the last call.
    pub fn samples(&mut self) -> Vec<u8> {
        let mut records = Vec::new();

        if let Some(events) = &mut self.sampling {
            events.drain(&mut records);
        }

        records
    }

    pub fn hold_debug_slots(&mut self, thread: Tid, count: usize, address: u64) {
        use super::perf::sys;
        let attributes = sys::Attr {
            kind: sys::TYPE_BREAKPOINT,
            size: u32::try_from(std::mem::size_of::<sys::Attr>()).expect("small"),
            flags: sys::flag::DISABLED | sys::flag::EXCLUDE_KERNEL | sys::flag::EXCLUDE_HV,
            bp_type: sys::BREAKPOINT_WRITE,
            bp_addr: address,
            bp_len: 8,
            ..sys::Attr::default()
        };

        for _ in 0..count {
            let fd = sys::open(&attributes, thread, -1)
                .unwrap_or_else(|error| panic!("perf_event_open failed: {error}"));
            self.held.push(fd);
        }
    }

    /// The thread group `/proc` names for `thread`, or `None` once it is
    /// gone.
    #[must_use]
    pub fn thread_group(&self, thread: Tid) -> Option<Tid> {
        match super::native::thread_group_id(Pid::from_raw(thread)) {
            Ok(group) => Some(group.as_raw()),
            Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("reading {thread}'s thread group failed: {error}"),
        }
    }

    /// The process's threads `/proc` lists, which includes zombies not yet
    /// reaped.
    #[must_use]
    pub fn threads(&self) -> Vec<Tid> {
        super::native::process_threads(self.pid)
            .map(|threads| threads.into_iter().map(Pid::as_raw).collect())
            .unwrap_or_default()
    }

    /// `/proc/<thread>/maps`, or `None` once it cannot be read.
    #[must_use]
    pub fn maps(&self, thread: Tid) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{thread}/maps")).ok()
    }

    /// Reads the memory of `process`, the program or a child it forked,
    /// through `/proc/<pid>/mem`, or `None` where the kernel refuses.
    #[must_use]
    pub fn read_memory(&self, process: Tid, address: u64, length: usize) -> Option<Vec<u8>> {
        use std::os::unix::fs::FileExt as _;
        let file = std::fs::File::open(format!("/proc/{process}/mem")).ok()?;
        let mut bytes = vec![0; length];
        file.read_exact_at(&mut bytes, address).ok()?;
        Some(bytes)
    }
}

fn errno_of(error: Error) -> Errno {
    match error {
        Error::Backend(error) => match error.downcast_ref::<LinuxError>() {
            Some(LinuxError::System(errno)) => *errno,
            _ => panic!("not a system error: {error}"),
        },
        other => panic!("not a system error: {other}"),
    }
}

impl Drop for NativeTracee {
    fn drop(&mut self) {
        self.held.clear();
        if self.abandoned {
            return;
        }

        for &child in &self.forked {
            let _ = nix::sys::signal::kill(child, nix::sys::signal::Signal::SIGKILL);
            let started = std::time::Instant::now();

            // A child still traced is reaped here; a detached one is its
            // parent's to reap.
            while !matches!(
                super::native::wait_for(child, libc::__WALL | libc::WNOHANG),
                Ok(Some(WaitEvent::Exited(..) | WaitEvent::Signaled(..))) | Err(_)
            ) {
                let _ = self.ptrace.continue_execution(child, None);
                assert!(
                    started.elapsed() < std::time::Duration::from_secs(10),
                    "{child} outlived its test"
                );
                std::thread::yield_now();
            }
        }

        let _ = nix::sys::signal::kill(self.pid, nix::sys::signal::Signal::SIGKILL);

        // Reap every thread, so nothing outlives the test. A thread in a
        // stop, even one already reported such as its exit event, waits
        // there until it is continued, and the leader's exit is reported
        // only once every other thread is reaped.
        let started = std::time::Instant::now();

        loop {
            let mut threads = self.threads();
            if !threads.contains(&self.pid()) {
                threads.push(self.pid());
            }

            for thread in threads {
                let thread = Pid::from_raw(thread);
                let _ = self.ptrace.continue_execution(thread, None);
                match super::native::wait_for(thread, libc::__WALL | libc::WNOHANG) {
                    Ok(Some(WaitEvent::Exited(..) | WaitEvent::Signaled(..)))
                        if thread == self.pid =>
                    {
                        return;
                    }
                    Err(Errno::ECHILD) if thread == self.pid => return,
                    _ => {}
                }
            }

            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "{} outlived its test",
                self.pid
            );
            std::thread::yield_now();
        }
    }
}
