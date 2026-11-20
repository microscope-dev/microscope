mod decode;
mod encode_core;
mod encode_events;
mod encode_syscalls;

pub use decode::{decode_flame_tables, decode_tables};
pub use encode_core::encode_tables;

use std::collections::BTreeMap;

use crate::capture::records::{NONE, UNSEEN};
use crate::capture::{Capture, CaptureBuilder};
use crate::image::{StrId, Strings, StringsBuilder};
use crate::syscalls::specs;
use crate::syscalls::{Abi, SyscallId};

use super::codec::{
    Coding, FLAG_BIT_PACKED, SectionReader, SectionWriter, decode_dict_u64_budgeted, decode_ints,
    delta_decode_i64, delta_decode_u64_in_place, encode_ints, relative_decode_i64, zigzag_decode,
};
use super::{MAX_ROWS, ObjectError, TimeUnit};

const TABLE_VERSION: u8 = 1;
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
    if len > MAX_ROWS {
        return Err(ObjectError::Limit("table row count exceeds limit"));
    }

    u32::try_from(len).map_err(|_| ObjectError::Limit("table row count exceeds u32"))
}

fn u32_from_u64(value: u64, what: &str) -> Result<u32, ObjectError> {
    u32::try_from(value).map_err(|_| ObjectError::Layout(format!("{what} exceeds u32")))
}

fn u8_from_u64(value: u64, what: &str) -> Result<u8, ObjectError> {
    u8::try_from(value).map_err(|_| ObjectError::Layout(format!("{what} exceeds u8")))
}

fn add_u64_col(
    writer: &mut SectionWriter,
    group: u16,
    col: u8,
    coding: Coding,
    key: u8,
    bit_packed: bool,
    values: &[u64],
) -> Result<(), ObjectError> {
    let flags = if bit_packed { FLAG_BIT_PACKED } else { 0 };
    let encoded = encode_ints(values, bit_packed);
    writer.add_column(group, col, coding, key, flags, &encoded)
}

fn read_u64_col(
    reader: &SectionReader<'_, '_>,
    group: u16,
    col: u8,
    count: usize,
) -> Result<Vec<u64>, ObjectError> {
    reader.charge(count.saturating_mul(size_of::<u64>()))?;
    let slice = reader.column(group, col)?;
    let raw = slice.decompress()?;
    match slice.coding {
        Coding::Dictionary => decode_dict_u64_budgeted(&raw, count, reader.budget()),
        Coding::Plain
        | Coding::Delta
        | Coding::DeltaByKey
        | Coding::Relative
        | Coding::String
        | Coding::Order => decode_ints(&raw, count, slice.bit_packed()),
        Coding::Bytes | Coding::Words => Err(ObjectError::Layout("expected integer column".into())),
    }
}

fn read_delta_u64_col(
    reader: &SectionReader<'_, '_>,
    group: u16,
    col: u8,
    count: usize,
) -> Result<Vec<u64>, ObjectError> {
    reader.charge(count.saturating_mul(size_of::<u64>()))?;
    let slice = reader.column(group, col)?;
    let raw = slice.decompress()?;
    let mut ints = match slice.coding {
        Coding::Dictionary => decode_dict_u64_budgeted(&raw, count, reader.budget())?,
        _ => decode_ints(&raw, count, slice.bit_packed())?,
    };
    match slice.coding {
        Coding::Delta => {
            delta_decode_u64_in_place(&mut ints);
            Ok(ints)
        }
        Coding::Plain | Coding::String | Coding::Dictionary | Coding::Order => Ok(ints),
        _ => Err(ObjectError::Layout(
            "unexpected coding for unsigned column".into(),
        )),
    }
}

fn read_u32_col(
    reader: &SectionReader<'_, '_>,
    group: u16,
    col: u8,
    count: usize,
) -> Result<Vec<u32>, ObjectError> {
    reader.charge(count.saturating_mul(size_of::<u32>()))?;
    read_delta_u64_col(reader, group, col, count)?
        .into_iter()
        .map(|v| u32_from_u64(v, "u32 column"))
        .collect()
}

