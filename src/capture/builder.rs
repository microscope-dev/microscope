//! Interns what a recording sees into a [`Capture`]'s tables.

use std::collections::BTreeMap;

use foldhash::HashMap;

use super::Capture;
use super::records::{
    Account, Frame, FrameKind, Function, Gap, GapReason, Line, Mapping, Marker, MarkerKind, Module,
    NONE, OffCpu, Plot, Process, Range, Sample, Setting, Site, Stack, Syscall, SyscallErrno,
    SyscallNote, SyscallStat, Task, TaskWait, Thread, ThreadRange, Truncation, UNSEEN,
    ZONE_BEGAN_UNSEEN, ZONE_ENDED_UNSEEN, Zone, ZoneRange,
};

use crate::image::{StrId, StringsBuilder};
use crate::model::{Pid, Tid};

/// One source line of a frame, as a symbolizer reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineInput<'a> {
    pub function: &'a str,
    pub file: Option<&'a str>,
    pub line: u32,
    pub column: u32,

    /// Where the function is declared.
    pub declared: Option<(&'a str, u32)>,

    /// The function's role, by [`role_number`](super::records::role_number).
    pub role: u8,
}

/// Where an interpreted frame is: its function, and the line it runs of
/// the file its code is in, which its function's code begins at line
/// `declared` of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterpretedPlace<'a> {
    pub function: &'a str,
    pub file: Option<&'a str>,
    pub line: u32,
    pub declared: Option<u32>,
}

/// Builds a capture as a recording proceeds.
#[derive(Debug, Default, Clone)]
pub struct CaptureBuilder {
    strings: StringsBuilder,
    settings: BTreeMap<String, String>,
    modules: Vec<Module>,
    module_index: HashMap<(u32, u32, [u8; 16]), u32>,
    processes: Vec<Process>,
    live_processes: HashMap<Pid, u32>,

    /// Each pid's and tid's ended rows, which an event from before its end
    /// still belongs to: an id names one process or thread at a time.
    ended_processes: HashMap<Pid, Vec<u32>>,
    threads: Vec<Thread>,
    live_threads: BTreeMap<Tid, u32>,
    ended_threads: HashMap<Tid, Vec<u32>>,
    mappings: Vec<Mapping>,
    frames: Vec<Frame>,
    frame_index: HashMap<(u64, u32, u8), u32>,
    stacks: Vec<Stack>,
    stack_index: HashMap<(u32, u32), u32>,
    samples: Vec<Vec<Sample>>,
    off_cpu: Vec<Vec<OffCpu>>,
    accounts: BTreeMap<u32, u64>,
    symbols: HashMap<u32, Range>,
    lines: Vec<Line>,
    functions: Vec<Function>,
    function_index: HashMap<(u32, u32, u32, u32), u32>,
    sites: Vec<Site>,

    /// Each thread's zones, whose depths their nesting gives.
    zones: BTreeMap<u32, Vec<Zone>>,
    plots: Vec<Plot>,
    markers: Vec<Marker>,
    tasks: Vec<Task>,
    task_index: HashMap<(u32, u64), u32>,
    task_waits: Vec<TaskWait>,
    gaps: Vec<Gap>,
    syscalls: Vec<Syscall>,
    syscall_blobs: Vec<u8>,
    syscall_notes: Vec<SyscallNote>,
    syscall_stats: Vec<SyscallStat>,
    syscall_errnos: Vec<SyscallErrno>,
}

/// Rows kept per thread, as one table in time order per thread and each
/// thread's range of it.
fn grouped<T: Timed>(by_thread: Vec<Vec<T>>) -> (Vec<T>, Vec<ThreadRange>) {
    let mut rows = Vec::with_capacity(by_thread.iter().map(Vec::len).sum());
    let mut ranges = Vec::new();
    for (thread, mut samples) in by_thread.into_iter().enumerate() {
        if samples.is_empty() {
            continue;
        }

        samples.sort_by_key(T::time);
        ranges.push(ThreadRange {
            thread: index(thread).into(),
            start: index(rows.len()).into(),
            count: index(samples.len()).into(),
        });
        rows.extend(samples);
    }

    (rows, ranges)
}

/// Whether a process's span holds `time`.
const fn covers(process: &Process, time: i64) -> bool {
    process.start.get() <= time && time <= process.end.get()
}

