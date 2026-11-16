#![allow(
    clippy::expect_used,
    clippy::unreachable,
    reason = "the simulator is a test harness"
)]

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use nix::errno::Errno;
use nix::libc;
use nix::unistd::Pid;
use tokio::sync::{broadcast, mpsc};

use super::memory::MemoryAccessError;
use super::modules::{ModuleMapping, ProcessMappings, load_bias_in, process_mappings_in};
use super::native::{
    InspectionOps, LinuxTraceOps, maps_executable, siginfo_has_fault_address, siginfo_names_sender,
};

use super::FloatingRegisters;
use super::signals::{Signal, WaitEvent};
use super::tls::TlsModule;
use super::{Controller, LinuxError, SessionLease, SignalMetadata, Waiter, backend_error};
use crate::backend::{
    ControllerChannels, ControllerMessage, EventSender, ExecutableSource, FileIdentity,
};
use crate::debug_info::DebugInfo;
use crate::protocol::{DebuggerEvent, LaunchOptions, Request, StopId};
use crate::sim::cpu::Registers;
use crate::sim::kernel::{Kernel, Options, SigInfo, Tgid, Tid, WaitStatus};
use crate::sim::loader::Image;
use crate::{Error, Result, VirtualAddress};

#[cfg(debug_assertions)]
type Edge = super::recorded::Recorded<SimTrace>;

#[cfg(not(debug_assertions))]
type Edge = SimTrace;

pub struct SimLaunch {
    pub image: Arc<Image>,

    pub path: Arc<str>,

    pub random: [u8; 16],
}

pub struct SimExecutable {
    pub path: Arc<str>,
    pub data: Arc<[u8]>,
    pub inode: u64,
}

pub trait Preemption {
    fn before_call(&self);
}

pub struct SimWaiter {
    tracer: i32,

    messages: Option<mpsc::Sender<ControllerMessage>>,

    hand: Option<WaitStatus>,
}

pub struct Collected {
    pub reaped: Option<WaitStatus>,

    pub held: bool,
}

impl SimWaiter {
    #[must_use]
    pub const fn new(tracer: i32) -> Self {
        Self {
            tracer,
            messages: None,
            hand: None,
        }
    }

    #[must_use]
    pub const fn started(&self) -> bool {
        self.messages.is_some()
    }

    #[must_use]
    pub const fn holding(&self) -> Option<WaitStatus> {
        self.hand
    }

    #[must_use]
    pub fn can_collect(&self, kernel: &Kernel) -> bool {
        let Some(messages) = self
            .messages
            .as_ref()
            .filter(|messages| !messages.is_closed())
        else {
            return false;
        };

        match self.hand {
            Some(_) => messages.capacity() > 0,
            None => kernel.reportable(self.tracer).next().is_some(),
        }
    }

    pub fn collect(
        &mut self,
        kernel: &mut Kernel,
        choose: impl FnOnce(&[Tid]) -> Tid,
    ) -> Collected {
        let mut reaped = None;
        if self.hand.is_none() {
            let ready = kernel.reportable(self.tracer).collect::<Vec<_>>();
            let status = kernel
                .collect(choose(&ready))
                .expect("a reportable thread reports");
            reaped = Some(status);
            self.hand = Some(status);
        }

        let status = self.hand.take().expect("the waiter holds a status");
        let queued = self.messages.as_ref().is_some_and(|messages| {
            messages
                .try_send(ControllerMessage::Wait(wait_event(status)))
                .is_ok()
        });
        if !queued {
            self.hand = Some(status);
        }

        Collected {
            reaped,
            held: !queued,
        }
    }
}

pub struct SimTrace {
    tracer: i32,
    kernel: Rc<RefCell<Kernel>>,
    launch: SimLaunch,
    waiter: Rc<RefCell<SimWaiter>>,
    preemption: Rc<dyn Preemption>,

    last_stop_id: Cell<u64>,
}

fn system(errno: Errno) -> Error {
    backend_error(LinuxError::System(errno))
}

fn failed_read(request: &str, pid: Pid, errno: Errno) -> Error {
    #[cfg(debug_assertions)]
    record!("{request} {pid} -> error {errno}");
    #[cfg(not(debug_assertions))]
    let _ = (request, pid);
    system(errno)
}

const fn tid(pid: Pid) -> Tid {
    pid.as_raw()
}

