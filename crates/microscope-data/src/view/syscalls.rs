use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path};
use std::sync::Arc;

use crate::analysis::Keys;
use crate::capture::Capture;
use crate::capture::records::{
    self, AccountKind, CallOutcome, FrameKind, NONE, NoteKind, Truncation, UNSEEN,
    note_flags as flags,
};
use crate::syscalls::fds::{FdTable, Kind as FdKind};
use crate::syscalls::filter::{self, Filter};
use crate::syscalls::present::{self, Context, Tokens};
use crate::syscalls::procs::ProcessTree;
use crate::syscalls::specs::{self, Class};
use crate::syscalls::stats::{self, Folded, SyscallStats, bytes_moved};
use crate::syscalls::{
    Abi, Device, Errno, EventFlags, Loss, Losses, Outcome, Pid, ProcessEnd, SyscallEvent,
    SyscallId, Tid, TraceEvent,
};

use super::protocol::{
    ProfileDuring, SyscallArgument, SyscallBlocked, SyscallCall, SyscallCallQuery, SyscallCaller,
    SyscallCount, SyscallDetail, SyscallDetailQuery, SyscallEndpoint, SyscallFile, SyscallFiles,
    SyscallGroup, SyscallHeatmap, SyscallHeatmapQuery, SyscallInFlight, SyscallLine, SyscallLog,
    SyscallLogQuery, SyscallNetwork, SyscallProcess, SyscallProcesses, SyscallQuery, SyscallRow,
    SyscallSite, SyscallSummary, SyscallSummaryQuery, SyscallToken,
};
use super::{SourceLines, ViewError};

const AT_FDCWD: i64 = -100;
const MOST_BUCKETS: u32 = 4096;
const MOST_LINES: u32 = 2000;
const SITE_MARGIN: u64 = 3;
const BLOCKED_AT: &str = "syscalls blocked at";

const CLASSES: [Class; 8] = [
    Class::Net,
    Class::File,
    Class::Desc,
    Class::Memory,
    Class::Process,
    Class::Signal,
    Class::Ipc,
    Class::Clock,
];

const C_LIBRARY: [&str; 5] = [
    "libc.so",
    "libpthread.so",
    "ld-linux",
    "ld-musl",
    "libc.musl",
];

struct RecordedEvent {
    event: TraceEvent,
    stack: Option<u32>,
}

struct RecordedSyscalls {
    events: Vec<RecordedEvent>,
    counted: Folded,
    lost: Losses,
}

pub(super) struct Trace {
    events: Vec<RecordedEvent>,
    counted: Folded,
    lost: Losses,
    calls: u64,
    start: i64,
    end: i64,
    engine: Option<String>,
    processes: ProcessTree,
}

impl Trace {
    pub(super) fn of(capture: &Capture) -> Self {
        let _span = crate::span!("web syscall trace");
        let recorded = extract_syscalls(capture);
        let (start, end) = capture.span();
        let mut processes = ProcessTree::default();
        let mut calls = 0;

        for event in &recorded.events {
            match &event.event {
                TraceEvent::Call(call) => {
                    calls += u64::from(!call.flags.kept());
                    if processes.get(call.process).is_none() {
                        processes.root(call.process, call.time(), None);
                    }
                }
                other => processes.apply(other),
            }
        }

        Self {
            events: recorded.events,
            counted: recorded.counted,
            lost: recorded.lost,
            calls,
            start,
            end,
            engine: capture.setting("syscall engine").map(str::to_owned),
            processes,
        }
    }

    const fn relative(&self, time: u64) -> i64 {
        time.cast_signed().saturating_sub(self.start)
    }

    fn program(&self, pid: Pid) -> Option<String> {
        let (_, image) = self.processes.get(pid)?.images.last()?;
        Some(image.rsplit('/').next().unwrap_or(image).to_owned())
    }
}

pub(super) fn syscalls_lost(capture: &Capture) -> Losses {
    let account = |kind: AccountKind| capture.account(kind as u32);
    Losses {
        dropped: account(AccountKind::SyscallsLost),
        unseen: account(AccountKind::SyscallsUnseen),
        capped: account(AccountKind::SyscallsCapped),
        overflowed: account(AccountKind::SyscallsOverflowed),
        skipped: account(AccountKind::SyscallsSkipped),
    }
}

