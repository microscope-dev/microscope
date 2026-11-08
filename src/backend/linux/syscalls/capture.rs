//! Records a system call trace in a capture, the profiler's file, on its
//! clock: `CLOCK_MONOTONIC` nanoseconds.

use crate::capture::builder::CaptureBuilder;
use crate::capture::records::{
    AccountKind, CallOutcome, NONE, NoteKind, Syscall, SyscallErrno, SyscallNote, SyscallStat,
    UNSEEN, note_flags as flags,
};

use zerocopy::little_endian::{I64, U32, U64};

use crate::profiler::Profile;
use crate::syscalls::stats::Folded;
use crate::syscalls::{
    CallStack, CaptureState, InFlight, Outcome, Pid, ProcessEnd, ReadAt, SyscallEvent, TraceEvent,
};

use super::Sink;
use super::stacks::SharedStacks;

/// Turns a trace's events into a capture's rows as they come.
pub struct SyscallCapture {
    builder: CaptureBuilder,
    stacks: Option<SharedStacks>,
    first: Option<u64>,
    last: u64,
    blob: Vec<u8>,
}

impl std::fmt::Debug for SyscallCapture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SyscallCapture")
            .finish_non_exhaustive()
    }
}

const fn signed(time: u64) -> i64 {
    time.cast_signed()
}

/// A note at `time` that says nothing yet.
const fn blank(time: i64) -> SyscallNote {
    SyscallNote {
        time: I64::new(time),
        value: I64::new(0),
        extra: U64::new(0),
        thread: U32::new(NONE),
        process: U32::new(NONE),
        text: U32::new(NONE),
        stack: U32::new(NONE),
        blob: U32::new(NONE),
        blob_len: U32::new(0),
        calls: U32::new(0),
        kind: 0,
        flags: 0,
        reserved: [0; 10],
    }
}

/// An end as `wait(2)` encodes it.
const fn status(end: ProcessEnd) -> i64 {
    match end {
        ProcessEnd::Exited(code) => ((code & 0xff) << 8) as i64,
        ProcessEnd::Killed { signal, core } => (signal | if core { 0x80 } else { 0 }) as i64,
    }
}

impl SyscallCapture {
    /// A capture that names stacks' frames through `stacks`, which the
    /// engine unwinds them with.
    #[must_use]
    pub fn new(stacks: Option<SharedStacks>) -> Self {
        Self {
            builder: CaptureBuilder::new(),
            stacks,
            first: None,
            last: 0,
            blob: Vec::new(),
        }
    }

    /// Records a setting, such as the command traced.
    pub fn setting(&mut self, key: &str, value: &str) {
        self.builder.setting(key, value);
    }

    /// Says the capture spans `start` to `end`, whatever its events do.
    pub fn span(&mut self, start: u64, end: u64) {
        self.saw(start);
        self.saw(end);
    }

    fn saw(&mut self, time: u64) {
        if time != 0 {
            self.first = Some(self.first.map_or(time, |first| first.min(time)));
            self.last = self.last.max(time);
        }
    }

    fn stack(&mut self, process: Pid, stack: Option<&CallStack>) -> u32 {
        match (stack, &self.stacks) {
            (Some(stack), Some(stacks)) => stacks.lock().map_or(NONE, |mut stacks| {
                stacks.capture(process, stack, &mut self.builder)
            }),
            _ => NONE,
        }
    }

