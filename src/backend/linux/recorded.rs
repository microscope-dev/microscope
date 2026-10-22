//! Records the native control calls a development build makes.
//!
//! [`Recorded`] wraps the ptrace edge and records every call that controls a
//! tracee or reads how it stopped. A breakpoint operation is one line, not
//! the word writes it makes. Memory, register, and `/proc` reads are left
//! out: they are frequent, change nothing, and would crowd the record of
//! what the debugger did out of the ring. Wait statuses are recorded where
//! they are consumed, in `native::wait_for`.

use std::collections::BTreeMap;
use std::fmt::{self, Arguments, Debug, Display, Formatter};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;
use tokio::sync::mpsc;

use crate::backend::{ControllerMessage, FileIdentity};
use crate::debug_info::DebugInfo;
use crate::protocol::{LaunchOptions, StopId};
use crate::{Result, VirtualAddress};

use super::memory::MemoryAccessError;
use super::modules::{ModuleMapping, ProcessMappings};
use super::native::{InspectionOps, LinuxTraceOps, SignalQueue};
use super::registers::Fxsave;
use super::signals::{Signal, WaitEvent};
use super::tls::TlsModule;
use super::{BreakpointOwner, BreakpointSite, SignalMetadata, Waiter};

pub(super) struct Recorded<P>(pub(super) P);

/// Records a call that acts on a tracee before making it, since a wait
/// status it causes may be recorded before it returns, then records its
/// result unless it is `expected`.
fn issued<T: Debug + PartialEq, E: Debug>(
    call: Arguments<'_>,
    expected: &T,
    make: impl FnOnce() -> std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    record!("{call}");
    let result = make();
    match &result {
        Ok(value) if value == expected => {}
        Ok(value) => record!("{call} -> {value:?}"),
        Err(error) => record!("{call} -> error {error:?}"),
    }

    result
}

/// Records a call that only reads, with its result.
fn queried<T: Debug, E: Debug>(
    call: Arguments<'_>,
    result: std::result::Result<T, E>,
) -> std::result::Result<T, E> {
    match &result {
        Ok(value) => record!("{call} -> {value:?}"),
        Err(error) => record!("{call} -> error {error:?}"),
    }

    result
}

/// Describes a wait status, naming its ptrace event.
pub(super) struct Described<'a>(pub(super) &'a WaitEvent);

impl Display for Described<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self.0 {
            WaitEvent::Exited(pid, code) => write!(f, "{pid} exited {code}"),
            WaitEvent::Signaled(pid, signal, core) => {
                let core = if core { ", core dumped" } else { "" };
                write!(f, "{pid} killed by {signal}{core}")
            }
            WaitEvent::Stopped(pid, signal) => write!(f, "{pid} stopped by {signal}"),
            WaitEvent::PtraceEvent(pid, signal, event) => {
                let name = match event {
                    libc::PTRACE_EVENT_FORK => "FORK",
                    libc::PTRACE_EVENT_VFORK => "VFORK",
                    libc::PTRACE_EVENT_CLONE => "CLONE",
                    libc::PTRACE_EVENT_EXEC => "EXEC",
                    libc::PTRACE_EVENT_VFORK_DONE => "VFORK_DONE",
                    libc::PTRACE_EVENT_EXIT => "EXIT",
                    libc::PTRACE_EVENT_SECCOMP => "SECCOMP",
                    libc::PTRACE_EVENT_STOP => "STOP",
                    _ => {
                        return write!(f, "{pid} stopped by ptrace event {event} with {signal}");
                    }
                };

                write!(f, "{pid} PTRACE_EVENT_{name} with {signal}")
            }
            WaitEvent::PtraceSyscall(pid) => write!(f, "{pid} stopped at a syscall"),
            WaitEvent::Continued(pid) => write!(f, "{pid} continued"),
        }
    }
}

impl<P: InspectionOps> InspectionOps for Recorded<P> {
    fn target_thread_id(
        &self,
        process: Pid,
        thread: Pid,
    ) -> Option<crate::runtime_model::TargetThreadId> {
        self.0.target_thread_id(process, thread)
    }

    fn target_thread_ids(
        &self,
        process: Pid,
        threads: &[Pid],
    ) -> Vec<(Pid, crate::runtime_model::TargetThreadId)> {
        self.0.target_thread_ids(process, threads)
    }

    fn read_word(&self, pid: Pid, address: u64) -> Result<u64> {
        self.0.read_word(pid, address)
    }

    fn read_memory_word(
        &self,
        pid: Pid,
        address: u64,
    ) -> std::result::Result<u64, MemoryAccessError> {
        self.0.read_memory_word(pid, address)
    }

