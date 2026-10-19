//! The harness the kernel's conformance tests share: one script of ptrace
//! operations, run on a real traced process and on the simulated kernel,
//! recording what it observes in terms that do not depend on where things
//! happen to be, so the two records can be compared.
//!
//! Threads run concurrently on Linux, so a script observes only what does
//! not depend on how they interleave: it waits for one named thread's next
//! status, and holds threads in stops where a race would matter.

use std::collections::BTreeMap;
use std::sync::Arc;

use nix::errno::Errno;
use nix::libc;
use object::{Object as _, ObjectSection as _, ObjectSegment as _, ObjectSymbol as _};

use crate::backend::native_tracee::NativeTracee;
use crate::backend::perf::decode;
use crate::backend::sim_edge::signal_view;
use crate::sim::corpus::{Corpus, Variant};
use crate::sim::cpu::{RAX, Registers};
use crate::sim::kernel::perf::perf_registers;
use crate::sim::kernel::ptrace::CallInfo;
use crate::sim::kernel::{Kernel, Options, Tgid, Tid, WaitStatus};

/// The simulated debugger's process identifier.
const TRACER: i32 = 100;

/// How many instructions a simulated wait may run before it gives up.
const MAX_WAIT_STEPS: u64 = 10_000_000;

/// One traced process, real or simulated, as a script drives it. Requests
/// name the thread they address.
pub(super) trait Tracee {
    fn leader(&self) -> Tid;

    fn tracer(&self) -> i32;

    /// Waits for `thread`'s next status.
    fn wait(&mut self, thread: Tid) -> WaitStatus;

    /// Whether `thread` has a status to report, without reaping it.
    fn has_report(&mut self, thread: Tid) -> bool;

    /// Lets running threads run a little while a script polls for what
    /// they do: real ones run by themselves.
    fn pass_time(&mut self);

    /// Whether `thread` finished exiting and awaits reaping.
    fn is_zombie(&self, thread: Tid) -> bool;

    /// The registers, and `orig_rax`.
    fn registers(&self, thread: Tid) -> Result<(Registers, u64), Errno>;

    fn set_registers(&mut self, thread: Tid, registers: &Registers) -> Result<(), Errno>;

    /// The signal, code, sender, and fault address, as the controller sees
    /// them.
    fn signal(&self, thread: Tid) -> Result<(i32, i32, Option<i32>, Option<u64>), Errno>;

    fn event_message(&self, thread: Tid) -> Result<u64, Errno>;

    fn set_options(&mut self, thread: Tid) -> Result<(), Errno>;

    fn resume(&mut self, thread: Tid, signal: Option<i32>, single_step: bool) -> Result<(), Errno>;

    fn kill(&mut self) -> Result<(), Errno>;

    /// `kill(process, signal)`, from the tracer.
    fn signal_process(&mut self, process: Tid, signal: i32) -> Result<(), Errno>;

    fn request_stop(&mut self, thread: Tid) -> Result<(), Errno>;

    /// The tracer's `tgkill(SIGSTOP)` of `thread` of `process`.
    fn request_stop_of(&mut self, process: Tid, thread: Tid) -> Result<(), Errno>;

    /// Whether `thread` is in a job-control stop, untraced.
    fn job_stopped(&self, thread: Tid) -> bool;

    fn peek(&self, thread: Tid, address: u64) -> Result<u64, Errno>;

    fn poke(&mut self, thread: Tid, address: u64, value: u64) -> Result<(), Errno>;

    fn name(&self, thread: Tid) -> String;

    /// The thread group `/proc` names, or `None` once the thread is gone.
    fn thread_group(&self, thread: Tid) -> Option<Tid>;

    /// `/proc/<thread>/maps`.
    fn maps(&self, thread: Tid) -> Option<String>;

    fn read_debug(&self, thread: Tid, index: usize) -> Result<u64, Errno>;

    fn write_debug(&mut self, thread: Tid, index: usize, value: u64) -> Result<(), Errno>;

    /// Lets something other than the tracer hold `count` of `thread`'s
    /// hardware breakpoints, watching `address`.
    fn hold_debug_slots(&mut self, thread: Tid, count: usize, address: u64);

    /// Samples every thread of the process each time it reaches
    /// `address`, copying `stack_bytes` of stack (K-PERF-4).
    fn sample_at(&mut self, address: u64, stack_bytes: u32);

    /// The records the samples wrote since the last call.
    fn samples(&mut self) -> Vec<u8>;

    /// Takes on `child`, which the process forked, to end it with the
    /// process.
    fn adopt(&mut self, child: Tid);

    fn detach(&mut self, thread: Tid, signal: Option<i32>) -> Result<(), Errno>;

    /// `PTRACE_SEIZE` of an untraced thread, with the controller's options.
    fn seize(&mut self, thread: Tid) -> Result<(), Errno>;

    fn interrupt(&mut self, thread: Tid) -> Result<(), Errno>;

    /// A register read and a seize of `thread` by another tracer.
    fn as_another_tracer(&mut self, thread: Tid) -> (Result<(), Errno>, Result<(), Errno>);

    /// The children `thread` forked that are not reaped.
    fn children(&self, thread: Tid) -> Vec<Tid>;

    /// `PTRACE_SYSCALL`.
    fn resume_to_call(&mut self, thread: Tid, signal: Option<i32>) -> Result<(), Errno>;