    /// Records one event.
    pub fn event(&mut self, event: &TraceEvent) {
        let time = event.time();
        self.saw(time);
        let at = signed(time);
        let mut note = blank(at);
        let mut bytes: &[u8] = &[];
        match event {
            TraceEvent::Call(call) => return self.call(call),
            TraceEvent::Signal {
                thread,
                process,
                signal,
                code,
                sender,
                info,
                stack,
                ..
            } => {
                note.kind = NoteKind::Signal as u8;
                note.thread = self.builder.thread(*process, *thread, at).into();
                note.process = self.builder.process(*process, at).into();
                note.value = i64::from(*signal).into();
                note.stack = self.stack(*process, stack.as_ref()).into();
                let code = code.map_or(0, |code| {
                    note.flags |= flags::SIGNAL_CODE;
                    u64::from(code.cast_unsigned())
                });
                let sender = sender.map_or(0, |sender| {
                    note.flags |= flags::SIGNAL_SENDER;
                    u64::from(sender.get())
                });
                note.extra = (code | sender << 32).into();
                bytes = info.as_deref().unwrap_or_default();
            }

            TraceEvent::Descriptor {
                process,
                fd,
                text,
                cloexec,
                device,
                made,
                ..
            } => {
                note.kind = NoteKind::Descriptor as u8;
                note.process = self.builder.process(*process, at).into();
                note.value = (*fd).into();
                note.text = self.builder.string(text).into();
                for (set, flag) in [
                    (*cloexec, flags::FD_CLOEXEC),
                    (*made, flags::FD_MADE),
                    (device.is_some(), flags::FD_DEVICE),
                ] {
                    if set {
                        note.flags |= flag;
                    }
                }

                if let Some(device) = device {
                    note.extra = (u64::from(device.block) << 63
                        | u64::from(device.major) << 32
                        | u64::from(device.minor))
                    .into();
                }
            }
            _ => self.life(event, &mut note, at),
        }

        self.builder.syscall_note(note, bytes);
    }

    /// A note of a process or thread begun or ended, or of records lost.
    fn life(&mut self, event: &TraceEvent, note: &mut SyscallNote, at: i64) {
        match event {
            TraceEvent::Spawn {
                parent_thread,
                parent,
                child,
                thread,
                ..
            } => {
                note.kind = NoteKind::Spawn as u8;
                note.thread = self.builder.thread(*parent, *parent_thread, at).into();
                note.process = self.builder.process(*parent, at).into();
                note.value = i64::from(child.get()).into();
                if *thread {
                    note.flags = flags::SPAWN_THREAD;
                    self.builder.thread(*parent, *child, at);
                } else {
                    let process = Pid::led_by(*child);
                    self.builder.thread(process, *child, at);
                    if let Some(stacks) = &self.stacks
                        && let Ok(mut stacks) = stacks.lock()
                    {
                        stacks.forked(*parent, process);
                    }
                }
            }

            TraceEvent::Exec {
                thread,
                process,
                path,
                ..
            } => {
                note.kind = NoteKind::Exec as u8;
                note.thread = self.builder.thread(*process, *thread, at).into();
                note.process = self.builder.process(*process, at).into();
                note.text = self.builder.string(path).into();
                let name = path.rsplit('/').next().unwrap_or_default();
                self.builder.process_name(note.process.get(), name);
            }
            TraceEvent::Exit { process, end, .. } => {
                note.kind = NoteKind::Exit as u8;
                note.process = self.builder.process(*process, at).into();
                note.value = status(*end).into();
                self.builder.process_exit(*process, at);
            }

            TraceEvent::ThreadExit {
                thread,
                process,
                end,
                ..
            } => {
                note.kind = NoteKind::ThreadExit as u8;
                note.thread = self.builder.thread(*process, *thread, at).into();
                note.process = self.builder.process(*process, at).into();
                note.value = status(*end).into();
                self.builder.thread_exit(*thread, at);
            }
            TraceEvent::Lost { count, .. } => {
                note.kind = NoteKind::Lost as u8;
                note.value = count.cast_signed().into();
                self.builder.count(AccountKind::SyscallsLost as u32, *count);
            }
            TraceEvent::Call(_) | TraceEvent::Signal { .. } | TraceEvent::Descriptor { .. } => {}
        }
    }

