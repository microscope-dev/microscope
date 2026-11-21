mod decode;
mod decode_syscalls;
mod encode_core;
mod encode_events;
mod encode_syscalls;

pub use decode::{
    FlameBundle, SharedSymbols, decode_event_bundle, decode_flame_bundle, decode_flame_tables,
    decode_tables, decode_tables_budgeted,
};
pub use encode_core::{encode_event_bundle, encode_tables};

use std::collections::BTreeMap;

use foldhash::HashMap;

use crate::capture::records::{NONE, UNSEEN};
use crate::capture::{Capture, CaptureBuilder};
use crate::image::{StrId, Strings, StringsBuilder};
use crate::syscalls::specs;
use crate::syscalls::{Abi, SyscallId};

use super::codec::{
    Coding, PageReader, SectionWriter, delta_decode_i64, encode_varints, relative_decode_i64,
};
use super::{MAX_TOTAL_ROWS, ObjectError, TimeUnit};

const TABLE_VERSION: u8 = 2;
const NOTE_BLOBS_ARGUMENT: u8 = u8::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkRow {
    pub agent: u64,
    pub number: u64,
    pub start: i64,
    pub end: i64,
    pub sampled_ns: u64,
    pub preset: u8,
    pub time_unit: TimeUnit,
    pub policy: u64,
    pub rules: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DimensionRow {
    pub process: u32,
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizerCountRow {
    pub rule: String,
    pub field: u16,
    pub fired: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChunkData {
    pub capture: Capture,
    pub chunks: Vec<ChunkRow>,
    pub dimensions: Vec<DimensionRow>,
    pub sanitizer_counts: Vec<SanitizerCountRow>,
}

impl ChunkData {
    #[must_use]
    pub fn time_bounds(&self) -> (i64, i64) {
        let mut start = i64::MAX;
        let mut end = i64::MIN;
        for chunk in &self.chunks {
            start = start.min(chunk.start);
            end = end.max(chunk.end);
        }

        if start <= end {
            return (start, end);
        }

        self.capture.span()
    }
}

pub struct StringEncodeMap {
    offset_to_id: HashMap<u32, u32>,
    count: u32,
}

impl StringEncodeMap {
    pub fn from_pool(pool: &[u8]) -> Result<Self, ObjectError> {
        let mut offset_to_id = HashMap::default();
        let mut count = 0_u32;
        let mut at = 0_usize;
        while at < pool.len() {
            let off = u32::try_from(at)
                .map_err(|_| ObjectError::Limit("string pool offset exceeds u32"))?;
            offset_to_id.insert(off, count);
            count = count
                .checked_add(1)
                .ok_or(ObjectError::Limit("string count exceeds u32"))?;
            let rel = memchr::memchr(0, &pool[at..])
                .ok_or_else(|| ObjectError::Capture("string pool is not NUL-terminated".into()))?;
            at += rel + 1;
        }
        Ok(Self {
            offset_to_id,
            count,
        })
    }

    pub(super) const fn from_raw_map(offset_to_id: HashMap<u32, u32>, count: u32) -> Self {
        Self {
            offset_to_id,
            count,
        }
    }

    #[must_use]
    pub const fn count(&self) -> u32 {
        self.count
    }

    pub fn encode_ref(&self, offset: u32) -> Result<u64, ObjectError> {
        if offset == NONE {
            return Ok(0);
        }
        let &id = self.offset_to_id.get(&offset).ok_or_else(|| {
            ObjectError::Capture(format!("string offset {offset} not found in pool"))
        })?;
        Ok(u64::from(id) + 1)
    }

    pub fn encode_refs<I>(&self, offsets: I) -> Result<Vec<u64>, ObjectError>
    where
        I: IntoIterator<Item = u32>,
    {
        offsets
            .into_iter()
            .map(|off| self.encode_ref(off))
            .collect()
    }
}

pub struct StringDecodeMap {
    offsets: Vec<u32>,
}

impl StringDecodeMap {
    pub fn from_pool(pool: &[u8]) -> Result<Self, ObjectError> {
        let mut offsets = Vec::new();
        let mut at = 0_usize;
        while at < pool.len() {
            let off = u32::try_from(at)
                .map_err(|_| ObjectError::Limit("string pool offset exceeds u32"))?;
            offsets.push(off);
            let rel = memchr::memchr(0, &pool[at..])
                .ok_or_else(|| ObjectError::Capture("string pool is not NUL-terminated".into()))?;
            at += rel + 1;
        }
        Ok(Self { offsets })
    }

    pub fn decode_ref(&self, code: u64) -> Result<u32, ObjectError> {
        if code == 0 {
            return Ok(NONE);
        }
        let idx = usize::try_from(code - 1)
            .map_err(|_| ObjectError::Capture("dense string id overflow".into()))?;
        self.offsets
            .get(idx)
            .copied()
            .ok_or_else(|| ObjectError::Capture("dense string id out of bounds".into()))
    }

    pub fn decode_refs(&self, codes: &[u64]) -> Result<Vec<u32>, ObjectError> {
        codes.iter().map(|&c| self.decode_ref(c)).collect()
    }
}

pub fn shift_capture_times(capture: &mut Capture, delta_ns: i64) {
    if delta_ns == 0 {
        return;
    }

    let shift = |t: i64| {
        if t == UNSEEN {
            UNSEEN
        } else {
            t.saturating_add(delta_ns)
        }
    };

    for process in &mut capture.processes {
        let s = shift(process.start.get());
        let e = shift(process.end.get()).max(s);
        process.start = s.into();
        process.end = e.into();
    }

    for thread in &mut capture.threads {
        let s = shift(thread.start.get());
        let e = shift(thread.end.get()).max(s);
        thread.start = s.into();
        thread.end = e.into();
    }

    for mapping in &mut capture.mappings {
        mapping.time = shift(mapping.time.get()).into();
    }

    for sample in &mut capture.samples {
        sample.time = shift(sample.time.get()).into();
    }

    for interval in &mut capture.off_cpu {
        interval.time = shift(interval.time.get()).into();
    }

    for zone in &mut capture.zones {
        let s = shift(zone.start.get());
        let e = shift(zone.end.get()).max(s);
        zone.start = s.into();
        zone.end = e.into();
    }

    for plot in &mut capture.plots {
        plot.time = shift(plot.time.get()).into();
    }

    for marker in &mut capture.markers {
        marker.time = shift(marker.time.get()).into();
    }

    for task in &mut capture.tasks {
        let s = shift(task.first_seen.get());
        let e = shift(task.last_seen.get()).max(s);
        task.first_seen = s.into();
        task.last_seen = e.into();
    }

    for wait in &mut capture.task_waits {
        wait.time = shift(wait.time.get()).into();
    }

    for gap in &mut capture.gaps {
        let s = shift(gap.start.get());
        let e = shift(gap.end.get()).max(s);
        gap.start = s.into();
        gap.end = e.into();
    }

    for call in &mut capture.syscalls {
        call.entered = shift(call.entered.get()).into();
        call.exited = shift(call.exited.get()).into();
    }

    for note in &mut capture.syscall_notes {
        note.time = shift(note.time.get()).into();
    }

    let mut updated_settings = Vec::new();
    for (idx, setting) in capture.settings.iter().enumerate() {
        let key = capture.string(setting.key.get());
        if matches!(key, "start" | "end")
            && let Ok(val) = capture.string(setting.value.get()).parse::<i64>()
        {
            updated_settings.push((idx, shift(val).to_string()));
        }
    }
    for (idx, text) in updated_settings {
        if let Ok(offset) = u32::try_from(capture.strings.len()) {
            capture.strings.extend_from_slice(text.as_bytes());
            capture.strings.push(0);
            if let Some(setting) = capture.settings.get_mut(idx) {
                setting.value = offset.into();
            }
        }
    }
}

pub fn round_capture_times(capture: &mut Capture, unit: TimeUnit) {
    if unit == TimeUnit::Ns {
        return;
    }

    let round = |t: i64| {
        if t == UNSEEN {
            UNSEEN
        } else {
            unit.round_down(t)
        }
    };

    for process in &mut capture.processes {
        let s = round(process.start.get());
        let e = round(process.end.get()).max(s);
        process.start = s.into();
        process.end = e.into();
    }

    for thread in &mut capture.threads {
        let s = round(thread.start.get());
        let e = round(thread.end.get()).max(s);
        thread.start = s.into();
        thread.end = e.into();
    }

    for mapping in &mut capture.mappings {
        mapping.time = round(mapping.time.get()).into();
    }

    for sample in &mut capture.samples {
        sample.time = round(sample.time.get()).into();
    }

    for interval in &mut capture.off_cpu {
        let s = round(interval.time.get());
        let end_raw = interval
            .time
            .get()
            .saturating_add_unsigned(interval.duration.get());
        let e = round(end_raw).max(s);
        interval.time = s.into();
        interval.duration = e.abs_diff(s).into();
    }

    for zone in &mut capture.zones {
        let s = round(zone.start.get());
        let e = round(zone.end.get()).max(s);
        zone.start = s.into();
        zone.end = e.into();
    }

    for plot in &mut capture.plots {
        plot.time = round(plot.time.get()).into();
    }

    for marker in &mut capture.markers {
        marker.time = round(marker.time.get()).into();
    }

    for task in &mut capture.tasks {
        let s = round(task.first_seen.get());
        let e = round(task.last_seen.get()).max(s);
        task.first_seen = s.into();
        task.last_seen = e.into();
    }

    for wait in &mut capture.task_waits {
        wait.time = round(wait.time.get()).into();
    }

    for gap in &mut capture.gaps {
        let s = round(gap.start.get());
        let e = round(gap.end.get()).max(s);
        gap.start = s.into();
        gap.end = e.into();
    }

    for call in &mut capture.syscalls {
        call.entered = round(call.entered.get()).into();
        call.exited = round(call.exited.get()).into();
    }

    for note in &mut capture.syscall_notes {
        note.time = round(note.time.get()).into();
    }
}

pub fn merge_chunks(chunks: &[ChunkData]) -> Result<ChunkData, ObjectError> {
    let mut builder = CaptureBuilder::new();
    let mut merged_chunks = Vec::new();
    let mut merged_dims = Vec::new();
    let mut counts: BTreeMap<(String, u16), u64> = BTreeMap::new();

    for chunk in chunks {
        let remap = builder.absorb(&chunk.capture);
        merged_chunks.extend(chunk.chunks.iter().cloned());
        for dim in &chunk.dimensions {
            let process = if dim.process == NONE {
                NONE
            } else {
                remap.process(dim.process)
            };
            let entry = DimensionRow {
                process,
                name: dim.name.clone(),
                value: dim.value.clone(),
            };
            if !merged_dims.contains(&entry) {
                merged_dims.push(entry);
            }
        }
        for row in &chunk.sanitizer_counts {
            *counts.entry((row.rule.clone(), row.field)).or_default() += row.fired;
        }
    }

    let capture = builder.finish_read().map_err(ObjectError::Capture)?;
    let sanitizer_counts = counts
        .into_iter()
        .map(|((rule, field), fired)| SanitizerCountRow { rule, field, fired })
        .collect();

    Ok(ChunkData {
        capture,
        chunks: merged_chunks,
        dimensions: merged_dims,
        sanitizer_counts,
    })
}

const fn abi_of(code: u8) -> Option<Abi> {
    match code {
        0 => Some(Abi::X86_64),
        1 => Some(Abi::X86_32),
        2 => Some(Abi::X32),
        _ => None,
    }
}

#[must_use]
pub fn syscall_arg_count(abi: u8, number: u32) -> usize {
    abi_of(abi)
        .and_then(|abi| specs::spec(SyscallId::new(abi, number)))
        .map_or(6, |spec| spec.args.len().min(6))
}

fn count_u32(len: usize) -> Result<u32, ObjectError> {
    if len > MAX_TOTAL_ROWS {
        return Err(ObjectError::Limit("table row count exceeds limit"));
    }

    u32::try_from(len).map_err(|_| ObjectError::Limit("table row count exceeds u32"))
}

fn u32_from_u64(value: u64, what: &str) -> Result<u32, ObjectError> {
    u32::try_from(value).map_err(|_| ObjectError::Layout(format!("{what} exceeds u32")))
}

fn add_u64_col(
    writer: &mut SectionWriter,
    group: u16,
    col: u8,
    coding: Coding,
    key: u8,
    values: &[u64],
) -> Result<(), ObjectError> {
    if coding == Coding::VarintPlain
        && let Some(&first) = values.first()
        && values.iter().all(|&v| v == first)
    {
        return writer.add_constant(group, col, first);
    }
    let encoded = encode_varints(values);
    writer.add_column(group, col, coding, key, &encoded)
}

fn add_delta_u64_col(
    writer: &mut SectionWriter,
    group: u16,
    col: u8,
    values: &[u64],
) -> Result<(), ObjectError> {
    writer.add_u64_smart(group, col, values, true)
}

fn add_plain_u64_col(
    writer: &mut SectionWriter,
    group: u16,
    col: u8,
    values: &[u64],
) -> Result<(), ObjectError> {
    writer.add_u64_smart(group, col, values, false)
}

fn read_u64_col(
    page: &PageReader<'_>,
    group: u16,
    col: u8,
    count: usize,
) -> Result<Vec<u64>, ObjectError> {
    page.column(group, col)?
        .decode_raw_u64(count, page.budget())
}

fn read_delta_u64_col(
    page: &PageReader<'_>,
    group: u16,
    col: u8,
    count: usize,
) -> Result<Vec<u64>, ObjectError> {
    page.column(group, col)?.decode_u64(count, page.budget())
}

fn read_u32_col(
    page: &PageReader<'_>,
    group: u16,
    col: u8,
    count: usize,
) -> Result<Vec<u32>, ObjectError> {
    page.column(group, col)?.decode_u32(count, page.budget())
}

fn read_u8_col(
    page: &PageReader<'_>,
    group: u16,
    col: u8,
    count: usize,
) -> Result<Vec<u8>, ObjectError> {
    page.column(group, col)?.decode_u8(count, page.budget())
}

fn read_string_col(
    page: &PageReader<'_>,
    group: u16,
    col: u8,
    count: usize,
    strings: &StringDecodeMap,
) -> Result<Vec<u32>, ObjectError> {
    let codes = page.column(group, col)?.decode_u64(count, page.budget())?;
    strings.decode_refs(&codes)
}

fn read_time_col(
    page: &PageReader<'_>,
    time_unit: TimeUnit,
    base_time: i64,
    group: u16,
    col: u8,
    count: usize,
) -> Result<Vec<i64>, ObjectError> {
    let raw = page
        .column(group, col)?
        .decode_raw_u64(count, page.budget())?;
    let units = delta_decode_i64(&raw, base_time);
    Ok(units.into_iter().map(|u| time_unit.to_nanos(u)).collect())
}

fn read_time_units_col(
    page: &PageReader<'_>,
    base_time: i64,
    group: u16,
    col: u8,
    count: usize,
) -> Result<Vec<i64>, ObjectError> {
    let raw = page
        .column(group, col)?
        .decode_raw_u64(count, page.budget())?;
    Ok(delta_decode_i64(&raw, base_time))
}

fn read_relative_time_col(
    page: &PageReader<'_>,
    time_unit: TimeUnit,
    group: u16,
    col: u8,
    anchors_units: &[i64],
) -> Result<Vec<i64>, ObjectError> {
    let raw = page
        .column(group, col)?
        .decode_raw_u64(anchors_units.len(), page.budget())?;
    let units = relative_decode_i64(&raw, anchors_units)?;
    Ok(units.into_iter().map(|u| time_unit.to_nanos(u)).collect())
}

type InternedStrings = (Vec<u8>, Vec<u32>, Vec<(u32, u32)>, Vec<u32>);

fn intern_extra_strings(data: &ChunkData) -> InternedStrings {
    let mut strings = StringsBuilder::default();
    let base_pool = Strings(&data.capture.strings);
    let mut at = 0_usize;
    while at < data.capture.strings.len() {
        let text = base_pool.get(StrId(u32::try_from(at).unwrap_or(0)));
        let _ = strings.push(text);
        at += text.len() + 1;
    }

    let mut push = |text: &str| -> u32 {
        if text.is_empty() {
            NONE
        } else {
            strings.push(text).map_or(NONE, |StrId(id)| id)
        }
    };

    let chunk_rules: Vec<u32> = data.chunks.iter().map(|c| push(&c.rules)).collect();
    let dim_strings: Vec<(u32, u32)> = data
        .dimensions
        .iter()
        .map(|d| (push(&d.name), push(&d.value)))
        .collect();
    let count_rules: Vec<u32> = data
        .sanitizer_counts
        .iter()
        .map(|s| push(&s.rule))
        .collect();

    (strings.into_bytes(), chunk_rules, dim_strings, count_rules)
}

pub(super) fn remap_stack(old_id: u32, old_to_dfs: &[u32]) -> u32 {
    if old_id == NONE {
        NONE
    } else {
        old_to_dfs.get(old_id as usize).copied().unwrap_or(NONE)
    }
}
