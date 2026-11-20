use super::super::codec::{
    Coding, NO_KEY_COLUMN, SectionWriter, WHOLE_SECTION_GROUP, delta_encode_i64, delta_encode_u64,
    relative_encode_i64, relative_encode_u64, zigzag_encode,
};
use super::super::{EncodedSection, ObjectError, SectionKind, TimeUnit};
use super::encode_events::encode_event_tables;
use super::{ChunkData, TABLE_VERSION, add_u64_col, count_u32, intern_extra_strings};
use crate::capture::Capture;
use crate::capture::records::ThreadRange;

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

    if !all_strings.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Strings,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(all_strings.len())?,
            0,
            zstd_level,
        );
        w.add_column(
            WHOLE_SECTION_GROUP,
            0,
            Coding::Bytes,
            NO_KEY_COLUMN,
            0,
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
        let keys: Vec<u64> = capture
            .settings
            .iter()
            .map(|r| u64::from(r.key.get()))
            .collect();
        let vals: Vec<u64> = capture
            .settings
            .iter()
            .map(|r| u64::from(r.value.get()))
            .collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &keys,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &vals,
        )?;
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
        let paths: Vec<u64> = capture
            .modules
            .iter()
            .map(|r| u64::from(r.path.get()))
            .collect();
        let bids: Vec<u64> = capture
            .modules
            .iter()
            .map(|r| u64::from(r.build_id.get()))
            .collect();
        let mut digests = Vec::with_capacity(capture.modules.len() * 16);
        for m in &capture.modules {
            digests.extend_from_slice(&m.digest);
        }
        let sizes: Vec<u64> = capture.modules.iter().map(|r| r.size.get()).collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &paths,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &bids,
        )?;
        w.add_column(
            WHOLE_SECTION_GROUP,
            2,
            Coding::Bytes,
            NO_KEY_COLUMN,
            0,
            &digests,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &sizes,
        )?;
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
        let names: Vec<u64> = capture
            .processes
            .iter()
            .map(|r| u64::from(r.name.get()))
            .collect();
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
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&pids),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &names,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Relative,
            2,
            false,
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
        let names: Vec<u64> = capture
            .threads
            .iter()
            .map(|r| u64::from(r.name.get()))
            .collect();
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
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&procs),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&tids),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &names,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::Relative,
            3,
            false,
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
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&procs),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&mods),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&starts),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Relative,
            2,
            false,
            &relative_encode_u64(&ends, &starts),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &offs,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            5,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&biases),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            6,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
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
        let addrs: Vec<u64> = capture.frames.iter().map(|r| r.address.get()).collect();
        let mods: Vec<u64> = capture
            .frames
            .iter()
            .map(|r| u64::from(r.module.get()))
            .collect();
        let kinds: Vec<u64> = capture.frames.iter().map(|r| u64::from(r.kind)).collect();
        let roles: Vec<u64> = capture.frames.iter().map(|r| u64::from(r.role)).collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&addrs),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&mods),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &kinds,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &roles,
        )?;
        sections.push(w.finish(key)?);
    }

    if !capture.stacks.is_empty() {
        let mut w = SectionWriter::new(
            SectionKind::Stacks,
            TABLE_VERSION,
            TimeUnit::Ns,
            count_u32(capture.stacks.len())?,
            0,
            zstd_level,
        );
        let parents: Vec<u64> = capture
            .stacks
            .iter()
            .map(|r| u64::from(r.parent.get()))
            .collect();
        let frames: Vec<u64> = capture
            .stacks
            .iter()
            .map(|r| u64::from(r.frame.get()))
            .collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&parents),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&frames),
        )?;
        sections.push(w.finish(key)?);
    }

    if !capture.samples.is_empty() {
        let base = capture
            .samples
            .first()
            .map_or(0, |s| time_unit.to_units(s.time.get()));
        let mut w = SectionWriter::new(
            SectionKind::Samples,
            TABLE_VERSION,
            time_unit,
            count_u32(capture.samples.len())?,
            base,
            zstd_level,
        );
        let times: Vec<i64> = capture
            .samples
            .iter()
            .map(|r| time_unit.to_units(r.time.get()))
            .collect();
        let weights: Vec<u64> = capture.samples.iter().map(|r| r.weight.get()).collect();
        let stacks: Vec<u64> = capture
            .samples
            .iter()
            .map(|r| u64::from(r.stack.get()))
            .collect();
        let tasks: Vec<u64> = capture
            .samples
            .iter()
            .map(|r| u64::from(r.task.get()))
            .collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Delta,
            NO_KEY_COLUMN,
            true,
            &delta_encode_i64(&times, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &weights,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&stacks),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&tasks),
        )?;
        sections.push(w.finish(key)?);
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
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &counts,
        )?;
        if !tiles {
            add_u64_col(
                &mut w,
                WHOLE_SECTION_GROUP,
                1,
                Coding::Delta,
                NO_KEY_COLUMN,
                false,
                &delta_encode_u64(&starts),
            )?;
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
        let files: Vec<u64> = capture
            .lines
            .iter()
            .map(|r| u64::from(r.file.get()))
            .collect();
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
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&funcs),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&files),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&nums),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &cols,
        )?;
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
        let names: Vec<u64> = capture
            .functions
            .iter()
            .map(|r| u64::from(r.name.get()))
            .collect();
        let mods: Vec<u64> = capture
            .functions
            .iter()
            .map(|r| u64::from(r.module.get()))
            .collect();
        let files: Vec<u64> = capture
            .functions
            .iter()
            .map(|r| u64::from(r.file.get()))
            .collect();
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
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &names,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&mods),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &files,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &lines,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &roles,
        )?;
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
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &kinds,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &vals,
        )?;
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

    encode_event_tables(capture, time_unit, zstd_level, key, &mut sections)?;
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
        let rules: Vec<u64> = chunk_rules.iter().map(|&r| u64::from(r)).collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &agents,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&numbers),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_i64(&starts, base),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            3,
            Coding::Relative,
            2,
            false,
            &relative_encode_i64(&ends, &starts),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            4,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &sampled,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            5,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &presets,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            6,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &units,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            7,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &policies,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            8,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &rules,
        )?;
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
        let names: Vec<u64> = dim_strings.iter().map(|&(n, _)| u64::from(n)).collect();
        let vals: Vec<u64> = dim_strings.iter().map(|&(_, v)| u64::from(v)).collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::Delta,
            NO_KEY_COLUMN,
            false,
            &delta_encode_u64(&procs),
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &names,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &vals,
        )?;
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
        let rules: Vec<u64> = count_rules.iter().map(|&r| u64::from(r)).collect();
        let fields: Vec<u64> = data
            .sanitizer_counts
            .iter()
            .map(|c| u64::from(c.field))
            .collect();
        let fired: Vec<u64> = data.sanitizer_counts.iter().map(|c| c.fired).collect();
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            0,
            Coding::String,
            NO_KEY_COLUMN,
            false,
            &rules,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            1,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &fields,
        )?;
        add_u64_col(
            &mut w,
            WHOLE_SECTION_GROUP,
            2,
            Coding::Plain,
            NO_KEY_COLUMN,
            false,
            &fired,
        )?;
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
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        0,
        Coding::Delta,
        NO_KEY_COLUMN,
        false,
        &delta_encode_u64(&threads),
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        1,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &counts,
    )?;
    w.finish(key)
}