    /// Records one call.
    pub fn call(&mut self, call: &SyscallEvent) {
        let at = signed(call.time());
        let thread = self.builder.thread(call.process, call.thread, at);
        let stack = self.stack(call.process, call.stack.as_ref());
        let (outcome, ret) = match call.outcome {
            Outcome::Returned(value) => (CallOutcome::Returned, value),
            Outcome::Failed(errno) => (CallOutcome::Failed, -i64::from(errno.0)),
            Outcome::Unfinished => (CallOutcome::Unfinished, 0),
            Outcome::ProcessExited => (CallOutcome::ProcessExited, 0),
            Outcome::Unavailable => (CallOutcome::Unavailable, 0),
        };

        self.blob.clear();
        for capture in &call.captured {
            let (state, wanted) = match capture.state {
                CaptureState::Complete => (0, 0),
                CaptureState::Truncated { wanted } => (1, wanted),
                CaptureState::Unreadable => (2, 0),
                CaptureState::NotRequested => (3, 0),
            };

            let when = match capture.when {
                ReadAt::Entry => 0,
                ReadAt::Exit => 1,
            };

            let bytes = capture
                .bytes
                .get(..u32::MAX as usize)
                .unwrap_or(&*capture.bytes);
            let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
            self.blob.extend_from_slice(&[capture.arg, when, state, 0]);
            self.blob.extend_from_slice(&wanted.to_le_bytes());
            self.blob.extend_from_slice(&len.to_le_bytes());
            self.blob.extend_from_slice(bytes);
        }

        let time = |time: Option<u64>| time.map_or(UNSEEN, signed);
        let row = Syscall {
            entered: time(call.entered).into(),
            exited: time(call.exited).into(),
            ret: ret.into(),
            args: call.args.map(Into::into),
            thread: thread.into(),
            number: call.call.number.into(),
            stack: stack.into(),
            captures: NONE.into(),
            captures_len: 0.into(),
            abi: call.call.abi as u8,
            outcome: outcome as u8,
            flags: call.flags.0,
            reserved: 0,
        };

        if let Some(entered) = call.entered {
            self.saw(entered);
        }

        let blob = std::mem::take(&mut self.blob);
        self.builder.syscall(row, &blob);
        self.blob = blob;
    }

    /// Records statistics the kernel counted, for a trace that kept no
    /// calls.
    pub fn counted(&mut self, stats: &Folded) {
        let at = signed(self.last);
        for (&(process, call), stats) in &stats.by_call {
            let errnos: Vec<SyscallErrno> = stats
                .errors_by_errno
                .iter()
                .map(|(&errno, &count)| SyscallErrno {
                    count: count.into(),
                    errno: errno.into(),
                    reserved: [0; 6],
                })
                .collect();
            let mut log2_ns = [0.into(); crate::syscalls::stats::BUCKETS];
            for (bucket, &count) in log2_ns.iter_mut().zip(&stats.log2_ns) {
                *bucket = count.into();
            }

            let row = SyscallStat {
                calls: stats.calls.into(),
                errors: stats.errors.into(),
                timed: stats.timed.into(),
                total_ns: stats.total_ns.into(),
                min_ns: stats.min_ns.into(),
                max_ns: stats.max_ns.into(),
                bytes: stats.bytes.into(),
                log2_ns,
                process: self.builder.process(process, at).into(),
                number: call.number.into(),
                errnos: 0.into(),
                errno_count: 0.into(),
                abi: call.abi as u8,
                reserved: [0; 7],
            };

            self.builder.syscall_stat(row, &errnos);
        }
    }

    /// The capture so far, as a profile to show while the trace goes on.
    #[must_use]
    pub fn snapshot(&self) -> Profile {
        Self {
            builder: self.builder.clone(),
            stacks: None,
            first: self.first,
            last: self.last,
            blob: Vec::new(),
        }
        .finish()
    }

    /// The capture, as a profile to write or show.
    #[must_use]
    pub fn finish(mut self) -> Profile {
        let first = self.first.unwrap_or(0);
        self.builder.setting("start", &first.to_string());
        self.builder.setting("end", &self.last.to_string());
        Profile::from_capture(self.builder.finish())
    }
}

impl Sink for SyscallCapture {
    fn event(&mut self, event: TraceEvent) {
        Self::event(self, &event);
    }

    fn in_flight(&mut self, _calls: &[InFlight]) {}

    fn counted(&mut self, stats: Folded) {
        Self::counted(self, &stats);
    }
}