pub(super) fn syscall_count(capture: &Capture) -> u64 {
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

pub(super) fn syscall_name(capture: &Capture, row: u32) -> String {
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

fn extract_syscalls(capture: &Capture) -> RecordedSyscalls {
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

fn function_name(capture: &Capture, function: u32) -> &str {
    capture
        .functions
        .get(function as usize)
        .map_or("", |row| capture.string(row.name.get()))
}

fn address_name(capture: &Capture, frame: u32) -> String {
    let record = capture.frame(frame);
    let module = record.module.get();
    if module == NONE {
        return format!("{:#x}", record.address.get());
    }

    let path = capture.string(capture.module(module).path.get());
    let name = Path::new(path)
        .file_name()
        .map_or_else(|| path.into(), |name| name.to_string_lossy());
    format!("{name}+{:#x}", record.address.get())
}

fn stack_names(capture: &Capture, stack: u32) -> (Vec<String>, Option<String>) {
    let mut functions = Vec::new();
    let mut truncated = None;
    for frame in capture.stack_frames(stack) {
        let record = capture.frame(frame);
        if record.kind == FrameKind::Truncated as u8 {
            let reason = Truncation::from_number(record.address.get())
                .map_or("unknown", Truncation::describe);
            truncated = Some(reason.to_owned());
            continue;
        }

        let lines = capture.frame_lines(frame);
        if lines.is_empty() {
            functions.push(address_name(capture, frame));
        }
        for line in lines {
            functions.push(function_name(capture, line.function.get()).to_owned());
        }
    }
    (functions, truncated)
}

fn rust_standard_library_function(name: &str) -> bool {
    let standard = |path: &str| {
        let path = path.trim_start_matches('&');
        let path = path.strip_prefix("mut ").unwrap_or(path);
        let path = path.strip_prefix("dyn ").unwrap_or(path);
        ["std::", "core::", "alloc::"]
            .iter()
            .any(|krate| path.starts_with(krate))
    };
    let Some(inner) = name.strip_prefix('<') else {
        return standard(name);
    };

    let mut depth = 0_usize;
    let mut trait_at = None;
    let mut end = inner.len();
    for (at, byte) in inner.bytes().enumerate() {
        match byte {
            b'<' => depth += 1,
            b'>' if depth == 0 => {
                end = at;
                break;
            }
            b'>' => depth -= 1,
            b' ' if depth == 0
                && trait_at.is_none()
                && inner.get(at..).is_some_and(|rest| rest.starts_with(" as ")) =>
            {
                trait_at = Some(at);
            }
            _ => {}
        }
    }

    trait_at.map_or_else(
        || standard(inner.get(..end).unwrap_or(inner)),
        |at| {
            standard(inner.get(..at).unwrap_or_default())
                && standard(inner.get(at + 4..end).unwrap_or_default())
        },
    )
}

fn rust_standard_library(path: &Path) -> bool {
    let parts = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>();
    parts.iter().enumerate().any(|(at, part)| {
        let toolchain = match at.checked_sub(3).and_then(|start| parts.get(start..at)) {
            Some([_, "rustc", commit]) => commit.len() == 40,
            Some(["rustlib", "src", "rust"]) => true,
            _ => at == 2 && matches!(parts.as_slice(), ["rustc", commit, ..] if commit.len() == 40),
        };
        *part == "library"
            && toolchain
            && parts.get(at + 1).is_some_and(|krate| {
                matches!(
                    *krate,
                    "core" | "alloc" | "std" | "panic_unwind" | "panic_abort"
                )
            })
    })
}

struct SyscallPlace<'a> {
    function: &'a str,
    file: &'a str,
    line: u32,
}

fn syscall_place(capture: &Capture, row: u32) -> Option<SyscallPlace<'_>> {
    let interpreted = |frame: &u32| {
        capture
            .frames
            .get(*frame as usize)
            .is_some_and(|frame| frame.kind == FrameKind::Interpreted as u8)
    };
    let place = |frame: u32| {
        let line = capture.frame_lines(frame).iter().find(|line| {
            line.file.get() != NONE
                && !rust_standard_library(Path::new(capture.string(line.file.get())))
        })?;
        Some(SyscallPlace {
            function: function_name(capture, line.function.get()),
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

fn caller_of_c_library(capture: &Capture, row: u32) -> Option<String> {
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
                return vec![address_name(capture, frame)];
            }
            lines
                .iter()
                .map(|line| function_name(capture, line.function.get()).to_owned())
                .collect()
        })
        .collect();

    let own = |name: &&String| !rust_standard_library_function(name);
    names.iter().find(own).or_else(|| names.first()).cloned()
}

fn syscall_site_name(capture: &Capture, row: u32) -> String {
    let site = syscall_place(capture, row)
        .map(|place| {
            let file = place.file.rsplit('/').next().unwrap_or(place.file);
            format!("{} · {file}:{}", place.function, place.line)
        })
        .or_else(|| caller_of_c_library(capture, row));
    site.unwrap_or_else(|| {
        stack_names(capture, row)
            .0
            .into_iter()
            .next()
            .unwrap_or_else(|| "?".to_owned())
    })
}

#[must_use]
pub fn end_text(end: ProcessEnd) -> String {
    match end {
        ProcessEnd::Exited(code) => format!("exited with {code}"),
        ProcessEnd::Killed { signal, core } => format!(
            "killed by {}{}",
            present::signal(signal),
            if core { " (core dumped)" } else { "" }
        ),
    }
}

pub(super) fn in_flight(capture: &Capture, live: bool) -> SyscallInFlight {
    if !live {
        return SyscallInFlight {
            live,
            calls: Vec::new(),
        };
    }

    let at = capture
        .setting(BLOCKED_AT)
        .and_then(|at| at.parse().ok())
        .unwrap_or_else(|| capture.span().1.cast_unsigned());
    let calls = extract_syscalls(capture)
        .events
        .iter()
        .filter_map(|event| match &event.event {
            TraceEvent::Call(call) if call.outcome == Outcome::Unfinished => Some(call),
            _ => None,
        })
        .map(|call| SyscallBlocked {
            tid: call.thread.get(),
            pid: call.process.get(),
            tokens: tokens(present::entry(
                call,
                Context {
                    fds: None,
                    before: true,
                },
            )),
            blocked_ns: call.entered.map_or(0, |entered| at.saturating_sub(entered)),
        })
        .collect();

    SyscallInFlight { live, calls }
}

struct Selection {
    filter: Filter,
    from: i64,
    to: i64,
    kept: Option<Vec<bool>>,
    latency: Option<usize>,
}

impl Selection {
    fn calls(filter: &str, latency: Option<u32>) -> Result<Self, ViewError> {
        let expressions: Vec<String> = filter.split_whitespace().map(str::to_owned).collect();
        let filter = Filter::parse(&expressions, &present::signal_named)
            .map_err(|error| ViewError::Invalid(error.to_string()))?;
        Ok(Self {
            filter,
            from: i64::MIN,
            to: i64::MAX,
            kept: None,
            latency: latency.map(|bucket| usize::try_from(bucket).unwrap_or(usize::MAX)),
        })
    }

    fn new(capture: &Capture, trace: &Trace, query: &SyscallQuery) -> Result<Self, ViewError> {
        let calls = Self::calls(&query.filter, query.latency)?;
        let link = &query.link;
        let narrows = link.threads.is_some()
            || !link.labels.is_empty()
            || link.zones.is_some()
            || !link.transforms.is_empty();
        let kept = if narrows {
            let mut unranged = link.clone();
            unranged.range = None;
            unranged.during = None;
            unranged.syscall_leaves = false;
            let keys = Keys::new(capture, super::profile::granularity(link.granularity));
            let resolved = super::profile::resolve(capture, &keys, &unranged)?;
            Some(keys.syscalls(&resolved))
        } else {
            None
        };

        let (from, to) = link.range.map_or((i64::MIN, i64::MAX), |[from, to]| {
            (
                trace.start.saturating_add(from),
                trace.start.saturating_add(to),
            )
        });

        Ok(Self {
            from,
            to,
            kept,
            ..calls
        })
    }

    fn holds(&self, time: u64) -> bool {
        (self.from..self.to).contains(&time.cast_signed())
    }

    fn shows(&self, row: usize, call: &SyscallEvent, fds: &FdTable) -> bool {
        if !self.holds(call.time())
            || self
                .kept
                .as_ref()
                .is_some_and(|kept| !kept.get(row).copied().unwrap_or(false))
            || self
                .latency
                .is_some_and(|band| call.duration().map(stats::bucket) != Some(band))
        {
            return false;
        }

        let paths = filter::paths(call);
        let paths: Vec<&str> = paths.iter().map(String::as_str).collect();

        self.filter.shows(call, &paths, &|fd| {
            fds.describe(call.process, fd)
                .map(|description| description.text.to_string())
        })
    }

    const fn span(&self, trace: &Trace) -> (i64, i64) {
        if self.from == i64::MIN {
            (trace.start, trace.end.saturating_add(1))
        } else {
            (self.from, self.to)
        }
    }
}

enum Seen<'a> {
    Before(usize, &'a SyscallEvent, &'a FdTable),
    After(usize, &'a SyscallEvent, &'a FdTable),
    Other(usize, &'a TraceEvent),
}

fn walk(trace: &Trace, selection: &Selection, mut visit: impl FnMut(Seen<'_>)) {
    let mut fds = FdTable::default();
    let mut row = 0;
    for (index, recorded) in trace.events.iter().enumerate() {
        match &recorded.event {
            TraceEvent::Call(call) => {
                let shown = selection.shows(row, call, &fds);
                row += 1;
                if shown {
                    visit(Seen::Before(index, call, &fds));
                }

                fds.apply_call(call);
                if shown {
                    visit(Seen::After(index, call, &fds));
                }
            }
            other => {
                fds.apply(other);
                if selection.holds(other.time()) {
                    visit(Seen::Other(index, other));
                }
            }
        }
    }
}

pub(super) fn rows(
    capture: &Capture,
    trace: &Trace,
    during: &ProfileDuring,
) -> Result<Vec<u32>, ViewError> {
    let selection = Selection::calls(&during.filter, during.latency)?;
    let mut row = during
        .row
        .as_ref()
        .map(|row| (Grouping::new(capture, trace, row.group), row.key.as_str()));
    let mut matched = Vec::new();
    known(trace, &selection, |index, call, fds| {
        if row
            .as_mut()
            .is_none_or(|(grouping, key)| grouping.row(index, call, fds).0 == *key)
        {
            matched.push(index);
        }
    });

    let mut matched = matched.into_iter().peekable();
    let mut rows = Vec::new();
    let mut calls = 0;
    for (index, recorded) in trace.events.iter().enumerate() {
        if matches!(recorded.event, TraceEvent::Call(_)) {
            if matched.next_if_eq(&index).is_some() {
                rows.push(calls);
            }
            calls += 1;
        }
    }

    Ok(rows)
}

fn calls(
    trace: &Trace,
    selection: &Selection,
    mut visit: impl FnMut(usize, &SyscallEvent, &FdTable),
) {
    walk(trace, selection, |seen| {
        if let Seen::Before(index, call, fds) = seen {
            visit(index, call, fds);
        }
    });
}

fn known(
    trace: &Trace,
    selection: &Selection,
    mut visit: impl FnMut(usize, &SyscallEvent, &FdTable),
) {
    walk(trace, selection, |seen| match seen {
        Seen::Before(index, call, fds) if !is_connect(call) => visit(index, call, fds),
        Seen::After(index, call, fds) if is_connect(call) => visit(index, call, fds),
        _ => {}
    });
}

fn site<'a>(capture: &Capture, sites: &'a mut HashMap<u32, String>, row: u32) -> &'a String {
    sites
        .entry(row)
        .or_insert_with(|| syscall_site_name(capture, row))
}

fn named_path(call: &SyscallEvent, fds: &FdTable) -> Option<String> {
    let path = filter::paths(call).into_iter().next()?;
    let dirfd = match specs::spec(call.call).and_then(|spec| {
        spec.args
            .iter()
            .position(|arg| matches!(arg, specs::Arg::DirFd))
    }) {
        Some(index) => filter::fd_value(*call.args.get(index)?),
        None => AT_FDCWD,
    };

    Some(fds.resolve(call.process, dirfd, &path))
}

fn target(call: &SyscallEvent, fds: &FdTable) -> Option<String> {
    named_path(call, fds).or_else(|| {
        let spec = specs::spec(call.call)?;
        let index = spec
            .args
            .iter()
            .position(|arg| matches!(arg, specs::Arg::Fd))?;
        let fd = filter::fd_value(*call.args.get(index)?);
        Some(fds.describe(call.process, fd).map_or_else(
            || fd.to_string(),
            |description| description.text.to_string(),
        ))
    })
}

fn class(call: &SyscallEvent) -> &'static str {
    specs::spec(call.call)
        .and_then(|spec| {
            CLASSES
                .into_iter()
                .find(|&class| spec.classes.contains(class))
        })
        .map_or("other", Class::name)
}

