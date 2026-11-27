use crate::backend::linux::Registers;
use std::cell::RefCell;
use std::collections::BTreeMap;
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

use super::memory::{MemoryAccessError, install_site, lift_site, reinstall_site, remove_site};
#[path = "signal_action.rs"]
mod signal_action;
use super::modules::{
    ModuleMapping, ProcessMappings, identify_mapped_module, load_bias, process_mappings_in,
    read_maps,
};

use super::FloatingRegisters;
use super::signals::{Signal, WaitEvent};
use super::trap::Lift;
use super::{
    BreakpointOwner, BreakpointSite, LinuxError, SignalMetadata, WAITER_THREAD_NAME, Waiter,
    WaiterThread, allocate_stop_id, backend_error,
};

pub(super) struct ProcessProgram {
    pub(super) path: PathBuf,
    pub(super) identity: FileIdentity,
    pub(super) c_library: tls::CLibrary,
}

pub(super) fn process_program(pid: Pid) -> Result<ProcessProgram> {
    use std::os::unix::ffi::OsStrExt as _;

    let link = PathBuf::from(format!("/proc/{pid}/exe"));
    let path = fs::read_link(&link)?;
    let unreadable = |reason: String| {
        backend_error(LinuxError::ProgramUnreadable {
            path: path.clone(),
            reason,
        })
    };

    if path.as_os_str().as_bytes().ends_with(b" (deleted)") {
        return Err(unreadable("it was deleted".to_owned()));
    }

    let (data, stamp) =
        crate::image::backing::read_input(&link).map_err(|error| unreadable(error.to_string()))?;

    Ok(ProcessProgram {
        c_library: tls::CLibrary::of_executable(&data),
        identity: FileIdentity { inode: stamp.inode },
        path,
    })
}

pub(super) trait InspectionOps {
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

    fn read_block(&self, _pid: Pid, _address: u64, _size: usize) -> Option<Vec<u8>> {
        None
    }

    fn registers(&self, pid: Pid) -> Result<Registers>;

    fn auxv(&self, _pid: Pid) -> Option<Vec<(u64, u64)>> {
        None
    }

    fn memory_written(&self, _address: u64) -> bool {
        true
    }

    fn floating_registers(&self, _pid: Pid) -> Result<FloatingRegisters> {
        Err(backend_error(LinuxError::UnsupportedFloatingRegisters))
    }

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

    fn python_launch(
        &self,
        executable: &Path,
        environment: &mut Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
        follows: bool,
    ) -> std::result::Result<Option<super::python_launch::PythonLaunch>, Arc<str>> {
        let _ = (executable, environment, follows);
        Ok(None)
    }

    fn node_launch(
        &self,
        executable: &Path,
        environment: &mut Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
        follows: bool,
        tracer: u32,
    ) -> std::result::Result<Option<super::node_launch::NodeLaunch>, Arc<str>> {
        let _ = (executable, environment, follows, tracer);
        Ok(None)
    }

    fn open_inspector_signal(
        &self,
        pid: Pid,
        native_handler: u64,
        interrupted: &mut Option<super::PendingSignal>,
    ) -> std::result::Result<bool, Arc<str>> {
        let _ = (pid, native_handler, interrupted);
        Err("model gap: this target cannot prove and activate Node's native signal handler".into())
    }

    fn loopback_listeners(
        &self,
        pid: Pid,
    ) -> std::result::Result<Vec<std::net::SocketAddr>, Arc<str>> {
        let _ = pid;
        Err("this target has no sockets".into())
    }

    fn python_join(
        &self,
        executable: &Path,
        pid: Pid,
    ) -> std::result::Result<Option<super::python_launch::PythonLaunch>, Arc<str>> {
        let _ = (executable, pid);
        Ok(None)
    }

    fn spawn_waiter(
        &self,
        process: Pid,
        messages: mpsc::Sender<ControllerMessage>,
    ) -> Result<Waiter>;

    fn process_threads(&self, process: Pid) -> Result<Vec<Pid>>;

    fn traced_children(&self, process: Pid, thread: Pid) -> Vec<Pid>;

    fn thread_name(&self, _process: Pid, _thread: Pid) -> Option<Arc<str>> {
        None
    }

    fn seize(&self, pid: Pid, exit_kill: bool) -> Result<bool>;

