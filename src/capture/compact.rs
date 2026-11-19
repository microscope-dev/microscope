use super::records::{FrameKind, NONE};
use super::{
    Capture, Frame, Function, Line, Mapping, Marker, Module, Plot, Process, Range, Sample, Setting,
    Site, Stack, Syscall, SyscallNote, SyscallStat, Task, TaskWait, Thread, ThreadRange, Zone,
    ZoneRange,
};
use crate::image::StringsBuilder;

struct Kept(Vec<bool>);

impl Kept {
    fn new(len: usize) -> Self {
        Self(vec![false; len])
    }

    fn keep(&mut self, row: u32) {
        if let Some(kept) = self.0.get_mut(row as usize) {
            *kept = true;
        }
    }

    fn has(&self, row: u32) -> bool {
        self.0.get(row as usize).copied().unwrap_or(false)
    }

    fn rows(&self) -> impl Iterator<Item = u32> + '_ {
        (0..)
            .zip(&self.0)
            .filter(|(_, kept)| **kept)
            .map(|(row, _)| row)
    }

    fn map(&self) -> Vec<u32> {
        let mut next = 0;
        self.0
            .iter()
            .map(|&kept| {
                if kept {
                    next += 1;
                    next - 1
                } else {
                    NONE
                }
            })
            .collect()
    }
}

fn kept<T: Copy>(rows: &[T], kept: &Kept) -> impl Iterator<Item = T> {
    rows.iter()
        .zip(&kept.0)
        .filter(|(_, kept)| **kept)
        .map(|(row, _)| *row)
}

const fn overlaps(start: i64, end: i64, (lo, hi): (i64, i64)) -> bool {
    if start == end {
        lo <= start && start < hi
    } else {
        start < hi && end > lo
    }
}

struct Marks {
    threads: Kept,
    processes: Kept,
    stacks: Kept,
    tasks: Kept,
    sites: Kept,
    frames: Kept,
    functions: Kept,
    modules: Kept,
}

struct Maps {
    threads: Vec<u32>,
    processes: Vec<u32>,
    stacks: Vec<u32>,
    tasks: Vec<u32>,
    sites: Vec<u32>,
    frames: Vec<u32>,
    functions: Vec<u32>,
    modules: Vec<u32>,
}

impl Maps {
    fn get(map: &[u32], row: u32) -> u32 {
        if row == NONE {
            NONE
        } else {
            map.get(row as usize).copied().unwrap_or(NONE)
        }
    }
}

struct Pool<'a> {
    from: &'a Capture,
    strings: StringsBuilder,
}

impl Pool<'_> {
    fn copy(&mut self, id: u32) -> u32 {
        if id == NONE {
            return NONE;
        }
        self.strings
            .push(self.from.string(id))
            .map_or(NONE, |id| id.0)
    }
}

struct Compactor<'a> {
    from: &'a Capture,
    marks: Marks,
    maps: Maps,
    strings: Pool<'a>,
}

impl Capture {
    #[must_use]
    pub fn compact(&self, window: (i64, i64)) -> Self {
        let _span = crate::span!("capture compact");
        let marks = self.marks(window);
        let maps = Maps {
            threads: marks.threads.map(),
            processes: marks.processes.map(),
            stacks: marks.stacks.map(),
            tasks: marks.tasks.map(),
            sites: marks.sites.map(),
            frames: marks.frames.map(),
            functions: marks.functions.map(),
            modules: marks.modules.map(),
        };
        let mut compactor = Compactor {
            from: self,
            marks,
            maps,
            strings: Pool {
                from: self,
                strings: StringsBuilder::default(),
            },
        };

        let mut to = Self::default();
        compactor.identities(&mut to);
        compactor.code(&mut to);
        compactor.rows(&mut to);
        compactor.syscalls(&mut to);
        to.strings = compactor.strings.strings.into_bytes();
        to
    }

    fn marks(&self, window: (i64, i64)) -> Marks {
        let mut marks = Marks {
            threads: Kept::new(self.threads.len()),
            processes: Kept::new(self.processes.len()),
            stacks: Kept::new(self.stacks.len()),
            tasks: Kept::new(self.tasks.len()),
            sites: Kept::new(self.sites.len()),
            frames: Kept::new(self.frames.len()),
            functions: Kept::new(self.functions.len()),
            modules: Kept::new(self.modules.len()),
        };
        self.mark_rows(&mut marks);
        self.mark_lives(&mut marks, window);
        self.mark_code(&mut marks);
        marks
    }

