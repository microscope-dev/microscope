use super::super::codec::{
    Coding, NO_KEY_COLUMN, SectionWriter, WHOLE_SECTION_GROUP, delta_encode_i64, encode_bp_tree,
    forest_to_dfs, relative_encode_i64, relative_encode_u64, zigzag_encode,
};
use super::super::{DEFAULT_PAGE_ROWS, EncodedSection, ObjectError, SectionKind, TimeUnit};
use super::encode_events::encode_event_tables;
use super::{
    ChunkData, StringEncodeMap, TABLE_VERSION, add_delta_u64_col, add_plain_u64_col, add_u64_col,
    count_u32, intern_extra_strings, remap_stack,
};
use crate::capture::Capture;
use crate::capture::records::{FrameKind, ThreadRange};

#[derive(Debug)]
pub struct SharedStringIndex<'a> {
    text_to_id: foldhash::HashMap<&'a str, u32>,
}

impl<'a> SharedStringIndex<'a> {
    pub fn build(shared_strings: &'a [u8]) -> Result<Self, ObjectError> {
        let mut text_to_id = foldhash::HashMap::default();
        let mut id = 0_u32;
        let mut at = 0_usize;
        while at < shared_strings.len() {
            let rel = memchr::memchr(0, &shared_strings[at..]).ok_or_else(|| {
                ObjectError::Capture("shared string pool not NUL-terminated".into())
            })?;
            let text = std::str::from_utf8(&shared_strings[at..at + rel])
                .map_err(|_| ObjectError::Capture("shared string pool invalid UTF-8".into()))?;
            text_to_id.entry(text).or_insert(id);
            id = id
                .checked_add(1)
                .ok_or(ObjectError::Limit("string count exceeds u32"))?;
            at += rel + 1;
        }
        Ok(Self { text_to_id })
    }
}

pub fn encode_event_bundle(
    data: &ChunkData,
    shared_strings: &[u8],
    shared_process_ids: &[u32],
    time_unit: TimeUnit,
    zstd_level: i32,
    key: u64,
) -> Result<Vec<EncodedSection>, ObjectError> {
    let shared_index = SharedStringIndex::build(shared_strings)?;
    encode_event_bundle_with_index(
        data,
        &shared_index,
        shared_process_ids,
        time_unit,
        zstd_level,
        key,
    )
}

