//! The ptrace, waitpid, and /proc edge every controller operation goes through.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, Thread, ThreadId};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::libc;
use nix::sys::personality::{self, Persona};
use nix::sys::ptrace::{self, Options};
use nix::sys::signal::{self, Signal as NixSignal};
use nix::unistd::Pid;
use tokio::sync::mpsc;

use crate::backend::linux::tls::{self, TlsModule};
use crate::backend::{ControllerMessage, FileIdentity};
use crate::debug_info::DebugInfo;
use crate::protocol::{LaunchOptions, StopId};
use crate::{Error, Result, VirtualAddress};

use super::memory::MemoryAccessError;
use super::modules::{
    ModuleMapping, ProcessMappings, identify_mapped_module, load_bias, process_mappings_in,
    read_maps,
};

use super::registers::Fxsave;
use super::signals::{Signal, WaitEvent};
use super::{
    BREAKPOINT_OPCODE, BreakpointOwner, BreakpointSite, LinuxError, SignalMetadata,
    WAITER_THREAD_NAME, Waiter, WaiterThread, allocate_stop_id, backend_error,
};

/// Read-only access to a stopped target's registers, memory, and thread-local
/// storage. Live tracing and post-mortem targets share every inspection path.
pub(super) trait InspectionOps {
    /// A thread's id in its own namespace, if the edge proves the mapping.
    fn target_thread_id(
        &self,
        _process: Pid,
        _thread: Pid,
    ) -> Option<crate::runtime_model::TargetThreadId> {
        None
    }

    fn target_thread_ids(
        &self,
        process: Pid,
        threads: &[Pid],
    ) -> Vec<(Pid, crate::runtime_model::TargetThreadId)> {
        threads
            .iter()
            .filter_map(|&thread| Some((thread, self.target_thread_id(process, thread)?)))
            .collect()
    }

    fn read_word(&self, pid: Pid, address: u64) -> Result<u64>;

    fn read_memory_word(
        &self,
        pid: Pid,
        address: u64,
    ) -> std::result::Result<u64, MemoryAccessError> {
        self.read_word(pid, address)
            .map_err(MemoryAccessError::Fatal)
    }

    /// Reads `size` bytes at `address` at once, or `None` when this
    /// target reads only a word at a time or cannot read all of them.
    fn read_block(&self, _pid: Pid, _address: u64, _size: usize) -> Option<Vec<u8>> {
        None
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct>;

    fn floating_registers(&self, _pid: Pid) -> Result<Fxsave> {
        Err(backend_error(LinuxError::UnsupportedFloatingRegisters))
    }

    /// Resolves a module's thread-local block for one thread.
    fn tls_address(
        &self,
        _thread: Pid,
        _module: TlsModule,
        _offset: u64,
    ) -> std::result::Result<VirtualAddress, Arc<str>> {
        Err("thread-local storage lookup is unsupported by this target".into())
    }
}

pub(super) trait LinuxTraceOps: InspectionOps {
    fn agent_files(
        &self,
        _pid: Pid,
        _library: &[u8],
    ) -> std::result::Result<Box<dyn super::agent_files::AgentFiles>, Arc<str>> {
        Err("this target does not support runtime agent files".into())
    }

    fn spawn(&self, executable: &Path, options: LaunchOptions) -> Result<Pid>;

    fn spawn_waiter(&self, messages: mpsc::Sender<ControllerMessage>) -> Result<Waiter>;

    fn process_threads(&self, process: Pid) -> Result<Vec<Pid>>;

    /// The children `thread` of `process` forked that this tracer still
    /// traces. A child whose fork event was lost is found here while its
    /// parent thread exits.
    fn traced_children(&self, process: Pid, thread: Pid) -> Vec<Pid>;

    /// Reads the name a thread gave itself, if it is still readable.
    fn thread_name(&self, _process: Pid, _thread: Pid) -> Option<Arc<str>> {
        None
    }

    /// Seizes a thread, killing it with the tracer when `exit_kill` is set.
    fn seize(&self, pid: Pid, exit_kill: bool) -> Result<bool>;

    fn interrupt(&self, pid: Pid) -> Result<bool>;

    /// Releases a stopped thread, returning whether it was still in its
    /// stop. One SIGKILL took out of it stays traced until it exits.
    fn detach(&self, pid: Pid, signal: Option<Signal>) -> Result<bool>;

    fn kill(&self, pid: Pid, signal: Signal) -> Result<()>;

    fn reap(&self, pid: Pid) -> Result<()>;

    /// Waits for the next status of one tracee, as `waitpid(pid, __WALL)`.
    fn wait_status(&self, pid: Pid) -> std::result::Result<WaitEvent, Errno>;

    /// Takes a status one tracee already has, without waiting, as
    /// `waitpid(pid, __WALL | WNOHANG)`. Only a controller whose waiter has
    /// stopped may take one.
    fn poll_status(&self, _pid: Pid) -> Option<WaitEvent> {
        None
    }

    fn thread_group_id(&self, pid: Pid) -> Result<Pid>;

    /// A process's start time in clock ticks since boot, which tells it
    /// apart from a later process given the same identifier.
    fn process_start_time(&self, process: Pid) -> Option<u64>;