    /// `PTRACE_GET_SYSCALL_INFO`.
    fn call_info(&self, thread: Tid) -> Result<CallInfo, Errno>;
}

impl Tracee for NativeTracee {
    fn leader(&self) -> Tid {
        self.pid()
    }

    fn tracer(&self) -> i32 {
        i32::try_from(std::process::id()).expect("a process identifier fits i32")
    }

    fn wait(&mut self, thread: Tid) -> WaitStatus {
        Self::wait(self, thread)
    }

    fn has_report(&mut self, thread: Tid) -> bool {
        Self::has_report(self, thread)
    }

    fn pass_time(&mut self) {
        std::thread::yield_now();
    }

    fn is_zombie(&self, thread: Tid) -> bool {
        std::fs::read_to_string(format!("/proc/{}/task/{thread}/stat", self.pid()))
            .ok()
            .and_then(|stat| {
                let (_, fields) = stat.rsplit_once(')')?;
                fields.split_whitespace().next().map(|state| state == "Z")
            })
            .unwrap_or(false)
    }

    fn registers(&self, thread: Tid) -> Result<(Registers, u64), Errno> {
        Ok((
            Self::registers(self, thread)?,
            Self::system_call(self, thread)?,
        ))
    }

    fn set_registers(&mut self, thread: Tid, registers: &Registers) -> Result<(), Errno> {
        Self::set_registers(self, thread, registers)
    }

    fn signal(&self, thread: Tid) -> Result<(i32, i32, Option<i32>, Option<u64>), Errno> {
        self.signal_view(thread)
    }

    fn event_message(&self, thread: Tid) -> Result<u64, Errno> {
        Self::event_message(self, thread)
    }

    fn set_options(&mut self, thread: Tid) -> Result<(), Errno> {
        Self::set_options(self, thread, true)
    }

    fn resume(&mut self, thread: Tid, signal: Option<i32>, single_step: bool) -> Result<(), Errno> {
        Self::resume(self, thread, signal, single_step)
    }

    fn resume_to_call(&mut self, thread: Tid, signal: Option<i32>) -> Result<(), Errno> {
        Self::resume_to_call(self, thread, signal)
    }

    fn call_info(&self, thread: Tid) -> Result<CallInfo, Errno> {
        Self::call_info(self, thread)
    }

    fn kill(&mut self) -> Result<(), Errno> {
        Self::kill(self, libc::SIGKILL)
    }

    fn signal_process(&mut self, process: Tid, signal: i32) -> Result<(), Errno> {
        Self::signal_process(self, process, signal)
    }

    fn request_stop(&mut self, thread: Tid) -> Result<(), Errno> {
        Self::request_stop(self, thread)
    }

    fn request_stop_of(&mut self, process: Tid, thread: Tid) -> Result<(), Errno> {
        Self::request_stop_of(self, process, thread)
    }

    fn job_stopped(&self, thread: Tid) -> bool {
        Self::job_stopped(self, thread)
    }

    fn peek(&self, thread: Tid, address: u64) -> Result<u64, Errno> {
        Self::peek(self, thread, address)
    }

    fn poke(&mut self, thread: Tid, address: u64, value: u64) -> Result<(), Errno> {
        Self::poke(self, thread, address, value)
    }

    fn name(&self, thread: Tid) -> String {
        std::fs::read_to_string(format!("/proc/{}/task/{thread}/comm", self.pid()))
            .expect("read the thread's name")
            .trim_end()
            .to_owned()
    }

    fn thread_group(&self, thread: Tid) -> Option<Tid> {
        Self::thread_group(self, thread)
    }

    fn maps(&self, thread: Tid) -> Option<String> {
        Self::maps(self, thread)
    }

    fn read_debug(&self, thread: Tid, index: usize) -> Result<u64, Errno> {
        self.read_debug_register(thread, index)
    }

    fn write_debug(&mut self, thread: Tid, index: usize, value: u64) -> Result<(), Errno> {
        self.write_debug_register(thread, index, value)
    }

    fn hold_debug_slots(&mut self, thread: Tid, count: usize, address: u64) {
        Self::hold_debug_slots(self, thread, count, address);
    }

    fn sample_at(&mut self, address: u64, stack_bytes: u32) {
        Self::sample_at(self, address, stack_bytes);
    }

    fn samples(&mut self) -> Vec<u8> {
        Self::samples(self)
    }

    fn adopt(&mut self, child: Tid) {
        Self::adopt(self, child);
    }

    fn detach(&mut self, thread: Tid, signal: Option<i32>) -> Result<(), Errno> {
        Self::detach(self, thread, signal)
    }

    fn seize(&mut self, thread: Tid) -> Result<(), Errno> {
        Self::seize(self, thread)
    }

    fn interrupt(&mut self, thread: Tid) -> Result<(), Errno> {
        Self::interrupt(self, thread)
    }

    fn as_another_tracer(&mut self, thread: Tid) -> (Result<(), Errno>, Result<(), Errno>) {
        Self::as_another_tracer(self, thread)
    }

    fn children(&self, thread: Tid) -> Vec<Tid> {
        Self::children(self, thread)
    }
}

/// The simulated kernel running one golden program.
struct SimTracee {
    kernel: Kernel,
    pid: Tid,
}

