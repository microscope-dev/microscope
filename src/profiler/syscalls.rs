use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use super::{Profile, StackEnding};
use crate::capture::records::{
    self, AccountKind, CallOutcome, FrameKind, NONE, NoteKind, UNSEEN, note_flags as flags,
};

use crate::syscalls::stats::{Folded, SyscallStats};
use crate::syscalls::{
    Abi, Device, Errno, EventFlags, Loss, Losses, Outcome, Pid, ProcessEnd, SyscallEvent,
    SyscallId, Tid, TraceEvent,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedEvent {
    pub event: TraceEvent,

    pub stack: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecordedSyscalls {
    pub events: Vec<RecordedEvent>,

    pub counted: Folded,

    pub lost: Losses,
}

impl RecordedSyscalls {
    #[must_use]
    pub fn stats(&self) -> Folded {
        let mut folded = self.counted.clone();

        for event in &self.events {
            if let TraceEvent::Call(call) = &event.event
                && !call.flags.kept()
            {
                folded.add(call);
            }
        }

        folded
    }
}

#[expect(clippy::cast_possible_truncation, reason = "a status fits 16 bits")]
const fn end(status: i64) -> ProcessEnd {
    let status = status as i32;
    let signal = status & 0x7f;
    if signal == 0 {
        ProcessEnd::Exited((status >> 8) & 0xff)
    } else {
        ProcessEnd::Killed {
            signal,
            core: status & 0x80 != 0,
        }
    }
}

impl Profile {
    #[must_use]
    pub fn has_syscalls(&self) -> bool {
        let capture = &self.capture;
        !capture.syscalls.is_empty()
            || !capture.syscall_notes.is_empty()
            || !capture.syscall_stats.is_empty()
    }

    #[must_use]
    pub fn syscalls_lost(&self) -> Losses {
        let account = |kind: AccountKind| self.capture.account(kind as u32);
        Losses {
            dropped: account(AccountKind::SyscallsLost),
            unseen: account(AccountKind::SyscallsUnseen),
            capped: account(AccountKind::SyscallsCapped),
            overflowed: account(AccountKind::SyscallsOverflowed),
            skipped: account(AccountKind::SyscallsSkipped),
        }
    }

    #[must_use]
    pub fn syscall_count(&self) -> u64 {
        let capture = &self.capture;
        let kept = EventFlags::KEPT_FAILED.0 | EventFlags::KEPT_SLOW.0;
        capture
            .syscalls
            .iter()
            .filter(|call| call.flags & kept == 0)
            .count() as u64
            + capture
                .syscall_stats
                .iter()
                .map(|stats| stats.calls.get())
                .sum::<u64>()
    }

    #[must_use]
    pub fn syscalls(&self) -> RecordedSyscalls {
        let capture = &self.capture;
        let ids = |thread: u32| {
            capture
                .threads
                .get(thread as usize)
                .map_or((Tid::new(0), Pid::new(0)), |row| {
                    let pid = capture.process(row.process.get()).pid.get();
                    (Tid::new(row.tid.get()), Pid::new(pid))
                })
        };

        let pid = |process: u32| {
            capture
                .processes
                .get(process as usize)
                .map_or(Pid::new(0), |row| Pid::new(row.pid.get()))
        };

        let blob = |start: u32, len: u32| capture.blob(start, len);

        let stack = |row: u32| (row != NONE).then_some(row);
        let mut events = Vec::with_capacity(capture.syscalls.len() + capture.syscall_notes.len());
        let mut notes = capture.syscall_notes.iter().peekable();

        for (index, call) in capture.syscalls.iter().enumerate() {
            while let Some(note) = notes.next_if(|note| note.calls.get() as usize <= index) {
                events.push(self.note(note, &ids, &pid, &blob));
            }

            let (thread, process) = ids(call.thread.get());
            let outcome = match CallOutcome::from_number(call.outcome) {
                Some(CallOutcome::Returned) => Outcome::Returned(call.ret.get()),
                Some(CallOutcome::Failed) => {
                    Outcome::Failed(Errno(u16::try_from(-call.ret.get()).unwrap_or(0)))
                }
                Some(CallOutcome::Unfinished) => Outcome::Unfinished,
                Some(CallOutcome::ProcessExited) => Outcome::ProcessExited,
                Some(CallOutcome::Unavailable) | None => Outcome::Unavailable,
            };

            let time = |time: i64| (time != UNSEEN).then(|| time.cast_unsigned());
            events.push(RecordedEvent {
                event: TraceEvent::Call(SyscallEvent {
                    thread,
                    process,
                    call: SyscallId {
                        abi: Abi::from_index(call.abi),
                        number: call.number.get(),
                    },
                    args: call.args.map(zerocopy::little_endian::U64::get),
                    captured: records::captures(blob(call.captures.get(), call.captures_len.get()))
                        .unwrap_or_default(),
                    outcome,
                    entered: time(call.entered.get()),
                    exited: time(call.exited.get()),
                    stack: None,
                    flags: EventFlags(call.flags),
                }),
                stack: stack(call.stack.get()),
            });
        }

        for note in notes {
            events.push(self.note(note, &ids, &pid, &blob));
        }

        RecordedSyscalls {
            events,
            counted: self.kernel_counted(&pid),
            lost: self.syscalls_lost(),
        }
    }

    #[must_use]
    pub fn syscall_name(&self, row: u32) -> String {
        self.capture.syscalls.get(row as usize).map_or_else(
            || "?".to_owned(),
            |call| {
                SyscallId {
                    abi: Abi::from_index(call.abi),
                    number: call.number.get(),
                }
                .display_name()
                .into_owned()
            },
        )
    }

    #[must_use]
    pub fn syscall_site_name(&self, row: u32) -> String {
        self.syscall_site(row).unwrap_or_else(|| {
            self.syscall_stack(row)
                .0
                .into_iter()
                .next()
                .unwrap_or_else(|| "?".to_owned())
        })
    }

    #[must_use]
    pub fn syscall_sites(&self, syscalls: &RecordedSyscalls) -> BTreeMap<String, SyscallStats> {
        let mut names: HashMap<u32, String> = HashMap::new();
        let mut sites: BTreeMap<String, SyscallStats> = BTreeMap::new();

        for event in &syscalls.events {
            let TraceEvent::Call(call) = &event.event else {
                continue;
            };

            let site = event.stack.map_or_else(
                || "no stack".to_owned(),
                |row| {
                    names
                        .entry(row)
                        .or_insert_with(|| self.syscall_site_name(row))
                        .clone()
                },
            );
            sites.entry(site).or_default().add(
                call.outcome,
                call.duration(),
                crate::syscalls::stats::bytes_moved(call),
            );
        }

        sites
    }

    fn kernel_counted(&self, pid: &impl Fn(u32) -> Pid) -> Folded {
        let capture = &self.capture;
        let mut counted = Folded::default();

        for row in &capture.syscall_stats {
            let errnos = capture.stat_errnos(row);
            let stats = SyscallStats {
                calls: row.calls.get(),
                errors: row.errors.get(),
                errors_by_errno: errnos
                    .iter()
                    .map(|errno| (errno.errno.get(), errno.count.get()))
                    .collect(),
                timed: row.timed.get(),
                total_ns: row.total_ns.get(),
                min_ns: row.min_ns.get(),
                max_ns: row.max_ns.get(),
                log2_ns: if row.timed.get() == 0 {
                    Vec::new()
                } else {
                    row.log2_ns.iter().map(|count| count.get()).collect()
                },
                bytes: row.bytes.get(),
            };

            let call = SyscallId {
                abi: Abi::from_index(row.abi),
                number: row.number.get(),
            };

            let thread = capture
                .threads
                .get(row.thread.get() as usize)
                .map_or(Tid::new(0), |thread| Tid::new(thread.tid.get()));
            counted
                .by_call
                .entry((pid(row.process.get()), thread, call))
                .or_default()
                .merge(&stats);
        }

        counted
    }

    fn note<'a>(
        &self,
        note: &records::SyscallNote,
        ids: &impl Fn(u32) -> (Tid, Pid),
        pid: &impl Fn(u32) -> Pid,
        blob: &impl Fn(u32, u32) -> &'a [u8],
    ) -> RecordedEvent {
        let capture = &self.capture;
        let time = note.time.get().cast_unsigned();
        let (thread, _) = ids(note.thread.get());
        let process = pid(note.process.get());
        let value = note.value.get();
        let text = || -> Arc<str> {
            if note.text.get() == NONE {
                Arc::from("")
            } else {
                Arc::from(capture.string(note.text.get()))
            }
        };

        let small = |value: i64| i32::try_from(value).unwrap_or(0);
        let extra = note.extra.get();
        let event = match NoteKind::from_number(note.kind) {
            Some(NoteKind::Signal) => {
                let bytes = blob(note.blob.get(), note.blob_len.get());
                TraceEvent::Signal {
                    thread,
                    process,
                    time,
                    signal: small(value),

                    #[expect(clippy::cast_possible_truncation, reason = "the low half")]
                    code: (note.flags & flags::SIGNAL_CODE != 0)
                        .then_some((extra as u32).cast_signed()),
                    sender: (note.flags & flags::SIGNAL_SENDER != 0)
                        .then(|| Pid::new(u32::try_from(extra >> 32).unwrap_or(0))),
                    info: (!bytes.is_empty()).then(|| bytes.into()),
                    stack: None,
                }
            }
            Some(NoteKind::Spawn) => TraceEvent::Spawn {
                parent_thread: thread,
                parent: process,
                child: Tid::new(u32::try_from(value).unwrap_or(0)),
                thread: note.flags & flags::SPAWN_THREAD != 0,
                time,
            },
            Some(NoteKind::Exec) => TraceEvent::Exec {
                thread,
                process,
                time,
                path: text(),
            },
            Some(NoteKind::Exit) => TraceEvent::Exit {
                process,
                time,
                end: end(value),
            },
            Some(NoteKind::ThreadExit) => TraceEvent::ThreadExit {
                thread,
                process,
                time,
                end: end(value),
            },
            Some(NoteKind::Descriptor) => TraceEvent::Descriptor {
                process,
                fd: value,
                time,
                text: text(),
                cloexec: note.flags & flags::FD_CLOEXEC != 0,

                #[expect(clippy::cast_possible_truncation, reason = "each part's bits")]
                device: (note.flags & flags::FD_DEVICE != 0).then_some(Device {
                    block: extra >> 63 != 0,
                    major: ((extra >> 32) as u32) & 0x7fff_ffff,
                    minor: extra as u32,
                }),
                made: note.flags & flags::FD_MADE != 0,
            },
            Some(NoteKind::Lost) | None => TraceEvent::Lost {
                time,
                count: value.cast_unsigned(),
                why: match extra {
                    1 => Loss::Unseen,
                    2 => Loss::Capped,
                    3 => Loss::Overflowed,
                    4 => Loss::Skipped,
                    _ => Loss::Dropped,
                },
            },
        };

        RecordedEvent {
            event,
            stack: (note.stack.get() != NONE).then_some(note.stack.get()),
        }
    }

    #[must_use]
    pub fn syscall_stack(&self, row: u32) -> (Vec<String>, StackEnding) {
        self.names(row)
    }

    #[must_use]
    pub fn syscall_frames(&self, row: u32) -> Vec<SyscallFrame> {
        let capture = &self.capture;
        let mut frames = Vec::new();
        for frame in capture.stack_frames(row) {
            let record = capture.frame(frame);
            if record.kind == FrameKind::Truncated as u8 {
                continue;
            }

            let interpreted = record.kind == FrameKind::Interpreted as u8;
            let lines = capture.frame_lines(frame);
            if lines.is_empty() {
                frames.push(SyscallFrame {
                    function: self.address_name(frame),
                    place: None,
                    interpreted,
                });
            }

            for line in lines {
                let file = line.file.get();
                frames.push(SyscallFrame {
                    function: super::function_name(capture, line.function.get()).to_owned(),
                    place: (file != NONE)
                        .then(|| (capture.string(file).to_owned(), line.number.get())),
                    interpreted,
                });
            }
        }

        frames
    }

    #[must_use]
    pub fn syscall_site(&self, row: u32) -> Option<String> {
        let Some(place) = self.syscall_place(row) else {
            return self.caller_of_c_library(row);
        };
        let file = place.file.rsplit('/').next().unwrap_or(place.file);
        Some(format!("{} · {file}:{}", place.function, place.line))
    }

    fn caller_of_c_library(&self, row: u32) -> Option<String> {
        let capture = &self.capture;
        let names: Vec<String> = capture
            .stack_frames(row)
            .filter(|&frame| {
                let record = capture.frame(frame);
                let module = record.module.get();
                record.kind != FrameKind::Truncated as u8
                    && (module == NONE || {
                        let path = capture.string(capture.module(module).path.get());
                        let name = path.rsplit('/').next().unwrap_or(path);
                        !C_LIBRARY.iter().any(|prefix| name.starts_with(prefix))
                    })
            })
            .flat_map(|frame| {
                let lines = capture.frame_lines(frame);
                if lines.is_empty() {
                    return vec![self.address_name(frame)];
                }
                lines
                    .iter()
                    .map(|line| super::function_name(capture, line.function.get()).to_owned())
                    .collect()
            })
            .collect();

        let own = |name: &&String| !crate::runtime_model::rust::standard_library_function(name);
        names.iter().find(own).or_else(|| names.first()).cloned()
    }

    #[must_use]
    pub fn syscall_place(&self, row: u32) -> Option<SyscallPlace<'_>> {
        let capture = &self.capture;
        let interpreted = |frame: &u32| {
            capture
                .frames
                .get(*frame as usize)
                .is_some_and(|frame| frame.kind == FrameKind::Interpreted as u8)
        };
        let place = |frame: u32| {
            let line = capture.frame_lines(frame).iter().find(|line| {
                line.file.get() != NONE
                    && !crate::runtime_model::rust::standard_library(std::path::Path::new(
                        capture.string(line.file.get()),
                    ))
            })?;
            Some(SyscallPlace {
                function: super::function_name(capture, line.function.get()),
                file: capture.string(line.file.get()),
                line: line.number.get(),
            })
        };

        capture
            .stack_frames(row)
            .filter(interpreted)
            .find_map(place)
            .or_else(|| capture.stack_frames(row).find_map(place))
    }
}