    fn read_block(&self, pid: Pid, address: u64, size: usize) -> Option<Vec<u8>> {
        self.0.read_block(pid, address, size)
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct> {
        self.0.registers(pid)
    }

    fn floating_registers(&self, pid: Pid) -> Result<Fxsave> {
        self.0.floating_registers(pid)
    }

    fn tls_address(
        &self,
        thread: Pid,
        module: TlsModule,
        offset: u64,
    ) -> std::result::Result<VirtualAddress, Arc<str>> {
        self.0.tls_address(thread, module, offset)
    }
}

impl<P: LinuxTraceOps> LinuxTraceOps for Recorded<P> {
    fn agent_files(
        &self,
        pid: Pid,
        library: &[u8],
    ) -> std::result::Result<Box<dyn super::agent_files::AgentFiles>, Arc<str>> {
        record!("create runtime agent files for {pid}");
        self.0.agent_files(pid, library)
    }

    fn python_launch(
        &self,
        executable: &Path,
        environment: &mut Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
    ) -> std::result::Result<Option<super::python_launch::PythonLaunch>, Arc<str>> {
        let launch = self.0.python_launch(executable, environment);
        record!("python launch bootstrap: {launch:?}");
        launch
    }

    fn spawn(&self, executable: &Path, options: LaunchOptions) -> Result<Pid> {
        record!("spawn {} {:?}", executable.display(), options.arguments);
        let pid = self.0.spawn(executable, options);
        match &pid {
            Ok(pid) => record!("spawned {pid}"),
            Err(error) => record!("spawn -> error {error:?}"),
        }

        pid
    }

    fn spawn_waiter(
        &self,
        process: Pid,
        messages: mpsc::Sender<ControllerMessage>,
    ) -> Result<Waiter> {
        let waiter = self.0.spawn_waiter(process, messages);
        match &waiter {
            Ok(_) => record!("spawned the waiter"),
            Err(error) => record!("spawn waiter -> error {error:?}"),
        }

        waiter
    }

    fn traced_children(&self, process: Pid, thread: Pid) -> Vec<Pid> {
        let children = self.0.traced_children(process, thread);
        record!("traced children of {thread} -> {children:?}");
        children
    }

    fn process_threads(&self, process: Pid) -> Result<Vec<Pid>> {
        queried(
            format_args!("list threads of {process}"),
            self.0.process_threads(process),
        )
    }

    fn thread_name(&self, process: Pid, thread: Pid) -> Option<Arc<str>> {
        self.0.thread_name(process, thread)
    }

    fn seize(&self, pid: Pid, exit_kill: bool) -> Result<bool> {
        issued(
            format_args!("PTRACE_SEIZE {pid} exit_kill={exit_kill}"),
            &true,
            || self.0.seize(pid, exit_kill),
        )
    }

    fn interrupt(&self, pid: Pid) -> Result<bool> {
        issued(format_args!("PTRACE_INTERRUPT {pid}"), &true, || {
            self.0.interrupt(pid)
        })
    }

    fn detach(&self, pid: Pid, signal: Option<Signal>) -> Result<bool> {
        queried(
            format_args!("PTRACE_DETACH {pid} {signal:?}"),
            self.0.detach(pid, signal),
        )
    }

    fn kill(&self, pid: Pid, signal: Signal) -> Result<()> {
        issued(format_args!("kill {pid} {signal}"), &(), || {
            self.0.kill(pid, signal)
        })
    }

    fn reap(&self, pid: Pid) -> Result<()> {
        issued(format_args!("reap {pid}"), &(), || self.0.reap(pid))
    }

    fn wait_status(&self, pid: Pid) -> std::result::Result<WaitEvent, Errno> {
        self.0.wait_status(pid)
    }

    fn poll_status(&self, pid: Pid) -> Option<WaitEvent> {
        self.0.poll_status(pid)
    }

    fn process_start_time(&self, process: Pid) -> Option<u64> {
        self.0.process_start_time(process)
    }

    fn tracer_process(&self) -> i32 {
        self.0.tracer_process()
    }

    fn allocate_stop_id(&self) -> Result<StopId> {
        self.0.allocate_stop_id()
    }

    fn identify_module(&self, mapping: &ModuleMapping) -> Option<(PathBuf, crate::LoadBias)> {
        self.0.identify_module(mapping)
    }

    fn load_modules(
        &self,
        modules: &[(PathBuf, crate::ModuleImageId)],
        search: &crate::debug_info::DebugFileSearch,
    ) -> Vec<Result<DebugInfo>> {
        self.0.load_modules(modules, search)
    }

    fn thread_group_id(&self, pid: Pid) -> Result<Pid> {
        self.0.thread_group_id(pid)
    }

    fn load_bias(
        &self,
        pid: Pid,
        executable: &Path,
        image_base: u64,
        identity: FileIdentity,
    ) -> Result<crate::LoadBias> {
        let bias = self.0.load_bias(pid, executable, image_base, identity);
        match &bias {
            Ok(bias) => record!("load bias of {pid} -> {bias:#x}"),
            Err(error) => record!("load bias of {pid} -> error {error:?}"),
        }

        bias
    }

    fn module_mappings(&self, pid: Pid) -> Result<ProcessMappings> {
        self.0.module_mappings(pid)
    }