impl SimTracee {
    fn spawn(variant: &Variant, arguments: &[String]) -> (Self, WaitStatus) {
        let mut kernel = Kernel::new(TRACER);
        let pid = kernel.spawn(
            Arc::clone(&variant.image),
            &variant.path,
            arguments,
            [0; 16],
        );
        let mut tracee = Self { kernel, pid };
        let first = tracee.wait(pid);
        (tracee, first)
    }

    fn check_gap(&self) {
        if let Some(gap) = &self.kernel.gap {
            panic!("model gap: {}", gap.0);
        }
    }

    /// The tracer exits; runs every thread until the program ends, and
    /// returns how it ended.
    fn end(&mut self) -> String {
        self.kernel.forget_tracer(TRACER);
        let mut steps = 0;

        loop {
            if let Some(ended) = self.kernel.ended.get(&Tgid::led_by(self.pid)) {
                return match ended.status {
                    crate::sim::kernel::ExitStatus::Code(code) => format!("exited {code}"),
                    crate::sim::kernel::ExitStatus::Signal(signal, _) => {
                        format!("killed by {signal}")
                    }
                };
            }

            let runnable = self.kernel.runnable().collect::<Vec<_>>();
            assert!(!runnable.is_empty(), "the program can never end");
            for tid in runnable {
                steps += self.kernel.run(tid, 16).executed.max(1);
                self.check_gap();
            }

            assert!(steps < MAX_WAIT_STEPS, "the program never ended");
        }
    }
}

impl Tracee for SimTracee {
    fn leader(&self) -> Tid {
        self.pid
    }

    fn tracer(&self) -> i32 {
        TRACER
    }

    /// Runs every thread that can run, a few instructions each in turn,
    /// until `thread` reports.
    fn wait(&mut self, thread: Tid) -> WaitStatus {
        let mut steps = 0;

        loop {
            if let Some(status) = self.kernel.collect(thread) {
                return status;
            }

            let runnable = self.kernel.runnable().collect::<Vec<_>>();
            assert!(
                !runnable.is_empty(),
                "{thread} can never report: no simulated thread can run"
            );
            for tid in runnable {
                steps += self.kernel.run(tid, 16).executed.max(1);
                self.check_gap();
            }

            assert!(steps < MAX_WAIT_STEPS, "{thread} never reported");
        }
    }

    fn has_report(&mut self, thread: Tid) -> bool {
        self.kernel.reportable(TRACER).any(|tid| tid == thread)
    }

    fn pass_time(&mut self) {
        let runnable = self.kernel.runnable().collect::<Vec<_>>();
        for tid in runnable {
            self.kernel.run(tid, 16);
            self.check_gap();
        }
    }

    fn is_zombie(&self, thread: Tid) -> bool {
        self.kernel
            .threads
            .get(&thread)
            .is_some_and(|thread| matches!(thread.state, crate::sim::kernel::State::Zombie(_)))
    }

    fn registers(&self, thread: Tid) -> Result<(Registers, u64), Errno> {
        self.kernel.get_registers_and_call(thread)
    }

    fn set_registers(&mut self, thread: Tid, registers: &Registers) -> Result<(), Errno> {
        self.kernel.set_registers(thread, *registers)
    }

    fn signal(&self, thread: Tid) -> Result<(i32, i32, Option<i32>, Option<u64>), Errno> {
        self.kernel.signal_info(thread).map(signal_view)
    }

    fn event_message(&self, thread: Tid) -> Result<u64, Errno> {
        self.kernel.event_message(thread)
    }

    fn set_options(&mut self, thread: Tid) -> Result<(), Errno> {
        self.kernel.set_options(
            thread,
            Options {
                trace_clone: true,
                trace_fork: true,
                trace_exit: true,
                exit_kill: true,
            },
        )
    }

    fn resume(&mut self, thread: Tid, signal: Option<i32>, single_step: bool) -> Result<(), Errno> {
        self.kernel.resume(thread, signal, single_step)
    }

    fn resume_to_call(&mut self, thread: Tid, signal: Option<i32>) -> Result<(), Errno> {
        let result = self.kernel.resume_to_call(thread, signal);
        self.check_gap();
        result
    }

    fn call_info(&self, thread: Tid) -> Result<CallInfo, Errno> {
        self.kernel.call_info(thread)
    }

    fn kill(&mut self) -> Result<(), Errno> {
        self.kernel.kill(Tgid::led_by(self.pid), libc::SIGKILL)
    }

    fn signal_process(&mut self, process: Tid, signal: i32) -> Result<(), Errno> {
        self.kernel.kill(Tgid::new(process), signal)
    }

    fn request_stop(&mut self, thread: Tid) -> Result<(), Errno> {
        self.kernel
            .tgkill(Tgid::led_by(self.pid), thread, libc::SIGSTOP)
    }

    fn request_stop_of(&mut self, process: Tid, thread: Tid) -> Result<(), Errno> {
        self.kernel
            .tgkill(Tgid::new(process), thread, libc::SIGSTOP)
    }

    fn job_stopped(&self, thread: Tid) -> bool {
        self.kernel
            .threads
            .get(&thread)
            .is_some_and(|thread| thread.state == crate::sim::kernel::State::JobStopped)
    }