/// Encodes per-machine event tables referencing a shared Tier A string/symbol
/// pool (`SharedStringIndex`) instead of embedding duplicate strings and symbols.
#[expect(
    clippy::too_many_lines,
    reason = "encodes per-machine event bundle referencing shared Tier A symbols"
)]
pub fn encode_event_bundle_with_index(
    data: &ChunkData,
    shared_index: &SharedStringIndex<'_>,
    shared_process_ids: &[u32],
    time_unit: TimeUnit,
    zstd_level: i32,
    key: u64,
) -> Result<Vec<EncodedSection>, ObjectError> {
    let shared_text_to_id = &shared_index.text_to_id;

    let mut local_to_shared_id = foldhash::HashMap::default();
    let mut at = 0_usize;
    while at < data.capture.strings.len() {
        let off =
            u32::try_from(at).map_err(|_| ObjectError::Limit("string pool offset exceeds u32"))?;
        let rel = memchr::memchr(0, &data.capture.strings[at..])
            .ok_or_else(|| ObjectError::Capture("capture string pool not NUL-terminated".into()))?;
        let text = std::str::from_utf8(&data.capture.strings[at..at + rel])
            .map_err(|_| ObjectError::Capture("capture string pool invalid UTF-8".into()))?;
        let &shared_id = shared_text_to_id.get(text).ok_or_else(|| {
            ObjectError::Capture(format!("string {text:?} missing from shared Tier A pool"))
        })?;
        local_to_shared_id.insert(off, shared_id);
        at += rel + 1;
    }

    let str_map =
        StringEncodeMap::from_raw_map(local_to_shared_id, count_u32(shared_text_to_id.len())?);
    let lookup_text = |text: &str| -> Result<u64, ObjectError> {
        if text.is_empty() {
            return Ok(0);
        }
        let &id = shared_text_to_id.get(text).ok_or_else(|| {
            ObjectError::Capture(format!("metadata string {text:?} missing from shared pool"))
        })?;
        Ok(u64::from(id) + 1)
    };

    let identity_stacks: Vec<u32> = (0..data.capture.stacks.len())
        .map(|i| u32::try_from(i).unwrap_or(u32::MAX))
        .collect();

    let capture = &data.capture;
    let mut sections = Vec::new();

    {
        let mut w = SectionWriter::new(
            SectionKind::Processes,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(shared_process_ids.len())?,
            0,
            zstd_level,
        );
        let ids: Vec<u64> = shared_process_ids.iter().map(|&id| u64::from(id)).collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &ids)?;
        sections.push(w.finish(key)?);
    }

    if !capture.settings.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Settings,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.settings.len())?,
            0,
            zstd_level,
        );
        let keys = str_map.encode_refs(capture.settings.iter().map(|r| r.key.get()))?;
        let vals = str_map.encode_refs(capture.settings.iter().map(|r| r.value.get()))?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &keys)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &vals)?;
        sections.push(w.finish(key)?);
    }

    if !capture.threads.is_empty() {
        let base = capture
            .threads
            .first()
            .map_or(0, |t| time_unit.to_units(t.start.get()));
        let mut w = SectionWriter::new(
            SectionKind::Threads,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.threads.len())?,
            base,
            zstd_level,
        );
        let procs: Vec<u64> = capture
            .threads
            .iter()
            .map(|r| u64::from(r.process.get()))
            .collect();
        let tids: Vec<u64> = capture
            .threads
            .iter()
            .map(|r| u64::from(r.tid.get()))
            .collect();
        let names = str_map.encode_refs(capture.threads.iter().map(|r| r.name.get()))?;
        let starts: Vec<i64> = capture
            .threads
            .iter()
            .map(|r| time_unit.to_units(r.start.get()))
            .collect();
        let ends: Vec<i64> = capture
            .threads
            .iter()
            .map(|r| time_unit.to_units(r.end.get()))
            .collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &procs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &tids)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &names)?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::VarintRelative,
            3,
            &relative_encode_i64(&ends, &starts),
        )?;
        sections.push(w.finish(key)?);
    }

    if !capture.mappings.is_empty() {
        let base = capture
            .mappings
            .first()
            .map_or(0, |m| time_unit.to_units(m.time.get()));
        let mut w = SectionWriter::new(
            SectionKind::Mappings,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.mappings.len())?,
            base,
            zstd_level,
        );
        let procs: Vec<u64> = capture
            .mappings
            .iter()
            .map(|r| u64::from(r.process.get()))
            .collect();
        let mods: Vec<u64> = capture
            .mappings
            .iter()
            .map(|r| u64::from(r.module.get()))
            .collect();
        let starts: Vec<u64> = capture.mappings.iter().map(|r| r.start.get()).collect();
        let ends: Vec<u64> = capture.mappings.iter().map(|r| r.end.get()).collect();
        let offs: Vec<u64> = capture
            .mappings
            .iter()
            .map(|r| r.file_offset.get())
            .collect();
        let biases: Vec<u64> = capture.mappings.iter().map(|r| r.bias.get()).collect();
        let times: Vec<i64> = capture
            .mappings
            .iter()
            .map(|r| time_unit.to_units(r.time.get()))
            .collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &procs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &mods)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &starts)?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::VarintRelative,
            2,
            &relative_encode_u64(&ends, &starts),
        )?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 4, &offs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 5, &biases)?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            6,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&times, base),
        )?;
        sections.push(w.finish(key)?);
    }

    if !capture.samples.is_empty() {
        sections.push(encode_samples(
            capture,
            &identity_stacks,
            time_unit,
            zstd_level,
            key,
        )?);
    }
    if !capture.sample_ranges.is_empty() {
        sections.push(encode_thread_ranges(
            SectionKind::SampleRanges,
            &capture.sample_ranges,
            zstd_level,
            key,
        )?);
    }
    if !capture.accounting.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Accounting,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.accounting.len())?,
            0,
            zstd_level,
        );
        let kinds: Vec<u64> = capture
            .accounting
            .iter()
            .map(|r| u64::from(r.kind.get()))
            .collect();
        let vals: Vec<u64> = capture.accounting.iter().map(|r| r.value.get()).collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &kinds)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &vals)?;
        sections.push(w.finish(key)?);
    }
    if !capture.off_cpu.is_empty() {
        sections.push(encode_off_cpu(capture, time_unit, zstd_level, key)?);
    }
    if !capture.off_cpu_ranges.is_empty() {
        sections.push(encode_thread_ranges(
            SectionKind::OffCpuRanges,
            &capture.off_cpu_ranges,
            zstd_level,
            key,
        )?);
    }

    encode_event_tables(
        capture,
        &str_map,
        &identity_stacks,
        time_unit,
        zstd_level,
        key,
        false,
        &mut sections,
    )?;

    if !data.chunks.is_empty() {
        let base = data.chunks.first().map_or(0, |c| c.start);
        let mut w = SectionWriter::new(
            SectionKind::Chunk,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(data.chunks.len())?,
            base,
            zstd_level,
        );
        let agents: Vec<u64> = data.chunks.iter().map(|c| c.agent).collect();
        let numbers: Vec<u64> = data.chunks.iter().map(|c| c.number).collect();
        let starts: Vec<i64> = data.chunks.iter().map(|c| c.start).collect();
        let ends: Vec<i64> = data.chunks.iter().map(|c| c.end).collect();
        let sampled: Vec<u64> = data.chunks.iter().map(|c| c.sampled_ns).collect();
        let presets: Vec<u64> = data.chunks.iter().map(|c| u64::from(c.preset)).collect();
        let units: Vec<u64> = data
            .chunks
            .iter()
            .map(|c| u64::from(c.time_unit as u8))
            .collect();
        let policies: Vec<u64> = data.chunks.iter().map(|c| c.policy).collect();
        let rules: Vec<u64> = data
            .chunks
            .iter()
            .map(|c| lookup_text(&c.rules))
            .collect::<Result<Vec<_>, _>>()?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &agents)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &numbers)?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::VarintRelative,
            2,
            &relative_encode_i64(&ends, &starts),
        )?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 4, &sampled)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 5, &presets)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 6, &units)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 7, &policies)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 8, &rules)?;
        sections.push(w.finish(key)?);
    }

    if !data.dimensions.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Dimensions,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(data.dimensions.len())?,
            0,
            zstd_level,
        );
        let procs: Vec<u64> = data
            .dimensions
            .iter()
            .map(|d| u64::from(d.process))
            .collect();
        let names: Vec<u64> = data
            .dimensions
            .iter()
            .map(|d| lookup_text(&d.name))
            .collect::<Result<Vec<_>, _>>()?;
        let vals: Vec<u64> = data
            .dimensions
            .iter()
            .map(|d| lookup_text(&d.value))
            .collect::<Result<Vec<_>, _>>()?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &procs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &names)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &vals)?;
        sections.push(w.finish(key)?);
    }

    if !data.sanitizer_counts.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::SanitizerCounts,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(data.sanitizer_counts.len())?,
            0,
            zstd_level,
        );
        let rules: Vec<u64> = data
            .sanitizer_counts
            .iter()
            .map(|s| lookup_text(&s.rule))
            .collect::<Result<Vec<_>, _>>()?;
        let fields: Vec<u64> = data
            .sanitizer_counts
            .iter()
            .map(|s| u64::from(s.field))
            .collect();
        let fired: Vec<u64> = data.sanitizer_counts.iter().map(|s| s.fired).collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &rules)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &fields)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &fired)?;
        sections.push(w.finish(key)?);
    }

    Ok(sections)
}