    fn interrupt(&self, pid: Pid) -> Result<bool>;

    fn detach(&self, pid: Pid, signal: Option<Signal>) -> Result<bool>;

    fn kill(&self, pid: Pid, signal: Signal) -> Result<()>;

    fn reap(&self, pid: Pid) -> Result<()>;

    fn wait_status(&self, pid: Pid) -> std::result::Result<WaitEvent, Errno>;

    fn poll_status(&self, _pid: Pid) -> Option<WaitEvent> {
        None
    }

    fn thread_group_id(&self, pid: Pid) -> Result<Pid>;

    fn process_start_time(&self, process: Pid) -> Option<u64>;

    fn tracer_process(&self) -> i32;

    fn allocate_stop_id(&self) -> Result<StopId>;

    fn identify_module(&self, mapping: &ModuleMapping) -> Option<(PathBuf, crate::LoadBias)>;

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

    fn process_program(&self, _pid: Pid) -> Result<ProcessProgram> {
        Err(backend_error(LinuxError::UnsupportedExec(
            "the inferior replaced its program, which this tracer cannot read".to_owned(),
        )))
    }

    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()>;

    fn continue_execution(&self, pid: Pid, signal: Option<Signal>) -> Result<()>;

    fn continue_during_shutdown(&self, pid: Pid) -> Result<()>;

    fn step(&self, pid: Pid, signal: Option<Signal>) -> Result<()>;

    fn continue_to_syscall(&self, pid: Pid, signal: Option<Signal>) -> Result<()>;

    fn syscall_info(
        &self,
        pid: Pid,
    ) -> std::result::Result<super::syscalls::info::SyscallInfo, Errno>;
    fn set_registers(&self, pid: Pid, registers: Registers) -> Result<()>;

    fn set_options(&self, pid: Pid, exit_kill: bool) -> Result<()>;

    fn event_message(&self, pid: Pid) -> Result<libc::c_long>;

    fn signal_metadata(&self, pid: Pid) -> std::result::Result<SignalMetadata, Errno>;

    fn request_stop(&self, process: Pid, thread: Pid) -> Result<()>;

    fn queued_signal(&self, _pid: Pid, _signal: Signal) -> Result<bool> {
        Ok(false)
    }

    fn executable(&self, pid: Pid, address: VirtualAddress) -> Result<bool>;

    fn read_debug_register(&self, pid: Pid, index: usize) -> std::result::Result<u64, Errno>;

    fn write_debug_register(
        &self,
        pid: Pid,
        index: usize,
        value: u64,
    ) -> std::result::Result<(), Errno>;

    fn install_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
        owner: BreakpointOwner,
    ) -> Result<()> {
        install_site(
            sites,
            address,
            owner,
            |word| self.read_word(pid, word),
            |word, value| self.write_word(pid, word, value),
        )
    }

    fn lift_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
        lift: Lift,
    ) -> Result<()> {
        lift_site(
            sites,
            address,
            lift,
            |word| self.read_word(pid, word),
            |word, value| self.write_word(pid, word, value),
        )
    }

    fn reinstall_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<()> {
        reinstall_site(
            sites,
            address,
            |word| self.read_word(pid, word),
            |word, value| self.write_word(pid, word, value),
        )
    }

    fn remove_breakpoint(
        &self,
        pid: Pid,
        sites: &mut BTreeMap<VirtualAddress, BreakpointSite>,
        address: VirtualAddress,
    ) -> Result<BreakpointSite> {
        remove_site(
            sites,
            address,
            |word| self.read_word(pid, word),
            |word, value| self.write_word(pid, word, value),
        )
    }
}

/// Live `ptrace` handle pinned to the OS thread that created it (`!Send` plus runtime checks)
/// because Linux rejects `ptrace` calls from any thread other than the tracee's tracer.
pub(super) struct LinuxPtrace {
    owner: ThreadId,
    not_send: PhantomData<Rc<()>>,

    waiter: RefCell<Option<Thread>>,

    hub: Option<super::tracer::HubMembership>,
}

impl LinuxPtrace {
    pub(super) fn new() -> Self {
        Self {
            owner: thread::current().id(),
            not_send: PhantomData,
            waiter: RefCell::new(None),
            hub: None,
        }
    }