const fn tgid(pid: Pid) -> Tgid {
    Tgid::new(pid.as_raw())
}

const fn trace_options(exit_kill: bool) -> Options {
    Options {
        trace_clone: true,
        trace_fork: true,
        trace_exit: true,
        exit_kill,
    }
}

impl SimTrace {
    fn gap<T>(&self, request: &str) -> Result<T> {
        self.kernel
            .borrow_mut()
            .gap(format!("ptrace edge: {request}"));
        Err(system(Errno::ENOSYS))
    }

    fn kernel(&self) -> std::cell::Ref<'_, Kernel> {
        self.preemption.before_call();
        let kernel = self.kernel.borrow();
        kernel.serve(self.tracer);
        kernel
    }

    fn kernel_mut(&self) -> std::cell::RefMut<'_, Kernel> {
        self.preemption.before_call();
        let kernel = self.kernel.borrow_mut();
        kernel.serve(self.tracer);
        kernel
    }

    fn maps(&self, pid: Pid) -> Result<String> {
        let kernel = self.kernel();
        let Some(maps) = kernel.maps(tid(pid)) else {
            record!("read /proc/{pid}/maps -> not found");
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
        };

        if !maps.is_empty() {
            return Ok(maps);
        }

        let group = kernel.thread_group(tid(pid)).unwrap_or_else(|| tgid(pid));
        Ok(kernel
            .threads_of(group)
            .filter_map(|thread| kernel.maps(thread.tid))
            .find(|maps| !maps.is_empty())
            .unwrap_or(maps))
    }
}

#[cfg(test)]
#[must_use]
pub fn signal_view(info: SigInfo) -> (i32, i32, Option<i32>, Option<u64>) {
    let metadata = signal_metadata(info);
    (
        info.signal,
        metadata.code,
        metadata.sender,
        metadata.fault_address,
    )
}

fn signal_metadata(info: SigInfo) -> SignalMetadata {
    SignalMetadata {
        code: info.code,
        sender: siginfo_names_sender(info.code).then_some(info.pid),
        fault_address: siginfo_has_fault_address(info.signal, info.code).then_some(info.address),
    }
}

pub(super) const fn user_registers(registers: &Registers, orig_rax: u64) -> libc::user_regs_struct {
    let general = &registers.general;
    libc::user_regs_struct {
        r15: general[15],
        r14: general[14],
        r13: general[13],
        r12: general[12],
        rbp: general[5],
        rbx: general[3],
        r11: general[11],
        r10: general[10],
        r9: general[9],
        r8: general[8],
        rax: general[0],
        rcx: general[1],
        rdx: general[2],
        rsi: general[6],
        rdi: general[7],
        orig_rax,
        rip: registers.rip,
        cs: 0x33,
        eflags: registers.rflags,
        rsp: general[4],
        ss: 0x2b,
        fs_base: registers.fs_base,
        gs_base: registers.gs_base,
        ds: 0,
        es: 0,
        fs: 0,
        gs: 0,
    }
}

pub(super) const SETTABLE_FLAGS: u64 = 0x0005_0dd5;

pub(super) const fn apply_user_registers(registers: &mut Registers, user: &libc::user_regs_struct) {
    registers.general = [
        user.rax, user.rcx, user.rdx, user.rbx, user.rsp, user.rbp, user.rsi, user.rdi, user.r8,
        user.r9, user.r10, user.r11, user.r12, user.r13, user.r14, user.r15,
    ];
    registers.rip = user.rip;
    registers.rflags = (registers.rflags & !SETTABLE_FLAGS) | (user.eflags & SETTABLE_FLAGS);
    registers.fs_base = user.fs_base;
    registers.gs_base = user.gs_base;
}

pub(super) fn unmodeled_register_change(
    current: &libc::user_regs_struct,
    written: &libc::user_regs_struct,
) -> Option<&'static str> {
    const TRAP_AND_RESUME: u64 = 0x0001_0100;
    let segments =
        |user: &libc::user_regs_struct| [user.cs, user.ss, user.ds, user.es, user.fs, user.gs];
    if written.orig_rax != current.orig_rax {
        Some("writing orig_rax")
    } else if segments(written) != segments(current) {
        Some("writing segment registers")
    } else if (written.eflags ^ current.eflags) & TRAP_AND_RESUME != 0 {
        Some("writing the trap or resume flag")
    } else {
        None
    }
}