    fn process_program(&self, pid: Pid) -> Result<super::native::ProcessProgram> {
        let program = self.0.process_program(pid);
        match &program {
            Ok(program) => record!("program of {pid} -> {}", program.path.display()),
            Err(error) => record!("program of {pid} -> error {error:?}"),
        }

        program
    }

    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()> {
        issued(
            format_args!("PTRACE_POKEDATA {pid} {address:#x} {value:#018x}"),
            &(),
            || self.0.write_word(pid, address, value),
        )
    }

    fn continue_execution(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        issued(format_args!("PTRACE_CONT {pid} {signal:?}"), &(), || {
            self.0.continue_execution(pid, signal)
        })
    }

    fn continue_during_shutdown(&self, pid: Pid) -> Result<()> {
        issued(
            format_args!("PTRACE_CONT {pid} with SIGKILL to shut down"),
            &(),
            || self.0.continue_during_shutdown(pid),
        )
    }

    fn step(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        issued(
            format_args!("PTRACE_SINGLESTEP {pid} {signal:?}"),
            &(),
            || self.0.step(pid, signal),
        )
    }

    fn continue_to_syscall(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        issued(format_args!("PTRACE_SYSCALL {pid} {signal:?}"), &(), || {
            self.0.continue_to_syscall(pid, signal)
        })
    }

    fn syscall_info(
        &self,
        pid: Pid,
    ) -> std::result::Result<super::syscalls::info::SyscallInfo, nix::errno::Errno> {
        let info = self.0.syscall_info(pid);
        record!("PTRACE_GET_SYSCALL_INFO {pid} -> {info:?}");
        info
    }

    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()> {
        issued(
            format_args!("PTRACE_SETREGS {pid} rip={:#x}", registers.rip),
            &(),
            || self.0.set_registers(pid, registers),
        )
    }

    fn set_options(&self, pid: Pid, exit_kill: bool) -> Result<()> {
        issued(
            format_args!("PTRACE_SETOPTIONS {pid} exit_kill={exit_kill}"),
            &(),
            || self.0.set_options(pid, exit_kill),
        )
    }

    fn event_message(&self, pid: Pid) -> Result<libc::c_long> {
        queried(
            format_args!("PTRACE_GETEVENTMSG {pid}"),
            self.0.event_message(pid),
        )
    }

    fn signal_metadata(&self, pid: Pid) -> std::result::Result<SignalMetadata, Errno> {
        queried(
            format_args!("PTRACE_GETSIGINFO {pid}"),
            self.0.signal_metadata(pid),
        )
    }

    fn request_stop(&self, process: Pid, thread: Pid) -> Result<()> {
        issued(
            format_args!("tgkill {process} {thread} SIGSTOP"),
            &(),
            || self.0.request_stop(process, thread),
        )
    }

    fn held_release(&self) -> Option<fn(&crate::HeldProcess) -> Result<bool>> {
        self.0.held_release()
    }

    fn signal_mask(&self, pid: Pid) -> Result<u64> {
        queried(
            format_args!("PTRACE_GETSIGMASK {pid}"),
            self.0.signal_mask(pid),
        )
    }

    fn set_signal_mask(&self, pid: Pid, mask: u64) -> Result<()> {
        issued(
            format_args!("PTRACE_SETSIGMASK {pid} {mask:#x}"),
            &(),
            || self.0.set_signal_mask(pid, mask),
        )
    }

    fn executable(&self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        queried(
            format_args!("executable {address} in {pid}"),
            self.0.executable(pid, address),
        )
    }

    fn queued_signal(&self, pid: Pid, signal: Signal, queue: SignalQueue) -> Result<bool> {
        queried(
            format_args!("queued {signal} of {pid} in its {queue:?} queue"),
            self.0.queued_signal(pid, signal, queue),
        )
    }

    fn read_debug_register(&self, pid: Pid, index: usize) -> std::result::Result<u64, Errno> {
        queried(
            format_args!("read DR{index} of {pid}"),
            self.0.read_debug_register(pid, index),
        )
    }

    fn write_debug_register(
        &self,
        pid: Pid,
        index: usize,
        value: u64,
    ) -> std::result::Result<(), Errno> {
        issued(
            format_args!("write DR{index} of {pid} {value:#x}"),
            &(),
            || self.0.write_debug_register(pid, index, value),
        )
    }

    fn install_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
        owner: BreakpointOwner,
    ) -> Result<()> {
        issued(
            format_args!("install breakpoint {address} in {pid} for {owner:?}"),
            &(),
            || self.0.install_breakpoint(pid, sites, address, owner),
        )
    }

    fn remove_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        issued(
            format_args!("lift breakpoint {address} in {pid}"),
            &(),
            || self.0.remove_breakpoint(pid, sites, address),
        )
    }

    fn reinstall_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        issued(
            format_args!("reinstall breakpoint {address} in {pid}"),
            &(),
            || self.0.reinstall_breakpoint(pid, sites, address),
        )
    }
}
