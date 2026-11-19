use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nix::errno::Errno;
use nix::libc;
use nix::sys::ptrace::{self, Options};
use nix::unistd::Pid;

use super::super::native::{ptrace_with_signal, wait_for};
use super::super::signals::{Signal, WaitEvent};
use super::super::sys::tgkill;
use super::info::{self, OP_ENTRY, OP_EXIT, OP_SECCOMP, SyscallInfo, call_id, syscall_info};
use super::launch::{self, Gate, GateOptions};
use super::{Sink, seccomp};
use crate::backend::linux::Registers;
use crate::syscalls::captures::{self, Limits, Memory};
use crate::syscalls::specs::{self, Ret};
use crate::syscalls::{
    self as syscalls, CallSet, Capture, Device, EventFlags, InFlight, Outcome, ProcessEnd, ReadAt,
    SyscallEvent, SyscallId, TraceEvent,
};

#[derive(Debug, Clone)]
pub struct TraceOptions {
    pub calls: CallSet,
    pub limits: Limits,

    pub verbose: bool,

    pub follow: bool,

    pub stacks: Option<super::stacks::SharedStacks>,
}

pub enum Target {
    Launch(Command),

    Attach(Vec<u32>),
}

#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("starting the program failed: {0}")]
    Spawn(#[source] std::io::Error),

    #[error("could not attach to process {pid}: {errno}")]
    Attach { pid: u32, errno: Errno },

    #[error("tracing failed: {0}")]
    System(#[from] Errno),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Finished {
    pub end: Option<ProcessEnd>,
}

fn siginfo(pid: Pid) -> Option<[u8; 128]> {
    super::super::sys::siginfo(pid).ok()
}

#[must_use]
pub fn monotonic() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |time| {
        u64::try_from(time.tv_sec()).unwrap_or(0) * 1_000_000_000
            + u64::try_from(time.tv_nsec()).unwrap_or(0)
    })
}

struct ProcessMemory(u32);

impl Memory for ProcessMemory {
    fn read(&mut self, address: u64, buffer: &mut [u8]) -> std::io::Result<usize> {
        super::super::perf::read_memory(self.0, address, buffer)
    }
}

fn lineage(tid: u32) -> (Option<u32>, Option<u32>) {
    let Ok(status) = std::fs::read_to_string(format!("/proc/{tid}/status")) else {
        return (None, None);
    };

    let field = |name: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(name))?
            .trim()
            .parse::<u32>()
            .ok()
    };

    let process = field("Tgid:");
    let parent = if process == Some(tid) {
        field("PPid:")
    } else {
        process
    };

    (process, parent)
}

fn tgid_of(tid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Tgid:"))?
        .trim()
        .parse()
        .ok()
}

fn descriptor(process: u32, fd: i64, time: u64, made: bool) -> Option<TraceEvent> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    if fd == AT_FDCWD {
        let link = std::fs::read_link(format!("/proc/{process}/cwd")).ok()?;
        return Some(TraceEvent::Descriptor {
            process: syscalls::Pid::new(process),
            fd,
            time,
            text: link.to_string_lossy().into(),
            cloexec: false,
            device: None,
            made,
        });
    }

    let path = format!("/proc/{process}/fd/{fd}");
    let link = std::fs::read_link(&path).ok()?;
    let flags = std::fs::read_to_string(format!("/proc/{process}/fdinfo/{fd}")).ok();
    let cloexec = flags
        .as_deref()
        .and_then(|info| info.lines().find_map(|line| line.strip_prefix("flags:")))
        .and_then(|flags| u64::from_str_radix(flags.trim(), 8).ok())
        .is_some_and(|flags| flags & 0o2_000_000 != 0);
    let device = std::fs::metadata(&path).ok().and_then(|metadata| {
        let kind = metadata.file_type();
        (kind.is_char_device() || kind.is_block_device()).then(|| Device {
            block: kind.is_block_device(),
            major: nix::sys::stat::major(metadata.rdev())
                .try_into()
                .unwrap_or(0),
            minor: nix::sys::stat::minor(metadata.rdev())
                .try_into()
                .unwrap_or(0),
        })
    });
    Some(TraceEvent::Descriptor {
        process: syscalls::Pid::new(process),
        fd,
        time,
        text: link.to_string_lossy().into(),
        cloexec,
        device,
        made,
    })
}