    fn peek(&self, thread: Tid, address: u64) -> Result<u64, Errno> {
        self.kernel.peek(thread, address)
    }

    fn poke(&mut self, thread: Tid, address: u64, value: u64) -> Result<(), Errno> {
        self.kernel.poke(thread, address, value)
    }

    fn name(&self, thread: Tid) -> String {
        self.kernel
            .process_of(thread)
            .expect("the thread exists")
            .name
            .to_string()
    }

    fn thread_group(&self, thread: Tid) -> Option<Tid> {
        self.kernel.thread_group(thread).map(Tgid::id)
    }

    fn maps(&self, thread: Tid) -> Option<String> {
        self.kernel.maps(thread)
    }

    fn read_debug(&self, thread: Tid, index: usize) -> Result<u64, Errno> {
        self.kernel.peek_debug(thread, index)
    }

    fn write_debug(&mut self, thread: Tid, index: usize, value: u64) -> Result<(), Errno> {
        self.kernel.poke_debug(thread, index, value)
    }

    fn hold_debug_slots(&mut self, thread: Tid, count: usize, _address: u64) {
        self.kernel.hold_debug_slots(thread, count);
    }

    fn sample_at(&mut self, address: u64, stack_bytes: u32) {
        self.kernel.perf = Some(crate::sim::kernel::perf::Breakpoint {
            tgid: Tgid::led_by(self.pid),
            address,
            stack_bytes,
            records: Vec::new(),
        });
    }

    fn samples(&mut self) -> Vec<u8> {
        self.kernel
            .perf
            .as_mut()
            .map(|breakpoint| std::mem::take(&mut breakpoint.records))
            .unwrap_or_default()
    }

    fn adopt(&mut self, _child: Tid) {}

    fn detach(&mut self, thread: Tid, signal: Option<i32>) -> Result<(), Errno> {
        self.kernel.detach(thread, signal)
    }

    fn seize(&mut self, thread: Tid) -> Result<(), Errno> {
        self.kernel.seize(
            thread,
            Options {
                trace_clone: true,
                trace_fork: true,
                trace_exit: true,
                exit_kill: false,
            },
        )
    }

    fn interrupt(&mut self, thread: Tid) -> Result<(), Errno> {
        self.kernel.interrupt(thread)
    }

    fn as_another_tracer(&mut self, thread: Tid) -> (Result<(), Errno>, Result<(), Errno>) {
        self.kernel.serve(TRACER + 1);
        let read = self.kernel.get_registers_and_call(thread).map(drop);
        let seize = self.seize(thread);
        self.kernel.serve(TRACER);
        (read, seize)
    }

    fn children(&self, thread: Tid) -> Vec<Tid> {
        self.kernel.children(thread)
    }
}

/// Addresses a script needs in one variant, where it loaded, and symbols
/// that describe others.
pub(super) struct Landmarks {
    /// Where the image loaded, above its own addresses.
    pub(super) bias: u64,
    pub(super) entry: u64,

    /// The first `syscall` instruction.
    pub(super) syscall: u64,

    /// An address nothing maps.
    pub(super) unmapped: u64,

    /// The addresses the image's segments span.
    pub(super) image: std::ops::Range<u64>,
    symbols: BTreeMap<&'static str, u64>,
}

/// Symbols whose addresses describe others, where a program defines them.
const SYMBOLS: [&str; 17] = [
    "fib",
    "share",
    "tick",
    "rt_clone",
    "rt_thread_start",
    "stacks",
    "rt_exit",
    "counter",
    "steady",
    "pattern",
    "shared",
    "fill",
    "bump",
    "child",
    "work",
    "leaf",
    "rt_yield",
];

impl Landmarks {
    /// The landmarks of `variant` loaded `bias` above its own addresses.
    pub(super) fn of(variant: &Variant, bias: u64) -> Self {
        let file = object::File::parse(&*variant.data).expect("parse the golden binary");
        let symbols = SYMBOLS
            .into_iter()
            .filter_map(|name| {
                let symbol = file.symbols().find(|symbol| symbol.name() == Ok(name))?;
                Some((name, symbol.address() + bias))
            })
            .collect();

        let text = file.section_by_name(".text").expect("a text section");
        let mut decoder = iced_x86::Decoder::with_ip(
            64,
            text.data().expect("text bytes"),
            text.address(),
            iced_x86::DecoderOptions::NONE,
        );

        let syscall = decoder
            .iter()
            .find(|instruction| instruction.mnemonic() == iced_x86::Mnemonic::Syscall)
            .expect("a syscall instruction")
            .ip()
            + bias;

        let segments = file
            .segments()
            .map(|segment| (segment.address(), segment.address() + segment.size()))
            .collect::<Vec<_>>();

        let start = segments.iter().map(|&(start, _)| start).min().unwrap_or(0);
        let end = segments.iter().map(|&(_, end)| end).max().unwrap_or(0);

        Self {
            bias,
            entry: file.entry() + bias,
            syscall,
            unmapped: 0x1000,
            image: start + bias..end + bias,
            symbols,
        }
    }

    pub(super) fn symbol(&self, name: &str) -> u64 {
        *self
            .symbols
            .get(name)
            .unwrap_or_else(|| panic!("the program defines no {name}"))
    }