fn wait_event(status: WaitStatus) -> WaitEvent {
    let pid = Pid::from_raw(status.tid());
    let signal = |number| Signal::new(number).expect("the kernel reports real signals");
    match status {
        WaitStatus::Exited(_, code) => WaitEvent::Exited(pid, code),
        WaitStatus::Signaled(_, number, core) => WaitEvent::Signaled(pid, signal(number), core),
        WaitStatus::Stopped(_, number) => WaitEvent::Stopped(pid, signal(number)),
        WaitStatus::Event(_, event) => WaitEvent::PtraceEvent(pid, Signal::SIGTRAP, event),
        WaitStatus::GroupStop(_, number) => {
            WaitEvent::PtraceEvent(pid, signal(number), libc::PTRACE_EVENT_STOP)
        }
        WaitStatus::Syscall(_) => WaitEvent::PtraceSyscall(pid),
    }
}

fn wait_status(event: &WaitEvent) -> String {
    let status = match *event {
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
        WaitEvent::Continued(pid) => {
            return format!("{event:?} for {pid}");
        }
    };

    status.to_string()
}

impl InspectionOps for SimTrace {
    fn target_thread_id(
        &self,
        _process: Pid,
        thread: Pid,
    ) -> Option<crate::runtime_model::TargetThreadId> {
        crate::runtime_model::TargetThreadId::new(u64::try_from(thread.as_raw()).ok()?)
    }

    fn read_word(&self, pid: Pid, address: u64) -> Result<u64> {
        self.kernel()
            .peek(tid(pid), address)
            .map_err(|errno| failed_read("PTRACE_PEEKDATA", pid, errno))
    }

    fn read_memory_word(
        &self,
        pid: Pid,
        address: u64,
    ) -> std::result::Result<u64, MemoryAccessError> {
        match self.kernel().peek(tid(pid), address) {
            Ok(word) => Ok(word),
            Err(Errno::EIO) => Err(MemoryAccessError::Inaccessible),
            Err(errno) => Err(MemoryAccessError::Fatal(failed_read(
                "PTRACE_PEEKDATA",
                pid,
                errno,
            ))),
        }
    }

    fn registers(&self, pid: Pid) -> Result<super::Registers> {
        let (registers, orig_rax) = self
            .kernel()
            .get_registers_and_call(tid(pid))
            .map_err(|errno| failed_read("PTRACE_GETREGS", pid, errno))?;
        Ok(super::Registers::from_native(user_registers(
            &registers, orig_rax,
        )))
    }

    fn floating_registers(&self, _pid: Pid) -> Result<FloatingRegisters> {
        self.gap("floating-point registers")
    }

    fn tls_address(
        &self,
        _thread: Pid,
        _module: TlsModule,
        _offset: u64,
    ) -> std::result::Result<VirtualAddress, Arc<str>> {
        self.gap::<()>("thread-local storage")
            .map_err(|error| Arc::from(error.to_string()))?;
        unreachable!("a gap fails the request")
    }
}