/// Whether two spans share a moment.
const fn overlap(a: (i64, i64), b: (i64, i64)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

/// When a call happened, for cutting a window: its end, or its entry.
const fn call_time(call: &Syscall) -> i64 {
    if call.exited.get() == UNSEEN {
        call.entered.get()
    } else {
        call.exited.get()
    }
}

impl CaptureBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The pool's name for `text`; a string with a NUL is cut at it.
    pub fn string(&mut self, text: &str) -> u32 {
        let text = text.split('\0').next().unwrap_or_default();
        self.strings.push(text).map_or(NONE, |StrId(id)| id)
    }

    fn optional(&mut self, text: Option<&str>) -> u32 {
        text.map_or(NONE, |text| self.string(text))
    }

    /// Records what produced the capture.
    pub fn setting(&mut self, key: &str, value: &str) {
        self.settings.insert(key.to_owned(), value.to_owned());
    }

    /// The module of a file, by its path, build id, and contents.
    pub fn module(
        &mut self,
        path: &str,
        build_id: Option<&str>,
        digest: [u8; 16],
        size: u64,
    ) -> u32 {
        let path = self.string(path);
        let build_id = self.optional(build_id);
        if let Some(&module) = self.module_index.get(&(path, build_id, digest)) {
            return module;
        }

        let module = index(self.modules.len());
        self.modules.push(Module {
            path: path.into(),
            build_id: build_id.into(),
            digest,
            size: size.into(),
        });
        self.module_index.insert((path, build_id, digest), module);
        module
    }

    /// The live process `pid`, which begins at `time` if not yet seen.
    pub fn process(&mut self, pid: Pid, time: i64) -> u32 {
        if let Some(&process) = self.live_processes.get(&pid) {
            return process;
        }

        let ended = self.ended_processes.get(&pid).into_iter().flatten();
        if let Some(&process) = ended
            .rev()
            .find(|&&row| covers(&self.processes[row as usize], time))
        {
            return process;
        }

        let process = index(self.processes.len());
        self.processes.push(Process {
            pid: pid.get().into(),
            name: NONE.into(),
            start: time.into(),
            end: time.into(),
        });
        self.live_processes.insert(pid, process);
        process
    }

    /// Names a process, as when it execs.
    pub fn process_name(&mut self, process: u32, name: &str) {
        let name = self.string(name);
        self.processes[process as usize].name = name.into();
    }

    /// Ends process `pid` and its threads at `time`.
    pub fn process_exit(&mut self, pid: Pid, time: i64) {
        if let Some(process) = self.live_processes.remove(&pid) {
            self.processes[process as usize].end = time.into();
            self.ended_processes.entry(pid).or_default().push(process);
            let ended: Vec<Tid> = self
                .live_threads
                .iter()
                .filter(|(_, thread)| self.threads[**thread as usize].process.get() == process)
                .map(|(tid, _)| *tid)
                .collect();

            for tid in ended {
                self.thread_exit(tid, time);
            }
        }
    }

    /// The live thread `tid` of `pid`, which begins at `time` if not yet
    /// seen.
    pub fn thread(&mut self, pid: Pid, tid: Tid, time: i64) -> u32 {
        if let Some(&thread) = self.live_threads.get(&tid) {
            let thread_record = &mut self.threads[thread as usize];
            if self.processes[thread_record.process.get() as usize]
                .pid
                .get()
                == pid.get()
            {
                if thread_record.end.get() < time {
                    thread_record.end = time.into();
                }

                return thread;
            }

            self.live_threads.remove(&tid);
        }

        let ended = self.ended_threads.get(&tid).into_iter().flatten();
        if let Some(&thread) = ended.rev().find(|&&row| {
            let thread = &self.threads[row as usize];
            thread.start.get() <= time
                && time <= thread.end.get()
                && self.processes[thread.process.get() as usize].pid.get() == pid.get()
        }) {
            return thread;
        }

        let process = self.process(pid, time);
        if self.processes[process as usize].end.get() < time {
            self.processes[process as usize].end = time.into();
        }

        let thread = index(self.threads.len());
        self.threads.push(Thread {
            process: process.into(),
            tid: tid.get().into(),
            name: NONE.into(),
            reserved: 0.into(),
            start: time.into(),
            end: time.into(),
        });
        self.samples.push(Vec::new());
        self.off_cpu.push(Vec::new());
        self.live_threads.insert(tid, thread);
        thread
    }

    /// Names a thread.
    pub fn thread_name(&mut self, thread: u32, name: &str) {
        let name = self.string(name);
        self.threads[thread as usize].name = name.into();
    }

    /// Ends thread `tid` at `time`.
    pub fn thread_exit(&mut self, tid: Tid, time: i64) {
        if let Some(thread) = self.live_threads.remove(&tid) {
            self.ended_threads.entry(tid).or_default().push(thread);
            let thread = &mut self.threads[thread as usize];
            if thread.end.get() < time {
                thread.end = time.into();
            }
        }
    }

    /// Records a mapping of a module into a process.
    pub fn mapping(&mut self, mapping: Mapping) {
        self.mappings.push(mapping);
    }

    /// The frame of a kind, module, and address.
    pub fn frame(&mut self, kind: FrameKind, module: u32, address: u64, role: u8) -> u32 {
        let key = (address, module, kind as u8);
        if let Some(&frame) = self.frame_index.get(&key) {
            return frame;
        }

        let frame = index(self.frames.len());
        self.frames.push(Frame {
            address: address.into(),
            module: module.into(),
            kind: kind as u8,
            role,
            reserved: 0.into(),
        });
        self.frame_index.insert(key, frame);
        frame
    }

    /// Every frame so far, by index.
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// The frame that says why a stack ends early.
    pub fn truncation(&mut self, reason: Truncation, module: u32) -> u32 {
        self.frame(FrameKind::Truncated, module, reason as u64, 0)
    }

    /// The frame of a future a suspended task awaits, named by what it
    /// waits for.
    pub fn awaited(&mut self, text: &str) -> u32 {
        let name = self.string(text);
        let frame = self.frame(FrameKind::Awaited, NONE, u64::from(name), 0);
        if !self.symbolized(frame) {
            let line = LineInput {
                function: text,
                file: None,
                line: 0,
                column: 0,
                declared: None,
                role: 0,
            };

            self.symbolize(frame, NONE, &[line]);
        }

        frame
    }

    /// The frame of an interpreted function at a line of its source, whose
    /// code is in no module. Its function is declared at the file and line
    /// its code begins at, so that functions of one name in two files, or
    /// two of one file, stay apart.
    pub fn interpreted(&mut self, place: &InterpretedPlace<'_>) -> u32 {
        let file = place.file.unwrap_or_default();

        // The file's length makes the key unambiguous whatever the names
        // hold.
        let key = format!(
            "{}\t{}\t{}\t{}{file}{}",
            place.line,
            place.declared.map_or(-1, i64::from),
            file.len(),
            u8::from(place.file.is_some()),
            place.function
        );
        let name = self.string(&key);
        let frame = self.frame(FrameKind::Interpreted, NONE, u64::from(name), 0);
        if !self.symbolized(frame) {
            let line = LineInput {
                function: place.function,
                file: place.file,
                line: place.line,
                column: 0,
                declared: place.file.zip(place.declared),
                role: 0,
            };

            self.symbolize(frame, NONE, &[line]);
        }

        frame
    }

    /// The stack node of `frame` called from `parent`.
    pub fn child(&mut self, parent: u32, frame: u32) -> u32 {
        debug_assert!(
            parent == NONE || (parent as usize) < self.stacks.len(),
            "stack parent {parent} is not among the {} stacks",
            self.stacks.len()
        );
        debug_assert!(
            (frame as usize) < self.frames.len(),
            "stack frame {frame} is not among the {} frames",
            self.frames.len()
        );
        if let Some(&stack) = self.stack_index.get(&(parent, frame)) {
            return stack;
        }

        let stack = index(self.stacks.len());
        self.stacks.push(Stack {
            parent: parent.into(),
            frame: frame.into(),
        });
        self.stack_index.insert((parent, frame), stack);
        stack
    }

    /// The stack of `frames`, given innermost first.
    pub fn stack(&mut self, frames: &[u32]) -> u32 {
        frames
            .iter()
            .rev()
            .fold(NONE, |parent, frame| self.child(parent, *frame))
    }

    /// Records an on-CPU sample of a thread.
    pub fn sample(&mut self, thread: u32, time: i64, weight: u64, stack: u32, task: u32) {
        debug_assert!(
            stack == NONE || (stack as usize) < self.stacks.len(),
            "a sample of thread {thread} at {time} has stack {stack}, not among the {} stacks",
            self.stacks.len()
        );
        debug_assert!(
            task == NONE || (task as usize) < self.tasks.len(),
            "a sample of thread {thread} at {time} has task {task}, not among the {} tasks",
            self.tasks.len()
        );
        self.samples[thread as usize].push(Sample {
            time: time.into(),
            weight: weight.into(),
            stack: stack.into(),
            task: task.into(),
        });
    }

    /// Gives a thread's samples at `time` the task they ran, and says how
    /// many it gave. Samples come in time order, so the search runs back
    /// from the latest.
    pub fn claim(&mut self, thread: u32, time: i64, task: u32) -> u64 {
        let mut claimed = 0;

        for sample in self.samples[thread as usize].iter_mut().rev() {
            let at = sample.time.get();
            if at < time {
                break;
            }

            if at == time && sample.task.get() == NONE {
                sample.task = task.into();
                claimed += 1;
            }
        }

        claimed
    }

    /// Records that a thread spent `duration` nanoseconds off its CPU from
    /// `time`, preempted or blocked.
    pub fn off_cpu(&mut self, thread: u32, time: i64, duration: u64, preempted: bool) {
        self.off_cpu[thread as usize].push(OffCpu {
            time: time.into(),
            duration: duration.into(),
            preempted: preempted.into(),
        });
    }

    /// Adds to a count.
    pub fn count(&mut self, kind: u32, value: u64) {
        if value > 0 {
            *self.accounts.entry(kind).or_default() += value;
        }
    }

    /// A count so far.
    pub fn account(&self, kind: u32) -> u64 {
        self.accounts.get(&kind).copied().unwrap_or(0)
    }

    /// Whether `frame` has its lines.
    pub fn symbolized(&self, frame: u32) -> bool {
        self.symbols.contains_key(&frame)
    }

    /// Gives a frame its source lines, innermost inline first.
    pub fn symbolize(&mut self, frame: u32, module: u32, lines: &[LineInput<'_>]) {
        debug_assert!(
            (frame as usize) < self.frames.len(),
            "symbolizing frame {frame}, not among the {} frames",
            self.frames.len()
        );
        let start = index(self.lines.len());

        for line in lines {
            let name = self.string(line.function);
            let (declared, declared_line) = line
                .declared
                .map_or((NONE, 0), |(file, line)| (self.string(file), line));
            let function = self.function(name, module, (declared, declared_line), line.role);
            let file = self.optional(line.file);
            self.lines.push(Line {
                function: function.into(),
                file: file.into(),
                number: line.line.into(),
                column: line.column.into(),
            });
        }

        self.symbols.insert(frame, range(start, lines.len()));
    }

    /// The function named `name` of `module`, declared at `file`:`line`.
    fn function(&mut self, name: u32, module: u32, (file, line): (u32, u32), role: u8) -> u32 {
        let key = (name, module, file, line);
        if let Some(&function) = self.function_index.get(&key) {
            return function;
        }

        let function = index(self.functions.len());
        self.functions.push(Function {
            name: name.into(),
            module: module.into(),
            file: file.into(),
            line: line.into(),
            role,
            reserved: [0; 3],
        });
        self.function_index.insert(key, function);
        function
    }

    /// The site of an instrumentation record.
    pub fn site(&mut self, name: &str, file: &str, line: u32, module: u32, kind: u8) -> u32 {
        let name = self.string(name);
        let file = self.string(file);

        if let Some(position) = self.sites.iter().position(|site| {
            site.name.get() == name
                && site.file.get() == file
                && site.line.get() == line
                && site.module.get() == module
                && site.kind == kind
        }) {
            return index(position);
        }

        let site = index(self.sites.len());
        self.sites.push(Site {
            name: name.into(),
            file: file.into(),
            line: line.into(),
            module: module.into(),
            kind,
            reserved: [0; 3],
        });
        site
    }

    /// Records a zone of a thread, at the depth its nesting in the
    /// thread's other zones gives it.
    pub fn zone(&mut self, thread: u32, zone: Zone) {
        self.zones.entry(thread).or_default().push(zone);
    }

    /// Records a value of the series a site plots.
    pub fn plot(&mut self, thread: u32, site: u32, time: i64, value: f64) {
        self.plots.push(Plot {
            time: time.into(),
            value: value.to_bits().into(),
            site: site.into(),
            thread: thread.into(),
        });
    }

    /// Marks an instant of a thread, from a site.
    pub fn thread_marker(
        &mut self,
        (thread, site): (u32, u32),
        time: i64,
        kind: MarkerKind,
        text: Option<&str>,
    ) {
        let text = self.optional(text);
        self.markers.push(Marker {
            time: time.into(),
            thread: thread.into(),
            site: site.into(),
            text: text.into(),
            kind: kind as u8,
            reserved: [0; 3],
        });
    }

    /// The task a runtime numbers `number`, first seen at `time`.
    pub fn task(&mut self, runtime: &str, number: u64, time: i64) -> u32 {
        let runtime = self.string(runtime);
        if let Some(&task) = self.task_index.get(&(runtime, number)) {
            let record = &mut self.tasks[task as usize];
            if record.last_seen.get() < time {
                record.last_seen = time.into();
            }

            return task;
        }

        let task = index(self.tasks.len());
        self.tasks.push(Task {
            runtime: runtime.into(),
            number: number.into(),
            root: NONE.into(),
            labels: NONE.into(),
            first_seen: time.into(),
            last_seen: time.into(),
        });
        self.task_index.insert((runtime, number), task);
        task
    }

    /// Sets a task's root frame and labels, each unless `NONE`.
    pub fn describe_task(&mut self, task: u32, root: u32, labels: Option<&str>) {
        let labels = self.optional(labels);
        let record = &mut self.tasks[task as usize];
        if root != NONE {
            record.root = root.into();
        }

        if labels != NONE {
            record.labels = labels.into();
        }
    }

    /// Marks an instant of the whole recording, saying what happened.
    pub fn marker(&mut self, time: i64, kind: MarkerKind, text: &str) {
        let text = self.string(text);
        self.markers.push(Marker {
            time: time.into(),
            thread: NONE.into(),
            site: NONE.into(),
            text: text.into(),
            kind: kind as u8,
            reserved: [0; 3],
        });
    }

    /// Records that nothing was sampled from `start` to `end`.
    pub fn gap(&mut self, start: i64, end: i64, reason: GapReason) {
        self.gaps.push(Gap {
            start: start.into(),
            end: end.into(),
            reason: reason as u8,
        });
    }

    /// Forgets every row of what happened before `time`, as a rolling
    /// window does, and cuts the spans that cross it there. What rows
    /// refer to, such as threads, frames, and stacks, stays, and so do the
    /// counts.
    pub fn forget_before(&mut self, time: i64) {
        for samples in &mut self.samples {
            samples.retain(|sample| sample.time.get() >= time);
        }

        for intervals in &mut self.off_cpu {
            intervals.retain_mut(|interval| {
                let start = interval.time.get();
                let end = start.saturating_add_unsigned(interval.duration.get());
                if start < time && end > time {
                    interval.time = time.into();
                    interval.duration = end.abs_diff(time).into();
                }

                end > time
            });
        }

        self.gaps.retain_mut(|gap| {
            if gap.start.get() < time {
                gap.start = time.min(gap.end.get()).into();
            }

            gap.end.get() > time
        });

        // A zone open at the cut keeps the part after it, as an interval
        // off a CPU does, beginning unseen.
        for zones in self.zones.values_mut() {
            zones.retain_mut(|zone| {
                let (start, end) = (zone.start.get(), zone.end.get());
                if start < time && end > time {
                    zone.start = time.into();
                    zone.flags = (zone.flags.get() | ZONE_BEGAN_UNSEEN).into();
                }

                end > time || start >= time
            });
        }

        self.plots.retain(|plot| plot.time.get() >= time);
        self.markers.retain(|marker| marker.time.get() >= time);
        self.task_waits.retain(|wait| wait.time.get() >= time);
        self.forget_syscalls(|at| at >= time);
    }

    /// Forgets every row of what happened at `time` or after, as a segment
    /// that ends there does, and cuts the spans that cross it there.
    pub fn forget_from(&mut self, time: i64) {
        for samples in &mut self.samples {
            samples.retain(|sample| sample.time.get() < time);
        }

        for intervals in &mut self.off_cpu {
            intervals.retain_mut(|interval| {
                let start = interval.time.get();
                let length = time.saturating_sub(start).max(0).unsigned_abs();
                interval.duration = interval.duration.get().min(length).into();
                start < time
            });
        }

        self.gaps.retain_mut(|gap| {
            gap.end = gap.end.get().min(time).into();
            gap.start.get() < time
        });

        // A zone open at the cut keeps the part before it, ending unseen.
        for zones in self.zones.values_mut() {
            zones.retain_mut(|zone| {
                if zone.end.get() > time {
                    zone.end = time.into();
                    zone.flags = (zone.flags.get() | ZONE_ENDED_UNSEEN).into();
                }

                zone.start.get() < time
            });
        }

        self.plots.retain(|plot| plot.time.get() < time);
        self.markers.retain(|marker| marker.time.get() < time);
        self.task_waits.retain(|wait| wait.time.get() < time);
        self.forget_syscalls(|at| at < time);
    }

    /// Keeps the calls and notes of the times `keep` keeps, and only their
    /// bytes, each note still among the calls it came between.
    fn forget_syscalls(&mut self, keep: impl Fn(i64) -> bool) {
        let mut kept_before = Vec::with_capacity(self.syscalls.len() + 1);
        kept_before.push(0);
        let mut kept = 0;

        for call in &self.syscalls {
            kept += u32::from(keep(call_time(call)));
            kept_before.push(kept);
        }

        let blobs = std::mem::take(&mut self.syscall_blobs);
        let bytes = |start: u32, len: u32| {
            if start == NONE {
                &[][..]
            } else {
                &blobs[start as usize..(start + len) as usize]
            }
        };

        let calls = std::mem::take(&mut self.syscalls);

        for call in calls.into_iter().filter(|call| keep(call_time(call))) {
            self.syscall(call, bytes(call.captures.get(), call.captures_len.get()));
        }

        let notes = std::mem::take(&mut self.syscall_notes);

        for mut note in notes.into_iter().filter(|note| keep(note.time.get())) {
            note.calls = kept_before[note.calls.get() as usize].into();
            self.push_note(note, bytes(note.blob.get(), note.blob_len.get()));
        }
    }

    /// Every count, by kind.
    pub const fn accounts(&self) -> &BTreeMap<u32, u64> {
        &self.accounts
    }

    /// Takes `base` from every count, so that they count only what came
    /// after it.
    pub fn discount(&mut self, base: &BTreeMap<u32, u64>) {
        for (kind, value) in &mut self.accounts {
            *value = value.saturating_sub(base.get(kind).copied().unwrap_or(0));
        }

        self.accounts.retain(|_, value| *value > 0);
    }

    /// Keeps bytes in the blobs: where they start and how many, or
    /// [`NONE`] for none.
    fn blob(&mut self, bytes: &[u8]) -> (u32, u32) {
        if bytes.is_empty() {
            return (NONE, 0);
        }

        let start = index(self.syscall_blobs.len());
        self.syscall_blobs.extend_from_slice(bytes);
        (start, index(bytes.len()))
    }

    /// Records a system call, with the bytes captured behind its
    /// arguments.
    pub fn syscall(&mut self, mut call: Syscall, captures: &[u8]) {
        let (start, len) = self.blob(captures);
        call.captures = start.into();
        call.captures_len = len.into();
        self.syscalls.push(call);
    }

    /// Records what a system call trace saw besides a call, after the
    /// calls so far, with its bytes.
    pub fn syscall_note(&mut self, mut note: SyscallNote, bytes: &[u8]) {
        note.calls = index(self.syscalls.len()).into();
        self.push_note(note, bytes);
    }

    fn push_note(&mut self, mut note: SyscallNote, bytes: &[u8]) {
        let (start, len) = self.blob(bytes);
        note.blob = start.into();
        note.blob_len = len.into();
        self.syscall_notes.push(note);
    }

    /// Records a process's statistics of a call that the kernel counted,
    /// which a trace with no calls holds instead.
    pub fn syscall_stat(&mut self, mut stat: SyscallStat, errnos: &[SyscallErrno]) {
        stat.errnos = index(self.syscall_errnos.len()).into();
        stat.errno_count = index(errnos.len()).into();
        self.syscall_errnos.extend_from_slice(errnos);
        self.syscall_stats.push(stat);
    }

    /// Records where a task waited.
    pub fn task_wait(&mut self, task: u32, time: i64, weight: u64, stack: u32, state: &str) {
        let state = self.string(state);
        self.task_waits.push(TaskWait {
            time: time.into(),
            weight: weight.into(),
            task: task.into(),
            stack: stack.into(),
            state: state.into(),
            reserved: 0.into(),
        });
    }

    /// Adds every row of `capture`, which may come from another chunk of
    /// the same recording: threads, modules, frames, and stacks are matched
    /// by what they are, not by index. Returns where its rows landed.
    pub fn absorb(&mut self, capture: &Capture) -> Remap {
        self.absorb_settings(capture);
        let mut map = Remap::default();
        self.absorb_identities(capture, &mut map);
        self.absorb_code(capture, &mut map);
        self.absorb_rows(capture, &map);
        map
    }

    /// This pool's name for a string of `capture`'s.
    fn copy_string(&mut self, capture: &Capture, id: u32) -> u32 {
        if id == NONE {
            NONE
        } else {
            self.string(capture.string(id))
        }
    }

    fn absorb_settings(&mut self, capture: &Capture) {
        for setting in &capture.settings {
            let key = capture.string(setting.key.get());
            let value = capture.string(setting.value.get());

            // Chunks of one recording span its time between them.
            let keep_existing = match (key, self.settings.get(key)) {
                ("start", Some(existing)) => existing.parse::<i64>().ok() <= value.parse().ok(),
                ("end", Some(existing)) => existing.parse::<i64>().ok() >= value.parse().ok(),
                _ => false,
            };

            if !keep_existing {
                self.setting(key, value);
            }
        }
    }

    fn absorb_identities(&mut self, capture: &Capture, map: &mut Remap) {
        for module in &capture.modules {
            let path = capture.string(module.path.get());
            let build_id =
                (module.build_id.get() != NONE).then(|| capture.string(module.build_id.get()));
            map.modules
                .push(self.module(path, build_id, module.digest, module.size.get()));
        }

        for process in &capture.processes {
            // One pid names one process at a time.
            let found = self.processes.iter().position(|existing| {
                existing.pid == process.pid
                    && overlap(
                        (existing.start.get(), existing.end.get()),
                        (process.start.get(), process.end.get()),
                    )
            });
            let index = if let Some(found) = found {
                index(found)
            } else {
                let name = self.copy_string(capture, process.name.get());
                self.processes.push(Process {
                    name: name.into(),
                    ..*process
                });
                index(self.processes.len() - 1)
            };

            let existing = &mut self.processes[index as usize];
            existing.end = existing.end.get().max(process.end.get()).into();
            map.processes.push(index);
        }

        for thread in &capture.threads {
            let process = map.processes[thread.process.get() as usize];
            let found = self.threads.iter().position(|existing| {
                existing.process.get() == process
                    && existing.tid == thread.tid
                    && overlap(
                        (existing.start.get(), existing.end.get()),
                        (thread.start.get(), thread.end.get()),
                    )
            });
            let name = self.copy_string(capture, thread.name.get());
            let index = if let Some(found) = found {
                index(found)
            } else {
                self.threads.push(Thread {
                    process: process.into(),
                    name: NONE.into(),
                    ..*thread
                });
                self.samples.push(Vec::new());
                self.off_cpu.push(Vec::new());
                index(self.threads.len() - 1)
            };

            let existing = &mut self.threads[index as usize];
            existing.end = existing.end.get().max(thread.end.get()).into();
            if existing.name.get() == NONE {
                existing.name = name.into();
            }

            map.threads.push(index);
        }

        for mapping in &capture.mappings {
            self.mappings.push(Mapping {
                process: map.processes[mapping.process.get() as usize].into(),
                module: map.module(mapping.module.get()).into(),
                ..*mapping
            });
        }
    }

    fn absorb_code(&mut self, capture: &Capture, map: &mut Remap) {
        for frame in &capture.frames {
            let kind = FrameKind::from_number(frame.kind).unwrap_or(FrameKind::Native);
            let module = map.module(frame.module.get());

            // A frame keyed by a string is keyed by the string's text, not
            // by where the other capture kept it.
            let address = match u32::try_from(frame.address.get()) {
                Ok(name) if kind.named() => u64::from(self.copy_string(capture, name)),
                _ => frame.address.get(),
            };

            map.frames
                .push(self.frame(kind, module, address, frame.role));
        }

        for (frame, &new) in map.frames.iter().enumerate() {
            let symbolized = capture
                .frame_symbols
                .get(frame)
                .is_some_and(|range| range.count.get() > 0);
            if !symbolized || self.symbolized(new) {
                continue;
            }

            let module = map.module(capture.frames[frame].module.get());
            let start = index(self.lines.len());
            let lines = capture.frame_lines(index(frame));

            for line in lines {
                let function = &capture.functions[line.function.get() as usize];
                let name = self.copy_string(capture, function.name.get());
                let declared = self.copy_string(capture, function.file.get());
                let function =
                    self.function(name, module, (declared, function.line.get()), function.role);
                let file = self.copy_string(capture, line.file.get());
                self.lines.push(Line {
                    function: function.into(),
                    file: file.into(),
                    ..*line
                });
            }

            self.symbols.insert(new, range(start, lines.len()));
        }

        for stack in &capture.stacks {
            let parent = Remap::get(&map.stacks, stack.parent.get());
            let frame = map.frames[stack.frame.get() as usize];
            let stack = self.child(parent, frame);
            map.stacks.push(stack);
        }

        for site in &capture.sites {
            let name = capture.string(site.name.get());
            let file = capture.string(site.file.get());
            let module = map.module(site.module.get());
            map.sites
                .push(self.site(name, file, site.line.get(), module, site.kind));
        }

        for task in &capture.tasks {
            let runtime = capture.string(task.runtime.get());
            let new = self.task(runtime, task.number.get(), task.first_seen.get());
            self.task(runtime, task.number.get(), task.last_seen.get());

            // Snapshots find a task's labels with no root frame.
            let root = match task.root.get() {
                NONE => NONE,
                root => map.frames[root as usize],
            };

            let labels = (task.labels.get() != NONE).then(|| capture.string(task.labels.get()));
            self.describe_task(new, root, labels);
            map.tasks.push(new);
        }
    }

    fn absorb_rows(&mut self, capture: &Capture, map: &Remap) {
        let sample = |sample: &Sample| Sample {
            stack: Remap::get(&map.stacks, sample.stack.get()).into(),
            task: Remap::get(&map.tasks, sample.task.get()).into(),
            ..*sample
        };

        for range in &capture.sample_ranges {
            let thread = map.threads[range.thread.get() as usize] as usize;
            self.samples[thread].extend(capture.thread_samples(range).iter().map(sample));
        }

        for range in &capture.off_cpu_ranges {
            let thread = map.threads[range.thread.get() as usize] as usize;
            self.off_cpu[thread].extend_from_slice(capture.thread_off_cpu(range));
        }

        for account in &capture.accounting {
            self.count(account.kind.get(), account.value.get());
        }

        // Deepest first, so that of two zones of one span, the one that
        // holds the other comes later, as when it was recorded.
        for range in capture.zone_ranges.iter().rev() {
            let thread = map.threads[range.thread.get() as usize];
            let start = range.start.get() as usize;

            for zone in &capture.zones[start..start + range.count.get() as usize] {
                let value = self.copy_string(capture, zone.value.get());
                let zone = Zone {
                    site: map.sites[zone.site.get() as usize].into(),
                    value: value.into(),
                    ..*zone
                };

                self.zone(thread, zone);
            }
        }

        for plot in &capture.plots {
            self.plots.push(Plot {
                site: Remap::get(&map.sites, plot.site.get()).into(),
                thread: Remap::get(&map.threads, plot.thread.get()).into(),
                ..*plot
            });
        }

        for marker in &capture.markers {
            let text = self.copy_string(capture, marker.text.get());
            self.markers.push(Marker {
                thread: Remap::get(&map.threads, marker.thread.get()).into(),
                site: Remap::get(&map.sites, marker.site.get()).into(),
                text: text.into(),
                ..*marker
            });
        }

        for wait in &capture.task_waits {
            let state = capture.string(wait.state.get());
            let task = map.tasks[wait.task.get() as usize];
            let stack = Remap::get(&map.stacks, wait.stack.get());
            self.task_wait(task, wait.time.get(), wait.weight.get(), stack, state);
        }

        self.gaps.extend_from_slice(&capture.gaps);
        self.absorb_syscalls(capture, map);
    }

    fn absorb_syscalls(&mut self, capture: &Capture, map: &Remap) {
        let calls = index(self.syscalls.len());
        let bytes = |start: u32, len: u32| {
            if start == NONE {
                &[][..]
            } else {
                &capture.syscall_blobs[start as usize..(start + len) as usize]
            }
        };

        for call in &capture.syscalls {
            let row = Syscall {
                thread: Remap::get(&map.threads, call.thread.get()).into(),
                stack: Remap::get(&map.stacks, call.stack.get()).into(),
                ..*call
            };

            self.syscall(row, bytes(call.captures.get(), call.captures_len.get()));
        }

        for note in &capture.syscall_notes {
            let text = self.copy_string(capture, note.text.get());
            let row = SyscallNote {
                thread: Remap::get(&map.threads, note.thread.get()).into(),
                process: Remap::get(&map.processes, note.process.get()).into(),
                stack: Remap::get(&map.stacks, note.stack.get()).into(),
                text: text.into(),
                calls: (calls + note.calls.get()).into(),
                ..*note
            };

            self.push_note(row, bytes(note.blob.get(), note.blob_len.get()));
        }

        for stat in &capture.syscall_stats {
            let start = stat.errnos.get() as usize;
            let errnos = &capture.syscall_errnos[start..start + stat.errno_count.get() as usize];
            let row = SyscallStat {
                process: Remap::get(&map.processes, stat.process.get()).into(),
                ..*stat
            };

            self.syscall_stat(row, errnos);
        }
    }

    /// The finished capture: each thread's rows in time order, zones by
    /// thread and depth.
    pub fn finish(self) -> Capture {
        let (samples, sample_ranges) = grouped(self.samples);
        let (off_cpu, off_cpu_ranges) = grouped(self.off_cpu);
        let frame_symbols = if self.symbols.is_empty() {
            Vec::new()
        } else {
            (0..self.frames.len())
                .map(|frame| {
                    self.symbols
                        .get(&index(frame))
                        .copied()
                        .unwrap_or_else(|| range(0, 0))
                })
                .collect()
        };

        let mut zones = Vec::new();
        let mut zone_ranges = Vec::new();

        for (thread, group) in self.zones {
            for (depth, group) in depths(group).into_iter().enumerate() {
                zone_ranges.push(ZoneRange {
                    thread: thread.into(),
                    depth: index(depth).into(),
                    start: index(zones.len()).into(),
                    count: index(group.len()).into(),
                });
                zones.extend(group);
            }
        }

        let mut plots = self.plots;
        plots.sort_by_key(|plot| (plot.site.get(), plot.time.get()));
        let mut markers = self.markers;
        markers.sort_by_key(|marker| (marker.time.get(), marker.thread.get()));
        let mut gaps = self.gaps;
        gaps.sort_by_key(|gap| gap.start.get());

        // The chunks of one file may hold gaps that overlap; a gap keeps only
        // what no earlier one covers.
        let mut covered = i64::MIN;
        gaps.retain_mut(|gap| {
            gap.start = gap.start.get().max(covered).into();
            covered = covered.max(gap.end.get());
            gap.start.get() <= gap.end.get()
        });
        let mut task_waits = self.task_waits;
        task_waits.sort_by_key(|wait| (wait.time.get(), wait.task.get()));
        let mut strings = self.strings;
        let settings = self
            .settings
            .iter()
            .map(|(key, value)| {
                let key = strings.push(key).map_or(NONE, |StrId(id)| id);
                let value = strings.push(value).map_or(NONE, |StrId(id)| id);
                Setting {
                    key: key.into(),
                    value: value.into(),
                }
            })
            .collect();
        let capture = Capture {
            strings: strings.into_bytes(),
            settings,
            modules: self.modules,
            processes: self.processes,
            threads: self.threads,
            mappings: self.mappings,
            frames: self.frames,
            stacks: self.stacks,
            samples,
            sample_ranges,
            frame_symbols,
            lines: self.lines,
            functions: self.functions,
            accounting: self
                .accounts
                .into_iter()
                .map(|(kind, value)| Account {
                    kind: kind.into(),
                    reserved: 0.into(),
                    value: value.into(),
                })
                .collect(),
            off_cpu,
            off_cpu_ranges,
            sites: self.sites,
            zones,
            zone_ranges,
            plots,
            markers,
            tasks: self.tasks,
            task_waits,
            gaps,
            syscalls: self.syscalls,
            syscall_blobs: self.syscall_blobs,
            syscall_notes: self.syscall_notes,
            syscall_stats: self.syscall_stats,
            syscall_errnos: self.syscall_errnos,
        };

        // A saved capture is read back through the same check.
        if cfg!(debug_assertions)
            && let Err(error) = capture.validate()
        {
            debug_assert!(false, "the builder's capture is invalid: {error}");
        }

        capture
    }
}

/// A thread's zones by depth, each depth's by start: a zone is one deeper
/// than the latest zone that began before it and ends after it begins.
/// Zones that cross rather than nest go deeper still, so that no two of one
/// depth overlap.
/// Of two zones with one span, the one recorded later holds the other, as a
/// zone that holds another ends after it.
fn depths(zones: Vec<Zone>) -> Vec<Vec<Zone>> {
    let mut zones: Vec<(usize, Zone)> = zones.into_iter().enumerate().collect();
    zones.sort_by_key(|(order, zone)| {
        (
            zone.start.get(),
            std::cmp::Reverse(zone.end.get()),
            std::cmp::Reverse(*order),
        )
    });
    let zones = zones.into_iter().map(|(_, zone)| zone);
    let mut depths: Vec<Vec<Zone>> = Vec::new();
    let mut open: Vec<i64> = Vec::new();

    for zone in zones {
        while open.last().is_some_and(|&end| end <= zone.start.get()) {
            open.pop();
        }

        let depth = open.len();
        open.push(zone.end.get());

        if depth == depths.len() {
            depths.push(Vec::new());
        }

        depths[depth].push(zone);
    }

    depths
}

/// A row's position as an id. Each of a capture's tables would need more
/// memory than any machine recording it has to pass `u32::MAX` rows, and
/// validation, which reading a capture runs, refuses a capture that does.
fn index(position: usize) -> u32 {
    let index = u32::try_from(position);
    debug_assert!(
        index.is_ok(),
        "a capture holds fewer than 2^32 rows, not {position}"
    );

    index.unwrap_or(u32::MAX)
}

/// Where each of another capture's rows landed in this one.
#[derive(Debug, Default)]
pub struct Remap {
    modules: Vec<u32>,
    processes: Vec<u32>,
    threads: Vec<u32>,
    frames: Vec<u32>,
    stacks: Vec<u32>,
    sites: Vec<u32>,
    tasks: Vec<u32>,
}

impl Remap {
    /// The new index of `id` in `table`, keeping [`NONE`].
    const fn get(table: &[u32], id: u32) -> u32 {
        if id == NONE { NONE } else { table[id as usize] }
    }

    fn module(&self, id: u32) -> u32 {
        Self::get(&self.modules, id)
    }

    /// The new row of thread `id`.
    pub fn thread(&self, id: u32) -> u32 {
        Self::get(&self.threads, id)
    }

    /// The new row of stack `id`, keeping [`NONE`].
    pub fn stack(&self, id: u32) -> u32 {
        Self::get(&self.stacks, id)
    }
}

/// A row of a table grouped by thread and ordered by time.
trait Timed {
    fn time(&self) -> i64;
}

impl Timed for Sample {
    fn time(&self) -> i64 {
        self.time.get()
    }
}

impl Timed for OffCpu {
    fn time(&self) -> i64 {
        self.time.get()
    }
}

fn range(start: u32, count: usize) -> Range {
    Range {
        start: start.into(),
        count: index(count).into(),
    }
}