    /// The process identifier signals this debugger sends carry as their
    /// sender.
    fn tracer_process(&self) -> i32;

    /// Allocates the identifier of a new stop. A live edge draws from a
    /// process-wide counter so no two sessions in a process share one; an
    /// edge whose sessions share nothing, as a simulation's, may count its
    /// own so that a session's identifiers do not depend on the others.
    fn allocate_stop_id(&self) -> Result<StopId>;

    /// Resolves the file behind a module mapping and its load bias, or
    /// `None` when the file cannot be proven to be the mapped one.
    fn identify_module(&self, mapping: &ModuleMapping) -> Option<(PathBuf, crate::LoadBias)>;

    /// Loads the debug information of module files, each from a separate
    /// debug file that `search` finds when the module's own has none: all
    /// at once, with each result where its module is in `modules`.
    fn load_modules(
        &self,
        modules: &[(PathBuf, crate::ModuleImageId)],
        search: &crate::debug_info::DebugFileSearch,
    ) -> Vec<Result<DebugInfo>>;
    fn load_bias(
        &self,
        pid: Pid,
        executable: &Path,
        image_base: u64,
        identity: FileIdentity,
    ) -> Result<crate::LoadBias>;
    fn module_mappings(&self, _pid: Pid) -> Result<ProcessMappings> {
        Ok(ProcessMappings::default())
    }

    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()>;

    fn continue_execution(&self, pid: Pid, signal: Option<Signal>) -> Result<()>;

    fn continue_during_shutdown(&self, pid: Pid) -> Result<()>;

    fn step(&self, pid: Pid, signal: Option<Signal>) -> Result<()>;

    /// Resumes a stopped thread until its next system call's entry or
    /// exit, as `PTRACE_SYSCALL` does.
    fn continue_to_syscall(&self, pid: Pid, signal: Option<Signal>) -> Result<()>;

    /// What a thread stopped at a system call reports of it.
    fn syscall_info(
        &self,
        pid: Pid,
    ) -> std::result::Result<super::syscalls::info::SyscallInfo, Errno>;
    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()>;

    fn set_options(&self, pid: Pid, exit_kill: bool) -> Result<()>;

    fn event_message(&self, pid: Pid) -> Result<libc::c_long>;

    fn signal_metadata(&self, pid: Pid) -> std::result::Result<SignalMetadata, Errno>;

    fn request_stop(&self, process: Pid, thread: Pid) -> Result<()>;

    /// The signals a stopped thread blocks, bit `n - 1` for signal `n`.
    fn signal_mask(&self, pid: Pid) -> Result<u64>;

    /// Replaces the signals a stopped thread blocks.
    fn set_signal_mask(&self, pid: Pid, mask: u64) -> Result<()>;

    /// What lets a child this edge held run, for the [`crate::HeldChild`]
    /// that owns it, or none where processes are not the host's.
    fn held_release(&self) -> Option<fn(&crate::HeldProcess) -> Result<bool>>;

    /// Whether resuming a stopped thread would have it dequeue `signal`
    /// before running an instruction: the signal is pending in `queue`, and
    /// the thread does not block it.
    fn queued_signal(&self, _pid: Pid, _signal: Signal, _queue: SignalQueue) -> Result<bool> {
        Ok(false)
    }

    /// Whether `address` lies in memory the process may execute, where a
    /// trap byte replaces code rather than data.
    fn executable(&self, pid: Pid, address: VirtualAddress) -> Result<bool>;

    /// Reads one x86-64 debug register from a stopped thread's user area.
    fn read_debug_register(&self, pid: Pid, index: usize) -> std::result::Result<u64, Errno>;

    /// Writes one x86-64 debug register in a stopped thread's user area.
    fn write_debug_register(
        &self,
        pid: Pid,
        index: usize,
        value: u64,
    ) -> std::result::Result<(), Errno>;

    /// Adds `owner` to the site at `address`, writing a trap there if no
    /// site exists yet. Every backend shares this through its word access.
    fn install_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
        owner: BreakpointOwner,
    ) -> Result<()> {
        if let Some(site) = sites.get_mut(&address) {
            site.owners.insert(owner);
            return Ok(());
        }

        let word = self.read_word(pid, address.get())?;
        let original_byte = word.to_ne_bytes()[0];
        self.write_word(
            pid,
            address.get(),
            (word & !0xff) | u64::from(BREAKPOINT_OPCODE),
        )?;
        sites.insert(
            address,
            BreakpointSite {
                original_byte,
                installed: true,
                owners: BTreeSet::from([owner]),
            },
        );
        Ok(())
    }

    /// Restores the original byte of an installed site, keeping the site.
    fn remove_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        let site = sites
            .get_mut(&address)
            .ok_or_else(|| Error::internal("a breakpoint site is known"))?;
        if !site.installed {
            return Ok(());
        }

