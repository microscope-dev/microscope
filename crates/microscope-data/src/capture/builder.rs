use std::collections::BTreeMap;

use foldhash::HashMap;

use super::Capture;
use super::records::{
    Account, Frame, FrameKind, Function, Gap, GapReason, Line, Mapping, Marker, MarkerKind, Module,
    NONE, OFF_CPU_BEGAN_UNSEEN, OFF_CPU_ENDED_UNSEEN, OffCpu, Plot, Process, Range, Sample,
    Setting, Site, Stack, Syscall, SyscallErrno, SyscallNote, SyscallStat, Task, TaskWait, Thread,
    ThreadRange, Truncation, UNSEEN, ZONE_BEGAN_UNSEEN, ZONE_ENDED_UNSEEN, Zone, ZoneRange,
};

use crate::image::{StrId, StringsBuilder};
use crate::model::{Pid, Tid};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineInput<'a> {
    pub function: &'a str,
    pub file: Option<&'a str>,
    pub line: u32,
    pub column: u32,

    pub declared: Option<(&'a str, u32)>,

    pub role: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterpretedPlace<'a> {
    pub function: &'a str,
    pub file: Option<&'a str>,
    pub line: u32,
    pub declared: Option<(&'a str, u32)>,
}

#[derive(Debug, Default, Clone)]
pub struct CaptureBuilder {
    strings: StringsBuilder,
    settings: BTreeMap<String, String>,
    modules: Vec<Module>,
    module_index: HashMap<(u32, u32, [u8; 16]), u32>,
    processes: Vec<Process>,
    live_processes: HashMap<Pid, u32>,

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

const fn covers(process: &Process, time: i64) -> bool {
    process.start.get() <= time && time <= process.end.get()
}

const fn overlap(a: (i64, i64), b: (i64, i64)) -> bool {
    a.0 <= b.1 && b.0 <= a.1
}

const fn call_time(call: &Syscall) -> i64 {
    if call.exited.get() == UNSEEN {
        call.entered.get()
    } else {
        call.exited.get()
    }
}

impl CaptureBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn string(&mut self, text: &str) -> u32 {
        let text = text.split('\0').next().unwrap_or_default();
        self.strings.push(text).map_or(NONE, |StrId(id)| id)
    }

    fn optional(&mut self, text: Option<&str>) -> u32 {
        text.map_or(NONE, |text| self.string(text))
    }

    pub fn setting(&mut self, key: &str, value: &str) {
        self.settings.insert(key.to_owned(), value.to_owned());
    }

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

    pub fn process(&mut self, pid: Pid, time: i64) -> u32 {
        if let Some(&process) = self.live_processes.get(&pid) {
            return process;
        }

        let ended = self.ended_processes.get(&pid).into_iter().flatten();
        if let Some(&process) = ended
            .rev()
            .find(|&&row| covers(own(&self.processes, row), time))
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

    #[must_use]
    pub fn process_end(&self, pid: Pid) -> Option<i64> {
        let process = *self.ended_processes.get(&pid)?.last()?;
        Some(own(&self.processes, process).end.get())
    }

    pub fn process_name(&mut self, process: u32, name: &str) {
        let name = self.string(name);
        own_mut(&mut self.processes, process).name = name.into();
    }

    pub fn process_exit(&mut self, pid: Pid, time: i64) {
        if let Some(process) = self.live_processes.remove(&pid) {
            own_mut(&mut self.processes, process).end = time.into();
            self.ended_processes.entry(pid).or_default().push(process);
            let ended: Vec<Tid> = self
                .live_threads
                .iter()
                .filter(|(_, thread)| own(&self.threads, **thread).process.get() == process)
                .map(|(tid, _)| *tid)
                .collect();

            for tid in ended {
                self.thread_exit(tid, time);
            }
        }
    }

    pub fn thread(&mut self, pid: Pid, tid: Tid, time: i64) -> u32 {
        if let Some(&thread) = self.live_threads.get(&tid) {
            let thread_record = own_mut(&mut self.threads, thread);
            if own(&self.processes, thread_record.process.get()).pid.get() == pid.get() {
                if thread_record.end.get() < time {
                    thread_record.end = time.into();
                }

                return thread;
            }

            self.live_threads.remove(&tid);
        }

        let ended = self.ended_threads.get(&tid).into_iter().flatten();
        if let Some(&thread) = ended.rev().find(|&&row| {
            let thread = own(&self.threads, row);
            thread.start.get() <= time
                && time <= thread.end.get()
                && own(&self.processes, thread.process.get()).pid.get() == pid.get()
        }) {
            return thread;
        }

        let process = self.process(pid, time);
        let record = own_mut(&mut self.processes, process);
        if record.end.get() < time {
            record.end = time.into();
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

    pub fn thread_name(&mut self, thread: u32, name: &str) {
        let name = self.string(name);
        own_mut(&mut self.threads, thread).name = name.into();
    }

    pub fn inherit_name(&mut self, thread: u32, from: Tid) {
        let Some(&maker) = self.live_threads.get(&from) else {
            return;
        };
        let name = own(&self.threads, maker).name;
        let thread = own_mut(&mut self.threads, thread);
        if thread.name.get() == NONE {
            thread.name = name;
        }
    }

    pub fn thread_exit(&mut self, tid: Tid, time: i64) {
        if let Some(thread) = self.live_threads.remove(&tid) {
            self.ended_threads.entry(tid).or_default().push(thread);
            let thread = own_mut(&mut self.threads, thread);
            if thread.end.get() < time {
                thread.end = time.into();
            }
        }
    }

    pub fn mapping(&mut self, mapping: Mapping) {
        self.mappings.push(mapping);
    }

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

    #[must_use]
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    pub fn truncation(&mut self, reason: Truncation, module: u32) -> u32 {
        self.frame(FrameKind::Truncated, module, reason as u64, 0)
    }

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

    pub fn interpreted(&mut self, place: &InterpretedPlace<'_>) -> u32 {
        let file = place.file.unwrap_or_default();

        let elsewhere = place
            .declared
            .map(|(declared, _)| declared)
            .filter(|declared| place.file != Some(*declared));
        let key = format!(
            "{}\t{}\t{}\t{}{file}{}{}",
            place.line,
            place.declared.map_or(-1, |(_, line)| i64::from(line)),
            file.len(),
            match (place.file, elsewhere) {
                (_, Some(_)) => 2,
                (Some(_), None) => 1,
                (None, None) => 0,
            },
            elsewhere.map_or_else(String::new, |declared| format!(
                "{}\t{declared}",
                declared.len()
            )),
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
                declared: place.declared,
                role: 0,
            };

            self.symbolize(frame, NONE, &[line]);
        }

        frame
    }

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

    pub fn stack(&mut self, frames: &[u32]) -> u32 {
        frames
            .iter()
            .rev()
            .fold(NONE, |parent, frame| self.child(parent, *frame))
    }

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
        own_mut(&mut self.samples, thread).push(Sample {
            time: time.into(),
            weight: weight.into(),
            stack: stack.into(),
            task: task.into(),
        });
    }

    #[must_use]
    pub fn sample_weight(&self) -> u64 {
        self.samples
            .iter()
            .flatten()
            .fold(0, |total, sample| total.saturating_add(sample.weight.get()))
    }

    pub fn scale_samples(&mut self, scale: impl Fn(i64) -> Option<(u64, u64)>) {
        for sample in self.samples.iter_mut().flatten() {
            if let Some((of, kept)) = scale(sample.time.get()) {
                let weight = u128::from(sample.weight.get()) * u128::from(of) / u128::from(kept);
                sample.weight = u64::try_from(weight).unwrap_or(u64::MAX).into();
            }
        }
    }

    pub fn fit_samples(&mut self, total: u64) -> Option<u64> {
        let timed = self
            .settings
            .get("weight")
            .is_none_or(|unit| unit == "nanoseconds");
        let sampled = self.sample_weight();
        if !timed || sampled == 0 || total == 0 {
            return None;
        }

        for sample in self.samples.iter_mut().flatten() {
            let weight = u128::from(sample.weight.get()) * u128::from(total) / u128::from(sampled);
            sample.weight = u64::try_from(weight).unwrap_or(u64::MAX).into();
        }
        Some(sampled)
    }

    pub fn claim(&mut self, thread: u32, time: i64, task: u32) -> u64 {
        let mut claimed = 0;

        for sample in own_mut(&mut self.samples, thread).iter_mut().rev() {
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

    pub fn off_cpu(&mut self, thread: u32, time: i64, duration: u64, preempted: bool, flags: u8) {
        own_mut(&mut self.off_cpu, thread).push(OffCpu {
            time: time.into(),
            duration: duration.into(),
            preempted: preempted.into(),
            flags,
        });
    }

    pub fn count(&mut self, kind: u32, value: u64) {
        if value > 0 {
            *self.accounts.entry(kind).or_default() += value;
        }
    }

    #[must_use]
    pub fn account(&self, kind: u32) -> u64 {
        self.accounts.get(&kind).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn symbolized(&self, frame: u32) -> bool {
        self.symbols.contains_key(&frame)
    }

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

    pub fn zone(&mut self, thread: u32, zone: Zone) {
        self.zones.entry(thread).or_default().push(zone);
    }

    pub fn plot(&mut self, thread: u32, site: u32, time: i64, value: f64) {
        self.plots.push(Plot {
            time: time.into(),
            value: value.to_bits().into(),
            site: site.into(),
            thread: thread.into(),
        });
    }

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

    pub fn task(&mut self, runtime: &str, number: u64, time: i64) -> u32 {
        let runtime = self.string(runtime);
        if let Some(&task) = self.task_index.get(&(runtime, number)) {
            let record = own_mut(&mut self.tasks, task);
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

    pub fn describe_task(&mut self, task: u32, root: u32, labels: Option<&str>) {
        let labels = self.optional(labels);
        let record = own_mut(&mut self.tasks, task);
        if root != NONE {
            record.root = root.into();
        }

        if labels != NONE {
            record.labels = labels.into();
        }
    }

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

    pub fn gap(&mut self, start: i64, end: i64, reason: GapReason) {
        self.gaps.push(Gap {
            start: start.into(),
            end: end.into(),
            reason: reason as u8,
        });
    }

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
                    interval.flags |= OFF_CPU_BEGAN_UNSEEN;
                }

                end > time || start >= time
            });
        }

        self.gaps.retain_mut(|gap| {
            let (start, end) = (gap.start.get(), gap.end.get());
            if start < time {
                gap.start = time.min(end).into();
            }

            end > time || start >= time
        });

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

    pub fn forget_from(&mut self, time: i64) {
        for samples in &mut self.samples {
            samples.retain(|sample| sample.time.get() < time);
        }

        for intervals in &mut self.off_cpu {
            intervals.retain_mut(|interval| {
                let start = interval.time.get();
                let length = time.saturating_sub(start).max(0).unsigned_abs();
                if interval.duration.get() > length {
                    interval.duration = length.into();
                    interval.flags |= OFF_CPU_ENDED_UNSEEN;
                }
                start < time
            });
        }

        self.gaps.retain_mut(|gap| {
            gap.end = gap.end.get().min(time).into();
            gap.start.get() < time
        });

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

    #[expect(
        clippy::indexing_slicing,
        reason = "a note counts at most the calls, which `kept_before` has one more entry \
                  than, and the builder's own calls and notes keep bytes in its blobs"
    )]
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

    #[must_use]
    pub const fn accounts(&self) -> &BTreeMap<u32, u64> {
        &self.accounts
    }

    pub fn restore_accounts(&mut self, accounts: &BTreeMap<u32, u64>) {
        self.accounts.clone_from(accounts);
    }

    pub fn discount(&mut self, base: &BTreeMap<u32, u64>) {
        for (kind, value) in &mut self.accounts {
            *value = value.saturating_sub(base.get(kind).copied().unwrap_or(0));
        }

        self.accounts.retain(|_, value| *value > 0);
    }

    fn blob(&mut self, bytes: &[u8]) -> (u32, u32) {
        if bytes.is_empty() {
            return (NONE, 0);
        }

        let start = index(self.syscall_blobs.len());
        self.syscall_blobs.extend_from_slice(bytes);
        (start, index(bytes.len()))
    }

    pub fn syscall(&mut self, mut call: Syscall, captures: &[u8]) {
        let (start, len) = self.blob(captures);
        call.captures = start.into();
        call.captures_len = len.into();
        self.syscalls.push(call);
    }

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

    pub fn syscall_stat(&mut self, mut stat: SyscallStat, errnos: &[SyscallErrno]) {
        stat.errnos = index(self.syscall_errnos.len()).into();
        stat.errno_count = index(errnos.len()).into();
        self.syscall_errnos.extend_from_slice(errnos);
        self.syscall_stats.push(stat);
    }

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

    pub fn absorb(&mut self, capture: &Capture) -> Remap {
        self.absorb_settings(capture);
        let mut map = Remap::default();
        self.absorb_identities(capture, &mut map);
        self.absorb_code(capture, &mut map);
        self.absorb_rows(capture, &map);
        map
    }

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

            let existing = own_mut(&mut self.processes, index);
            existing.end = existing.end.get().max(process.end.get()).into();
            map.processes.push(index);
        }

        for thread in &capture.threads {
            let process = Remap::get(&map.processes, thread.process.get());
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

            let existing = own_mut(&mut self.threads, index);
            existing.end = existing.end.get().max(thread.end.get()).into();
            if existing.name.get() == NONE {
                existing.name = name.into();
            }

            map.threads.push(index);
        }

        for mapping in &capture.mappings {
            self.mappings.push(Mapping {
                process: Remap::get(&map.processes, mapping.process.get()).into(),
                module: map.module(mapping.module.get()).into(),
                ..*mapping
            });
        }
    }

    fn absorb_code(&mut self, capture: &Capture, map: &mut Remap) {
        for frame in &capture.frames {
            let kind = FrameKind::from_number(frame.kind).unwrap_or(FrameKind::Native);
            let module = map.module(frame.module.get());

            let address = match u32::try_from(frame.address.get()) {
                Ok(name) if kind.named() => u64::from(self.copy_string(capture, name)),
                _ => frame.address.get(),
            };

            map.frames
                .push(self.frame(kind, module, address, frame.role));
        }

        for ((frame, &new), record) in map.frames.iter().enumerate().zip(&capture.frames) {
            let symbolized = capture
                .frame_symbols
                .get(frame)
                .is_some_and(|range| range.count.get() > 0);
            if !symbolized || self.symbolized(new) {
                continue;
            }

            let module = map.module(record.module.get());
            let start = index(self.lines.len());
            let lines = capture.frame_lines(index(frame));

            for line in lines {
                let function = capture.function(line.function.get());
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
            let frame = Remap::get(&map.frames, stack.frame.get());
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

            let root = match task.root.get() {
                NONE => NONE,
                root => Remap::get(&map.frames, root),
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
            let thread = Remap::get(&map.threads, range.thread.get());
            own_mut(&mut self.samples, thread)
                .extend(capture.thread_samples(range).iter().map(sample));
        }

        for range in &capture.off_cpu_ranges {
            let thread = Remap::get(&map.threads, range.thread.get());
            own_mut(&mut self.off_cpu, thread).extend_from_slice(capture.thread_off_cpu(range));
        }

        for account in &capture.accounting {
            self.count(account.kind.get(), account.value.get());
        }

        for range in capture.zone_ranges.iter().rev() {
            let thread = Remap::get(&map.threads, range.thread.get());
            for zone in capture.range_zones(range).iter().rev() {
                let value = self.copy_string(capture, zone.value.get());
                let zone = Zone {
                    site: Remap::get(&map.sites, zone.site.get()).into(),
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
            let task = Remap::get(&map.tasks, wait.task.get());
            let stack = Remap::get(&map.stacks, wait.stack.get());
            self.task_wait(task, wait.time.get(), wait.weight.get(), stack, state);
        }

        self.gaps.extend_from_slice(&capture.gaps);
        self.absorb_syscalls(capture, map);
    }

    fn absorb_syscalls(&mut self, capture: &Capture, map: &Remap) {
        let calls = index(self.syscalls.len());
        let bytes = |start: u32, len: u32| capture.blob(start, len);

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
            let errnos = capture.stat_errnos(stat);
            let row = SyscallStat {
                process: Remap::get(&map.processes, stat.process.get()).into(),
                thread: Remap::get(&map.threads, stat.thread.get()).into(),
                ..*stat
            };

            self.syscall_stat(row, errnos);
        }
    }

    #[must_use]
    pub fn finish(self) -> Capture {
        let capture = self.assemble();

        if cfg!(debug_assertions)
            && let Err(error) = capture.validate()
        {
            debug_assert!(false, "the builder's capture is invalid: {error}");
        }

        capture
    }

    pub fn finish_read(self) -> Result<Capture, String> {
        let capture = self.assemble();
        capture.validate()?;
        Ok(capture)
    }

    fn assemble(self) -> Capture {
        let _span = crate::span!("capture build");
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
        Capture {
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
        }
    }
}

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

        match depths.get_mut(depth) {
            Some(depth) => depth.push(zone),
            None => depths.push(vec![zone]),
        }
    }

    depths
}

fn index(position: usize) -> u32 {
    let index = u32::try_from(position);
    debug_assert!(
        index.is_ok(),
        "a capture holds fewer than 2^32 rows, not {position}"
    );

    index.unwrap_or(u32::MAX)
}

#[expect(
    clippy::indexing_slicing,
    reason = "the builder hands out ids only of rows it pushed, and never removes \
              a process, thread, or task, or a thread's sample and interval lists"
)]
const fn own<T>(rows: &[T], id: u32) -> &T {
    &rows[id as usize]
}

#[expect(
    clippy::indexing_slicing,
    reason = "the builder hands out ids only of rows it pushed, and never removes \
              a process, thread, or task, or a thread's sample and interval lists"
)]
const fn own_mut<T>(rows: &mut [T], id: u32) -> &mut T {
    &mut rows[id as usize]
}

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
    #[expect(
        clippy::indexing_slicing,
        reason = "each table has an entry per row of the capture it remaps, and `validate` \
                  checks every id but `NONE` that the absorb passes"
    )]
    const fn get(table: &[u32], id: u32) -> u32 {
        if id == NONE { NONE } else { table[id as usize] }
    }

    fn module(&self, id: u32) -> u32 {
        Self::get(&self.modules, id)
    }
}

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
