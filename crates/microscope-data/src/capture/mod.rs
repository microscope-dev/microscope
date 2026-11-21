pub mod builder;
mod compact;
pub mod file;
#[cfg(test)]
mod properties;
pub mod records;
#[cfg(test)]
mod tests;

use zerocopy::IntoBytes as _;

use crate::image::{StrId, Strings};

pub use builder::CaptureBuilder;
pub use records::*;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capture {
    pub strings: Vec<u8>,
    pub settings: Vec<Setting>,
    pub modules: Vec<Module>,
    pub processes: Vec<Process>,
    pub threads: Vec<Thread>,
    pub mappings: Vec<Mapping>,
    pub frames: Vec<Frame>,
    pub stacks: Vec<Stack>,
    pub samples: Vec<Sample>,
    pub sample_ranges: Vec<ThreadRange>,
    pub frame_symbols: Vec<Range>,
    pub lines: Vec<Line>,
    pub functions: Vec<Function>,
    pub accounting: Vec<Account>,
    pub off_cpu: Vec<OffCpu>,
    pub off_cpu_ranges: Vec<ThreadRange>,
    pub sites: Vec<Site>,
    pub zones: Vec<Zone>,
    pub zone_ranges: Vec<ZoneRange>,
    pub plots: Vec<Plot>,
    pub markers: Vec<Marker>,
    pub tasks: Vec<Task>,
    pub task_waits: Vec<TaskWait>,
    pub gaps: Vec<Gap>,
    pub syscalls: Vec<Syscall>,
    pub syscall_blobs: Vec<u8>,
    pub syscall_notes: Vec<SyscallNote>,
    pub syscall_stats: Vec<SyscallStat>,
    pub syscall_errnos: Vec<SyscallErrno>,
}

impl Capture {
    #[must_use]
    pub fn string(&self, id: u32) -> &str {
        if id == NONE {
            return "";
        }

        Strings(&self.strings).get(StrId(id))
    }

    #[must_use]
    pub fn setting(&self, key: &str) -> Option<&str> {
        self.settings
            .iter()
            .find(|setting| self.string(setting.key.get()) == key)
            .map(|setting| self.string(setting.value.get()))
    }

    #[must_use]
    pub fn account(&self, kind: u32) -> u64 {
        self.accounting
            .iter()
            .filter(|account| account.kind.get() == kind)
            .map(|account| account.value.get())
            .sum()
    }

    #[must_use]
    pub fn thread_samples(&self, range: &ThreadRange) -> &[Sample] {
        rows(&self.samples, range.start.get(), range.count.get())
    }

    #[must_use]
    pub fn thread_off_cpu(&self, range: &ThreadRange) -> &[OffCpu] {
        rows(&self.off_cpu, range.start.get(), range.count.get())
    }

    #[must_use]
    pub fn frame_lines(&self, frame: u32) -> &[Line] {
        self.frame_symbols.get(frame as usize).map_or(&[], |range| {
            rows(&self.lines, range.start.get(), range.count.get())
        })
    }

    #[must_use]
    pub fn range_zones(&self, range: &ZoneRange) -> &[Zone] {
        rows(&self.zones, range.start.get(), range.count.get())
    }

    #[must_use]
    pub fn blob(&self, start: u32, len: u32) -> &[u8] {
        if start == NONE {
            return &[];
        }

        rows(&self.syscall_blobs, start, len)
    }