    fn mark_rows(&self, marks: &mut Marks) {
        let ranges = (self.sample_ranges.iter())
            .chain(&self.off_cpu_ranges)
            .filter(|range| range.count.get() > 0)
            .map(|range| range.thread.get());
        let zones = (self.zone_ranges.iter())
            .filter(|range| range.count.get() > 0)
            .map(|range| range.thread.get());
        for thread in ranges.chain(zones) {
            marks.threads.keep(thread);
        }

        for sample in &self.samples {
            marks.stacks.keep(sample.stack.get());
            marks.tasks.keep(sample.task.get());
        }
        for zone in &self.zones {
            marks.sites.keep(zone.site.get());
        }
        for plot in &self.plots {
            marks.threads.keep(plot.thread.get());
            marks.sites.keep(plot.site.get());
        }
        for marker in &self.markers {
            marks.threads.keep(marker.thread.get());
            marks.sites.keep(marker.site.get());
        }
        for wait in &self.task_waits {
            marks.tasks.keep(wait.task.get());
            marks.stacks.keep(wait.stack.get());
        }
        for call in &self.syscalls {
            marks.threads.keep(call.thread.get());
            marks.stacks.keep(call.stack.get());
        }
        for note in &self.syscall_notes {
            marks.threads.keep(note.thread.get());
            marks.processes.keep(note.process.get());
            marks.stacks.keep(note.stack.get());
        }
        for stat in &self.syscall_stats {
            marks.threads.keep(stat.thread.get());
            marks.processes.keep(stat.process.get());
        }
    }

    fn mark_lives(&self, marks: &mut Marks, window: (i64, i64)) {
        for (row, thread) in (0..).zip(&self.threads) {
            if overlaps(thread.start.get(), thread.end.get(), window) {
                marks.threads.keep(row);
            }
            if marks.threads.has(row) {
                marks.processes.keep(thread.process.get());
            }
        }
        for (row, process) in (0..).zip(&self.processes) {
            if overlaps(process.start.get(), process.end.get(), window) {
                marks.processes.keep(row);
            }
        }
    }

    fn mark_code(&self, marks: &mut Marks) {
        for (row, stack) in self.stacks.iter().enumerate().rev() {
            let row = u32::try_from(row).unwrap_or(NONE);
            if marks.stacks.has(row) {
                marks.stacks.keep(stack.parent.get());
                marks.frames.keep(stack.frame.get());
            }
        }
        for row in marks.tasks.rows() {
            if let Some(task) = self.tasks.get(row as usize) {
                marks.frames.keep(task.root.get());
            }
        }
        for row in marks.frames.rows() {
            for line in self.frame_lines(row) {
                marks.functions.keep(line.function.get());
            }
        }

        let modules = (self.mappings.iter())
            .filter(|mapping| marks.processes.has(mapping.process.get()))
            .map(|mapping| mapping.module.get())
            .chain(kept(&self.frames, &marks.frames).map(|frame| frame.module.get()))
            .chain(kept(&self.functions, &marks.functions).map(|function| function.module.get()))
            .chain(kept(&self.sites, &marks.sites).map(|site| site.module.get()))
            .collect::<Vec<_>>();
        for module in modules {
            marks.modules.keep(module);
        }
    }
}