impl LinuxTraceOps for SimTrace {
    fn spawn(&self, executable: &Path, options: LaunchOptions) -> Result<Pid> {
        if executable != Path::new(&*self.launch.path) {
            return self.gap(&format!("launching {}", executable.display()));
        }

        if !options.environment.is_empty()
            || options.working_directory.is_some()
            || options.stdin.is_some()
            || options.stdout.is_some()
            || options.stderr.is_some()
        {
            return self.gap("launch options other than arguments");
        }

        let Some(arguments) = options
            .arguments
            .iter()
            .map(|argument| argument.to_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
        else {
            return self.gap("arguments that are not UTF-8");
        };

        let tid = self.kernel_mut().spawn(
            Arc::clone(&self.launch.image),
            &self.launch.path,
            &arguments,
            self.launch.random,
        );

        Ok(Pid::from_raw(tid))
    }

    fn spawn_waiter(
        &self,
        _process: Pid,
        messages: mpsc::Sender<ControllerMessage>,
    ) -> Result<Waiter> {
        self.waiter.borrow_mut().messages = Some(messages);
        Ok(Waiter::external())
    }

    fn process_threads(&self, process: Pid) -> Result<Vec<Pid>> {
        let kernel = self.kernel();
        if !kernel.processes.contains_key(&tgid(process)) {
            return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
        }

        let mut threads = kernel
            .threads_of(tgid(process))
            .map(|thread| Pid::from_raw(thread.tid))
            .collect::<Vec<_>>();
        threads.sort_unstable();

        Ok(threads)
    }

    fn traced_children(&self, _process: Pid, thread: Pid) -> Vec<Pid> {
        let kernel = self.kernel();
        kernel
            .children(tid(thread))
            .into_iter()
            .filter(|child| {
                kernel
                    .threads
                    .get(child)
                    .is_some_and(|child| child.tracer() == Some(self.tracer))
            })
            .map(Pid::from_raw)
            .collect()
    }

    fn thread_name(&self, process: Pid, thread: Pid) -> Option<Arc<str>> {
        let kernel = self.kernel();
        let owner = kernel.process_of(tid(thread))?;
        (owner.tgid == tgid(process)).then(|| Arc::clone(&owner.name))
    }

    fn seize(&self, pid: Pid, exit_kill: bool) -> Result<bool> {
        let mut kernel = self.kernel_mut();
        match kernel.seize(tid(pid), trace_options(exit_kill)) {
            Ok(()) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(Errno::EPERM) if kernel.finished_exiting(tid(pid)) => Ok(false),
            Err(errno) => Err(system(errno)),
        }
    }

    fn interrupt(&self, pid: Pid) -> Result<bool> {
        match self.kernel_mut().interrupt(tid(pid)) {
            Ok(()) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(errno) => Err(system(errno)),
        }
    }

    fn detach(&self, pid: Pid, signal: Option<Signal>) -> Result<bool> {
        match self
            .kernel_mut()
            .detach(tid(pid), signal.map(Signal::number))
        {
            Ok(()) => Ok(true),
            Err(Errno::ESRCH) => Ok(false),
            Err(errno) => Err(system(errno)),
        }
    }

    fn kill(&self, pid: Pid, signal: Signal) -> Result<()> {
        match self.kernel_mut().kill(tgid(pid), signal.number()) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(errno) => Err(system(errno)),
        }
    }

    fn reap(&self, _pid: Pid) -> Result<()> {
        self.gap("reaping one thread")
    }

    fn wait_status(&self, _pid: Pid) -> std::result::Result<WaitEvent, Errno> {
        let _ = self.gap::<()>("waiting for one thread");
        Err(Errno::ENOSYS)
    }