        let word = self.read_word(pid, address.get())?;
        self.write_word(
            pid,
            address.get(),
            (word & !0xff) | u64::from(site.original_byte),
        )?;
        site.installed = false;
        Ok(())
    }

    /// Writes the trap of a site its removal lifted back in place.
    fn reinstall_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        let site = sites
            .get_mut(&address)
            .ok_or_else(|| Error::internal("a breakpoint site is known"))?;
        if site.installed {
            return Ok(());
        }

        let word = self.read_word(pid, address.get())?;
        self.write_word(
            pid,
            address.get(),
            (word & !0xff) | u64::from(BREAKPOINT_OPCODE),
        )?;
        site.installed = true;
        Ok(())
    }
}

/// The live ptrace edge, usable only from the thread that created it.
pub(super) struct LinuxPtrace {
    owner: ThreadId,
    not_send: PhantomData<Rc<()>>,

    /// The current inferior's waiter, woken by every request that makes a
    /// tracee report a new wait status.
    waiter: RefCell<Option<Thread>>,
}

impl LinuxPtrace {
    pub(super) fn new() -> Self {
        Self {
            owner: thread::current().id(),
            not_send: PhantomData,
            waiter: RefCell::new(None),
        }
    }

    fn assert_owner_thread(&self) {
        assert_eq!(
            self.owner,
            thread::current().id(),
            "ptrace called from non-controller thread"
        );
    }

    /// Makes the waiter poll promptly for the status a request just caused,
    /// instead of after its idle backoff. Requests wake it only after the
    /// syscall, so the poll the wake triggers cannot run too early to see
    /// the status and leave the waiter backing off.
    fn wake_waiter(&self) {
        if let Some(waiter) = self.waiter.borrow().as_ref() {
            waiter.unpark();
        }
    }
}

impl InspectionOps for LinuxPtrace {
    fn target_thread_id(
        &self,
        process: Pid,
        thread: Pid,
    ) -> Option<crate::runtime_model::TargetThreadId> {
        super::proc_status::target_thread_id(
            u32::try_from(process.as_raw()).ok()?,
            u32::try_from(thread.as_raw()).ok()?,
        )
    }

    fn target_thread_ids(
        &self,
        process: Pid,
        threads: &[Pid],
    ) -> Vec<(Pid, crate::runtime_model::TargetThreadId)> {
        let Ok(process) = u32::try_from(process.as_raw()) else {
            return Vec::new();
        };
        let threads = threads
            .iter()
            .filter_map(|thread| u32::try_from(thread.as_raw()).ok())
            .collect::<Vec<_>>();
        super::proc_status::target_thread_ids(process, &threads)
            .into_iter()
            .filter_map(|(thread, target)| {
                Some((Pid::from_raw(i32::try_from(thread).ok()?), target))
            })
            .collect()
    }

    fn read_word(&self, pid: Pid, address: u64) -> Result<u64> {
        self.assert_owner_thread();
        let value = ptrace::read(pid, address as ptrace::AddressType)
            .map_err(|error| backend_error(LinuxError::System(error)))?;
        Ok(u64::from_ne_bytes(value.to_ne_bytes()))
    }

    fn read_memory_word(
        &self,
        pid: Pid,
        address: u64,
    ) -> std::result::Result<u64, MemoryAccessError> {
        self.assert_owner_thread();
        match ptrace::read(pid, address as ptrace::AddressType) {
            Ok(value) => Ok(u64::from_ne_bytes(value.to_ne_bytes())),
            Err(Errno::EFAULT | Errno::EIO) => Err(MemoryAccessError::Inaccessible),
            Err(error) => Err(MemoryAccessError::Fatal(backend_error(LinuxError::System(
                error,
            )))),
        }
    }

    fn read_block(&self, pid: Pid, address: u64, size: usize) -> Option<Vec<u8>> {
        use std::os::unix::fs::FileExt as _;
        self.assert_owner_thread();
        let file = std::fs::File::open(format!("/proc/{pid}/mem")).ok()?;
        let mut bytes = vec![0; size];
        file.read_exact_at(&mut bytes, address).ok()?;
        Some(bytes)
    }