fn read_u8_col(
    reader: &SectionReader<'_, '_>,
    group: u16,
    col: u8,
    count: usize,
) -> Result<Vec<u8>, ObjectError> {
    reader.charge(count.saturating_mul(size_of::<u8>()))?;
    read_delta_u64_col(reader, group, col, count)?
        .into_iter()
        .map(|v| u8_from_u64(v, "u8 column"))
        .collect()
}

fn read_time_col(
    reader: &SectionReader<'_, '_>,
    group: u16,
    col: u8,
    count: usize,
) -> Result<Vec<i64>, ObjectError> {
    reader.charge(count.saturating_mul(size_of::<u64>() + size_of::<i64>()))?;
    let slice = reader.column(group, col)?;
    let raw = slice.decompress()?;
    let ints = decode_ints(&raw, count, slice.bit_packed())?;
    let mut units = match slice.coding {
        Coding::Delta => delta_decode_i64(&ints, reader.base_time),
        Coding::Plain => ints.into_iter().map(zigzag_decode).collect(),
        _ => {
            return Err(ObjectError::Layout(
                "unexpected coding for time column".into(),
            ));
        }
    };
    for slot in &mut units {
        *slot = reader.time_unit.to_nanos(*slot);
    }
    Ok(units)
}

fn read_relative_time_col(
    reader: &SectionReader<'_, '_>,
    group: u16,
    col: u8,
    anchor_units: &[i64],
) -> Result<Vec<i64>, ObjectError> {
    reader.charge(
        anchor_units
            .len()
            .saturating_mul(size_of::<u64>() + size_of::<i64>()),
    )?;
    let slice = reader.column(group, col)?;
    let raw = slice.decompress()?;
    let ints = decode_ints(&raw, anchor_units.len(), slice.bit_packed())?;
    let mut units = match slice.coding {
        Coding::Relative => relative_decode_i64(&ints, anchor_units)?,
        Coding::Delta => delta_decode_i64(&ints, reader.base_time),
        Coding::Plain => ints.into_iter().map(zigzag_decode).collect(),
        _ => {
            return Err(ObjectError::Layout(
                "unexpected coding for relative time column".into(),
            ));
        }
    };
    for slot in &mut units {
        *slot = reader.time_unit.to_nanos(*slot);
    }
    Ok(units)
}

type InternedExtraStrings = (Vec<u8>, Vec<u32>, Vec<(u32, u32)>, Vec<u32>);

fn intern_extra_strings(data: &ChunkData) -> InternedExtraStrings {
    let mut strings = StringsBuilder::default();
    let existing = Strings(&data.capture.strings);
    let mut offset = 0;
    while offset < data.capture.strings.len() {
        let id = u32::try_from(offset).unwrap_or(NONE);
        let text = existing.get(StrId(id));
        let _ = strings.push(text);
        offset += text.len() + 1;
    }

    let mut raw = data.capture.strings.clone();
    let mut push = |text: &str| -> u32 {
        let clean = text.split('\0').next().unwrap_or_default();
        if let Some(id) = strings.push(clean) {
            let start = id.0 as usize;
            if start >= raw.len() {
                let new_id = u32::try_from(raw.len()).unwrap_or(NONE);
                raw.extend_from_slice(clean.as_bytes());
                raw.push(0);
                return new_id;
            }
            if raw.get(start..start + clean.len()) == Some(clean.as_bytes())
                && raw.get(start + clean.len()) == Some(&0)
            {
                return id.0;
            }
        }
        let new_id = u32::try_from(raw.len()).unwrap_or(NONE);
        raw.extend_from_slice(clean.as_bytes());
        raw.push(0);
        new_id
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
        .map(|c| push(&c.rule))
        .collect();

    (raw, chunk_rules, dim_strings, count_rules)
}