    #[must_use]
    pub fn stat_errnos(&self, stat: &SyscallStat) -> &[SyscallErrno] {
        rows(
            &self.syscall_errnos,
            stat.errnos.get(),
            stat.errno_count.get(),
        )
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "`validate` checks every stack's frame and task's root against the frames"
    )]
    #[must_use]
    pub fn frame(&self, frame: u32) -> &Frame {
        &self.frames[frame as usize]
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "`validate` checks every line's function against the functions"
    )]
    #[must_use]
    pub fn function(&self, function: u32) -> &Function {
        &self.functions[function as usize]
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "`validate` checks every module id but `NONE` against the modules"
    )]
    #[must_use]
    pub fn module(&self, module: u32) -> &Module {
        &self.modules[module as usize]
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "`validate` checks every site id but `NONE` against the sites"
    )]
    #[must_use]
    pub fn site(&self, site: u32) -> &Site {
        &self.sites[site as usize]
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "`validate` requires every sample, off-CPU, and zone range's thread to be a row"
    )]
    #[must_use]
    pub fn thread(&self, thread: u32) -> &Thread {
        &self.threads[thread as usize]
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "`validate` requires every thread's and mapping's process to be a row"
    )]
    #[must_use]
    pub fn process(&self, process: u32) -> &Process {
        &self.processes[process as usize]
    }

    #[expect(
        clippy::indexing_slicing,
        reason = "`validate` checks every task id but `NONE` against the tasks"
    )]
    #[must_use]
    pub fn task(&self, task: u32) -> &Task {
        &self.tasks[task as usize]
    }

    pub fn stack_frames(&self, mut stack: u32) -> impl Iterator<Item = u32> + '_ {
        std::iter::from_fn(move || {
            let node = self.stacks.get(stack as usize)?;
            stack = node.parent.get();
            Some(node.frame.get())
        })
    }

    #[must_use]
    pub fn span(&self) -> (i64, i64) {
        let time = |key| {
            self.setting(key)
                .and_then(|value| value.parse().ok())
                .unwrap_or(0)
        };

        (time("start"), time("end"))
    }

    #[must_use]
    pub fn tables(&self) -> [(CaptureTable, usize, usize, &[u8]); CaptureTable::COUNT] {
        macro_rules! rows {
            ($field:ident, $table:ident) => {
                (
                    CaptureTable::$table,
                    records::stride(CaptureTable::$table),
                    self.$field.len(),
                    self.$field.as_bytes(),
                )
            };
        }

        [
            (CaptureTable::Strings, 1, self.strings.len(), &self.strings),
            rows!(settings, Settings),
            rows!(modules, Modules),
            rows!(processes, Processes),
            rows!(threads, Threads),
            rows!(mappings, Mappings),
            rows!(frames, Frames),
            rows!(stacks, Stacks),
            rows!(samples, Samples),
            rows!(sample_ranges, SampleRanges),
            rows!(frame_symbols, FrameSymbols),
            rows!(lines, Lines),
            rows!(functions, Functions),
            rows!(accounting, Accounting),
            rows!(off_cpu, OffCpu),
            rows!(off_cpu_ranges, OffCpuRanges),
            rows!(sites, Sites),
            rows!(zones, Zones),
            rows!(zone_ranges, ZoneRanges),
            rows!(plots, Plots),
            rows!(markers, Markers),
            rows!(tasks, Tasks),
            rows!(task_waits, TaskWaits),
            rows!(gaps, Gaps),
            rows!(syscalls, Syscalls),
            (
                CaptureTable::SyscallBlobs,
                1,
                self.syscall_blobs.len(),
                &self.syscall_blobs,
            ),
            rows!(syscall_notes, SyscallNotes),
            rows!(syscall_stats, SyscallStats),
            rows!(syscall_errnos, SyscallErrnos),
        ]
    }

    pub fn table_sizes(&self) -> impl Iterator<Item = (CaptureTable, u64)> + '_ {
        self.tables()
            .into_iter()
            .map(|(kind, _, _, bytes)| (kind, bytes.len() as u64))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one check per table keeps every promise the tables make in one place"
    )]
    pub fn validate(&self) -> Result<(), String> {
        let _span = crate::span!("capture validate");
        if !crate::image::valid_pool(&self.strings, true) {
            return Err("the string pool is not NUL-terminated UTF-8".into());
        }

        let string = |id: u32, what: &str| {
            if id == NONE || crate::image::valid_reference(&self.strings, id, true) {
                Ok(())
            } else {
                Err(format!("{what} refers to a missing string"))
            }
        };

        let index = |id: u32, len: usize, what: &str| {
            if id == NONE || (id as usize) < len {
                Ok(())
            } else {
                Err(format!("{what} {id} is out of bounds"))
            }
        };

        let required = |id: u32, len: usize, what: &str| {
            if (id as usize) < len {
                Ok(())
            } else {
                Err(format!("{what} {id} is out of bounds"))
            }
        };

        for setting in &self.settings {
            string(setting.key.get(), "a setting")?;
            string(setting.value.get(), "a setting's value")?;
        }

        for module in &self.modules {
            string(module.path.get(), "a module")?;
            string(module.build_id.get(), "a build id")?;
        }

        for process in &self.processes {
            string(process.name.get(), "a process")?;
        }

        for thread in &self.threads {
            string(thread.name.get(), "a thread")?;
            required(
                thread.process.get(),
                self.processes.len(),
                "a thread's process",
            )?;
        }

        for mapping in &self.mappings {
            required(
                mapping.process.get(),
                self.processes.len(),
                "a mapping's process",
            )?;
            index(
                mapping.module.get(),
                self.modules.len(),
                "a mapping's module",
            )?;
        }

        for frame in &self.frames {
            index(frame.module.get(), self.modules.len(), "a frame's module")?;
            let Some(kind) = FrameKind::from_number(frame.kind) else {
                return Err(format!("a frame has unknown kind {}", frame.kind));
            };

            if kind.named() {
                let name = u32::try_from(frame.address.get())
                    .map_err(|_| "a named frame's key refers to a missing string".to_owned())?;
                string(name, "a named frame's key")?;
            }

            if role_of(frame.role).is_none() {
                return Err(format!("a frame has unknown role {}", frame.role));
            }
        }

        for (position, stack) in self.stacks.iter().enumerate() {
            let parent = stack.parent.get();
            if parent != NONE && parent as usize >= position {
                return Err("a stack's parent does not precede it".into());
            }

            if stack.frame.get() as usize >= self.frames.len() {
                return Err("a stack names no frame".into());
            }
        }

        grouped(
            &self.sample_ranges,
            &self.samples,
            self.threads.len(),
            |sample| sample.time.get(),
            "samples",
        )?;

        for sample in &self.samples {
            index(sample.stack.get(), self.stacks.len(), "samples")?;
            index(sample.task.get(), self.tasks.len(), "samples")?;
        }

        let samples = (self.samples.iter()).try_fold(0_u64, |total, sample| {
            total.checked_add(sample.weight.get())
        });
        let waits = (self.task_waits.iter())
            .try_fold(0_u64, |total, wait| total.checked_add(wait.weight.get()));
        if samples.is_none() || waits.is_none() {
            return Err(format!(
                "samples or waits weigh more than {} in all",
                u64::MAX
            ));
        }

        grouped(
            &self.off_cpu_ranges,
            &self.off_cpu,
            self.threads.len(),
            |interval| interval.time.get(),
            "off-CPU intervals",
        )?;

        if self.off_cpu.iter().any(|interval| interval.preempted > 1) {
            return Err("an off-CPU interval has an unknown cause".into());
        }
        let known = records::OFF_CPU_BEGAN_UNSEEN | records::OFF_CPU_ENDED_UNSEEN;
        if self
            .off_cpu
            .iter()
            .any(|interval| interval.flags & !known != 0)
        {
            return Err("an off-CPU interval has unknown flags".into());
        }

        if !self.frame_symbols.is_empty() && self.frame_symbols.len() != self.frames.len() {
            return Err("symbols do not describe every frame".into());
        }

        for range in &self.frame_symbols {
            let end = u64::from(range.start.get()) + u64::from(range.count.get());
            if end > self.lines.len() as u64 {
                return Err("a frame's lines are out of bounds".into());
            }
        }

        for line in &self.lines {
            if line.function.get() as usize >= self.functions.len() {
                return Err("a line names no function".into());
            }

            string(line.file.get(), "a line's file")?;
        }

        for function in &self.functions {
            if role_of(function.role).is_none() || function.reserved != [0; 3] {
                return Err("a function has an unknown role".into());
            }

            string(function.name.get(), "a function")?;
            string(function.file.get(), "a function's file")?;
            index(
                function.module.get(),
                self.modules.len(),
                "a function's module",
            )?;
        }

        for site in &self.sites {
            string(site.name.get(), "a site")?;
            string(site.file.get(), "a site's file")?;
            index(site.module.get(), self.modules.len(), "a site's module")?;
        }

        let mut next = 0;

        for range in &self.zone_ranges {
            required(
                range.thread.get(),
                self.threads.len(),
                "a zone range's thread",
            )?;
            if range.start.get() != next {
                return Err("zone ranges do not tile their table".into());
            }

            next = range
                .start
                .get()
                .checked_add(range.count.get())
                .ok_or("a zone range overflows")?;
            let group = self
                .zones
                .get(range.start.get() as usize..next as usize)
                .ok_or("a zone range is out of bounds")?;
            if group
                .windows(2)
                .any(|pair| matches!(pair, [a, b] if a.end.get() > b.start.get()))
            {
                return Err("zones of one depth overlap or are out of order".into());
            }
        }

        if next as usize != self.zones.len() {
            return Err("zone ranges do not cover their table".into());
        }

        for zone in &self.zones {
            if zone.site.get() as usize >= self.sites.len() {
                return Err("a zone names no site".into());
            }

            if zone.end.get() < zone.start.get() {
                return Err("a zone ends before it begins".into());
            }

            if zone.flags.get() & !ZONE_FLAGS != 0 || zone.reserved.get() != 0 {
                return Err("a zone has unknown flags".into());
            }

            string(zone.value.get(), "a label's value")?;
        }

        for plot in &self.plots {
            index(plot.site.get(), self.sites.len(), "a plot's site")?;
            index(plot.thread.get(), self.threads.len(), "a plot's thread")?;
        }

        for marker in &self.markers {
            if MarkerKind::from_number(marker.kind).is_none() {
                return Err(format!("a marker has unknown kind {}", marker.kind));
            }

            index(marker.thread.get(), self.threads.len(), "a marker's thread")?;
            index(marker.site.get(), self.sites.len(), "a marker's site")?;
            string(marker.text.get(), "a marker's text")?;
        }

        for task in &self.tasks {
            string(task.runtime.get(), "a task's runtime")?;
            string(task.labels.get(), "a task's labels")?;
            index(task.root.get(), self.frames.len(), "a task's root")?;
        }

        for wait in &self.task_waits {
            required(wait.task.get(), self.tasks.len(), "a wait's task")?;
            index(wait.stack.get(), self.stacks.len(), "a wait's stack")?;
            string(wait.state.get(), "a wait's state")?;
        }

        for gap in &self.gaps {
            if GapReason::from_number(gap.reason).is_none() {
                return Err(format!("a gap has unknown reason {}", gap.reason));
            }

            if gap.end.get() < gap.start.get() {
                return Err("a gap ends before it begins".into());
            }
        }

        if self
            .gaps
            .windows(2)
            .any(|pair| matches!(pair, [a, b] if b.start.get() < a.end.get()))
        {
            return Err("gaps overlap or are out of time order".into());
        }

        self.validate_syscalls()
    }

    fn validate_syscalls(&self) -> Result<(), String> {
        let index = |id: u32, len: usize, what: &str| {
            if id == NONE || (id as usize) < len {
                Ok(())
            } else {
                Err(format!("{what} {id} is out of bounds"))
            }
        };

        let blob = |start: u32, len: u32, what: &str| {
            let fits = start == NONE && len == 0
                || u64::from(start) + u64::from(len) <= self.syscall_blobs.len() as u64;
            if fits {
                Ok(())
            } else {
                Err(format!("{what} bytes are out of bounds"))
            }
        };

        for call in &self.syscalls {
            index(call.thread.get(), self.threads.len(), "a call's thread")?;
            index(call.stack.get(), self.stacks.len(), "a call's stack")?;
            blob(call.captures.get(), call.captures_len.get(), "a call's")?;
            let start = call.captures.get() as usize;
            if start != NONE as usize {
                let bytes = self
                    .syscall_blobs
                    .get(start..start + call.captures_len.get() as usize)
                    .ok_or("a call's bytes are out of bounds")?;
                if records::captures(bytes).is_none() {
                    return Err("a call's captured bytes are malformed".into());
                }
            }

            if call.abi > 2 || CallOutcome::from_number(call.outcome).is_none() {
                return Err("a call has an unknown ABI or outcome".into());
            }
        }

        let mut calls = 0;

        for note in &self.syscall_notes {
            if note.calls.get() < calls || note.calls.get() as usize > self.syscalls.len() {
                return Err("a note is out of order among the calls".into());
            }

            calls = note.calls.get();

            if NoteKind::from_number(note.kind).is_none() {
                return Err(format!("a note has unknown kind {}", note.kind));
            }

            index(note.thread.get(), self.threads.len(), "a note's thread")?;
            index(note.process.get(), self.processes.len(), "a note's process")?;
            index(note.stack.get(), self.stacks.len(), "a note's stack")?;

            if note.text.get() != NONE
                && !crate::image::valid_reference(&self.strings, note.text.get(), true)
            {
                return Err("a note's text refers to a missing string".into());
            }

            blob(note.blob.get(), note.blob_len.get(), "a note's")?;
        }

        for stat in &self.syscall_stats {
            index(
                stat.process.get(),
                self.processes.len(),
                "statistics' process",
            )?;
            index(stat.thread.get(), self.threads.len(), "statistics' thread")?;
            let end = u64::from(stat.errnos.get()) + u64::from(stat.errno_count.get());
            if end > self.syscall_errnos.len() as u64 || stat.abi > 2 {
                return Err("statistics refer to missing errnos or an unknown ABI".into());
            }
        }

        Ok(())
    }
}