    fn registers(&self, pid: Pid) -> Result<libc::user_regs_struct> {
        self.assert_owner_thread();
        ptrace::getregs(pid).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn floating_registers(&self, pid: Pid) -> Result<Fxsave> {
        self.assert_owner_thread();
        super::sys::floating_registers(pid)
            .map(Arc::new)
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn tls_address(
        &self,
        thread: Pid,
        module: TlsModule,
        offset: u64,
    ) -> std::result::Result<VirtualAddress, Arc<str>> {
        self.assert_owner_thread();

        // The address space is read through the thread, since the leader
        // may have exited before the rest of its process.
        tls::tls_address(&tls::LiveProcess { pid: thread }, thread, module, offset)
    }
}

impl LinuxTraceOps for LinuxPtrace {
    fn agent_files(
        &self,
        pid: Pid,
        library: &[u8],
    ) -> std::result::Result<Box<dyn super::agent_files::AgentFiles>, Arc<str>> {
        self.assert_owner_thread();
        super::agent_files::create(pid, library)
    }

    fn spawn_waiter(&self, messages: mpsc::Sender<ControllerMessage>) -> Result<Waiter> {
        self.assert_owner_thread();
        let waiter = spawn_waiter(messages)?;
        *self.waiter.borrow_mut() = waiter
            .thread
            .as_ref()
            .map(|thread| thread.handle.thread().clone());
        Ok(waiter)
    }

    fn process_threads(&self, process: Pid) -> Result<Vec<Pid>> {
        self.assert_owner_thread();
        process_threads(process)
    }

    fn traced_children(&self, process: Pid, thread: Pid) -> Vec<Pid> {
        self.assert_owner_thread();

        // The tracer is this thread, the one that traced the parent.
        let tracer = nix::unistd::gettid();
        let Ok(children) = fs::read_to_string(format!("/proc/{process}/task/{thread}/children"))
        else {
            return Vec::new();
        };

        children
            .split_whitespace()
            .filter_map(|child| child.parse().ok().map(Pid::from_raw))
            .filter(|&child| tracer_of(child) == Some(tracer))
            .collect()
    }

    fn thread_name(&self, process: Pid, thread: Pid) -> Option<Arc<str>> {
        let name = fs::read_to_string(format!("/proc/{process}/task/{thread}/comm")).ok()?;
        let name = name.strip_suffix('\n').unwrap_or(&name);
        (!name.is_empty()).then(|| Arc::from(name))
    }

    fn seize(&self, pid: Pid, exit_kill: bool) -> Result<bool> {
        self.assert_owner_thread();
        match ptrace::seize(pid, trace_options(exit_kill)) {
            Ok(()) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(Errno::EPERM) if thread_has_exited(pid) => Ok(false),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
    }

    fn interrupt(&self, pid: Pid) -> Result<bool> {
        self.assert_owner_thread();
        let interrupted = interrupt_outcome(pid, ptrace::interrupt(pid))?;
        if interrupted {
            self.wake_waiter();
        }

        Ok(interrupted)
    }

    fn detach(&self, pid: Pid, signal: Option<Signal>) -> Result<bool> {
        self.assert_owner_thread();
        match ptrace_with_signal(ptrace::Request::PTRACE_DETACH, pid, signal) {
            Ok(()) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
    }

    fn kill(&self, pid: Pid, signal: Signal) -> Result<()> {
        self.assert_owner_thread();
        let signal = NixSignal::try_from(signal.number())
            .map_err(|error| backend_error(LinuxError::System(error)))?;
        let result = signal::kill(pid, signal);
        self.wake_waiter();
        match result {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
    }

    fn reap(&self, pid: Pid) -> Result<()> {
        self.assert_owner_thread();
        wait_for(pid, libc::__WALL)
            .map(|_| ())
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn poll_status(&self, pid: Pid) -> Option<WaitEvent> {
        self.assert_owner_thread();
        wait_for(pid, libc::__WALL | libc::WNOHANG).ok().flatten()
    }

    fn wait_status(&self, pid: Pid) -> std::result::Result<WaitEvent, Errno> {
        self.assert_owner_thread();
        loop {
            match wait_for(pid, libc::__WALL) {
                Ok(Some(status)) => return Ok(status),
                Ok(None) | Err(Errno::EINTR) => {}
                Err(error) => return Err(error),
            }
        }
    }

    fn thread_group_id(&self, pid: Pid) -> Result<Pid> {
        self.assert_owner_thread();
        thread_group_id(pid)
    }

    fn process_start_time(&self, process: Pid) -> Option<u64> {
        crate::backend::process_start_time(process.as_raw())
    }

    fn tracer_process(&self) -> i32 {
        i32::try_from(std::process::id()).unwrap_or(i32::MAX)
    }

    fn allocate_stop_id(&self) -> Result<StopId> {
        allocate_stop_id()
    }

    fn identify_module(&self, mapping: &ModuleMapping) -> Option<(PathBuf, crate::LoadBias)> {
        identify_mapped_module(mapping)
    }

    fn load_modules(
        &self,
        modules: &[(PathBuf, crate::ModuleImageId)],
        search: &crate::debug_info::DebugFileSearch,
    ) -> Vec<Result<DebugInfo>> {
        crate::debug_info::load_modules(modules, search)
    }

    fn load_bias(
        &self,
        pid: Pid,
        executable: &Path,
        image_base: u64,
        identity: FileIdentity,
    ) -> Result<crate::LoadBias> {
        self.assert_owner_thread();
        load_bias(pid, executable, image_base, identity)
    }

    fn module_mappings(&self, pid: Pid) -> Result<ProcessMappings> {
        self.assert_owner_thread();
        let mut mappings = process_mappings_in(&read_maps(pid)?)?;
        in_process_root(pid, &mut mappings.files);
        in_process_root(pid, &mut mappings.headers);
        Ok(mappings)
    }

    fn spawn(&self, executable: &Path, options: LaunchOptions) -> Result<Pid> {
        self.assert_owner_thread();
        let mut command = ProcessCommand::new(executable);
        command.args(options.arguments);
        for (name, value) in options.environment {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }

        if let Some(directory) = options.working_directory {
            command.current_dir(directory);
        }

        if let Some(stdin) = options.stdin {
            command.stdin(stdin);
        }

        if let Some(stdout) = options.stdout {
            command.stdout(stdout);
        }

        if let Some(stderr) = options.stderr {
            command.stderr(stderr);
        }

        trace_child(&mut command);
        let child = command.spawn()?;
        Ok(Pid::from_raw(
            i32::try_from(child.id()).map_err(|_| Error::AddressOverflow)?,
        ))
    }

    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()> {
        self.assert_owner_thread();
        let value = libc::c_long::from_ne_bytes(value.to_ne_bytes());
        ptrace::write(pid, address as ptrace::AddressType, value)
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn continue_execution(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        self.assert_owner_thread();
        ptrace_with_signal(ptrace::Request::PTRACE_CONT, pid, signal)
            .map_err(|error| backend_error(LinuxError::System(error)))?;
        self.wake_waiter();
        Ok(())
    }

    fn continue_during_shutdown(&self, pid: Pid) -> Result<()> {
        self.assert_owner_thread();
        let result = ptrace_with_signal(ptrace::Request::PTRACE_CONT, pid, Some(Signal::SIGKILL));
        self.wake_waiter();
        match result {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(backend_error(LinuxError::System(error))),
        }
    }

    fn step(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        self.assert_owner_thread();
        ptrace_with_signal(ptrace::Request::PTRACE_SINGLESTEP, pid, signal)
            .map_err(|error| backend_error(LinuxError::System(error)))?;
        self.wake_waiter();
        Ok(())
    }

    fn continue_to_syscall(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        self.assert_owner_thread();
        ptrace_with_signal(ptrace::Request::PTRACE_SYSCALL, pid, signal)
            .map_err(|error| backend_error(LinuxError::System(error)))?;
        self.wake_waiter();
        Ok(())
    }

    fn syscall_info(
        &self,
        pid: Pid,
    ) -> std::result::Result<super::syscalls::info::SyscallInfo, Errno> {
        self.assert_owner_thread();
        super::syscalls::info::syscall_info(pid)
    }

    fn set_registers(&self, pid: Pid, registers: libc::user_regs_struct) -> Result<()> {
        self.assert_owner_thread();
        ptrace::setregs(pid, registers).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn set_options(&self, pid: Pid, exit_kill: bool) -> Result<()> {
        self.assert_owner_thread();
        ptrace::setoptions(pid, trace_options(exit_kill))
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn event_message(&self, pid: Pid) -> Result<libc::c_long> {
        self.assert_owner_thread();
        ptrace::getevent(pid).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn signal_metadata(&self, pid: Pid) -> std::result::Result<SignalMetadata, Errno> {
        self.assert_owner_thread();
        super::sys::siginfo(pid).map(|bytes| decode_siginfo(&bytes).1)
    }

    fn request_stop(&self, process: Pid, thread: Pid) -> Result<()> {
        self.assert_owner_thread();
        tgkill(process, thread, Signal::SIGSTOP)?;
        self.wake_waiter();
        Ok(())
    }

    fn held_release(&self) -> Option<fn(&crate::HeldProcess) -> Result<bool>> {
        Some(super::release_held)
    }

    fn signal_mask(&self, pid: Pid) -> Result<u64> {
        self.assert_owner_thread();
        super::sys::signal_mask(pid).map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn set_signal_mask(&self, pid: Pid, mask: u64) -> Result<()> {
        self.assert_owner_thread();
        super::sys::set_signal_mask(pid, mask)
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn queued_signal(&self, pid: Pid, signal: Signal, queue: SignalQueue) -> Result<bool> {
        self.assert_owner_thread();
        let status = match super::proc_status::read_path(format!("/proc/{pid}/status")) {
            Ok(status) => status,
            Err(error) if super::proc_status::gone(&error) => return Ok(false),
            Err(error) => return Err(error.into()),
        };

        queued_in_status(&status, signal, queue).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("/proc/{pid}/status has missing or malformed signal masks"),
            )
            .into()
        })
    }

    fn executable(&self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        self.assert_owner_thread();
        let maps = read_maps(pid)?;
        Ok(maps_executable(&maps, address))
    }

    fn read_debug_register(&self, pid: Pid, index: usize) -> std::result::Result<u64, Errno> {
        self.assert_owner_thread();
        ptrace::read_user(pid, debug_register_offset(index))
            .map(|value| u64::from_ne_bytes(value.to_ne_bytes()))
    }

    fn write_debug_register(
        &self,
        pid: Pid,
        index: usize,
        value: u64,
    ) -> std::result::Result<(), Errno> {
        self.assert_owner_thread();
        ptrace::write_user(
            pid,
            debug_register_offset(index),
            libc::c_long::from_ne_bytes(value.to_ne_bytes()),
        )
    }
}

/// Whether a ptrace request failed because the tracee left its ptrace-stop.
pub(super) fn is_vanished_tracee(error: &Error) -> bool {
    matches!(
        error,
        Error::Backend(error)
            if matches!(error.downcast_ref::<LinuxError>(), Some(LinuxError::System(Errno::ESRCH)))
    )
}

/// Whether the mapping of `/proc/<pid>/maps` containing `address` is
/// executable. A malformed line describes no executable memory.
pub(super) fn maps_executable(maps: &str, address: VirtualAddress) -> bool {
    maps.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let (Some(range), Some(permissions)) = (fields.next(), fields.next()) else {
            return false;
        };

        let Some((start, end)) = range.split_once('-') else {
            return false;
        };

        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
        else {
            return false;
        };

        (start..end).contains(&address.get()) && permissions.as_bytes().get(2) == Some(&b'x')
    })
}

/// The pending set a signal waits in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SignalQueue {
    /// The thread's own, as for a signal its instruction raised.
    Thread,

    /// Its process's, as for a signal sent with `kill`.
    Process,
}

/// Whether the selected pending signal is unblocked. Missing or malformed
/// masks are unknown, never evidence that a signal can safely be drained.
pub(super) fn queued_in_status(status: &[u8], signal: Signal, queue: SignalQueue) -> Option<bool> {
    let pending = match queue {
        SignalQueue::Thread => super::proc_status::hex_field(status, b"SigPnd:")?,
        SignalQueue::Process => super::proc_status::hex_field(status, b"ShdPnd:")?,
    };
    let blocked = super::proc_status::hex_field(status, b"SigBlk:")?;
    let bit = 1 << (signal.number() - 1);
    Some(pending & bit != 0 && blocked & bit == 0)
}

/// The `struct user` offset of one debug register, as `PTRACE_PEEKUSER` and
/// `PTRACE_POKEUSER` address it.
fn debug_register_offset(index: usize) -> ptrace::AddressType {
    assert!(index < 8, "x86-64 has eight debug registers");
    (std::mem::offset_of!(libc::user, u_debugreg) + index * std::mem::size_of::<u64>())
        as ptrace::AddressType
}

/// The first poll interval after a wake or a status. Single steps and
/// breakpoint repairs complete within microseconds, so their stops are
/// collected almost at once.
const WAITER_MIN_POLL: Duration = Duration::from_micros(20);

/// The longest poll interval while the inferior runs without reporting.
const WAITER_MAX_POLL: Duration = Duration::from_millis(5);

/// Spawns the thread that collects every tracee's wait statuses.
///
/// It polls without blocking so it can be stopped while other children of
/// the debugger's process keep running. The interval doubles while nothing
/// happens and drops to the minimum after each status and each wake from
/// the controller, which wakes it whenever a request will cause a status.
fn spawn_waiter(messages: mpsc::Sender<ControllerMessage>) -> Result<Waiter> {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let handle = thread::Builder::new()
        .name(WAITER_THREAD_NAME.into())
        .spawn(move || {
            let mut interval = WAITER_MIN_POLL;
            while !thread_stop.load(Ordering::Acquire) {
                let status = match wait_for(Pid::from_raw(-1), libc::__WALL | libc::WNOHANG) {
                    Ok(Some(status)) => status,
                    Ok(None) => {
                        let parked = Instant::now();
                        thread::park_timeout(interval);

                        // Returning early means the controller woke the waiter.
                        interval = if parked.elapsed() < interval {
                            WAITER_MIN_POLL
                        } else {
                            interval.saturating_mul(2).min(WAITER_MAX_POLL)
                        };

                        continue;
                    }
                    Err(Errno::EINTR) => continue,
                    Err(_) => break,
                };

                interval = WAITER_MIN_POLL;
                if messages
                    .blocking_send(ControllerMessage::Wait(status))
                    .is_err()
                {
                    break;
                }
            }

            record!("exited");
        })?;
    Ok(Waiter {
        thread: Some(WaiterThread { stop, handle }),
    })
}

fn trace_child(command: &mut ProcessCommand) {
    super::sys::before_exec(
        command,
        super::sys::BeforeExec {
            dies_with_parent: true,
            fixed_addresses: true,
            traced: true,
            ..Default::default()
        },
    );
}

/// Runs the program this process executes next without address space
/// randomization, as gdb does, so a rerun shows the same addresses, pointers,
/// and stack contents. A sandbox may forbid it, which costs only that.
pub(super) fn disable_address_randomization() {
    if let Ok(persona) = personality::get() {
        let _ = personality::set(persona | Persona::ADDR_NO_RANDOMIZE);
    }
}

fn tgkill(process: Pid, thread: Pid, signal: Signal) -> Result<()> {
    super::sys::tgkill(process, thread, signal.number())
        .map_err(|error| backend_error(LinuxError::System(error)))
}

/// Whether `process`, if it started at `start_time`, is still held: no
/// tracer has it, and it is stopped by job control or has the `SIGSTOP`
/// that stops it still pending.
pub(super) fn process_held(process: Pid, start_time: u64) -> Result<bool> {
    let read = |file: &str| match super::proc_status::read_path(format!("/proc/{process}/{file}")) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if super::proc_status::gone(&error) => Ok(None),
        Err(error) => Err(Error::from(error)),
    };

    let Some(status) = read("status")? else {
        return Ok(false);
    };

    let Some(stat) = read("stat")? else {
        return Ok(false);
    };

    // Read after both, the start time proves that they described the
    // process that was held.
    let same = crate::backend::process_start_time(process.as_raw()) == Some(start_time);
    if !same {
        return Ok(false);
    }
    held_in_status(&status, &stat).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("/proc/{process} has missing or malformed held-process evidence"),
        )
        .into()
    })
}

/// Ends the job-control stop of `process` with `SIGCONT` if it is still
/// held, and returns whether it did. The process file descriptor, opened
/// first, names the process `process` named then, so the signal can never
/// reach a later process given its identifier. A `SIGCONT` also discards a
/// `SIGSTOP` still pending, so a child released before it stopped runs on.
pub(super) fn continue_held(process: Pid, start_time: u64) -> Result<bool> {
    let handle = match pidfd_open(process) {
        Ok(handle) => handle,
        Err(Errno::ESRCH) => return Ok(false),
        Err(error) => return Err(backend_error(LinuxError::System(error))),
    };

    if !process_held(process, start_time)? {
        return Ok(false);
    }

    match pidfd_send_signal(&handle, Signal::SIGCONT) {
        Ok(()) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(error) => Err(backend_error(LinuxError::System(error))),
    }
}

/// Whether `/proc/<pid>/status`, and `/proc/<pid>/stat` read after it,
/// show a held process: untraced, and with `SIGSTOP` pending or stopped by
/// job control. The status shows the state before the pending signals, so
/// a thread that takes `SIGSTOP` in between, which stops it at once, shows
/// neither there; the state the later stat shows is current.
pub(super) fn held_in_status(status: &[u8], stat: &[u8]) -> Option<bool> {
    let tracer = super::proc_status::tracer_pid(status)?;
    let stopped = super::proc_status::stat_state(stat)? == b'T';
    let thread = queued_in_status(status, Signal::SIGSTOP, SignalQueue::Thread)?;
    let process = queued_in_status(status, Signal::SIGSTOP, SignalQueue::Process)?;
    Some(tracer == 0 && (thread || process || stopped))
}

pub(super) fn pidfd_open(process: Pid) -> std::result::Result<std::os::fd::OwnedFd, Errno> {
    let process = rustix::process::Pid::from_raw(process.as_raw()).ok_or(Errno::EINVAL)?;
    rustix::process::pidfd_open(process, rustix::process::PidfdFlags::empty()).map_err(errno)
}

pub(super) fn pidfd_send_signal(
    handle: &std::os::fd::OwnedFd,
    signal: Signal,
) -> std::result::Result<(), Errno> {
    let signal = rustix::process::Signal::from_named_raw(signal.number()).ok_or(Errno::EINVAL)?;
    rustix::process::pidfd_send_signal(handle, signal).map_err(errno)
}

/// nix's view of an error rustix returned.
pub(super) const fn errno(error: rustix::io::Errno) -> Errno {
    Errno::from_raw(error.raw_os_error())
}

/// A signal's number and what its siginfo says of it, from the 128 bytes
/// the kernel writes: the number at 0 and the code at 8, then a union at
/// 16 whose layout the code selects, holding the sender's process or the
/// faulting address.
pub(super) fn decode_siginfo(bytes: &[u8; 128]) -> (i32, SignalMetadata) {
    // The union at 16 holds the sender's process ID, or the fault address
    // as eight bytes, so its first bytes are the sender's.
    let [
        n0,
        n1,
        n2,
        n3,
        _,
        _,
        _,
        _,
        c0,
        c1,
        c2,
        c3,
        _,
        _,
        _,
        _,
        u0,
        u1,
        u2,
        u3,
        u4,
        u5,
        u6,
        u7,
        ..,
    ] = *bytes;
    let number = i32::from_ne_bytes([n0, n1, n2, n3]);
    let code = i32::from_ne_bytes([c0, c1, c2, c3]);
    let metadata = SignalMetadata {
        code,
        sender: siginfo_names_sender(code).then(|| i32::from_ne_bytes([u0, u1, u2, u3])),
        fault_address: siginfo_has_fault_address(number, code)
            .then(|| u64::from_ne_bytes([u0, u1, u2, u3, u4, u5, u6, u7])),
    };

    (number, metadata)
}

/// Whether a signal's siginfo names the process that sent it. Other user
/// codes reuse that field: `SI_TIMER` for a timer ID and `SI_SIGIO` for a
/// poll band.
pub(super) const fn siginfo_names_sender(code: i32) -> bool {
    code == libc::SI_USER || (code < 0 && code != libc::SI_TIMER && code != libc::SI_SIGIO)
}

/// Whether a signal's siginfo holds a faulting address: a synchronous fault
/// raised by the kernel for an instruction. `SI_KERNEL` faults, such as a
/// general-protection fault on a non-canonical address, record none.
pub(super) const fn siginfo_has_fault_address(number: i32, code: i32) -> bool {
    matches!(
        number,
        libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE | libc::SIGTRAP
    ) && code > 0
        && code != libc::SI_KERNEL
}

/// Waits for a status change of `pid`, or of any child for -1, decoding
/// every signal; `None` means `WNOHANG` found no change.
pub(super) fn wait_for(
    pid: Pid,
    options: libc::c_int,
) -> std::result::Result<Option<WaitEvent>, Errno> {
    let options = rustix::process::WaitOptions::from_bits_retain(options.cast_unsigned());

    // rustix's `waitpid` without a pid waits for the caller's process
    // group; `wait` is waitpid's -1, any child.
    let waited = match pid.as_raw() {
        -1 => rustix::process::wait(options),
        raw => rustix::process::Pid::from_raw(raw).map_or(Err(rustix::io::Errno::INVAL), |child| {
            rustix::process::waitpid(Some(child), options)
        }),
    };

    let event = match waited {
        Ok(None) => return Ok(None),
        Ok(Some((child, status))) => {
            let pid = child.as_raw_nonzero().get();

            // A child that exited is gone for its owner, which may wait
            // for it elsewhere in this process.
            if status.exited() || status.signaled() {
                super::reaped::REAPED.leave(pid.cast_unsigned(), status.as_raw());
            }

            WaitEvent::decode(Pid::from_raw(pid), status.as_raw())
        }
        Err(error) => Err(errno(error)),
    };

    // Every status is recorded where it is consumed, whichever thread waits.
    #[cfg(debug_assertions)]
    match &event {
        Ok(event) => record!("{}", super::recorded::Described(event)),
        Err(error) => record!("waitpid {pid} -> error {error}"),
    }

    event.map(Some)
}

/// Resumes or detaches a stopped tracee, delivering `signal` to it.
pub(super) fn ptrace_with_signal(
    request: ptrace::Request,
    pid: Pid,
    signal: Option<Signal>,
) -> std::result::Result<(), Errno> {
    super::sys::ptrace_with_signal(request, pid, signal.map(Signal::number))
}

pub(super) fn thread_group_id(pid: Pid) -> Result<Pid> {
    let path = format!("/proc/{pid}/status");
    let status = super::proc_status::read_path(&path)?;
    super::proc_status::tgid(&status)
        .and_then(|group| i32::try_from(group).ok())
        .map(Pid::from_raw)
        .ok_or_else(|| backend_error(LinuxError::ProcFile(path)))
}

pub(super) fn process_threads(process: Pid) -> Result<Vec<Pid>> {
    let mut threads = fs::read_dir(format!("/proc/{process}/task"))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let raw = entry.file_name().to_str()?.parse::<i32>().ok()?;
            (raw > 0).then_some(Pid::from_raw(raw))
        })
        .collect::<Vec<_>>();
    threads.sort_unstable();
    Ok(threads)
}

