use std::collections::BTreeMap;

use super::super::codec::{
    Coding, DecodeBudget, SectionReader, WHOLE_SECTION_GROUP, decode_bp_tree,
    decode_sparse_histograms, decode_words, delta_by_key_decode_u64, delta_decode_i64,
    relative_decode_u64, zigzag_decode,
};
use super::super::{
    MAX_DECOMPRESSED_OBJECT_BYTES, MAX_TOTAL_ROWS, ObjectError, RawSection, SectionKind, TimeUnit,
};
use super::{
    ChunkData, ChunkRow, DimensionRow, NOTE_BLOBS_ARGUMENT, SanitizerCountRow, StringDecodeMap,
    read_delta_u64_col, read_relative_time_col, read_string_col, read_time_col,
    read_time_units_col, read_u8_col, read_u32_col, read_u64_col, u32_from_u64,
};
use crate::capture::Capture;
use crate::capture::records::{
    Account, Frame, FrameKind, Function, Gap, Line, Mapping, Marker, Module, NONE, OffCpu, Plot,
    Process, Range, Sample, Setting, Site, Stack, Syscall, SyscallErrno, SyscallNote, SyscallStat,
    Task, TaskWait, Thread, ThreadRange, UNSEEN, Zone, ZoneRange,
};
use crate::image::{StrId, Strings};

fn charge_rows<T>(r: &SectionReader<'_, '_>, rows: usize) -> Result<(), ObjectError> {
    r.charge(rows.saturating_mul(size_of::<T>()))
}

/// Shared Tier A symbol and stack tables supplied to [`decode_event_bundle`].
#[derive(Debug, Clone, Copy)]
pub struct SharedSymbols<'a> {
    pub strings: &'a [u8],
    pub modules: &'a [Module],
    pub processes: &'a [Process],
    pub frames: &'a [Frame],
    pub frame_symbols: &'a [Range],
    pub lines: &'a [Line],
    pub functions: &'a [Function],
    pub stacks: &'a [Stack],
    pub sites: &'a [Site],
}

