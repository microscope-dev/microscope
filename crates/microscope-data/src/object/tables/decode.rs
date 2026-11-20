use std::collections::BTreeMap;

use super::super::codec::{
    Coding, SectionReader, WHOLE_SECTION_GROUP, decode_ints, decode_words, delta_by_key_decode_u64,
    delta_decode_i64, relative_decode_u64, zigzag_decode,
};
use super::super::{MAX_ROWS, ObjectError, RawSection, SectionKind, TimeUnit};
use super::{
    ChunkData, ChunkRow, DimensionRow, NOTE_BLOBS_ARGUMENT, SanitizerCountRow, read_delta_u64_col,
    read_relative_time_col, read_time_col, read_u8_col, read_u32_col, read_u64_col, u32_from_u64,
};
use crate::capture::Capture;
use crate::capture::records::{
    Account, Frame, Function, Gap, Line, Mapping, Marker, Module, NONE, OffCpu, Plot, Process,
    Range, Sample, Setting, Site, Stack, Syscall, SyscallErrno, SyscallNote, SyscallStat, Task,
    TaskWait, Thread, ThreadRange, UNSEEN, Zone, ZoneRange,
};
use crate::image::{StrId, Strings};

pub fn decode_tables(sections: &[RawSection<'_>]) -> Result<ChunkData, ObjectError> {
    decode_selected(sections, |_| true)
}

pub fn decode_flame_tables(sections: &[RawSection<'_>]) -> Result<ChunkData, ObjectError> {
    decode_selected(sections, SectionKind::needed_for_flame)
}

#[expect(
    clippy::too_many_lines,
    reason = "decodes each chunk table section into Capture and metadata"
)]
fn decode_selected(
    sections: &[RawSection<'_>],
    include: impl Fn(SectionKind) -> bool,
) -> Result<ChunkData, ObjectError> {
    let mut by_kind: BTreeMap<SectionKind, RawSection<'_>> = BTreeMap::new();
    for &sec in sections {
        if !include(sec.kind) {
            continue;
        }
        if by_kind.insert(sec.kind, sec).is_some() {
            return Err(ObjectError::Layout(format!(
                "duplicate section kind {:?}",
                sec.kind
            )));
        }
    }

    let mut capture = Capture::default();
    let mut chunks = Vec::new();
    let mut dimensions = Vec::new();
    let mut sanitizer_counts = Vec::new();

    if let Some(sec) = by_kind.get(&SectionKind::Strings) {
        let r = SectionReader::open(sec.bytes, SectionKind::Strings)?;
        capture.strings = r.column(WHOLE_SECTION_GROUP, 0)?.decompress()?;
    }

    if !crate::image::valid_pool(&capture.strings, true) {
        return Err(ObjectError::Capture(
            "string pool is not NUL-terminated UTF-8".into(),
        ));
    }

    if let Some(sec) = by_kind.get(&SectionKind::Settings) {
        let r = SectionReader::open(sec.bytes, SectionKind::Settings)?;
        let n = r.rows as usize;
        let keys = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let vals = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        capture.settings = keys
            .into_iter()
            .zip(vals)
            .map(|(k, v)| Setting {
                key: k.into(),
                value: v.into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Modules) {
        let r = SectionReader::open(sec.bytes, SectionKind::Modules)?;
        let n = r.rows as usize;
        let paths = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let bids = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let digests = r.column(WHOLE_SECTION_GROUP, 2)?.decompress()?;
        if digests.len() != n.saturating_mul(16) {
            return Err(ObjectError::Layout(
                "module digest column length mismatch".into(),
            ));
        }
        let sizes = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        capture.modules = (0..n)
            .map(|i| {
                let mut digest = [0_u8; 16];
                if let Some(slice) = digests.get(i * 16..(i + 1) * 16) {
                    digest.copy_from_slice(slice);
                }
                Module {
                    path: paths.get(i).copied().unwrap_or(NONE).into(),
                    build_id: bids.get(i).copied().unwrap_or(NONE).into(),
                    digest,
                    size: sizes.get(i).copied().unwrap_or(0).into(),
                }
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Processes) {
        let r = SectionReader::open(sec.bytes, SectionKind::Processes)?;
        let n = r.rows as usize;
        let pids = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let names = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let start_units = {
            let s = r.column(WHOLE_SECTION_GROUP, 2)?;
            let raw = s.decompress()?;
            delta_decode_i64(&decode_ints(&raw, n, s.bit_packed())?, r.base_time)
        };
        let ends = read_relative_time_col(&r, WHOLE_SECTION_GROUP, 3, &start_units)?;
        capture.processes = (0..n)
            .map(|i| Process {
                pid: pids.get(i).copied().unwrap_or(0).into(),
                name: names.get(i).copied().unwrap_or(NONE).into(),
                start: r
                    .time_unit
                    .to_nanos(start_units.get(i).copied().unwrap_or(0))
                    .into(),
                end: ends.get(i).copied().unwrap_or(0).into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Threads) {
        let r = SectionReader::open(sec.bytes, SectionKind::Threads)?;
        let n = r.rows as usize;
        let procs = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let tids = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let names = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let start_units = {
            let s = r.column(WHOLE_SECTION_GROUP, 3)?;
            let raw = s.decompress()?;
            delta_decode_i64(&decode_ints(&raw, n, s.bit_packed())?, r.base_time)
        };
        let ends = read_relative_time_col(&r, WHOLE_SECTION_GROUP, 4, &start_units)?;
        capture.threads = (0..n)
            .map(|i| Thread {
                process: procs.get(i).copied().unwrap_or(0).into(),
                tid: tids.get(i).copied().unwrap_or(0).into(),
                name: names.get(i).copied().unwrap_or(NONE).into(),
                reserved: 0.into(),
                start: r
                    .time_unit
                    .to_nanos(start_units.get(i).copied().unwrap_or(0))
                    .into(),
                end: ends.get(i).copied().unwrap_or(0).into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Mappings) {
        let r = SectionReader::open(sec.bytes, SectionKind::Mappings)?;
        let n = r.rows as usize;
        let procs = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let mods = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let starts = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let ends_raw = read_u64_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        let ends = relative_decode_u64(&ends_raw, &starts)?;
        let offs = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 4, n)?;
        let biases = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 5, n)?;
        let times = read_time_col(&r, WHOLE_SECTION_GROUP, 6, n)?;
        capture.mappings = (0..n)
            .map(|i| Mapping {
                process: procs.get(i).copied().unwrap_or(0).into(),
                module: mods.get(i).copied().unwrap_or(NONE).into(),
                start: starts.get(i).copied().unwrap_or(0).into(),
                end: ends.get(i).copied().unwrap_or(0).into(),
                file_offset: offs.get(i).copied().unwrap_or(0).into(),
                bias: biases.get(i).copied().unwrap_or(0).into(),
                time: times.get(i).copied().unwrap_or(0).into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Frames) {
        let r = SectionReader::open(sec.bytes, SectionKind::Frames)?;
        let n = r.rows as usize;
        let addrs = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let mods = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let kinds = read_u8_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let roles = read_u8_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        capture.frames = (0..n)
            .map(|i| Frame {
                address: addrs.get(i).copied().unwrap_or(0).into(),
                module: mods.get(i).copied().unwrap_or(NONE).into(),
                kind: kinds.get(i).copied().unwrap_or(0),
                role: roles.get(i).copied().unwrap_or(0),
                reserved: 0.into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Stacks) {
        let r = SectionReader::open(sec.bytes, SectionKind::Stacks)?;
        let n = r.rows as usize;
        let parents = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let frames = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        capture.stacks = parents
            .into_iter()
            .zip(frames)
            .map(|(p, f)| Stack {
                parent: p.into(),
                frame: f.into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Samples) {
        let r = SectionReader::open(sec.bytes, SectionKind::Samples)?;
        let n = r.rows as usize;
        let times = read_time_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let weights = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let stacks = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let tasks = read_u32_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        let keep_tasks = include(SectionKind::Tasks);
        capture.samples = (0..n)
            .map(|i| Sample {
                time: times.get(i).copied().unwrap_or(0).into(),
                weight: weights.get(i).copied().unwrap_or(0).into(),
                stack: stacks.get(i).copied().unwrap_or(NONE).into(),
                task: if keep_tasks {
                    tasks.get(i).copied().unwrap_or(NONE)
                } else {
                    NONE
                }
                .into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::SampleRanges) {
        capture.sample_ranges = decode_thread_ranges(sec.bytes, SectionKind::SampleRanges)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::FrameSymbols) {
        let r = SectionReader::open(sec.bytes, SectionKind::FrameSymbols)?;
        let n = r.rows as usize;
        let counts = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let explicit_starts = if r.optional_column(WHOLE_SECTION_GROUP, 1).is_some() {
            Some(read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?)
        } else {
            None
        };
        let mut next = 0_u32;
        capture.frame_symbols = counts
            .into_iter()
            .enumerate()
            .map(|(i, count)| {
                let start = explicit_starts
                    .as_ref()
                    .and_then(|s| s.get(i).copied())
                    .unwrap_or(next);
                next = next.saturating_add(count);
                Range {
                    start: start.into(),
                    count: count.into(),
                }
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Lines) {
        let r = SectionReader::open(sec.bytes, SectionKind::Lines)?;
        let n = r.rows as usize;
        let funcs = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let files = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let nums = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let cols = read_u32_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        capture.lines = (0..n)
            .map(|i| Line {
                function: funcs.get(i).copied().unwrap_or(0).into(),
                file: files.get(i).copied().unwrap_or(NONE).into(),
                number: nums.get(i).copied().unwrap_or(0).into(),
                column: cols.get(i).copied().unwrap_or(0).into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Functions) {
        let r = SectionReader::open(sec.bytes, SectionKind::Functions)?;
        let n = r.rows as usize;
        let names = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let mods = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let files = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let lines = read_u32_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        let roles = read_u8_col(&r, WHOLE_SECTION_GROUP, 4, n)?;
        capture.functions = (0..n)
            .map(|i| Function {
                name: names.get(i).copied().unwrap_or(NONE).into(),
                module: mods.get(i).copied().unwrap_or(NONE).into(),
                file: files.get(i).copied().unwrap_or(NONE).into(),
                line: lines.get(i).copied().unwrap_or(0).into(),
                role: roles.get(i).copied().unwrap_or(0),
                reserved: [0; 3],
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Accounting) {
        let r = SectionReader::open(sec.bytes, SectionKind::Accounting)?;
        let n = r.rows as usize;
        let kinds = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let vals = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        capture.accounting = kinds
            .into_iter()
            .zip(vals)
            .map(|(k, v)| Account {
                kind: k.into(),
                reserved: 0.into(),
                value: v.into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::OffCpu) {
        let r = SectionReader::open(sec.bytes, SectionKind::OffCpu)?;
        let n = r.rows as usize;
        let gaps = read_u64_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let durations = read_u64_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let packed = read_u8_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let mut prev_end = r.base_time;
        capture.off_cpu = (0..n)
            .map(|i| {
                let gap = zigzag_decode(gaps.get(i).copied().unwrap_or(0));
                let dur = durations.get(i).copied().unwrap_or(0);
                let start = prev_end.wrapping_add(gap);
                prev_end = start.saturating_add_unsigned(dur);
                let byte = packed.get(i).copied().unwrap_or(0);
                OffCpu {
                    time: r.time_unit.to_nanos(start).into(),
                    duration: dur
                        .saturating_mul(r.time_unit.nanos().cast_unsigned())
                        .into(),
                    preempted: byte & 1,
                    flags: byte >> 1,
                }
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::OffCpuRanges) {
        capture.off_cpu_ranges = decode_thread_ranges(sec.bytes, SectionKind::OffCpuRanges)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::Sites) {
        let r = SectionReader::open(sec.bytes, SectionKind::Sites)?;
        let n = r.rows as usize;
        let names = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let files = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let lines = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let mods = read_u32_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        let kinds = read_u8_col(&r, WHOLE_SECTION_GROUP, 4, n)?;
        capture.sites = (0..n)
            .map(|i| Site {
                name: names.get(i).copied().unwrap_or(NONE).into(),
                file: files.get(i).copied().unwrap_or(NONE).into(),
                line: lines.get(i).copied().unwrap_or(0).into(),
                module: mods.get(i).copied().unwrap_or(NONE).into(),
                kind: kinds.get(i).copied().unwrap_or(0),
                reserved: [0; 3],
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Zones) {
        let r = SectionReader::open(sec.bytes, SectionKind::Zones)?;
        let n = r.rows as usize;
        let start_units = {
            let s = r.column(WHOLE_SECTION_GROUP, 0)?;
            let raw = s.decompress()?;
            delta_decode_i64(&decode_ints(&raw, n, s.bit_packed())?, r.base_time)
        };
        let ends = read_relative_time_col(&r, WHOLE_SECTION_GROUP, 1, &start_units)?;
        let sites = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let flags = read_u32_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        let vals = read_u32_col(&r, WHOLE_SECTION_GROUP, 4, n)?;
        capture.zones = (0..n)
            .map(|i| Zone {
                start: r
                    .time_unit
                    .to_nanos(start_units.get(i).copied().unwrap_or(0))
                    .into(),
                end: ends.get(i).copied().unwrap_or(0).into(),
                site: sites.get(i).copied().unwrap_or(0).into(),
                flags: flags.get(i).copied().unwrap_or(0).into(),
                value: vals.get(i).copied().unwrap_or(NONE).into(),
                reserved: 0.into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::ZoneRanges) {
        let r = SectionReader::open(sec.bytes, SectionKind::ZoneRanges)?;
        let n = r.rows as usize;
        let threads = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let depths = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let counts = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let mut next = 0_u32;
        capture.zone_ranges = (0..n)
            .map(|i| {
                let count = counts.get(i).copied().unwrap_or(0);
                let start = next;
                next = next.saturating_add(count);
                ZoneRange {
                    thread: threads.get(i).copied().unwrap_or(0).into(),
                    depth: depths.get(i).copied().unwrap_or(0).into(),
                    start: start.into(),
                    count: count.into(),
                }
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Plots) {
        let r = SectionReader::open(sec.bytes, SectionKind::Plots)?;
        let n = r.rows as usize;
        let times = read_time_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let vals = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let sites = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let threads = read_u32_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        capture.plots = (0..n)
            .map(|i| Plot {
                time: times.get(i).copied().unwrap_or(0).into(),
                value: vals.get(i).copied().unwrap_or(0).into(),
                site: sites.get(i).copied().unwrap_or(NONE).into(),
                thread: threads.get(i).copied().unwrap_or(NONE).into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Markers) {
        let r = SectionReader::open(sec.bytes, SectionKind::Markers)?;
        let n = r.rows as usize;
        let times = read_time_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let threads = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let sites = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let texts = read_u32_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        let kinds = read_u8_col(&r, WHOLE_SECTION_GROUP, 4, n)?;
        capture.markers = (0..n)
            .map(|i| Marker {
                time: times.get(i).copied().unwrap_or(0).into(),
                thread: threads.get(i).copied().unwrap_or(NONE).into(),
                site: sites.get(i).copied().unwrap_or(NONE).into(),
                text: texts.get(i).copied().unwrap_or(NONE).into(),
                kind: kinds.get(i).copied().unwrap_or(0),
                reserved: [0; 3],
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Tasks) {
        let r = SectionReader::open(sec.bytes, SectionKind::Tasks)?;
        let n = r.rows as usize;
        let runtimes = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let numbers = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let roots = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let labels = read_u32_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        let first_units = {
            let s = r.column(WHOLE_SECTION_GROUP, 4)?;
            let raw = s.decompress()?;
            delta_decode_i64(&decode_ints(&raw, n, s.bit_packed())?, r.base_time)
        };
        let last = read_relative_time_col(&r, WHOLE_SECTION_GROUP, 5, &first_units)?;
        capture.tasks = (0..n)
            .map(|i| Task {
                runtime: runtimes.get(i).copied().unwrap_or(NONE).into(),
                number: numbers.get(i).copied().unwrap_or(0).into(),
                root: roots.get(i).copied().unwrap_or(NONE).into(),
                labels: labels.get(i).copied().unwrap_or(NONE).into(),
                first_seen: r
                    .time_unit
                    .to_nanos(first_units.get(i).copied().unwrap_or(0))
                    .into(),
                last_seen: last.get(i).copied().unwrap_or(0).into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::TaskWaits) {
        let r = SectionReader::open(sec.bytes, SectionKind::TaskWaits)?;
        let n = r.rows as usize;
        let times = read_time_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let weights = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let tasks = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let stacks = read_u32_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        let states = read_u32_col(&r, WHOLE_SECTION_GROUP, 4, n)?;
        capture.task_waits = (0..n)
            .map(|i| TaskWait {
                time: times.get(i).copied().unwrap_or(0).into(),
                weight: weights.get(i).copied().unwrap_or(0).into(),
                task: tasks.get(i).copied().unwrap_or(0).into(),
                stack: stacks.get(i).copied().unwrap_or(NONE).into(),
                state: states.get(i).copied().unwrap_or(NONE).into(),
                reserved: 0.into(),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Gaps) {
        let r = SectionReader::open(sec.bytes, SectionKind::Gaps)?;
        let n = r.rows as usize;
        let start_units = {
            let s = r.column(WHOLE_SECTION_GROUP, 0)?;
            let raw = s.decompress()?;
            delta_decode_i64(&decode_ints(&raw, n, s.bit_packed())?, r.base_time)
        };
        let ends = read_relative_time_col(&r, WHOLE_SECTION_GROUP, 1, &start_units)?;
        let reasons = read_u8_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        capture.gaps = (0..n)
            .map(|i| Gap {
                start: r
                    .time_unit
                    .to_nanos(start_units.get(i).copied().unwrap_or(0))
                    .into(),
                end: ends.get(i).copied().unwrap_or(0).into(),
                reason: reasons.get(i).copied().unwrap_or(0),
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::Syscalls) {
        capture.syscalls = decode_syscalls(sec.bytes)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::SyscallNotes) {
        let r = SectionReader::open(sec.bytes, SectionKind::SyscallNotes)?;
        let n = r.rows as usize;
        let times = read_time_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let values = read_u64_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let extras = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        let threads = read_u32_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
        let procs = read_u32_col(&r, WHOLE_SECTION_GROUP, 4, n)?;
        let texts = read_u32_col(&r, WHOLE_SECTION_GROUP, 5, n)?;
        let stacks = read_u32_col(&r, WHOLE_SECTION_GROUP, 6, n)?;
        let blens = read_u32_col(&r, WHOLE_SECTION_GROUP, 7, n)?;
        let calls = read_u32_col(&r, WHOLE_SECTION_GROUP, 8, n)?;
        let kinds = read_u8_col(&r, WHOLE_SECTION_GROUP, 9, n)?;
        let flags = read_u8_col(&r, WHOLE_SECTION_GROUP, 10, n)?;
        capture.syscall_notes = (0..n)
            .map(|i| SyscallNote {
                time: times.get(i).copied().unwrap_or(0).into(),
                value: zigzag_decode(values.get(i).copied().unwrap_or(0)).into(),
                extra: extras.get(i).copied().unwrap_or(0).into(),
                thread: threads.get(i).copied().unwrap_or(NONE).into(),
                process: procs.get(i).copied().unwrap_or(NONE).into(),
                text: texts.get(i).copied().unwrap_or(NONE).into(),
                stack: stacks.get(i).copied().unwrap_or(NONE).into(),
                blob: NONE.into(),
                blob_len: blens.get(i).copied().unwrap_or(0).into(),
                calls: calls.get(i).copied().unwrap_or(0).into(),
                kind: kinds.get(i).copied().unwrap_or(0),
                flags: flags.get(i).copied().unwrap_or(0),
                reserved: [0; 10],
            })
            .collect();
    }

    if let Some(sec) = by_kind.get(&SectionKind::SyscallBlobs) {
        decode_syscall_blobs(sec.bytes, &mut capture)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::SyscallStats) {
        capture.syscall_stats = decode_syscall_stats(sec.bytes)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::SyscallErrnos) {
        let r = SectionReader::open(sec.bytes, SectionKind::SyscallErrnos)?;
        let n = r.rows as usize;
        let counts = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let errnos = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        capture.syscall_errnos = counts
            .into_iter()
            .zip(errnos)
            .map(|(count, errno)| {
                let errno = u16::try_from(errno)
                    .map_err(|_| ObjectError::Layout("errno exceeds u16".into()))?;
                Ok(SyscallErrno {
                    count: count.into(),
                    errno: errno.into(),
                    reserved: [0; 6],
                })
            })
            .collect::<Result<Vec<_>, ObjectError>>()?;
    }

    let lookup_string = |id: u32| -> Result<String, ObjectError> {
        if id == NONE {
            return Ok(String::new());
        }
        if !crate::image::valid_reference(&capture.strings, id, true) {
            return Err(ObjectError::Capture(
                "string reference out of bounds".into(),
            ));
        }
        Ok(Strings(&capture.strings).get(StrId(id)).to_owned())
    };

    if let Some(sec) = by_kind.get(&SectionKind::Chunk) {
        let r = SectionReader::open(sec.bytes, SectionKind::Chunk)?;
        let n = r.rows as usize;
        let agents = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let numbers = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let start_units = {
            let s = r.column(WHOLE_SECTION_GROUP, 2)?;
            let raw = s.decompress()?;
            delta_decode_i64(&decode_ints(&raw, n, s.bit_packed())?, r.base_time)
        };
        let ends = read_relative_time_col(&r, WHOLE_SECTION_GROUP, 3, &start_units)?;
        let sampled = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 4, n)?;
        let presets = read_u8_col(&r, WHOLE_SECTION_GROUP, 5, n)?;
        let units = read_u8_col(&r, WHOLE_SECTION_GROUP, 6, n)?;
        let policies = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 7, n)?;
        let rules = read_u32_col(&r, WHOLE_SECTION_GROUP, 8, n)?;
        for i in 0..n {
            let unit_byte = units.get(i).copied().unwrap_or(0);
            let time_unit =
                TimeUnit::from_number(unit_byte).ok_or_else(|| ObjectError::UnknownValue {
                    what: "chunk time_unit",
                    value: u64::from(unit_byte),
                })?;
            chunks.push(ChunkRow {
                agent: agents.get(i).copied().unwrap_or(0),
                number: numbers.get(i).copied().unwrap_or(0),
                start: start_units.get(i).copied().unwrap_or(0),
                end: ends.get(i).copied().unwrap_or(0),
                sampled_ns: sampled.get(i).copied().unwrap_or(0),
                preset: presets.get(i).copied().unwrap_or(0),
                time_unit,
                policy: policies.get(i).copied().unwrap_or(0),
                rules: lookup_string(rules.get(i).copied().unwrap_or(NONE))?,
            });
        }
    }

    if let Some(sec) = by_kind.get(&SectionKind::Dimensions) {
        let r = SectionReader::open(sec.bytes, SectionKind::Dimensions)?;
        let n = r.rows as usize;
        let procs = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let names = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let vals = read_u32_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        for i in 0..n {
            dimensions.push(DimensionRow {
                process: procs.get(i).copied().unwrap_or(NONE),
                name: lookup_string(names.get(i).copied().unwrap_or(NONE))?,
                value: lookup_string(vals.get(i).copied().unwrap_or(NONE))?,
            });
        }
    }

    if let Some(sec) = by_kind.get(&SectionKind::SanitizerCounts) {
        let r = SectionReader::open(sec.bytes, SectionKind::SanitizerCounts)?;
        let n = r.rows as usize;
        let rules = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
        let fields = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
        let fired = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
        for i in 0..n {
            let field = u16::try_from(fields.get(i).copied().unwrap_or(0))
                .map_err(|_| ObjectError::Layout("sanitizer field class exceeds u16".into()))?;
            sanitizer_counts.push(SanitizerCountRow {
                rule: lookup_string(rules.get(i).copied().unwrap_or(NONE))?,
                field,
                fired: fired.get(i).copied().unwrap_or(0),
            });
        }
    }

    capture.validate().map_err(ObjectError::Capture)?;
    Ok(ChunkData {
        capture,
        chunks,
        dimensions,
        sanitizer_counts,
    })
}

fn decode_thread_ranges(bytes: &[u8], kind: SectionKind) -> Result<Vec<ThreadRange>, ObjectError> {
    let r = SectionReader::open(bytes, kind)?;
    let n = r.rows as usize;
    let threads = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
    let counts = read_u32_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
    let mut next = 0_u32;
    Ok(threads
        .into_iter()
        .zip(counts)
        .map(|(thread, count)| {
            let start = next;
            next = next.saturating_add(count);
            ThreadRange {
                thread: thread.into(),
                start: start.into(),
                count: count.into(),
            }
        })
        .collect())
}

#[expect(
    clippy::too_many_lines,
    reason = "decodes grouped syscall columns per abi and number"
)]
fn decode_syscalls(bytes: &[u8]) -> Result<Vec<Syscall>, ObjectError> {
    struct DecodedGroup {
        abi: u8,
        number: u32,
        durations: Vec<i64>,
        threads: Vec<u32>,
        rets: Vec<i64>,
        stacks: Vec<u32>,
        cap_lens: Vec<u32>,
        outcomes: Vec<u8>,
        flags: Vec<u8>,
        unseen: Option<Vec<u8>>,
        args: Vec<Vec<u64>>,
        cursor: usize,
    }

    let r = SectionReader::open(bytes, SectionKind::Syscalls)?;
    let n = r.rows as usize;
    let order = read_u64_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
    let exit_units = {
        let s = r.column(WHOLE_SECTION_GROUP, 1)?;
        let raw = s.decompress()?;
        delta_decode_i64(&decode_ints(&raw, n, s.bit_packed())?, r.base_time)
    };

    let mut groups = Vec::with_capacity(r.groups.len());
    for (gi, entry) in r.groups.iter().enumerate() {
        let g = u16::try_from(gi).map_err(|_| ObjectError::Limit("group index overflow"))?;
        let m = entry.rows.get() as usize;
        if m > MAX_ROWS {
            return Err(ObjectError::Limit("group rows exceed limit"));
        }

        let dur_raw = read_u64_col(&r, g, 0, m)?;
        let durations: Vec<i64> = dur_raw.into_iter().map(zigzag_decode).collect();
        let threads = read_u32_col(&r, g, 1, m)?;
        let rets_raw = read_u64_col(&r, g, 2, m)?;
        let rets: Vec<i64> = delta_by_key_decode_u64(&rets_raw, &threads)?
            .into_iter()
            .map(u64::cast_signed)
            .collect();
        let stacks_raw = read_u64_col(&r, g, 3, m)?;
        let stacks: Vec<u32> = delta_by_key_decode_u64(&stacks_raw, &threads)?
            .into_iter()
            .map(|v| u32_from_u64(v, "syscall stack"))
            .collect::<Result<Vec<_>, _>>()?;
        let cap_lens = read_u32_col(&r, g, 4, m)?;
        let outcomes = read_u8_col(&r, g, 5, m)?;
        let flags = read_u8_col(&r, g, 6, m)?;
        let unseen = if r.optional_column(g, 7).is_some() {
            Some(read_u8_col(&r, g, 7, m)?)
        } else {
            None
        };

        let mut args = Vec::with_capacity(6);
        for i in 0_u8..6 {
            if r.optional_column(g, 10 + i).is_some() {
                let raw = read_u64_col(&r, g, 10 + i, m)?;
                args.push(delta_by_key_decode_u64(&raw, &threads)?);
            } else {
                break;
            }
        }

        groups.push(DecodedGroup {
            abi: entry.abi,
            number: u32::from(entry.number.get()),
            durations,
            threads,
            rets,
            stacks,
            cap_lens,
            outcomes,
            flags,
            unseen,
            args,
            cursor: 0,
        });
    }

    let mut syscalls = Vec::with_capacity(n);
    for (row_idx, &gi_u64) in order.iter().enumerate() {
        let gi = usize::try_from(gi_u64)
            .map_err(|_| ObjectError::Layout("syscall order group overflow".into()))?;
        let group = groups
            .get_mut(gi)
            .ok_or_else(|| ObjectError::Layout("syscall order refers to missing group".into()))?;
        let pos = group.cursor;
        if pos >= group.threads.len() {
            return Err(ObjectError::Layout(
                "syscall order exceeds group row count".into(),
            ));
        }
        group.cursor += 1;

        let exit_u = exit_units.get(row_idx).copied().unwrap_or(0);
        let dur_u = group.durations.get(pos).copied().unwrap_or(0);
        let mask = group
            .unseen
            .as_ref()
            .and_then(|u| u.get(pos).copied())
            .unwrap_or(0);
        let (entered, exited) = match mask {
            0 => (
                r.time_unit.to_nanos(exit_u.wrapping_sub(dur_u)),
                r.time_unit.to_nanos(exit_u),
            ),
            1 => (UNSEEN, r.time_unit.to_nanos(exit_u)),
            2 => (r.time_unit.to_nanos(exit_u), UNSEEN),
            _ => (UNSEEN, UNSEEN),
        };

        let mut call_args = [zerocopy::little_endian::U64::new(0); 6];
        for (i, col) in group.args.iter().enumerate() {
            if let (Some(slot), Some(&val)) = (call_args.get_mut(i), col.get(pos)) {
                *slot = val.into();
            }
        }

        syscalls.push(Syscall {
            entered: entered.into(),
            exited: exited.into(),
            ret: group.rets.get(pos).copied().unwrap_or(0).into(),
            args: call_args,
            thread: group.threads.get(pos).copied().unwrap_or(NONE).into(),
            number: group.number.into(),
            stack: group.stacks.get(pos).copied().unwrap_or(NONE).into(),
            captures: NONE.into(),
            captures_len: group.cap_lens.get(pos).copied().unwrap_or(0).into(),
            abi: group.abi,
            outcome: group.outcomes.get(pos).copied().unwrap_or(0),
            flags: group.flags.get(pos).copied().unwrap_or(0),
            reserved: 0,
        });
    }

    for group in &groups {
        if group.cursor != group.threads.len() {
            return Err(ObjectError::Layout(
                "syscall group has unreferenced rows".into(),
            ));
        }
    }

    Ok(syscalls)
}

#[expect(
    clippy::too_many_lines,
    reason = "decodes grouped syscall capture blobs and note blobs"
)]
fn decode_syscall_blobs(bytes: &[u8], capture: &mut Capture) -> Result<(), ObjectError> {
    struct DecodedBlobGroup {
        headers: Vec<u64>,
        wanted: Vec<u32>,
        lengths: Vec<u32>,
        data: Vec<u8>,
        data_offset: usize,
        cursor: usize,
    }

    let r = SectionReader::open(bytes, SectionKind::SyscallBlobs)?;
    let total_captures = r.rows as usize;
    let call_counts = read_u32_col(&r, WHOLE_SECTION_GROUP, 0, capture.syscalls.len())?;
    let order = read_u64_col(&r, WHOLE_SECTION_GROUP, 1, total_captures)?;

    let mut groups: Vec<Option<DecodedBlobGroup>> = Vec::with_capacity(r.groups.len());
    let mut note_blobs: Option<(Vec<u32>, Vec<u8>)> = None;

    for (gi, entry) in r.groups.iter().enumerate() {
        let g = u16::try_from(gi).map_err(|_| ObjectError::Limit("group index overflow"))?;
        let m = entry.rows.get() as usize;
        if m > MAX_ROWS {
            return Err(ObjectError::Limit("blob group rows exceed limit"));
        }

        if entry.argument == NOTE_BLOBS_ARGUMENT {
            let lengths = read_u32_col(&r, g, 0, m)?;
            let data = r.column(g, 1)?.decompress()?;
            note_blobs = Some((lengths, data));
            groups.push(None);
            continue;
        }

        let headers = read_u64_col(&r, g, 0, m)?;
        let wanted = read_u32_col(&r, g, 1, m)?;
        let lengths = read_u32_col(&r, g, 2, m)?;
        let data_col = r.column(g, 3)?;
        let raw_data = data_col.decompress()?;
        let data = match data_col.coding {
            Coding::Words => {
                let stride = lengths.first().copied().unwrap_or(0) as usize;
                if lengths.iter().any(|&l| l as usize != stride) {
                    return Err(ObjectError::Layout(
                        "word-coded blob group has varying lengths".into(),
                    ));
                }
                decode_words(&raw_data, m, stride)?
            }
            Coding::Bytes => raw_data,
            _ => {
                return Err(ObjectError::Layout(
                    "unexpected coding for syscall blob bytes".into(),
                ));
            }
        };

        groups.push(Some(DecodedBlobGroup {
            headers,
            wanted,
            lengths,
            data,
            data_offset: 0,
            cursor: 0,
        }));
    }

    let mut blobs = Vec::new();
    let mut order_pos = 0_usize;

    for (call, &cap_count) in capture.syscalls.iter_mut().zip(&call_counts) {
        if cap_count == 0 {
            call.captures = NONE.into();
            call.captures_len = 0.into();
            continue;
        }

        let start = u32::try_from(blobs.len())
            .map_err(|_| ObjectError::Limit("syscall_blobs exceeds u32"))?;
        for _ in 0..cap_count {
            let &gi_u64 = order
                .get(order_pos)
                .ok_or_else(|| ObjectError::Layout("blob order too short".into()))?;
            order_pos += 1;
            let gi = usize::try_from(gi_u64)
                .map_err(|_| ObjectError::Layout("blob group index overflow".into()))?;
            let group = groups
                .get_mut(gi)
                .and_then(Option::as_mut)
                .ok_or_else(|| ObjectError::Layout("blob order refers to missing group".into()))?;
            let pos = group.cursor;
            if pos >= group.headers.len() {
                return Err(ObjectError::Layout(
                    "blob order exceeds group row count".into(),
                ));
            }
            group.cursor += 1;

            let hdr = group.headers.get(pos).copied().unwrap_or(0);
            let arg = (hdr & 0xff) as u8;
            let when = ((hdr >> 8) & 0xff) as u8;
            let state = ((hdr >> 16) & 0xff) as u8;
            let want = group.wanted.get(pos).copied().unwrap_or(0);
            let len = group.lengths.get(pos).copied().unwrap_or(0);
            let end = group
                .data_offset
                .checked_add(len as usize)
                .ok_or(ObjectError::Limit("blob data offset overflow"))?;
            let payload = group
                .data
                .get(group.data_offset..end)
                .ok_or(ObjectError::Truncated)?;
            group.data_offset = end;

            blobs.extend_from_slice(&[arg, when, state, 0]);
            blobs.extend_from_slice(&want.to_le_bytes());
            blobs.extend_from_slice(&len.to_le_bytes());
            blobs.extend_from_slice(payload);
        }

        let len = u32::try_from(blobs.len())
            .map_err(|_| ObjectError::Limit("syscall_blobs exceeds u32"))?
            .saturating_sub(start);
        call.captures = start.into();
        call.captures_len = len.into();
    }

    if order_pos != order.len() {
        return Err(ObjectError::Layout(
            "unconsumed entries in blob order".into(),
        ));
    }

    if let Some((lengths, data)) = note_blobs {
        let mut idx = 0_usize;
        let mut off = 0_usize;
        for note in &mut capture.syscall_notes {
            if note.blob_len.get() == 0 {
                note.blob = NONE.into();
                continue;
            }
            let &len = lengths
                .get(idx)
                .ok_or_else(|| ObjectError::Layout("missing note blob length".into()))?;
            idx += 1;
            let end = off
                .checked_add(len as usize)
                .ok_or(ObjectError::Limit("note blob offset overflow"))?;
            let slice = data.get(off..end).ok_or(ObjectError::Truncated)?;
            off = end;
            let start = u32::try_from(blobs.len())
                .map_err(|_| ObjectError::Limit("syscall_blobs exceeds u32"))?;
            blobs.extend_from_slice(slice);
            note.blob = start.into();
            note.blob_len = len.into();
        }
        if idx != lengths.len() || off != data.len() {
            return Err(ObjectError::Layout("unconsumed note blob data".into()));
        }
    }

    capture.syscall_blobs = blobs;
    Ok(())
}

fn decode_syscall_stats(bytes: &[u8]) -> Result<Vec<SyscallStat>, ObjectError> {
    let r = SectionReader::open(bytes, SectionKind::SyscallStats)?;
    let n = r.rows as usize;
    let calls = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 0, n)?;
    let errors = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 1, n)?;
    let timed = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 2, n)?;
    let total_ns = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 3, n)?;
    let min_ns = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 4, n)?;
    let max_ns = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 5, n)?;
    let bytes_col = read_delta_u64_col(&r, WHOLE_SECTION_GROUP, 6, n)?;
    let procs = read_u32_col(&r, WHOLE_SECTION_GROUP, 7, n)?;
    let threads = read_u32_col(&r, WHOLE_SECTION_GROUP, 8, n)?;
    let nums = read_u32_col(&r, WHOLE_SECTION_GROUP, 9, n)?;
    let ecounts = read_u32_col(&r, WHOLE_SECTION_GROUP, 10, n)?;
    let abis = read_u8_col(&r, WHOLE_SECTION_GROUP, 11, n)?;

    let mut buckets = Vec::with_capacity(crate::syscalls::stats::BUCKETS);
    for b in 0..crate::syscalls::stats::BUCKETS {
        let col_id = u8::try_from(12 + b).unwrap_or(u8::MAX);
        if r.optional_column(WHOLE_SECTION_GROUP, col_id).is_some() {
            buckets.push(Some(read_delta_u64_col(
                &r,
                WHOLE_SECTION_GROUP,
                col_id,
                n,
            )?));
        } else {
            buckets.push(None);
        }
    }

    let mut next_errno = 0_u32;
    Ok((0..n)
        .map(|i| {
            let mut log2_ns =
                [zerocopy::little_endian::U64::new(0); crate::syscalls::stats::BUCKETS];
            for (b, slot) in log2_ns.iter_mut().enumerate() {
                if let Some(Some(col)) = buckets.get(b) {
                    *slot = col.get(i).copied().unwrap_or(0).into();
                }
            }
            let ecount = ecounts.get(i).copied().unwrap_or(0);
            let errnos = next_errno;
            next_errno = next_errno.saturating_add(ecount);
            SyscallStat {
                calls: calls.get(i).copied().unwrap_or(0).into(),
                errors: errors.get(i).copied().unwrap_or(0).into(),
                timed: timed.get(i).copied().unwrap_or(0).into(),
                total_ns: total_ns.get(i).copied().unwrap_or(0).into(),
                min_ns: min_ns.get(i).copied().unwrap_or(0).into(),
                max_ns: max_ns.get(i).copied().unwrap_or(0).into(),
                bytes: bytes_col.get(i).copied().unwrap_or(0).into(),
                log2_ns,
                process: procs.get(i).copied().unwrap_or(NONE).into(),
                thread: threads.get(i).copied().unwrap_or(NONE).into(),
                number: nums.get(i).copied().unwrap_or(0).into(),
                errnos: errnos.into(),
                errno_count: ecount.into(),
                abi: abis.get(i).copied().unwrap_or(0),
                reserved: [0; 3],
            }
        })
        .collect())
}