    pub(super) fn hosted(hub: super::tracer::HubMembership) -> Self {
        Self {
            hub: Some(hub),
            ..Self::new()
        }
    }

    pub(super) fn route_here(&self, process: Pid) {
        self.assert_owner_thread();
        if let Some(hub) = &self.hub {
            hub.edge().start(process);
        }
    }

    fn assert_owner_thread(&self) {
        assert_eq!(
            self.owner,
            thread::current().id(),
            "ptrace called from non-controller thread"
        );
    }

    fn wake_waiter(&self) {
        if let Some(hub) = &self.hub {
            hub.edge().wake();
        }

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

    fn registers(&self, pid: Pid) -> Result<Registers> {
        self.assert_owner_thread();
        ptrace::getregs(pid)
            .map(Registers::from_native)
            .map_err(|error| backend_error(LinuxError::System(error)))
    }

    fn auxv(&self, pid: Pid) -> Option<Vec<(u64, u64)>> {
        let bytes = fs::read(format!("/proc/{pid}/auxv")).ok()?;
        let (words, _) = bytes.as_chunks::<8>();
        Some(
            words
                .as_chunks::<2>()
                .0
                .iter()
                .map(|[key, value]| (u64::from_le_bytes(*key), u64::from_le_bytes(*value)))
                .take_while(|&(key, _)| key != 0)
                .collect(),
        )
    }

    fn floating_registers(&self, pid: Pid) -> Result<FloatingRegisters> {
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

    fn spawn_waiter(
        &self,
        process: Pid,
        messages: mpsc::Sender<ControllerMessage>,
    ) -> Result<Waiter> {
        self.assert_owner_thread();
        if let Some(hub) = &self.hub {
            hub.edge().start(process);
            return Ok(Waiter {
                thread: None,
                hub: Some(hub.edge().clone()),
            });
        }

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

    fn process_program(&self, pid: Pid) -> Result<ProcessProgram> {
        self.assert_owner_thread();
        process_program(pid)
    }

    fn module_mappings(&self, pid: Pid) -> Result<ProcessMappings> {
        self.assert_owner_thread();
        let mut mappings = process_mappings_in(&read_maps(pid)?)?;
        in_process_root(pid, &mut mappings.files);
        in_process_root(pid, &mut mappings.headers);

        Ok(mappings)
    }

    fn python_launch(
        &self,
        executable: &Path,
        environment: &mut Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
        follows: bool,
    ) -> std::result::Result<Option<super::python_launch::PythonLaunch>, Arc<str>> {
        super::python_launch::PythonLaunch::prepare(executable, environment, follows)
    }

    fn node_launch(
        &self,
        executable: &Path,
        environment: &mut Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
        follows: bool,
        tracer: u32,
    ) -> std::result::Result<Option<super::node_launch::NodeLaunch>, Arc<str>> {
        super::node_launch::NodeLaunch::prepare(executable, environment, follows, tracer)
    }

    fn loopback_listeners(
        &self,
        pid: Pid,
    ) -> std::result::Result<Vec<std::net::SocketAddr>, Arc<str>> {
        super::inspector::loopback_listeners(pid)
    }

    fn open_inspector_signal(
        &self,
        pid: Pid,
        native_handler: u64,
        interrupted: &mut Option<super::PendingSignal>,
    ) -> std::result::Result<bool, Arc<str>> {
        self.assert_owner_thread();
        signal_action::open_inspector(self, pid, native_handler, interrupted)
            .map_err(|error| error.to_string().into())
    }

    fn python_join(
        &self,
        executable: &Path,
        pid: Pid,
    ) -> std::result::Result<Option<super::python_launch::PythonLaunch>, Arc<str>> {
        super::python_launch::PythonLaunch::join(executable, pid)
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

    fn set_registers(&self, pid: Pid, registers: Registers) -> Result<()> {
        self.assert_owner_thread();
        ptrace::setregs(pid, *registers.native())
            .map_err(|error| backend_error(LinuxError::System(error)))
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

    fn queued_signal(&self, pid: Pid, signal: Signal) -> Result<bool> {
        self.assert_owner_thread();
        let status = match super::proc_status::read_path(format!("/proc/{pid}/status")) {
            Ok(status) => status,
            Err(error) if super::proc_status::gone(&error) => return Ok(false),
            Err(error) => return Err(error.into()),
        };

        queued_in_status(&status, signal).ok_or_else(|| {
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

pub(super) fn is_vanished_tracee(error: &Error) -> bool {
    matches!(
        error,
        Error::Backend(error)
            if matches!(error.downcast_ref::<LinuxError>(), Some(LinuxError::System(Errno::ESRCH)))
    )
}

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

pub(super) fn queued_in_status(status: &[u8], signal: Signal) -> Option<bool> {
    let pending = super::proc_status::hex_field(status, b"SigPnd:")?;
    let blocked = super::proc_status::hex_field(status, b"SigBlk:")?;
    let bit = 1 << (signal.number() - 1);
    Some(pending & bit != 0 && blocked & bit == 0)
}

fn debug_register_offset(index: usize) -> ptrace::AddressType {
    assert!(index < 8, "x86-64 has eight debug registers");
    (std::mem::offset_of!(libc::user, u_debugreg) + index * std::mem::size_of::<u64>())
        as ptrace::AddressType
}

const WAITER_MIN_POLL: Duration = Duration::from_micros(20);

const WAITER_MAX_POLL: Duration = Duration::from_millis(5);

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
        hub: None,
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

pub(super) fn disable_address_randomization() {
    if let Ok(persona) = personality::get() {
        let _ = personality::set(persona | Persona::ADDR_NO_RANDOMIZE);
    }
}

fn tgkill(process: Pid, thread: Pid, signal: Signal) -> Result<()> {
    super::sys::tgkill(process, thread, signal.number())
        .map_err(|error| backend_error(LinuxError::System(error)))
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

pub(super) const fn errno(error: rustix::io::Errno) -> Errno {
    Errno::from_raw(error.raw_os_error())
}

pub(super) fn decode_siginfo(bytes: &[u8; 128]) -> (i32, SignalMetadata) {
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

pub(super) const fn siginfo_names_sender(code: i32) -> bool {
    code == libc::SI_USER || (code < 0 && code != libc::SI_TIMER && code != libc::SI_SIGIO)
}

pub(super) const fn siginfo_has_fault_address(number: i32, code: i32) -> bool {
    matches!(
        number,
        libc::SIGSEGV | libc::SIGBUS | libc::SIGILL | libc::SIGFPE | libc::SIGTRAP
    ) && code > 0
        && code != libc::SI_KERNEL
}

pub(super) fn wait_for(
    pid: Pid,
    options: libc::c_int,
) -> std::result::Result<Option<WaitEvent>, Errno> {
    let options = rustix::process::WaitOptions::from_bits_retain(options.cast_unsigned());

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

            if status.exited() || status.signaled() {
                super::reaped::REAPED.leave(pid.cast_unsigned(), status.as_raw());
            }

            WaitEvent::decode(Pid::from_raw(pid), status.as_raw())
        }
        Err(error) => Err(errno(error)),
    };

    #[cfg(debug_assertions)]
    match &event {
        Ok(event) => record!("{}", super::recorded::Described(event)),
        Err(error) => record!("waitpid {pid} -> error {error}"),
    }

    event.map(Some)
}

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

pub(super) fn interrupt_outcome(pid: Pid, result: nix::Result<()>) -> Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(Errno::ESRCH) => Ok(false),
        Err(Errno::EIO) if thread_has_exited(pid) => Ok(false),
        Err(error) => Err(backend_error(LinuxError::System(error))),
    }
}

fn tracer_of(pid: Pid) -> Option<Pid> {
    let status = super::proc_status::read_path(format!("/proc/{pid}/status")).ok()?;
    super::proc_status::tracer_pid(&status)
        .filter(|tracer| *tracer != 0)
        .and_then(|tracer| i32::try_from(tracer).ok())
        .map(Pid::from_raw)
}

fn thread_has_exited(pid: Pid) -> bool {
    match super::proc_status::read_path(format!("/proc/{pid}/stat")) {
        Ok(stat) => matches!(super::proc_status::stat_state(&stat), Some(b'Z' | b'X')),
        Err(error) => super::proc_status::gone(&error),
    }
}

pub(super) fn trace_options(exit_kill: bool) -> Options {
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

/// Rewrites mapped file paths through `/proc/<pid>/root` when the tracee runs in a
/// different mount namespace or chroot, matching by inode to confirm the same file.
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
