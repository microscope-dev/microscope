use crate::capture::Capture;
use crate::capture::records::{AccountKind, CaptureTable, FrameKind};

use super::protocol::{
    ProfileCommand, ProfileFile, ProfileInspection, ProfileModule, ProfileProcess, ProfileTable,
};

const TIME_BINS: usize = 60;
const ARGUMENTS: &str = "arguments";

fn arguments_setting(process: u32) -> String {
    format!("arguments {process}")
}

fn command_setting(process: u32) -> String {
    format!("command {process}")
}

fn command(capture: &Capture, process: u32) -> Option<&str> {
    capture
        .setting(&command_setting(process))
        .filter(|command| !command.is_empty())
}

fn command_of(capture: &Capture, key: &str, text: Option<String>) -> Option<ProfileCommand> {
    let arguments = capture
        .setting(key)
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .filter(|arguments| !arguments.is_empty());
    let text = match &arguments {
        Some(arguments) => arguments.join(" "),
        None => text.filter(|text| !text.is_empty())?,
    };
    Some(ProfileCommand { arguments, text })
}

pub(super) fn recorded_command(capture: &Capture) -> Option<ProfileCommand> {
    let first = (!capture.processes.is_empty())
        .then(|| {
            command_of(
                capture,
                &arguments_setting(0),
                command(capture, 0).map(str::to_owned),
            )
        })
        .flatten();

    first.or_else(|| {
        command_of(
            capture,
            ARGUMENTS,
            capture.setting("program").map(str::to_owned),
        )
    })
}

pub(super) fn inspection(capture: &Capture, file: Option<ProfileFile>) -> ProfileInspection {
    let processes = capture
        .processes
        .iter()
        .enumerate()
        .map(|(row, process)| {
            let row = u32::try_from(row).unwrap_or(u32::MAX);
            ProfileProcess {
                pid: process.pid.get(),
                name: capture.string(process.name.get()).to_owned(),
                command: command_of(
                    capture,
                    &arguments_setting(row),
                    command(capture, row).map(str::to_owned),
                ),
            }
        })
        .collect();

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
        .map(|(module, frames)| ProfileModule {
            path: capture.string(module.path.get()).to_owned(),
            size: module.size.get(),
            build_id: Some(capture.string(module.build_id.get()).to_owned())
                .filter(|id| !id.is_empty()),
            frames,
        })
        .collect();

    let mut stacks: Vec<u32> = capture
        .samples
        .iter()
        .map(|sample| sample.stack.get())
        .collect();
    stacks.sort_unstable();
    stacks.dedup();

    ProfileInspection {
        processes,
        modules,
        depths: depths(capture),
        over_time: over_time(capture),
        distinct_stacks: stacks.len() as u64,
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
        recorder_finishing: capture
            .setting("final debug enrichment cpu ns")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        tables: capture
            .table_sizes()
            .filter(|(_, bytes)| *bytes > 0)
            .map(|(kind, bytes)| ProfileTable {
                name: table_name(kind).to_owned(),
                bytes,
            })
            .collect(),
        file,
    }
}

fn depths(capture: &Capture) -> Vec<u64> {
    let mut depth = Vec::with_capacity(capture.stacks.len());
    for node in &capture.stacks {
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

fn over_time(capture: &Capture) -> Vec<u64> {
    let samples = &capture.samples;
    if samples.is_empty() {
        return Vec::new();
    }

    let (start, end) = capture.span();
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