#[expect(
    clippy::too_many_lines,
    reason = "encodes each of the 32 chunk tables in one clear sequence"
)]
pub fn encode_tables(
    data: &ChunkData,
    time_unit: TimeUnit,
    zstd_level: i32,
    key: u64,
) -> Result<Vec<EncodedSection>, ObjectError> {
    let capture = &data.capture;
    let mut sections = Vec::new();
    let (all_strings, chunk_rules, dim_strings, count_rules) = intern_extra_strings(data);
    let str_map = StringEncodeMap::from_pool(&all_strings)?;

    let raw_parents: Vec<u32> = capture.stacks.iter().map(|s| s.parent.get()).collect();
    let (dfs_parents, old_to_dfs, dfs_to_old) = forest_to_dfs(&raw_parents);

    if !all_strings.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Strings,
            TABLE_VERSION,
            TimeUnit::Ns,
            str_map.count(),
            0,
            zstd_level,
        );
        w.add_column(
            WHOLE_SECTION_GROUP,
            0,
            Coding::Bytes,
            NO_KEY_COLUMN,
            &all_strings,
        )?;
        sections.push(w.finish(key)?);
    }

    if !capture.settings.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Settings,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.settings.len())?,
            0,
            zstd_level,
        );
        let keys = str_map.encode_refs(capture.settings.iter().map(|r| r.key.get()))?;
        let vals = str_map.encode_refs(capture.settings.iter().map(|r| r.value.get()))?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &keys)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &vals)?;
        sections.push(w.finish(key)?);
    }

    if !capture.modules.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Modules,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.modules.len())?,
            0,
            zstd_level,
        );
        let paths = str_map.encode_refs(capture.modules.iter().map(|r| r.path.get()))?;
        let bids = str_map.encode_refs(capture.modules.iter().map(|r| r.build_id.get()))?;
        let mut digests = Vec::with_capacity(capture.modules.len() * 16);
        for m in &capture.modules {
            digests.extend_from_slice(&m.digest);
        }
        let sizes: Vec<u64> = capture.modules.iter().map(|r| r.size.get()).collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &paths)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &bids)?;
        w.add_column(
            WHOLE_SECTION_GROUP,
            2,
            Coding::Bytes,
            NO_KEY_COLUMN,
            &digests,
        )?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &sizes)?;
        sections.push(w.finish(key)?);
    }

    if !capture.processes.is_empty() {
        let base = capture
            .processes
            .first()
            .map_or(0, |p| time_unit.to_units(p.start.get()));
        let mut w = SectionWriter::new(
            SectionKind::Processes,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.processes.len())?,
            base,
            zstd_level,
        );
        let pids: Vec<u64> = capture
            .processes
            .iter()
            .map(|r| u64::from(r.pid.get()))
            .collect();
        let names = str_map.encode_refs(capture.processes.iter().map(|r| r.name.get()))?;
        let starts: Vec<i64> = capture
            .processes
            .iter()
            .map(|r| time_unit.to_units(r.start.get()))
            .collect();
        let ends: Vec<i64> = capture
            .processes
            .iter()
            .map(|r| time_unit.to_units(r.end.get()))
            .collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &pids)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &names)?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::VarintRelative,
            2,
            &relative_encode_i64(&ends, &starts),
        )?;
        sections.push(w.finish(key)?);
    }

    if !capture.threads.is_empty() {
        let base = capture
            .threads
            .first()
            .map_or(0, |t| time_unit.to_units(t.start.get()));
        let mut w = SectionWriter::new(
            SectionKind::Threads,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.threads.len())?,
            base,
            zstd_level,
        );
        let procs: Vec<u64> = capture
            .threads
            .iter()
            .map(|r| u64::from(r.process.get()))
            .collect();
        let tids: Vec<u64> = capture
            .threads
            .iter()
            .map(|r| u64::from(r.tid.get()))
            .collect();
        let names = str_map.encode_refs(capture.threads.iter().map(|r| r.name.get()))?;
        let starts: Vec<i64> = capture
            .threads
            .iter()
            .map(|r| time_unit.to_units(r.start.get()))
            .collect();
        let ends: Vec<i64> = capture
            .threads
            .iter()
            .map(|r| time_unit.to_units(r.end.get()))
            .collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &procs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &tids)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &names)?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::VarintRelative,
            3,
            &relative_encode_i64(&ends, &starts),
        )?;
        sections.push(w.finish(key)?);
    }

    if !capture.mappings.is_empty() {
        let base = capture
            .mappings
            .first()
            .map_or(0, |m| time_unit.to_units(m.time.get()));
        let mut w = SectionWriter::new(
            SectionKind::Mappings,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.mappings.len())?,
            base,
            zstd_level,
        );
        let procs: Vec<u64> = capture
            .mappings
            .iter()
            .map(|r| u64::from(r.process.get()))
            .collect();
        let mods: Vec<u64> = capture
            .mappings
            .iter()
            .map(|r| u64::from(r.module.get()))
            .collect();
        let starts: Vec<u64> = capture.mappings.iter().map(|r| r.start.get()).collect();
        let ends: Vec<u64> = capture.mappings.iter().map(|r| r.end.get()).collect();
        let offs: Vec<u64> = capture
            .mappings
            .iter()
            .map(|r| r.file_offset.get())
            .collect();
        let biases: Vec<u64> = capture.mappings.iter().map(|r| r.bias.get()).collect();
        let times: Vec<i64> = capture
            .mappings
            .iter()
            .map(|r| time_unit.to_units(r.time.get()))
            .collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &procs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &mods)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &starts)?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::VarintRelative,
            2,
            &relative_encode_u64(&ends, &starts),
        )?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 4, &offs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 5, &biases)?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            6,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&times, base),
        )?;
        sections.push(w.finish(key)?);
    }

    if !capture.frames.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Frames,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.frames.len())?,
            0,
            zstd_level,
        );
        let mut addrs = Vec::with_capacity(capture.frames.len());
        for frame in &capture.frames {
            let is_named = FrameKind::from_number(frame.kind).is_some_and(FrameKind::named);
            if is_named {
                let off = u32::try_from(frame.address.get())
                    .map_err(|_| ObjectError::Capture("named frame key exceeds u32".into()))?;
                addrs.push(str_map.encode_ref(off)?);
            } else {
                addrs.push(frame.address.get());
            }
        }
        let mods: Vec<u64> = capture
            .frames
            .iter()
            .map(|r| u64::from(r.module.get()))
            .collect();
        let kinds: Vec<u64> = capture.frames.iter().map(|r| u64::from(r.kind)).collect();
        let roles: Vec<u64> = capture.frames.iter().map(|r| u64::from(r.role)).collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &addrs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &mods)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &kinds)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &roles)?;
        sections.push(w.finish(key)?);
    }

    if !capture.stacks.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Stacks,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(dfs_parents.len())?,
            0,
            zstd_level,
        );
        let bp = encode_bp_tree(&dfs_parents);
        let frames: Vec<u64> = dfs_to_old
            .iter()
            .map(|&old| {
                capture
                    .stacks
                    .get(old)
                    .map_or(0, |s| u64::from(s.frame.get()))
            })
            .collect();
        w.add_column(WHOLE_SECTION_GROUP, 0, Coding::BpTree, NO_KEY_COLUMN, &bp)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &frames)?;
        sections.push(w.finish(key)?);
    }

    if !capture.samples.is_empty() {
        sections.push(encode_samples(
            capture,
            &old_to_dfs,
            time_unit,
            zstd_level,
            key,
        )?);
    }

    if !capture.sample_ranges.is_empty() {
        sections.push(encode_thread_ranges(
            SectionKind::SampleRanges,
            &capture.sample_ranges,
            zstd_level,
            key,
        )?);
    }

    if !capture.frame_symbols.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::FrameSymbols,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.frame_symbols.len())?,
            0,
            zstd_level,
        );
        let starts: Vec<u64> = capture
            .frame_symbols
            .iter()
            .map(|r| u64::from(r.start.get()))
            .collect();
        let counts: Vec<u64> = capture
            .frame_symbols
            .iter()
            .map(|r| u64::from(r.count.get()))
            .collect();
        let mut running = 0_u32;
        let tiles = capture.frame_symbols.iter().all(|r| {
            let ok = r.start.get() == running;
            running = running.saturating_add(r.count.get());
            ok
        });
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &counts)?;
        if !tiles {
            add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &starts)?;
        }
        sections.push(w.finish(key)?);
    }

    if !capture.lines.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Lines,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.lines.len())?,
            0,
            zstd_level,
        );
        let funcs: Vec<u64> = capture
            .lines
            .iter()
            .map(|r| u64::from(r.function.get()))
            .collect();
        let files = str_map.encode_refs(capture.lines.iter().map(|r| r.file.get()))?;
        let nums: Vec<u64> = capture
            .lines
            .iter()
            .map(|r| u64::from(r.number.get()))
            .collect();
        let cols: Vec<u64> = capture
            .lines
            .iter()
            .map(|r| u64::from(r.column.get()))
            .collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &funcs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &files)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &nums)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &cols)?;
        sections.push(w.finish(key)?);
    }

    if !capture.functions.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Functions,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.functions.len())?,
            0,
            zstd_level,
        );
        let names = str_map.encode_refs(capture.functions.iter().map(|r| r.name.get()))?;
        let mods: Vec<u64> = capture
            .functions
            .iter()
            .map(|r| u64::from(r.module.get()))
            .collect();
        let files = str_map.encode_refs(capture.functions.iter().map(|r| r.file.get()))?;
        let lines: Vec<u64> = capture
            .functions
            .iter()
            .map(|r| u64::from(r.line.get()))
            .collect();
        let roles: Vec<u64> = capture
            .functions
            .iter()
            .map(|r| u64::from(r.role))
            .collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &names)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &mods)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &files)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &lines)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 4, &roles)?;
        sections.push(w.finish(key)?);
    }

    if !capture.accounting.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Accounting,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.accounting.len())?,
            0,
            zstd_level,
        );
        let kinds: Vec<u64> = capture
            .accounting
            .iter()
            .map(|r| u64::from(r.kind.get()))
            .collect();
        let vals: Vec<u64> = capture.accounting.iter().map(|r| r.value.get()).collect();
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &kinds)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &vals)?;
        sections.push(w.finish(key)?);
    }

    if !capture.off_cpu.is_empty() {
        sections.push(encode_off_cpu(capture, time_unit, zstd_level, key)?);
    }

    if !capture.off_cpu_ranges.is_empty() {
        sections.push(encode_thread_ranges(
            SectionKind::OffCpuRanges,
            &capture.off_cpu_ranges,
            zstd_level,
            key,
        )?);
    }

    encode_event_tables(
        capture,
        &str_map,
        &old_to_dfs,
        time_unit,
        zstd_level,
        key,
        true,
        &mut sections,
    )?;

    if !data.chunks.is_empty() {
        let base = data.chunks.first().map_or(0, |c| c.start);
        let mut w = SectionWriter::new(
            SectionKind::Chunk,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(data.chunks.len())?,
            base,
            zstd_level,
        );
        let agents: Vec<u64> = data.chunks.iter().map(|c| c.agent).collect();
        let numbers: Vec<u64> = data.chunks.iter().map(|c| c.number).collect();
        let starts: Vec<i64> = data.chunks.iter().map(|c| c.start).collect();
        let ends: Vec<i64> = data.chunks.iter().map(|c| c.end).collect();
        let sampled: Vec<u64> = data.chunks.iter().map(|c| c.sampled_ns).collect();
        let presets: Vec<u64> = data.chunks.iter().map(|c| u64::from(c.preset)).collect();
        let units: Vec<u64> = data
            .chunks
            .iter()
            .map(|c| u64::from(c.time_unit as u8))
            .collect();
        let policies: Vec<u64> = data.chunks.iter().map(|c| c.policy).collect();
        let rules = str_map.encode_refs(chunk_rules)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &agents)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &numbers)?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::VarintDelta,
            NO_KEY_COLUMN,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::VarintRelative,
            2,
            &relative_encode_i64(&ends, &starts),
        )?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 4, &sampled)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 5, &presets)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 6, &units)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 7, &policies)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 8, &rules)?;
        sections.push(w.finish(key)?);
    }

    if !data.dimensions.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Dimensions,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(data.dimensions.len())?,
            0,
            zstd_level,
        );
        let procs: Vec<u64> = data
            .dimensions
            .iter()
            .map(|d| u64::from(d.process))
            .collect();
        let names = str_map.encode_refs(dim_strings.iter().map(|&(n, _)| n))?;
        let vals = str_map.encode_refs(dim_strings.iter().map(|&(_, v)| v))?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &procs)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &names)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &vals)?;
        sections.push(w.finish(key)?);
    }

    if !data.sanitizer_counts.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::SanitizerCounts,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(data.sanitizer_counts.len())?,
            0,
            zstd_level,
        );
        let rules = str_map.encode_refs(count_rules)?;
        let fields: Vec<u64> = data
            .sanitizer_counts
            .iter()
            .map(|s| u64::from(s.field))
            .collect();
        let fired: Vec<u64> = data.sanitizer_counts.iter().map(|s| s.fired).collect();
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &rules)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &fields)?;
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &fired)?;
        sections.push(w.finish(key)?);
    }

    Ok(sections)
}