/// Whether `PTRACE_INTERRUPT` reached `pid`, or `false` when the thread is
/// gone.
pub(super) fn interrupt_outcome(pid: Pid, result: nix::Result<()>) -> Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        // The waiter can reap the thread between ptrace's check that it is
        // traced and the interrupt taking its signal lock, which then fails
        // with EIO. As seize's EPERM, that means the thread is gone.
        Err(Errno::EIO) if thread_has_exited(pid) => Ok(false),
        Err(error) => Err(backend_error(LinuxError::System(error))),
    }
}

/// The thread tracing `pid`, from its `/proc` status.
fn tracer_of(pid: Pid) -> Option<Pid> {
    let status = super::proc_status::read_path(format!("/proc/{pid}/status")).ok()?;
    super::proc_status::tracer_pid(&status)
        .filter(|tracer| *tracer != 0)
        .and_then(|tracer| i32::try_from(tracer).ok())
        .map(Pid::from_raw)
}

/// Whether a thread is gone or has finished exiting. Such a thread stays
/// in the thread list for a moment, and `PTRACE_SEIZE` refuses it with
/// `EPERM`.
fn thread_has_exited(pid: Pid) -> bool {
    match super::proc_status::read_path(format!("/proc/{pid}/stat")) {
        Ok(stat) => matches!(super::proc_status::stat_state(&stat), Some(b'Z' | b'X')),
        Err(error) => super::proc_status::gone(&error),
    }
}