fn grouped<T>(
    ranges: &[ThreadRange],
    rows: &[T],
    threads: usize,
    time: impl Fn(&T) -> i64,
    what: &str,
) -> Result<(), String> {
    let mut next = 0;
    for range in ranges {
        let thread = range.thread.get();
        if thread != NONE && thread as usize >= threads {
            return Err(format!("{what} {thread} is out of bounds"));
        }

        if thread == NONE || range.start.get() != next {
            return Err(format!("{what} ranges do not tile their table"));
        }

        next = range
            .start
            .get()
            .checked_add(range.count.get())
            .ok_or_else(|| format!("{what} range overflows"))?;
        let group = rows
            .get(range.start.get() as usize..next as usize)
            .ok_or_else(|| format!("{what} range is out of bounds"))?;
        if group
            .windows(2)
            .any(|pair| matches!(pair, [a, b] if time(a) > time(b)))
        {
            return Err(format!("{what} are out of time order"));
        }
    }

    if next as usize != rows.len() {
        return Err(format!("{what} ranges do not cover their table"));
    }

    Ok(())
}

#[expect(
    clippy::indexing_slicing,
    reason = "`validate` checks every range the accessors pass against its table"
)]
fn rows<T>(rows: &[T], start: u32, count: u32) -> &[T] {
    let start = start as usize;
    &rows[start..start + count as usize]
}
