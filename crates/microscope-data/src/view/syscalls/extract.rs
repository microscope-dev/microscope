use std::sync::Arc;

use crate::capture::Capture;
use crate::capture::records::{
    self, AccountKind, CallOutcome, NONE, NoteKind, UNSEEN, note_flags as flags,
};
use crate::syscalls::stats::{Folded, SyscallStats};
use crate::syscalls::{
    Abi, Device, Errno, EventFlags, Loss, Losses, Outcome, Pid, ProcessEnd, SyscallEvent,
    SyscallId, Tid, TraceEvent,
};

use super::RecordedEvent;

pub(super) struct RecordedSyscalls {
    pub(super) events: Vec<RecordedEvent>,
    pub(super) counted: Folded,
    pub(super) lost: Losses,
}

pub(in crate::view) fn syscalls_lost(capture: &Capture) -> Losses {
    let account = |kind: AccountKind| capture.account(kind as u32);
    Losses {
        dropped: account(AccountKind::SyscallsLost),
        unseen: account(AccountKind::SyscallsUnseen),
        capped: account(AccountKind::SyscallsCapped),
        overflowed: account(AccountKind::SyscallsOverflowed),
        skipped: account(AccountKind::SyscallsSkipped),
    }
}

pub(in crate::view) fn syscall_count(capture: &Capture) -> u64 {
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

pub(in crate::view) fn syscall_name(capture: &Capture, row: u32) -> String {
    capture.syscalls.get(row as usize).map_or_else(
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

pub(super) fn extract_syscalls(capture: &Capture) -> RecordedSyscalls {
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
            events.push(decode_note(capture, note, &ids, &pid, &blob));
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
        events.push(decode_note(capture, note, &ids, &pid, &blob));
    }

    RecordedSyscalls {
        events,
        counted: kernel_counted(capture, &pid),
        lost: syscalls_lost(capture),
    }
}

fn kernel_counted(capture: &Capture, pid: &impl Fn(u32) -> Pid) -> Folded {
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

fn decode_note<'a>(
    capture: &Capture,
    note: &records::SyscallNote,
    ids: &impl Fn(u32) -> (Tid, Pid),
    pid: &impl Fn(u32) -> Pid,
    blob: &impl Fn(u32, u32) -> &'a [u8],
) -> RecordedEvent {
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