    /// Describes `address` relative to the landmarks.
    pub(super) fn describe(&self, address: u64) -> String {
        [("entry", self.entry), ("syscall", self.syscall)]
            .into_iter()
            .chain(self.symbols.iter().map(|(&name, &address)| (name, address)))
            .filter(|&(_, landmark)| address >= landmark && address - landmark < 64)
            .map(|(name, landmark)| format!("{name}+{}", address - landmark))
            .next()
            .unwrap_or_else(|| "elsewhere".to_owned())
    }
}

/// A sample as a record carries it.
struct Decoded {
    tid: Tid,
    chain: Vec<u64>,
    registers: Vec<u64>,

    /// The bytes of stack it asked for, and those it copied.
    size: u64,
    stack: Vec<u8>,
}

/// The end of user memory.
const USER_END: u64 = 0x7fff_ffff_f000;

/// The registers of a sample, in its order.
const PERF_REGISTER_NAMES: [&str; 18] = [
    "ax", "bx", "cx", "dx", "si", "di", "bp", "sp", "ip", "flags", "r8", "r9", "r10", "r11", "r12",
    "r13", "r14", "r15",
];

/// Records what a script observes.
pub(super) struct Record<'a> {
    pub(super) tracee: &'a mut dyn Tracee,
    pub(super) landmarks: &'a Landmarks,

    /// Threads by creation order, the leader first.
    threads: Vec<Tid>,

    /// Forked processes by creation order.
    children: Vec<Tid>,
    pub(super) lines: Vec<String>,
}