fn made(
    process: u32,
    call: SyscallId,
    outcome: Outcome,
    captured: &[Capture],
    time: u64,
) -> Vec<TraceEvent> {
    let mut fds = Vec::new();
    match outcome {
        Outcome::Returned(fd) if specs::spec(call).is_some_and(|spec| spec.ret == Ret::Fd) => {
            fds.push(fd);
        }
        Outcome::Returned(0) if matches!(call.name(), Some("chdir" | "fchdir")) => {
            fds.push(AT_FDCWD);
        }
        Outcome::Returned(0) if matches!(call.name(), Some("pipe" | "pipe2" | "socketpair")) => {
            if let Some(pair) = captured.iter().find(|capture| capture.when == ReadAt::Exit) {
                for at in [0, 4] {
                    let fd = pair
                        .bytes
                        .get(at..at + 4)
                        .and_then(|bytes| bytes.try_into().ok());
                    fds.extend(fd.map(|fd| i64::from(i32::from_le_bytes(fd))));
                }
            }
        }
        _ => {}
    }

    fds.into_iter()
        .filter_map(|fd| descriptor(process, fd, time, true))
        .collect()
}

const AT_FDCWD: i64 = -100;

#[derive(Debug)]
struct Entry {
    call: SyscallId,
    args: [u64; 6],
    entered: u64,
    captured: Vec<Capture>,

    chosen: bool,
}

#[derive(Debug)]
struct Thread {
    process: u32,
    pending: Option<Entry>,
}

#[derive(Debug)]
struct Unannounced {
    process: Option<u32>,

    parent: Option<u32>,

    ended: Option<ProcessEnd>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Filtered,

    EveryCall,
}

struct Tracer<'a> {
    mode: Mode,
    options: &'a TraceOptions,
    threads: BTreeMap<u32, Thread>,

    initial: BTreeSet<u32>,
    unannounced: BTreeMap<u32, Unannounced>,

    reported: BTreeSet<u32>,
    first: u32,
    end: Option<ProcessEnd>,
    detaching: bool,
    sink: &'a mut dyn Sink,
}