fn encode_off_cpu(
    capture: &Capture,
    time_unit: TimeUnit,
    zstd_level: i32,
    key: u64,
) -> Result<EncodedSection, ObjectError> {
    let base = capture
        .off_cpu
        .first()
        .map_or(0, |row| time_unit.to_units(row.time.get()));
    let mut w = SectionWriter::new(
        SectionKind::OffCpu,
        TABLE_VERSION,
        time_unit,
        count_u32(capture.off_cpu.len())?,
        base,
        zstd_level,
    );
    let mut gaps = Vec::with_capacity(capture.off_cpu.len());
    let mut durations = Vec::with_capacity(capture.off_cpu.len());
    let mut packed = Vec::with_capacity(capture.off_cpu.len());

    let mut prev_end = base;
    for row in &capture.off_cpu {
        let start = time_unit.to_units(row.time.get());
        let end_ns = row.time.get().saturating_add_unsigned(row.duration.get());
        let end = time_unit.to_units(end_ns).max(start);
        let dur = end.abs_diff(start);
        gaps.push(zigzag_encode(start.wrapping_sub(prev_end)));
        durations.push(dur);
        packed.push(u64::from((row.flags << 1) | (row.preempted & 1)));
        prev_end = end;
    }

    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        0,
        Coding::Delta,
        NO_KEY_COLUMN,
        true,
        &gaps,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        1,
        Coding::Plain,
        NO_KEY_COLUMN,
        true,
        &durations,
    )?;
    add_u64_col(
        &mut w,
        WHOLE_SECTION_GROUP,
        2,
        Coding::Plain,
        NO_KEY_COLUMN,
        false,
        &packed,
    )?;
    w.finish(key)
}