pub(super) fn trace_options(exit_kill: bool) -> Options {
    // Fork children are traced only long enough to remove inherited traps.
    // A vfork child shares its parent's memory, so its parent's traps are
    // lifted from that memory until the child execs or exits, which
    // TRACEVFORKDONE reports to the parent.
    let common = Options::PTRACE_O_TRACECLONE
        | Options::PTRACE_O_TRACEFORK
        | Options::PTRACE_O_TRACEVFORK
        | Options::PTRACE_O_TRACEVFORKDONE
        | Options::PTRACE_O_TRACEEXEC
        | Options::PTRACE_O_TRACEEXIT
        | Options::PTRACE_O_TRACESYSGOOD;
    if exit_kill {
        common | Options::PTRACE_O_EXITKILL
    } else {
        common
    }
}

/// A process in another mount namespace, as a container's is, names its
/// files by paths under its own root, where the debugger's own path may be
/// another file or none. The process's root, seen through `/proc`, holds the
/// file it mapped, proven by its inode.
fn in_process_root(pid: Pid, files: &mut [ModuleMapping]) {
    use std::os::unix::fs::MetadataExt as _;
    for mapping in files {
        let same = |path: &Path| fs::metadata(path).is_ok_and(|meta| meta.ino() == mapping.inode);
        if mapping.deleted || same(&mapping.path) {
            continue;
        }

        let Ok(relative) = mapping.path.strip_prefix("/") else {
            continue;
        };

        let rooted = Path::new("/proc")
            .join(pid.as_raw().to_string())
            .join("root")
            .join(relative);
        if same(&rooted) {
            mapping.path = rooted;
        }
    }
}