const C_LIBRARY: [&str; 5] = [
    "libc.so",
    "libpthread.so",
    "ld-linux",
    "ld-musl",
    "libc.musl",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyscallFrame {
    pub function: String,
    pub place: Option<(String, u32)>,
    pub interpreted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyscallPlace<'a> {
    pub function: &'a str,
    pub file: &'a str,
    pub line: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syscall_engines::capture::SyscallCapture;
    use crate::syscalls::{Capture, CaptureState, ReadAt};

    fn call(thread: u32, number: u32, outcome: Outcome, at: u64) -> TraceEvent {
        TraceEvent::Call(SyscallEvent {
            thread: Tid::new(thread),
            process: Pid::new(10),
            call: SyscallId {
                abi: Abi::X86_32,
                number,
            },
            args: [1, 2, 3, 4, 5, u64::MAX],
            captured: vec![
                Capture {
                    arg: 1,
                    when: ReadAt::Entry,
                    state: CaptureState::Truncated { wanted: 40 },
                    bytes: b"/etc/pas".as_slice().into(),
                },
                Capture {
                    arg: 2,
                    when: ReadAt::Exit,
                    state: CaptureState::Unreadable,
                    bytes: [].as_slice().into(),
                },
            ],
            outcome,
            entered: Some(at - 5),
            exited: (outcome != Outcome::Unfinished).then_some(at),
            stack: None,
            flags: EventFlags::SPANNED_STOP,
        })
    }

    fn trace() -> Vec<TraceEvent> {
        vec![
            TraceEvent::Exec {
                thread: Tid::new(10),
                process: Pid::new(10),
                time: 100,
                path: "/bin/app".into(),
            },
            TraceEvent::Descriptor {
                process: Pid::new(10),
                fd: 3,
                time: 300,
                text: "/etc/passwd".into(),
                cloexec: true,
                device: Some(Device {
                    block: true,
                    major: 8,
                    minor: 1,
                }),
                made: true,
            },
            call(10, 257, Outcome::Returned(3), 200),
            TraceEvent::Spawn {
                parent_thread: Tid::new(10),
                parent: Pid::new(10),
                child: Tid::new(11),
                thread: true,
                time: 210,
            },
            call(11, 0, Outcome::Failed(Errno(4)), 220),
            TraceEvent::Signal {
                thread: Tid::new(11),
                process: Pid::new(10),
                time: 230,
                signal: 10,
                code: Some(-6),
                sender: Some(Pid::new(10)),
                info: Some([7; 128].as_slice().into()),
                stack: None,
            },
            TraceEvent::Lost {
                time: 240,
                count: 3,
                why: Loss::Capped,
            },
            TraceEvent::Lost {
                time: 245,
                count: 5,
                why: Loss::Overflowed,
            },
            TraceEvent::ThreadExit {
                thread: Tid::new(11),
                process: Pid::new(10),
                time: 250,
                end: ProcessEnd::Exited(0),
            },
            call(10, 231, Outcome::ProcessExited, 260),
            TraceEvent::Exit {
                process: Pid::new(10),
                time: 270,
                end: ProcessEnd::Killed {
                    signal: 9,
                    core: true,
                },
            },
            call(10, 7, Outcome::Unfinished, 268),
        ]
    }

    fn events(profile: &Profile) -> Vec<TraceEvent> {
        let syscalls = profile.syscalls();
        syscalls
            .events
            .into_iter()
            .map(|event| event.event)
            .collect()
    }

    #[test]
    fn a_trace_reads_back_from_its_file_as_it_was_reported() {
        let mut recorder = SyscallCapture::new(None);

        for event in trace() {
            recorder.event(&event);
        }

        let mut counted = Folded::default();

        for event in trace() {
            if let TraceEvent::Call(call) = event {
                counted.add(&call);
            }
        }

        recorder.counted(&counted);
        let bytes = recorder.finish().to_bytes().unwrap();
        let read = Profile::from_bytes(&bytes).expect("the file reads").profile;
        assert_eq!(events(&read), trace());
        let syscalls = read.syscalls();
        let lost = Losses {
            capped: 3,
            overflowed: 5,
            ..Losses::default()
        };
        assert_eq!((syscalls.lost, &syscalls.counted), (lost, &counted));

        let mut builder = crate::capture::builder::CaptureBuilder::new();
        builder.absorb(&read.capture);
        builder.forget_before(215);
        let windowed = Profile::from_capture(builder.finish());
        let kept: Vec<TraceEvent> = trace()
            .into_iter()
            .filter(|event| event.time() >= 215)
            .collect();
        assert_eq!(events(&windowed), kept);

        assert_eq!(windowed.capture.syscall_blobs.len(), 3 * (24 + 8) + 128);
    }

    #[test]
    fn samples_and_calls_of_one_thread_merge_into_its_one_row() {
        let mut samples = crate::capture::builder::CaptureBuilder::new();

        for (tid, start) in [(10, 50), (11, 205)] {
            let thread = samples.thread(Pid::new(10), Tid::new(tid), start);
            samples.sample(thread, start + 1, 1, NONE, NONE);
        }

        samples.process_exit(Pid::new(10), 400);
        let mut recorder = SyscallCapture::new(None);

        for event in trace() {
            recorder.event(&event);
        }

        let mut builder = crate::capture::builder::CaptureBuilder::new();
        builder.absorb(&samples.finish());
        builder.absorb(&recorder.finish().capture);
        let merged = Profile::from_capture(builder.finish());
        let threads: Vec<(u32, u32, u64)> = merged
            .threads()
            .iter()
            .map(|thread| (thread.pid, thread.tid, thread.samples))
            .collect();
        assert_eq!(threads, [(10, 10, 1), (10, 11, 1)]);
        let rows = &merged.capture;
        assert_eq!((rows.processes.len(), rows.threads.len()), (1, 2));
        assert_eq!(events(&merged), trace());
    }
}