impl Record<'_> {
    pub(super) fn note(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    pub(super) fn leader(&self) -> Tid {
        self.tracee.leader()
    }

    /// A thread's name in the record, which does not depend on its id.
    pub(super) fn name_of(&self, tid: Tid) -> String {
        match self.threads.iter().position(|&known| known == tid) {
            Some(0) => "leader".to_owned(),
            Some(index) => format!("thread {index}"),
            None if self.children.contains(&tid) => format!(
                "child {}",
                1 + self
                    .children
                    .iter()
                    .position(|&child| child == tid)
                    .expect("a known child")
            ),
            None if tid == self.tracee.tracer() => "tracer".to_owned(),
            None => format!("unknown {}", if tid == 0 { "0" } else { "thread" }),
        }
    }

    pub(super) fn wait(&mut self, thread: Tid) -> WaitStatus {
        let status = self.tracee.wait(thread);
        let text = status
            .to_string()
            .replacen(&thread.to_string(), &self.name_of(thread), 1);
        self.note(format!("wait {}: {text}", self.name_of(thread)));
        status
    }

    /// The stopped thread's signal information, without its location, for
    /// a signal that arrives wherever the thread happens to be.
    pub(super) fn signal(&mut self, thread: Tid) {
        let signal = self.tracee.signal(thread).map(|(signal, code, sender, _)| {
            (signal, code, sender.map(|sender| self.name_of(sender)))
        });
        let name = self.name_of(thread);
        self.note(format!("siginfo of {name}: {signal:?}"));
    }

    /// The stopped thread's signal information and location.
    pub(super) fn stop(&mut self, thread: Tid) {
        let signal = self
            .tracee
            .signal(thread)
            .map(|(signal, code, sender, address)| {
                let sender = sender.map(|sender| self.name_of(sender));
                let address = address.map(|address| self.landmarks.describe(address));
                format!("signal {signal} code {code:#x} sender {sender:?} address {address:?}")
            });
        let name = self.name_of(thread);
        self.note(format!("siginfo of {name}: {signal:?}"));
        let rip = self
            .tracee
            .registers(thread)
            .map(|(registers, _)| self.landmarks.describe(registers.rip));
        self.note(format!("rip of {name}: {rip:?}"));
    }

    /// Where a stopped thread is in a system call: `rax` and `orig_rax`.
    pub(super) fn system_call(&mut self, thread: Tid) {
        let call = self.tracee.registers(thread).map(|(registers, orig_rax)| {
            (registers.general[RAX].cast_signed(), orig_rax.cast_signed())
        });
        let name = self.name_of(thread);
        self.note(format!("rax and orig_rax of {name}: {call:?}"));
    }

    pub(super) fn event_message(&mut self, thread: Tid) {
        let message = self.tracee.event_message(thread);
        let name = self.name_of(thread);
        self.note(format!("event message of {name}: {message:x?}"));
    }

    /// Handles a creator's clone event: records it and names the new
    /// thread, which is returned.
    pub(super) fn cloned(&mut self, creator: Tid) -> Tid {
        self.wait(creator);
        self.stop(creator);
        self.system_call(creator);
        let child = self
            .tracee
            .event_message(creator)
            .map(|message| Tid::try_from(message).expect("a thread id"))
            .expect("the clone event names the new thread");
        self.threads.push(child);

        let group = self.tracee.thread_group(child);
        let same = group == Some(self.leader());
        let name = self.name_of(child);
        self.note(format!("{name} is in the creator's group: {same}"));
        child
    }

    /// Handles a parent's fork event: records it and names the child,
    /// which is returned.
    pub(super) fn forked(&mut self, parent: Tid) -> Tid {
        self.wait(parent);
        self.stop(parent);
        self.system_call(parent);
        let child = self
            .tracee
            .event_message(parent)
            .map(|message| Tid::try_from(message).expect("a process id"))
            .expect("the fork event names the child");
        self.children.push(child);
        self.tracee.adopt(child);

        let group = self.tracee.thread_group(child);
        let name = self.name_of(child);
        self.note(format!(
            "{name} leads its own group: {}",
            group == Some(child)
        ));
        self.children_of(parent);
        child
    }

    /// The unreaped children `thread` forked, by name.
    pub(super) fn children_of(&mut self, thread: Tid) {
        let children = self
            .tracee
            .children(thread)
            .into_iter()
            .map(|child| self.name_of(child))
            .collect::<Vec<_>>();
        let name = self.name_of(thread);
        self.note(format!("children of {name}: {children:?}"));
    }

    pub(super) fn detach(&mut self, thread: Tid, signal: Option<i32>) {
        let result = self.tracee.detach(thread, signal);
        let name = self.name_of(thread);
        self.result(&format!("detach {name} with {signal:?}"), result);
    }

    pub(super) fn seize(&mut self, thread: Tid) {
        let result = self.tracee.seize(thread);
        let name = self.name_of(thread);
        self.result(&format!("seize {name}"), result);
    }

    pub(super) fn interrupt(&mut self, thread: Tid) {
        let result = self.tracee.interrupt(thread);
        let name = self.name_of(thread);
        self.result(&format!("interrupt {name}"), result);
    }

    /// Another tracer's register read and seize of `thread`.
    pub(super) fn as_another_tracer(&mut self, thread: Tid) {
        let (read, seize) = self.tracee.as_another_tracer(thread);
        let name = self.name_of(thread);
        self.result(&format!("another tracer reads {name}'s registers"), read);
        self.result(&format!("another tracer seizes {name}"), seize);
    }

    /// The tracer's `tgkill(SIGSTOP)` of `process`'s thread `thread`.
    pub(super) fn request_stop_of(&mut self, process: Tid, thread: Tid) {
        let result = self.tracee.request_stop_of(process, thread);
        let name = self.name_of(thread);
        self.result(&format!("tgkill SIGSTOP to {name}"), result);
    }

    /// `kill(process, SIGCONT)` from the tracer.
    pub(super) fn continue_process(&mut self, process: Tid) {
        let result = self.tracee.signal_process(process, libc::SIGCONT);
        let name = self.name_of(process);
        self.result(&format!("SIGCONT to {name}"), result);
    }

    /// Waits until `thread` is in a job-control stop, untraced.
    pub(super) fn job_stopped(&mut self, thread: Tid) {
        let started = std::time::Instant::now();
        while !self.tracee.job_stopped(thread) {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(10),
                "{thread} never stopped"
            );
            self.tracee.pass_time();
        }

        let name = self.name_of(thread);
        self.note(format!("{name} is in a job-control stop"));
    }

    pub(super) fn result(&mut self, operation: &str, result: Result<(), Errno>) {
        self.note(format!("{operation}: {result:?}"));
    }

    pub(super) fn set_options(&mut self, thread: Tid) {
        let result = self.tracee.set_options(thread);
        let name = self.name_of(thread);
        self.result(&format!("set options of {name}"), result);
    }

    pub(super) fn resume(&mut self, thread: Tid, signal: Option<i32>) {
        let result = self.tracee.resume(thread, signal, false);
        let name = self.name_of(thread);
        self.result(&format!("continue {name} with {signal:?}"), result);
    }

    /// `PTRACE_SYSCALL`.
    pub(super) fn resume_to_call(&mut self, thread: Tid, signal: Option<i32>) {
        let result = self.tracee.resume_to_call(thread, signal);
        let name = self.name_of(thread);
        self.result(
            &format!("continue {name} to a call with {signal:?}"),
            result,
        );
    }

    /// What `PTRACE_GET_SYSCALL_INFO` reports of a stopped thread, with
    /// arguments in the image described by where they point. Returns it.
    pub(super) fn call(&mut self, thread: Tid) -> Option<CallInfo> {
        let info = self.tracee.call_info(thread);
        let image = &self.landmarks.image;

        // A call that returns a thread names it.
        let value = info.ok().map(|info| {
            Tid::try_from(info.value)
                .ok()
                .filter(|tid| self.threads.contains(tid) || self.children.contains(tid))
                .map_or_else(|| info.value.to_string(), |tid| self.name_of(tid))
        });

        let text = info.map(|info| {
            let args = info
                .args
                .iter()
                .map(|&arg| {
                    if image.contains(&arg) {
                        format!("image+{:#x}", arg - image.start)
                    } else if arg < 0x1_0000 {
                        arg.to_string()
                    } else {
                        "elsewhere".to_owned()
                    }
                })
                .collect::<Vec<_>>();
            format!(
                "op {} number {} args {args:?} value {}",
                info.op,
                info.number,
                value.clone().unwrap_or_default()
            )
        });

        let name = self.name_of(thread);
        self.note(format!("call info of {name}: {text:?}"));

        info.ok()
    }

    pub(super) fn step(&mut self, thread: Tid) {
        let result = self.tracee.resume(thread, None, true);
        let name = self.name_of(thread);
        self.result(&format!("step {name}"), result);
    }

    pub(super) fn has_report(&mut self, thread: Tid) {
        let reported = self.tracee.has_report(thread);
        let name = self.name_of(thread);
        self.note(format!("{name} has a report: {reported}"));
    }

    /// Plants `int3` at `address`, returning the original word.
    pub(super) fn plant(&mut self, address: u64) -> u64 {
        let leader = self.leader();
        let original = self.tracee.peek(leader, address).expect("read code");
        let result = self.tracee.poke(leader, address, (original & !0xff) | 0xcc);
        self.result("poke int3", result);
        original
    }

    /// Restores a planted word through `thread`.
    pub(super) fn restore(&mut self, thread: Tid, address: u64, original: u64) {
        let result = self.tracee.poke(thread, address, original);
        self.result("restore", result);
    }

    /// Where an address is in the image, whichever address it loaded at.
    pub(super) fn place(&self, address: u64) -> String {
        format!("{:#x}", address.wrapping_sub(self.landmarks.bias))
    }

    /// A stopped thread's exact place, and its `rcx`.
    pub(super) fn rip(&mut self, thread: Tid) {
        let place = self.tracee.registers(thread).map(|(registers, _)| {
            (
                self.place(registers.rip),
                registers.general[crate::sim::cpu::RCX],
            )
        });
        let name = self.name_of(thread);
        self.note(format!("rip and rcx of {name}: {place:x?}"));
    }

    /// Reads a debug register, recording what it holds.
    pub(super) fn debug(&mut self, thread: Tid, index: usize) -> Result<u64, Errno> {
        let value = self.tracee.read_debug(thread, index);
        let name = self.name_of(thread);
        let shown = value.map(|value| {
            if index < 4 {
                self.place(value)
            } else {
                format!("{value:#x}")
            }
        });
        self.note(format!("DR{index} of {name}: {shown:?}"));
        value
    }

    /// Writes a debug register, recording the result.
    pub(super) fn set_debug(&mut self, thread: Tid, index: usize, value: u64) {
        let result = self.tracee.write_debug(thread, index, value);
        let name = self.name_of(thread);
        self.result(&format!("write DR{index} of {name}"), result);
    }

    /// Watches `address` with slot 0 for `control`'s accesses, as microscope
    /// programs the registers: DR7 cleared, the address, then DR7.
    pub(super) fn watch(&mut self, thread: Tid, address: u64, control: u64) {
        self.set_debug(thread, 7, 0);
        self.set_debug(thread, 0, address);
        self.set_debug(thread, 7, control);
    }

    /// The samples written since the last look, decoded.
    fn decoded_samples(&mut self) -> Vec<Decoded> {
        let records = self.tracee.samples();
        let mut samples = Vec::new();
        let mut rest = &records[..];

        while let Some(next) = decode::next(rest) {
            let (record, length) = next.expect("the kernel writes whole records");
            if let decode::Record::Sample(sample) = record {
                samples.push(Decoded {
                    tid: Tid::try_from(sample.tid.get()).expect("a thread id"),
                    chain: sample.chain.iter().collect(),
                    registers: sample.registers.iter().collect(),
                    size: sample.stack_asked,
                    stack: sample.stack.to_vec(),
                });
            }

            rest = &rest[length..];
        }

        samples
    }

    /// How a sample's word reads in a record: where it is in the image,
    /// or near the sampled stack pointer, as `stack`, or as it is.
    fn word(&self, value: u64, sp: u64) -> String {
        if self.landmarks.image.contains(&value) {
            self.place(value)
        } else if value.wrapping_sub(sp) < 1 << 20 || sp.wrapping_sub(value) < 1 << 16 {
            "stack".to_owned()
        } else {
            format!("{value:#x}")
        }
    }

    /// The one sample `thread` wrote reaching the trap it is stopped at:
    /// whether its registers are the thread's, with `rip` at the trap,
    /// and its stack the thread's memory from `rsp` (K-PERF-1, K-PERF-3).
    pub(super) fn sampled_at_stop(&mut self, thread: Tid) {
        let name = self.name_of(thread);
        let samples = self.decoded_samples();
        let [sample] = &samples[..] else {
            self.note(format!("{name} wrote {} samples", samples.len()));
            return;
        };

        let (mut registers, _) = self.tracee.registers(thread).expect("registers at a trap");
        registers.rip -= 1;
        let differ = perf_registers(&registers)
            .zip(&sample.registers)
            .zip(PERF_REGISTER_NAMES)
            .filter(|((held, sampled), _)| held != *sampled)
            .map(|(_, name)| name)
            .collect::<Vec<_>>();

        self.note(format!(
            "sample of {}: registers differ in {differ:?}",
            self.name_of(sample.tid)
        ));
        let sp = registers.general[crate::sim::cpu::RSP];
        let same = sample.stack.chunks(8).enumerate().all(|(index, word)| {
            let mut held = [0; 8];
            let address = sp + 8 * index as u64;
            let value = self.tracee.peek(thread, address).unwrap_or(0);
            held.copy_from_slice(&value.to_le_bytes());
            held[..word.len()] == *word
        });

        self.note(format!("its stack is the thread's memory: {same}"));
    }

    /// Every sample written since the last look, sorted: its thread, its
    /// chain, and how much of the stack it copied (K-PERF-2, K-PERF-3).
    pub(super) fn samples(&mut self) {
        let mut lines = Vec::new();

        for sample in self.decoded_samples() {
            let sp = sample.registers[7];
            let chain = sample
                .chain
                .iter()
                .map(|&entry| {
                    if entry >= (-4095_i64).cast_unsigned() {
                        format!("context {}", entry.cast_signed())
                    } else {
                        self.word(entry, sp)
                    }
                })
                .collect::<Vec<_>>();

            let copied = sample.stack.len() as u64;

            // A sample asks for no more than reaches the end of user
            // memory, which the environment's size moves.
            let asked = if sample.size == USER_END - sp {
                "to the end of user memory".to_owned()
            } else {
                format!("{} bytes", sample.size)
            };

            let copy = if copied == sample.size {
                "whole".to_owned()
            } else if (sp + copied).is_multiple_of(4096) {
                "to a page's end".to_owned()
            } else {
                format!("{copied} bytes")
            };

            lines.push(format!(
                "sample of {}: chain {}; rbp {}; asked {asked}, copied {copy}",
                self.name_of(sample.tid),
                chain.join(" "),
                self.word(sample.registers[6], sp),
            ));
        }

        lines.sort();
        self.lines.extend(lines);
    }

    /// Rewinds a thread's `rip` over the trap it executed.
    pub(super) fn rewind(&mut self, thread: Tid) {
        let (mut registers, _) = self.tracee.registers(thread).expect("registers at a trap");
        registers.rip -= 1;
        let result = self.tracee.set_registers(thread, &registers);
        let name = self.name_of(thread);
        self.result(&format!("rewind {name}"), result);
    }
}