impl Compactor<'_> {
    fn identities(&mut self, to: &mut Capture) {
        let from = self.from;
        for setting in &from.settings {
            let key = self.strings.copy(setting.key.get()).into();
            let value = self.strings.copy(setting.value.get()).into();
            to.settings.push(Setting { key, value });
        }
        for module in kept(&from.modules, &self.marks.modules) {
            let path = self.strings.copy(module.path.get()).into();
            let build_id = self.strings.copy(module.build_id.get()).into();
            to.modules.push(Module {
                path,
                build_id,
                ..module
            });
        }
        for process in kept(&from.processes, &self.marks.processes) {
            let name = self.strings.copy(process.name.get()).into();
            to.processes.push(Process { name, ..process });
        }
        for thread in kept(&from.threads, &self.marks.threads) {
            let name = self.strings.copy(thread.name.get()).into();
            to.threads.push(Thread {
                process: Maps::get(&self.maps.processes, thread.process.get()).into(),
                name,
                ..thread
            });
        }
        to.mappings = (from.mappings.iter())
            .filter(|mapping| self.marks.processes.has(mapping.process.get()))
            .map(|mapping| Mapping {
                process: Maps::get(&self.maps.processes, mapping.process.get()).into(),
                module: Maps::get(&self.maps.modules, mapping.module.get()).into(),
                ..*mapping
            })
            .collect();
    }

    fn code(&mut self, to: &mut Capture) {
        let from = self.from;
        for frame in kept(&from.frames, &self.marks.frames) {
            let named = FrameKind::from_number(frame.kind).is_some_and(FrameKind::named);
            let address = match u32::try_from(frame.address.get()) {
                Ok(name) if named => u64::from(self.strings.copy(name)),
                _ => frame.address.get(),
            };
            to.frames.push(Frame {
                address: address.into(),
                module: Maps::get(&self.maps.modules, frame.module.get()).into(),
                ..frame
            });
        }
        to.stacks = kept(&from.stacks, &self.marks.stacks)
            .map(|stack| Stack {
                parent: Maps::get(&self.maps.stacks, stack.parent.get()).into(),
                frame: Maps::get(&self.maps.frames, stack.frame.get()).into(),
            })
            .collect();

        if !from.frame_symbols.is_empty() {
            let frames: Vec<u32> = self.marks.frames.rows().collect();
            for frame in frames {
                let lines = from.frame_lines(frame);
                to.frame_symbols.push(Range {
                    start: u32::try_from(to.lines.len()).unwrap_or(NONE).into(),
                    count: u32::try_from(lines.len()).unwrap_or(0).into(),
                });
                for line in lines {
                    let file = self.strings.copy(line.file.get()).into();
                    to.lines.push(Line {
                        function: Maps::get(&self.maps.functions, line.function.get()).into(),
                        file,
                        ..*line
                    });
                }
            }
        }

        for function in kept(&from.functions, &self.marks.functions) {
            let name = self.strings.copy(function.name.get()).into();
            let file = self.strings.copy(function.file.get()).into();
            to.functions.push(Function {
                name,
                module: Maps::get(&self.maps.modules, function.module.get()).into(),
                file,
                ..function
            });
        }
        for site in kept(&from.sites, &self.marks.sites) {
            let name = self.strings.copy(site.name.get()).into();
            let file = self.strings.copy(site.file.get()).into();
            to.sites.push(Site {
                name,
                file,
                module: Maps::get(&self.maps.modules, site.module.get()).into(),
                ..site
            });
        }
        for task in kept(&from.tasks, &self.marks.tasks) {
            let runtime = self.strings.copy(task.runtime.get()).into();
            let labels = self.strings.copy(task.labels.get()).into();
            to.tasks.push(Task {
                runtime,
                root: Maps::get(&self.maps.frames, task.root.get()).into(),
                labels,
                ..task
            });
        }
    }

    fn rows(&mut self, to: &mut Capture) {
        let from = self.from;
        let maps = &self.maps;
        let ranges = |ranges: &[ThreadRange]| {
            (ranges.iter())
                .filter(|range| range.count.get() > 0)
                .map(|range| ThreadRange {
                    thread: Maps::get(&maps.threads, range.thread.get()).into(),
                    ..*range
                })
                .collect()
        };
        to.samples = (from.samples.iter())
            .map(|sample| Sample {
                stack: Maps::get(&maps.stacks, sample.stack.get()).into(),
                task: Maps::get(&maps.tasks, sample.task.get()).into(),
                ..*sample
            })
            .collect();
        to.sample_ranges = ranges(&from.sample_ranges);
        to.off_cpu.clone_from(&from.off_cpu);
        to.off_cpu_ranges = ranges(&from.off_cpu_ranges);
        to.zone_ranges = (from.zone_ranges.iter())
            .filter(|range| range.count.get() > 0)
            .map(|range| ZoneRange {
                thread: Maps::get(&maps.threads, range.thread.get()).into(),
                ..*range
            })
            .collect();
        to.plots = (from.plots.iter())
            .map(|plot| Plot {
                site: Maps::get(&maps.sites, plot.site.get()).into(),
                thread: Maps::get(&maps.threads, plot.thread.get()).into(),
                ..*plot
            })
            .collect();
        to.accounting.clone_from(&from.accounting);
        to.gaps.clone_from(&from.gaps);

        for zone in &from.zones {
            let value = self.strings.copy(zone.value.get()).into();
            to.zones.push(Zone {
                site: Maps::get(&self.maps.sites, zone.site.get()).into(),
                value,
                ..*zone
            });
        }
        for marker in &from.markers {
            let text = self.strings.copy(marker.text.get()).into();
            to.markers.push(Marker {
                thread: Maps::get(&self.maps.threads, marker.thread.get()).into(),
                site: Maps::get(&self.maps.sites, marker.site.get()).into(),
                text,
                ..*marker
            });
        }
        for wait in &from.task_waits {
            let state = self.strings.copy(wait.state.get()).into();
            to.task_waits.push(TaskWait {
                task: Maps::get(&self.maps.tasks, wait.task.get()).into(),
                stack: Maps::get(&self.maps.stacks, wait.stack.get()).into(),
                state,
                ..*wait
            });
        }
    }

    fn syscalls(&mut self, to: &mut Capture) {
        let from = self.from;
        to.syscalls = (from.syscalls.iter())
            .map(|call| Syscall {
                thread: Maps::get(&self.maps.threads, call.thread.get()).into(),
                stack: Maps::get(&self.maps.stacks, call.stack.get()).into(),
                ..*call
            })
            .collect();
        to.syscall_blobs.clone_from(&from.syscall_blobs);
        to.syscall_stats = (from.syscall_stats.iter())
            .map(|stat| SyscallStat {
                process: Maps::get(&self.maps.processes, stat.process.get()).into(),
                thread: Maps::get(&self.maps.threads, stat.thread.get()).into(),
                ..*stat
            })
            .collect();
        to.syscall_errnos.clone_from(&from.syscall_errnos);

        for note in &from.syscall_notes {
            let text = self.strings.copy(note.text.get()).into();
            to.syscall_notes.push(SyscallNote {
                thread: Maps::get(&self.maps.threads, note.thread.get()).into(),
                process: Maps::get(&self.maps.processes, note.process.get()).into(),
                stack: Maps::get(&self.maps.stacks, note.stack.get()).into(),
                text,
                ..*note
            });
        }
    }
}