fn encode_thread_ranges(
    kind: SectionKind,
    ranges: &[ThreadRange],
    zstd_level: i32,
    key: u64,
) -> Result<EncodedSection, ObjectError> {
    let mut w = SectionWriter::new(
        kind,
        TABLE_VERSION,
        TimeUnit::Ns,
        count_u32(ranges.len())?,
        0,
        zstd_level,
    );
    let threads: Vec<u64> = ranges.iter().map(|r| u64::from(r.thread.get())).collect();
    let counts: Vec<u64> = ranges.iter().map(|r| u64::from(r.count.get())).collect();
    add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 0, &threads)?;
    add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &counts)?;
    w.finish(key)
}

fn build_reset_bitmap(ranges: &[ThreadRange], total_rows: usize) -> Option<Vec<bool>> {
    if ranges.is_empty() {
        return None;
    }
    let mut resets = vec![false; total_rows];
    let mut running = 0_usize;
    for r in ranges {
        let start = r.start.get() as usize;
        let count = r.count.get() as usize;
        if start != running || start.saturating_add(count) > total_rows {
            return None;
        }
        if count > 0
            && let Some(slot) = resets.get_mut(start)
        {
            *slot = true;
        }
        running += count;
    }
    (running == total_rows).then_some(resets)
}

fn encode_samples(
    capture: &Capture,
    old_to_dfs: &[u32],
    time_unit: TimeUnit,
    zstd_level: i32,
    key: u64,
) -> Result<EncodedSection, ObjectError> {
    let total = capture.samples.len();
    let base = capture
        .samples
        .first()
        .map_or(0, |s| time_unit.to_units(s.time.get()));
    let resets = build_reset_bitmap(&capture.sample_ranges, total);
    let use_thread_reset = resets.is_some();

    let mut w = SectionWriter::new(
        SectionKind::Samples,
        TABLE_VERSION,
        time_unit,
        count_u32(total)?,
        base,
        zstd_level,
    );

    let mut start = 0_usize;
    while start < total {
        let end = (start + DEFAULT_PAGE_ROWS).min(total);
        let slice = &capture.samples[start..end];
        let mut min_t = i64::MAX;
        let mut max_t = i64::MIN;
        let mut time_deltas = Vec::with_capacity(slice.len());
        let mut weights = Vec::with_capacity(slice.len());
        let mut stacks = Vec::with_capacity(slice.len());
        let mut tasks = Vec::with_capacity(slice.len());

        let mut prev = base;
        for (local_idx, row) in slice.iter().enumerate() {
            let global_idx = start + local_idx;
            let t = time_unit.to_units(row.time.get());
            let decoded_t = time_unit.to_nanos(t);
            min_t = min_t.min(decoded_t);
            max_t = max_t.max(decoded_t);
            let is_reset = local_idx == 0
                || resets
                    .as_ref()
                    .and_then(|bm| bm.get(global_idx).copied())
                    .unwrap_or(false);
            let anchor = if is_reset { base } else { prev };
            time_deltas.push(zigzag_encode(t.wrapping_sub(anchor)));
            prev = t;

            weights.push(row.weight.get());
            stacks.push(u64::from(remap_stack(row.stack.get(), old_to_dfs)));
            tasks.push(u64::from(row.task.get()));
        }

        let reset_key = if use_thread_reset { 1 } else { NO_KEY_COLUMN };
        if time_unit == TimeUnit::Ns {
            w.add_binned_2b(WHOLE_SECTION_GROUP, 0, &time_deltas)?;
        } else {
            add_u64_col(
                &mut w,
                WHOLE_SECTION_GROUP,
                0,
                Coding::VarintPlain,
                reset_key,
                &time_deltas,
            )?;
        }
        if use_thread_reset {
            w.add_constant(WHOLE_SECTION_GROUP, 4, 1)?;
        }
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &weights)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &stacks)?;
        add_delta_u64_col(&mut w, WHOLE_SECTION_GROUP, 3, &tasks)?;

        w.finish_page(min_t, max_t, count_u32(slice.len())?)?;
        start = end;
    }

    w.finish_paged(key)
}