    fn thread_group_id(&self, pid: Pid) -> Result<Pid> {
        self.kernel()
            .thread_group(tid(pid))
            .map(|group| Pid::from_raw(group.id()))
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound).into())
    }

    fn process_start_time(&self, process: Pid) -> Option<u64> {
        self.kernel()
            .processes
            .get(&tgid(process))
            .map(|process| process.start_time)
    }

    fn tracer_process(&self) -> i32 {
        self.tracer
    }

    fn allocate_stop_id(&self) -> Result<StopId> {
        let next = self.last_stop_id.get() + 1;
        self.last_stop_id.set(next);
        Ok(StopId::new(next))
    }

    fn identify_module(&self, _mapping: &ModuleMapping) -> Option<(PathBuf, crate::LoadBias)> {
        let _ = self.gap::<()>("identifying a library");
        None
    }

    fn load_modules(
        &self,
        modules: &[(PathBuf, crate::ModuleImageId)],
        _search: &crate::debug_info::DebugFileSearch,
    ) -> Vec<Result<DebugInfo>> {
        modules
            .iter()
            .map(|_| self.gap("loading a library"))
            .collect()
    }

    fn load_bias(
        &self,
        pid: Pid,
        executable: &Path,
        image_base: u64,
        identity: FileIdentity,
    ) -> Result<crate::LoadBias> {
        load_bias_in(&self.maps(pid)?, executable, image_base, identity)
    }

    fn module_mappings(&self, pid: Pid) -> Result<ProcessMappings> {
        process_mappings_in(&self.maps(pid)?)
    }

    fn write_word(&self, pid: Pid, address: u64, value: u64) -> Result<()> {
        self.kernel_mut()
            .poke(tid(pid), address, value)
            .map_err(system)
    }

    fn continue_execution(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        self.kernel_mut()
            .resume(tid(pid), signal.map(Signal::number), false)
            .map_err(system)
    }

    fn continue_during_shutdown(&self, pid: Pid) -> Result<()> {
        match self
            .kernel_mut()
            .resume(tid(pid), Some(libc::SIGKILL), false)
        {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(errno) => Err(system(errno)),
        }
    }

    fn step(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        self.kernel_mut()
            .resume(tid(pid), signal.map(Signal::number), true)
            .map_err(system)
    }

    fn continue_to_syscall(&self, pid: Pid, signal: Option<Signal>) -> Result<()> {
        self.kernel_mut()
            .resume_to_call(tid(pid), signal.map(Signal::number))
            .map_err(system)
    }

    fn syscall_info(
        &self,
        pid: Pid,
    ) -> std::result::Result<super::syscalls::info::SyscallInfo, Errno> {
        let info = self.kernel().call_info(tid(pid))?;
        Ok(super::syscalls::info::SyscallInfo {
            op: info.op,
            arch: super::syscalls::info::AUDIT_ARCH_X86_64,
            number: info.number,
            args: info.args,
            value: info.value,
        })
    }

    fn set_registers(&self, pid: Pid, registers: super::Registers) -> Result<()> {
        let registers = *registers.native();
        let mut kernel = self.kernel_mut();
        let (mut current, orig_rax) = kernel.get_registers_and_call(tid(pid)).map_err(system)?;
        if let Some(change) =
            unmodeled_register_change(&user_registers(&current, orig_rax), &registers)
        {
            drop(kernel);
            return self.gap(change);
        }

        apply_user_registers(&mut current, &registers);
        kernel.set_registers(tid(pid), current).map_err(system)
    }

    fn set_options(&self, pid: Pid, exit_kill: bool) -> Result<()> {
        self.kernel_mut()
            .set_options(tid(pid), trace_options(exit_kill))
            .map_err(system)
    }

    fn event_message(&self, pid: Pid) -> Result<libc::c_long> {
        self.kernel()
            .event_message(tid(pid))
            .map(u64::cast_signed)
            .map_err(system)
    }

    fn signal_metadata(&self, pid: Pid) -> std::result::Result<SignalMetadata, Errno> {
        self.kernel().signal_info(tid(pid)).map(signal_metadata)
    }

    fn request_stop(&self, process: Pid, thread: Pid) -> Result<()> {
        self.kernel_mut()
            .tgkill(tgid(process), tid(thread), libc::SIGSTOP)
            .map_err(system)
    }

    fn queued_signal(&self, pid: Pid, signal: Signal) -> Result<bool> {
        Ok(self.kernel().signal_queued(tid(pid), signal.number()))
    }

    fn executable(&self, pid: Pid, address: VirtualAddress) -> Result<bool> {
        Ok(maps_executable(&self.maps(pid)?, address))
    }

    fn read_debug_register(&self, pid: Pid, index: usize) -> std::result::Result<u64, Errno> {
        self.kernel()
            .peek_debug(tid(pid), index)
            .inspect_err(|&errno| {
                let _ = failed_read("PTRACE_PEEKUSER", pid, errno);
            })
    }

    fn write_debug_register(
        &self,
        pid: Pid,
        index: usize,
        value: u64,
    ) -> std::result::Result<(), Errno> {
        self.kernel_mut().poke_debug(tid(pid), index, value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub original_byte: u8,
    pub installed: bool,

    pub owners: usize,

    pub plans: Vec<u64>,

    pub users: BTreeSet<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserBreakpoint {
    pub addresses: BTreeSet<u64>,
    pub hit_count: u64,
}

#[derive(Debug, Default)]
pub struct Truth {
    pub sites: BTreeMap<u64, Site>,

    pub plan_sites: BTreeMap<u64, BTreeSet<u64>>,

    pub active_execution: Option<u64>,

    pub public_stop: Option<u64>,

    pub inferior: Option<Tgid>,

    pub established: bool,

    pub threads: BTreeSet<Tid>,

    pub stopped: BTreeSet<Tid>,

    pub stop_requested: BTreeSet<Tid>,

    pub in_call: Option<BTreeSet<Tid>>,

    pub breakpoints: BTreeMap<u64, UserBreakpoint>,

    pub watchpoints: BTreeMap<u64, u64>,

    pub reasons: BTreeMap<Tid, crate::StopReason>,
}

pub struct SimController {
    controller: Controller<Edge>,
}

pub struct ClientChannels {
    pub requests: mpsc::Sender<ControllerMessage>,
    pub events: broadcast::Sender<DebuggerEvent>,

    pub program: tokio::sync::watch::Receiver<Arc<crate::ModuleImage>>,

    pub audit: mpsc::UnboundedReceiver<DebuggerEvent>,
}

pub struct Delivery {
    message: ControllerMessage,

    pub description: String,

    pub trap: Option<Tid>,

    #[cfg(test)]
    pub request: bool,

    pub shutdown: bool,
}

pub struct SimParts {
    pub tracer: i32,
    pub kernel: Rc<RefCell<Kernel>>,
    pub waiter: Rc<RefCell<SimWaiter>>,
    pub preemption: Rc<dyn Preemption>,
    pub launch: SimLaunch,
    pub executable: SimExecutable,
    pub debug_info: DebugInfo,

    pub queue_capacity: usize,

    pub event_capacity: usize,
}

impl SimController {
    #[must_use]
    pub fn new(parts: SimParts) -> (Self, ClientChannels) {
        let (sender, receiver) = mpsc::channel(parts.queue_capacity);
        let (events, _) = broadcast::channel(parts.event_capacity);
        let (tap, audit) = mpsc::unbounded_channel();
        let trace = SimTrace {
            tracer: parts.tracer,
            kernel: parts.kernel,
            launch: parts.launch,
            waiter: parts.waiter,
            preemption: parts.preemption,
            last_stop_id: Cell::new(0),
        };

        #[cfg(debug_assertions)]
        let trace = super::recorded::Recorded(trace);
        let executable = parts.executable;
        let program = tokio::sync::watch::Sender::new(Arc::clone(&parts.debug_info.image));
        let current = program.subscribe();
        let controller = Controller::new(
            SessionLease::detached(),
            ExecutableSource {
                display_path: Arc::new(PathBuf::from(&*executable.path)),
                launch_path: Arc::new(PathBuf::from(&*executable.path)),
                data: Arc::clone(&executable.data),
                identity: FileIdentity {
                    inode: executable.inode,
                },
                process_start_time: None,
                debug_files: crate::debug_info::DebugFileSearch::default(),
            }
            .described(),
            parts.debug_info,
            ControllerChannels {
                sender: sender.clone(),
                receiver,
                events: EventSender::tapped(events.clone(), tap),
            },
            program,
            trace,
        );

        (
            Self { controller },
            ClientChannels {
                requests: sender,
                events,
                program: current,
                audit,
            },
        )
    }

    #[must_use]
    pub fn has_message(&self) -> bool {
        !self.controller.pending.borrow().is_empty()
            || !self.controller.messages.borrow().is_empty()
    }

    pub fn take(&self) -> Option<Delivery> {
        let message = self.controller.next_message(false)?;
        let (description, trap) = match &message {
            ControllerMessage::Request(request) => (request.describe(), None),
            ControllerMessage::AgentTimeout => {
                ("runtime agent acknowledgement deadline".into(), None)
            }
            ControllerMessage::Inspector(_) => ("inspector message".into(), None),
            ControllerMessage::Wait(event) => (
                format!("wait {}", wait_status(event)),
                match *event {
                    WaitEvent::Stopped(pid, Signal::SIGTRAP) => Some(pid.as_raw()),
                    _ => None,
                },
            ),
        };

        #[cfg(test)]
        let request = matches!(message, ControllerMessage::Request(_));
        let shutdown = matches!(
            message,
            ControllerMessage::Request(Request::Shutdown { .. })
        );

        Some(Delivery {
            message,
            description,
            trap,
            #[cfg(test)]
            request,
            shutdown,
        })
    }

    pub fn handle(&mut self, delivery: Delivery) -> bool {
        self.controller.handle_message(delivery.message)
    }

    #[must_use]
    pub fn inferior(&self) -> Option<Tgid> {
        self.controller
            .session
            .inferior()
            .map(|inferior| Tgid::new(inferior.tgid.as_raw()))
    }

    #[must_use]
    pub fn public_stop(&self) -> Option<u64> {
        self.controller
            .session
            .inferior()?
            .run
            .stop()
            .map(|stop| stop.id.get())
    }

    #[must_use]
    pub fn truth(&self) -> Truth {
        let inferior = self.controller.session.inferior();
        let breakpoints = self
            .controller
            .breakpoints
            .iter()
            .map(|breakpoint| {
                let addresses = inferior
                    .iter()
                    .flat_map(|inferior| {
                        breakpoint.locations.iter().filter_map(|resolved| {
                            super::breakpoints::runtime_breakpoint_address(
                                inferior,
                                &resolved.location,
                            )
                            .ok()
                            .flatten()
                        })
                    })
                    .map(VirtualAddress::get)
                    .collect();
                (
                    breakpoint.id.get(),
                    UserBreakpoint {
                        addresses,
                        hit_count: breakpoint.hit_count,
                    },
                )
            })
            .collect();
        let Some(inferior) = inferior else {
            return Truth {
                breakpoints,
                ..Truth::default()
            };
        };

        Truth {
            sites: inferior
                .breakpoints
                .iter()
                .map(|(address, site)| {
                    let mut plans = Vec::new();
                    let mut users = BTreeSet::new();
                    for owner in &site.owners {
                        match owner {
                            super::BreakpointOwner::Plan(execution) => plans.push(execution.get()),
                            super::BreakpointOwner::User(id) => {
                                users.insert(id.get());
                            }

                            super::BreakpointOwner::Loader
                            | super::BreakpointOwner::Runtime
                            | super::BreakpointOwner::StackMove
                            | super::BreakpointOwner::InterpretedMove
                            | super::BreakpointOwner::Resolver
                            | super::BreakpointOwner::InspectorHook => {}
                        }
                    }

                    (
                        address.get(),
                        Site {
                            original_byte: site.original_byte,
                            installed: site.trap == super::trap::Trap::Installed,
                            owners: site.owners.len(),
                            plans,
                            users,
                        },
                    )
                })
                .collect(),
            plan_sites: inferior
                .plan_sites
                .iter()
                .map(|(execution, sites)| {
                    (
                        execution.get(),
                        sites.iter().map(|address| address.get()).collect(),
                    )
                })
                .collect(),
            active_execution: inferior.run.execution().map(|active| active.id.get()),
            public_stop: inferior.run.stop().map(|stop| stop.id.get()),
            inferior: Some(tgid(inferior.tgid)),
            established: !self.controller.session.starting(),
            threads: inferior.threads.keys().map(|pid| pid.as_raw()).collect(),
            stopped: threads_in(inferior, super::NativeThreadState::Stopped),
            stop_requested: threads_in(inferior, super::NativeThreadState::StopRequested),
            in_call: in_calls(inferior, self.controller.shutdown.is_some()),
            reasons: inferior
                .threads
                .iter()
                .filter_map(|(pid, thread)| Some((pid.as_raw(), thread.reason.clone()?)))
                .collect(),
            breakpoints,
            watchpoints: inferior
                .watch
                .watchpoints
                .iter()
                .map(|(id, record)| (id.get(), record.watchpoint.hit_count))
                .collect(),
        }
    }
}

fn in_calls(inferior: &super::Inferior, shutting_down: bool) -> Option<BTreeSet<Tid>> {
    let inside = |thread: &super::TraceThread| {
        !matches!(thread.call, super::catchpoints::CallPosition::Outside)
    };
    (!shutting_down).then(|| {
        inferior
            .threads
            .iter()
            .filter(|(_, thread)| inside(thread))
            .map(|(pid, _)| pid.as_raw())
            .collect()
    })
}

fn threads_in(inferior: &super::Inferior, state: super::NativeThreadState) -> BTreeSet<Tid> {
    inferior
        .threads
        .iter()
        .filter(|(_, thread)| thread.state == state)
        .map(|(pid, _)| pid.as_raw())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmodeled_register_writes_are_gaps() {
        let current = user_registers(&Registers::default(), 39);
        let moved = libc::user_regs_struct {
            rip: 0x0040_1000,
            rax: 7,
            eflags: current.eflags | 0x1,
            ..current
        };
        assert_eq!(unmodeled_register_change(&current, &moved), None);

        for (written, change) in [
            (
                libc::user_regs_struct {
                    orig_rax: u64::MAX,
                    ..current
                },
                "writing orig_rax",
            ),
            (
                libc::user_regs_struct {
                    fs: 0x63,
                    ..current
                },
                "writing segment registers",
            ),
            (
                libc::user_regs_struct {
                    eflags: current.eflags | 0x100,
                    ..current
                },
                "writing the trap or resume flag",
            ),
        ] {
            assert_eq!(unmodeled_register_change(&current, &written), Some(change));
        }
    }
}