impl Tracer<'_> {
    fn emit(&mut self, event: TraceEvent) {
        self.sink.event(event);
    }

    fn report_in_flight(&mut self) {
        let Some(after) = self.sink.in_flight_after() else {
            return;
        };

        let now = monotonic();
        let calls: Vec<InFlight> = self
            .threads
            .iter()
            .filter(|(_, thread)| self.reports(thread.process))
            .filter_map(|(&tid, thread)| {
                let entry = thread.pending.as_ref().filter(|entry| entry.chosen)?;
                (now.saturating_sub(entry.entered) >= after).then_some(InFlight {
                    thread: syscalls::Tid::new(tid),
                    process: syscalls::Pid::new(thread.process),
                    call: entry.call,
                    args: entry.args,
                    entered: entry.entered,
                })
            })
            .collect();
        self.sink.in_flight(&calls);
    }

    fn reports(&self, process: u32) -> bool {
        self.options.follow || self.reported.contains(&process)
    }

    fn resume(&mut self, tid: u32, signal: Option<Signal>) -> Result<(), Errno> {
        let pid = Pid::from_raw(tid.cast_signed());
        if self.detaching {
            return self.detach(tid, signal);
        }

        let request = match self.threads.get(&tid) {
            Some(thread) if thread.pending.is_some() => ptrace::Request::PTRACE_SYSCALL,
            _ if self.mode == Mode::EveryCall => ptrace::Request::PTRACE_SYSCALL,
            _ => ptrace::Request::PTRACE_CONT,
        };

        record!("syscalls ptrace: resume {tid} with {request:?} and {signal:?}");
        match ptrace_with_signal(request, pid, signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn detach(&mut self, tid: u32, signal: Option<Signal>) -> Result<(), Errno> {
        record!("syscalls ptrace: detach {tid}");
        if let Some(thread) = self.threads.remove(&tid) {
            self.unfinished(tid, thread, Outcome::Unfinished);
        }

        match ptrace_with_signal(
            ptrace::Request::PTRACE_DETACH,
            Pid::from_raw(tid.cast_signed()),
            signal,
        ) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn unfinished(&mut self, tid: u32, thread: Thread, outcome: Outcome) {
        let Some(entry) = thread.pending.filter(|entry| entry.chosen) else {
            return;
        };

        if !self.reports(thread.process) {
            return;
        }

        let ends = entry
            .call
            .name()
            .is_some_and(|name| matches!(name, "exit" | "exit_group" | "execve" | "execveat"));
        let outcome = if ends && outcome == Outcome::Unfinished {
            Outcome::ProcessExited
        } else {
            outcome
        };

        self.emit(TraceEvent::Call(SyscallEvent {
            thread: syscalls::Tid::new(tid),
            process: syscalls::Pid::new(thread.process),
            call: entry.call,
            args: entry.args,
            captured: entry.captured,
            outcome,
            entered: Some(entry.entered),
            exited: None,
            stack: None,
            flags: EventFlags::default(),
        }));
    }

    fn thread(&mut self, tid: u32) -> &mut Thread {
        self.threads.entry(tid).or_insert_with(|| Thread {
            process: tgid_of(tid).unwrap_or(tid),
            pending: None,
        })
    }

    fn entry(&mut self, tid: u32, info: &SyscallInfo) -> Result<(), Errno> {
        let entered = monotonic();
        let call = call_id(info);
        let chosen = self.options.calls.contains(call);
        let process = self.thread(tid).process;
        let captured = if chosen && self.reports(process) {
            captures::capture(
                call,
                &info.args,
                ReadAt::Entry,
                None,
                self.options.limits,
                self.options.verbose,
                &mut ProcessMemory(tid),
            )
        } else {
            Vec::new()
        };

        let thread = self.thread(tid);
        if let Some(previous) = thread.pending.take() {
            let stale = Thread {
                process: thread.process,
                pending: Some(previous),
            };

            self.unfinished(tid, stale, Outcome::Unavailable);
        }

        self.thread(tid).pending = Some(Entry {
            call,
            args: info.args,
            entered,
            captured,
            chosen,
        });

        self.resume(tid, None)
    }

    fn exit(&mut self, tid: u32, info: &SyscallInfo) -> Result<(), Errno> {
        let exited = monotonic();
        let outcome = Outcome::of_return(info.value);
        let thread = self.thread(tid);
        let process = thread.process;
        let pending = thread.pending.take();
        let (call, args, entered, mut captured, flags, chosen) = if let Some(entry) = pending {
            (
                entry.call,
                entry.args,
                Some(entry.entered),
                entry.captured,
                EventFlags::default(),
                entry.chosen,
            )
        } else {
            let Ok(registers) = ptrace::getregs(Pid::from_raw(tid.cast_signed())) else {
                return self.resume(tid, None);
            };
            let registers = Registers::from_native(registers);
            let (call, args) = info::from_registers(registers.syscall_audit_arch(), &registers);

            let chosen = self.options.calls.contains(call);
            (
                call,
                args,
                None,
                Vec::new(),
                EventFlags::ENTRY_UNSEEN,
                chosen,
            )
        };

        if chosen && self.reports(process) {
            captured.extend(captures::capture(
                call,
                &args,
                ReadAt::Exit,
                Some(outcome),
                self.options.limits,
                self.options.verbose,
                &mut ProcessMemory(tid),
            ));
            for event in made(process, call, outcome, &captured, exited) {
                self.emit(event);
            }

            let stack = self.stack(process, tid);
            self.emit(TraceEvent::Call(SyscallEvent {
                thread: syscalls::Tid::new(tid),
                process: syscalls::Pid::new(process),
                call,
                args,
                captured,
                outcome,
                entered,
                exited: Some(exited),
                stack,
                flags,
            }));
        }

        self.resume(tid, None)
    }

    fn stack(&self, process: u32, tid: u32) -> Option<crate::syscalls::CallStack> {
        let stacks = self.options.stacks.as_ref()?;
        let registers =
            Registers::from_native(ptrace::getregs(Pid::from_raw(tid.cast_signed())).ok()?);
        let thread_pointer = registers.thread_pointer();
        let registers: Vec<_> = registers.register_file().iter().collect();
        let mut stacks = stacks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        Some(stacks.unwind_live(
            syscalls::Pid::new(process),
            tid,
            &registers,
            Some(thread_pointer),
        ))
    }

    fn syscall_stop(&mut self, tid: u32, seccomp: bool) -> Result<(), Errno> {
        let info = match syscall_info(Pid::from_raw(tid.cast_signed())) {
            Ok(info) => info,
            Err(Errno::ESRCH) => return Ok(()),
            Err(error) => return Err(error),
        };

        record!(
            "syscalls ptrace: {tid} stopped with op {} for {}",
            info.op,
            info.number
        );
        let _ = seccomp;
        match info.op {
            OP_ENTRY
                if self
                    .threads
                    .get(&tid)
                    .and_then(|thread| thread.pending.as_ref())
                    .is_some_and(|entry| entry.call == call_id(&info)) =>
            {
                self.resume(tid, None)
            }
            OP_SECCOMP | OP_ENTRY => self.entry(tid, &info),
            OP_EXIT => self.exit(tid, &info),
            _ => self.resume(tid, None),
        }
    }

    fn event(&mut self, tid: u32, signal: Signal, event: i32) -> Result<(), Errno> {
        let pid = Pid::from_raw(tid.cast_signed());
        match event {
            libc::PTRACE_EVENT_SECCOMP => self.syscall_stop(tid, true),
            libc::PTRACE_EVENT_FORK | libc::PTRACE_EVENT_VFORK | libc::PTRACE_EVENT_CLONE => {
                let parent = self.thread(tid).process;
                if let Ok(child) = ptrace::getevent(pid) {
                    let child = u32::try_from(child).unwrap_or(0);
                    let early = self.unannounced.remove(&child);
                    let process = early
                        .as_ref()
                        .and_then(|early| early.process)
                        .or_else(|| tgid_of(child))
                        .unwrap_or(child);
                    let thread = process == parent;
                    self.threads.entry(child).or_insert(Thread {
                        process,
                        pending: None,
                    });
                    if self.reports(parent) {
                        if !thread {
                            self.reported.insert(process);
                        }

                        self.emit(TraceEvent::Spawn {
                            parent_thread: syscalls::Tid::new(tid),
                            parent: syscalls::Pid::new(parent),
                            child: syscalls::Tid::new(child),
                            thread,
                            time: monotonic(),
                        });
                    }

                    match early {
                        Some(Unannounced {
                            ended: Some(end), ..
                        }) => self.ended(child, end),
                        Some(_) => self.resume(child, None)?,
                        None => {}
                    }
                }

                self.resume(tid, None)
            }
            libc::PTRACE_EVENT_EXEC => {
                if let Ok(former) = ptrace::getevent(pid)
                    && let Ok(former) = u32::try_from(former)
                    && former != tid
                    && let Some(moved) = self.threads.remove(&former)
                    && let Some(leader) = self.threads.insert(tid, moved)
                {
                    self.unfinished(tid, leader, Outcome::Unfinished);
                }

                let process = self.thread(tid).process;
                if let Some(stacks) = &self.options.stacks {
                    stacks
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .forget(syscalls::Pid::new(process));
                }

                if self.reports(process) {
                    let path = std::fs::read_link(format!("/proc/{tid}/exe"))
                        .map_or_else(|_| "?".into(), |path| path.to_string_lossy().into());
                    self.emit(TraceEvent::Exec {
                        thread: syscalls::Tid::new(tid),
                        process: syscalls::Pid::new(process),
                        time: monotonic(),
                        path,
                    });
                }

                self.resume(tid, None)
            }
            libc::PTRACE_EVENT_STOP => {
                if !self.detaching
                    && !self.threads.contains_key(&tid)
                    && !self.initial.contains(&tid)
                {
                    let (process, parent) = lineage(tid);
                    record!(
                        "syscalls ptrace: {tid} of {process:?} waits for {parent:?} to announce it"
                    );
                    self.unannounced.insert(
                        tid,
                        Unannounced {
                            process,
                            parent,
                            ended: None,
                        },
                    );
                    return Ok(());
                }

                self.thread(tid);
                if is_stopping(signal) && !self.detaching {
                    record!("syscalls ptrace: listen {tid}");
                    return match ptrace_with_signal(ptrace::Request::PTRACE_LISTEN, pid, None) {
                        Ok(()) | Err(Errno::ESRCH) => Ok(()),
                        Err(error) => Err(error),
                    };
                }

                self.resume(tid, None)
            }
            _ => self.resume(tid, None),
        }
    }

    fn signal(&mut self, tid: u32, signal: Signal) -> Result<(), Errno> {
        let process = self.thread(tid).process;
        if self.reports(process) && !self.detaching {
            let info = siginfo(Pid::from_raw(tid.cast_signed()));
            let word = |at: usize| {
                info.and_then(|info| {
                    Some(i32::from_le_bytes(info.get(at..at + 4)?.try_into().ok()?))
                })
            };

            let code = word(8);

            let sender = code
                .filter(|&code| code <= 0 && code != -2)
                .and_then(|_| word(16))
                .and_then(|pid| u32::try_from(pid).ok())
                .map(syscalls::Pid::new);
            self.emit(TraceEvent::Signal {
                thread: syscalls::Tid::new(tid),
                process: syscalls::Pid::new(process),
                time: monotonic(),
                signal: signal.number(),
                code,
                sender,
                info: info.map(|info| Arc::from(&info[..])),
                stack: self.stack(process, tid),
            });
        }

        self.resume(tid, Some(signal))
    }

    fn ended(&mut self, tid: u32, end: ProcessEnd) {
        let Some(thread) = self.threads.remove(&tid) else {
            if !self.initial.contains(&tid) {
                self.unannounced
                    .entry(tid)
                    .or_insert(Unannounced {
                        process: None,
                        parent: None,
                        ended: None,
                    })
                    .ended = Some(end);
            }

            return;
        };

        let process = thread.process;
        self.unfinished(tid, thread, Outcome::Unfinished);
        if tid != process {
            if self.reports(process) {
                self.emit(TraceEvent::ThreadExit {
                    thread: syscalls::Tid::new(tid),
                    process: syscalls::Pid::new(process),
                    time: monotonic(),
                    end,
                });
            }

            return;
        }

        let orphans: Vec<u32> = self
            .threads
            .iter()
            .filter(|(_, thread)| thread.process == process)
            .map(|(&tid, _)| tid)
            .collect();
        for orphan in orphans {
            if let Some(thread) = self.threads.remove(&orphan) {
                self.unfinished(orphan, thread, Outcome::Unfinished);
            }
        }

        if process == self.first {
            self.end = Some(end);
        }

        let orphans: Vec<u32> = self
            .unannounced
            .iter()
            .filter(|(_, early)| early.parent == Some(process) && early.ended.is_none())
            .map(|(&tid, _)| tid)
            .collect();
        for orphan in orphans {
            self.unannounced.remove(&orphan);
            self.thread(orphan);
            let _ = self.resume(orphan, None);
        }

        if self.reports(process) {
            self.emit(TraceEvent::Exit {
                process: syscalls::Pid::new(process),
                time: monotonic(),
                end,
            });
        }
    }

    fn handle(&mut self, event: WaitEvent) -> Result<(), Errno> {
        match event {
            WaitEvent::Exited(pid, code) => {
                self.ended(pid.as_raw().cast_unsigned(), ProcessEnd::Exited(code));
                Ok(())
            }
            WaitEvent::Signaled(pid, signal, core) => {
                self.ended(
                    pid.as_raw().cast_unsigned(),
                    ProcessEnd::Killed {
                        signal: signal.number(),
                        core,
                    },
                );
                Ok(())
            }
            WaitEvent::PtraceSyscall(pid) => self.syscall_stop(pid.as_raw().cast_unsigned(), false),
            WaitEvent::PtraceEvent(pid, signal, event) => {
                self.event(pid.as_raw().cast_unsigned(), signal, event)
            }
            WaitEvent::Stopped(pid, signal) => self.signal(pid.as_raw().cast_unsigned(), signal),
            WaitEvent::Continued(_) => Ok(()),
        }
    }

    fn begin_detach(&mut self) {
        self.detaching = true;
        let waiting: Vec<u32> = self
            .unannounced
            .iter()
            .filter(|(_, early)| early.ended.is_none())
            .map(|(&tid, _)| tid)
            .collect();
        for tid in waiting {
            self.unannounced.remove(&tid);
            let _ = self.detach(tid, None);
        }

        let threads: Vec<u32> = self.threads.keys().copied().collect();
        for tid in threads {
            record!("syscalls ptrace: interrupt {tid} to detach");
            let _ = ptrace::interrupt(Pid::from_raw(tid.cast_signed()));
        }
    }
}

const fn is_stopping(signal: Signal) -> bool {
    matches!(
        signal.number(),
        libc::SIGSTOP | libc::SIGTSTP | libc::SIGTTIN | libc::SIGTTOU
    )
}

fn trace_options(launched: bool, follow: bool) -> Options {
    let mut options =
        Options::PTRACE_O_TRACESYSGOOD | Options::PTRACE_O_TRACECLONE | Options::PTRACE_O_TRACEEXEC;

    if launched || follow {
        options |= Options::PTRACE_O_TRACEFORK | Options::PTRACE_O_TRACEVFORK;
    }

    if launched {
        options |= Options::PTRACE_O_TRACESECCOMP | Options::PTRACE_O_EXITKILL;
    }

    options
}

fn seize_process(pid: u32, options: Options, seized: &mut BTreeSet<u32>) -> Result<(), TraceError> {
    loop {
        let entries =
            std::fs::read_dir(format!("/proc/{pid}/task")).map_err(|_| TraceError::Attach {
                pid,
                errno: Errno::ESRCH,
            })?;
        let mut new = false;
        for entry in entries.flatten() {
            let Some(tid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };

            if !seized.insert(tid) {
                continue;
            }

            new = true;
            record!("syscalls ptrace: seize {tid}");
            match ptrace::seize(Pid::from_raw(tid.cast_signed()), options) {
                Ok(()) => {
                    let _ = ptrace::interrupt(Pid::from_raw(tid.cast_signed()));
                }

                Err(Errno::ESRCH) if tid != pid => {}
                Err(errno) => return Err(TraceError::Attach { pid, errno }),
            }
        }

        if !new {
            return Ok(());
        }
    }
}

pub fn trace(
    target: Target,
    options: &TraceOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, TraceError> {
    let mut gate: Option<Gate> = None;
    let mut initial = BTreeSet::new();
    let (mode, first) = match target {
        Target::Launch(command) => {
            let filter = seccomp::filter(&options.calls);
            let mut launched = launch::launch(
                command,
                GateOptions {
                    filter: Some(filter),
                    reap: false,
                },
            )
            .map_err(TraceError::Spawn)?;
            let pid = launched.pid;
            record!("syscalls ptrace: seize launched {pid}");
            ptrace::seize(
                Pid::from_raw(pid.cast_signed()),
                trace_options(true, options.follow),
            )
            .map_err(|errno| TraceError::Attach { pid, errno })?;
            launched.open().map_err(TraceError::Spawn)?;
            gate = Some(launched);
            initial.insert(pid);
            (Mode::Filtered, pid)
        }
        Target::Attach(pids) => {
            for (index, &pid) in pids.iter().enumerate() {
                match seize_process(pid, trace_options(false, options.follow), &mut initial) {
                    Err(TraceError::Attach {
                        errno: Errno::ESRCH,
                        ..
                    }) if index > 0 => continue,
                    seized => seized?,
                }
                seed_descriptors(pid, sink);
                sink.attached(pid);
            }

            (Mode::EveryCall, pids.first().copied().unwrap_or(0))
        }
    };

    let result = follow(mode, first, initial, gate.is_some(), options, stop, sink);
    let spawn = gate.as_mut().map(Gate::finish);
    let finished = result?;
    if finished.end.is_none()
        && let Some(Err(error)) = spawn
    {
        return Err(TraceError::Spawn(error));
    }

    Ok(finished)
}

pub fn trace_held(
    pid: u32,
    options: &TraceOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, TraceError> {
    let _held = super::super::reaped::REAPED.hold(pid);
    record!("syscalls ptrace: seize held {pid}");
    ptrace::seize(
        Pid::from_raw(pid.cast_signed()),
        trace_options(true, options.follow),
    )
    .map_err(|errno| TraceError::Attach { pid, errno })?;

    seed_descriptors(pid, sink);
    sink.attached(pid);
    follow(
        Mode::Filtered,
        pid,
        BTreeSet::from([pid]),
        true,
        options,
        stop,
        sink,
    )
}

fn follow(
    mode: Mode,
    first: u32,
    initial: BTreeSet<u32>,
    launched: bool,
    options: &TraceOptions,
    stop: &AtomicBool,
    sink: &mut dyn Sink,
) -> Result<Finished, TraceError> {
    let mut tracer = Tracer {
        mode,
        options,
        threads: BTreeMap::new(),
        initial,
        unannounced: BTreeMap::new(),
        reported: BTreeSet::from([first]),
        first,
        end: None,
        detaching: false,
        sink,
    };

    let result = run(&mut tracer, stop, launched);

    if launched && result.is_err() {
        for &tid in tracer.threads.keys() {
            let _ = nix::sys::signal::kill(
                Pid::from_raw(tid.cast_signed()),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }

    result
}

const TICK: Duration = Duration::from_millis(100);

struct Ticker {
    done: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

const extern "C" fn woken(_: libc::c_int) {}

impl Ticker {
    #[allow(
        unsafe_code,
        reason = "installing a signal handler is unsafe; this one does nothing"
    )]
    fn start() -> Result<Self, TraceError> {
        use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal as Native};

        let action = SigAction::new(
            SigHandler::Handler(woken),
            SaFlags::empty(),
            SigSet::empty(),
        );

        // SAFETY: the handler does nothing, so it is async-signal-safe, and
        // nothing else in this process handles SIGURG.
        unsafe { nix::sys::signal::sigaction(Native::SIGURG, &action) }
            .map_err(TraceError::System)?;

        let (process, tracer) = (nix::unistd::getpid(), nix::unistd::gettid());
        let done = Arc::new(AtomicBool::new(false));
        let ended = Arc::clone(&done);
        let thread = std::thread::Builder::new()
            .name("microscope-ticker".into())
            .spawn(move || {
                loop {
                    std::thread::park_timeout(TICK);
                    if ended.load(Ordering::Relaxed) {
                        break;
                    }

                    let _ = tgkill(process, tracer, libc::SIGURG);
                }
            })
            .map_err(|_| TraceError::System(Errno::EAGAIN))?;

        Ok(Self {
            done,
            thread: Some(thread),
        })
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}

fn run(tracer: &mut Tracer<'_>, stop: &AtomicBool, launched: bool) -> Result<Finished, TraceError> {
    let mut stopping = false;
    let mut reported = monotonic();
    let _ticker = Ticker::start()?;
    loop {
        if monotonic() - reported > 100_000_000 {
            tracer.report_in_flight();
            reported = monotonic();
        }

        if stop.load(Ordering::Relaxed) && !stopping {
            stopping = true;
            if launched {
                for &tid in tracer.threads.keys() {
                    let _ = nix::sys::signal::kill(
                        Pid::from_raw(tid.cast_signed()),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }

                let _ = nix::sys::signal::kill(
                    Pid::from_raw(tracer.first.cast_signed()),
                    nix::sys::signal::Signal::SIGKILL,
                );
            } else {
                tracer.begin_detach();
                if tracer.threads.is_empty() {
                    break;
                }
            }
        }

        match wait_for(Pid::from_raw(-1), libc::__WALL) {
            Ok(Some(event)) => {
                tracer.handle(event)?;
                if tracer.detaching && tracer.threads.is_empty() {
                    break;
                }
            }

            Ok(None) | Err(Errno::EINTR) => {}
            Err(Errno::ECHILD) => break,
            Err(error) => return Err(TraceError::System(error)),
        }
    }

    let threads = std::mem::take(&mut tracer.threads);
    for (tid, thread) in threads {
        tracer.unfinished(tid, thread, Outcome::Unfinished);
    }

    Ok(Finished { end: tracer.end })
}

pub fn seed_descriptors(process: u32, sink: &mut dyn Sink) {
    let Ok(entries) = std::fs::read_dir(format!("/proc/{process}/fd")) else {
        return;
    };

    let time = monotonic();
    let mut fds: Vec<i64> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
        .collect();
    fds.sort_unstable();
    for fd in std::iter::once(AT_FDCWD).chain(fds) {
        if let Some(event) = descriptor(process, fd, time, false) {
            sink.event(event);
        }
    }
}
