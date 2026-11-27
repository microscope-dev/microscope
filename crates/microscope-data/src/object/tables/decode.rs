use std::collections::BTreeMap;

use super::super::codec::{
    Coding, DecodeBudget, SectionReader, WHOLE_SECTION_GROUP, decode_bp_tree_budgeted,
    relative_decode_u64, zigzag_decode,
};
use super::super::{MAX_DECOMPRESSED_OBJECT_BYTES, ObjectError, RawSection, SectionKind, TimeUnit};
use super::decode_syscalls::{decode_syscall_blobs, decode_syscall_stats, decode_syscalls};
use super::{
    ChunkData, ChunkRow, DimensionRow, SanitizerCountRow, StringDecodeMap, TABLE_VERSION,
    read_delta_u64_col, read_relative_time_col, read_string_col, read_time_col,
    read_time_units_col, read_u8_col, read_u32_col, read_u64_col,
};
use crate::capture::Capture;
use crate::capture::records::{
    Account, Frame, FrameKind, Function, Gap, Line, Mapping, Marker, Module, NONE, OffCpu, Plot,
    Process, Range, Sample, Setting, Site, Stack, SyscallErrno, SyscallNote, Task, TaskWait,
    Thread, ThreadRange, Zone, ZoneRange,
};
use crate::image::{StrId, Strings};

pub(super) fn charge_rows<T>(r: &SectionReader<'_, '_>, rows: usize) -> Result<(), ObjectError> {
    r.charge(rows.saturating_mul(size_of::<T>()))
}

pub(super) fn open_table_section<'a, 'b>(
    bytes: &'a [u8],
    kind: SectionKind,
    budget: &'b DecodeBudget,
) -> Result<SectionReader<'a, 'b>, ObjectError> {
    let reader = SectionReader::open_budgeted(bytes, kind, budget)?;
    if reader.version != TABLE_VERSION {
        return Err(ObjectError::UnknownValue {
            what: "table section version",
            value: u64::from(reader.version),
        });
    }
    Ok(reader)
}

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
    let budget = DecodeBudget::new(MAX_DECOMPRESSED_OBJECT_BYTES);
    decode_selected(sections, None, &budget, |_| true)
}