/// Runs `script` natively and simulated on every variant of `program`
/// with `arguments`, and requires the same observations.
pub(super) fn dual_run(program: &str, arguments: &[&str], script: impl Fn(&mut Record<'_>) + Sync) {
    run_both(program, arguments, false, script);
}

/// Runs `script` as [`dual_run`] does, then the tracer exits: on Linux,
/// the thread that traced the program ends, and the test, the program's
/// parent, waits for it. The record ends with how the program ended.
pub(super) fn dual_run_then_exit(
    program: &str,
    arguments: &[&str],
    script: impl Fn(&mut Record<'_>) + Sync,
) {
    run_both(program, arguments, true, script);
}

fn run_both(
    program: &str,
    arguments: &[&str],
    tracer_exits: bool,
    script: impl Fn(&mut Record<'_>) + Sync,
) {
    let corpus = Corpus::load().expect("load the golden corpus");
    let program = corpus
        .programs
        .iter()
        .find(|candidate| candidate.name == program)
        .unwrap_or_else(|| panic!("no golden program {program}"));
    let arguments = arguments
        .iter()
        .map(|&argument| argument.to_owned())
        .collect::<Vec<_>>();
    for variant in &program.variants {
        let observe = |tracee: &mut dyn Tracee, first: WaitStatus| {
            let leader = tracee.leader();

            // A position-independent program loads wherever the kernel put
            // it, which K-EXEC-2 checks; the other rules are described
            // relative to that.
            let (registers, _) = tracee
                .registers(leader)
                .expect("registers at the first stop");
            let file = object::File::parse(&*variant.data).expect("parse the golden binary");
            let landmarks = Landmarks::of(variant, registers.rip - file.entry());
            let mut record = Record {
                tracee,
                landmarks: &landmarks,
                threads: vec![leader],
                children: Vec::new(),
                lines: Vec::new(),
            };

            record.note(format!("first: {first:?}").replace(&leader.to_string(), "leader"));
            script(&mut record);
            record.lines
        };

        let native_lines = if tracer_exits {
            let (mut lines, process) = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        let (mut native, first) =
                            NativeTracee::spawn_outliving_tracer(&variant.file, &arguments);
                        let lines = observe(&mut native, first);
                        (lines, native.abandon())
                    })
                    .join()
                    .expect("the tracing thread")
            });
            lines.push(format!("the parent reaps: {}", reap_native(process)));
            lines
        } else {
            let (mut native, first) = NativeTracee::spawn(&variant.file, &arguments);
            observe(&mut native, first)
        };

        let (mut simulated, first) = SimTracee::spawn(variant, &arguments);
        let mut simulated_lines = observe(&mut simulated, first);

        if tracer_exits {
            simulated_lines.push(format!("the parent reaps: {}", simulated.end()));
        }

        assert_eq!(
            simulated_lines, native_lines,
            "{} behaves differently simulated (left) and on Linux (right)",
            variant.name
        );
    }
}

/// Waits, as its parent, for a process its tracer left, killing it if it
/// does not end within ten seconds. Returns how it ended.
fn reap_native(process: Tid) -> String {
    use nix::sys::wait::{WaitPidFlag, WaitStatus as Status, waitpid};
    let pid = nix::unistd::Pid::from_raw(process);
    let started = std::time::Instant::now();

    loop {
        match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(Status::Exited(_, code)) => return format!("exited {code}"),
            Ok(Status::Signaled(_, signal, _)) => return format!("killed by {}", signal as i32),
            Ok(_) => {}
            Err(errno) => panic!("waiting for {process} failed: {errno}"),
        }

        if started.elapsed() > std::time::Duration::from_secs(10) {
            let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL);
            let _ = waitpid(pid, None);
            panic!("{process} did not end once its tracer exited");
        }

        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