fn errno_name(errno: u16) -> String {
    present::errno(Errno(errno)).0
}

fn counts(errnos: &BTreeMap<u16, u64>) -> Vec<SyscallCount> {
    let mut counts: Vec<SyscallCount> = errnos
        .iter()
        .map(|(&errno, &count)| SyscallCount {
            name: errno_name(errno),
            count,
        })
        .collect();
    counts.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    counts
}

fn row(key: String, name: String, stats: &SyscallStats) -> SyscallRow {
    SyscallRow {
        key,
        name,
        calls: stats.calls,
        errors: stats.errors,
        timed: stats.timed,
        total_ns: stats.total_ns,
        p50_ns: stats.quantile_ns(0.5),
        p90_ns: stats.quantile_ns(0.9),
        p99_ns: stats.quantile_ns(0.99),
        max_ns: stats.max_ns,
        bytes: stats.bytes,
        errnos: counts(&stats.errors_by_errno),
    }
}

struct Grouping<'a> {
    capture: &'a Capture,
    trace: &'a Trace,
    group: SyscallGroup,
    sites: HashMap<u32, String>,
}

impl<'a> Grouping<'a> {
    fn new(capture: &'a Capture, trace: &'a Trace, group: SyscallGroup) -> Self {
        Self {
            capture,
            trace,
            group,
            sites: HashMap::new(),
        }
    }