pub fn decode_tables_budgeted(
    sections: &[RawSection<'_>],
    max_bytes: usize,
) -> Result<ChunkData, ObjectError> {
    let budget = DecodeBudget::new(max_bytes);
    decode_selected(sections, None, &budget, |_| true)
}

pub fn decode_flame_tables(sections: &[RawSection<'_>]) -> Result<ChunkData, ObjectError> {
    let budget = DecodeBudget::new(MAX_DECOMPRESSED_OBJECT_BYTES);
    decode_selected(sections, None, &budget, SectionKind::needed_for_flame)
}

pub fn decode_flame_tables_budgeted(
    sections: &[RawSection<'_>],
    max_bytes: usize,
) -> Result<ChunkData, ObjectError> {
    let budget = DecodeBudget::new(max_bytes);
    decode_selected(sections, None, &budget, SectionKind::needed_for_flame)
}

#[derive(Debug, Default, Clone)]
pub struct FlameBundle {
    pub weight_unit: Option<String>,
    pub end_setting: Option<i64>,
    pub samples: Vec<Sample>,
}

pub fn decode_flame_bundle(
    sections: &[RawSection<'_>],
    shared: &SharedSymbols<'_>,
    budget: &DecodeBudget,
) -> Result<FlameBundle, ObjectError> {
    let mut by_kind: BTreeMap<SectionKind, RawSection<'_>> = BTreeMap::new();
    for &sec in sections {
        if !matches!(
            sec.kind,
            SectionKind::Settings | SectionKind::SampleRanges | SectionKind::Samples
        ) {
            continue;
        }
        if by_kind.insert(sec.kind, sec).is_some() {
            return Err(ObjectError::Layout(format!(
                "duplicate section kind {:?}",
                sec.kind
            )));
        }
    }

    let mut weight_unit = None;
    let mut end_setting = None;
    if let Some(sec) = by_kind.get(&SectionKind::Settings) {
        if !crate::image::valid_pool(shared.strings, true) {
            return Err(ObjectError::Capture(
                "string pool is not NUL-terminated UTF-8".into(),
            ));
        }
        let str_map = StringDecodeMap::from_pool(shared.strings)?;
        let r = open_table_section(sec.bytes, SectionKind::Settings, budget)?;
        let n = r.rows as usize;
        let p = r.read_single_page()?;
        let before = budget.remaining();
        let keys = read_string_col(&p, WHOLE_SECTION_GROUP, 0, n, &str_map)?;
        let vals = read_string_col(&p, WHOLE_SECTION_GROUP, 1, n, &str_map)?;
        let pool = Strings(shared.strings);
        for (k, v) in keys.into_iter().zip(vals) {
            if k != NONE
                && v != NONE
                && crate::image::valid_reference(shared.strings, k, true)
                && crate::image::valid_reference(shared.strings, v, true)
            {
                match pool.get(StrId(k)) {
                    "weight" => weight_unit = Some(pool.get(StrId(v)).to_owned()),
                    "end" => {
                        if let Ok(parsed) = pool.get(StrId(v)).parse::<i64>() {
                            end_setting = Some(parsed);
                        }
                    }
                    _ => {}
                }
            }
        }
        budget.release(before.saturating_sub(budget.remaining()));
    }

    let sample_ranges = if let Some(sec) = by_kind.get(&SectionKind::SampleRanges) {
        decode_thread_ranges(sec.bytes, SectionKind::SampleRanges, budget)?
    } else {
        Vec::new()
    };

    let samples = if let Some(sec) = by_kind.get(&SectionKind::Samples) {
        let decoded = decode_samples_section(sec.bytes, &sample_ranges, false, budget)?;
        for s in &decoded {
            let stack = s.stack.get();
            if stack != NONE && (stack as usize) >= shared.stacks.len() {
                return Err(ObjectError::Capture("sample stack out of bounds".into()));
            }
        }
        decoded
    } else {
        Vec::new()
    };

    Ok(FlameBundle {
        weight_unit,
        end_setting,
        samples,
    })
}

pub fn decode_event_bundle(
    sections: &[RawSection<'_>],
    shared: &SharedSymbols<'_>,
    flame_only: bool,
) -> Result<ChunkData, ObjectError> {
    let budget = DecodeBudget::new(MAX_DECOMPRESSED_OBJECT_BYTES);
    decode_event_bundle_budgeted(sections, shared, flame_only, &budget)
}

pub fn decode_event_bundle_budgeted(
    sections: &[RawSection<'_>],
    shared: &SharedSymbols<'_>,
    flame_only: bool,
    budget: &DecodeBudget,
) -> Result<ChunkData, ObjectError> {
    if flame_only {
        decode_selected(
            sections,
            Some(shared),
            budget,
            SectionKind::needed_for_flame,
        )
    } else {
        decode_selected(sections, Some(shared), budget, |_| true)
    }
}

/// Tracks per-thread range boundaries so delta-encoded timestamps reset to
/// `base_time` at each thread's start instead of encoding large backward jumps.
struct ResetCursor<'a> {
    ranges: &'a [ThreadRange],
    idx: usize,
    next_reset: usize,
}

impl<'a> ResetCursor<'a> {
    fn new(ranges: &'a [ThreadRange], total_rows: usize) -> Result<Self, ObjectError> {
        let mut running = 0_usize;
        for r in ranges {
            let start = r.start.get() as usize;
            let count = r.count.get() as usize;
            if start != running || start.saturating_add(count) > total_rows {
                return Err(ObjectError::Layout(
                    "thread ranges do not tile event table".into(),
                ));
            }
            running += count;
        }
        if running != total_rows {
            return Err(ObjectError::Layout(
                "thread ranges do not cover event table".into(),
            ));
        }
        let mut cursor = Self {
            ranges,
            idx: 0,
            next_reset: usize::MAX,
        };
        cursor.advance_next();
        Ok(cursor)
    }

    fn advance_next(&mut self) {
        while let Some(r) = self.ranges.get(self.idx) {
            self.idx += 1;
            if r.count.get() > 0 {
                self.next_reset = r.start.get() as usize;
                return;
            }
        }
        self.next_reset = usize::MAX;
    }

    fn is_reset(&mut self, row: usize) -> bool {
        if row == self.next_reset {
            self.advance_next();
            true
        } else {
            false
        }
    }
}

fn decode_samples_section(
    bytes: &[u8],
    sample_ranges: &[ThreadRange],
    keep_tasks: bool,
    budget: &DecodeBudget,
) -> Result<Vec<Sample>, ObjectError> {
    let r = open_table_section(bytes, SectionKind::Samples, budget)?;
    let total = r.rows as usize;
    charge_rows::<Sample>(&r, total)?;
    let mut samples = Vec::with_capacity(total);
    let mut resets_opt: Option<ResetCursor<'_>> = None;
    let mut global_offset = 0_usize;

    for page_idx in 0..r.page_count() {
        let p = r.read_page(page_idx)?;
        let before_cols = budget.remaining();
        let n = p.rows as usize;
        let use_reset = p.optional_column(WHOLE_SECTION_GROUP, 4).is_some();
        if use_reset && resets_opt.is_none() {
            resets_opt = Some(ResetCursor::new(sample_ranges, total)?);
        }
        let time_deltas = read_u64_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
        let weights = read_delta_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
        let stacks = read_u32_col(&p, WHOLE_SECTION_GROUP, 2, n)?;
        let tasks = read_u32_col(&p, WHOLE_SECTION_GROUP, 3, n)?;

        let mut prev = r.base_time;
        for i in 0..n {
            let global_idx = global_offset + i;
            let range_reset = resets_opt
                .as_mut()
                .is_some_and(|cursor| cursor.is_reset(global_idx));
            let is_reset = i == 0 || range_reset;
            let anchor = if is_reset { r.base_time } else { prev };
            let t = anchor.wrapping_add(zigzag_decode(time_deltas.get(i).copied().unwrap_or(0)));
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
    Ok(samples)
}

#[expect(
    clippy::too_many_lines,
    reason = "decodes each chunk table section into Capture and metadata"
)]
fn decode_selected(
    sections: &[RawSection<'_>],
    shared: Option<&SharedSymbols<'_>>,
    budget: &DecodeBudget,
    include: impl Fn(SectionKind) -> bool,
) -> Result<ChunkData, ObjectError> {
    let mut by_kind: BTreeMap<SectionKind, RawSection<'_>> = BTreeMap::new();
    for &sec in sections {
        if !include(sec.kind) {
            continue;
        }
        if shared.is_some()
            && matches!(
                sec.kind,
                SectionKind::Strings
                    | SectionKind::Modules
                    | SectionKind::Frames
                    | SectionKind::FrameSymbols
                    | SectionKind::Lines
                    | SectionKind::Functions
                    | SectionKind::Stacks
                    | SectionKind::Sites
            )
        {
            return Err(ObjectError::Layout(format!(
                "event bundle must not contain shared symbol section {:?}",
                sec.kind
            )));
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

    let mut bundle_proc_remap: Option<BTreeMap<u32, u32>> = None;

    if let Some(sym) = shared {
        let shared_bytes = sym
            .strings
            .len()
            .saturating_add(sym.modules.len().saturating_mul(size_of::<Module>()))
            .saturating_add(sym.frames.len().saturating_mul(size_of::<Frame>()))
            .saturating_add(sym.frame_symbols.len().saturating_mul(size_of::<Range>()))
            .saturating_add(sym.lines.len().saturating_mul(size_of::<Line>()))
            .saturating_add(sym.functions.len().saturating_mul(size_of::<Function>()))
            .saturating_add(sym.stacks.len().saturating_mul(size_of::<Stack>()))
            .saturating_add(sym.sites.len().saturating_mul(size_of::<Site>()));
        budget.take(shared_bytes)?;
        capture.strings.extend_from_slice(sym.strings);
        capture.modules.extend_from_slice(sym.modules);
        capture.frames.extend_from_slice(sym.frames);
        capture.frame_symbols.extend_from_slice(sym.frame_symbols);
        capture.lines.extend_from_slice(sym.lines);
        capture.functions.extend_from_slice(sym.functions);
        capture.stacks.extend_from_slice(sym.stacks);
        capture.sites.extend_from_slice(sym.sites);

        if let Some(sec) = by_kind.get(&SectionKind::Processes) {
            let mut remap = BTreeMap::new();
            let r = open_table_section(sec.bytes, SectionKind::Processes, budget)?;
            let n = r.rows as usize;
            charge_rows::<Process>(&r, n)?;
            let p = r.read_single_page()?;
            let shared_ids = read_u32_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
            capture.processes.reserve(n);
            for sid in shared_ids {
                let &proc_rec = sym.processes.get(sid as usize).ok_or_else(|| {
                    ObjectError::Layout("bundle process index out of shared range".into())
                })?;
                let local_id = u32::try_from(capture.processes.len())
                    .map_err(|_| ObjectError::Limit("process count exceeds u32"))?;
                capture.processes.push(proc_rec);
                remap.entry(sid).or_insert(local_id);
            }
            bundle_proc_remap = Some(remap);
        } else {
            budget.take(sym.processes.len().saturating_mul(size_of::<Process>()))?;
            capture.processes.extend_from_slice(sym.processes);
        }
    } else if let Some(sec) = by_kind.get(&SectionKind::Strings) {
        let r = open_table_section(sec.bytes, SectionKind::Strings, budget)?;
        for page_idx in 0..r.page_count() {
            let p = r.read_page(page_idx)?;
            let col = p.column(WHOLE_SECTION_GROUP, 0)?;
            if col.coding != Coding::Bytes {
                return Err(ObjectError::Layout(
                    "strings column must use Bytes coding".into(),
                ));
            }
            budget.take(col.data.len())?;
            capture.strings.extend_from_slice(col.data);
        }
    }

    if !crate::image::valid_pool(&capture.strings, true) {
        return Err(ObjectError::Capture(
            "string pool is not NUL-terminated UTF-8".into(),
        ));
    }
    let str_map = StringDecodeMap::from_pool(&capture.strings)?;

    if let Some(sec) = by_kind.get(&SectionKind::Settings) {
        let r = open_table_section(sec.bytes, SectionKind::Settings, budget)?;
        let n = r.rows as usize;
        charge_rows::<Setting>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Modules, budget)?;
        let n = r.rows as usize;
        charge_rows::<Module>(&r, n)?;
        let p = r.read_single_page()?;
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

    if shared.is_none()
        && let Some(sec) = by_kind.get(&SectionKind::Processes)
    {
        let r = open_table_section(sec.bytes, SectionKind::Processes, budget)?;
        let n = r.rows as usize;
        charge_rows::<Process>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Threads, budget)?;
        let n = r.rows as usize;
        charge_rows::<Thread>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Mappings, budget)?;
        let n = r.rows as usize;
        charge_rows::<Mapping>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Frames, budget)?;
        let n = r.rows as usize;
        charge_rows::<Frame>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Stacks, budget)?;
        let n = r.rows as usize;
        charge_rows::<Stack>(&r, n)?;
        let p = r.read_single_page()?;
        let col0 = p.column(WHOLE_SECTION_GROUP, 0)?;
        if col0.coding != Coding::BpTree {
            return Err(ObjectError::Layout(
                "stacks topology column must use BpTree coding".into(),
            ));
        }
        let parents = decode_bp_tree_budgeted(col0.data, n, Some(budget))?;
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

    if let Some(sec) = by_kind.get(&SectionKind::SampleRanges) {
        capture.sample_ranges = decode_thread_ranges(sec.bytes, SectionKind::SampleRanges, budget)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::Samples) {
        capture.samples = decode_samples_section(
            sec.bytes,
            &capture.sample_ranges,
            include(SectionKind::Tasks),
            budget,
        )?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::FrameSymbols) {
        let r = open_table_section(sec.bytes, SectionKind::FrameSymbols, budget)?;
        let n = r.rows as usize;
        charge_rows::<Range>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Lines, budget)?;
        let n = r.rows as usize;
        charge_rows::<Line>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Functions, budget)?;
        let n = r.rows as usize;
        charge_rows::<Function>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Accounting, budget)?;
        let n = r.rows as usize;
        charge_rows::<Account>(&r, n)?;
        let p = r.read_single_page()?;
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

    if let Some(sec) = by_kind.get(&SectionKind::OffCpuRanges) {
        capture.off_cpu_ranges =
            decode_thread_ranges(sec.bytes, SectionKind::OffCpuRanges, budget)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::OffCpu) {
        let r = open_table_section(sec.bytes, SectionKind::OffCpu, budget)?;
        let total = r.rows as usize;
        charge_rows::<OffCpu>(&r, total)?;
        let mut off_cpu = Vec::with_capacity(total);
        let mut resets_opt: Option<ResetCursor<'_>> = None;
        let mut global_offset = 0_usize;

        for page_idx in 0..r.page_count() {
            let p = r.read_page(page_idx)?;
            let before_cols = budget.remaining();
            let n = p.rows as usize;
            let use_reset = p.optional_column(WHOLE_SECTION_GROUP, 3).is_some();
            if use_reset && resets_opt.is_none() {
                resets_opt = Some(ResetCursor::new(&capture.off_cpu_ranges, total)?);
            }
            let gaps = read_u64_col(&p, WHOLE_SECTION_GROUP, 0, n)?;
            let durations = read_u64_col(&p, WHOLE_SECTION_GROUP, 1, n)?;
            let packed = read_u8_col(&p, WHOLE_SECTION_GROUP, 2, n)?;

            let mut prev_end = r.base_time;
            for i in 0..n {
                let global_idx = global_offset + i;
                let range_reset = resets_opt
                    .as_mut()
                    .is_some_and(|cursor| cursor.is_reset(global_idx));
                let is_reset = i == 0 || range_reset;
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
        let r = open_table_section(sec.bytes, SectionKind::Sites, budget)?;
        let n = r.rows as usize;
        charge_rows::<Site>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Zones, budget)?;
        let n = r.rows as usize;
        charge_rows::<Zone>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::ZoneRanges, budget)?;
        let n = r.rows as usize;
        charge_rows::<ZoneRange>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Plots, budget)?;
        let n = r.rows as usize;
        charge_rows::<Plot>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Markers, budget)?;
        let n = r.rows as usize;
        charge_rows::<Marker>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Tasks, budget)?;
        let n = r.rows as usize;
        charge_rows::<Task>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::TaskWaits, budget)?;
        let n = r.rows as usize;
        charge_rows::<TaskWait>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Gaps, budget)?;
        let n = r.rows as usize;
        charge_rows::<Gap>(&r, n)?;
        let p = r.read_single_page()?;
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
        capture.syscalls = decode_syscalls(sec.bytes, budget)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::SyscallNotes) {
        let r = open_table_section(sec.bytes, SectionKind::SyscallNotes, budget)?;
        let n = r.rows as usize;
        charge_rows::<SyscallNote>(&r, n)?;
        let p = r.read_single_page()?;
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
        decode_syscall_blobs(sec.bytes, &mut capture, budget)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::SyscallStats) {
        capture.syscall_stats = decode_syscall_stats(sec.bytes, budget)?;
    }

    if let Some(sec) = by_kind.get(&SectionKind::SyscallErrnos) {
        let r = open_table_section(sec.bytes, SectionKind::SyscallErrnos, budget)?;
        let n = r.rows as usize;
        charge_rows::<SyscallErrno>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::Chunk, budget)?;
        let n = r.rows as usize;
        charge_rows::<ChunkRow>(&r, n)?;
        let p = r.read_single_page()?;
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
                start: r
                    .time_unit
                    .to_nanos(start_units.get(i).copied().unwrap_or(0)),
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
        let r = open_table_section(sec.bytes, SectionKind::Dimensions, budget)?;
        let n = r.rows as usize;
        charge_rows::<DimensionRow>(&r, n)?;
        let p = r.read_single_page()?;
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
        let r = open_table_section(sec.bytes, SectionKind::SanitizerCounts, budget)?;
        let n = r.rows as usize;
        charge_rows::<SanitizerCountRow>(&r, n)?;
        let p = r.read_single_page()?;
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

    if let Some(proc_remap) = bundle_proc_remap {
        let remap_proc = |id: u32| -> Result<u32, ObjectError> {
            if id == NONE {
                return Ok(NONE);
            }
            proc_remap.get(&id).copied().ok_or_else(|| {
                ObjectError::Layout(
                    "bundle row references process outside bundle process set".into(),
                )
            })
        };
        for row in &mut capture.threads {
            row.process = remap_proc(row.process.get())?.into();
        }
        for row in &mut capture.mappings {
            row.process = remap_proc(row.process.get())?.into();
        }
        for row in &mut capture.syscall_notes {
            row.process = remap_proc(row.process.get())?.into();
        }
        for row in &mut capture.syscall_stats {
            row.process = remap_proc(row.process.get())?.into();
        }
        for row in &mut dimensions {
            row.process = remap_proc(row.process)?;
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
    let r = open_table_section(bytes, kind, budget)?;
    let n = r.rows as usize;
    charge_rows::<ThreadRange>(&r, n)?;
    let p = r.read_single_page()?;
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