fn encode_off_cpu(
    capture: &Capture,
    time_unit: TimeUnit,
    zstd_level: i32,
    key: u64,
) -> Result<EncodedSection, ObjectError> {
    let total = capture.off_cpu.len();
    let base = capture
        .off_cpu
        .first()
        .map_or(0, |row| time_unit.to_units(row.time.get()));
    let resets = build_reset_bitmap(&capture.off_cpu_ranges, total);
    let use_thread_reset = resets.is_some();

    let mut w = SectionWriter::new(
        SectionKind::OffCpu,
        TABLE_VERSION,
        time_unit,
        count_u32(total)?,
        base,
        zstd_level,
    );

    let mut start = 0_usize;
    while start < total {
        let end = (start + DEFAULT_PAGE_ROWS).min(total);
        let slice = &capture.off_cpu[start..end];
        let mut min_t = i64::MAX;
        let mut max_t = i64::MIN;
        let mut gaps = Vec::with_capacity(slice.len());
        let mut durations = Vec::with_capacity(slice.len());
        let mut packed = Vec::with_capacity(slice.len());

        let mut prev_end = base;
        for (local_idx, row) in slice.iter().enumerate() {
            let global_idx = start + local_idx;
            let raw_t = row.time.get();
            let end_ns = raw_t.saturating_add_unsigned(row.duration.get());
            let s = time_unit.to_units(raw_t);
            let e = time_unit.to_units(end_ns).max(s);
            min_t = min_t.min(time_unit.to_nanos(s));
            max_t = max_t.max(time_unit.to_nanos(e));
            let dur = e.abs_diff(s);
            let is_reset = local_idx == 0
                || resets
                    .as_ref()
                    .and_then(|bm| bm.get(global_idx).copied())
                    .unwrap_or(false);
            let anchor = if is_reset { base } else { prev_end };
            gaps.push(zigzag_encode(s.wrapping_sub(anchor)));
            durations.push(dur);
            packed.push(u64::from((row.flags << 1) | (row.preempted & 1)));
            prev_end = e;
        }

        if time_unit == TimeUnit::Ns {
            w.add_binned_2b(WHOLE_SECTION_GROUP, 0, &gaps)?;
            w.add_binned_2b(WHOLE_SECTION_GROUP, 1, &durations)?;
        } else {
            add_u64_col(
                &mut w,
                WHOLE_SECTION_GROUP,
                0,
                Coding::VarintPlain,
                NO_KEY_COLUMN,
                &gaps,
            )?;
            add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 1, &durations)?;
        }
        add_plain_u64_col(&mut w, WHOLE_SECTION_GROUP, 2, &packed)?;
        if use_thread_reset {
            w.add_constant(WHOLE_SECTION_GROUP, 3, 1)?;
        }

        w.finish_page(min_t, max_t, count_u32(slice.len())?)?;
        start = end;
    }

    w.finish_paged(key)
}