    fn row(&mut self, index: usize, call: &SyscallEvent, fds: &FdTable) -> (String, String) {
        match self.group {
            SyscallGroup::Path => {
                let path = target(call, fds).unwrap_or_else(|| "none".to_owned());
                (path.clone(), path)
            }
            SyscallGroup::Site => {
                let site = self
                    .trace
                    .events
                    .get(index)
                    .and_then(|event| event.stack)
                    .map_or_else(
                        || "no stack".to_owned(),
                        |stack| site(self.capture, &mut self.sites, stack).clone(),
                    );
                (site.clone(), site)
            }
            SyscallGroup::Syscall => {
                let name = call.call.display_name().into_owned();
                (name.clone(), name)
            }
            SyscallGroup::Class => (format!("%{}", class(call)), class(call).to_owned()),
            SyscallGroup::Process => (
                call.process.to_string(),
                self.trace.program(call.process).map_or_else(
                    || call.process.to_string(),
                    |program| format!("{} {program}", call.process),
                ),
            ),
            SyscallGroup::Thread => (call.thread.to_string(), call.thread.to_string()),
        }
    }
}

pub(super) fn summary(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallSummaryQuery,
) -> Result<SyscallSummary, ViewError> {
    let selection = Selection::new(capture, trace, &query.query)?;
    let mut grouping = Grouping::new(capture, trace, query.group);
    let mut groups: BTreeMap<String, (String, SyscallStats)> = BTreeMap::new();
    let mut processes = BTreeSet::new();
    let mut add = |key: String, name: String, stats: &SyscallStats| {
        groups
            .entry(key)
            .or_insert_with(|| (name, SyscallStats::default()))
            .1
            .merge(stats);
    };

    known(trace, &selection, |index, call, fds| {
        if call.flags.kept() {
            return;
        }

        processes.insert(call.process);
        let (key, name) = grouping.row(index, call, fds);
        let mut one = SyscallStats::default();
        one.add(call.outcome, call.duration(), bytes_moved(call));
        add(key, name, &one);
    });

    let counted = !trace.counted.by_call.is_empty();
    for (&(pid, tid, id), stats) in &trace.counted.by_call {
        if !selection.filter.calls.contains(id) {
            continue;
        }

        processes.insert(pid);
        let name = id.display_name().into_owned();
        let (key, name) = match query.group {
            SyscallGroup::Thread if tid.get() != 0 => (tid.to_string(), tid.to_string()),
            SyscallGroup::Syscall
            | SyscallGroup::Thread
            | SyscallGroup::Path
            | SyscallGroup::Site => (name.clone(), name),
            SyscallGroup::Class => {
                let class = specs::spec(id)
                    .and_then(|spec| {
                        CLASSES
                            .into_iter()
                            .find(|&class| spec.classes.contains(class))
                    })
                    .map_or("other", Class::name);
                (format!("%{class}"), class.to_owned())
            }
            SyscallGroup::Process => (pid.to_string(), pid.to_string()),
        };

        add(key, name, stats);
    }

    let mut rows: Vec<SyscallRow> = groups
        .into_iter()
        .map(|(key, (name, stats))| row(key, name, &stats))
        .collect();
    rows.sort_by(|a, b| {
        (b.total_ns, b.calls)
            .cmp(&(a.total_ns, a.calls))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(SyscallSummary {
        total: trace.calls
            + trace
                .counted
                .by_call
                .values()
                .map(|stats| stats.calls)
                .sum::<u64>(),
        calls: rows.iter().map(|row| row.calls).sum(),
        errors: rows.iter().map(|row| row.errors).sum(),
        lost: trace.lost.into(),
        counted,
        engine: trace.engine.clone(),
        processes: u32::try_from(processes.len()).unwrap_or(u32::MAX),
        rows,
    })
}

struct Slices {
    start: i64,
    width: i64,
    count: usize,
}

impl Slices {
    fn new(span: (i64, i64), buckets: u32) -> Self {
        let count = buckets.clamp(1, MOST_BUCKETS);
        let length = span.1.saturating_sub(span.0).max(1);

        Self {
            start: span.0,
            width: (length + i64::from(count) - 1) / i64::from(count),
            count: count as usize,
        }
    }

    fn of(&self, time: u64) -> Option<usize> {
        let offset = time.cast_signed().checked_sub(self.start)?;
        usize::try_from(offset / self.width)
            .ok()
            .filter(|&slice| offset >= 0 && slice < self.count)
    }
}

pub(super) fn heatmap(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallHeatmapQuery,
) -> Result<SyscallHeatmap, ViewError> {
    let selection = Selection::new(capture, trace, &query.query)?;
    let slices = Slices::new(selection.span(trace), query.buckets);
    let rows = stats::BUCKETS;
    let mut counts = vec![0_u32; slices.count * rows];
    let mut failed = vec![0_u32; slices.count * rows];
    calls(trace, &selection, |_, call, _| {
        if let (Some(duration), Some(slice)) = (call.duration(), slices.of(call.time())) {
            let cell = slice * rows + stats::bucket(duration);
            if let Some(slot) = counts.get_mut(cell) {
                *slot += 1;
            }
            if let Some(slot) = failed.get_mut(cell) {
                *slot += u32::from(matches!(call.outcome, Outcome::Failed(_)));
            }
        }
    });

    Ok(SyscallHeatmap {
        start: slices.start - trace.start,
        width: slices.width,
        rows: u32::try_from(rows).unwrap_or(u32::MAX),
        most: counts.iter().copied().max().unwrap_or(0),
        counts,
        failed,
    })
}

pub(super) fn detail(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallDetailQuery,
) -> Result<SyscallDetail, ViewError> {
    let named = |call: &SyscallEvent| call.call.display_name() == query.call;
    let mut whole = vec![0; stats::BUCKETS];
    for event in &trace.events {
        if let TraceEvent::Call(call) = &event.event
            && named(call)
            && let Some(duration) = call.duration()
            && let Some(slot) = whole.get_mut(stats::bucket(duration))
        {
            *slot += 1;
        }
    }

    let selection = Selection::new(capture, trace, &query.query)?;
    let mut selected = SyscallStats::default();
    let mut chosen = Vec::new();
    calls(trace, &selection, |index, call, _| {
        if named(call) {
            selected.add(call.outcome, call.duration(), bytes_moved(call));
            chosen.push(index);
        }
    });

    let slow_ns = selected.quantile_ns(0.9);
    let mut sites: HashMap<u32, String> = HashMap::new();
    let mut callers: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut unplaced = 0;

    for index in chosen {
        let Some(event) = trace.events.get(index) else {
            continue;
        };
        let TraceEvent::Call(call) = &event.event else {
            continue;
        };

        let slow = slow_ns.is_some_and(|floor| call.duration().is_some_and(|took| took >= floor));
        let failed = matches!(call.outcome, Outcome::Failed(_));
        if !slow && !failed {
            continue;
        }

        let Some(stack) = event.stack else {
            unplaced += 1;
            continue;
        };

        let site = site(capture, &mut sites, stack);
        let counts = callers.entry(site.clone()).or_default();
        counts.0 += u64::from(slow);
        counts.1 += u64::from(failed);
    }

    let mut callers: Vec<SyscallCaller> = callers
        .into_iter()
        .map(|(site, (slow, failed))| SyscallCaller { site, slow, failed })
        .collect();

    callers.sort_by(|a, b| {
        (b.slow + b.failed)
            .cmp(&(a.slow + a.failed))
            .then_with(|| a.site.cmp(&b.site))
    });

    let mut histogram = selected.log2_ns.clone();
    histogram.resize(stats::BUCKETS, 0);

    Ok(SyscallDetail {
        whole,
        selected: histogram,
        errnos: counts(&selected.errors_by_errno),
        slow_ns,
        callers,
        unplaced,
    })
}

fn tokens(tokens: Tokens) -> Vec<SyscallToken> {
    tokens
        .0
        .into_iter()
        .map(|token| SyscallToken {
            kind: token.kind.name().to_owned(),
            text: token.text,
        })
        .collect()
}

pub(super) fn log(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallLogQuery,
) -> Result<SyscallLog, ViewError> {
    let selection = Selection::new(capture, trace, &query.query)?;
    let first = query.offset;
    let last = first.saturating_add(u64::from(query.limit.min(MOST_LINES)));
    let mut total = 0_u64;
    let mut lines = Vec::new();
    let mut entry = Tokens::default();
    walk(trace, &selection, |seen| match seen {
        Seen::Before(_, call, fds) => {
            if (first..last).contains(&total) {
                entry = present::entry(
                    call,
                    Context {
                        fds: Some(fds),
                        before: true,
                    },
                );
            }
        }
        Seen::After(index, call, fds) => {
            if (first..last).contains(&total) {
                let mut line = std::mem::take(&mut entry);
                line.0.extend(
                    present::returned(
                        call,
                        Context {
                            fds: Some(fds),
                            before: false,
                        },
                    )
                    .0,
                );
                lines.push(SyscallLine {
                    event: index as u64,
                    time: trace.relative(call.time()),
                    tid: call.thread.get(),
                    pid: call.process.get(),
                    tokens: tokens(line),
                    duration_ns: call.duration(),
                    failed: matches!(call.outcome, Outcome::Failed(_)),
                    call: true,
                });
            }

            total += 1;
        }
        Seen::Other(index, event) => {
            let Some(line) = present::other(event) else {
                return;
            };

            if (first..last).contains(&total) {
                let (tid, pid) = match *event {
                    TraceEvent::Signal {
                        thread, process, ..
                    }
                    | TraceEvent::ThreadExit {
                        thread, process, ..
                    } => (thread.get(), process.get()),
                    TraceEvent::Exit { process, .. } => (process.leader().get(), process.get()),
                    _ => (0, 0),
                };

                lines.push(SyscallLine {
                    event: index as u64,
                    time: trace.relative(event.time()),
                    tid,
                    pid,
                    tokens: tokens(line),
                    duration_ns: None,
                    failed: false,
                    call: false,
                });
            }

            total += 1;
        }
    });

    Ok(SyscallLog { total, lines })
}

pub(super) fn call(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallCallQuery,
    source_lines: Option<SourceLines<'_>>,
) -> Result<SyscallCall, ViewError> {
    let index = usize::try_from(query.event).unwrap_or(usize::MAX);
    let missing = || ViewError::Invalid(format!("no call {}", query.event));
    let event = trace.events.get(index).ok_or_else(missing)?;
    let TraceEvent::Call(target) = &event.event else {
        return Err(missing());
    };

    let mut fds = FdTable::default();
    for event in trace.events.iter().take(index) {
        match &event.event {
            TraceEvent::Call(call) => fds.apply_call(call),
            other => fds.apply(other),
        }
    }

    let before = Context {
        fds: Some(&fds),
        before: true,
    };

    let arguments = present::argument_values(target, before)
        .into_iter()
        .map(|(name, value)| SyscallArgument {
            name,
            tokens: tokens(value),
        })
        .collect();

    let mut after = fds.clone();
    after.apply_call(target);
    let result = present::returned(
        target,
        Context {
            fds: Some(&after),
            before: false,
        },
    );
    let (stack, stack_ends) = event
        .stack
        .map_or((Vec::new(), None), |row| stack_names(capture, row));

    Ok(SyscallCall {
        arguments,
        dumps: present::dumps::dump(target),
        stack,
        stack_ends,
        entered: target.entered.map(|time| trace.relative(time)),
        exited: target.exited.map(|time| trace.relative(time)),
        result: tokens(result),
        site: event
            .stack
            .and_then(|row| syscall_place(capture, row))
            .map(|place| {
                let line = u64::from(place.line);
                let first = line.saturating_sub(SITE_MARGIN).max(1);
                let (text, unread) = source_lines.map_or_else(
                    || {
                        (
                            None,
                            Some("source files are not available on this server".to_owned()),
                        )
                    },
                    |reader| match reader(place.file, first, line + SITE_MARGIN) {
                        Ok(text) => (Some(text), None),
                        Err(why) => (None, Some(why)),
                    },
                );

                SyscallSite {
                    function: place.function.to_owned(),
                    path: place.file.to_owned(),
                    line: place.line,
                    first: u32::try_from(first).unwrap_or(u32::MAX),
                    text,
                    unread,
                }
            }),
    })
}

fn opens(call: &SyscallEvent) -> bool {
    call.call
        .name()
        .is_some_and(|name| matches!(name, "open" | "openat" | "openat2" | "creat"))
}

pub(super) fn files(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallQuery,
) -> Result<SyscallFiles, ViewError> {
    let selection = Selection::new(capture, trace, query)?;
    let mut files: BTreeMap<String, SyscallFile> = BTreeMap::new();
    calls(trace, &selection, |_, call, fds| {
        if opens(call)
            && let Some(path) = named_path(call, fds)
        {
            let entry = files.entry(path.clone()).or_insert_with(|| SyscallFile {
                path,
                ..SyscallFile::default()
            });
            entry.opens += 1;
            entry.failed += u64::from(matches!(call.outcome, Outcome::Failed(_)));
            entry.total_ns += call.duration().unwrap_or(0);
            return;
        }

        let Some(fd) = filter::descriptors(call).next() else {
            return;
        };

        let Some(description) = fds.describe(call.process, fd) else {
            return;
        };

        if !matches!(description.kind, FdKind::File) {
            return;
        }

        let entry = files
            .entry(description.text.to_string())
            .or_insert_with(|| SyscallFile {
                path: description.text.to_string(),
                ..SyscallFile::default()
            });
        let took = call.duration().unwrap_or(0);
        entry.total_ns += took;
        if call
            .call
            .name()
            .is_some_and(|name| matches!(name, "fsync" | "fdatasync"))
        {
            entry.sync_ns += took;
        }

        match present::dumps::moves(call) {
            Some((_, present::dumps::Direction::Read)) => entry.read += bytes_moved(call),
            Some((_, present::dumps::Direction::Write)) => entry.written += bytes_moved(call),
            None => {}
        }
    });

    let mut files: Vec<SyscallFile> = files.into_values().collect();
    files.sort_by(|a, b| {
        b.total_ns
            .cmp(&a.total_ns)
            .then_with(|| a.path.cmp(&b.path))
    });

    Ok(SyscallFiles { files })
}

pub(super) fn network(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallQuery,
) -> Result<SyscallNetwork, ViewError> {
    let selection = Selection::new(capture, trace, query)?;
    let mut endpoints: BTreeMap<String, SyscallEndpoint> = BTreeMap::new();
    known(trace, &selection, |_, call, fds| {
        let connect = is_connect(call);

        let Some(fd) = filter::descriptors(call).next() else {
            return;
        };

        let Some(description) = fds.describe(call.process, fd) else {
            return;
        };

        if !matches!(description.kind, FdKind::Socket(_)) {
            return;
        }

        let endpoint = endpoints
            .entry(description.text.to_string())
            .or_insert_with(|| SyscallEndpoint {
                endpoint: description.text.to_string(),
                ..SyscallEndpoint::default()
            });
        if connect {
            endpoint.connects += 1;
            if matches!(call.outcome, Outcome::Failed(errno) if present::errno(errno).0 != "EINPROGRESS")
            {
                endpoint.failed += 1;
            }
            endpoint.connect_ns += call.duration().unwrap_or(0);
        }

        match present::dumps::moves(call) {
            Some((_, present::dumps::Direction::Read)) => endpoint.received += bytes_moved(call),
            Some((_, present::dumps::Direction::Write)) => endpoint.sent += bytes_moved(call),
            None => {}
        }
    });

    let mut endpoints: Vec<SyscallEndpoint> = endpoints.into_values().collect();
    endpoints.sort_by(|a, b| {
        (b.sent + b.received)
            .cmp(&(a.sent + a.received))
            .then_with(|| a.endpoint.cmp(&b.endpoint))
    });

    Ok(SyscallNetwork { endpoints })
}

fn is_connect(call: &SyscallEvent) -> bool {
    call.call.name() == Some("connect")
}

pub(super) fn processes(
    capture: &Capture,
    trace: &Trace,
    query: &SyscallQuery,
) -> Result<SyscallProcesses, ViewError> {
    let selection = Selection::new(capture, trace, query)?;
    let mut counts: BTreeMap<Pid, (u64, u64)> = BTreeMap::new();

    calls(trace, &selection, |_, call, _| {
        let entry = counts.entry(call.process).or_default();
        entry.0 += 1;
        entry.1 += u64::from(matches!(call.outcome, Outcome::Failed(_)));
    });

    let mut ordered: Vec<SyscallProcess> = Vec::new();
    let mut pending: Vec<Pid> = trace
        .processes
        .iter()
        .filter(|process| {
            process
                .parent
                .is_none_or(|parent| trace.processes.get(parent).is_none())
        })
        .map(|process| process.pid)
        .collect();
    pending.reverse();

    while let Some(pid) = pending.pop() {
        let Some(process) = trace.processes.get(pid) else {
            continue;
        };

        let (calls, errors) = counts.get(&pid).copied().unwrap_or_default();
        ordered.push(SyscallProcess {
            pid: pid.get(),
            parent: process.parent.map(Pid::get),
            images: process
                .images
                .iter()
                .map(|(_, image)| image.to_string())
                .collect(),
            threads: u32::try_from(process.threads.len()).unwrap_or(u32::MAX),
            calls,
            errors,
            started: trace.relative(process.started),
            ended: process.ended.map(|(time, _)| trace.relative(time)),
            end: process.ended.map(|(_, end)| end_text(end)),
        });

        let mut children: Vec<Pid> = trace
            .processes
            .iter()
            .filter(|child| child.parent == Some(pid))
            .map(|child| child.pid)
            .collect();
        children.reverse();
        pending.extend(children);
    }

    Ok(SyscallProcesses { processes: ordered })
}
