//! What a profile holds, for the page's Inspect dialog: the command it
//! recorded, its modules and processes, the shape of its samples, and the
//! bytes each of its tables takes.

use super::{ARGUMENTS, Profile, arguments_setting};
use crate::capture::records::{AccountKind, CaptureTable, FrameKind};

/// How many equal slices of the span samples are counted in.
pub const TIME_BINS: usize = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspection {
    /// The command the recording ran or attached to: its first process's,
    /// or for a system call trace, the one it ran.
    pub command: Option<InspectedCommand>,

    /// Every process the recording saw, in the order it saw them.
    pub processes: Vec<InspectedProcess>,
    pub modules: Vec<InspectedModule>,

    /// Samples by how many frames their stack holds, the index being the
    /// depth. A truncation marker is no frame.
    pub depths: Vec<u64>,

    /// Samples in each of [`TIME_BINS`] slices of the span; empty when the
    /// samples keep no times or there are none.
    pub over_time: Vec<u64>,

    /// Distinct stacks samples ended in.
    pub distinct_stacks: u64,
    pub frames: u64,

    /// Frames that have source lines.
    pub frames_with_lines: u64,

    /// What the events counted, sampled or not, in the weight's unit.
    pub counted: u64,

    /// Bytes of stack the kernel copied.
    pub stack_bytes: u64,

    /// Samples whose stack copy microscope shortened while it was behind.
    pub shortened: u64,

    /// Nanoseconds of microscope's CPU time spent before the program was
    /// recorded, and reading debug information after collection ended.
    pub recorder_setup: u64,
    pub recorder_finishing: u64,

    /// Bytes of rows each table that has any holds, as a saved file stores
    /// them.
    pub tables: Vec<(&'static str, u64)>,
}

/// A command line: its arguments when the recording kept them apart, and
/// the line they make joined with spaces, which older recordings keep alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedCommand {
    pub arguments: Option<Vec<String>>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedProcess {
    pub pid: u32,
    pub name: String,
    pub command: Option<InspectedCommand>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectedModule {
    pub path: String,
    pub size: u64,

    /// Its GNU build ID in hex, if it has one.
    pub build_id: Option<String>,

    /// Distinct frames of its code the samples hold.
    pub frames: u64,
}

impl Profile {
    /// What the profile holds, for a person looking it over.
    #[must_use]
    pub fn inspect(&self) -> Inspection {
        let capture = &self.capture;

        let processes: Vec<_> = capture
            .processes
            .iter()
            .enumerate()
            .map(|(row, process)| {
                let row = u32::try_from(row).unwrap_or(u32::MAX);
                InspectedProcess {
                    pid: process.pid.get(),
                    name: capture.string(process.name.get()).to_owned(),
                    command: self.command_of(
                        &arguments_setting(row),
                        self.command(row).map(str::to_owned),
                    ),
                }
            })
            .collect();
        let command = self.recorded_command();

        let mut module_frames = vec![0_u64; capture.modules.len()];
        for frame in &capture.frames {
            if let Some(count) = module_frames.get_mut(frame.module.get() as usize) {
                *count += 1;
            }
        }
        let modules = capture
            .modules
            .iter()
            .zip(module_frames)
            .map(|(module, frames)| InspectedModule {
                path: capture.string(module.path.get()).to_owned(),
                size: module.size.get(),
                build_id: Some(capture.string(module.build_id.get()).to_owned())
                    .filter(|id| !id.is_empty()),
                frames,
            })
            .collect();

        Inspection {
            command,
            processes,
            modules,
            depths: self.depths(),
            over_time: self.over_time(),
            distinct_stacks: {
                let mut stacks: Vec<u32> = capture
                    .samples
                    .iter()
                    .map(|sample| sample.stack.get())
                    .collect();
                stacks.sort_unstable();
                stacks.dedup();
                stacks.len() as u64
            },
            frames: capture.frames.len() as u64,
            frames_with_lines: capture
                .frame_symbols
                .iter()
                .filter(|range| range.count.get() > 0)
                .count() as u64,
            counted: capture.account(AccountKind::Counted as u32),
            stack_bytes: capture.account(AccountKind::StackBytes as u32),
            shortened: capture.account(AccountKind::ShortenedStacks as u32),
            recorder_setup: capture.account(AccountKind::RecorderSetup as u32),
            recorder_finishing: self
                .setting("final debug enrichment cpu ns")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            tables: capture
                .table_sizes()
                .filter(|(_, bytes)| *bytes > 0)
                .map(|(kind, bytes)| (table_name(kind), bytes))
                .collect(),
        }
    }

    /// The command the recording ran or attached to: its first process's,
    /// or for a system call trace, the one it ran.
    #[must_use]
    pub fn recorded_command(&self) -> Option<InspectedCommand> {
        let first = (!self.capture.processes.is_empty())
            .then(|| self.command_of(&arguments_setting(0), self.command(0).map(str::to_owned)))
            .flatten();

        first.or_else(|| self.command_of(ARGUMENTS, self.setting("program").map(str::to_owned)))
    }

    /// A command from the arguments under `key`, or `text` alone when the
    /// recording did not keep them apart.
    fn command_of(&self, key: &str, text: Option<String>) -> Option<InspectedCommand> {
        let arguments = self
            .setting(key)
            .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
            .filter(|arguments| !arguments.is_empty());
        let text = match &arguments {
            Some(arguments) => arguments.join(" "),
            None => text.filter(|text| !text.is_empty())?,
        };
        Some(InspectedCommand { arguments, text })
    }

    /// Samples by the depth of their stacks. A parent comes before its
    /// children, so each node's depth is known by the time its children are.
    fn depths(&self) -> Vec<u64> {
        let capture = &self.capture;
        let mut depth = Vec::with_capacity(capture.stacks.len());
        for (index, node) in capture.stacks.iter().enumerate() {
            // depth holds every node before this one, so a parent at or
            // after it, or NONE, is not found.
            debug_assert_eq!(depth.len(), index, "depths of stack nodes before {index}");
            let above = depth.get(node.parent.get() as usize).copied().unwrap_or(0);
            let marker = capture
                .frames
                .get(node.frame.get() as usize)
                .is_some_and(|frame| frame.kind == FrameKind::Truncated as u8);
            depth.push(above + usize::from(!marker));
        }

        let mut counts = Vec::new();
        for sample in &capture.samples {
            let frames = depth.get(sample.stack.get() as usize).copied().unwrap_or(0);
            if counts.len() <= frames {
                counts.resize(frames + 1, 0);
            }
            if let Some(count) = counts.get_mut(frames) {
                *count += 1;
            }
        }
        counts
    }

    /// Samples in each slice of the span. A recording still running has no
    /// end yet, so its last sample stands for it.
    fn over_time(&self) -> Vec<u64> {
        let samples = &self.capture.samples;
        if !self.timed() || samples.is_empty() {
            return Vec::new();
        }

        let (start, end) = self.span();
        let last = samples
            .iter()
            .map(|sample| sample.time.get())
            .max()
            .unwrap_or(end);
        let first = samples
            .iter()
            .map(|sample| sample.time.get())
            .min()
            .unwrap_or(start);
        let start = if start == 0 { first } else { start.min(first) };
        let width = i128::from(end.max(last)) - i128::from(start) + 1;

        let mut bins = vec![0; TIME_BINS];
        for sample in samples {
            let offset = i128::from(sample.time.get()) - i128::from(start);
            let bin = usize::try_from(offset * TIME_BINS as i128 / width).unwrap_or(0);
            if let Some(count) = bins.get_mut(bin.min(TIME_BINS - 1)) {
                *count += 1;
            }
        }
        bins
    }
}

const fn table_name(kind: CaptureTable) -> &'static str {
    match kind {
        CaptureTable::Strings => "strings",
        CaptureTable::Settings => "settings",
        CaptureTable::Modules => "modules",
        CaptureTable::Processes => "processes",
        CaptureTable::Threads => "threads",
        CaptureTable::Mappings => "mappings",
        CaptureTable::Frames => "frames",
        CaptureTable::Stacks => "stacks",
        CaptureTable::Samples => "samples",
        CaptureTable::SampleRanges => "sample ranges",
        CaptureTable::FrameSymbols => "frame lines",
        CaptureTable::Lines => "source lines",
        CaptureTable::Functions => "functions",
        CaptureTable::Accounting => "accounting",
        CaptureTable::OffCpu => "off-CPU spans",
        CaptureTable::OffCpuRanges => "off-CPU ranges",
        CaptureTable::Sites => "probe sites",
        CaptureTable::Zones => "zones",
        CaptureTable::ZoneRanges => "zone ranges",
        CaptureTable::Plots => "plots",
        CaptureTable::Markers => "markers",
        CaptureTable::Tasks => "tasks",
        CaptureTable::TaskWaits => "task waits",
        CaptureTable::Gaps => "gaps",
        CaptureTable::Syscalls => "system calls",
        CaptureTable::SyscallBlobs => "call arguments",
        CaptureTable::SyscallNotes => "call notes",
        CaptureTable::SyscallStats => "call counts",
        CaptureTable::SyscallErrnos => "call errors",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::builder::CaptureBuilder;
    use crate::capture::records::{NONE, Truncation};
    use crate::model::{Pid, Tid};
    use crate::profiler::{command_setting, encode_cmdline};

    #[test]
    fn arguments_keep_their_boundaries() {
        let cases: [(&[u8], &str); 5] = [
            (b"", "[]"),
            (b"\0\0", "[]"),
            (b"rg\0-e\0a b\0", r#"["rg","-e","a b"]"#),
            (b"cmd\0\0last\0\0\0", r#"["cmd","","last"]"#),
            (b"no-terminator", r#"["no-terminator"]"#),
        ];
        for (cmdline, expected) in cases {
            assert_eq!(encode_cmdline(cmdline), expected, "{cmdline:?}");
        }
    }

    #[test]
    fn command_prefers_arguments_and_falls_back_to_text() {
        let mut builder = CaptureBuilder::new();
        builder.process(Pid::new(1), 0);
        builder.process(Pid::new(2), 0);
        builder.setting(&command_setting(0), "rg -e a b");
        builder.setting(&arguments_setting(0), &encode_cmdline(b"rg\0-e\0a b\0"));
        builder.setting(&command_setting(1), "old style");
        builder.setting(&arguments_setting(1), "not json");
        let inspection = Profile {
            capture: builder.finish(),
        }
        .inspect();

        let first = InspectedCommand {
            arguments: Some(vec!["rg".into(), "-e".into(), "a b".into()]),
            text: "rg -e a b".into(),
        };
        assert_eq!(inspection.command, Some(first.clone()));
        assert_eq!(inspection.processes[0].command, Some(first));
        assert_eq!(
            inspection.processes[1].command,
            Some(InspectedCommand {
                arguments: None,
                text: "old style".into()
            })
        );
    }

    #[test]
    fn a_trace_without_processes_uses_the_program_it_ran() {
        let mut builder = CaptureBuilder::new();
        builder.setting(ARGUMENTS, r#"["/bin/true","x y"]"#);
        let inspection = Profile {
            capture: builder.finish(),
        }
        .inspect();

        assert_eq!(
            inspection.command,
            Some(InspectedCommand {
                arguments: Some(vec!["/bin/true".into(), "x y".into()]),
                text: "/bin/true x y".into(),
            })
        );
    }

    #[test]
    fn depths_count_frames_but_not_truncation_markers() {
        let mut builder = CaptureBuilder::new();
        let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
        let module = builder.module("/bin/app", Some("abcd"), [1; 16], 4096);
        let other = builder.module("/lib/other.so", None, [2; 16], 10);
        let a = builder.frame(FrameKind::Native, module, 0x10, 0);
        let b = builder.frame(FrameKind::Native, module, 0x20, 0);
        let c = builder.frame(FrameKind::Native, other, 0x30, 0);
        let cut = builder.truncation(Truncation::StackCopyExhausted, module);

        // Depths 3, 3, 1, 2 (a marker below two frames), and no stack at all.
        let deep = builder.stack(&[c, b, a]);
        let shallow = builder.stack(&[a]);
        let cut_short = builder.stack(&[cut, b, a]);
        for (time, stack) in [
            (0, deep),
            (1, deep),
            (2, shallow),
            (3, cut_short),
            (4, NONE),
        ] {
            builder.sample(thread, time, 1, stack, NONE);
        }
        let inspection = Profile {
            capture: builder.finish(),
        }
        .inspect();

        assert_eq!(inspection.depths, vec![1, 1, 1, 2]);
        assert_eq!(inspection.distinct_stacks, 4);
        assert_eq!(inspection.frames, 4);

        let modules: Vec<_> = inspection
            .modules
            .iter()
            .map(|module| {
                (
                    module.path.as_str(),
                    module.build_id.as_deref(),
                    module.size,
                    module.frames,
                )
            })
            .collect();
        assert_eq!(
            modules,
            vec![
                ("/bin/app", Some("abcd"), 4096, 3),
                ("/lib/other.so", None, 10, 1)
            ]
        );
    }

    #[test]
    fn samples_fall_in_equal_slices_of_the_span() {
        let mut builder = CaptureBuilder::new();
        let thread = builder.thread(Pid::new(1), Tid::new(1), 0);

        // 600 ns over 60 bins is 10 ns each; check both sides of each edge.
        builder.setting("start", "1000");
        builder.setting("end", "1599");
        for time in [1000, 1009, 1010, 1300, 1589, 1590, 1599] {
            builder.sample(thread, time, 1, NONE, NONE);
        }
        let inspection = Profile {
            capture: builder.finish(),
        }
        .inspect();

        let mut expected = vec![0; TIME_BINS];
        expected[0] = 2;
        expected[1] = 1;
        expected[30] = 1;
        expected[58] = 1;
        expected[59] = 2;
        assert_eq!(inspection.over_time, expected);
        assert_eq!(inspection.depths, vec![7]);
    }

    #[test]
    fn untimed_or_empty_profiles_have_no_time_slices() {
        let empty = Profile {
            capture: CaptureBuilder::new().finish(),
        }
        .inspect();
        assert!(empty.over_time.is_empty());
        assert!(empty.depths.is_empty());
        assert_eq!(empty.command, None);

        let mut builder = CaptureBuilder::new();
        let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
        builder.setting("sample times", "none");
        builder.sample(thread, 0, 1, NONE, NONE);
        let untimed = Profile {
            capture: builder.finish(),
        }
        .inspect();
        assert!(untimed.over_time.is_empty());
    }

    #[test]
    fn tables_list_only_those_with_rows() {
        let mut builder = CaptureBuilder::new();
        let thread = builder.thread(Pid::new(1), Tid::new(1), 0);
        builder.sample(thread, 0, 1, NONE, NONE);
        let inspection = Profile {
            capture: builder.finish(),
        }
        .inspect();

        let names: Vec<_> = inspection.tables.iter().map(|(name, _)| *name).collect();
        assert!(names.contains(&"samples"), "{names:?}");
        assert!(!names.contains(&"system calls"), "{names:?}");
        assert!(inspection.tables.iter().all(|(_, bytes)| *bytes > 0));
    }
}