pub fn decode_tables(sections: &[RawSection<'_>]) -> Result<ChunkData, ObjectError> {
    decode_selected(sections, None, MAX_DECOMPRESSED_OBJECT_BYTES, |_| true)
}

pub fn decode_tables_budgeted(
    sections: &[RawSection<'_>],
    max_bytes: usize,
) -> Result<ChunkData, ObjectError> {
    decode_selected(sections, None, max_bytes, |_| true)
}

pub fn decode_flame_tables(sections: &[RawSection<'_>]) -> Result<ChunkData, ObjectError> {
    decode_selected(
        sections,
        None,
        MAX_DECOMPRESSED_OBJECT_BYTES,
        SectionKind::needed_for_flame,
    )
}

/// Decodes a per-machine Tier D event bundle referencing a `DetailPart`'s shared Tier A symbols.
pub fn decode_event_bundle(
    sections: &[RawSection<'_>],
    shared: &SharedSymbols<'_>,
    flame_only: bool,
) -> Result<ChunkData, ObjectError> {
    if flame_only {
        decode_selected(
            sections,
            Some(shared),
            MAX_DECOMPRESSED_OBJECT_BYTES,
            SectionKind::needed_for_flame,
        )
    } else {
        decode_selected(
            sections,
            Some(shared),
            MAX_DECOMPRESSED_OBJECT_BYTES,
            |_| true,
        )
    }
}

fn build_reset_bitmap(ranges: &[ThreadRange], total_rows: usize) -> Result<Vec<bool>, ObjectError> {
    let mut resets = vec![false; total_rows];
    let mut running = 0_usize;
    for r in ranges {
        let start = r.start.get() as usize;
        let count = r.count.get() as usize;
        if start != running || start.saturating_add(count) > total_rows {
            return Err(ObjectError::Layout(
                "thread ranges do not tile event table".into(),
            ));
        }
        if count > 0
            && let Some(slot) = resets.get_mut(start)
        {
            *slot = true;
        }
        running += count;
    }
    if running != total_rows {
        return Err(ObjectError::Layout(
            "thread ranges do not cover event table".into(),
        ));
    }
    Ok(resets)
}

#[expect(
    clippy::too_many_lines,
    reason = "decodes each chunk table section into Capture and metadata"
)]
fn decode_selected(
    sections: &[RawSection<'_>],
    shared: Option<&SharedSymbols<'_>>,
    max_bytes: usize,
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

    let budget = DecodeBudget::new(max_bytes);
    let mut capture = Capture::default();
    let mut chunks = Vec::new();
    let mut dimensions = Vec::new();
    let mut sanitizer_counts = Vec::new();

    if let Some(sym) = shared {
        capture.strings.extend_from_slice(sym.strings);
        capture.modules.extend_from_slice(sym.modules);
        capture.processes.extend_from_slice(sym.processes);
        capture.frames.extend_from_slice(sym.frames);
        capture.frame_symbols.extend_from_slice(sym.frame_symbols);
        capture.lines.extend_from_slice(sym.lines);
        capture.functions.extend_from_slice(sym.functions);
        capture.stacks.extend_from_slice(sym.stacks);
        capture.sites.extend_from_slice(sym.sites);
    } else if let Some(sec) = by_kind.get(&SectionKind::Strings) {
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Strings, &budget)?;
        let p = r.read_page(0)?;
        capture
            .strings
            .clone_from(&p.column(WHOLE_SECTION_GROUP, 0)?.data);
    }

    if !crate::image::valid_pool(&capture.strings, true) {
        return Err(ObjectError::Capture(
            "string pool is not NUL-terminated UTF-8".into(),
        ));
    }
    let str_map = StringDecodeMap::from_pool(&capture.strings)?;

    if let Some(sec) = by_kind.get(&SectionKind::Settings) {
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Settings, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Setting>(&r, n)?;
        let p = r.read_page(0)?;
        let keys = read_string_col(&p, WHOLE_SECTION_GROUP, 0, n, &str_map)?;
        let vals = read_string_col(&p, WHOLE_SECTION_GROUP, 1, n, &str_map)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Modules, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Module>(&r, n)?;
        let p = r.read_page(0)?;
        let paths = read_string_col(&p, WHOLE_SECTION_GROUP, 0, n, &str_map)?;
        let bids = read_string_col(&p, WHOLE_SECTION_GROUP, 1, n, &str_map)?;
        let digests = &p.column(WHOLE_SECTION_GROUP, 2)?.data;
        if digests.len() != n.saturating_mul(16) {
            return Err(ObjectError::Layout(
                "module digest column length mismatch".into(),
            ));
        }
        let sizes = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 3, n)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Processes, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Process>(&r, n)?;
        let p = r.read_page(0)?;
        let pids = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let names = read_string_col(&p, WHOLE_SECTION_GROUP, 1, n, &str_map)?;
        let start_units = read_time_units_col(&p, r.base_time, WHOLE_SECTION_GROUP, 2, n)?;
        let ends = read_relative_time_col(&p, r.time_unit, WHOLE_SECTION_GROUP, 3, &start_units)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Threads, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Thread>(&r, n)?;
        let p = r.read_page(0)?;
        let procs = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let tids = read_u32_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let names = read_string_col(&p, WHOLE_SECTION_GROUP, 2, n, &str_map)?;
        let start_units = read_time_units_col(&p, r.base_time, WHOLE_SECTION_GROUP, 3, n)?;
        let ends = read_relative_time_col(&p, r.time_unit, WHOLE_SECTION_GROUP, 4, &start_units)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Mappings, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Mapping>(&r, n)?;
        let p = r.read_page(0)?;
        let procs = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let mods = read_u32_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let starts = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
        let ends_raw = read_u64_col(&p, WHOLE_SECTION_GROUP, 3, n)?;
        charge_rows::<u64>(&r, n)?;
        let ends = relative_decode_u64(&ends_raw, &starts)?;
        let offs = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 4, n)?;
        let biases = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 5, n)?;
        let times = read_time_col(&p, r.time_unit, r.base_time, WHOLE_SECTION_GROUP, 6, n)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Frames, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Frame>(&r, n)?;
        let p = r.read_page(0)?;
        let raw_addrs = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let mods = read_u32_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let kinds = read_u8_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
        let roles = read_u8_col(&p, WHOLE_SECTION_GROUP, 3, n)?;
        let mut frames = Vec::with_capacity(n);
        for i in 0..n {
            let kind = kinds.get(i).copied().unwrap_or(0);
            let raw_addr = raw_addrs.get(i).copied().unwrap_or(0);
            let address = if FrameKind::from_number(kind).is_some_and(FrameKind::named) {
                u64::from(str_map.decode_ref(raw_addr)?)
            } else {
                raw_addr
            };
            frames.push(Frame {
                address: address.into(),
                module: mods.get(i).copied().unwrap_or(NONE).into(),
                kind,
                role: roles.get(i).copied().unwrap_or(0),
                reserved: 0.into(),
            });
        }
        capture.frames = frames;
    }

    if let Some(sec) = by_kind.get(&SectionKind::Stacks) {
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Stacks, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Stack>(&r, n)?;
        let p = r.read_page(0)?;
        let col0 = p.column(WHOLE_SECTION_GROUP, 0)?;
        if col0.coding != Coding::BpTree {
            return Err(ObjectError::Layout(
                "stacks topology column must use BpTree coding".into(),
            ));
        }
        let parents = decode_bp_tree(&col0.data, n)?;
        let frames = read_u32_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        capture.stacks = parents
            .into_iter()
            .zip(frames)
            .map(|(p, f)| Stack {
                parent: p.into(),
                frame: f.into(),
            })
            .collect();
    }

    // Decode SampleRanges before Samples so per-thread time delta resets can be reconstructed.
    if let Some(sec) = by_kind.get(&SectionKind::SampleRanges) {
        capture.sample_ranges =
            decode_thread_ranges(sec.bytes, SectionKind::SampleRanges, &budget)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::Samples) {
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Samples, &budget)?;
        let total = r.rows as usize;
        charge_rows::<Sample>(&r, total)?;
        let keep_tasks = include(SectionKind::Tasks);
        let mut samples = Vec::with_capacity(total);
        let mut resets_opt: Option<Vec<bool>> = None;
        let mut global_offset = 0_usize;

        for page_idx in 0..r.page_count() {
            let p = r.read_page(page_idx)?;
            let before_cols = budget.remaining();
            let n = p.rows as usize;
            let use_reset = p.optional_column(WHOLE_SECTION_GROUP, 4).is_some();
            if use_reset && resets_opt.is_none() {
                resets_opt = Some(build_reset_bitmap(&capture.sample_ranges, total)?);
            }
            let time_deltas = read_u64_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
            let weights = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
            let stacks = read_u32_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
            let tasks = read_u32_col(&p, WHOLE_SECTION_GROUP, 3, n)?;

            let mut prev = r.base_time;
            for i in 0..n {
                let global_idx = global_offset + i;
                let is_reset = i == 0
                    || resets_opt
                        .as_ref()
                        .and_then(|bm| bm.get(global_idx).copied())
                        .unwrap_or(false);
                let anchor = if is_reset { r.base_time } else { prev };
                let t =
                    anchor.wrapping_add(zigzag_decode(time_deltas.get(i).copied().unwrap_or(0)));
                prev = t;
                samples.push(Sample {
                    time: r.time_unit.to_nanos(t).into(),
                    weight: weights.get(i).copied().unwrap_or(0).into(),
                    stack: stacks.get(i).copied().unwrap_or(NONE).into(),
                    task: if keep_tasks {
                        tasks.get(i).copied().unwrap_or(NONE)
                    } else {
                        NONE
                    }
                    .into(),
                });
            }
            global_offset += n;
            budget.release(before_cols.saturating_sub(budget.remaining()));
        }
        capture.samples = samples;
    }

    if let Some(sec) = by_kind.get(&SectionKind::FrameSymbols) {
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::FrameSymbols, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Range>(&r, n)?;
        let p = r.read_page(0)?;
        let counts = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let explicit_starts = if p.optional_column(WHOLE_SECTION_GROUP, 1).is_some() {
            Some(read_u32_col(&p, WHOLE_SECTION_GROUP, 1, n)?)
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Lines, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Line>(&r, n)?;
        let p = r.read_page(0)?;
        let funcs = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let files = read_string_col(&p, WHOLE_SECTION_GROUP, 1, n, &str_map)?;
        let nums = read_u32_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
        let cols = read_u32_col(&p, WHOLE_SECTION_GROUP, 3, n)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Functions, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Function>(&r, n)?;
        let p = r.read_page(0)?;
        let names = read_string_col(&p, WHOLE_SECTION_GROUP, 0, n, &str_map)?;
        let mods = read_u32_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let files = read_string_col(&p, WHOLE_SECTION_GROUP, 2, n, &str_map)?;
        let lines = read_u32_col(&p, WHOLE_SECTION_GROUP, 3, n)?;
        let roles = read_u8_col(&p, WHOLE_SECTION_GROUP, 4, n)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Accounting, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Account>(&r, n)?;
        let p = r.read_page(0)?;
        let kinds = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let vals = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
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

    // Decode OffCpuRanges before OffCpu so per-thread resets can be reconstructed.
    if let Some(sec) = by_kind.get(&SectionKind::OffCpuRanges) {
        capture.off_cpu_ranges =
            decode_thread_ranges(sec.bytes, SectionKind::OffCpuRanges, &budget)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::OffCpu) {
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::OffCpu, &budget)?;
        let total = r.rows as usize;
        charge_rows::<OffCpu>(&r, total)?;
        let mut off_cpu = Vec::with_capacity(total);
        let mut resets_opt: Option<Vec<bool>> = None;
        let mut global_offset = 0_usize;

        for page_idx in 0..r.page_count() {
            let p = r.read_page(page_idx)?;
            let before_cols = budget.remaining();
            let n = p.rows as usize;
            let use_reset = p.optional_column(WHOLE_SECTION_GROUP, 3).is_some();
            if use_reset && resets_opt.is_none() {
                resets_opt = Some(build_reset_bitmap(&capture.off_cpu_ranges, total)?);
            }
            let gaps = read_u64_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
            let durations = read_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
            let packed = read_u8_col(&p, WHOLE_SECTION_GROUP, 2, n)?;

            let mut prev_end = r.base_time;
            for i in 0..n {
                let global_idx = global_offset + i;
                let is_reset = i == 0
                    || resets_opt
                        .as_ref()
                        .and_then(|bm| bm.get(global_idx).copied())
                        .unwrap_or(false);
                let anchor = if is_reset { r.base_time } else { prev_end };
                let gap = zigzag_decode(gaps.get(i).copied().unwrap_or(0));
                let dur = durations.get(i).copied().unwrap_or(0);
                let start = anchor.wrapping_add(gap);
                prev_end = start.saturating_add_unsigned(dur);
                let byte = packed.get(i).copied().unwrap_or(0);
                off_cpu.push(OffCpu {
                    time: r.time_unit.to_nanos(start).into(),
                    duration: dur
                        .saturating_mul(r.time_unit.nanos().cast_unsigned())
                        .into(),
                    preempted: byte & 1,
                    flags: byte >> 1,
                });
            }
            global_offset += n;
            budget.release(before_cols.saturating_sub(budget.remaining()));
        }
        capture.off_cpu = off_cpu;
    }

    if let Some(sec) = by_kind.get(&SectionKind::Sites) {
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Sites, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Site>(&r, n)?;
        let p = r.read_page(0)?;
        let names = read_string_col(&p, WHOLE_SECTION_GROUP, 0, n, &str_map)?;
        let files = read_string_col(&p, WHOLE_SECTION_GROUP, 1, n, &str_map)?;
        let lines = read_u32_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
        let mods = read_u32_col(&p, WHOLE_SECTION_GROUP, 3, n)?;
        let kinds = read_u8_col(&p, WHOLE_SECTION_GROUP, 4, n)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Zones, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Zone>(&r, n)?;
        let p = r.read_page(0)?;
        let start_units = read_time_units_col(&p, r.base_time, WHOLE_SECTION_GROUP, 0, n)?;
        let ends = read_relative_time_col(&p, r.time_unit, WHOLE_SECTION_GROUP, 1, &start_units)?;
        let sites = read_u32_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
        let flags = read_u32_col(&p, WHOLE_SECTION_GROUP, 3, n)?;
        let vals = read_string_col(&p, WHOLE_SECTION_GROUP, 4, n, &str_map)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::ZoneRanges, &budget)?;
        let n = r.rows as usize;
        charge_rows::<ZoneRange>(&r, n)?;
        let p = r.read_page(0)?;
        let threads = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let depths = read_u32_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let counts = read_u32_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Plots, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Plot>(&r, n)?;
        let p = r.read_page(0)?;
        let times = read_time_col(&p, r.time_unit, r.base_time, WHOLE_SECTION_GROUP, 0, n)?;
        let vals = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let sites = read_u32_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
        let threads = read_u32_col(&p, WHOLE_SECTION_GROUP, 3, n)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Markers, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Marker>(&r, n)?;
        let p = r.read_page(0)?;
        let times = read_time_col(&p, r.time_unit, r.base_time, WHOLE_SECTION_GROUP, 0, n)?;
        let threads = read_u32_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let sites = read_u32_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
        let texts = read_string_col(&p, WHOLE_SECTION_GROUP, 3, n, &str_map)?;
        let kinds = read_u8_col(&p, WHOLE_SECTION_GROUP, 4, n)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Tasks, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Task>(&r, n)?;
        let p = r.read_page(0)?;
        let runtimes = read_string_col(&p, WHOLE_SECTION_GROUP, 0, n, &str_map)?;
        let numbers = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let roots = read_u32_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
        let labels = read_string_col(&p, WHOLE_SECTION_GROUP, 3, n, &str_map)?;
        let first_units = read_time_units_col(&p, r.base_time, WHOLE_SECTION_GROUP, 4, n)?;
        let last = read_relative_time_col(&p, r.time_unit, WHOLE_SECTION_GROUP, 5, &first_units)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::TaskWaits, &budget)?;
        let n = r.rows as usize;
        charge_rows::<TaskWait>(&r, n)?;
        let p = r.read_page(0)?;
        let times = read_time_col(&p, r.time_unit, r.base_time, WHOLE_SECTION_GROUP, 0, n)?;
        let weights = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let tasks = read_u32_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
        let stacks = read_u32_col(&p, WHOLE_SECTION_GROUP, 3, n)?;
        let states = read_string_col(&p, WHOLE_SECTION_GROUP, 4, n, &str_map)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Gaps, &budget)?;
        let n = r.rows as usize;
        charge_rows::<Gap>(&r, n)?;
        let p = r.read_page(0)?;
        let start_units = read_time_units_col(&p, r.base_time, WHOLE_SECTION_GROUP, 0, n)?;
        let ends = read_relative_time_col(&p, r.time_unit, WHOLE_SECTION_GROUP, 1, &start_units)?;
        let reasons = read_u8_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
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
        capture.syscalls = decode_syscalls(sec.bytes, &budget)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::SyscallNotes) {
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::SyscallNotes, &budget)?;
        let n = r.rows as usize;
        charge_rows::<SyscallNote>(&r, n)?;
        let p = r.read_page(0)?;
        let times = read_time_col(&p, r.time_unit, r.base_time, WHOLE_SECTION_GROUP, 0, n)?;
        let values = read_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let extras = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
        let threads = read_u32_col(&p, WHOLE_SECTION_GROUP, 3, n)?;
        let procs = read_u32_col(&p, WHOLE_SECTION_GROUP, 4, n)?;
        let texts = read_string_col(&p, WHOLE_SECTION_GROUP, 5, n, &str_map)?;
        let stacks = read_u32_col(&p, WHOLE_SECTION_GROUP, 6, n)?;
        let blens = read_u32_col(&p, WHOLE_SECTION_GROUP, 7, n)?;
        let calls = read_u32_col(&p, WHOLE_SECTION_GROUP, 8, n)?;
        let kinds = read_u8_col(&p, WHOLE_SECTION_GROUP, 9, n)?;
        let flags = read_u8_col(&p, WHOLE_SECTION_GROUP, 10, n)?;
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
        decode_syscall_blobs(sec.bytes, &mut capture, &budget)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::SyscallStats) {
        capture.syscall_stats = decode_syscall_stats(sec.bytes, &budget)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::SyscallErrnos) {
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::SyscallErrnos, &budget)?;
        let n = r.rows as usize;
        charge_rows::<SyscallErrno>(&r, n)?;
        let p = r.read_page(0)?;
        let counts = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let errnos = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
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
        let text = Strings(&capture.strings).get(StrId(id));
        budget.take(text.len())?;
        Ok(text.to_owned())
    };

    if let Some(sec) = by_kind.get(&SectionKind::Chunk) {
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Chunk, &budget)?;
        let n = r.rows as usize;
        charge_rows::<ChunkRow>(&r, n)?;
        let p = r.read_page(0)?;
        let agents = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let numbers = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let start_units = read_time_units_col(&p, r.base_time, WHOLE_SECTION_GROUP, 2, n)?;
        let ends = read_relative_time_col(&p, r.time_unit, WHOLE_SECTION_GROUP, 3, &start_units)?;
        let sampled = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 4, n)?;
        let presets = read_u8_col(&p, WHOLE_SECTION_GROUP, 5, n)?;
        let units = read_u8_col(&p, WHOLE_SECTION_GROUP, 6, n)?;
        let policies = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 7, n)?;
        let rules = read_string_col(&p, WHOLE_SECTION_GROUP, 8, n, &str_map)?;
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
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::Dimensions, &budget)?;
        let n = r.rows as usize;
        charge_rows::<DimensionRow>(&r, n)?;
        let p = r.read_page(0)?;
        let procs = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let names = read_string_col(&p, WHOLE_SECTION_GROUP, 1, n, &str_map)?;
        let vals = read_string_col(&p, WHOLE_SECTION_GROUP, 2, n, &str_map)?;
        for i in 0..n {
            dimensions.push(DimensionRow {
                process: procs.get(i).copied().unwrap_or(NONE),
                name: lookup_string(names.get(i).copied().unwrap_or(NONE))?,
                value: lookup_string(vals.get(i).copied().unwrap_or(NONE))?,
            });
        }
    }

    if let Some(sec) = by_kind.get(&SectionKind::SanitizerCounts) {
        let r = SectionReader::open_budgeted(sec.bytes, SectionKind::SanitizerCounts, &budget)?;
        let n = r.rows as usize;
        charge_rows::<SanitizerCountRow>(&r, n)?;
        let p = r.read_page(0)?;
        let rules = read_string_col(&p, WHOLE_SECTION_GROUP, 0, n, &str_map)?;
        let fields = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let fired = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
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

fn decode_thread_ranges(
    bytes: &[u8],
    kind: SectionKind,
    budget: &DecodeBudget,
) -> Result<Vec<ThreadRange>, ObjectError> {
    let r = SectionReader::open_budgeted(bytes, kind, budget)?;
    let n = r.rows as usize;
    charge_rows::<ThreadRange>(&r, n)?;
    let p = r.read_page(0)?;
    let threads = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
    let counts = read_u32_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
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
    reason = "decodes paged grouped syscall columns per abi and number"
)]
fn decode_syscalls(bytes: &[u8], budget: &DecodeBudget) -> Result<Vec<Syscall>, ObjectError> {
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

    let reader = SectionReader::open_budgeted(bytes, SectionKind::Syscalls, budget)?;
    let total = reader.rows as usize;
    charge_rows::<Syscall>(&reader, total)?;
    let mut syscalls = Vec::with_capacity(total);

    for page_idx in 0..reader.page_count() {
        let page = reader.read_page(page_idx)?;
        let before_cols = budget.remaining();
        let row_count = page.rows as usize;
        let order = read_u64_col(&page, WHOLE_SECTION_GROUP, 0, row_count)?;
        let exit_deltas = read_u64_col(&page, WHOLE_SECTION_GROUP, 1, row_count)?;
        let exit_units = delta_decode_i64(&exit_deltas, reader.base_time);

        let mut groups = Vec::with_capacity(page.groups.len());
        for (gi, entry) in page.groups.iter().enumerate() {
            let group_id =
                u16::try_from(gi).map_err(|_| ObjectError::Limit("group index overflow"))?;
            let group_rows = entry.rows.get() as usize;
            if group_rows > MAX_TOTAL_ROWS {
                return Err(ObjectError::Limit("group rows exceed limit"));
            }

            let dur_raw = read_u64_col(&page, group_id, 0, group_rows)?;
            charge_rows::<i64>(&reader, group_rows)?;
            let durations: Vec<i64> = dur_raw.into_iter().map(zigzag_decode).collect();
            let threads = read_u32_col(&page, group_id, 1, group_rows)?;
            let rets_raw = read_u64_col(&page, group_id, 2, group_rows)?;
            charge_rows::<i64>(&reader, group_rows)?;
            let rets: Vec<i64> = delta_by_key_decode_u64(&rets_raw, &threads)?
                .into_iter()
                .map(u64::cast_signed)
                .collect();
            let stacks_raw = read_u64_col(&page, group_id, 3, group_rows)?;
            charge_rows::<u32>(&reader, group_rows)?;
            let stacks: Vec<u32> = delta_by_key_decode_u64(&stacks_raw, &threads)?
                .into_iter()
                .map(|v| u32_from_u64(v, "syscall stack"))
                .collect::<Result<Vec<_>, _>>()?;
            let cap_lens = read_u32_col(&page, group_id, 4, group_rows)?;
            let outcomes = read_u8_col(&page, group_id, 5, group_rows)?;
            let flags = read_u8_col(&page, group_id, 6, group_rows)?;
            let unseen = if page.optional_column(group_id, 7).is_some() {
                Some(read_u8_col(&page, group_id, 7, group_rows)?)
            } else {
                None
            };

            let mut args = Vec::with_capacity(6);
            for arg_idx in 0_u8..6 {
                if page.optional_column(group_id, 10 + arg_idx).is_some() {
                    let raw = read_u64_col(&page, group_id, 10 + arg_idx, group_rows)?;
                    charge_rows::<u64>(&reader, group_rows)?;
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

        for (row_idx, &gi_u64) in order.iter().enumerate() {
            let gi = usize::try_from(gi_u64)
                .map_err(|_| ObjectError::Layout("syscall order group overflow".into()))?;
            let group = groups.get_mut(gi).ok_or_else(|| {
                ObjectError::Layout("syscall order refers to missing group".into())
            })?;
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
                    reader.time_unit.to_nanos(exit_u.wrapping_sub(dur_u)),
                    reader.time_unit.to_nanos(exit_u),
                ),
                1 => (UNSEEN, reader.time_unit.to_nanos(exit_u)),
                2 => (reader.time_unit.to_nanos(exit_u), UNSEEN),
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
        budget.release(before_cols.saturating_sub(budget.remaining()));
    }

    Ok(syscalls)
}

#[expect(
    clippy::too_many_lines,
    reason = "decodes grouped syscall capture blobs and note blobs"
)]
fn decode_syscall_blobs(
    bytes: &[u8],
    capture: &mut Capture,
    budget: &DecodeBudget,
) -> Result<(), ObjectError> {
    struct DecodedBlobGroup {
        headers: Vec<u64>,
        wanted: Vec<u32>,
        lengths: Vec<u32>,
        data: Vec<u8>,
        data_offset: usize,
        cursor: usize,
    }

    let r = SectionReader::open_budgeted(bytes, SectionKind::SyscallBlobs, budget)?;
    let total_captures = r.rows as usize;
    r.charge(total_captures.saturating_mul(12))?;
    let p = r.read_page(0)?;
    let call_counts = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, capture.syscalls.len())?;
    let order = read_u64_col(&p, WHOLE_SECTION_GROUP, 1, total_captures)?;

    let mut groups: Vec<Option<DecodedBlobGroup>> = Vec::with_capacity(p.groups.len());
    let mut note_blobs: Option<(Vec<u32>, Vec<u8>)> = None;

    for (gi, entry) in p.groups.iter().enumerate() {
        let g = u16::try_from(gi).map_err(|_| ObjectError::Limit("group index overflow"))?;
        let m = entry.rows.get() as usize;
        if m > MAX_TOTAL_ROWS {
            return Err(ObjectError::Limit("blob group rows exceed limit"));
        }

        if entry.argument == NOTE_BLOBS_ARGUMENT {
            let lengths = read_u32_col(&p, g, 0, m)?;
            let data = p.column(g, 1)?.data.clone();
            note_blobs = Some((lengths, data));
            groups.push(None);
            continue;
        }

        let headers = read_u64_col(&p, g, 0, m)?;
        let wanted = read_u32_col(&p, g, 1, m)?;
        let lengths = read_u32_col(&p, g, 2, m)?;
        let data_col = p.column(g, 3)?;
        let data = match data_col.coding {
            Coding::Words => {
                let stride = lengths.first().copied().unwrap_or(0) as usize;
                if lengths.iter().any(|&l| l as usize != stride) {
                    return Err(ObjectError::Layout(
                        "word-coded blob group has varying lengths".into(),
                    ));
                }
                r.charge(m.saturating_mul(stride))?;
                decode_words(&data_col.data, m, stride)?
            }
            Coding::Bytes => data_col.data.clone(),
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

            r.charge(payload.len())?;
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
            r.charge(slice.len())?;
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

fn decode_syscall_stats(
    bytes: &[u8],
    budget: &DecodeBudget,
) -> Result<Vec<SyscallStat>, ObjectError> {
    let r = SectionReader::open_budgeted(bytes, SectionKind::SyscallStats, budget)?;
    let n = r.rows as usize;
    charge_rows::<SyscallStat>(&r, n)?;
    let p = r.read_page(0)?;
    let calls = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
    let errors = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
    let timed = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
    let total_ns = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 3, n)?;
    let min_ns = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 4, n)?;
    let max_ns = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 5, n)?;
    let bytes_col = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 6, n)?;
    let procs = read_u32_col(&p, WHOLE_SECTION_GROUP, 7, n)?;
    let threads = read_u32_col(&p, WHOLE_SECTION_GROUP, 8, n)?;
    let nums = read_u32_col(&p, WHOLE_SECTION_GROUP, 9, n)?;
    let ecounts = read_u32_col(&p, WHOLE_SECTION_GROUP, 10, n)?;
    let abis = read_u8_col(&p, WHOLE_SECTION_GROUP, 11, n)?;

    let hist_col = p.column(WHOLE_SECTION_GROUP, 12)?;
    if hist_col.coding != Coding::SparseHistogram40 {
        return Err(ObjectError::Layout(
            "syscall_stats histogram column must use SparseHistogram40 coding".into(),
        ));
    }
    let hists = decode_sparse_histograms(&hist_col.data, n, Some(budget))?;

    let mut next_errno = 0_u32;
    Ok((0..n)
        .map(|i| {
            let mut log2_ns =
                [zerocopy::little_endian::U64::new(0); crate::syscalls::stats::BUCKETS];
            if let Some(h) = hists.get(i) {
                for (slot, &val) in log2_ns.iter_mut().zip(h) {
                    *slot = val.into();
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
